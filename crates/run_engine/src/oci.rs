use std::io::{self, Cursor};

use oci_spec::image::{
    Descriptor, Digest, ImageConfiguration, ImageManifest, MediaType, Os, Platform,
};
use run_protocol::{ImageDescriptor, ImageDescriptorError};
use serde_json::{Map, Value, json};
use thiserror::Error;

use crate::{ContentError, ContentErrorKind, OciContentStore};

use self::content::{descriptor_for_bytes, enforce_generated_json_limit};
#[cfg(test)]
use self::content::{hex_sha256, publish_content};
pub(crate) use self::content::{publish_expected, read_small_verified, verify_content};
use self::json::{json_bytes, parse_unique_json};
#[cfg(test)]
use self::layer::digest_stream_limited;
use self::layer::{preflight_layers, supported_layer_media_types, verify_layer};

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
#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OciErrorKind {
    Content,
    Descriptor,
    Json,
    Image,
    Layer,
}

/// Protocol-facing category of the source that made OCI processing fail.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OciSourceCategory {
    InvalidInput,
    InputUnavailable,
    Unsupported,
    Internal,
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
    #[error("unsupported OCI Image field {path}: {reason}")]
    Unsupported { path: String, reason: String },
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
    #[cfg(test)]
    pub(crate) const fn kind(&self) -> OciErrorKind {
        match self {
            Self::Content { .. } | Self::Io { .. } => OciErrorKind::Content,
            Self::MediaType { .. }
            | Self::DigestAlgorithm { .. }
            | Self::Size { .. }
            | Self::Digest { .. }
            | Self::ImageDescriptor { .. } => OciErrorKind::Descriptor,
            Self::Json { .. } | Self::JsonLimit { .. } => OciErrorKind::Json,
            Self::Image { .. } | Self::Unsupported { .. } => OciErrorKind::Image,
            Self::LayerDecode { .. }
            | Self::LayerLimit { .. }
            | Self::CompressedLayerLimit { .. }
            | Self::DiffId { .. } => OciErrorKind::Layer,
        }
    }

    pub(crate) fn source_category(&self) -> OciSourceCategory {
        match self {
            Self::Content { source, .. } => match source.kind() {
                // A policy-refused read still means the caller's required bytes
                // cannot be obtained; it does not make those bytes invalid.
                ContentErrorKind::Unavailable | ContentErrorKind::Rejected => {
                    OciSourceCategory::InputUnavailable
                }
                ContentErrorKind::Internal => OciSourceCategory::Internal,
            },
            Self::Io { .. } | Self::ImageDescriptor { .. } => OciSourceCategory::Internal,
            Self::MediaType { .. }
            | Self::DigestAlgorithm { .. }
            | Self::JsonLimit { .. }
            | Self::Unsupported { .. }
            | Self::LayerLimit { .. }
            | Self::CompressedLayerLimit { .. } => OciSourceCategory::Unsupported,
            Self::Size { .. }
            | Self::Digest { .. }
            | Self::Json { .. }
            | Self::Image { .. }
            | Self::LayerDecode { .. }
            | Self::DiffId { .. } => OciSourceCategory::InvalidInput,
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
    uncompressed_size: u64,
}

impl VerifiedLayer {
    pub(crate) fn descriptor(&self) -> &Descriptor {
        &self.descriptor
    }

    pub(crate) fn diff_id(&self) -> &Digest {
        &self.diff_id
    }

    pub(crate) const fn uncompressed_size(&self) -> u64 {
        self.uncompressed_size
    }
}

/// Manifest and Config facts whose Layer bytes have not yet been revalidated.
#[derive(Clone, Debug)]
pub(crate) struct ImagePlan {
    manifest: VerifiedContent,
    manifest_value: Value,
    config: VerifiedContent,
    config_value: Value,
    platform: Platform,
    layers: Vec<(Descriptor, Digest)>,
    total_compressed_layer_bytes: u64,
}

impl ImagePlan {
    pub(crate) fn layers(&self) -> impl ExactSizeIterator<Item = (&Descriptor, &Digest)> {
        self.layers
            .iter()
            .map(|(descriptor, diff_id)| (descriptor, diff_id))
    }

    pub(crate) fn verified_from_snapshot(
        self,
        uncompressed_sizes: Vec<u64>,
    ) -> Result<VerifiedImage, OciError> {
        if uncompressed_sizes.len() != self.layers.len() {
            return Err(image_error(
                "snapshot.layers",
                "cached validation does not match the Image Layer count",
            ));
        }
        let total_uncompressed_layer_bytes = uncompressed_sizes
            .iter()
            .try_fold(0_u64, |total, size| total.checked_add(*size))
            .ok_or_else(|| image_error("snapshot.layers", "uncompressed size overflow"))?;
        if total_uncompressed_layer_bytes > IMAGE_LIMITS.uncompressed_layer_bytes {
            return Err(OciError::LayerLimit {
                path: "snapshot.layers".to_owned(),
                limit: IMAGE_LIMITS.uncompressed_layer_bytes,
                actual: total_uncompressed_layer_bytes,
            });
        }
        Ok(self.finish(uncompressed_sizes, total_uncompressed_layer_bytes))
    }

    fn verify(
        self,
        store: &dyn OciContentStore,
        limits: ImageLimits,
    ) -> Result<VerifiedImage, OciError> {
        let mut sizes = Vec::with_capacity(self.layers.len());
        let mut total_uncompressed = 0_u64;
        for (index, (descriptor, expected_diff_id)) in self.layers.iter().enumerate() {
            let path = format!("manifest.layers[{index}]");
            let remaining = limits
                .uncompressed_layer_bytes
                .checked_sub(total_uncompressed)
                .expect("accounted Layer bytes never exceed the limit");
            let uncompressed = verify_layer(store, descriptor, expected_diff_id, &path, remaining)?;
            total_uncompressed = total_uncompressed
                .checked_add(uncompressed)
                .ok_or_else(|| image_error("manifest.layers", "uncompressed size overflow"))?;
            sizes.push(uncompressed);
        }
        Ok(self.finish(sizes, total_uncompressed))
    }

    fn finish(self, sizes: Vec<u64>, total_uncompressed_layer_bytes: u64) -> VerifiedImage {
        #[cfg(test)]
        let diff_ids = self
            .layers
            .iter()
            .map(|(_, diff_id)| diff_id.clone())
            .collect();
        let layers = self
            .layers
            .into_iter()
            .zip(sizes)
            .map(|((descriptor, diff_id), uncompressed_size)| VerifiedLayer {
                descriptor,
                diff_id,
                uncompressed_size,
            })
            .collect();
        VerifiedImage {
            manifest: self.manifest,
            manifest_value: self.manifest_value,
            config: self.config,
            config_value: self.config_value,
            platform: self.platform,
            layers,
            #[cfg(test)]
            diff_ids,
            total_compressed_layer_bytes: self.total_compressed_layer_bytes,
            total_uncompressed_layer_bytes,
        }
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
    inspect_image_plan_with_limits(store, image, limits)?.verify(store, limits)
}

pub(crate) fn inspect_image_plan(
    store: &dyn OciContentStore,
    image: &ImageDescriptor,
) -> Result<ImagePlan, OciError> {
    inspect_image_plan_with_limits(store, image, IMAGE_LIMITS)
}

fn inspect_image_plan_with_limits(
    store: &dyn OciContentStore,
    image: &ImageDescriptor,
    limits: ImageLimits,
) -> Result<ImagePlan, OciError> {
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

    let layers = manifest_view
        .layers()
        .iter()
        .cloned()
        .zip(diff_ids.iter().cloned())
        .collect();
    Ok(ImagePlan {
        manifest,
        manifest_value,
        config,
        config_value,
        platform,
        layers,
        total_compressed_layer_bytes: total_compressed,
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
    let parent = inspect_image_with_limits(store, parent, IMAGE_LIMITS)?;
    publish_final_image_from_verified(store, &parent, added_layer)
}

pub(crate) fn publish_final_image_from_verified(
    store: &dyn OciContentStore,
    parent: &VerifiedImage,
    added_layer: Option<(Descriptor, Digest)>,
) -> Result<ImageDescriptor, OciError> {
    publish_final_image_from_verified_with_limits(store, parent, added_layer, IMAGE_LIMITS)
}

#[cfg(test)]
fn publish_final_image_with_limits(
    store: &dyn OciContentStore,
    parent: &ImageDescriptor,
    added_layer: Option<(Descriptor, Digest)>,
    limits: ImageLimits,
) -> Result<ImageDescriptor, OciError> {
    let parent = inspect_image_with_limits(store, parent, limits)?;
    publish_final_image_from_verified_with_limits(store, &parent, added_layer, limits)
}

fn publish_final_image_from_verified_with_limits(
    store: &dyn OciContentStore,
    parent: &VerifiedImage,
    added_layer: Option<(Descriptor, Digest)>,
    limits: ImageLimits,
) -> Result<ImageDescriptor, OciError> {
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

    if !store.published_content_is_immutable() {
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
        return Err(unsupported_error(
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
                return Err(unsupported_error(
                    &path,
                    "only sha256 DiffIDs are supported",
                ));
            }
            Ok(digest)
        })
        .collect()
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

fn image_error(path: impl Into<String>, reason: impl Into<String>) -> OciError {
    OciError::Image {
        path: path.into(),
        reason: reason.into(),
    }
}

fn unsupported_error(path: impl Into<String>, reason: impl Into<String>) -> OciError {
    OciError::Unsupported {
        path: path.into(),
        reason: reason.into(),
    }
}

mod content;
mod json;
mod layer;

#[cfg(test)]
#[path = "oci/tests.rs"]
mod tests;
