use std::fmt;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{Cursor, Read};
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use flate2::read::GzDecoder;
use oci_spec::image::{Descriptor, ImageConfiguration, ImageIndex, ImageManifest, MediaType, Os};
use run_engine::OciContentStore;
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest as _, Sha256};

use crate::metadata::Metadata;
use crate::storage::{Database, LocalOciStore};

const MAX_PAGE_SIZE: usize = 1_000;

#[derive(Clone, Debug)]
pub(crate) enum ImageSelector {
    Digest(String),
    Name(String),
}

impl FromStr for ImageSelector {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        if value.starts_with("sha256:") {
            validate_digest(value)?;
            Ok(Self::Digest(value.to_owned()))
        } else {
            validate_name(value)?;
            Ok(Self::Name(value.to_owned()))
        }
    }
}

impl fmt::Display for ImageSelector {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Digest(value) | Self::Name(value) => value.fmt(formatter),
        }
    }
}

pub(crate) struct Images<'a> {
    store: Arc<LocalOciStore>,
    database: &'a Database,
}

#[derive(Debug, Serialize)]
pub(crate) struct ImageImportResult {
    schema_version: u32,
    name: String,
    manifest: Descriptor,
    platform: ImagePlatform,
    metadata: Metadata,
    imported_blobs: usize,
}

#[derive(Debug, Serialize)]
pub(crate) struct ImageListResult {
    schema_version: u32,
    images: Vec<ImageListItem>,
    next_after: Option<String>,
}

#[derive(Debug, Serialize)]
struct ImageListItem {
    name: String,
    manifest: Value,
    metadata: Metadata,
}

#[derive(Debug, Serialize)]
pub(crate) struct ImageGetResult {
    schema_version: u32,
    requested: String,
    manifest: Descriptor,
    config: Descriptor,
    layers: Vec<Descriptor>,
    platform: ImagePlatform,
    metadata: Option<Metadata>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ImagePlatform {
    os: String,
    architecture: String,
    variant: Option<String>,
}

pub(crate) struct InspectedImage {
    pub(crate) manifest: Descriptor,
    pub(crate) config: Descriptor,
    pub(crate) layers: Vec<Descriptor>,
    pub(crate) platform: ImagePlatform,
    pub(crate) image_configuration: ImageConfiguration,
}

pub(crate) struct ImageFilesystem {
    store: Arc<LocalOciStore>,
    pub(crate) manifest: Descriptor,
    pub(crate) layers: Vec<Descriptor>,
}

impl ImageFilesystem {
    pub(crate) fn read_layer(&self, descriptor: &Descriptor) -> Result<Vec<u8>> {
        self.store.read(descriptor)
    }
}

impl<'a> Images<'a> {
    pub(crate) fn new(store: Arc<LocalOciStore>, database: &'a Database) -> Self {
        Self { store, database }
    }

    pub(crate) fn import(
        &self,
        source: &Path,
        name: &str,
        metadata: &Metadata,
    ) -> Result<ImageImportResult> {
        validate_name(name)?;
        if source.is_dir() {
            return self.import_layout(source, name, metadata);
        }
        if !source.is_file() {
            bail!("OCI Image source does not exist: {}", source.display());
        }
        let temporary = tempfile::tempdir().context("failed to create OCI archive workspace")?;
        tar::Archive::new(
            File::open(source)
                .with_context(|| format!("failed to open OCI archive {}", source.display()))?,
        )
        .unpack(temporary.path())
        .with_context(|| format!("failed to unpack OCI archive {}", source.display()))?;
        self.import_layout(temporary.path(), name, metadata)
    }

    pub(crate) fn list(&self, limit: usize, after: Option<&str>) -> Result<ImageListResult> {
        validate_page_size(limit)?;
        if let Some(after) = after {
            validate_name(after)?;
        }
        let mut entries = self.database.catalog_list(limit + 1, after)?;
        let has_more = entries.len() > limit;
        if has_more {
            entries.truncate(limit);
        }
        let next_after = has_more
            .then(|| entries.last().map(|entry| entry.0.clone()))
            .flatten();
        Ok(ImageListResult {
            schema_version: 1,
            images: entries
                .into_iter()
                .map(|(name, manifest, metadata)| ImageListItem {
                    name,
                    manifest,
                    metadata,
                })
                .collect(),
            next_after,
        })
    }

    pub(crate) fn get(&self, selector: &ImageSelector) -> Result<ImageGetResult> {
        let (image, metadata) = match selector {
            ImageSelector::Name(name) => {
                let (descriptor, metadata) = self
                    .database
                    .catalog_get(name)?
                    .with_context(|| format!("local Image name is unknown: {name}"))?;
                let descriptor = serde_json::from_value(descriptor)
                    .context("stored Image descriptor is invalid")?;
                (inspect_descriptor(&self.store, descriptor)?, Some(metadata))
            }
            ImageSelector::Digest(_) => (self.inspect(selector)?, None),
        };
        Ok(ImageGetResult {
            schema_version: 1,
            requested: selector.to_string(),
            manifest: image.manifest,
            config: image.config,
            layers: image.layers,
            platform: image.platform,
            metadata,
        })
    }

    pub(crate) fn resolve(&self, selector: &ImageSelector) -> Result<InspectedImage> {
        self.inspect(selector)
    }

    pub(crate) fn filesystem(&self, selector: &ImageSelector) -> Result<ImageFilesystem> {
        self.filesystem_from_manifest(self.resolve_descriptor(selector)?)
    }

    pub(crate) fn filesystem_from_manifest(&self, manifest: Descriptor) -> Result<ImageFilesystem> {
        // A path read verifies every Descriptor it touches without turning the
        // lookup into a full-Image DiffID verification pass.
        let (manifest_view, _) = read_image_structure(&self.store, &manifest)?;
        Ok(ImageFilesystem {
            store: Arc::clone(&self.store),
            manifest,
            layers: manifest_view.layers().clone(),
        })
    }

    fn resolve_descriptor(&self, selector: &ImageSelector) -> Result<Descriptor> {
        match selector {
            ImageSelector::Digest(digest) => self.store.manifest_descriptor(digest),
            ImageSelector::Name(name) => {
                let (value, _) = self
                    .database
                    .catalog_get(name)?
                    .with_context(|| format!("local Image name is unknown: {name}"))?;
                serde_json::from_value(value).context("stored Image descriptor is invalid")
            }
        }
    }

    fn import_layout(
        &self,
        layout: &Path,
        name: &str,
        metadata: &Metadata,
    ) -> Result<ImageImportResult> {
        validate_layout_marker(layout)?;
        let index_bytes = fs::read(layout.join("index.json"))
            .context("OCI Image Layout does not contain a readable index.json")?;
        let index: ImageIndex =
            serde_json::from_slice(&index_bytes).context("OCI index.json is invalid")?;
        let manifests = index
            .manifests()
            .iter()
            .filter(|descriptor| descriptor.media_type() == &MediaType::ImageManifest)
            .collect::<Vec<_>>();
        let [manifest] = manifests.as_slice() else {
            bail!(
                "OCI index.json must contain exactly one Image Manifest; found {}",
                manifests.len()
            );
        };

        let manifest_bytes = read_layout_blob(layout, manifest)?;
        let manifest_view: ImageManifest =
            serde_json::from_slice(&manifest_bytes).context("OCI Image Manifest is invalid")?;
        let config_bytes = read_layout_blob(layout, manifest_view.config())?;
        let config: ImageConfiguration =
            serde_json::from_slice(&config_bytes).context("OCI Image Config is invalid")?;
        validate_config(&config, manifest_view.layers())?;

        let mut blobs = Vec::with_capacity(manifest_view.layers().len() + 2);
        blobs.push(((*manifest).clone(), manifest_bytes));
        blobs.push((manifest_view.config().clone(), config_bytes));
        for (layer, diff_id) in manifest_view
            .layers()
            .iter()
            .zip(config.rootfs().diff_ids())
        {
            let bytes = read_layout_blob(layout, layer)?;
            verify_diff_id(layer, &bytes, diff_id)?;
            blobs.push((layer.clone(), bytes));
        }

        for (descriptor, bytes) in &mut blobs {
            self.store.publish(descriptor, &mut Cursor::new(bytes))?;
        }
        let inspected = inspect_descriptor(&self.store, (*manifest).clone())?;
        let descriptor_value = serde_json::to_value(&inspected.manifest)?;
        self.database
            .catalog_set(name, &descriptor_value, metadata, &Utc::now().to_rfc3339())?;
        Ok(ImageImportResult {
            schema_version: 1,
            name: name.to_owned(),
            manifest: inspected.manifest,
            platform: inspected.platform,
            metadata: metadata.clone(),
            imported_blobs: blobs.len(),
        })
    }

    fn inspect(&self, selector: &ImageSelector) -> Result<InspectedImage> {
        inspect_descriptor(&self.store, self.resolve_descriptor(selector)?)
    }
}

fn inspect_descriptor(store: &LocalOciStore, manifest: Descriptor) -> Result<InspectedImage> {
    let (manifest_view, config) = read_image_structure(store, &manifest)?;
    for (layer, diff_id) in manifest_view
        .layers()
        .iter()
        .zip(config.rootfs().diff_ids())
    {
        let bytes = store.read(layer)?;
        verify_diff_id(layer, &bytes, diff_id)?;
    }
    let platform = ImagePlatform {
        os: config.os().to_string(),
        architecture: config.architecture().to_string(),
        variant: config.variant().clone(),
    };
    Ok(InspectedImage {
        manifest,
        config: manifest_view.config().clone(),
        layers: manifest_view.layers().clone(),
        platform,
        image_configuration: config,
    })
}

fn read_image_structure(
    store: &LocalOciStore,
    manifest: &Descriptor,
) -> Result<(ImageManifest, ImageConfiguration)> {
    if manifest.media_type() != &MediaType::ImageManifest {
        bail!("selected descriptor is not an OCI Image Manifest");
    }
    let manifest_bytes = store.read(manifest)?;
    let manifest_view: ImageManifest =
        serde_json::from_slice(&manifest_bytes).context("OCI Image Manifest is invalid")?;
    let config_bytes = store.read(manifest_view.config())?;
    let config: ImageConfiguration =
        serde_json::from_slice(&config_bytes).context("OCI Image Config is invalid")?;
    validate_config(&config, manifest_view.layers())?;
    Ok((manifest_view, config))
}

fn validate_config(config: &ImageConfiguration, layers: &[Descriptor]) -> Result<()> {
    if config.os() != &Os::Linux {
        bail!("OCI Image Config must target Linux");
    }
    if config.rootfs().typ() != "layers" {
        bail!("OCI Image Config rootfs.type must be layers");
    }
    if layers.len() != config.rootfs().diff_ids().len() {
        bail!("OCI Image Manifest layers do not match Config rootfs.diff_ids");
    }
    Ok(())
}

fn validate_layout_marker(layout: &Path) -> Result<()> {
    let value: Value = serde_json::from_slice(
        &fs::read(layout.join("oci-layout"))
            .context("OCI Image Layout does not contain a readable oci-layout file")?,
    )
    .context("OCI oci-layout file is invalid JSON")?;
    if value.get("imageLayoutVersion").and_then(Value::as_str) != Some("1.0.0") {
        bail!("OCI Image Layout version must be 1.0.0");
    }
    Ok(())
}

fn read_layout_blob(layout: &Path, descriptor: &Descriptor) -> Result<Vec<u8>> {
    let digest = descriptor.digest().to_string();
    let encoded = validate_digest(&digest)?;
    let path = layout.join("blobs/sha256").join(encoded);
    let bytes =
        fs::read(&path).with_context(|| format!("OCI Layout content is unavailable: {digest}"))?;
    verify_descriptor_bytes(descriptor, &bytes)?;
    Ok(bytes)
}

fn validate_digest(digest: &str) -> Result<&str> {
    let encoded = digest
        .strip_prefix("sha256:")
        .with_context(|| format!("unsupported OCI digest algorithm: {digest}"))?;
    if encoded.len() != 64
        || !encoded
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("invalid sha256 digest: {digest}");
    }
    Ok(encoded)
}

fn verify_descriptor_bytes(descriptor: &Descriptor, bytes: &[u8]) -> Result<()> {
    let expected_size = usize::try_from(descriptor.size()).context("negative OCI content size")?;
    if bytes.len() != expected_size {
        bail!("OCI content size does not match {}", descriptor.digest());
    }
    let actual = sha256_digest(bytes);
    if actual != descriptor.digest().to_string() {
        bail!("OCI content digest does not match {}", descriptor.digest());
    }
    Ok(())
}

fn verify_diff_id(descriptor: &Descriptor, bytes: &[u8], expected: &str) -> Result<()> {
    let mut reader = layer_reader(descriptor, bytes)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let digest = hasher.finalize();
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    let actual = format!("sha256:{encoded}");
    if actual != expected {
        bail!("OCI Layer DiffID does not match Config: {actual} != {expected}");
    }
    Ok(())
}

pub(crate) fn layer_reader<'a>(
    descriptor: &Descriptor,
    bytes: &'a [u8],
) -> Result<Box<dyn Read + 'a>> {
    let media_type = descriptor.media_type().to_string();
    match media_type.as_str() {
        "application/vnd.oci.image.layer.v1.tar" => Ok(Box::new(Cursor::new(bytes))),
        "application/vnd.oci.image.layer.v1.tar+gzip" => {
            Ok(Box::new(GzDecoder::new(Cursor::new(bytes))))
        }
        "application/vnd.oci.image.layer.v1.tar+zstd" => Ok(Box::new(
            zstd::stream::read::Decoder::new(Cursor::new(bytes))?,
        )),
        _ => bail!("unsupported OCI Layer media type: {media_type}"),
    }
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() || name.trim() != name || name.chars().any(char::is_whitespace) {
        bail!("Image name must be non-empty and contain no whitespace");
    }
    if name.starts_with("sha256:") {
        bail!("Image name cannot use the sha256 digest prefix");
    }
    Ok(())
}

fn validate_page_size(limit: usize) -> Result<()> {
    if !(1..=MAX_PAGE_SIZE).contains(&limit) {
        bail!("--limit must be between 1 and {MAX_PAGE_SIZE}");
    }
    Ok(())
}

fn sha256_digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    format!("sha256:{encoded}")
}
