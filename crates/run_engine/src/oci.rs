use std::fmt::Write as _;
use std::io::{self, Cursor, Read, Seek, SeekFrom};

use flate2::read::MultiGzDecoder;
use oci_spec::image::{
    Descriptor, Digest, ImageConfiguration, ImageManifest, MediaType, Os, Platform,
};
use run_protocol::{ImageDescriptor, ImageDescriptorError};
use serde::de::{Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value, json};
use sha2::{Digest as _, Sha256};
use thiserror::Error;

use crate::{ContentError, OciContentStore};

const MAX_IMAGE_LAYERS: usize = 1024;
const MAX_MANIFEST_BYTES: u64 = 16 * 1024 * 1024;
const MAX_CONFIG_BYTES: u64 = 16 * 1024 * 1024;
const MAX_TOTAL_COMPRESSED_LAYER_BYTES: u64 = 64 * 1024 * 1024 * 1024;
const MAX_TOTAL_UNCOMPRESSED_LAYER_BYTES: u64 = 64 * 1024 * 1024 * 1024;
const FILESYSTEM_CAPTURE_HISTORY: &str = "run_engine filesystem capture";

#[derive(Clone, Copy)]
struct ImageLimits {
    layers: usize,
    manifest_bytes: u64,
    config_bytes: u64,
    compressed_layer_bytes: u64,
    uncompressed_layer_bytes: u64,
}

const IMAGE_LIMITS: ImageLimits = ImageLimits {
    layers: MAX_IMAGE_LAYERS,
    manifest_bytes: MAX_MANIFEST_BYTES,
    config_bytes: MAX_CONFIG_BYTES,
    compressed_layer_bytes: MAX_TOTAL_COMPRESSED_LAYER_BYTES,
    uncompressed_layer_bytes: MAX_TOTAL_UNCOMPRESSED_LAYER_BYTES,
};

/// Stable category for mapping OCI pipeline failures into an Engine error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OciErrorKind {
    Content,
    Descriptor,
    Json,
    Image,
    Layer,
}

/// A failure while verifying or publishing OCI Image content.
#[derive(Debug, Error)]
pub(crate) enum OciError {
    #[error("failed to {operation} OCI content at {path}: {source}")]
    Content {
        operation: &'static str,
        path: String,
        #[source]
        source: ContentError,
    },
    #[error("failed to stream OCI content at {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("OCI descriptor at {path} has mediaType {actual}; expected one of {expected}")]
    MediaType {
        path: String,
        expected: String,
        actual: String,
    },
    #[error("OCI descriptor at {path} uses unsupported digest {digest}; only sha256 is supported")]
    DigestAlgorithm { path: String, digest: String },
    #[error("OCI content at {path} has size {actual}; descriptor requires {expected}")]
    Size {
        path: String,
        expected: u64,
        actual: u64,
    },
    #[error("OCI content at {path} has digest {actual}; descriptor requires {expected}")]
    Digest {
        path: String,
        expected: String,
        actual: String,
    },
    #[error("OCI JSON at {path} is invalid: {source}")]
    Json {
        path: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("OCI JSON at {path} is {size} bytes; limit is {limit} bytes")]
    JsonLimit { path: String, limit: u64, size: u64 },
    #[error("invalid OCI Image field {path}: {reason}")]
    Image { path: String, reason: String },
    #[error("failed to decode OCI Image Layer at {path}: {source}")]
    LayerDecode {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("OCI Image Layers at {path} expand to {actual} bytes; limit is {limit} bytes")]
    LayerLimit {
        path: String,
        limit: u64,
        actual: u64,
    },
    #[error("OCI Image Layers at {path} declare {actual} compressed bytes; limit is {limit} bytes")]
    CompressedLayerLimit {
        path: String,
        limit: u64,
        actual: u64,
    },
    #[error("OCI Image Layer at {path} has DiffID {actual}; image config requires {expected}")]
    DiffId {
        path: String,
        expected: String,
        actual: String,
    },
    #[error("published manifest is not an OCI Image descriptor: {source}")]
    ImageDescriptor {
        #[source]
        source: ImageDescriptorError,
    },
}

impl OciError {
    pub(crate) const fn kind(&self) -> OciErrorKind {
        match self {
            Self::Content { .. } | Self::Io { .. } => OciErrorKind::Content,
            Self::MediaType { .. }
            | Self::DigestAlgorithm { .. }
            | Self::Size { .. }
            | Self::Digest { .. }
            | Self::ImageDescriptor { .. } => OciErrorKind::Descriptor,
            Self::Json { .. } | Self::JsonLimit { .. } => OciErrorKind::Json,
            Self::Image { .. } => OciErrorKind::Image,
            Self::LayerDecode { .. }
            | Self::LayerLimit { .. }
            | Self::CompressedLayerLimit { .. }
            | Self::DiffId { .. } => OciErrorKind::Layer,
        }
    }
}

/// Exact bytes read and verified against a complete OCI Descriptor.
#[derive(Clone, Debug)]
pub(crate) struct VerifiedContent {
    descriptor: Descriptor,
    bytes: Vec<u8>,
}

impl VerifiedContent {
    #[cfg(test)]
    pub(crate) fn descriptor(&self) -> &Descriptor {
        &self.descriptor
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// A verified Layer reference. Layer bytes are never retained in memory.
#[derive(Clone, Debug)]
pub(crate) struct VerifiedLayer {
    descriptor: Descriptor,
    diff_id: Digest,
}

impl VerifiedLayer {
    pub(crate) fn descriptor(&self) -> &Descriptor {
        &self.descriptor
    }

    pub(crate) fn diff_id(&self) -> &Digest {
        &self.diff_id
    }
}

/// A Linux OCI Image whose manifest, config, Layers, and `DiffIDs` were verified.
#[derive(Clone, Debug)]
pub(crate) struct VerifiedImage {
    manifest: VerifiedContent,
    manifest_value: Value,
    config: VerifiedContent,
    config_value: Value,
    platform: Platform,
    layers: Vec<VerifiedLayer>,
    #[cfg(test)]
    diff_ids: Vec<Digest>,
    total_compressed_layer_bytes: u64,
    total_uncompressed_layer_bytes: u64,
}

impl VerifiedImage {
    #[cfg(test)]
    pub(crate) fn manifest(&self) -> &VerifiedContent {
        &self.manifest
    }

    pub(crate) fn config(&self) -> &VerifiedContent {
        &self.config
    }

    pub(crate) fn platform(&self) -> &Platform {
        &self.platform
    }

    pub(crate) fn layers(&self) -> &[VerifiedLayer] {
        &self.layers
    }

    #[cfg(test)]
    pub(crate) fn diff_ids(&self) -> &[Digest] {
        &self.diff_ids
    }
}

/// Reads bounded exact bytes and verifies all identity fields of `descriptor`.
pub(crate) fn read_small_verified(
    store: &dyn OciContentStore,
    descriptor: &Descriptor,
    expected_media_types: &[MediaType],
    limit: u64,
    path: impl Into<String>,
) -> Result<VerifiedContent, OciError> {
    let path = path.into();
    verify_media_type(descriptor, expected_media_types, &path)?;
    if descriptor.size() > limit {
        return Err(OciError::JsonLimit {
            path,
            limit,
            size: descriptor.size(),
        });
    }
    let mut content = store.open(descriptor).map_err(|source| OciError::Content {
        operation: "open",
        path: path.clone(),
        source,
    })?;
    let capacity = usize::try_from(descriptor.size()).map_err(|_| OciError::JsonLimit {
        path: path.clone(),
        limit,
        size: descriptor.size(),
    })?;
    let mut bytes = Vec::with_capacity(capacity);
    let read_limit = descriptor_read_limit(descriptor, &path)?;
    content
        .by_ref()
        .take(read_limit)
        .read_to_end(&mut bytes)
        .map_err(|source| layer_io_error(&path, source))?;
    verify_bytes(descriptor, &bytes, &path)?;
    verify_embedded_bytes(descriptor, &bytes, &path)?;
    Ok(VerifiedContent {
        descriptor: descriptor.clone(),
        bytes,
    })
}

/// Opens and stream-verifies content without retaining its bytes.
pub(crate) fn verify_content(
    store: &dyn OciContentStore,
    descriptor: &Descriptor,
    expected_media_types: &[MediaType],
    path: impl Into<String>,
) -> Result<(), OciError> {
    let path = path.into();
    verify_media_type(descriptor, expected_media_types, &path)?;
    let mut content = store.open(descriptor).map_err(|source| OciError::Content {
        operation: "open",
        path: path.clone(),
        source,
    })?;
    verify_reader(descriptor, &mut content, &path)
}

/// Verifies and atomically publishes exact bytes under a complete Descriptor.
///
/// A successful return also proves that the published content can be read back
/// through the same complete Descriptor.
pub(crate) fn publish_expected(
    store: &dyn OciContentStore,
    descriptor: &Descriptor,
    content: &mut dyn Read,
    expected_media_types: &[MediaType],
    path: impl Into<String>,
) -> Result<(), OciError> {
    let path = path.into();
    verify_media_type(descriptor, expected_media_types, &path)?;
    store
        .publish(descriptor, content)
        .map_err(|source| OciError::Content {
            operation: "publish",
            path: path.clone(),
            source,
        })?;
    verify_content(store, descriptor, expected_media_types, path)
}

/// Computes a complete sha256 Descriptor and atomically publishes its bytes.
#[cfg(test)]
pub(crate) fn publish_content<R: Read + Seek>(
    store: &dyn OciContentStore,
    media_type: MediaType,
    content: &mut R,
    path: impl Into<String>,
) -> Result<Descriptor, OciError> {
    let path = path.into();
    let (size, digest) = digest_stream(content, &path)?;
    content
        .seek(SeekFrom::Start(0))
        .map_err(|source| layer_io_error(&path, source))?;
    let descriptor = Descriptor::new(media_type.clone(), size, digest);
    publish_expected(store, &descriptor, content, &[media_type], path)?;
    Ok(descriptor)
}

/// Reads and verifies a complete OCI Image, retaining every exact source byte.
pub(crate) fn inspect_image(
    store: &dyn OciContentStore,
    image: &ImageDescriptor,
) -> Result<VerifiedImage, OciError> {
    inspect_image_with_limits(store, image, IMAGE_LIMITS)
}

fn inspect_image_with_limits(
    store: &dyn OciContentStore,
    image: &ImageDescriptor,
    limits: ImageLimits,
) -> Result<VerifiedImage, OciError> {
    let manifest = read_small_verified(
        store,
        image.as_oci(),
        &[MediaType::ImageManifest],
        limits.manifest_bytes,
        "manifest",
    )?;
    let (manifest_value, manifest_view) = parse_manifest(&manifest)?;

    let config = read_small_verified(
        store,
        manifest_view.config(),
        &[MediaType::ImageConfig],
        limits.config_bytes,
        "manifest.config",
    )?;
    let (config_value, config_view) = parse_config(&config)?;
    let platform = linux_platform(&config_value, &config_view)?;
    verify_descriptor_platform(image.as_oci(), &platform, "manifest")?;
    verify_descriptor_platform(manifest_view.config(), &platform, "manifest.config")?;
    let diff_ids = config_diff_ids(&config_value)?;

    if manifest_view.layers().len() != diff_ids.len() {
        return Err(image_error(
            "manifest.layers",
            format!(
                "contains {} entries but config.rootfs.diff_ids contains {}",
                manifest_view.layers().len(),
                diff_ids.len()
            ),
        ));
    }

    let total_compressed = preflight_layers(manifest_view.layers(), limits)?;

    let mut layers = Vec::with_capacity(manifest_view.layers().len());
    let mut total_uncompressed = 0_u64;
    for (index, (descriptor, expected_diff_id)) in
        manifest_view.layers().iter().zip(&diff_ids).enumerate()
    {
        let path = format!("manifest.layers[{index}]");
        let remaining = limits
            .uncompressed_layer_bytes
            .checked_sub(total_uncompressed)
            .expect("accounted Layer bytes never exceed the limit");
        let uncompressed = verify_layer(store, descriptor, expected_diff_id, &path, remaining)?;
        total_uncompressed = total_uncompressed
            .checked_add(uncompressed)
            .ok_or_else(|| image_error("manifest.layers", "uncompressed size overflow"))?;
        layers.push(VerifiedLayer {
            descriptor: descriptor.clone(),
            diff_id: expected_diff_id.clone(),
        });
    }

    Ok(VerifiedImage {
        manifest,
        manifest_value,
        config,
        config_value,
        platform,
        layers,
        #[cfg(test)]
        diff_ids,
        total_compressed_layer_bytes: total_compressed,
        total_uncompressed_layer_bytes: total_uncompressed,
    })
}

/// Publishes a final Image derived from `parent` and one already-published Layer.
///
/// Config is published first. Every referenced Config and Layer is then read
/// and verified before the Manifest is published last. The fixed history label
/// contains no clock, product, or Run identity, so the same parent and
/// filesystem change always produce the same bytes. `None` returns the complete
/// parent Descriptor unchanged and publishes no empty child Image.
pub(crate) fn publish_final_image(
    store: &dyn OciContentStore,
    parent: &ImageDescriptor,
    added_layer: Option<(Descriptor, Digest)>,
) -> Result<ImageDescriptor, OciError> {
    publish_final_image_with_limits(store, parent, added_layer, IMAGE_LIMITS)
}

fn publish_final_image_with_limits(
    store: &dyn OciContentStore,
    parent: &ImageDescriptor,
    added_layer: Option<(Descriptor, Digest)>,
    limits: ImageLimits,
) -> Result<ImageDescriptor, OciError> {
    let parent = inspect_image_with_limits(store, parent, limits)?;
    let Some((layer_descriptor, diff_id)) = added_layer else {
        return ImageDescriptor::new(parent.manifest.descriptor.clone())
            .map_err(|source| OciError::ImageDescriptor { source });
    };
    let final_descriptors = parent
        .layers
        .iter()
        .map(|layer| layer.descriptor.clone())
        .chain(std::iter::once(layer_descriptor.clone()))
        .collect::<Vec<_>>();
    let final_compressed = preflight_layers(&final_descriptors, limits)?;
    let expected_compressed = parent
        .total_compressed_layer_bytes
        .checked_add(layer_descriptor.size())
        .ok_or_else(|| image_error("final.layers", "compressed size overflow"))?;
    debug_assert_eq!(final_compressed, expected_compressed);
    let remaining_uncompressed = limits
        .uncompressed_layer_bytes
        .checked_sub(parent.total_uncompressed_layer_bytes)
        .expect("a verified parent cannot exceed its uncompressed Layer limit");
    let added_uncompressed = verify_layer(
        store,
        &layer_descriptor,
        &diff_id,
        "final.layer",
        remaining_uncompressed,
    )?;
    let _final_uncompressed = parent
        .total_uncompressed_layer_bytes
        .checked_add(added_uncompressed)
        .ok_or_else(|| image_error("final.layers", "uncompressed size overflow"))?;

    let config_value = final_config_value(&parent.config_value, &diff_id)?;
    validate_config_value(&config_value, "final.config")?;
    let config_bytes = json_bytes(&config_value, "final.config")?;
    enforce_generated_json_limit(&config_bytes, limits.config_bytes, "final.config")?;
    let config = descriptor_for_bytes(MediaType::ImageConfig, &config_bytes);

    let manifest_value = final_manifest_value(&parent.manifest_value, &config, &layer_descriptor)?;
    validate_manifest_value(&manifest_value, "final.manifest")?;
    let manifest_bytes = json_bytes(&manifest_value, "final.manifest")?;
    enforce_generated_json_limit(&manifest_bytes, limits.manifest_bytes, "final.manifest")?;
    let mut config_reader = Cursor::new(config_bytes);
    let mut manifest_reader = Cursor::new(manifest_bytes);
    let manifest_descriptor =
        descriptor_for_bytes(MediaType::ImageManifest, manifest_reader.get_ref());

    // Both generated JSON objects and the complete final Layer set have been
    // validated before this first publication.
    publish_expected(
        store,
        &config,
        &mut config_reader,
        &[MediaType::ImageConfig],
        "final.config",
    )?;

    // Re-establish availability immediately before the commit point. No
    // Manifest publication is attempted if any referenced content disappeared.
    verify_content(
        store,
        &config,
        &[MediaType::ImageConfig],
        "final.manifest.config",
    )?;
    for (index, descriptor) in parent
        .layers
        .iter()
        .map(VerifiedLayer::descriptor)
        .chain(std::iter::once(&layer_descriptor))
        .enumerate()
    {
        verify_content(
            store,
            descriptor,
            supported_layer_media_types(),
            format!("final.manifest.layers[{index}]"),
        )?;
    }

    publish_expected(
        store,
        &manifest_descriptor,
        &mut manifest_reader,
        &[MediaType::ImageManifest],
        "final.manifest",
    )?;
    ImageDescriptor::new(manifest_descriptor).map_err(|source| OciError::ImageDescriptor { source })
}

fn enforce_generated_json_limit(bytes: &[u8], limit: u64, path: &str) -> Result<(), OciError> {
    let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    if size > limit {
        return Err(OciError::JsonLimit {
            path: path.to_owned(),
            limit,
            size,
        });
    }
    Ok(())
}

fn verify_media_type(
    descriptor: &Descriptor,
    expected: &[MediaType],
    path: &str,
) -> Result<(), OciError> {
    if expected.iter().any(|item| item == descriptor.media_type()) {
        return Ok(());
    }
    Err(OciError::MediaType {
        path: path.to_owned(),
        expected: expected
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", "),
        actual: descriptor.media_type().to_string(),
    })
}

fn verify_bytes(descriptor: &Descriptor, bytes: &[u8], path: &str) -> Result<(), OciError> {
    let actual_size = u64::try_from(bytes.len()).map_err(|_| OciError::Size {
        path: path.to_owned(),
        expected: descriptor.size(),
        actual: u64::MAX,
    })?;
    if descriptor.size() != actual_size {
        return Err(OciError::Size {
            path: path.to_owned(),
            expected: descriptor.size(),
            actual: actual_size,
        });
    }
    let Some(expected) = descriptor.as_digest_sha256() else {
        return Err(OciError::DigestAlgorithm {
            path: path.to_owned(),
            digest: descriptor.digest().to_string(),
        });
    };
    let actual_hex = hex_sha256(bytes);
    if expected != actual_hex {
        return Err(OciError::Digest {
            path: path.to_owned(),
            expected: descriptor.digest().to_string(),
            actual: format!("sha256:{actual_hex}"),
        });
    }
    Ok(())
}

fn verify_reader(
    descriptor: &Descriptor,
    reader: &mut dyn Read,
    path: &str,
) -> Result<(), OciError> {
    let Some(expected) = descriptor.as_digest_sha256() else {
        return Err(OciError::DigestAlgorithm {
            path: path.to_owned(),
            digest: descriptor.digest().to_string(),
        });
    };
    let embedded = decode_descriptor_data(descriptor, path)?;
    if let Some(bytes) = &embedded
        && u64::try_from(bytes.len()).unwrap_or(u64::MAX) != descriptor.size()
    {
        return Err(image_error(
            format!("{path}.data"),
            format!(
                "decodes to {} bytes but descriptor.size is {}",
                bytes.len(),
                descriptor.size()
            ),
        ));
    }
    let read_limit = descriptor_read_limit(descriptor, path)?;
    let mut bounded = reader.take(read_limit);
    let mut hasher = Sha256::new();
    let mut actual_size = 0_u64;
    let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
    while let count @ 1.. = bounded
        .read(&mut buffer)
        .map_err(|source| layer_io_error(path, source))?
    {
        if let Some(bytes) = &embedded {
            let offset = usize::try_from(actual_size)
                .map_err(|_| image_error(format!("{path}.data"), "comparison offset overflow"))?;
            let end = offset.saturating_add(count);
            if bytes.get(offset..end) != Some(&buffer[..count]) {
                return Err(image_error(
                    format!("{path}.data"),
                    "decoded bytes do not equal the target content",
                ));
            }
        }
        actual_size = actual_size
            .checked_add(u64::try_from(count).expect("read count fits u64"))
            .ok_or_else(|| image_error(path, "content size overflow"))?;
        hasher.update(&buffer[..count]);
    }
    let actual_digest = Digest::try_from(format!("sha256:{}", lowercase_hex(&hasher.finalize())))
        .expect("a SHA-256 result is always a valid OCI digest");
    if descriptor.size() != actual_size {
        return Err(OciError::Size {
            path: path.to_owned(),
            expected: descriptor.size(),
            actual: actual_size,
        });
    }
    if expected != actual_digest.digest() {
        return Err(OciError::Digest {
            path: path.to_owned(),
            expected: descriptor.digest().to_string(),
            actual: actual_digest.to_string(),
        });
    }
    Ok(())
}

fn descriptor_read_limit(descriptor: &Descriptor, path: &str) -> Result<u64, OciError> {
    descriptor
        .size()
        .checked_add(1)
        .ok_or_else(|| image_error(format!("{path}.size"), "cannot be bounded by size + 1"))
}

fn verify_embedded_bytes(
    descriptor: &Descriptor,
    bytes: &[u8],
    path: &str,
) -> Result<(), OciError> {
    if let Some(embedded) = decode_descriptor_data(descriptor, path)?
        && embedded != bytes
    {
        return Err(image_error(
            format!("{path}.data"),
            "decoded bytes do not equal the target content",
        ));
    }
    Ok(())
}

fn decode_descriptor_data(
    descriptor: &Descriptor,
    path: &str,
) -> Result<Option<Vec<u8>>, OciError> {
    descriptor
        .data()
        .as_deref()
        .map(|encoded| {
            let expected_encoded_size =
                descriptor
                    .size()
                    .div_ceil(3)
                    .checked_mul(4)
                    .ok_or_else(|| {
                        image_error(format!("{path}.data"), "encoded size calculation overflow")
                    })?;
            if u64::try_from(encoded.len()).unwrap_or(u64::MAX) != expected_encoded_size {
                return Err(image_error(
                    format!("{path}.data"),
                    format!(
                        "has {} encoded bytes; descriptor.size {} requires {expected_encoded_size}",
                        encoded.len(),
                        descriptor.size()
                    ),
                ));
            }
            decode_base64(encoded).map_err(|reason| image_error(format!("{path}.data"), reason))
        })
        .transpose()
}

fn decode_base64(encoded: &str) -> Result<Vec<u8>, String> {
    let input = encoded.as_bytes();
    if !input.len().is_multiple_of(4) {
        return Err("must be padded RFC 4648 base64".to_owned());
    }
    let mut decoded = Vec::with_capacity(input.len() / 4 * 3);
    for (chunk_index, chunk) in input.chunks_exact(4).enumerate() {
        let last = chunk_index + 1 == input.len() / 4;
        let first = base64_value(chunk[0])?;
        let second = base64_value(chunk[1])?;
        decoded.push((first << 2) | (second >> 4));
        match (chunk[2], chunk[3]) {
            (b'=', b'=') if last && second.trailing_zeros() >= 4 => {}
            (third, b'=') if last => {
                let third = base64_value(third)?;
                if third.trailing_zeros() < 2 {
                    return Err("has non-zero padding bits".to_owned());
                }
                decoded.push((second << 4) | (third >> 2));
            }
            (b'=', _) => return Err("has invalid padding".to_owned()),
            (third, fourth) => {
                let third = base64_value(third)?;
                let fourth = base64_value(fourth)?;
                decoded.push((second << 4) | (third >> 2));
                decoded.push((third << 6) | fourth);
            }
        }
    }
    Ok(decoded)
}

fn base64_value(byte: u8) -> Result<u8, String> {
    match byte {
        b'A'..=b'Z' => Ok(byte - b'A'),
        b'a'..=b'z' => Ok(byte - b'a' + 26),
        b'0'..=b'9' => Ok(byte - b'0' + 52),
        b'+' => Ok(62),
        b'/' => Ok(63),
        _ => Err(format!("contains invalid base64 byte 0x{byte:02x}")),
    }
}

fn parse_manifest(content: &VerifiedContent) -> Result<(Value, ImageManifest), OciError> {
    let value = parse_unique_json(&content.bytes, "manifest")?;
    let manifest = validate_manifest_value(&value, "manifest")?;
    Ok((value, manifest))
}

fn validate_manifest_value(value: &Value, path: &str) -> Result<ImageManifest, OciError> {
    let manifest: ImageManifest =
        serde_json::from_value(value.clone()).map_err(|source| OciError::Json {
            path: path.to_owned(),
            source,
        })?;
    if manifest.schema_version() != 2 {
        return Err(image_error(format!("{path}.schemaVersion"), "must equal 2"));
    }
    if let Some(media_type) = value.get("mediaType")
        && media_type.as_str() != Some(MediaType::ImageManifest.as_ref())
    {
        return Err(image_error(
            format!("{path}.mediaType"),
            format!("must be {} when present", MediaType::ImageManifest),
        ));
    }
    if manifest.config().media_type() != &MediaType::ImageConfig {
        return Err(image_error(
            format!("{path}.config.mediaType"),
            format!(
                "must be {}; received {}",
                MediaType::ImageConfig,
                manifest.config().media_type()
            ),
        ));
    }
    Ok(manifest)
}

fn parse_config(content: &VerifiedContent) -> Result<(Value, ImageConfiguration), OciError> {
    let value = parse_unique_json(&content.bytes, "manifest.config")?;
    let config = validate_config_value(&value, "manifest.config")?;
    Ok((value, config))
}

fn validate_config_value(value: &Value, path: &str) -> Result<ImageConfiguration, OciError> {
    let config: ImageConfiguration =
        serde_json::from_value(value.clone()).map_err(|source| OciError::Json {
            path: path.to_owned(),
            source,
        })?;
    if config.rootfs().typ() != "layers" {
        return Err(image_error(
            format!("{path}.rootfs.type"),
            "must equal layers",
        ));
    }
    if config.os() != &Os::Linux {
        return Err(image_error(
            format!("{path}.os"),
            format!("must equal linux; received {}", config.os()),
        ));
    }
    if let Some(history) = config.history() {
        let filesystem_entries = history
            .iter()
            .filter(|entry| entry.empty_layer() != Some(true))
            .count();
        if filesystem_entries != config.rootfs().diff_ids().len() {
            return Err(image_error(
                format!("{path}.history"),
                format!(
                    "contains {filesystem_entries} filesystem entries but rootfs.diff_ids contains {}",
                    config.rootfs().diff_ids().len()
                ),
            ));
        }
    }
    Ok(config)
}

fn linux_platform(value: &Value, config: &ImageConfiguration) -> Result<Platform, OciError> {
    let object = value
        .as_object()
        .ok_or_else(|| image_error("manifest.config", "must be a JSON object"))?;
    let mut platform = Map::new();
    copy_required(object, &mut platform, "architecture", "manifest.config")?;
    copy_required(object, &mut platform, "os", "manifest.config")?;
    copy_optional(object, &mut platform, "os.version");
    copy_optional(object, &mut platform, "os.features");
    copy_optional(object, &mut platform, "variant");
    let platform: Platform =
        serde_json::from_value(Value::Object(platform)).map_err(|source| OciError::Json {
            path: "manifest.config platform".to_owned(),
            source,
        })?;
    if platform.os() != &Os::Linux || platform.architecture() != config.architecture() {
        return Err(image_error(
            "manifest.config platform",
            "does not match the validated Image Configuration",
        ));
    }
    Ok(platform)
}

fn verify_descriptor_platform(
    descriptor: &Descriptor,
    actual: &Platform,
    path: &str,
) -> Result<(), OciError> {
    if let Some(declared) = descriptor.platform()
        && declared != actual
    {
        return Err(image_error(
            format!("{path}.platform"),
            format!(
                "does not match Image Config platform {}/{:?}",
                actual.os(),
                actual.architecture()
            ),
        ));
    }
    Ok(())
}

fn copy_required(
    source: &Map<String, Value>,
    target: &mut Map<String, Value>,
    field: &str,
    path: &str,
) -> Result<(), OciError> {
    let value = source
        .get(field)
        .ok_or_else(|| image_error(format!("{path}.{field}"), "is required"))?;
    target.insert(field.to_owned(), value.clone());
    Ok(())
}

fn copy_optional(source: &Map<String, Value>, target: &mut Map<String, Value>, field: &str) {
    if let Some(value) = source.get(field) {
        target.insert(field.to_owned(), value.clone());
    }
}

fn config_diff_ids(value: &Value) -> Result<Vec<Digest>, OciError> {
    let entries = value
        .get("rootfs")
        .and_then(Value::as_object)
        .and_then(|rootfs| rootfs.get("diff_ids"))
        .and_then(Value::as_array)
        .ok_or_else(|| image_error("manifest.config.rootfs.diff_ids", "must be an array"))?;
    entries
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let path = format!("manifest.config.rootfs.diff_ids[{index}]");
            let text = value
                .as_str()
                .ok_or_else(|| image_error(&path, "must be a digest string"))?;
            let digest =
                Digest::try_from(text).map_err(|error| image_error(&path, error.to_string()))?;
            if digest.algorithm().as_ref() != "sha256" {
                return Err(image_error(&path, "only sha256 DiffIDs are supported"));
            }
            Ok(digest)
        })
        .collect()
}

fn preflight_layers(descriptors: &[Descriptor], limits: ImageLimits) -> Result<u64, OciError> {
    if descriptors.len() > limits.layers {
        return Err(image_error(
            "manifest.layers",
            format!(
                "contains {} entries; limit is {}",
                descriptors.len(),
                limits.layers
            ),
        ));
    }
    let mut total_compressed = 0_u64;
    for (index, descriptor) in descriptors.iter().enumerate() {
        let path = format!("manifest.layers[{index}]");
        verify_media_type(descriptor, supported_layer_media_types(), &path)?;
        total_compressed = total_compressed
            .checked_add(descriptor.size())
            .ok_or_else(|| image_error("manifest.layers", "compressed size overflow"))?;
        if total_compressed > limits.compressed_layer_bytes {
            return Err(OciError::CompressedLayerLimit {
                path: "manifest.layers".to_owned(),
                limit: limits.compressed_layer_bytes,
                actual: total_compressed,
            });
        }
    }
    Ok(total_compressed)
}

fn verify_layer(
    store: &dyn OciContentStore,
    descriptor: &Descriptor,
    expected: &Digest,
    path: &str,
    uncompressed_limit: u64,
) -> Result<u64, OciError> {
    verify_media_type(descriptor, supported_layer_media_types(), path)?;
    if expected.algorithm().as_ref() != "sha256" {
        return Err(image_error(path, "only sha256 DiffIDs are supported"));
    }
    let mut content = store.open(descriptor).map_err(|source| OciError::Content {
        operation: "open",
        path: path.to_owned(),
        source,
    })?;
    verify_reader(descriptor, &mut content, path)?;
    content
        .seek(SeekFrom::Start(0))
        .map_err(|source| layer_io_error(path, source))?;
    let mut bounded_content = content.take(descriptor.size());
    let (uncompressed, actual) = layer_diff_id(
        descriptor.media_type(),
        &mut bounded_content,
        path,
        uncompressed_limit,
    )?;
    if &actual != expected {
        return Err(OciError::DiffId {
            path: path.to_owned(),
            expected: expected.to_string(),
            actual: actual.to_string(),
        });
    }
    Ok(uncompressed)
}

fn layer_diff_id(
    media_type: &MediaType,
    content: &mut dyn Read,
    path: &str,
    uncompressed_limit: u64,
) -> Result<(u64, Digest), OciError> {
    let mut reader: Box<dyn Read + '_> = match media_type {
        MediaType::ImageLayer | MediaType::ImageLayerNonDistributable => Box::new(content),
        MediaType::ImageLayerGzip | MediaType::ImageLayerNonDistributableGzip => {
            Box::new(MultiGzDecoder::new(content))
        }
        MediaType::ImageLayerZstd | MediaType::ImageLayerNonDistributableZstd => Box::new(
            zstd::stream::read::Decoder::new(content)
                .map_err(|source| layer_decode_error(path, source))?,
        ),
        media_type => {
            return Err(OciError::MediaType {
                path: path.to_owned(),
                expected: supported_layer_media_types()
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", "),
                actual: media_type.to_string(),
            });
        }
    };
    digest_stream_limited(&mut reader, path, uncompressed_limit)
}

fn digest_stream_limited<R: Read + ?Sized>(
    reader: &mut R,
    path: &str,
    limit: u64,
) -> Result<(u64, Digest), OciError> {
    let observation_limit = limit
        .checked_add(1)
        .ok_or_else(|| image_error(path, "streaming limit cannot be bounded by limit + 1"))?;
    let mut bounded = reader.take(observation_limit);
    let mut hasher = Sha256::new();
    let mut size = 0_u64;
    let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
    loop {
        let count = bounded
            .read(&mut buffer)
            .map_err(|source| layer_io_error(path, source))?;
        if count == 0 {
            break;
        }
        size = size
            .checked_add(u64::try_from(count).expect("read count fits u64"))
            .ok_or_else(|| image_error(path, "content size overflow"))?;
        if size > limit {
            return Err(OciError::LayerLimit {
                path: path.to_owned(),
                limit,
                actual: size,
            });
        }
        hasher.update(&buffer[..count]);
    }
    let digest = Digest::try_from(format!("sha256:{}", lowercase_hex(&hasher.finalize())))
        .expect("a SHA-256 result is always a valid OCI digest");
    Ok((size, digest))
}

#[cfg(test)]
fn digest_stream<R: Read + ?Sized>(reader: &mut R, path: &str) -> Result<(u64, Digest), OciError> {
    let mut hasher = Sha256::new();
    let mut size = 0_u64;
    let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
    loop {
        let count = reader
            .read(&mut buffer)
            .map_err(|source| layer_io_error(path, source))?;
        if count == 0 {
            break;
        }
        size = size
            .checked_add(u64::try_from(count).expect("read count fits u64"))
            .ok_or_else(|| image_error(path, "content size overflow"))?;
        hasher.update(&buffer[..count]);
    }
    let digest = Digest::try_from(format!("sha256:{}", lowercase_hex(&hasher.finalize())))
        .expect("a SHA-256 result is always a valid OCI digest");
    Ok((size, digest))
}

fn final_config_value(parent: &Value, diff_id: &Digest) -> Result<Value, OciError> {
    let mut value = parent.clone();
    let object = value
        .as_object_mut()
        .ok_or_else(|| image_error("parent.config", "must be a JSON object"))?;
    let rootfs = object
        .get_mut("rootfs")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| image_error("parent.config.rootfs", "must be an object"))?;
    let diff_ids = rootfs
        .get_mut("diff_ids")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| image_error("parent.config.rootfs.diff_ids", "must be an array"))?;
    diff_ids.push(Value::String(diff_id.to_string()));

    if let Some(history) = object.get_mut("history") {
        history
            .as_array_mut()
            .ok_or_else(|| image_error("parent.config.history", "must be an array when present"))?
            .push(json!({
                "created_by": FILESYSTEM_CAPTURE_HISTORY,
                "empty_layer": false
            }));
    }
    Ok(value)
}

fn final_manifest_value(
    parent: &Value,
    config: &Descriptor,
    layer: &Descriptor,
) -> Result<Value, OciError> {
    let mut value = parent.clone();
    let object = value
        .as_object_mut()
        .ok_or_else(|| image_error("parent.manifest", "must be a JSON object"))?;
    object.insert(
        "config".to_owned(),
        serde_json::to_value(config).expect("an OCI Descriptor is always serializable"),
    );
    object
        .get_mut("layers")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| image_error("parent.manifest.layers", "must be an array"))?
        .push(serde_json::to_value(layer).expect("an OCI Descriptor is always serializable"));
    Ok(value)
}

fn json_bytes(value: &Value, path: &str) -> Result<Vec<u8>, OciError> {
    serde_json::to_vec(value).map_err(|source| OciError::Json {
        path: path.to_owned(),
        source,
    })
}

fn parse_unique_json(bytes: &[u8], path: &str) -> Result<Value, OciError> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let UniqueValue(value) =
        UniqueValue::deserialize(&mut deserializer).map_err(|source| OciError::Json {
            path: path.to_owned(),
            source,
        })?;
    deserializer.end().map_err(|source| OciError::Json {
        path: path.to_owned(),
        source,
    })?;
    Ok(value)
}

struct UniqueValue(Value);

impl<'de> Deserialize<'de> for UniqueValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(UniqueValueVisitor)
    }
}

struct UniqueValueVisitor;

impl<'de> Visitor<'de> for UniqueValueVisitor {
    type Value = UniqueValue;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a JSON value without duplicate object keys")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::Bool(value)))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::Number(value.into())))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::Number(value.into())))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        serde_json::Number::from_f64(value)
            .map(Value::Number)
            .map(UniqueValue)
            .ok_or_else(|| E::custom("JSON number is not finite"))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::String(value.to_owned())))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::String(value)))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::Null))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::Null))
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(UniqueValue(value)) = sequence.next_element::<UniqueValue>()? {
            values.push(value);
        }
        Ok(UniqueValue(Value::Array(values)))
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut values = Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if values.contains_key(&key) {
                return Err(serde::de::Error::custom(format!(
                    "duplicate JSON key: {key}"
                )));
            }
            let UniqueValue(value) = map.next_value::<UniqueValue>()?;
            values.insert(key, value);
        }
        Ok(UniqueValue(Value::Object(values)))
    }
}

fn descriptor_for_bytes(media_type: MediaType, bytes: &[u8]) -> Descriptor {
    let size = u64::try_from(bytes.len()).expect("usize always fits in u64 on supported targets");
    Descriptor::new(
        media_type,
        size,
        Digest::try_from(format!("sha256:{}", hex_sha256(bytes)))
            .expect("a SHA-256 result is always a valid OCI digest"),
    )
}

fn hex_sha256(bytes: &[u8]) -> String {
    lowercase_hex(&Sha256::digest(bytes))
}

fn lowercase_hex(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    encoded
}

fn supported_layer_media_types() -> &'static [MediaType] {
    const MEDIA_TYPES: &[MediaType] = &[
        MediaType::ImageLayer,
        MediaType::ImageLayerGzip,
        MediaType::ImageLayerZstd,
        MediaType::ImageLayerNonDistributable,
        MediaType::ImageLayerNonDistributableGzip,
        MediaType::ImageLayerNonDistributableZstd,
    ];
    MEDIA_TYPES
}

fn image_error(path: impl Into<String>, reason: impl Into<String>) -> OciError {
    OciError::Image {
        path: path.into(),
        reason: reason.into(),
    }
}

fn layer_io_error(path: &str, source: io::Error) -> OciError {
    OciError::Io {
        path: path.to_owned(),
        source,
    }
}

fn layer_decode_error(path: &str, source: io::Error) -> OciError {
    OciError::LayerDecode {
        path: path.to_owned(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};
    use std::io::{Cursor, Read, Seek, SeekFrom, Write};
    use std::sync::Mutex;

    use flate2::{Compression, write::GzEncoder};
    use oci_spec::image::DigestAlgorithm;

    use super::*;
    use crate::{ContentErrorKind, OciContent, OciContentStore};

    #[derive(Default)]
    struct MemoryStore {
        blobs: Mutex<HashMap<String, Vec<u8>>>,
        opened: Mutex<Vec<String>>,
        published_media_types: Mutex<Vec<MediaType>>,
        fail_read_media_type: Mutex<Option<MediaType>>,
        fail_reads_after_publish: Mutex<Option<MediaType>>,
    }

    impl MemoryStore {
        fn insert_unchecked(&self, descriptor: &Descriptor, bytes: impl AsRef<[u8]>) {
            self.blobs
                .lock()
                .expect("blobs lock")
                .insert(descriptor.digest().to_string(), bytes.as_ref().to_vec());
        }

        fn fail_reads_after_publish(&self, media_type: MediaType) {
            *self
                .fail_reads_after_publish
                .lock()
                .expect("publish failure lock") = Some(media_type);
        }

        fn published(&self, media_type: &MediaType) -> bool {
            self.published_media_types
                .lock()
                .expect("publications lock")
                .contains(media_type)
        }

        fn clear_publications(&self) {
            self.published_media_types
                .lock()
                .expect("publications lock")
                .clear();
        }
    }

    impl OciContentStore for MemoryStore {
        fn open(&self, descriptor: &Descriptor) -> Result<Box<dyn OciContent>, ContentError> {
            self.opened
                .lock()
                .expect("opened lock")
                .push(descriptor.digest().to_string());
            if self
                .fail_read_media_type
                .lock()
                .expect("failure lock")
                .as_ref()
                == Some(descriptor.media_type())
            {
                return Err(ContentError::new(
                    ContentErrorKind::Unavailable,
                    "injected read failure",
                ));
            }
            self.blobs
                .lock()
                .expect("blobs lock")
                .get(&descriptor.digest().to_string())
                .cloned()
                .map(|bytes| Box::new(Cursor::new(bytes)) as Box<dyn OciContent>)
                .ok_or_else(|| {
                    ContentError::new(ContentErrorKind::Unavailable, "content is absent")
                })
        }

        fn publish(
            &self,
            descriptor: &Descriptor,
            content: &mut dyn Read,
        ) -> Result<(), ContentError> {
            let mut bytes = Vec::new();
            content.read_to_end(&mut bytes).map_err(|error| {
                ContentError::new(ContentErrorKind::Internal, error.to_string())
            })?;
            let mut blobs = self.blobs.lock().expect("blobs lock");
            match blobs.get(&descriptor.digest().to_string()) {
                Some(existing) if existing != &bytes => Err(ContentError::new(
                    ContentErrorKind::Rejected,
                    "conflicting content",
                )),
                Some(_) => Ok(()),
                None => {
                    blobs.insert(descriptor.digest().to_string(), bytes);
                    self.published_media_types
                        .lock()
                        .expect("publications lock")
                        .push(descriptor.media_type().clone());
                    if self
                        .fail_reads_after_publish
                        .lock()
                        .expect("publish failure lock")
                        .as_ref()
                        == Some(descriptor.media_type())
                    {
                        *self.fail_read_media_type.lock().expect("failure lock") =
                            Some(descriptor.media_type().clone());
                    }
                    Ok(())
                }
            }
        }
    }

    struct EndlessStore;

    impl OciContentStore for EndlessStore {
        fn open(&self, _descriptor: &Descriptor) -> Result<Box<dyn OciContent>, ContentError> {
            Ok(Box::new(EndlessContent))
        }

        fn publish(
            &self,
            _descriptor: &Descriptor,
            _content: &mut dyn Read,
        ) -> Result<(), ContentError> {
            Err(ContentError::new(
                ContentErrorKind::Rejected,
                "test store is read-only",
            ))
        }
    }

    struct EndlessContent;

    impl Read for EndlessContent {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            buffer.fill(0);
            Ok(buffer.len())
        }
    }

    impl Seek for EndlessContent {
        fn seek(&mut self, _position: SeekFrom) -> io::Result<u64> {
            Ok(0)
        }
    }

    struct CountingReader<R> {
        inner: R,
        bytes_read: usize,
        largest_request: usize,
    }

    impl<R> CountingReader<R> {
        const fn new(inner: R) -> Self {
            Self {
                inner,
                bytes_read: 0,
                largest_request: 0,
            }
        }
    }

    impl<R: Read> Read for CountingReader<R> {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.largest_request = self.largest_request.max(buffer.len());
            let count = self.inner.read(buffer)?;
            self.bytes_read += count;
            Ok(count)
        }
    }

    #[test]
    fn read_rejects_media_type_size_and_digest_mismatches() {
        let store = MemoryStore::default();
        let bytes = b"content";
        let valid = descriptor_for_bytes(MediaType::ImageConfig, bytes);
        store.insert_unchecked(&valid, bytes.as_slice());

        let media_error = read_small_verified(
            &store,
            &valid,
            &[MediaType::ImageManifest],
            MAX_CONFIG_BYTES,
            "subject",
        )
        .expect_err("media type mismatch");
        assert_eq!(media_error.kind(), OciErrorKind::Descriptor);

        let wrong_size = Descriptor::new(
            MediaType::ImageConfig,
            valid.size() + 1,
            valid.digest().clone(),
        );
        store.insert_unchecked(&wrong_size, bytes.as_slice());
        assert!(matches!(
            read_small_verified(
                &store,
                &wrong_size,
                &[MediaType::ImageConfig],
                MAX_CONFIG_BYTES,
                "subject"
            ),
            Err(OciError::Size { .. })
        ));

        let wrong_digest = Descriptor::new(
            MediaType::ImageConfig,
            u64::try_from(bytes.len()).expect("size"),
            Digest::try_from(format!("sha256:{}", "0".repeat(64))).expect("digest"),
        );
        store.insert_unchecked(&wrong_digest, bytes.as_slice());
        assert!(matches!(
            read_small_verified(
                &store,
                &wrong_digest,
                &[MediaType::ImageConfig],
                MAX_CONFIG_BYTES,
                "subject"
            ),
            Err(OciError::Digest { .. })
        ));
    }

    #[test]
    fn complete_descriptors_and_exact_json_bytes_are_preserved() {
        let store = MemoryStore::default();
        let config_bytes = config_bytes(&[]);
        let mut config = descriptor_for_bytes(MediaType::ImageConfig, &config_bytes);
        config.set_urls(Some(vec!["https://example.test/config".to_owned()]));
        config.set_annotations(Some(HashMap::from([(
            "config".to_owned(),
            "preserved".to_owned(),
        )])));
        config.set_platform(Some(linux_platform_value("amd64")));
        config.set_artifact_type(Some(MediaType::ImageConfig));
        config.set_data(Some(base64_encode(&config_bytes)));
        store.insert_unchecked(&config, &config_bytes);

        let manifest_bytes = format!(
            "{{\n  \"schemaVersion\": 2, \"config\": {}, \"layers\": []\n}}",
            serde_json::to_string(&config).expect("descriptor JSON")
        )
        .into_bytes();
        let mut descriptor = descriptor_for_bytes(MediaType::ImageManifest, &manifest_bytes);
        descriptor.set_urls(Some(vec!["https://example.test/manifest".to_owned()]));
        descriptor.set_annotations(Some(HashMap::from([(
            "example".to_owned(),
            "value".to_owned(),
        )])));
        descriptor.set_platform(Some(linux_platform_value("amd64")));
        descriptor.set_artifact_type(Some(MediaType::ImageConfig));
        descriptor.set_data(Some(base64_encode(&manifest_bytes)));
        store.insert_unchecked(&descriptor, manifest_bytes.clone());
        let image_descriptor = ImageDescriptor::new(descriptor.clone()).expect("image descriptor");

        let image = inspect_image(&store, &image_descriptor).expect("verified image");
        assert_eq!(image.manifest().descriptor(), &descriptor);
        assert_eq!(image.config().descriptor(), &config);
        assert_eq!(image.manifest().bytes(), manifest_bytes);
        assert_eq!(image.platform().os(), &Os::Linux);
    }

    #[test]
    fn embedded_descriptor_data_must_equal_target_bytes() {
        let store = MemoryStore::default();
        let bytes = b"target";
        let mut descriptor = descriptor_for_bytes(MediaType::ImageConfig, bytes);
        descriptor.set_data(Some(base64_encode(b"other!")));
        store.insert_unchecked(&descriptor, bytes);

        let error = verify_content(&store, &descriptor, &[MediaType::ImageConfig], "config")
            .expect_err("embedded data mismatch");

        assert_eq!(error.kind(), OciErrorKind::Image);
        assert!(error.to_string().contains("config.data"));
    }

    #[test]
    fn config_descriptor_platform_must_match_config() {
        let store = MemoryStore::default();
        let config_bytes = config_bytes_for_platform("arm64", Some("v8"), &[]);
        let mut config = descriptor_for_bytes(MediaType::ImageConfig, &config_bytes);
        config.set_platform(Some(
            serde_json::from_value(
                json!({"architecture": "arm64", "os": "linux", "variant": "v9"}),
            )
            .expect("platform"),
        ));
        store.insert_unchecked(&config, config_bytes);
        let manifest_bytes = serde_json::to_vec(&json!({
            "schemaVersion": 2,
            "config": config,
            "layers": []
        }))
        .expect("manifest JSON");
        let manifest = descriptor_for_bytes(MediaType::ImageManifest, &manifest_bytes);
        store.insert_unchecked(&manifest, manifest_bytes);
        let image = ImageDescriptor::new(manifest).expect("image descriptor");

        let error = inspect_image(&store, &image).expect_err("platform mismatch");

        assert_eq!(error.kind(), OciErrorKind::Image);
        assert!(error.to_string().contains("manifest.config.platform"));
    }

    #[test]
    fn manifest_rejects_duplicate_object_keys() {
        let store = MemoryStore::default();
        let config_bytes = config_bytes(&[]);
        let config = descriptor_for_bytes(MediaType::ImageConfig, &config_bytes);
        store.insert_unchecked(&config, config_bytes);
        let manifest_bytes = format!(
            "{{\"schemaVersion\":2,\"schemaVersion\":2,\"config\":{},\"layers\":[]}}",
            serde_json::to_string(&config).expect("descriptor JSON")
        )
        .into_bytes();
        let manifest = descriptor_for_bytes(MediaType::ImageManifest, &manifest_bytes);
        store.insert_unchecked(&manifest, manifest_bytes);
        let image = ImageDescriptor::new(manifest).expect("image descriptor");

        let error = inspect_image(&store, &image).expect_err("duplicate manifest key");

        assert_eq!(error.kind(), OciErrorKind::Json);
        assert!(
            error
                .to_string()
                .contains("duplicate JSON key: schemaVersion")
        );
    }

    #[test]
    fn config_rejects_duplicate_object_keys() {
        let store = MemoryStore::default();
        let config_bytes = br#"{
            "architecture":"amd64",
            "os":"linux",
            "os":"linux",
            "rootfs":{"type":"layers","diff_ids":[]}
        }"#;
        let config = descriptor_for_bytes(MediaType::ImageConfig, config_bytes);
        store.insert_unchecked(&config, config_bytes);
        let manifest_bytes = serde_json::to_vec(&json!({
            "schemaVersion": 2,
            "config": config,
            "layers": []
        }))
        .expect("manifest JSON");
        let manifest = descriptor_for_bytes(MediaType::ImageManifest, &manifest_bytes);
        store.insert_unchecked(&manifest, manifest_bytes);
        let image = ImageDescriptor::new(manifest).expect("image descriptor");

        let error = inspect_image(&store, &image).expect_err("duplicate config key");

        assert_eq!(error.kind(), OciErrorKind::Json);
        assert!(error.to_string().contains("duplicate JSON key: os"));
    }

    #[test]
    fn verifies_all_six_oci_layer_media_types_against_uncompressed_diff_ids() {
        let store = MemoryStore::default();
        let raw = b"deterministic tar bytes";
        let mut gzip = GzEncoder::new(Vec::new(), Compression::default());
        gzip.write_all(raw).expect("gzip write");
        let gzip = gzip.finish().expect("gzip finish");
        let zstd = zstd::stream::encode_all(raw.as_slice(), 3).expect("zstd");
        let diff_id = digest_for(raw);
        let layers = [
            (MediaType::ImageLayer, raw.to_vec()),
            (MediaType::ImageLayerGzip, gzip.clone()),
            (MediaType::ImageLayerZstd, zstd.clone()),
            (MediaType::ImageLayerNonDistributable, raw.to_vec()),
            (MediaType::ImageLayerNonDistributableGzip, gzip),
            (MediaType::ImageLayerNonDistributableZstd, zstd),
        ];

        for (media_type, bytes) in layers {
            let layer = publish_bytes(&store, media_type, &bytes, "layer");
            verify_layer(
                &store,
                &layer,
                &diff_id,
                "layer",
                MAX_TOTAL_UNCOMPRESSED_LAYER_BYTES,
            )
            .expect("DiffID");
        }

        let bad = digest_for(b"different");
        let layer = publish_bytes(&store, MediaType::ImageLayer, raw, "bad layer");
        assert!(matches!(
            verify_layer(
                &store,
                &layer,
                &bad,
                "bad layer",
                MAX_TOTAL_UNCOMPRESSED_LAYER_BYTES
            ),
            Err(OciError::DiffId { .. })
        ));
    }

    #[test]
    fn store_readers_are_bounded_at_descriptor_size_plus_one() {
        let descriptor = descriptor_for_bytes(MediaType::ImageLayer, b"abc");
        let extra_store = MemoryStore::default();
        extra_store.insert_unchecked(&descriptor, b"abcd");
        assert!(matches!(
            verify_content(&extra_store, &descriptor, &[MediaType::ImageLayer], "extra"),
            Err(OciError::Size {
                expected: 3,
                actual: 4,
                ..
            })
        ));

        let endless = EndlessStore;
        assert!(matches!(
            verify_content(&endless, &descriptor, &[MediaType::ImageLayer], "endless"),
            Err(OciError::Size {
                expected: 3,
                actual: 4,
                ..
            })
        ));
    }

    #[test]
    fn cumulative_compressed_layer_size_is_rejected_before_any_layer_open() {
        let store = MemoryStore::default();
        let diff_ids = [digest_for(b"one"), digest_for(b"two")];
        let config_bytes = config_bytes(&diff_ids);
        let config = descriptor_for_bytes(MediaType::ImageConfig, &config_bytes);
        store.insert_unchecked(&config, config_bytes);
        let first = Descriptor::new(
            MediaType::ImageLayer,
            MAX_TOTAL_COMPRESSED_LAYER_BYTES,
            digest_for(b"first"),
        );
        let second = Descriptor::new(MediaType::ImageLayer, 1, digest_for(b"second"));
        let manifest_bytes = serde_json::to_vec(&json!({
            "schemaVersion": 2,
            "config": config,
            "layers": [&first, &second]
        }))
        .expect("manifest JSON");
        let manifest = descriptor_for_bytes(MediaType::ImageManifest, &manifest_bytes);
        store.insert_unchecked(&manifest, manifest_bytes);
        let image = ImageDescriptor::new(manifest).expect("image descriptor");

        let error = inspect_image(&store, &image).expect_err("compressed size limit");

        assert!(matches!(error, OciError::CompressedLayerLimit { .. }));
        let opened = store.opened.lock().expect("opened lock");
        assert!(!opened.contains(&first.digest().to_string()));
        assert!(!opened.contains(&second.digest().to_string()));
    }

    #[test]
    fn later_layer_stops_at_remaining_uncompressed_budget_plus_one() {
        let store = MemoryStore::default();
        let image = publish_image(
            &store,
            &[
                (MediaType::ImageLayer, b"one".to_vec()),
                (MediaType::ImageLayer, b"two".to_vec()),
            ],
        );
        let limits = ImageLimits {
            uncompressed_layer_bytes: 5,
            ..IMAGE_LIMITS
        };

        let error =
            inspect_image_with_limits(&store, &image, limits).expect_err("remaining budget");

        assert!(matches!(
            error,
            OciError::LayerLimit {
                limit: 2,
                actual: 3,
                ..
            }
        ));
    }

    #[test]
    fn final_image_rejects_layer_1025_before_publication() {
        let store = MemoryStore::default();
        let parent_layers = vec![(MediaType::ImageLayer, Vec::new()); MAX_IMAGE_LAYERS];
        let parent = publish_image(&store, &parent_layers);
        let added = publish_bytes(&store, MediaType::ImageLayer, b"", "added layer");
        store.clear_publications();

        let error = publish_final_image(&store, &parent, Some((added, digest_for(b""))))
            .expect_err("Layer 1025");

        assert_eq!(error.kind(), OciErrorKind::Image);
        assert!(error.to_string().contains("contains 1025 entries"));
        assert!(!store.published(&MediaType::ImageConfig));
        assert!(!store.published(&MediaType::ImageManifest));
    }

    #[test]
    fn final_layer_uses_parent_remaining_uncompressed_budget() {
        let store = MemoryStore::default();
        let parent = publish_image(&store, &[(MediaType::ImageLayer, b"one".to_vec())]);
        let added = publish_bytes(&store, MediaType::ImageLayer, b"two", "added layer");
        store.clear_publications();
        let limits = ImageLimits {
            uncompressed_layer_bytes: 5,
            ..IMAGE_LIMITS
        };

        let error = publish_final_image_with_limits(
            &store,
            &parent,
            Some((added, digest_for(b"two"))),
            limits,
        )
        .expect_err("remaining final budget");

        assert!(matches!(
            error,
            OciError::LayerLimit {
                limit: 2,
                actual: 3,
                ..
            }
        ));
        assert!(!store.published(&MediaType::ImageConfig));
        assert!(!store.published(&MediaType::ImageManifest));
    }

    #[test]
    fn generated_config_and_manifest_limits_fail_before_config_publication() {
        let store = MemoryStore::default();
        let parent = publish_image(&store, &[]);
        let parent_image = inspect_image(&store, &parent).expect("parent image");
        let added = publish_bytes(&store, MediaType::ImageLayer, b"new", "added layer");
        store.clear_publications();
        let config_limits = ImageLimits {
            config_bytes: u64::try_from(parent_image.config().bytes().len()).expect("config size"),
            ..IMAGE_LIMITS
        };

        let config_error = publish_final_image_with_limits(
            &store,
            &parent,
            Some((added.clone(), digest_for(b"new"))),
            config_limits,
        )
        .expect_err("generated Config limit");

        assert!(matches!(
            config_error,
            OciError::JsonLimit { ref path, .. } if path == "final.config"
        ));
        assert!(!store.published(&MediaType::ImageConfig));
        assert!(!store.published(&MediaType::ImageManifest));

        let manifest_limits = ImageLimits {
            manifest_bytes: u64::try_from(parent_image.manifest().bytes().len())
                .expect("manifest size"),
            ..IMAGE_LIMITS
        };
        let manifest_error = publish_final_image_with_limits(
            &store,
            &parent,
            Some((added, digest_for(b"new"))),
            manifest_limits,
        )
        .expect_err("generated Manifest limit");

        assert!(matches!(
            manifest_error,
            OciError::JsonLimit { ref path, .. } if path == "final.manifest"
        ));
        assert!(!store.published(&MediaType::ImageConfig));
        assert!(!store.published(&MediaType::ImageManifest));
    }

    #[test]
    fn image_rejects_layer_and_diff_id_count_mismatch() {
        let store = MemoryStore::default();
        let layer = publish_bytes(&store, MediaType::ImageLayer, b"layer", "layer");
        let config_bytes = config_bytes(&[]);
        let config = publish_bytes(&store, MediaType::ImageConfig, &config_bytes, "config");
        let manifest_bytes = serde_json::to_vec(&json!({
            "schemaVersion": 2,
            "config": config,
            "layers": [layer]
        }))
        .expect("manifest JSON");
        let manifest = publish_bytes(
            &store,
            MediaType::ImageManifest,
            &manifest_bytes,
            "manifest",
        );
        let image = ImageDescriptor::new(manifest).expect("image descriptor");

        let error = inspect_image(&store, &image).expect_err("count mismatch");

        assert_eq!(error.kind(), OciErrorKind::Image);
        assert!(error.to_string().contains("contains 1 entries"));
        assert!(error.to_string().contains("diff_ids contains 0"));
    }

    #[test]
    fn manifest_is_published_last_and_not_published_when_reference_check_fails() {
        let store = MemoryStore::default();
        let parent = publish_image(&store, &[]);
        let raw = b"new layer";
        let layer = publish_bytes(&store, MediaType::ImageLayer, raw, "new layer");
        store.fail_reads_after_publish(MediaType::ImageConfig);

        let error = publish_final_image(&store, &parent, Some((layer, digest_for(raw))))
            .expect_err("injected config failure");

        assert_eq!(error.kind(), OciErrorKind::Content);
        assert!(store.published(&MediaType::ImageConfig));
        assert!(!store.published(&MediaType::ImageManifest));
    }

    #[test]
    fn unchanged_final_image_revalidates_and_returns_complete_parent_without_publication() {
        let store = MemoryStore::default();
        let parent = publish_image(&store, &[]);
        let bytes = store
            .blobs
            .lock()
            .expect("blobs lock")
            .get(&parent.as_oci().digest().to_string())
            .expect("parent manifest")
            .clone();
        let mut descriptor = descriptor_for_bytes(MediaType::ImageManifest, &bytes);
        descriptor.set_urls(Some(vec!["https://example.test/image".to_owned()]));
        descriptor.set_annotations(Some(HashMap::from([(
            "identity".to_owned(),
            "preserved".to_owned(),
        )])));
        descriptor.set_platform(Some(linux_platform_value("amd64")));
        descriptor.set_data(Some(base64_encode(&bytes)));
        store.insert_unchecked(&descriptor, bytes);
        let parent = ImageDescriptor::new(descriptor.clone()).expect("image descriptor");

        let final_image = publish_final_image(&store, &parent, None).expect("unchanged image");

        assert_eq!(final_image.into_oci(), descriptor);
        assert!(
            store
                .published_media_types
                .lock()
                .expect("publications lock")
                .is_empty()
        );
    }

    #[test]
    fn unchanged_final_image_does_not_bypass_missing_parent_content() {
        let store = MemoryStore::default();
        let descriptor = descriptor_for_bytes(MediaType::ImageManifest, b"absent");
        let parent = ImageDescriptor::new(descriptor).expect("image descriptor");

        let error = publish_final_image(&store, &parent, None).expect_err("missing parent");

        assert_eq!(error.kind(), OciErrorKind::Content);
        assert!(!store.published(&MediaType::ImageManifest));
    }

    #[test]
    fn final_image_is_deterministic_and_readable_with_manifest_last() {
        let store = MemoryStore::default();
        let parent = publish_image(&store, &[]);
        let raw = b"new layer";
        let layer = publish_bytes(&store, MediaType::ImageLayer, raw, "new layer");
        let first = publish_final_image(&store, &parent, Some((layer.clone(), digest_for(raw))))
            .expect("final image");
        let second = publish_final_image(&store, &parent, Some((layer, digest_for(raw))))
            .expect("same final image");

        assert_eq!(first, second);
        let image = inspect_image(&store, &first).expect("readable final image");
        assert_eq!(image.layers().len(), 1);
        assert_eq!(image.diff_ids(), &[digest_for(raw)]);
        assert_eq!(
            store
                .published_media_types
                .lock()
                .expect("publications lock")
                .last(),
            Some(&MediaType::ImageManifest)
        );
    }

    fn publish_image(store: &MemoryStore, layers: &[(MediaType, Vec<u8>)]) -> ImageDescriptor {
        let mut descriptors = Vec::new();
        let mut diff_ids = Vec::new();
        for (index, (media_type, bytes)) in layers.iter().enumerate() {
            let layer = publish_bytes(store, media_type.clone(), bytes, format!("layer[{index}]"));
            descriptors.push(layer);
            diff_ids.push(digest_for(bytes));
        }
        let config_bytes = config_bytes(&diff_ids);
        let config = publish_bytes(store, MediaType::ImageConfig, &config_bytes, "config");
        let manifest_bytes = serde_json::to_vec(&json!({
            "schemaVersion": 2,
            "mediaType": MediaType::ImageManifest,
            "config": config,
            "layers": descriptors,
            "x-extension": {"preserved": true}
        }))
        .expect("manifest JSON");
        let manifest = publish_bytes(store, MediaType::ImageManifest, &manifest_bytes, "manifest");
        // The fixture's publication should not count as the operation under test.
        store
            .published_media_types
            .lock()
            .expect("publications lock")
            .clear();
        ImageDescriptor::new(manifest).expect("image descriptor")
    }

    fn config_bytes(diff_ids: &[Digest]) -> Vec<u8> {
        config_bytes_for_platform("amd64", None, diff_ids)
    }

    fn config_bytes_for_platform(
        architecture: &str,
        variant: Option<&str>,
        diff_ids: &[Digest],
    ) -> Vec<u8> {
        let diff_ids = diff_ids.iter().map(ToString::to_string).collect::<Vec<_>>();
        let mut value = json!({
            "architecture": architecture,
            "os": "linux",
            "rootfs": {"type": "layers", "diff_ids": diff_ids},
            "config": {"Env": ["A=B"]},
            "x-extension": {"preserved": true}
        });
        if let Some(variant) = variant {
            value
                .as_object_mut()
                .expect("config object")
                .insert("variant".to_owned(), Value::String(variant.to_owned()));
        }
        serde_json::to_vec(&value).expect("config JSON")
    }

    fn digest_for(bytes: &[u8]) -> Digest {
        Digest::try_from(format!("sha256:{}", hex_sha256(bytes))).expect("digest")
    }

    fn linux_platform_value(architecture: &str) -> Platform {
        serde_json::from_value(json!({"architecture": architecture, "os": "linux"}))
            .expect("platform")
    }

    fn base64_encode(bytes: &[u8]) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
        for chunk in bytes.chunks(3) {
            let first = chunk[0];
            let second = chunk.get(1).copied().unwrap_or(0);
            let third = chunk.get(2).copied().unwrap_or(0);
            encoded.push(char::from(ALPHABET[usize::from(first >> 2)]));
            encoded.push(char::from(
                ALPHABET[usize::from(((first & 0x03) << 4) | (second >> 4))],
            ));
            if chunk.len() > 1 {
                encoded.push(char::from(
                    ALPHABET[usize::from(((second & 0x0f) << 2) | (third >> 6))],
                ));
            } else {
                encoded.push('=');
            }
            if chunk.len() > 2 {
                encoded.push(char::from(ALPHABET[usize::from(third & 0x3f)]));
            } else {
                encoded.push('=');
            }
        }
        encoded
    }

    #[test]
    fn digest_algorithm_error_is_typed() {
        let store = MemoryStore::default();
        let bytes = b"content";
        let descriptor = Descriptor::new(
            MediaType::ImageConfig,
            u64::try_from(bytes.len()).expect("size"),
            Digest::try_from(format!("sha512:{}", "0".repeat(128))).expect("digest"),
        );
        store.insert_unchecked(&descriptor, bytes.as_slice());
        let error = read_small_verified(
            &store,
            &descriptor,
            &[MediaType::ImageConfig],
            MAX_CONFIG_BYTES,
            "config",
        )
        .expect_err("unsupported algorithm");
        assert!(matches!(error, OciError::DigestAlgorithm { .. }));
        assert_eq!(descriptor.digest().algorithm(), &DigestAlgorithm::Sha512);
    }

    #[test]
    fn decompressed_layer_limit_is_enforced_while_streaming() {
        let error = digest_stream_limited(&mut Cursor::new(b"too large"), "layer", 3)
            .expect_err("stream must stop at the declared limit");

        assert!(matches!(
            error,
            OciError::LayerLimit {
                limit: 3,
                actual: 4,
                ..
            }
        ));
    }

    #[test]
    fn layer_budget_observes_at_most_remaining_plus_one_raw_and_decoded_bytes() {
        const REMAINING: u64 = 3;
        let raw = vec![b'x'; 128 * 1024];
        let mut raw_reader = CountingReader::new(Cursor::new(&raw));

        let raw_error = digest_stream_limited(&mut raw_reader, "raw layer", REMAINING)
            .expect_err("raw Layer exceeds remaining budget");

        assert!(matches!(
            raw_error,
            OciError::LayerLimit {
                limit: REMAINING,
                actual: 4,
                ..
            }
        ));
        assert_eq!(raw_reader.bytes_read, 4);
        assert_eq!(raw_reader.largest_request, 4);

        let mut gzip = GzEncoder::new(Vec::new(), Compression::default());
        gzip.write_all(&raw).expect("gzip write");
        let gzip = gzip.finish().expect("gzip finish");
        let decoder = MultiGzDecoder::new(Cursor::new(gzip));
        let mut decoded_reader = CountingReader::new(decoder);

        let decoded_error = digest_stream_limited(&mut decoded_reader, "gzip layer", REMAINING)
            .expect_err("decoded Layer exceeds remaining budget");

        assert!(matches!(
            decoded_error,
            OciError::LayerLimit {
                limit: REMAINING,
                actual: 4,
                ..
            }
        ));
        assert_eq!(decoded_reader.bytes_read, 4);
        assert_eq!(decoded_reader.largest_request, 4);
    }

    #[test]
    fn generated_json_is_stable_for_unknown_object_fields() {
        let value = Value::Object(Map::from_iter(BTreeMap::from([
            ("z".to_owned(), Value::from(1)),
            ("a".to_owned(), Value::from(2)),
        ])));
        assert_eq!(
            json_bytes(&value, "value").expect("JSON"),
            br#"{"a":2,"z":1}"#
        );
    }

    fn publish_bytes(
        store: &MemoryStore,
        media_type: MediaType,
        bytes: &[u8],
        path: impl Into<String>,
    ) -> Descriptor {
        let mut reader = Cursor::new(bytes);
        publish_content(store, media_type, &mut reader, path).expect("published content")
    }
}
