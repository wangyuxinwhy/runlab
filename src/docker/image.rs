use std::fs::File;
use std::io::{Cursor, Read};
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::Value;
use tar::{Archive, EntryType};

use crate::changeset::{ChangeSet, LayerEncoder};
use crate::core::{
    Digest, ImageView, OCI_IMAGE_CONFIG, OCI_LAYER_GZIP, OCI_LAYER_TAR, OCI_LAYER_ZSTD,
    OciDescriptor, Platform,
};
use crate::docker::DockerBackend;
use crate::filesystem::ContentStore;
use crate::image::{
    CaptureAction, CaptureMetadata, CaptureResult, ImageService, ImportedImage, config_diff_ids,
    config_platform,
};
use crate::integrity::{digest_bytes, digest_reader};

const MAX_ARCHIVE_JSON_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Debug)]
struct DockerArchiveImage {
    config_bytes: Vec<u8>,
    layers: Vec<DockerArchiveLayer>,
    diff_ids: Vec<Digest>,
    platform: Platform,
}

#[derive(Debug)]
struct DockerArchiveLayer {
    path: String,
    descriptor: OciDescriptor,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct DockerManifestEntry {
    config: String,
    layers: Vec<String>,
}

pub(crate) struct DockerImageAdapter<'a> {
    images: &'a ImageService,
    docker: &'a DockerBackend,
}

impl<'a> DockerImageAdapter<'a> {
    #[must_use]
    pub(crate) const fn new(images: &'a ImageService, docker: &'a DockerBackend) -> Self {
        Self { images, docker }
    }

    pub(crate) fn import(&self, image: &str) -> Result<ImageView> {
        let directory = tempfile::Builder::new()
            .prefix("runlab-import-")
            .tempdir()
            .context("failed to create image import directory")?;
        let archive_path = directory.path().join("image.tar");
        self.docker.save_image(image, &archive_path)?;
        let imported = self.import_archive(&archive_path)?;
        self.images.publish_imported(imported, None, None)
    }

    pub(crate) fn materialize(&self, manifest_digest: &Digest) -> Result<String> {
        let image = self.images.inspect(manifest_digest)?;
        let reference = image.manifest.digest.to_string();
        if self.docker.image_exists(&reference)? {
            self.verify_materialized(&reference, &image)?;
            return Ok(reference);
        }
        let directory = tempfile::Builder::new()
            .prefix("runlab-materialize-")
            .tempdir()
            .context("failed to create materialization directory")?;
        let archive = directory.path().join("image.tar");
        let tag = format!("runlab-image:{}", &manifest_digest.hex()[..24]);
        self.images.write_oci_archive(&image, &archive, &tag)?;
        self.docker.load_image(&archive)?;
        if !self.docker.image_exists(&reference)? {
            bail!("Docker did not load OCI Image Manifest: {manifest_digest}");
        }
        self.verify_materialized(&reference, &image)?;
        Ok(reference)
    }

    pub(crate) fn create_checkout(&self, manifest_digest: &Digest) -> Result<(String, Digest)> {
        let image = self.materialize(manifest_digest)?;
        let container = self
            .docker
            .create_checkout(&image, manifest_digest.as_str())?;
        Ok((container, manifest_digest.clone()))
    }

    pub(crate) fn freeze_checkout(&self, container: &str) -> Result<ImageView> {
        let parent = Digest::parse(self.docker.checkout_parent(container)?)?;
        let capture = self.capture_container(container, &parent, CaptureAction::Checkout)?;
        if let Some(error) = capture.cleanup_error {
            bail!(
                "captured Image {} is available, but temporary tag cleanup failed: {error}",
                capture.image.manifest.digest
            );
        }
        Ok(capture.image)
    }

    pub(crate) fn freeze_run(
        &self,
        container: &str,
        parent: &Digest,
        run_id: &str,
    ) -> Result<CaptureResult> {
        self.capture_container(container, parent, CaptureAction::Run(run_id.to_owned()))
    }

    fn capture_container(
        &self,
        container: &str,
        parent_digest: &Digest,
        action: CaptureAction,
    ) -> Result<CaptureResult> {
        let parent = self.images.inspect(parent_digest)?;
        let capture = CaptureMetadata {
            captured_at: chrono::Utc::now(),
            action,
        };
        let safe_id = container
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() {
                    character
                } else {
                    '-'
                }
            })
            .collect::<String>();
        let tag = format!("runlab-freeze:{}", &safe_id[..safe_id.len().min(48)]);
        self.docker.commit(container, &tag)?;
        let directory = tempfile::Builder::new()
            .prefix("runlab-capture-")
            .tempdir()
            .context("failed to create image capture directory")?;
        let archive_path = directory.path().join("image.tar");
        let result = (|| {
            self.docker.save_image(&tag, &archive_path)?;
            let child = Self::inspect_archive(&archive_path)?;
            if child.layers.len() != child.diff_ids.len() {
                bail!("captured OCI Image has inconsistent Layer and DiffID counts");
            }
            if child.platform != parent.platform {
                bail!("committed OCI Image changed its parent platform");
            }
            let child_descriptors = child
                .layers
                .iter()
                .map(|layer| layer.descriptor.clone())
                .collect::<Vec<_>>();
            let structure =
                crate::image::compare_layer_structure(&parent.layers, &child_descriptors);
            if structure.parent_remaining != 0 || structure.common_prefix != parent.layers.len() {
                bail!("committed OCI Image does not extend its parent Layer chain");
            }
            if child.diff_ids[..parent.diff_ids.len()] != parent.diff_ids {
                bail!("committed OCI Image does not extend its parent DiffID chain");
            }
            let added_layers = &child.layers[structure.common_prefix..];
            if added_layers.len() != structure.child_remaining {
                bail!("captured OCI Image Layer comparison is inconsistent");
            }
            let added_diff_ids = &child.diff_ids[parent.diff_ids.len()..];
            let layer = if added_layers.is_empty() && added_diff_ids.is_empty() {
                let staged =
                    LayerEncoder::default().stage(&ChangeSet::default(), &ContentStore::new()?)?;
                self.images.publish_staged_layer(staged)?
            } else if let ([added_layer], [added_diff_id]) = (added_layers, added_diff_ids) {
                let descriptor = self.import_layer_member(&archive_path, &added_layer.path)?;
                if descriptor != added_layer.descriptor {
                    bail!("captured OCI Image Layer changed during archive ingestion");
                }
                self.images
                    .verify_stored_final_layer(descriptor, added_diff_id.clone())?
            } else {
                bail!(
                    "one Run capture must produce exactly one child Layer, received {}",
                    added_layers.len()
                );
            };
            self.images
                .publish_final_image(parent_digest, layer, &capture)
        })();
        let cleanup = self.docker.remove_image_tag(&tag);
        match (result, cleanup) {
            (Ok(capture), Ok(())) => Ok(capture),
            (Err(error), Ok(())) => Err(error),
            (Err(error), Err(cleanup_error)) => Err(error).context(format!(
                "capture failed and temporary tag cleanup also failed: {cleanup_error:#}"
            )),
            (Ok(mut capture), Err(error)) => {
                capture.cleanup_error = Some(format!("{error:#}"));
                Ok(capture)
            }
        }
    }

    fn verify_materialized(&self, docker_image: &str, image: &ImageView) -> Result<()> {
        let actual = self.docker.image_diff_ids(docker_image)?;
        let expected = image
            .diff_ids
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        if actual != expected {
            bail!(
                "Docker materialization changed OCI Image rootfs: {}",
                image.manifest.digest
            );
        }
        Ok(())
    }

    fn import_archive(&self, path: &Path) -> Result<ImportedImage> {
        let archive = Self::inspect_archive(path)?;
        let config = self
            .images
            .layout()
            .put_bytes(&archive.config_bytes, OCI_IMAGE_CONFIG)?;
        let mut layers = Vec::with_capacity(archive.layers.len());
        for layer in archive.layers {
            let descriptor = self.import_layer_member(path, &layer.path)?;
            if descriptor != layer.descriptor {
                bail!(
                    "Docker archive Layer changed during ingestion: {}",
                    layer.path
                );
            }
            layers.push(descriptor);
        }
        Ok(ImportedImage::new(
            config,
            layers,
            archive.diff_ids,
            archive.platform,
        ))
    }

    fn inspect_archive(path: &Path) -> Result<DockerArchiveImage> {
        let manifest_bytes = read_tar_member(path, "manifest.json", MAX_ARCHIVE_JSON_BYTES)?;
        let entries: Vec<DockerManifestEntry> = serde_json::from_slice(&manifest_bytes)
            .context("Docker archive manifest.json is invalid")?;
        let [entry] = entries.as_slice() else {
            bail!("Docker archive must describe exactly one saved image");
        };
        let config_bytes = read_tar_member(path, &entry.config, MAX_ARCHIVE_JSON_BYTES)?;
        let expected_config_digest = digest_from_archive_path(&entry.config)?;
        if digest_bytes(&config_bytes) != expected_config_digest {
            bail!("Docker archive OCI Image config failed descriptor verification");
        }
        let config_value: Value = serde_json::from_slice(&config_bytes)
            .context("Docker archive OCI Image config is invalid JSON")?;
        let diff_ids = config_diff_ids(&config_value)?;
        if entry.layers.len() != diff_ids.len() {
            bail!("Docker archive has inconsistent Layer and DiffID counts");
        }
        let layers = entry
            .layers
            .iter()
            .map(|layer_path| {
                Ok(DockerArchiveLayer {
                    path: layer_path.clone(),
                    descriptor: inspect_layer_member(path, layer_path)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(DockerArchiveImage {
            config_bytes,
            layers,
            diff_ids,
            platform: config_platform(&config_value)?,
        })
    }

    fn import_layer_member(&self, archive_path: &Path, member_name: &str) -> Result<OciDescriptor> {
        let expected_digest = digest_from_archive_path(member_name)?;
        let file = File::open(archive_path)
            .with_context(|| format!("failed to open Docker archive {}", archive_path.display()))?;
        let mut archive = Archive::new(file);
        for entry in archive.entries().context("failed to read Docker archive")? {
            let mut entry = entry.context("failed to read Docker archive entry")?;
            if entry.path_bytes().as_ref() != member_name.as_bytes() {
                continue;
            }
            if entry.header().entry_type() != EntryType::Regular {
                bail!("Docker archive Layer is not a regular file: {member_name}");
            }
            let size = entry
                .header()
                .size()
                .context("Docker Layer size is invalid")?;
            let mut prefix = [0_u8; 4];
            let read = entry
                .read(&mut prefix)
                .context("failed to read Docker Layer")?;
            let media_type = layer_media_type(&prefix[..read]);
            let expected = OciDescriptor {
                digest: expected_digest,
                size,
                media_type: media_type.to_owned(),
            };
            return self.images.layout().put_reader(
                Cursor::new(prefix[..read].to_vec()).chain(entry),
                media_type,
                Some(&expected),
            );
        }
        bail!("Docker archive is missing {member_name}")
    }
}

fn layer_media_type(prefix: &[u8]) -> &'static str {
    if prefix.starts_with(&[0x1f, 0x8b]) {
        OCI_LAYER_GZIP
    } else if prefix.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        OCI_LAYER_ZSTD
    } else {
        OCI_LAYER_TAR
    }
}

fn digest_from_archive_path(path: &str) -> Result<Digest> {
    let name = Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .with_context(|| format!("Docker archive path is invalid: {path}"))?;
    Digest::parse(format!("sha256:{name}"))
}

fn read_tar_member(path: &Path, name: &str, max_bytes: u64) -> Result<Vec<u8>> {
    let file = File::open(path)
        .with_context(|| format!("failed to open Docker archive {}", path.display()))?;
    let mut archive = Archive::new(file);
    for entry in archive.entries().context("failed to read Docker archive")? {
        let entry = entry.context("failed to read Docker archive entry")?;
        if entry.path_bytes().as_ref() != name.as_bytes() {
            continue;
        }
        let size = entry
            .header()
            .size()
            .context("Docker archive member size is invalid")?;
        if size > max_bytes {
            bail!("Docker archive member {name} exceeds the {max_bytes}-byte limit");
        }
        let mut bytes = Vec::with_capacity(usize::try_from(size).context("member is too large")?);
        entry
            .take(max_bytes + 1)
            .read_to_end(&mut bytes)
            .with_context(|| format!("failed to read Docker archive member {name}"))?;
        return Ok(bytes);
    }
    bail!("Docker archive is missing {name}")
}

fn inspect_layer_member(archive_path: &Path, member_name: &str) -> Result<OciDescriptor> {
    let expected_digest = digest_from_archive_path(member_name)?;
    let file = File::open(archive_path)
        .with_context(|| format!("failed to open Docker archive {}", archive_path.display()))?;
    let mut archive = Archive::new(file);
    for entry in archive.entries().context("failed to read Docker archive")? {
        let mut entry = entry.context("failed to read Docker archive entry")?;
        if entry.path_bytes().as_ref() != member_name.as_bytes() {
            continue;
        }
        if entry.header().entry_type() != EntryType::Regular {
            bail!("Docker archive Layer is not a regular file: {member_name}");
        }
        let expected_size = entry
            .header()
            .size()
            .context("Docker Layer size is invalid")?;
        let mut prefix = [0_u8; 4];
        let read = entry
            .read(&mut prefix)
            .context("failed to read Docker Layer")?;
        let media_type = layer_media_type(&prefix[..read]);
        let (actual_digest, actual_size) =
            digest_reader(Cursor::new(prefix[..read].to_vec()).chain(entry))?;
        if actual_digest != expected_digest || actual_size != expected_size {
            bail!("Docker archive Layer failed descriptor verification: {member_name}");
        }
        return Ok(OciDescriptor {
            digest: actual_digest,
            size: actual_size,
            media_type: media_type.to_owned(),
        });
    }
    bail!("Docker archive is missing {member_name}")
}
