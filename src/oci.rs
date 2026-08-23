use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Read, Seek, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rustix::fs::{Mode, OFlags, open};
use serde_json::{Map, Value, json};
use sha2::{Digest as _, Sha256};
use tempfile::NamedTempFile;

use crate::core::{Digest, OCI_IMAGE_INDEX, OCI_IMAGE_MANIFEST, OciDescriptor, Platform};
use crate::integrity::{
    canonical_json, digest_reader, ensure_private_directory, finish_sha256, open_regular_lock,
};

const COPY_BUFFER_BYTES: usize = 1024 * 1024;
const MAX_JSON_BYTES: u64 = 16 * 1024 * 1024;
pub(crate) const MAX_IMAGE_LAYERS: usize = 1024;
const MAX_STORE_ENTRIES: usize = 1_000_000;
const REFERENCE_ANNOTATION: &str = "org.opencontainers.image.ref.name";

#[derive(Debug, Clone)]
pub(crate) struct ManifestReference {
    pub(crate) reference: String,
    pub(crate) descriptor: OciDescriptor,
    pub(crate) platform: Option<Platform>,
    pub(crate) annotations: BTreeMap<String, String>,
}

#[derive(Debug)]
pub(crate) struct ManifestReferenceUpdate {
    pub(crate) previous: Option<ManifestReference>,
    pub(crate) current: ManifestReference,
    pub(crate) changed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StoredBlob {
    pub(crate) digest: Digest,
    pub(crate) size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ManifestRoot {
    pub(crate) descriptor: OciDescriptor,
    pub(crate) reference: Option<String>,
}

struct IndexEntry {
    descriptor: OciDescriptor,
    platform: Option<Platform>,
    annotations: BTreeMap<String, String>,
    reference: Option<String>,
}

#[derive(Debug, Clone)]
pub struct OciLayout {
    root: PathBuf,
}

impl OciLayout {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        ensure_private_directory(root)?;
        ensure_private_directory(&root.join("blobs"))?;
        ensure_private_directory(&root.join("blobs/sha256"))?;
        ensure_private_directory(&root.join(".staging"))?;
        let root = root
            .canonicalize()
            .with_context(|| format!("failed to canonicalize OCI Layout {}", root.display()))?;
        let layout = Self { root };
        layout.initialize()?;
        layout.validate_layout_files()?;
        Ok(layout)
    }

    pub(crate) fn open_existing(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        validate_directory(root, "OCI Layout")?;
        validate_directory(&root.join("blobs"), "OCI blob directory")?;
        validate_directory(&root.join("blobs/sha256"), "OCI SHA-256 blob directory")?;
        validate_directory(&root.join(".staging"), "OCI staging directory")?;
        let root = root
            .canonicalize()
            .with_context(|| format!("failed to canonicalize OCI Layout {}", root.display()))?;
        let layout = Self { root };
        layout.validate_layout_files()?;
        Ok(layout)
    }

    fn initialize(&self) -> Result<()> {
        let lock_path = self.root.join(".mutation.lock");
        let lock = open_regular_lock(&lock_path, true, "OCI Layout mutation lock")?;
        lock.lock()
            .context("failed to lock OCI Layout initialization")?;
        let layout = self.root.join("oci-layout");
        if !layout.exists() {
            write_new_private(
                &layout,
                &canonical_json(&json!({"imageLayoutVersion": "1.0.0"}))?,
            )?;
        }
        let index = self.root.join("index.json");
        if !index.exists() {
            write_new_private(
                &index,
                &canonical_json(&json!({
                    "schemaVersion": 2,
                    "mediaType": OCI_IMAGE_INDEX,
                    "manifests": []
                }))?,
            )?;
        }
        Ok(())
    }

    pub fn put_bytes(&self, bytes: &[u8], media_type: &str) -> Result<OciDescriptor> {
        self.put_reader(bytes, media_type, None)
    }

    pub(crate) fn root_path(&self) -> &Path {
        &self.root
    }

    pub fn put_reader(
        &self,
        reader: impl Read,
        media_type: &str,
        expected: Option<&OciDescriptor>,
    ) -> Result<OciDescriptor> {
        let staging = self.root.join(".staging");
        let mut temporary = NamedTempFile::new_in(&staging)
            .with_context(|| format!("failed to stage OCI blob in {}", staging.display()))?;
        set_private_file(temporary.as_file())?;
        let (digest, size) = copy_and_digest(reader, temporary.as_file_mut())?;
        temporary
            .as_file_mut()
            .sync_all()
            .context("failed to fsync temporary OCI blob")?;

        if let Some(expected) = expected {
            if media_type != expected.media_type {
                bail!(
                    "OCI blob mediaType mismatch: expected {}, received {media_type}",
                    expected.media_type
                );
            }
            if digest != expected.digest {
                bail!(
                    "OCI blob digest mismatch: expected {}, received {}",
                    expected.digest,
                    digest
                );
            }
            if size != expected.size {
                bail!(
                    "OCI blob size mismatch for {}: expected {}, received {}",
                    digest,
                    expected.size,
                    size
                );
            }
        }

        let directory = self.root.join("blobs/sha256");
        let target = self.blob_path(&digest);
        if target.exists() {
            Self::verify_path(&target, &digest, Some(size))?;
        } else {
            match temporary.persist_noclobber(&target) {
                Ok(_) => sync_directory(&directory)?,
                Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                    Self::verify_path(&target, &digest, Some(size))?;
                }
                Err(error) => {
                    return Err(error.error).with_context(|| {
                        format!("failed to publish OCI blob {}", target.display())
                    });
                }
            }
        }
        Ok(OciDescriptor {
            digest,
            size,
            media_type: media_type.to_owned(),
        })
    }

    #[cfg(test)]
    pub fn get_descriptor_path(&self, descriptor: &OciDescriptor) -> Result<PathBuf> {
        let path = self.blob_path(&descriptor.digest);
        if !path.is_file() {
            bail!("OCI blob is unavailable: {}", descriptor.digest);
        }
        Self::verify_path(&path, &descriptor.digest, Some(descriptor.size))?;
        Ok(path)
    }

    pub fn open_descriptor(&self, descriptor: &OciDescriptor) -> Result<File> {
        self.open_verified(&descriptor.digest, Some(descriptor.size))
    }

    pub fn get_bytes(&self, digest: &Digest) -> Result<Vec<u8>> {
        let file = self.open_verified(digest, None)?;
        read_bounded_reader(file, MAX_JSON_BYTES, &digest.to_string())
    }

    pub fn get_descriptor_bytes(&self, descriptor: &OciDescriptor) -> Result<Vec<u8>> {
        let file = self.open_descriptor(descriptor)?;
        read_bounded_reader(file, MAX_JSON_BYTES, &descriptor.digest.to_string())
    }

    pub fn get_json(&self, descriptor: &OciDescriptor) -> Result<Value> {
        let bytes = self.get_descriptor_bytes(descriptor)?;
        serde_json::from_slice(&bytes)
            .with_context(|| format!("OCI blob is not valid JSON: {}", descriptor.digest))
    }

    pub(crate) fn manifest_references(&self) -> Result<Vec<ManifestReference>> {
        self.index_entries()?
            .into_iter()
            .filter_map(|entry| {
                entry.reference.map(|reference| {
                    Ok(ManifestReference {
                        reference,
                        descriptor: entry.descriptor,
                        platform: entry.platform,
                        annotations: entry.annotations,
                    })
                })
            })
            .collect()
    }

    pub(crate) fn manifest_root_entries(&self) -> Result<Vec<ManifestRoot>> {
        Ok(self
            .index_entries()?
            .into_iter()
            .map(|entry| ManifestRoot {
                descriptor: entry.descriptor,
                reference: entry.reference,
            })
            .collect())
    }

    pub(crate) fn stored_blobs(&self) -> Result<Vec<StoredBlob>> {
        let directory = self.root.join("blobs/sha256");
        let mut blobs = Vec::new();
        for entry in fs::read_dir(&directory)
            .with_context(|| format!("failed to list OCI blobs in {}", directory.display()))?
        {
            if blobs.len() >= MAX_STORE_ENTRIES {
                bail!("OCI blob directory exceeds {MAX_STORE_ENTRIES} entries");
            }
            let entry = entry.context("failed to read OCI blob directory entry")?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| anyhow::anyhow!("OCI blob filename is not UTF-8"))?;
            if name.len() != 64
                || !name.as_bytes().iter().all(u8::is_ascii_hexdigit)
                || name.as_bytes().iter().any(u8::is_ascii_uppercase)
            {
                bail!("invalid OCI SHA-256 blob filename: {name}");
            }
            let digest = Digest::parse(format!("sha256:{name}"))?;
            let file = open_regular_nofollow(&entry.path())?;
            let (actual, size) = digest_reader(file)?;
            if actual != digest {
                bail!("OCI blob failed filename digest verification: {digest}");
            }
            blobs.push(StoredBlob { digest, size });
        }
        blobs.sort_by(|left, right| left.digest.cmp(&right.digest));
        Ok(blobs)
    }

    pub(crate) fn staging_entries(&self) -> Result<u64> {
        let directory = self.root.join(".staging");
        let mut count = 0_u64;
        for entry in fs::read_dir(&directory)
            .with_context(|| format!("failed to list OCI staging in {}", directory.display()))?
        {
            entry.context("failed to read OCI staging entry")?;
            count = count.checked_add(1).context("OCI staging count overflow")?;
            if count > u64::try_from(MAX_STORE_ENTRIES).expect("entry limit fits u64") {
                bail!("OCI staging directory exceeds {MAX_STORE_ENTRIES} entries");
            }
        }
        Ok(count)
    }

    pub(crate) fn verify_stored_blob(&self, blob: &StoredBlob) -> Result<bool> {
        let path = self.blob_path(&blob.digest);
        match fs::symlink_metadata(&path) {
            Ok(_) => {
                Self::verify_path(&path, &blob.digest, Some(blob.size))?;
                Ok(true)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => {
                Err(error).with_context(|| format!("failed to inspect OCI blob {}", blob.digest))
            }
        }
    }

    pub(crate) fn remove_blob(&self, digest: &Digest) -> Result<bool> {
        let path = self.blob_path(digest);
        match fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error).with_context(|| format!("failed to remove OCI blob {digest}")),
        }
    }

    pub(crate) fn sync_blob_directory(&self) -> Result<()> {
        sync_directory(&self.root.join("blobs/sha256"))
    }

    pub(crate) fn upsert_manifest_reference(
        &self,
        descriptor: &OciDescriptor,
        platform: Option<Platform>,
        reference: &str,
        annotation_updates: &BTreeMap<&str, Option<&str>>,
    ) -> Result<ManifestReferenceUpdate> {
        let annotation_updates = annotation_updates
            .iter()
            .map(|(key, value)| ((*key).to_owned(), value.map(ToOwned::to_owned)))
            .collect::<BTreeMap<_, _>>();
        self.update_manifest_reference(descriptor, platform, reference, |_| Ok(annotation_updates))
    }

    pub(crate) fn update_manifest_reference(
        &self,
        descriptor: &OciDescriptor,
        platform: Option<Platform>,
        reference: &str,
        annotation_updates: impl FnOnce(
            Option<&ManifestReference>,
        ) -> Result<BTreeMap<String, Option<String>>>,
    ) -> Result<ManifestReferenceUpdate> {
        validate_reference(reference)?;
        self.verify_manifest_content(descriptor)?;
        self.mutate_index(|manifests| {
            let matching = manifests
                .iter()
                .filter(|entry| reference_name(entry) == Some(reference))
                .collect::<Vec<_>>();
            if matching.len() > 1 {
                bail!("duplicate local OCI reference: {reference}");
            }
            let previous_value = matching.first().map(|entry| (*entry).clone());
            let previous = previous_value
                .as_ref()
                .map(parse_manifest_reference)
                .transpose()?;
            let updates = annotation_updates(previous.as_ref())?;
            let mut annotations = previous_value
                .as_ref()
                .and_then(|entry| entry.get("annotations"))
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            annotations.insert(
                REFERENCE_ANNOTATION.to_owned(),
                Value::String(reference.to_owned()),
            );
            for (key, value) in updates {
                match value {
                    Some(value) => {
                        annotations.insert(key, Value::String(value));
                    }
                    None => {
                        annotations.remove(&key);
                    }
                }
            }
            let mut entry = json!({
                "mediaType": descriptor.media_type,
                "digest": descriptor.digest,
                "size": descriptor.size,
                "annotations": annotations,
            });
            if let Some(platform) = platform {
                entry
                    .as_object_mut()
                    .expect("constructed index descriptor is an object")
                    .insert("platform".to_owned(), serde_json::to_value(platform)?);
            }
            let changed = previous_value.as_ref() != Some(&entry);
            if changed {
                manifests.retain(|entry| reference_name(entry) != Some(reference));
                manifests.push(entry.clone());
            }
            let current = parse_manifest_reference(&entry)?;
            Ok((
                ManifestReferenceUpdate {
                    previous,
                    current,
                    changed,
                },
                changed,
            ))
        })
    }

    pub(crate) fn remove_manifest_reference(
        &self,
        reference: &str,
    ) -> Result<Option<ManifestReference>> {
        validate_reference(reference)?;
        self.mutate_index(|manifests| {
            let matching = manifests
                .iter()
                .filter(|entry| reference_name(entry) == Some(reference))
                .cloned()
                .collect::<Vec<_>>();
            if matching.len() > 1 {
                bail!("duplicate local OCI reference: {reference}");
            }
            let removed = matching
                .into_iter()
                .next()
                .map(|entry| -> Result<ManifestReference> {
                    let entry = parse_index_entry(&entry)?;
                    let reference = entry
                        .reference
                        .context("Catalog entry lost its reference annotation")?;
                    Ok(ManifestReference {
                        reference,
                        descriptor: entry.descriptor,
                        platform: entry.platform,
                        annotations: entry.annotations,
                    })
                })
                .transpose()?;
            manifests.retain(|entry| reference_name(entry) != Some(reference));
            let changed = removed.is_some();
            Ok((removed, changed))
        })
    }

    pub(crate) fn verify_manifest_content(&self, descriptor: &OciDescriptor) -> Result<()> {
        if descriptor.media_type != OCI_IMAGE_MANIFEST {
            bail!(
                "local OCI reference target must be an Image Manifest: {}",
                descriptor.digest
            );
        }
        let bytes = self.get_descriptor_bytes(descriptor)?;
        let manifest: Value = serde_json::from_slice(&bytes)
            .with_context(|| format!("OCI Image Manifest is invalid: {}", descriptor.digest))?;
        let object = manifest.as_object().with_context(|| {
            format!(
                "OCI Image Manifest must be an object: {}",
                descriptor.digest
            )
        })?;
        if object.get("schemaVersion").and_then(Value::as_u64) != Some(2) {
            bail!(
                "OCI Image Manifest schemaVersion must be 2: {}",
                descriptor.digest
            );
        }
        if let Some(media_type) = object.get("mediaType")
            && media_type.as_str() != Some(OCI_IMAGE_MANIFEST)
        {
            bail!(
                "OCI Image Manifest mediaType is invalid: {}",
                descriptor.digest
            );
        }
        if !object.get("config").is_some_and(Value::is_object) {
            bail!(
                "OCI Image Manifest config must be an object: {}",
                descriptor.digest
            );
        }
        if !object.get("layers").is_some_and(Value::is_array) {
            bail!(
                "OCI Image Manifest layers must be an array: {}",
                descriptor.digest
            );
        }
        Ok(())
    }

    fn read_index(&self) -> Result<Value> {
        let path = self.root.join("index.json");
        let bytes = read_bounded(&path, MAX_JSON_BYTES)?;
        serde_json::from_slice(&bytes).context("local OCI Image Layout index.json is invalid")
    }

    fn validate_layout_files(&self) -> Result<()> {
        let marker = read_bounded(&self.root.join("oci-layout"), MAX_JSON_BYTES)?;
        let marker: Value =
            serde_json::from_slice(&marker).context("OCI Layout marker is invalid")?;
        if marker
            .as_object()
            .and_then(|object| object.get("imageLayoutVersion"))
            .and_then(Value::as_str)
            != Some("1.0.0")
        {
            bail!("OCI Layout imageLayoutVersion must be 1.0.0");
        }
        let index = self.read_index()?;
        let object = index
            .as_object()
            .context("local OCI Image Layout index.json must be an object")?;
        if object.get("schemaVersion").and_then(Value::as_u64) != Some(2) {
            bail!("local OCI Image Layout index.json schemaVersion must be 2");
        }
        if let Some(media_type) = object.get("mediaType")
            && media_type.as_str() != Some(OCI_IMAGE_INDEX)
        {
            bail!("local OCI Image Layout index.json mediaType is invalid");
        }
        if !object.get("manifests").is_some_and(Value::is_array) {
            bail!("local OCI Image Layout index.json manifests must be an array");
        }
        Ok(())
    }

    fn index_entries(&self) -> Result<Vec<IndexEntry>> {
        let index = self.read_index()?;
        let manifests = index
            .as_object()
            .and_then(|object| object.get("manifests"))
            .and_then(Value::as_array)
            .context("local OCI Image Layout index.json manifests must be an array")?;
        if manifests.len() > MAX_STORE_ENTRIES {
            bail!("local OCI Image Layout index exceeds {MAX_STORE_ENTRIES} entries");
        }
        manifests.iter().map(parse_index_entry).collect()
    }

    fn mutate_index<T>(
        &self,
        mutate: impl FnOnce(&mut Vec<Value>) -> Result<(T, bool)>,
    ) -> Result<T> {
        let lock_path = self.root.join(".mutation.lock");
        let lock = open_regular_lock(&lock_path, true, "OCI Layout mutation lock")?;
        lock.lock().context("failed to lock OCI Layout mutation")?;

        let path = self.root.join("index.json");
        let mut index = self.read_index()?;
        let object = index
            .as_object_mut()
            .context("local OCI Image Layout index.json must be an object")?;
        let manifests = object
            .get_mut("manifests")
            .and_then(Value::as_array_mut)
            .context("local OCI Image Layout index.json manifests must be an array")?;
        let (result, changed) = mutate(manifests)?;
        if manifests.len() > MAX_STORE_ENTRIES {
            bail!("local OCI Image Layout index exceeds {MAX_STORE_ENTRIES} entries");
        }
        if changed {
            let bytes = canonical_json(&index)?;
            if u64::try_from(bytes.len()).context("OCI index size overflow")? > MAX_JSON_BYTES {
                bail!("local OCI Image Layout index exceeds {MAX_JSON_BYTES} bytes");
            }
            atomic_replace_private(&path, &bytes)?;
        }
        Ok(result)
    }

    fn verify_path(path: &Path, expected: &Digest, expected_size: Option<u64>) -> Result<()> {
        let file = open_regular_nofollow(path)?;
        let (actual, size) = digest_reader(file)?;
        if &actual != expected {
            bail!("OCI blob failed digest verification: {expected}");
        }
        if let Some(expected_size) = expected_size
            && size != expected_size
        {
            bail!(
                "OCI blob size mismatch for {expected}: expected {expected_size}, received {size}"
            );
        }
        Ok(())
    }

    fn open_verified(&self, expected: &Digest, expected_size: Option<u64>) -> Result<File> {
        let path = self.blob_path(expected);
        let mut file = open_regular_nofollow(&path)
            .with_context(|| format!("OCI blob is unavailable: {expected}"))?;
        let (actual, size) = digest_reader(&mut file)?;
        if &actual != expected {
            bail!("OCI blob failed digest verification: {expected}");
        }
        if let Some(expected_size) = expected_size
            && size != expected_size
        {
            bail!(
                "OCI blob size mismatch for {expected}: expected {expected_size}, received {size}"
            );
        }
        file.rewind()
            .with_context(|| format!("failed to rewind OCI blob {expected}"))?;
        Ok(file)
    }

    fn blob_path(&self, digest: &Digest) -> PathBuf {
        self.root.join("blobs/sha256").join(digest.hex())
    }
}

fn open_regular_nofollow(path: &Path) -> Result<File> {
    let file = File::from(
        open(
            path,
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .with_context(|| format!("failed to open OCI blob {}", path.display()))?,
    );
    if !file
        .metadata()
        .with_context(|| format!("failed to inspect OCI blob {}", path.display()))?
        .is_file()
    {
        bail!("OCI blob is not a regular file: {}", path.display());
    }
    Ok(file)
}

fn copy_and_digest(mut reader: impl Read, destination: &mut File) -> Result<(Digest, u64)> {
    let mut writer = BufWriter::with_capacity(COPY_BUFFER_BYTES, destination);
    let mut hasher = Sha256::new();
    let mut size = 0_u64;
    let mut buffer = vec![0_u8; COPY_BUFFER_BYTES];
    loop {
        let read = reader
            .read(&mut buffer)
            .context("failed to read OCI blob")?;
        if read == 0 {
            break;
        }
        writer
            .write_all(&buffer[..read])
            .context("failed to write OCI blob")?;
        hasher.update(&buffer[..read]);
        size = size
            .checked_add(u64::try_from(read).context("OCI blob size overflow")?)
            .context("OCI blob size overflow")?;
    }
    writer.flush().context("failed to flush OCI blob")?;
    let digest = finish_sha256(hasher);
    Ok((digest, size))
}

fn read_bounded(path: &Path, max_bytes: u64) -> Result<Vec<u8>> {
    let file = open_regular_nofollow(path)?;
    read_bounded_reader(file, max_bytes, &path.display().to_string())
}

fn read_bounded_reader(mut file: File, max_bytes: u64, name: &str) -> Result<Vec<u8>> {
    let size = file
        .metadata()
        .with_context(|| format!("failed to inspect {name}"))?
        .len();
    if size > max_bytes {
        bail!("{name} exceeds the {max_bytes}-byte JSON limit");
    }
    let mut bytes = Vec::with_capacity(usize::try_from(size).context("file is too large")?);
    Read::by_ref(&mut file)
        .take(max_bytes + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("failed to read {name}"))?;
    if u64::try_from(bytes.len()).context("file is too large")? > max_bytes {
        bail!("{name} exceeds the {max_bytes}-byte JSON limit");
    }
    Ok(bytes)
}

fn write_new_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let file = options
        .open(path)
        .with_context(|| format!("failed to create {}", path.display()))?;
    let mut writer = BufWriter::new(file);
    writer
        .write_all(bytes)
        .with_context(|| format!("failed to write {}", path.display()))?;
    writer
        .flush()
        .with_context(|| format!("failed to flush {}", path.display()))?;
    writer
        .get_ref()
        .sync_all()
        .with_context(|| format!("failed to fsync {}", path.display()))?;
    sync_directory(path.parent().unwrap_or_else(|| Path::new(".")))
}

fn atomic_replace_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .context("atomic replacement path has no parent")?;
    let mut temporary = NamedTempFile::new_in(parent)
        .with_context(|| format!("failed to create replacement for {}", path.display()))?;
    set_private_file(temporary.as_file())?;
    temporary
        .write_all(bytes)
        .with_context(|| format!("failed to write replacement for {}", path.display()))?;
    temporary
        .as_file_mut()
        .sync_all()
        .with_context(|| format!("failed to fsync replacement for {}", path.display()))?;
    let temporary_path = temporary.into_temp_path();
    fs::rename(&temporary_path, path)
        .with_context(|| format!("failed to publish replacement for {}", path.display()))?;
    sync_directory(parent)
}

fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)
        .with_context(|| format!("failed to open directory {}", path.display()))?
        .sync_all()
        .with_context(|| format!("failed to fsync directory {}", path.display()))
}

fn set_private_file(file: &File) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .context("failed to set owner-only file permissions")?;
    }
    Ok(())
}

fn validate_reference(reference: &str) -> Result<()> {
    if reference.is_empty() || reference.contains(char::is_whitespace) {
        bail!("invalid local OCI reference: {reference}");
    }
    Ok(())
}

fn validate_directory(path: &Path, name: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {name} {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("{name} is not a real directory: {}", path.display());
    }
    Ok(())
}

fn reference_name(value: &Value) -> Option<&str> {
    value
        .as_object()
        .and_then(|entry| entry.get("annotations"))
        .and_then(Value::as_object)
        .and_then(|annotations: &Map<String, Value>| annotations.get(REFERENCE_ANNOTATION))
        .and_then(Value::as_str)
}

fn parse_manifest_reference(value: &Value) -> Result<ManifestReference> {
    let entry = parse_index_entry(value)?;
    let reference = entry
        .reference
        .context("Catalog entry lost its reference annotation")?;
    Ok(ManifestReference {
        reference,
        descriptor: entry.descriptor,
        platform: entry.platform,
        annotations: entry.annotations,
    })
}

fn parse_index_entry(value: &Value) -> Result<IndexEntry> {
    let object = value
        .as_object()
        .context("local OCI index Manifest descriptor must be an object")?;
    let media_type = object
        .get("mediaType")
        .and_then(Value::as_str)
        .context("local OCI index Manifest mediaType must be a string")?;
    if media_type != OCI_IMAGE_MANIFEST {
        bail!("local OCI index root must be an Image Manifest");
    }
    let digest = object
        .get("digest")
        .and_then(Value::as_str)
        .context("local OCI index Manifest digest must be a string")?;
    let size = object
        .get("size")
        .and_then(Value::as_u64)
        .context("local OCI index Manifest size must be an unsigned integer")?;
    let platform = object
        .get("platform")
        .map(|value| {
            serde_json::from_value(value.clone()).context("local OCI index platform is invalid")
        })
        .transpose()?;
    let annotations = object
        .get("annotations")
        .map(|value| {
            let annotations = value
                .as_object()
                .context("local OCI index annotations must be an object")?;
            annotations
                .iter()
                .map(|(key, value)| {
                    let value = value
                        .as_str()
                        .with_context(|| format!("local OCI annotation {key} must be a string"))?;
                    Ok((key.clone(), value.to_owned()))
                })
                .collect::<Result<BTreeMap<_, _>>>()
        })
        .transpose()?
        .unwrap_or_default();
    let reference = annotations.get(REFERENCE_ANNOTATION).cloned();
    if let Some(reference) = &reference {
        validate_reference(reference)?;
    }
    Ok(IndexEntry {
        descriptor: OciDescriptor {
            digest: Digest::parse(digest)?,
            size,
            media_type: media_type.to_owned(),
        },
        platform,
        annotations,
        reference,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::Duration;

    use super::*;
    use crate::integrity::digest_bytes;

    #[test]
    fn digest_is_sha256_of_exact_bytes() {
        assert_eq!(
            digest_bytes(b"hello").as_str(),
            "sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn layout_verifies_descriptor_size() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let layout = OciLayout::open(directory.path()).expect("OCI Layout");
        let descriptor = layout
            .put_bytes(b"content", "application/octet-stream")
            .expect("put blob");
        let wrong = OciDescriptor {
            size: descriptor.size + 1,
            ..descriptor
        };
        let error = layout
            .get_descriptor_path(&wrong)
            .expect_err("wrong size should fail");
        assert!(error.to_string().contains("size mismatch"));
    }

    #[test]
    fn expected_descriptor_includes_media_type() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let layout = OciLayout::open(directory.path()).expect("OCI Layout");
        let expected = OciDescriptor {
            digest: digest_bytes(b"content"),
            size: 7,
            media_type: "application/octet-stream".to_owned(),
        };
        let error = layout
            .put_reader(b"content".as_slice(), "text/plain", Some(&expected))
            .expect_err("wrong mediaType should fail");
        assert!(error.to_string().contains("mediaType mismatch"));
    }

    #[test]
    fn concurrent_first_open_initializes_one_valid_layout() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let root = Arc::new(directory.path().join("oci"));
        let barrier = Arc::new(Barrier::new(8));
        let threads = (0..8)
            .map(|_| {
                let root = Arc::clone(&root);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    OciLayout::open(root.as_path()).expect("concurrent open")
                })
            })
            .collect::<Vec<_>>();
        for thread in threads {
            thread.join().expect("open thread");
        }
        let index: Value =
            serde_json::from_slice(&fs::read(root.join("index.json")).expect("index bytes"))
                .expect("index JSON");
        assert_eq!(index["manifests"], json!([]));
    }

    #[cfg(unix)]
    #[test]
    fn layout_mutation_lock_never_follows_a_symlink() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().expect("temporary directory");
        let outside = directory.path().join("outside");
        fs::write(&outside, b"outside").expect("outside file");
        let root = directory.path().join("oci");
        fs::create_dir(&root).expect("OCI root");
        symlink(&outside, root.join(".mutation.lock")).expect("lock symlink");

        let error = OciLayout::open(&root).expect_err("lock symlink must fail closed");
        assert!(format!("{error:#}").contains("failed to open OCI Layout mutation lock"));
        assert_eq!(fs::read(outside).expect("outside bytes"), b"outside");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn layout_mutation_lock_rejects_a_fifo_without_blocking() {
        use rustix::fs::{CWD, Mode, mkfifoat};

        let directory = tempfile::tempdir().expect("temporary directory");
        let root = directory.path().join("oci");
        fs::create_dir(&root).expect("OCI root");
        let lock = root.join(".mutation.lock");
        mkfifoat(CWD, &lock, Mode::RUSR | Mode::WUSR).expect("lock FIFO");

        let error = OciLayout::open(&root).expect_err("lock FIFO must fail closed");
        assert!(format!("{error:#}").contains("OCI Layout mutation lock is not a regular file"));
    }

    #[test]
    fn reference_update_observes_metadata_under_the_index_lock() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let layout = OciLayout::open(directory.path()).expect("OCI Layout");
        let manifest = layout
            .put_bytes(
                br#"{"schemaVersion":2,"config":{},"layers":[]}"#,
                OCI_IMAGE_MANIFEST,
            )
            .expect("Manifest");
        layout
            .upsert_manifest_reference(
                &manifest,
                Some(Platform::linux(crate::core::Architecture::Amd64)),
                "runlab/atomic:test",
                &BTreeMap::from([("test.description", Some("initial"))]),
            )
            .expect("initial reference");

        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let first_layout = layout.clone();
        let first_manifest = manifest.clone();
        let first = thread::spawn(move || {
            first_layout
                .update_manifest_reference(
                    &first_manifest,
                    Some(Platform::linux(crate::core::Architecture::Amd64)),
                    "runlab/atomic:test",
                    |_| {
                        entered_tx.send(()).expect("entered update");
                        release_rx.recv().expect("release update");
                        Ok(BTreeMap::from([(
                            "test.description".to_owned(),
                            Some("updated".to_owned()),
                        )]))
                    },
                )
                .expect("first update");
        });
        entered_rx.recv().expect("first update entered");

        let (observed_tx, observed_rx) = mpsc::channel();
        let second_layout = layout.clone();
        let second_manifest = manifest.clone();
        let second = thread::spawn(move || {
            second_layout
                .update_manifest_reference(
                    &second_manifest,
                    Some(Platform::linux(crate::core::Architecture::Amd64)),
                    "runlab/atomic:test",
                    |previous| {
                        observed_tx
                            .send(
                                previous
                                    .and_then(|entry| entry.annotations.get("test.description"))
                                    .cloned(),
                            )
                            .expect("observed metadata");
                        Ok(BTreeMap::new())
                    },
                )
                .expect("second update");
        });
        assert!(
            observed_rx
                .recv_timeout(Duration::from_millis(100))
                .is_err()
        );
        release_tx.send(()).expect("release first update");
        assert_eq!(
            observed_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("second update observed metadata"),
            Some("updated".to_owned())
        );
        first.join().expect("first update thread");
        second.join().expect("second update thread");
    }

    #[test]
    fn index_mutation_rejects_an_oversized_result_without_replacing_the_index() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let layout = OciLayout::open(directory.path()).expect("OCI Layout");
        let manifest = layout
            .put_bytes(
                br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{},"layers":[]}"#,
                OCI_IMAGE_MANIFEST,
            )
            .expect("Manifest");
        let mut index = json!({
            "schemaVersion": 2,
            "mediaType": OCI_IMAGE_INDEX,
            "manifests": [{
                "mediaType": manifest.media_type,
                "digest": manifest.digest,
                "size": manifest.size,
                "annotations": {"test.padding": ""}
            }]
        });
        let base = canonical_json(&index).expect("base index");
        let padding = usize::try_from(MAX_JSON_BYTES).expect("JSON limit") - base.len();
        index["manifests"][0]["annotations"]["test.padding"] = Value::String("x".repeat(padding));
        let original = canonical_json(&index).expect("maximum-size index");
        assert_eq!(
            original.len(),
            usize::try_from(MAX_JSON_BYTES).expect("JSON limit")
        );
        fs::write(directory.path().join("index.json"), &original).expect("index bytes");

        let error = layout
            .upsert_manifest_reference(&manifest, None, "runlab/too-large:test", &BTreeMap::new())
            .expect_err("oversized mutation must fail");
        assert!(format!("{error:#}").contains("index exceeds 16777216 bytes"));
        assert_eq!(
            fs::read(directory.path().join("index.json")).expect("unchanged index"),
            original
        );
    }

    #[test]
    fn blob_staging_never_enters_the_digest_namespace() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let layout = OciLayout::open(directory.path()).expect("OCI Layout");
        let descriptor = layout
            .put_bytes(b"content", "application/octet-stream")
            .expect("put blob");
        let names = fs::read_dir(directory.path().join("blobs/sha256"))
            .expect("blob directory")
            .map(|entry| entry.expect("blob entry").file_name())
            .collect::<Vec<_>>();
        assert_eq!(names, [descriptor.digest.hex()]);
        assert_eq!(
            fs::read_dir(directory.path().join(".staging"))
                .expect("staging directory")
                .count(),
            0
        );
    }

    #[test]
    fn manifest_reference_rejects_a_non_oci_manifest_body() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let layout = OciLayout::open(directory.path()).expect("OCI Layout");
        let descriptor = layout
            .put_bytes(
                &canonical_json(&json!({
                    "schemaVersion": 2,
                    "mediaType": "application/vnd.docker.distribution.manifest.v2+json",
                    "config": {},
                    "layers": []
                }))
                .expect("manifest bytes"),
                OCI_IMAGE_MANIFEST,
            )
            .expect("manifest");
        let error = layout
            .verify_manifest_content(&descriptor)
            .expect_err("non-OCI body");
        assert!(error.to_string().contains("mediaType is invalid"));
    }

    #[cfg(unix)]
    #[test]
    fn existing_blob_symlink_is_never_followed_or_overwritten() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().expect("temporary directory");
        let layout = OciLayout::open(directory.path()).expect("OCI Layout");
        let bytes = b"content behind a symlink";
        let digest = digest_bytes(bytes);
        let outside = directory.path().join("outside");
        fs::write(&outside, bytes).expect("outside bytes");
        symlink(
            &outside,
            directory.path().join("blobs/sha256").join(digest.hex()),
        )
        .expect("blob symlink");
        let error = layout
            .put_reader(
                bytes.as_slice(),
                "application/octet-stream",
                Some(&OciDescriptor {
                    digest,
                    size: u64::try_from(bytes.len()).expect("size"),
                    media_type: "application/octet-stream".to_owned(),
                }),
            )
            .expect_err("blob symlink must fail");
        assert!(format!("{error:#}").contains("failed to open OCI blob"));
        assert_eq!(fs::read(outside).expect("outside bytes"), bytes);
    }
}
