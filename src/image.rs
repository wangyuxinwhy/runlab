//! OCI Images: inspect, diff, export, and publish.
//!
//! `ImageService` is the one way to ask an Image a question or add one to the
//! local Layout. It composes `render` for reading Layers, `changeset` for
//! turning a before/after pair into a new Layer, and `oci` for exact-byte
//! storage.
//!
//! Questions here are about Images. Whether a particular backend can realize an
//! Image is that backend's question, asked with the primitives this module
//! exposes.

use std::fs::{self, File};
use std::io::Seek as _;
#[cfg(test)]
use std::io::{Cursor, Read};
use std::path::Path;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, SecondsFormat, Utc};
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::{Value, json};
#[cfg(test)]
use tar::Archive;
use tar::{Builder, EntryType, Header};

#[cfg(test)]
use crate::changeset::ChangeSet;
#[cfg(any(test, target_os = "linux"))]
use crate::changeset::LayerEncoder;
use crate::changeset::StagedLayer;
#[cfg(target_os = "linux")]
use crate::core::RunId;
use crate::core::{
    Architecture, Digest, ImageView, OCI_IMAGE_CONFIG, OCI_IMAGE_INDEX, OCI_IMAGE_MANIFEST,
    OCI_LAYER_GZIP, OCI_LAYER_TAR, OCI_LAYER_ZSTD, OciDescriptor, Platform,
};
#[cfg(test)]
use crate::filesystem::ContentStore;
use crate::integrity::{canonical_json, digest_reader, sync_directory};
use crate::oci::{MAX_IMAGE_LAYERS, OciLayout};
use crate::render::{FilesystemDiff, ImageRenderer, layer_diff_id};
#[cfg(target_os = "linux")]
use crate::{
    filesystem::{CapturedTree, FilesystemOwnership, Inventory},
    materialize::MaterializedRootfs,
};

#[derive(Debug, Clone)]
pub struct ImageService {
    layout: OciLayout,
}

#[derive(Debug)]
pub struct CaptureResult {
    pub image: ImageView,
    pub cleanup_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub(crate) struct ImageDiff {
    pub(crate) schema_version: u32,
    pub(crate) from: OciDescriptor,
    pub(crate) to: OciDescriptor,
    pub(crate) structure: ImageStructureDiff,
    pub(crate) filesystem: FilesystemDiff,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub(crate) struct ImageStructureDiff {
    pub(crate) platform_changed: bool,
    pub(crate) config_changed: bool,
    pub(crate) common_layer_prefix: u64,
    pub(crate) removed_layers: Vec<OciDescriptor>,
    pub(crate) added_layers: Vec<OciDescriptor>,
}

#[derive(Debug)]
pub(crate) struct FinalLayer {
    pub(crate) descriptor: OciDescriptor,
    pub(crate) diff_id: Digest,
}

#[derive(Debug)]
pub(crate) struct CaptureMetadata {
    pub(crate) captured_at: DateTime<Utc>,
    pub(crate) action: CaptureAction,
}

#[derive(Debug)]
pub(crate) enum CaptureAction {
    Run(String),
    Checkout,
}

impl CaptureAction {
    fn history_value(&self) -> String {
        match self {
            Self::Run(run_id) => format!("runlab capture run:{run_id}"),
            Self::Checkout => "runlab capture checkout".to_owned(),
        }
    }
}

impl ImageService {
    #[must_use]
    pub const fn new(layout: OciLayout) -> Self {
        Self { layout }
    }

    #[must_use]
    pub(crate) const fn layout(&self) -> &OciLayout {
        &self.layout
    }

    pub fn inspect(&self, manifest_digest: &Digest) -> Result<ImageView> {
        let manifest_bytes = self.layout.get_bytes(manifest_digest)?;
        let manifest_value: Value = serde_json::from_slice(&manifest_bytes)
            .with_context(|| format!("OCI Image Manifest is invalid JSON: {manifest_digest}"))?;
        let manifest_object = manifest_value
            .as_object()
            .context("OCI Image Manifest must be an object")?;
        if manifest_object.get("schemaVersion").and_then(Value::as_u64) != Some(2) {
            bail!("OCI Image Manifest schemaVersion must be 2");
        }
        if let Some(media_type) = manifest_object.get("mediaType")
            && media_type.as_str() != Some(OCI_IMAGE_MANIFEST)
        {
            bail!("OCI Image Manifest mediaType is invalid");
        }
        let manifest = OciDescriptor {
            digest: manifest_digest.clone(),
            size: u64::try_from(manifest_bytes.len()).context("Manifest size overflow")?,
            media_type: OCI_IMAGE_MANIFEST.to_owned(),
        };
        let config = descriptor_from_value(
            manifest_object
                .get("config")
                .context("OCI Image Manifest lacks config")?,
            "config",
        )?;
        if config.media_type != OCI_IMAGE_CONFIG {
            bail!("OCI Image Manifest config has an unsupported mediaType");
        }
        let layers = manifest_object
            .get("layers")
            .and_then(Value::as_array)
            .context("OCI Image Manifest layers must be an array")?
            .iter()
            .enumerate()
            .map(|(index, value)| descriptor_from_value(value, &format!("layers[{index}]")))
            .collect::<Result<Vec<_>>>()?;
        if layers.len() > MAX_IMAGE_LAYERS {
            bail!("OCI Image exceeds the {MAX_IMAGE_LAYERS}-Layer limit");
        }
        for layer in &layers {
            if !matches!(
                layer.media_type.as_str(),
                OCI_LAYER_TAR | OCI_LAYER_GZIP | OCI_LAYER_ZSTD
            ) {
                bail!(
                    "OCI Image Layer has an unsupported mediaType: {}",
                    layer.media_type
                );
            }
        }
        let config_value = self.layout.get_json(&config)?;
        let diff_ids = config_diff_ids(&config_value)?;
        if layers.len() != diff_ids.len() {
            bail!("OCI Image has inconsistent Layer and DiffID counts");
        }
        self.verify_diff_ids(&layers, &diff_ids)?;
        let view = ImageView {
            manifest,
            config,
            platform: config_platform(&config_value)?,
            layers,
            diff_ids,
            parent_manifest: None,
            added_layers: Vec::new(),
        };
        view.validate()?;
        ImageRenderer::new(self.layout.clone()).verify(&view)?;
        Ok(view)
    }

    /// Whether `path` resolves to a regular file in this Image without
    /// traversing a symbolic link. Callers that need to bind-mount over a path
    /// ask this; why they need it is their concern.
    #[cfg(target_os = "linux")]
    pub(crate) fn verify_regular_path_without_symlinks(
        &self,
        image: &ImageView,
        path: &[u8],
    ) -> Result<()> {
        ImageRenderer::new(self.layout.clone()).verify_regular_path_without_symlinks(image, path)
    }

    pub fn image_config(&self, manifest_digest: &Digest) -> Result<Value> {
        let image = self.inspect(manifest_digest)?;
        self.layout.get_json(&image.config)
    }

    pub(crate) fn diff(&self, from: &Digest, to: &Digest) -> Result<ImageDiff> {
        let from = self.inspect(from)?;
        let to = self.inspect(to)?;
        let common_layer_prefix = from
            .layers
            .iter()
            .zip(&to.layers)
            .take_while(|(left, right)| left == right)
            .count();
        let common_layer_prefix =
            u64::try_from(common_layer_prefix).context("common Layer prefix overflow")?;
        let offset = usize::try_from(common_layer_prefix)?;
        let filesystem = ImageRenderer::new(self.layout.clone()).diff(&from, &to)?;
        Ok(ImageDiff {
            schema_version: 1,
            structure: ImageStructureDiff {
                platform_changed: from.platform != to.platform,
                config_changed: from.config != to.config,
                common_layer_prefix,
                removed_layers: from.layers[offset..].to_vec(),
                added_layers: to.layers[offset..].to_vec(),
            },
            from: from.manifest,
            to: to.manifest,
            filesystem,
        })
    }

    /// Write this Image's filesystem into `workspace` under `ownership`.
    #[cfg(target_os = "linux")]
    pub(crate) fn materialize_rootfs_at(
        &self,
        manifest_digest: &Digest,
        workspace: &Path,
        ownership: FilesystemOwnership,
    ) -> Result<MaterializedRootfs> {
        let image = self.inspect(manifest_digest)?;
        crate::materialize::materialize_at_with_ownership(
            &self.layout,
            &image,
            crate::render::RenderLimits::default(),
            workspace,
            ownership,
        )
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn capture_filesystem(
        &self,
        parent_manifest: &Digest,
        before: &Inventory,
        after: &CapturedTree,
        run_id: &RunId,
        captured_at: DateTime<Utc>,
        staging_parent: &Path,
    ) -> Result<CaptureResult> {
        let changes = crate::changeset::compare(before, &after.inventory)?;
        let staged = LayerEncoder::default().stage_in(&changes, &after.contents, staging_parent)?;
        let layer = self.publish_staged_layer(staged)?;
        self.publish_final_image(
            parent_manifest,
            layer,
            &CaptureMetadata {
                captured_at,
                action: CaptureAction::Run(run_id.to_string()),
            },
        )
    }

    pub fn get_file(
        &self,
        manifest_digest: &Digest,
        source: &str,
        destination: &Path,
    ) -> Result<(Digest, u64)> {
        if !source.starts_with('/') || source.contains('\0') {
            bail!("image source path must be an absolute Linux path");
        }
        match fs::symlink_metadata(destination) {
            Ok(_) => bail!("output path already exists: {}", destination.display()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to inspect output path {}", destination.display())
                });
            }
        }
        let parent = destination.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create output directory {}", parent.display()))?;
        let image = self.inspect(manifest_digest)?;
        let mut staged = tempfile::NamedTempFile::new_in(parent)
            .with_context(|| format!("failed to create output staging in {}", parent.display()))?;
        let renderer = ImageRenderer::new(self.layout.clone());
        let (digest, size) = renderer.copy_file(&image, source.as_bytes(), staged.as_file_mut())?;
        staged
            .as_file_mut()
            .sync_all()
            .context("failed to fsync extracted image file")?;
        staged.persist_noclobber(destination).map_err(|error| {
            anyhow::Error::new(error.error).context(format!(
                "failed to publish output {}",
                destination.display()
            ))
        })?;
        sync_directory(parent)?;
        Ok((digest, size))
    }

    pub(crate) fn export_tar(
        &self,
        manifest_digest: &Digest,
        destination: &Path,
    ) -> Result<(Digest, u64)> {
        match fs::symlink_metadata(destination) {
            Ok(_) => bail!("output path already exists: {}", destination.display()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to inspect output path {}", destination.display())
                });
            }
        }
        let parent = destination.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create output directory {}", parent.display()))?;
        let image = self.inspect(manifest_digest)?;
        let mut staged = tempfile::NamedTempFile::new_in(parent)
            .with_context(|| format!("failed to create output staging in {}", parent.display()))?;
        ImageRenderer::new(self.layout.clone()).export_tar(&image, staged.as_file_mut())?;
        staged
            .as_file_mut()
            .sync_all()
            .context("failed to fsync exported Image tar")?;
        staged.as_file_mut().rewind()?;
        let (digest, size) = digest_reader(staged.as_file_mut())?;
        staged.persist_noclobber(destination).map_err(|error| {
            anyhow::Error::new(error.error).context(format!(
                "failed to publish output {}",
                destination.display()
            ))
        })?;
        sync_directory(parent)?;
        Ok((digest, size))
    }

    pub(crate) fn publish_final_image(
        &self,
        parent_manifest: &Digest,
        layer: FinalLayer,
        capture: &CaptureMetadata,
    ) -> Result<CaptureResult> {
        let image = self.assemble_final_image(parent_manifest, layer, capture)?;
        Ok(CaptureResult {
            image,
            cleanup_error: None,
        })
    }

    pub(crate) fn publish_staged_layer(&self, mut staged: StagedLayer) -> Result<FinalLayer> {
        let expected = staged.descriptor.clone();
        let descriptor =
            self.layout
                .put_reader(staged.reader(), OCI_LAYER_GZIP, Some(&expected))?;
        Ok(FinalLayer {
            descriptor,
            diff_id: staged.diff_id,
        })
    }

    pub(crate) fn verify_stored_final_layer(
        &self,
        descriptor: OciDescriptor,
        expected_diff_id: Digest,
    ) -> Result<FinalLayer> {
        if !matches!(
            descriptor.media_type.as_str(),
            OCI_LAYER_TAR | OCI_LAYER_GZIP | OCI_LAYER_ZSTD
        ) {
            bail!(
                "Final OCI Image Layer has an unsupported mediaType: {}",
                descriptor.media_type
            );
        }
        self.layout.open_descriptor(&descriptor)?;
        let actual_diff_id = layer_diff_id(&self.layout, &descriptor)?;
        if actual_diff_id != expected_diff_id {
            bail!(
                "Final OCI Image Layer DiffID mismatch for {}: expected {}, received {}",
                descriptor.digest,
                expected_diff_id,
                actual_diff_id
            );
        }
        Ok(FinalLayer {
            descriptor,
            diff_id: expected_diff_id,
        })
    }

    fn assemble_final_image(
        &self,
        parent_manifest: &Digest,
        layer: FinalLayer,
        capture: &CaptureMetadata,
    ) -> Result<ImageView> {
        let parent = self.inspect(parent_manifest)?;
        let layer = self.verify_stored_final_layer(layer.descriptor, layer.diff_id)?;
        let parent_config = self.layout.get_json(&parent.config)?;
        let config_value = final_config_value(&parent_config, &layer.diff_id, capture)?;
        let config = self
            .layout
            .put_bytes(&canonical_json(&config_value)?, OCI_IMAGE_CONFIG)?;
        let parent_manifest_value: Value =
            serde_json::from_slice(&self.layout.get_descriptor_bytes(&parent.manifest)?)
                .context("Initial OCI Image Manifest is invalid JSON")?;
        let manifest_value =
            final_manifest_value(&parent_manifest_value, &config, &layer.descriptor)?;
        let manifest = self
            .layout
            .put_bytes(&canonical_json(&manifest_value)?, OCI_IMAGE_MANIFEST)?;
        let inspected = self.inspect(&manifest.digest)?;
        let expected_layers = parent
            .layers
            .iter()
            .cloned()
            .chain(std::iter::once(layer.descriptor.clone()))
            .collect::<Vec<_>>();
        let expected_diff_ids = parent
            .diff_ids
            .iter()
            .cloned()
            .chain(std::iter::once(layer.diff_id))
            .collect::<Vec<_>>();
        if inspected.layers != expected_layers || inspected.diff_ids != expected_diff_ids {
            bail!("Final OCI Image does not extend its Initial Image by exactly one Layer");
        }
        if inspected.platform != parent.platform {
            bail!("Final OCI Image changed the Initial Image platform");
        }
        Ok(ImageView {
            manifest: inspected.manifest,
            config: inspected.config,
            platform: inspected.platform,
            layers: inspected.layers,
            diff_ids: inspected.diff_ids,
            parent_manifest: Some(parent.manifest.digest),
            added_layers: vec![layer.descriptor.digest],
        })
    }

    pub(crate) fn publish_imported(
        &self,
        imported: ImportedImage,
        parent_manifest: Option<Digest>,
        added_layers: Option<Vec<Digest>>,
    ) -> Result<ImageView> {
        if imported.layers.len() != imported.diff_ids.len() {
            bail!("OCI Image has inconsistent Layer and DiffID counts");
        }
        self.verify_diff_ids(&imported.layers, &imported.diff_ids)?;
        let manifest_value = image_manifest_value(&imported.config, &imported.layers);
        let manifest = self
            .layout
            .put_bytes(&canonical_json(&manifest_value)?, OCI_IMAGE_MANIFEST)?;
        let view = ImageView {
            manifest,
            config: imported.config,
            platform: imported.platform,
            layers: imported.layers.clone(),
            diff_ids: imported.diff_ids,
            parent_manifest,
            added_layers: added_layers.unwrap_or_else(|| {
                imported
                    .layers
                    .iter()
                    .map(|layer| layer.digest.clone())
                    .collect()
            }),
        };
        view.validate()?;
        Ok(view)
    }

    fn verify_diff_ids(&self, layers: &[OciDescriptor], diff_ids: &[Digest]) -> Result<()> {
        for (layer, expected_diff_id) in layers.iter().zip(diff_ids) {
            let actual_diff_id = layer_diff_id(&self.layout, layer)?;
            if &actual_diff_id != expected_diff_id {
                bail!(
                    "OCI Image Layer DiffID mismatch for {}: expected {}, received {}",
                    layer.digest,
                    expected_diff_id,
                    actual_diff_id
                );
            }
        }
        Ok(())
    }

    pub(crate) fn write_oci_archive(
        &self,
        image: &ImageView,
        destination: &Path,
        tag: &str,
    ) -> Result<()> {
        let file = File::create(destination)
            .with_context(|| format!("failed to create OCI archive {}", destination.display()))?;
        let mut builder = Builder::new(file);
        append_bytes(
            &mut builder,
            "oci-layout",
            &canonical_json(&json!({"imageLayoutVersion": "1.0.0"}))?,
        )?;
        append_bytes(
            &mut builder,
            "index.json",
            &canonical_json(&json!({
                "schemaVersion": 2,
                "mediaType": OCI_IMAGE_INDEX,
                "manifests": [{
                    "mediaType": image.manifest.media_type,
                    "digest": image.manifest.digest,
                    "size": image.manifest.size,
                    "platform": image.platform,
                    "annotations": {"org.opencontainers.image.ref.name": tag}
                }]
            }))?,
        )?;
        for descriptor in std::iter::once(&image.manifest)
            .chain(std::iter::once(&image.config))
            .chain(&image.layers)
        {
            let file = self.layout.open_descriptor(descriptor)?;
            append_file(
                &mut builder,
                &format!("blobs/sha256/{}", descriptor.digest.hex()),
                file,
                descriptor.size,
            )?;
        }
        builder.finish().context("failed to finish OCI archive")?;
        builder
            .into_inner()
            .context("failed to close OCI archive")?
            .sync_all()
            .context("failed to fsync OCI archive")
    }
}

#[derive(Debug)]
pub(crate) struct ImportedImage {
    config: OciDescriptor,
    layers: Vec<OciDescriptor>,
    diff_ids: Vec<Digest>,
    platform: Platform,
}

impl ImportedImage {
    pub(crate) fn new(
        config: OciDescriptor,
        layers: Vec<OciDescriptor>,
        diff_ids: Vec<Digest>,
        platform: Platform,
    ) -> Self {
        Self {
            config,
            layers,
            diff_ids,
            platform,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LayerStructureDiff {
    pub(crate) common_prefix: usize,
    pub(crate) parent_remaining: usize,
    pub(crate) child_remaining: usize,
}

pub(crate) fn compare_layer_structure(
    parent: &[OciDescriptor],
    child: &[OciDescriptor],
) -> LayerStructureDiff {
    let common_prefix = parent
        .iter()
        .zip(child)
        .take_while(|(left, right)| left == right)
        .count();
    LayerStructureDiff {
        common_prefix,
        parent_remaining: parent.len() - common_prefix,
        child_remaining: child.len() - common_prefix,
    }
}

fn descriptor_from_value(value: &Value, field: &str) -> Result<OciDescriptor> {
    let object = value
        .as_object()
        .with_context(|| format!("OCI Image Manifest {field} must be a descriptor"))?;
    Ok(OciDescriptor {
        digest: Digest::parse(
            object
                .get("digest")
                .and_then(Value::as_str)
                .with_context(|| format!("OCI Image Manifest {field} digest is invalid"))?,
        )?,
        size: object
            .get("size")
            .and_then(Value::as_u64)
            .with_context(|| format!("OCI Image Manifest {field} size is invalid"))?,
        media_type: object
            .get("mediaType")
            .and_then(Value::as_str)
            .with_context(|| format!("OCI Image Manifest {field} mediaType is invalid"))?
            .to_owned(),
    })
}

fn image_manifest_value(config: &OciDescriptor, layers: &[OciDescriptor]) -> Value {
    json!({
        "schemaVersion": 2,
        "mediaType": OCI_IMAGE_MANIFEST,
        "config": descriptor_value(config),
        "layers": layers.iter().map(descriptor_value).collect::<Vec<_>>()
    })
}

fn final_manifest_value(
    parent: &Value,
    config: &OciDescriptor,
    layer: &OciDescriptor,
) -> Result<Value> {
    let mut final_manifest = parent.clone();
    let object = final_manifest
        .as_object_mut()
        .context("Initial OCI Image Manifest must be an object")?;
    if object.get("schemaVersion").and_then(Value::as_u64) != Some(2) {
        bail!("Initial OCI Image Manifest schemaVersion must be 2");
    }
    if let Some(media_type) = object.get("mediaType")
        && media_type.as_str() != Some(OCI_IMAGE_MANIFEST)
    {
        bail!("Initial OCI Image Manifest mediaType is invalid");
    }
    object.insert("config".to_owned(), descriptor_value(config));
    object
        .get_mut("layers")
        .and_then(Value::as_array_mut)
        .context("Initial OCI Image Manifest layers must be an array")?
        .push(descriptor_value(layer));
    Ok(final_manifest)
}

fn descriptor_value(descriptor: &OciDescriptor) -> Value {
    json!({
        "mediaType": descriptor.media_type,
        "digest": descriptor.digest,
        "size": descriptor.size
    })
}

pub(crate) fn config_diff_ids(config: &Value) -> Result<Vec<Digest>> {
    let rootfs = config
        .get("rootfs")
        .and_then(Value::as_object)
        .context("OCI Image config does not contain rootfs")?;
    if rootfs.get("type").and_then(Value::as_str) != Some("layers") {
        bail!("OCI Image config rootfs.type must be layers");
    }
    rootfs
        .get("diff_ids")
        .and_then(Value::as_array)
        .context("OCI Image config does not contain rootfs.diff_ids")?
        .iter()
        .map(|value| {
            Digest::parse(
                value
                    .as_str()
                    .context("OCI Image rootfs.diff_ids must contain digests")?,
            )
        })
        .collect()
}

pub(crate) fn config_platform(config: &Value) -> Result<Platform> {
    let object = config
        .as_object()
        .context("OCI Image config must be an object")?;
    if object.get("os").and_then(Value::as_str) != Some("linux") {
        bail!("OCI Image has an unsupported operating system");
    }
    let architecture = object
        .get("architecture")
        .and_then(Value::as_str)
        .context("OCI Image config lacks architecture")?
        .parse::<Architecture>()?;
    Ok(Platform::linux(architecture))
}

fn final_config_value(
    parent: &Value,
    added_diff_id: &Digest,
    capture: &CaptureMetadata,
) -> Result<Value> {
    let mut final_config = parent.clone();
    let object = final_config
        .as_object_mut()
        .context("Initial OCI Image Config must be an object")?;
    let rootfs = object
        .get_mut("rootfs")
        .and_then(Value::as_object_mut)
        .context("Initial OCI Image Config lacks rootfs")?;
    if rootfs.get("type").and_then(Value::as_str) != Some("layers") {
        bail!("Initial OCI Image Config rootfs.type must be layers");
    }
    let diff_ids = rootfs
        .get_mut("diff_ids")
        .and_then(Value::as_array_mut)
        .context("Initial OCI Image Config lacks rootfs.diff_ids")?;
    diff_ids.push(Value::String(added_diff_id.to_string()));
    let history = object
        .entry("history")
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .context("Initial OCI Image Config history must be an array")?;
    history.push(json!({
        "created": capture.captured_at.to_rfc3339_opts(SecondsFormat::Nanos, true),
        "created_by": capture.action.history_value(),
        "empty_layer": false
    }));
    Ok(final_config)
}

fn append_bytes(builder: &mut Builder<File>, name: &str, bytes: &[u8]) -> Result<()> {
    let size = u64::try_from(bytes.len()).context("archive member is too large")?;
    let mut header = deterministic_header(size);
    builder
        .append_data(&mut header, name, bytes)
        .with_context(|| format!("failed to add {name} to OCI archive"))
}

fn append_file(builder: &mut Builder<File>, name: &str, file: File, size: u64) -> Result<()> {
    let mut header = deterministic_header(size);
    builder
        .append_data(&mut header, name, file)
        .with_context(|| format!("failed to add {name} to OCI archive"))
}

fn deterministic_header(size: u64) -> Header {
    let mut header = Header::new_gnu();
    header.set_size(size);
    header.set_mode(0o644);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_entry_type(EntryType::Regular);
    header.set_cksum();
    header
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    #[cfg(target_os = "linux")]
    use std::ffi::OsString;
    #[cfg(target_os = "linux")]
    use std::io::Write as _;
    #[cfg(target_os = "linux")]
    use std::process::Command;

    use super::*;
    #[cfg(target_os = "linux")]
    use crate::catalog::{CatalogMetadata, LocalImageCatalog};
    use crate::changeset::compare;
    use crate::filesystem::{EntryKind, FsEntry, FsPath, Inventory, Metadata, Timestamp};
    use chrono::TimeZone;

    #[test]
    fn final_config_preserves_unknown_fields_and_only_patches_rootfs_history() {
        let value = json!({
            "created": "original",
            "architecture": "arm64",
            "os": "linux",
            "rootfs": {"type": "layers", "diff_ids": [format!("sha256:{}", "1".repeat(64))]},
            "config": {"Env": ["A=B"], "Labels": {"custom": "kept"}},
            "x-extension": {"value": 3}
        });
        let added = Digest::parse(format!("sha256:{}", "2".repeat(64))).expect("digest");
        let capture = CaptureMetadata {
            captured_at: Utc.with_ymd_and_hms(2026, 8, 21, 4, 5, 6).unwrap(),
            action: CaptureAction::Run("test".to_owned()),
        };
        let first = final_config_value(&value, &added, &capture).expect("final config");
        let second = final_config_value(&value, &added, &capture).expect("final config");
        assert_eq!(first, second);
        assert_eq!(first["created"], "original");
        assert_eq!(first["config"]["Labels"]["custom"], "kept");
        assert_eq!(first["x-extension"]["value"], 3);
        assert_eq!(
            first["rootfs"]["diff_ids"]
                .as_array()
                .expect("diff ids")
                .len(),
            2
        );
        assert_eq!(first["history"].as_array().expect("history").len(), 1);
        assert_eq!(
            first["history"][0]["created"],
            "2026-08-21T04:05:06.000000000Z"
        );
        assert_eq!(first["history"][0]["created_by"], "runlab capture run:test");
    }

    #[test]
    fn final_manifest_preserves_parent_fields_and_descriptor_objects() {
        let parent = json!({
            "schemaVersion": 2,
            "mediaType": OCI_IMAGE_MANIFEST,
            "artifactType": "application/vnd.example.executable",
            "subject": {
                "mediaType": "application/vnd.example.subject",
                "digest": format!("sha256:{}", "3".repeat(64)),
                "size": 7,
                "annotations": {"subject-key": "subject-value"}
            },
            "annotations": {"custom": "kept"},
            "x-extension": {"value": 4},
            "config": {
                "mediaType": OCI_IMAGE_CONFIG,
                "digest": format!("sha256:{}", "4".repeat(64)),
                "size": 8,
                "annotations": {"old-config": "kept-only-on-old-object"}
            },
            "layers": [{
                "mediaType": OCI_LAYER_TAR,
                "digest": format!("sha256:{}", "5".repeat(64)),
                "size": 9,
                "annotations": {"layer-key": "layer-value"},
                "x-descriptor-extension": true
            }]
        });
        let config = descriptor('6', OCI_IMAGE_CONFIG);
        let layer = descriptor('7', OCI_LAYER_TAR);
        let final_manifest =
            final_manifest_value(&parent, &config, &layer).expect("final manifest");
        assert_eq!(final_manifest["artifactType"], parent["artifactType"]);
        assert_eq!(final_manifest["subject"], parent["subject"]);
        assert_eq!(final_manifest["annotations"], parent["annotations"]);
        assert_eq!(final_manifest["x-extension"], parent["x-extension"]);
        assert_eq!(final_manifest["layers"][0], parent["layers"][0]);
        assert_eq!(final_manifest["layers"][1], descriptor_value(&layer));
        assert_eq!(final_manifest["config"], descriptor_value(&config));
    }

    #[test]
    fn encoded_changeset_uses_the_common_deterministic_assembler() {
        let state = tempfile::tempdir().expect("state");
        let layout = OciLayout::open(state.path()).expect("layout");
        let service = ImageService::new(layout.clone());
        let parent_config = layout
            .put_bytes(
                &canonical_json(&json!({
                    "architecture": "arm64",
                    "os": "linux",
                    "rootfs": {"type": "layers", "diff_ids": []},
                    "config": {}
                }))
                .expect("config bytes"),
                OCI_IMAGE_CONFIG,
            )
            .expect("config");
        let parent_manifest = layout
            .put_bytes(
                &canonical_json(&image_manifest_value(&parent_config, &[]))
                    .expect("manifest bytes"),
                OCI_IMAGE_MANIFEST,
            )
            .expect("manifest");
        let mut contents = ContentStore::new().expect("content store");
        let digest = contents.put_bytes(b"hello changeset\n").expect("content");
        let mut after = Inventory::default();
        after
            .insert(
                FsPath::from_relative(b"hello", 1024).expect("path"),
                FsEntry {
                    metadata: Metadata {
                        mode: 0o644,
                        uid: 0,
                        gid: 0,
                        mtime: Timestamp {
                            seconds: 0,
                            nanos: 0,
                        },
                        xattrs: BTreeMap::new(),
                    },
                    kind: EntryKind::Regular {
                        digest,
                        size: 16,
                        hardlink: None,
                    },
                },
            )
            .expect("inventory");
        let changes = compare(&Inventory::default(), &after).expect("diff");
        let encoded = LayerEncoder::default()
            .encode(&layout, &changes, &contents)
            .expect("encoded Layer");
        let capture = CaptureMetadata {
            captured_at: Utc.with_ymd_and_hms(2026, 8, 21, 4, 5, 6).unwrap(),
            action: CaptureAction::Run("test".to_owned()),
        };
        let assemble = || {
            let layer = service
                .verify_stored_final_layer(encoded.descriptor.clone(), encoded.diff_id.clone())
                .expect("verified Layer");
            service
                .assemble_final_image(&parent_manifest.digest, layer, &capture)
                .expect("Final Image")
        };
        let first = assemble();
        let second = assemble();
        assert_eq!(first.manifest, second.manifest);
        let output_directory = tempfile::tempdir().expect("output directory");
        let output = output_directory.path().join("hello");
        let (file_digest, size) = service
            .get_file(&first.manifest.digest, "/hello", &output)
            .expect("file get");
        assert_eq!(
            file_digest,
            crate::integrity::digest_bytes(b"hello changeset\n")
        );
        assert_eq!(size, 16);
        assert_eq!(
            fs::read(output).expect("output bytes"),
            b"hello changeset\n"
        );
        assert_hello_diff(&service, &parent_manifest.digest, &first.manifest.digest);
        assert_hello_export(&service, &first.manifest.digest);
    }

    fn assert_hello_diff(service: &ImageService, parent: &Digest, child: &Digest) {
        let diff = service.diff(parent, child).expect("Image diff");
        assert_eq!(diff.structure.common_layer_prefix, 0);
        assert_eq!(diff.structure.added_layers.len(), 1);
        assert!(diff.structure.removed_layers.is_empty());
        assert_eq!(diff.filesystem.changes.len(), 1);
        let change = &diff.filesystem.changes[0];
        assert_eq!(change.change, crate::render::FilesystemChangeKind::Added);
        assert_eq!(change.path, "/hello");
        assert_eq!(change.path_hex, "2f68656c6c6f");
        let Some(crate::render::FilesystemNode {
            details: crate::render::FilesystemNodeDetails::Regular { digest, size },
            ..
        }) = &change.after
        else {
            panic!("added path must be a regular file")
        };
        assert_eq!(
            *digest,
            crate::integrity::digest_bytes(b"hello changeset\n")
        );
        assert_eq!(*size, 16);
    }

    fn assert_hello_export(service: &ImageService, image: &Digest) {
        let directory = tempfile::tempdir().expect("export directory");
        let output = directory.path().join("rootfs.tar");
        let (digest, size) = service.export_tar(image, &output).expect("Image export");
        let bytes = fs::read(&output).expect("exported tar");
        assert_eq!(digest, crate::integrity::digest_bytes(&bytes));
        assert_eq!(size, u64::try_from(bytes.len()).expect("tar size"));
        let mut archive = Archive::new(Cursor::new(bytes));
        let mut entries = archive.entries().expect("tar entries");
        let mut entry = entries.next().expect("hello entry").expect("hello");
        assert_eq!(entry.path_bytes().as_ref(), b"hello");
        let mut content = Vec::new();
        entry.read_to_end(&mut content).expect("hello content");
        assert_eq!(content, b"hello changeset\n");
        assert!(entries.next().is_none());
    }

    #[test]
    fn final_asset_publication_does_not_depend_on_catalog_index() {
        let state = tempfile::tempdir().expect("state");
        let layout = OciLayout::open(state.path()).expect("layout");
        let service = ImageService::new(layout.clone());
        let parent_config = layout
            .put_bytes(
                &canonical_json(&json!({
                    "architecture": "arm64",
                    "os": "linux",
                    "rootfs": {"type": "layers", "diff_ids": []},
                    "config": {}
                }))
                .expect("config bytes"),
                OCI_IMAGE_CONFIG,
            )
            .expect("config");
        let parent_manifest = layout
            .put_bytes(
                &canonical_json(&image_manifest_value(&parent_config, &[]))
                    .expect("manifest bytes"),
                OCI_IMAGE_MANIFEST,
            )
            .expect("manifest");
        let encoded = LayerEncoder::default()
            .encode(
                &layout,
                &ChangeSet::default(),
                &ContentStore::new().expect("content store"),
            )
            .expect("encoded Layer");
        let layer = service
            .verify_stored_final_layer(encoded.descriptor, encoded.diff_id)
            .expect("verified Layer");
        fs::write(state.path().join("index.json"), b"{").expect("corrupt index");
        let capture = service
            .publish_final_image(
                &parent_manifest.digest,
                layer,
                &CaptureMetadata {
                    captured_at: Utc.with_ymd_and_hms(2026, 8, 21, 4, 5, 6).unwrap(),
                    action: CaptureAction::Run("catalog-independent".to_owned()),
                },
            )
            .expect("content-addressed Final Image");
        assert!(capture.cleanup_error.is_none());
        let inspected = service
            .inspect(&capture.image.manifest.digest)
            .expect("published Final Manifest");
        assert_eq!(inspected.manifest, capture.image.manifest);
        assert_eq!(inspected.layers.len(), 1);
    }

    #[test]
    fn empty_capture_layer_is_deterministic_and_contains_no_entries() {
        let state = tempfile::tempdir().expect("state");
        let layout = OciLayout::open(state.path()).expect("layout");
        let contents = ContentStore::new().expect("content store");
        let changes = ChangeSet::default();
        let first = LayerEncoder::default()
            .encode(&layout, &changes, &contents)
            .expect("first");
        let second = LayerEncoder::default()
            .encode(&layout, &changes, &contents)
            .expect("second");
        assert_eq!(first.descriptor, second.descriptor);
        assert_eq!(first.diff_id, second.diff_id);
    }

    #[test]
    fn layer_structure_diff_reports_common_prefix_and_both_sides() {
        let parent = vec![
            descriptor('1', OCI_LAYER_TAR),
            descriptor('2', OCI_LAYER_TAR),
            descriptor('3', OCI_LAYER_TAR),
        ];
        let child = vec![
            descriptor('1', OCI_LAYER_TAR),
            descriptor('2', OCI_LAYER_TAR),
            descriptor('4', OCI_LAYER_TAR),
            descriptor('5', OCI_LAYER_TAR),
        ];
        assert_eq!(
            compare_layer_structure(&parent, &child),
            LayerStructureDiff {
                common_prefix: 2,
                parent_remaining: 1,
                child_remaining: 2
            }
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires rootful Linux and RUNLAB_TEST_UMOCI pointing to a real umoci executable"]
    fn umoci_applies_final_image_to_the_intended_semantic_inventory() {
        let executable = std::env::var_os("RUNLAB_TEST_UMOCI").expect("RUNLAB_TEST_UMOCI");
        let state = tempfile::tempdir().expect("state");
        let layout = OciLayout::open(state.path()).expect("layout");
        let service = ImageService::new(layout.clone());

        let mut base_contents = ContentStore::new().expect("base content store");
        let base_inventory = oracle_base_inventory(&mut base_contents);
        let parent = oracle_parent_inventory(&base_inventory);
        let base = compare(&Inventory::default(), &base_inventory).expect("base changeset");
        let base_layer = LayerEncoder::default()
            .encode(&layout, &base, &base_contents)
            .expect("base Layer");
        let opaque_layer = oracle_opaque_layer(&layout, parent.root().expect("parent root"));
        let architecture = match std::env::consts::ARCH {
            "aarch64" => "arm64",
            "x86_64" => "amd64",
            other => panic!("unsupported oracle architecture: {other}"),
        };
        let parent_config = layout
            .put_bytes(
                &canonical_json(&json!({
                    "architecture": architecture,
                    "os": "linux",
                    "rootfs": {
                        "type": "layers",
                        "diff_ids": [base_layer.diff_id, opaque_layer.diff_id]
                    },
                    "config": {}
                }))
                .expect("parent config bytes"),
                OCI_IMAGE_CONFIG,
            )
            .expect("parent config");
        let parent_layers = vec![base_layer.descriptor, opaque_layer.descriptor];
        let parent_manifest = layout
            .put_bytes(
                &canonical_json(&image_manifest_value(&parent_config, &parent_layers))
                    .expect("parent manifest bytes"),
                OCI_IMAGE_MANIFEST,
            )
            .expect("parent manifest");
        service
            .inspect(&parent_manifest.digest)
            .expect("verified parent Image");

        let mut final_contents = ContentStore::new().expect("Final content store");
        let intended = oracle_final_inventory(&mut final_contents, oracle_can_create_device());
        let changes = compare(&parent, &intended).expect("Final changeset");
        let encoded = LayerEncoder::default()
            .encode(&layout, &changes, &final_contents)
            .expect("Final Layer");
        let final_layer = service
            .verify_stored_final_layer(encoded.descriptor, encoded.diff_id)
            .expect("verified Final Layer");
        let final_image = service
            .assemble_final_image(
                &parent_manifest.digest,
                final_layer,
                &CaptureMetadata {
                    captured_at: Utc.with_ymd_and_hms(2026, 8, 21, 4, 5, 6).unwrap(),
                    action: CaptureAction::Run("umoci-apply-oracle".to_owned()),
                },
            )
            .expect("Final Image");
        assert_eq!(final_image.layers[..2], parent_layers);
        assert_eq!(final_image.layers.len(), 3);
        assert_eq!(final_image.diff_ids.len(), 3);
        assert_eq!(final_image.added_layers.len(), 1);
        LocalImageCatalog::new(&layout)
            .upsert(
                "apply-oracle",
                &final_image.manifest,
                final_image.platform,
                &CatalogMetadata::default(),
            )
            .expect("Final Image reference");

        let extraction = tempfile::tempdir().expect("extraction");
        let bundle = extraction.path().join("bundle");
        let mut image = OsString::from(state.path().as_os_str());
        image.push(":apply-oracle");
        let output = Command::new(executable)
            .arg("unpack")
            .arg("--image")
            .arg(image)
            .arg(&bundle)
            .output()
            .expect("umoci unpack");
        assert!(
            output.status.success(),
            "umoci unpack failed with {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        let captured = crate::filesystem::TreeCapture::default()
            .capture(&bundle.join("rootfs"))
            .expect("capture unpacked Final Image");
        intended.validate().expect("intended Final inventory");
        assert_eq!(captured.inventory, intended);
    }

    #[cfg(target_os = "linux")]
    fn oracle_base_inventory(contents: &mut ContentStore) -> Inventory {
        let mut inventory = Inventory::default();
        inventory
            .insert(oracle_path(b"opaque"), oracle_directory(0o755, 1))
            .expect("opaque directory");
        oracle_insert_regular(
            &mut inventory,
            contents,
            b"opaque/hidden-by-opq",
            b"hidden\n",
            oracle_metadata(0o640, 0, 0, 1, 0),
            None,
        );
        oracle_insert_regular(
            &mut inventory,
            contents,
            b"modified",
            b"before\n",
            oracle_metadata(0o600, 0, 0, 2, 0),
            None,
        );
        oracle_insert_regular(
            &mut inventory,
            contents,
            b"gone",
            b"gone\n",
            oracle_metadata(0o644, 0, 0, 3, 0),
            None,
        );
        inventory
            .insert(oracle_path(b"removed-tree"), oracle_directory(0o755, 4))
            .expect("removed directory");
        oracle_insert_regular(
            &mut inventory,
            contents,
            b"removed-tree/child",
            b"removed\n",
            oracle_metadata(0o644, 0, 0, 4, 0),
            None,
        );
        inventory
            .insert(oracle_path(b"metadata-dir"), oracle_directory(0o755, 5))
            .expect("metadata directory");
        inventory
    }

    #[cfg(target_os = "linux")]
    fn oracle_parent_inventory(base: &Inventory) -> Inventory {
        let mut parent = Inventory::default();
        parent
            .set_root(oracle_metadata(0o755, 0, 0, 6, 0))
            .expect("parent root");
        for (path, entry) in base.iter() {
            if path.as_bytes() != b"opaque/hidden-by-opq" {
                parent
                    .insert(path.clone(), entry.clone())
                    .expect("parent entry");
            }
        }
        parent
            .insert(
                oracle_path(b"opaque/visible"),
                oracle_regular(
                    crate::integrity::digest_bytes(b"visible\n"),
                    8,
                    oracle_metadata(0o644, 0, 0, 6, 0),
                    None,
                ),
            )
            .expect("opaque visible entry");
        parent
    }

    #[cfg(target_os = "linux")]
    fn oracle_final_inventory(contents: &mut ContentStore, include_device: bool) -> Inventory {
        let mut root = oracle_metadata(0o751, 0, 0, 10, 250_000_000);
        root.xattrs.insert(
            b"user.runlab.root".to_vec().into_boxed_slice(),
            b"root\nvalue\0".to_vec().into_boxed_slice(),
        );
        let mut inventory = Inventory::default();
        inventory.set_root(root).expect("Final root");
        inventory
            .insert(oracle_path(b"opaque"), oracle_directory(0o755, 1))
            .expect("opaque directory");
        inventory
            .insert(
                oracle_path(b"opaque/visible"),
                oracle_regular(
                    crate::integrity::digest_bytes(b"visible\n"),
                    8,
                    oracle_metadata(0o644, 0, 0, 6, 0),
                    None,
                ),
            )
            .expect("opaque visible entry");

        let mut modified = oracle_metadata(0o640, 123, 456, -1, 500_000_000);
        modified.xattrs.insert(
            b"user.runlab".to_vec().into_boxed_slice(),
            b"line\nzero\0tail\xff".to_vec().into_boxed_slice(),
        );
        oracle_insert_regular(
            &mut inventory,
            contents,
            b"modified",
            b"after\n",
            modified,
            None,
        );
        oracle_insert_regular(
            &mut inventory,
            contents,
            b"added-\xff",
            b"raw path\n",
            oracle_metadata(0o604, 42, 43, 11, 125_000_000),
            None,
        );
        let shared = contents.put_bytes(b"hardlinked\n").expect("hardlink bytes");
        let hardlink_metadata = oracle_metadata(0o644, 44, 45, 12, 375_000_000);
        inventory
            .insert(
                oracle_path(b"hard-anchor"),
                oracle_regular(shared.clone(), 11, hardlink_metadata.clone(), None),
            )
            .expect("hardlink anchor");
        inventory
            .insert(
                oracle_path(b"hard-link"),
                oracle_regular(
                    shared,
                    11,
                    hardlink_metadata,
                    Some(oracle_path(b"hard-anchor")),
                ),
            )
            .expect("hardlink follower");

        let mut directory = oracle_metadata(0o710, 46, 47, 13, 625_000_000);
        directory.xattrs.insert(
            b"user.runlab.dir".to_vec().into_boxed_slice(),
            b"directory\0metadata".to_vec().into_boxed_slice(),
        );
        inventory
            .insert(
                oracle_path(b"metadata-dir"),
                FsEntry {
                    metadata: directory,
                    kind: EntryKind::Directory,
                },
            )
            .expect("metadata directory");
        inventory
            .insert(
                oracle_path(b"event-fifo"),
                FsEntry {
                    metadata: oracle_metadata(0o620, 48, 49, 14, 875_000_000),
                    kind: EntryKind::Fifo,
                },
            )
            .expect("FIFO");
        if include_device {
            inventory
                .insert(
                    oracle_path(b"null-device"),
                    FsEntry {
                        metadata: oracle_metadata(0o600, 0, 0, 15, 0),
                        kind: EntryKind::Character { major: 1, minor: 3 },
                    },
                )
                .expect("character device");
        }
        inventory
    }

    #[cfg(target_os = "linux")]
    fn oracle_insert_regular(
        inventory: &mut Inventory,
        contents: &mut ContentStore,
        path: &[u8],
        bytes: &[u8],
        metadata: Metadata,
        hardlink: Option<FsPath>,
    ) {
        let digest = contents.put_bytes(bytes).expect("regular content");
        inventory
            .insert(
                oracle_path(path),
                oracle_regular(
                    digest,
                    u64::try_from(bytes.len()).expect("content size"),
                    metadata,
                    hardlink,
                ),
            )
            .expect("regular entry");
    }

    #[cfg(target_os = "linux")]
    fn oracle_regular(
        digest: Digest,
        size: u64,
        metadata: Metadata,
        hardlink: Option<FsPath>,
    ) -> FsEntry {
        FsEntry {
            metadata,
            kind: EntryKind::Regular {
                digest,
                size,
                hardlink,
            },
        }
    }

    #[cfg(target_os = "linux")]
    fn oracle_directory(mode: u32, mtime: i64) -> FsEntry {
        FsEntry {
            metadata: oracle_metadata(mode, 0, 0, mtime, 0),
            kind: EntryKind::Directory,
        }
    }

    #[cfg(target_os = "linux")]
    fn oracle_metadata(mode: u32, uid: u32, gid: u32, seconds: i64, nanos: u32) -> Metadata {
        Metadata {
            mode,
            uid,
            gid,
            mtime: Timestamp { seconds, nanos },
            xattrs: BTreeMap::new(),
        }
    }

    #[cfg(target_os = "linux")]
    fn oracle_path(bytes: &[u8]) -> FsPath {
        FsPath::from_relative(bytes, 16 * 1024).expect("oracle path")
    }

    #[cfg(target_os = "linux")]
    fn oracle_opaque_layer(layout: &OciLayout, root: &Metadata) -> FinalLayer {
        let mut uncompressed = Vec::new();
        {
            let mut builder = Builder::new(&mut uncompressed);
            let mut root_header = oracle_tar_header(0, root, EntryType::Directory);
            builder
                .append_data(&mut root_header, ".", std::io::empty())
                .expect("root entry");
            let mut whiteout =
                oracle_tar_header(0, &oracle_metadata(0, 0, 0, 0, 0), EntryType::Regular);
            builder
                .append_data(&mut whiteout, "opaque/.wh..wh..opq", std::io::empty())
                .expect("opaque whiteout");
            let bytes = b"visible\n";
            let mut visible = oracle_tar_header(
                u64::try_from(bytes.len()).expect("visible size"),
                &oracle_metadata(0o644, 0, 0, 6, 0),
                EntryType::Regular,
            );
            builder
                .append_data(&mut visible, "opaque/visible", bytes.as_slice())
                .expect("visible entry");
            builder.finish().expect("opaque Layer tar");
        }
        let diff_id = crate::integrity::digest_bytes(&uncompressed);
        let mut encoder = flate2::GzBuilder::new()
            .mtime(0)
            .operating_system(255)
            .write(Vec::new(), flate2::Compression::new(6));
        encoder
            .write_all(&uncompressed)
            .expect("opaque Layer compression");
        let compressed = encoder.finish().expect("opaque Layer gzip");
        let descriptor = layout
            .put_bytes(&compressed, OCI_LAYER_GZIP)
            .expect("opaque Layer blob");
        FinalLayer {
            descriptor,
            diff_id,
        }
    }

    #[cfg(target_os = "linux")]
    fn oracle_tar_header(size: u64, metadata: &Metadata, entry_type: EntryType) -> Header {
        let mut header = Header::new_gnu();
        header.set_size(size);
        header.set_mode(metadata.mode);
        header.set_uid(u64::from(metadata.uid));
        header.set_gid(u64::from(metadata.gid));
        header.set_mtime(u64::try_from(metadata.mtime.seconds).expect("nonnegative tar mtime"));
        header.set_entry_type(entry_type);
        header.set_username("").expect("tar username");
        header.set_groupname("").expect("tar group name");
        header.set_cksum();
        header
    }

    #[cfg(target_os = "linux")]
    fn oracle_can_create_device() -> bool {
        use rustix::fs::{CWD, FileType, Mode, makedev, mknodat};

        let directory = tempfile::tempdir().expect("device capability probe");
        mknodat(
            CWD,
            directory.path().join("device"),
            FileType::CharacterDevice,
            Mode::RUSR | Mode::WUSR,
            makedev(1, 3),
        )
        .is_ok()
    }

    fn descriptor(digit: char, media_type: &str) -> OciDescriptor {
        OciDescriptor {
            digest: Digest::parse(format!("sha256:{}", digit.to_string().repeat(64)))
                .expect("digest"),
            size: 1,
            media_type: media_type.to_owned(),
        }
    }
}
