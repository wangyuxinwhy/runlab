//! Pulling Images over the OCI Distribution protocol: registry authentication,
//! platform selection from an index, and blob download.
//!
//! Downloaded bytes go straight into the OCI Layout and are verified there.
//! This module never decides what a local reference means; the `ingress` root
//! composes it with the catalog.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::StatusCode;
use reqwest::blocking::{Client, Response};
use reqwest::header::{
    ACCEPT, AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, HeaderMap, HeaderValue, WWW_AUTHENTICATE,
};
use serde::Deserialize;
use serde_json::Value;

use crate::core::{
    Architecture, Digest, OCI_IMAGE_CONFIG, OCI_IMAGE_INDEX, OCI_IMAGE_MANIFEST, OCI_LAYER_GZIP,
    OCI_LAYER_TAR, OCI_LAYER_ZSTD, OciDescriptor, Platform,
};
use crate::integrity::digest_bytes;
use crate::oci::{MAX_IMAGE_LAYERS, OciLayout};

const MAX_JSON_BYTES: u64 = 16 * 1024 * 1024;
const MAX_TOKEN_RESPONSE_BYTES: u64 = 1024 * 1024;
const METADATA_REQUEST_TIMEOUT: Duration = Duration::from_mins(1);
const BLOB_REQUEST_TIMEOUT: Duration = Duration::from_hours(1);
const MANIFEST_ACCEPT: &str = concat!(
    "application/vnd.oci.image.index.v1+json, ",
    "application/vnd.oci.image.manifest.v1+json"
);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteReference {
    registry: String,
    repository: String,
    selector: RemoteSelector,
    requested: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RemoteSelector {
    Tag(String),
    Digest(Digest),
}

impl RemoteSelector {
    fn as_str(&self) -> &str {
        match self {
            Self::Tag(tag) => tag,
            Self::Digest(digest) => digest.as_str(),
        }
    }
}

impl RemoteReference {
    pub(crate) fn parse(value: &str) -> Result<Self> {
        if value.contains("://") || value.contains(['?', '#']) {
            bail!("remote OCI reference must not contain a URL scheme, query, or fragment");
        }
        let (registry, remainder) = value
            .split_once('/')
            .context("remote OCI reference must include an explicit registry host")?;
        validate_registry(registry)?;
        let (repository, selector) = if let Some((repository, digest)) = remainder.rsplit_once('@')
        {
            (repository, RemoteSelector::Digest(Digest::parse(digest)?))
        } else {
            let separator = remainder
                .rfind(':')
                .context("remote OCI reference must include a tag or digest")?;
            let (repository, tag) = remainder.split_at(separator);
            let tag = &tag[1..];
            validate_tag(tag)?;
            (repository, RemoteSelector::Tag(tag.to_owned()))
        };
        validate_repository(repository)?;
        Ok(Self {
            registry: registry.to_owned(),
            repository: repository.to_owned(),
            selector,
            requested: value.to_owned(),
        })
    }

    pub(crate) fn default_local_reference(&self) -> String {
        match &self.selector {
            RemoteSelector::Digest(digest) => {
                format!("{}:sha256-{}", self.repository, &digest.hex()[..12])
            }
            RemoteSelector::Tag(tag) => format!("{}:{tag}", self.repository),
        }
    }

    fn digest_pinned(&self, digest: &Digest) -> String {
        format!("{}/{}@{digest}", self.registry, self.repository)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DistributionPullResult {
    pub(crate) remote_reference: String,
    pub(crate) source_index: Option<OciDescriptor>,
    pub(crate) selected_manifest: OciDescriptor,
    pub(crate) source: String,
    pub(crate) downloaded_blobs: u64,
    pub(crate) downloaded_bytes: u64,
}

pub(crate) struct DistributionClient {
    client: Client,
    scheme: &'static str,
}

impl DistributionClient {
    pub(crate) fn https() -> Result<Self> {
        Self::new("https")
    }

    #[cfg(test)]
    pub(crate) fn plain_http_for_tests() -> Result<Self> {
        Self::new("http")
    }

    fn new(scheme: &'static str) -> Result<Self> {
        let redirects = reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() > 10 {
                attempt.error("too many OCI registry redirects")
            } else if attempt.url().scheme() != scheme {
                attempt.error("OCI registry redirect changed transport scheme")
            } else {
                attempt.follow()
            }
        });
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .redirect(redirects)
            .user_agent(concat!("runlab/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("failed to construct OCI Distribution client")?;
        Ok(Self { client, scheme })
    }

    pub(crate) fn pull(
        &self,
        layout: &OciLayout,
        reference: &RemoteReference,
        platform: Platform,
    ) -> Result<DistributionPullResult> {
        let mut repository = RepositoryClient::new(self, reference)?;
        let source = repository.fetch_json_manifest(reference.selector.as_str(), None)?;
        if let RemoteSelector::Digest(requested_digest) = &reference.selector
            && source.descriptor.digest != *requested_digest
        {
            bail!(
                "remote OCI manifest digest mismatch: expected {requested_digest}, received {}",
                source.descriptor.digest
            );
        }
        let mut fetched = BTreeMap::from([(source.descriptor.digest.clone(), source.bytes)]);
        let stored_source = layout.put_bytes(&source.body, &source.descriptor.media_type)?;
        verify_descriptor(&stored_source, &source.descriptor, "stored OCI manifest")?;
        let (source_index, manifest) = match source.descriptor.media_type.as_str() {
            OCI_IMAGE_MANIFEST => (None, source),
            OCI_IMAGE_INDEX => {
                let selected = select_manifest(&source.body_value, platform)?;
                let manifest =
                    repository.fetch_json_manifest(selected.digest.as_str(), Some(&selected))?;
                let stored_manifest = layout.put_bytes(&manifest.body, OCI_IMAGE_MANIFEST)?;
                verify_descriptor(
                    &stored_manifest,
                    &manifest.descriptor,
                    "stored OCI Image Manifest",
                )?;
                fetched.insert(manifest.descriptor.digest.clone(), manifest.bytes);
                (Some(source.descriptor.clone()), manifest)
            }
            media_type => bail!("unsupported remote OCI manifest mediaType: {media_type}"),
        };

        let content = parse_image_manifest(&manifest.body_value)?;
        let mut descriptors = Vec::with_capacity(content.layers.len() + 1);
        descriptors.push(content.config);
        descriptors.extend(content.layers);
        let mut seen = BTreeSet::new();
        for descriptor in descriptors {
            if !seen.insert(descriptor.digest.clone()) {
                continue;
            }
            if let Some(previous_size) = fetched.get(&descriptor.digest) {
                if *previous_size != descriptor.size {
                    bail!("conflicting OCI descriptor size for {}", descriptor.digest);
                }
                continue;
            }
            if descriptor.media_type == OCI_IMAGE_CONFIG && descriptor.size > MAX_JSON_BYTES {
                bail!("OCI Image Config exceeds the {MAX_JSON_BYTES}-byte JSON limit");
            }
            let response = repository.get_blob(&descriptor)?;
            layout.put_reader(
                BoundedDescriptorReader::new(response, descriptor.size),
                &descriptor.media_type,
                Some(&descriptor),
            )?;
            fetched.insert(descriptor.digest, descriptor.size);
        }

        if let Some(index) = &source_index
            && index.digest == manifest.descriptor.digest
        {
            bail!("OCI Index and selected Manifest must have distinct identities");
        }
        let downloaded_blobs =
            u64::try_from(fetched.len()).context("downloaded blob count overflow")?;
        let downloaded_bytes = fetched.values().try_fold(0_u64, |total, size| {
            total
                .checked_add(*size)
                .context("downloaded byte count overflow")
        })?;
        let source_digest = source_index
            .as_ref()
            .map_or(&manifest.descriptor.digest, |index| &index.digest);
        let source = reference.digest_pinned(source_digest);
        Ok(DistributionPullResult {
            remote_reference: reference.requested.clone(),
            source_index,
            source,
            selected_manifest: manifest.descriptor,
            downloaded_blobs,
            downloaded_bytes,
        })
    }
}

struct RepositoryClient<'client> {
    transport: &'client DistributionClient,
    reference: &'client RemoteReference,
    token: Option<String>,
}

impl<'client> RepositoryClient<'client> {
    fn new(
        transport: &'client DistributionClient,
        reference: &'client RemoteReference,
    ) -> Result<Self> {
        if transport.scheme != "https" && transport.scheme != "http" {
            bail!("unsupported OCI Distribution transport scheme");
        }
        Ok(Self {
            transport,
            reference,
            token: None,
        })
    }

    fn fetch_json_manifest(
        &mut self,
        selector: &str,
        expected: Option<&OciDescriptor>,
    ) -> Result<FetchedJson> {
        let url = self.url("manifests", selector)?;
        let response = self.get_authenticated(url, Some(MANIFEST_ACCEPT))?;
        let response = require_success(response, "manifest")?;
        let media_type = response_media_type(response.headers())?;
        if !matches!(media_type.as_str(), OCI_IMAGE_INDEX | OCI_IMAGE_MANIFEST) {
            bail!("unsupported remote OCI manifest mediaType: {media_type}");
        }
        if let Some(expected) = expected {
            if expected.media_type != media_type {
                bail!(
                    "OCI manifest mediaType mismatch: expected {}, received {media_type}",
                    expected.media_type
                );
            }
            if expected.size > MAX_JSON_BYTES {
                bail!("OCI manifest exceeds the {MAX_JSON_BYTES}-byte JSON limit");
            }
        }
        let header_digest = response
            .headers()
            .get("docker-content-digest")
            .map(|value| {
                Digest::parse(
                    value
                        .to_str()
                        .context("remote Docker-Content-Digest header is not ASCII")?,
                )
            })
            .transpose()?;
        let body = read_bounded_response(response, MAX_JSON_BYTES, "OCI manifest")?;
        let descriptor = OciDescriptor {
            digest: digest_bytes(&body),
            size: u64::try_from(body.len()).context("OCI manifest size overflow")?,
            media_type,
        };
        if let Some(expected) = expected {
            verify_descriptor(&descriptor, expected, "OCI manifest")?;
        }
        if let Some(header_digest) = header_digest
            && header_digest != descriptor.digest
        {
            bail!(
                "remote Docker-Content-Digest mismatch: expected {header_digest}, received {}",
                descriptor.digest
            );
        }
        let body_value: Value =
            serde_json::from_slice(&body).context("remote OCI manifest is invalid JSON")?;
        validate_top_level_media_type(&body_value, &descriptor.media_type)?;
        Ok(FetchedJson {
            bytes: descriptor.size,
            descriptor,
            body,
            body_value,
        })
    }

    fn get_blob(&mut self, descriptor: &OciDescriptor) -> Result<Response> {
        let url = self.url("blobs", descriptor.digest.as_str())?;
        let response = self.get_authenticated(url, None)?;
        let response = require_success(response, "blob")?;
        if let Some(length) = response.content_length()
            && length != descriptor.size
        {
            bail!(
                "OCI blob size mismatch for {}: expected {}, received {length}",
                descriptor.digest,
                descriptor.size
            );
        }
        Ok(response)
    }

    fn get_authenticated(&mut self, url: reqwest::Url, accept: Option<&str>) -> Result<Response> {
        let mut response = self.send(url.clone(), accept)?;
        if response.status() != StatusCode::UNAUTHORIZED {
            return Ok(response);
        }
        let challenge = response
            .headers()
            .get(WWW_AUTHENTICATE)
            .context("OCI registry returned 401 without WWW-Authenticate")?
            .to_str()
            .context("OCI registry WWW-Authenticate header is not ASCII")?
            .to_owned();
        response = self.authenticate(&challenge).and_then(|token| {
            self.token = Some(token);
            self.send(url, accept)
        })?;
        Ok(response)
    }

    fn send(&self, url: reqwest::Url, accept: Option<&str>) -> Result<Response> {
        let mut request = self.transport.client.get(url).timeout(if accept.is_some() {
            METADATA_REQUEST_TIMEOUT
        } else {
            BLOB_REQUEST_TIMEOUT
        });
        if let Some(accept) = accept {
            request = request.header(ACCEPT, accept);
        }
        if let Some(token) = &self.token {
            let value = HeaderValue::from_str(&format!("Bearer {token}"))
                .context("registry token cannot be represented as an HTTP header")?;
            request = request.header(AUTHORIZATION, value);
        }
        request.send().context("OCI registry request failed")
    }

    fn authenticate(&self, challenge: &str) -> Result<String> {
        let parameters = parse_bearer_challenge(challenge)?;
        let realm = parameters
            .get("realm")
            .context("OCI Bearer challenge lacks realm")?;
        let mut url = reqwest::Url::parse(realm).context("OCI Bearer realm is not a valid URL")?;
        if url.scheme() != self.transport.scheme {
            bail!("OCI Bearer realm must use {}", self.transport.scheme);
        }
        if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
            bail!("OCI Bearer realm must not contain user information or a fragment");
        }
        {
            let mut query = url.query_pairs_mut();
            if let Some(service) = parameters.get("service") {
                query.append_pair("service", service);
            }
            query.append_pair(
                "scope",
                parameters
                    .get("scope")
                    .map_or_else(
                        || format!("repository:{}:pull", self.reference.repository),
                        Clone::clone,
                    )
                    .as_str(),
            );
        }
        let response = self
            .transport
            .client
            .get(url)
            .timeout(METADATA_REQUEST_TIMEOUT)
            .send()
            .context("OCI registry token request failed")?;
        let response = require_success(response, "token")?;
        let body = read_bounded_response(response, MAX_TOKEN_RESPONSE_BYTES, "token response")?;
        let token: TokenResponse =
            serde_json::from_slice(&body).context("OCI registry token response is invalid JSON")?;
        let token = token
            .token
            .or(token.access_token)
            .context("OCI registry token response lacks token or access_token")?;
        if token.is_empty() || token.contains(['\r', '\n']) {
            bail!("OCI registry returned an invalid Bearer token");
        }
        Ok(token)
    }

    fn url(&self, kind: &str, identity: &str) -> Result<reqwest::Url> {
        reqwest::Url::parse(&format!(
            "{}://{}/v2/{}/{kind}/{identity}",
            self.transport.scheme, self.reference.registry, self.reference.repository
        ))
        .context("failed to construct OCI Distribution URL")
    }
}

struct FetchedJson {
    descriptor: OciDescriptor,
    bytes: u64,
    body: Vec<u8>,
    body_value: Value,
}

#[derive(Deserialize)]
struct TokenResponse {
    token: Option<String>,
    access_token: Option<String>,
}

#[derive(Debug)]
struct ManifestContent {
    config: OciDescriptor,
    layers: Vec<OciDescriptor>,
}

struct BoundedDescriptorReader<R> {
    inner: R,
    remaining: u64,
}

impl<R> BoundedDescriptorReader<R> {
    const fn new(inner: R, expected_size: u64) -> Self {
        Self {
            inner,
            remaining: expected_size.saturating_add(1),
        }
    }
}

impl<R: Read> Read for BoundedDescriptorReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if self.remaining == 0 {
            return Ok(0);
        }
        let limit = usize::try_from(self.remaining)
            .unwrap_or(usize::MAX)
            .min(buffer.len());
        let read = self.inner.read(&mut buffer[..limit])?;
        self.remaining -= u64::try_from(read).expect("read length fits u64");
        Ok(read)
    }
}

fn parse_image_manifest(value: &Value) -> Result<ManifestContent> {
    let object = value
        .as_object()
        .context("remote OCI Image Manifest must be an object")?;
    if object.get("schemaVersion").and_then(Value::as_u64) != Some(2) {
        bail!("remote OCI Image Manifest schemaVersion must be 2");
    }
    let config = parse_descriptor(
        object
            .get("config")
            .context("remote OCI Image Manifest lacks config")?,
        "config",
    )?;
    if config.media_type != OCI_IMAGE_CONFIG {
        bail!("remote OCI Image Manifest config has an unsupported mediaType");
    }
    let layers = object
        .get("layers")
        .and_then(Value::as_array)
        .context("remote OCI Image Manifest layers must be an array")?
        .iter()
        .enumerate()
        .map(|(index, value)| parse_descriptor(value, &format!("layers[{index}]")))
        .collect::<Result<Vec<_>>>()?;
    if layers.len() > MAX_IMAGE_LAYERS {
        bail!("remote OCI Image exceeds the {MAX_IMAGE_LAYERS}-Layer limit");
    }
    for layer in &layers {
        if !matches!(
            layer.media_type.as_str(),
            OCI_LAYER_TAR | OCI_LAYER_GZIP | OCI_LAYER_ZSTD
        ) {
            bail!(
                "remote OCI Image Layer has an unsupported mediaType: {}",
                layer.media_type
            );
        }
    }
    Ok(ManifestContent { config, layers })
}

fn select_manifest(index: &Value, platform: Platform) -> Result<OciDescriptor> {
    let object = index
        .as_object()
        .context("remote OCI Image Index must be an object")?;
    if object.get("schemaVersion").and_then(Value::as_u64) != Some(2) {
        bail!("remote OCI Image Index schemaVersion must be 2");
    }
    let manifests = object
        .get("manifests")
        .and_then(Value::as_array)
        .context("remote OCI Image Index manifests must be an array")?;
    let mut matching = Vec::new();
    for value in manifests {
        if let Some((descriptor, candidate_platform)) = parse_index_candidate(value)?
            && candidate_platform == platform
        {
            matching.push(descriptor);
        }
    }
    let mut matching = matching.into_iter();
    let selected = matching
        .next()
        .with_context(|| format!("OCI Image Index has no Manifest for {platform}"))?;
    if matching.next().is_some() {
        bail!("OCI Image Index has multiple Manifests for {platform}");
    }
    Ok(selected)
}

fn parse_index_candidate(value: &Value) -> Result<Option<(OciDescriptor, Platform)>> {
    let object = value
        .as_object()
        .context("OCI Image Index manifest entry must be a descriptor")?;
    let descriptor = parse_descriptor(value, "Index manifest")?;
    if descriptor.media_type != OCI_IMAGE_MANIFEST {
        return Ok(None);
    }
    let Some(platform) = object.get("platform") else {
        return Ok(None);
    };
    let platform = platform
        .as_object()
        .context("OCI Index platform must be an object")?;
    let os = platform
        .get("os")
        .and_then(Value::as_str)
        .context("OCI Index platform os must be a string")?;
    let architecture = platform
        .get("architecture")
        .and_then(Value::as_str)
        .context("OCI Index platform architecture must be a string")?;
    if os != "linux" {
        return Ok(None);
    }
    let architecture = match architecture {
        "amd64" => Architecture::Amd64,
        "arm64" => Architecture::Arm64,
        _ => return Ok(None),
    };
    Ok(Some((descriptor, Platform::linux(architecture))))
}

fn parse_descriptor(value: &Value, field: &str) -> Result<OciDescriptor> {
    let object = value
        .as_object()
        .with_context(|| format!("remote OCI {field} must be a descriptor"))?;
    Ok(OciDescriptor {
        media_type: object
            .get("mediaType")
            .and_then(Value::as_str)
            .with_context(|| format!("remote OCI {field} mediaType is invalid"))?
            .to_owned(),
        digest: Digest::parse(
            object
                .get("digest")
                .and_then(Value::as_str)
                .with_context(|| format!("remote OCI {field} digest is invalid"))?,
        )?,
        size: object
            .get("size")
            .and_then(Value::as_u64)
            .with_context(|| format!("remote OCI {field} size is invalid"))?,
    })
}

fn validate_top_level_media_type(value: &Value, expected: &str) -> Result<()> {
    let object = value
        .as_object()
        .context("remote OCI manifest must be an object")?;
    if let Some(media_type) = object.get("mediaType")
        && media_type.as_str() != Some(expected)
    {
        bail!("remote OCI manifest mediaType does not match Content-Type");
    }
    Ok(())
}

fn verify_descriptor(actual: &OciDescriptor, expected: &OciDescriptor, name: &str) -> Result<()> {
    if actual != expected {
        bail!(
            "{name} descriptor mismatch: expected {} ({} bytes, {}), received {} ({} bytes, {})",
            expected.digest,
            expected.size,
            expected.media_type,
            actual.digest,
            actual.size,
            actual.media_type
        );
    }
    Ok(())
}

fn response_media_type(headers: &HeaderMap) -> Result<String> {
    let value = headers
        .get(CONTENT_TYPE)
        .context("remote OCI manifest response lacks Content-Type")?
        .to_str()
        .context("remote OCI manifest Content-Type is not ASCII")?;
    Ok(value
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_owned())
}

fn require_success(response: Response, resource: &str) -> Result<Response> {
    if response.status().is_success() {
        return Ok(response);
    }
    bail!(
        "OCI registry {resource} request failed with HTTP {}",
        response.status()
    )
}

fn read_bounded_response(mut response: Response, limit: u64, name: &str) -> Result<Vec<u8>> {
    if let Some(length) = response
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        && length > limit
    {
        bail!("{name} exceeds the {limit}-byte limit");
    }
    let mut body = Vec::new();
    response
        .by_ref()
        .take(limit + 1)
        .read_to_end(&mut body)
        .with_context(|| format!("failed to read {name}"))?;
    if u64::try_from(body.len()).context("response is too large")? > limit {
        bail!("{name} exceeds the {limit}-byte limit");
    }
    Ok(body)
}

fn parse_bearer_challenge(value: &str) -> Result<BTreeMap<String, String>> {
    let (scheme, parameters) = value
        .split_once(char::is_whitespace)
        .context("OCI registry authentication challenge is malformed")?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        bail!("OCI registry requires unsupported authentication scheme: {scheme}");
    }
    let mut result = BTreeMap::new();
    let mut rest = parameters.trim();
    while !rest.is_empty() {
        let Some(equal) = rest.find('=') else {
            bail!("OCI Bearer challenge parameter is malformed");
        };
        let key = rest[..equal].trim().to_ascii_lowercase();
        rest = rest[equal + 1..].trim_start();
        if !rest.starts_with('"') {
            bail!("OCI Bearer challenge values must be quoted");
        }
        let (value, remaining) = parse_quoted(&rest[1..])?;
        if result.insert(key, value).is_some() {
            bail!("OCI Bearer challenge contains a duplicate parameter");
        }
        rest = remaining.trim_start();
        if rest.is_empty() {
            break;
        }
        rest = rest
            .strip_prefix(',')
            .context("OCI Bearer challenge parameters must be comma-separated")?
            .trim_start();
    }
    Ok(result)
}

fn parse_quoted(value: &str) -> Result<(String, &str)> {
    let mut parsed = String::new();
    let mut escaped = false;
    for (index, character) in value.char_indices() {
        if escaped {
            parsed.push(character);
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character == '"' {
            return Ok((parsed, &value[index + 1..]));
        } else if character == '\r' || character == '\n' {
            bail!("OCI Bearer challenge contains a line break");
        } else {
            parsed.push(character);
        }
    }
    bail!("OCI Bearer challenge contains an unterminated quoted value")
}

fn validate_registry(registry: &str) -> Result<()> {
    if registry.is_empty() || registry.contains(['@', '\\']) {
        bail!("invalid OCI registry host: {registry}");
    }
    let url = reqwest::Url::parse(&format!("https://{registry}/"))
        .context("invalid OCI registry host")?;
    if url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
    {
        bail!("invalid OCI registry host: {registry}");
    }
    Ok(())
}

fn validate_repository(repository: &str) -> Result<()> {
    if repository.is_empty()
        || repository.len() > 255
        || repository.split('/').any(|part| {
            !part
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphanumeric)
                || !part
                    .as_bytes()
                    .last()
                    .is_some_and(u8::is_ascii_alphanumeric)
                || !part.bytes().all(|byte| {
                    byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || matches!(byte, b'.' | b'_' | b'-')
                })
        })
    {
        bail!("invalid OCI repository name: {repository}");
    }
    Ok(())
}

fn validate_tag(tag: &str) -> Result<()> {
    let mut bytes = tag.bytes();
    let valid_first = bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphanumeric() || byte == b'_');
    if !valid_first
        || tag.len() > 128
        || !bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
    {
        bail!("invalid OCI tag: {tag}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::{BufRead as _, BufReader, Write as _};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant};

    use serde_json::json;
    use tar::{Builder, Header};

    use super::*;
    use crate::catalog::LocalImageCatalog;
    use crate::core::Architecture;
    use crate::image::ImageService;
    use crate::integrity::canonical_json;

    #[test]
    fn parses_explicit_remote_references() {
        let tagged = RemoteReference::parse("registry.example/team/agent:latest").expect("tagged");
        assert_eq!(tagged.registry, "registry.example");
        assert_eq!(tagged.repository, "team/agent");
        assert_eq!(tagged.selector, RemoteSelector::Tag("latest".to_owned()));
        assert_eq!(tagged.default_local_reference(), "team/agent:latest");

        let digest = format!("sha256:{}", "a".repeat(64));
        let pinned = RemoteReference::parse(&format!("registry.example/team/agent@{digest}"))
            .expect("pinned");
        assert_eq!(
            pinned.selector,
            RemoteSelector::Digest(Digest::parse(digest).expect("digest"))
        );
        assert_eq!(
            pinned.default_local_reference(),
            "team/agent:sha256-aaaaaaaaaaaa"
        );
    }

    #[test]
    fn rejects_ambiguous_or_noncanonical_remote_references() {
        for reference in [
            "agent:latest",
            "https://registry.example/team/agent:latest",
            "registry.example/team/agent",
            "registry.example/Team/agent:latest",
            "registry.example/team/-agent:latest",
            "registry.example/team/agent:bad tag",
            "registry.example/team/agent@sha256:broken",
        ] {
            assert!(
                RemoteReference::parse(reference).is_err(),
                "accepted {reference}"
            );
        }
    }

    #[test]
    fn bearer_parser_handles_quoted_commas_and_escapes() {
        let challenge = parse_bearer_challenge(
            r#"Bearer realm="http://127.0.0.1/token",service="registry,service",scope="repository:one\/two:pull""#,
        )
        .expect("challenge");
        assert_eq!(challenge["service"], "registry,service");
        assert_eq!(challenge["scope"], "repository:one/two:pull");
    }

    #[test]
    fn production_transport_is_https_only() {
        assert_eq!(DistributionClient::https().expect("client").scheme, "https");
        assert_eq!(
            DistributionClient::plain_http_for_tests()
                .expect("client")
                .scheme,
            "http"
        );
    }

    #[test]
    fn rejects_excessive_layer_count_before_blob_requests() {
        let layer = json!({
            "mediaType": OCI_LAYER_GZIP,
            "digest": format!("sha256:{}", "1".repeat(64)),
            "size": 1
        });
        let manifest = json!({
            "schemaVersion": 2,
            "mediaType": OCI_IMAGE_MANIFEST,
            "config": {
                "mediaType": OCI_IMAGE_CONFIG,
                "digest": format!("sha256:{}", "2".repeat(64)),
                "size": 1
            },
            "layers": vec![layer; MAX_IMAGE_LAYERS + 1]
        });

        let error = parse_image_manifest(&manifest).expect_err("Layer limit");
        assert!(error.to_string().contains("Layer limit"));
    }

    #[test]
    fn pulls_bearer_protected_index_by_exact_platform_and_updates_catalog_last() {
        let fixture = RegistryFixture::new();
        let registry = MockRegistry::start(fixture.clone(), false);
        let state = tempfile::tempdir().expect("state");
        let layout = OciLayout::open(state.path()).expect("layout");
        let images = ImageService::new(layout.clone());
        let remote = format!("{}/team/agent@{}", registry.address, fixture.index.digest);
        let result = crate::ingress::ImageIngress::new(&images)
            .pull_with(
                &DistributionClient::plain_http_for_tests().expect("client"),
                &remote,
                Platform::linux(Architecture::Arm64),
                Some("runlab/agent:test"),
                Some("test image"),
            )
            .expect("pull");

        assert_eq!(result.source_index, Some(fixture.index.clone()));
        assert_eq!(result.selected_manifest, fixture.manifest);
        assert_eq!(result.downloaded_blobs, 4);
        assert_eq!(result.downloaded_bytes, fixture.total_bytes());
        assert_eq!(result.local_reference, "runlab/agent:test");
        let entry = LocalImageCatalog::new(&layout)
            .resolve("runlab/agent:test")
            .expect("resolve")
            .expect("entry");
        assert_eq!(entry.manifest, fixture.manifest);
        assert_eq!(entry.platform, Some(Platform::linux(Architecture::Arm64)));
        assert_eq!(entry.metadata.description.as_deref(), Some("test image"));
        assert_eq!(
            entry.metadata.source.as_deref(),
            Some(format!("{}/team/agent@{}", registry.address, fixture.index.digest).as_str())
        );

        let requests = registry.finish();
        assert_eq!(requests.len(), 6);
        assert!(requests.iter().any(|request| {
            request.target.starts_with("/token?")
                && request.target.contains("service=mock-registry")
                && request
                    .target
                    .contains("scope=repository%3Ateam%2Fagent%3Apull")
        }));
        let protected = requests
            .iter()
            .filter(|request| request.target.starts_with("/v2/"))
            .collect::<Vec<_>>();
        assert_eq!(protected.len(), 5);
        assert_eq!(
            protected
                .iter()
                .filter(|request| !request.headers.contains_key("authorization"))
                .count(),
            1
        );
        assert!(protected.iter().skip(1).all(|request| {
            request.headers.get("authorization").map(String::as_str)
                == Some("Bearer opaque-test-token")
        }));
    }

    #[test]
    fn corrupt_stream_does_not_publish_catalog_reference_or_expose_token() {
        let fixture = RegistryFixture::new();
        let registry = MockRegistry::start(fixture, true);
        let state = tempfile::tempdir().expect("state");
        let layout = OciLayout::open(state.path()).expect("layout");
        let images = ImageService::new(layout.clone());
        let remote = format!("{}/team/agent:latest", registry.address);
        let error = crate::ingress::ImageIngress::new(&images)
            .pull_with(
                &DistributionClient::plain_http_for_tests().expect("client"),
                &remote,
                Platform::linux(Architecture::Arm64),
                Some("runlab/agent:broken"),
                None,
            )
            .expect_err("corrupt Layer");
        let message = format!("{error:#}");
        assert!(message.contains("digest mismatch"), "{message}");
        assert!(!message.contains("opaque-test-token"));
        assert!(
            LocalImageCatalog::new(&layout)
                .resolve("runlab/agent:broken")
                .expect("resolve")
                .is_none()
        );
        assert_eq!(registry.finish().len(), 6);
    }

    #[derive(Clone)]
    struct RegistryFixture {
        index: OciDescriptor,
        index_bytes: Vec<u8>,
        manifest: OciDescriptor,
        manifest_bytes: Vec<u8>,
        config: OciDescriptor,
        config_bytes: Vec<u8>,
        layer: OciDescriptor,
        layer_bytes: Vec<u8>,
    }

    impl RegistryFixture {
        fn new() -> Self {
            let layer_bytes = layer_bytes();
            let layer = descriptor(&layer_bytes, OCI_LAYER_TAR);
            let config_bytes = canonical_json(&json!({
                "architecture": "arm64",
                "config": {"Cmd": ["/bin/true"]},
                "os": "linux",
                "rootfs": {"type": "layers", "diff_ids": [layer.digest]}
            }))
            .expect("Config JSON");
            let config = descriptor(&config_bytes, OCI_IMAGE_CONFIG);
            let manifest_bytes = canonical_json(&json!({
                "schemaVersion": 2,
                "mediaType": OCI_IMAGE_MANIFEST,
                "config": descriptor_value(&config),
                "layers": [descriptor_value(&layer)]
            }))
            .expect("Manifest JSON");
            let manifest = descriptor(&manifest_bytes, OCI_IMAGE_MANIFEST);
            let other = OciDescriptor {
                digest: Digest::parse(format!("sha256:{}", "f".repeat(64))).expect("digest"),
                size: 123,
                media_type: OCI_IMAGE_MANIFEST.to_owned(),
            };
            let index_bytes = canonical_json(&json!({
                "schemaVersion": 2,
                "mediaType": OCI_IMAGE_INDEX,
                "manifests": [
                    index_entry(&other, "amd64"),
                    index_entry(&manifest, "arm64")
                ]
            }))
            .expect("Index JSON");
            let index = descriptor(&index_bytes, OCI_IMAGE_INDEX);
            Self {
                index,
                index_bytes,
                manifest,
                manifest_bytes,
                config,
                config_bytes,
                layer,
                layer_bytes,
            }
        }

        fn total_bytes(&self) -> u64 {
            [
                self.index.size,
                self.manifest.size,
                self.config.size,
                self.layer.size,
            ]
            .into_iter()
            .sum()
        }

        fn response(
            &self,
            request: &HttpRequest,
            address: &str,
            corrupt_layer: bool,
        ) -> HttpResponse {
            let authorized = request.headers.get("authorization").map(String::as_str)
                == Some("Bearer opaque-test-token");
            if request.target.starts_with("/v2/team/agent/manifests/") && !authorized {
                return HttpResponse {
                    status: "401 Unauthorized",
                    headers: vec![(
                        "WWW-Authenticate".to_owned(),
                        format!(
                            "Bearer realm=\"http://{address}/token\",service=\"mock-registry\",scope=\"repository:team/agent:pull\""
                        ),
                    )],
                    body: Vec::new(),
                };
            }
            if request.target.starts_with("/token?") {
                return HttpResponse::json(br#"{"token":"opaque-test-token"}"#.to_vec());
            }
            if !authorized {
                return HttpResponse::plain("403 Forbidden", b"authorization required".to_vec());
            }
            if request.target == "/v2/team/agent/manifests/latest"
                || request.target == format!("/v2/team/agent/manifests/{}", self.index.digest)
            {
                return HttpResponse::oci(&self.index, self.index_bytes.clone());
            }
            if request.target == format!("/v2/team/agent/manifests/{}", self.manifest.digest) {
                return HttpResponse::oci(&self.manifest, self.manifest_bytes.clone());
            }
            if request.target == format!("/v2/team/agent/blobs/{}", self.config.digest) {
                return HttpResponse::plain("200 OK", self.config_bytes.clone());
            }
            if request.target == format!("/v2/team/agent/blobs/{}", self.layer.digest) {
                let mut body = self.layer_bytes.clone();
                if corrupt_layer {
                    body[1024] ^= 1;
                }
                return HttpResponse::plain("200 OK", body);
            }
            HttpResponse::plain("404 Not Found", Vec::new())
        }
    }

    fn layer_bytes() -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut builder = Builder::new(&mut bytes);
            let contents = vec![0x5a; 2 * 1024 * 1024 + 17];
            let mut header = Header::new_ustar();
            header.set_path("payload").expect("path");
            header.set_size(u64::try_from(contents.len()).expect("size"));
            header.set_mode(0o644);
            header.set_uid(0);
            header.set_gid(0);
            header.set_mtime(0);
            header.set_cksum();
            builder
                .append(&header, contents.as_slice())
                .expect("append payload");
            builder.finish().expect("finish tar");
        }
        bytes
    }

    fn descriptor(bytes: &[u8], media_type: &str) -> OciDescriptor {
        OciDescriptor {
            digest: digest_bytes(bytes),
            size: u64::try_from(bytes.len()).expect("size"),
            media_type: media_type.to_owned(),
        }
    }

    fn descriptor_value(descriptor: &OciDescriptor) -> Value {
        json!({
            "mediaType": descriptor.media_type,
            "digest": descriptor.digest,
            "size": descriptor.size
        })
    }

    fn index_entry(descriptor: &OciDescriptor, architecture: &str) -> Value {
        let mut value = descriptor_value(descriptor);
        value["platform"] = json!({"os": "linux", "architecture": architecture});
        value
    }

    struct MockRegistry {
        address: String,
        stop: Arc<AtomicBool>,
        requests: Arc<Mutex<Vec<HttpRequest>>>,
        handle: Option<JoinHandle<()>>,
    }

    impl MockRegistry {
        fn start(fixture: RegistryFixture, corrupt_layer: bool) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind registry");
            listener.set_nonblocking(true).expect("nonblocking");
            let address = listener.local_addr().expect("registry address").to_string();
            let stop = Arc::new(AtomicBool::new(false));
            let requests = Arc::new(Mutex::new(Vec::new()));
            let thread_stop = Arc::clone(&stop);
            let thread_requests = Arc::clone(&requests);
            let thread_address = address.clone();
            let handle = thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(15);
                while !thread_stop.load(Ordering::Acquire) && Instant::now() < deadline {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            stream.set_nonblocking(false).expect("blocking stream");
                            let request = read_request(&stream).expect("request");
                            let response =
                                fixture.response(&request, &thread_address, corrupt_layer);
                            thread_requests.lock().expect("requests").push(request);
                            write_response(stream, response).expect("response");
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(2));
                        }
                        Err(error) => panic!("registry accept: {error}"),
                    }
                }
            });
            Self {
                address,
                stop,
                requests,
                handle: Some(handle),
            }
        }

        fn finish(mut self) -> Vec<HttpRequest> {
            self.stop.store(true, Ordering::Release);
            let _ = TcpStream::connect(&self.address);
            self.handle
                .take()
                .expect("server thread")
                .join()
                .expect("server");
            Arc::try_unwrap(self.requests)
                .expect("request owner")
                .into_inner()
                .expect("requests")
        }
    }

    #[derive(Debug)]
    struct HttpRequest {
        target: String,
        headers: BTreeMap<String, String>,
    }

    struct HttpResponse {
        status: &'static str,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    impl HttpResponse {
        fn plain(status: &'static str, body: Vec<u8>) -> Self {
            Self {
                status,
                headers: Vec::new(),
                body,
            }
        }

        fn json(body: Vec<u8>) -> Self {
            Self {
                status: "200 OK",
                headers: vec![("Content-Type".to_owned(), "application/json".to_owned())],
                body,
            }
        }

        fn oci(descriptor: &OciDescriptor, body: Vec<u8>) -> Self {
            Self {
                status: "200 OK",
                headers: vec![
                    ("Content-Type".to_owned(), descriptor.media_type.clone()),
                    (
                        "Docker-Content-Digest".to_owned(),
                        descriptor.digest.to_string(),
                    ),
                ],
                body,
            }
        }
    }

    fn read_request(stream: &TcpStream) -> std::io::Result<HttpRequest> {
        let mut reader = BufReader::new(stream);
        let mut request_line = String::new();
        reader.read_line(&mut request_line)?;
        let target = request_line
            .split_ascii_whitespace()
            .nth(1)
            .unwrap_or_default()
            .to_owned();
        let mut headers = BTreeMap::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line)?;
            if line == "\r\n" || line.is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':') {
                headers.insert(name.to_ascii_lowercase(), value.trim().to_owned());
            }
        }
        Ok(HttpRequest { target, headers })
    }

    fn write_response(mut stream: TcpStream, response: HttpResponse) -> std::io::Result<()> {
        write!(
            stream,
            "HTTP/1.1 {}\r\nContent-Length: {}\r\nConnection: close\r\n",
            response.status,
            response.body.len()
        )?;
        for (name, value) in response.headers {
            write!(stream, "{name}: {value}\r\n")?;
        }
        stream.write_all(b"\r\n")?;
        stream.write_all(&response.body)
    }
}
