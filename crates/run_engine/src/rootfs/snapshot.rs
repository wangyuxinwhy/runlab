use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write as _};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use oci_spec::image::{Descriptor, Digest};
use rustix::fs::{Mode, OFlags, open};
use rustix::mount::{MountFlags, UnmountFlags, mount, unmount};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::apply::{CleanupBudget, apply_directory_metadata, apply_plan};
use super::capture::capture_stable;
use super::layer::{scan_layer, update_directory_metadata, validate_workspace, verify_and_stage};
use super::plan::{LayerKind, LayerPlan};
use super::preflight::MaterializationBudget;
use super::{
    FsPath, Inventory, Metadata, Rootfs, RootfsError, RootfsErrorKind, RootfsLimits,
    classify_materialization_error, default_directory, enforce, usize_to_u64,
};
use crate::oci::{ImagePlan, VerifiedImage};
use crate::rootfs::VerifiedLayer;

const CACHE_FORMAT_VERSION: u32 = 3;
const VALIDATION_FORMAT_VERSION: u32 = 1;
const VALIDATION_FILE: &str = "validation-v1.json";

pub(super) fn cached_image_validation(
    cache_root: &Path,
    image: &ImagePlan,
) -> Result<Option<Vec<u64>>> {
    let chains_root = cache_root.join("chains");
    let mut parent = None;
    let mut sizes = Vec::with_capacity(image.layers().len());
    for (descriptor, diff_id) in image.layers() {
        let id = chain_id(parent.as_deref(), diff_id)?;
        let chain = chains_root.join(&id);
        if !valid_chain(&chain, &id, parent.as_deref(), diff_id) {
            return Ok(None);
        }
        let validation: StoredValidation = match File::open(chain.join(VALIDATION_FILE))
            .ok()
            .and_then(|file| serde_json::from_reader(file).ok())
        {
            Some(validation) => validation,
            None => return Ok(None),
        };
        if !validation.matches(&id, descriptor, diff_id) {
            return Ok(None);
        }
        sizes.push(validation.uncompressed_size);
        parent = Some(id);
    }
    Ok(Some(sizes))
}

pub(super) fn record_image_validation(cache_root: &Path, image: &VerifiedImage) -> Result<()> {
    let chains_root = cache_root.join("chains");
    let mut parent = None;
    for layer in image.layers() {
        let id = chain_id(parent.as_deref(), layer.diff_id())?;
        let chain = chains_root.join(&id);
        if !valid_chain(&chain, &id, parent.as_deref(), layer.diff_id()) {
            bail!("cannot record validation for absent snapshot chain {id}");
        }
        let validation = StoredValidation::new(&id, layer);
        let destination = chain.join(VALIDATION_FILE);
        if File::open(&destination)
            .ok()
            .and_then(|file| serde_json::from_reader::<_, StoredValidation>(file).ok())
            .is_some_and(|existing| existing == validation)
        {
            parent = Some(id);
            continue;
        }
        let mut temporary = tempfile::NamedTempFile::new_in(&chain)?;
        serde_json::to_writer(temporary.as_file_mut(), &validation)?;
        temporary.as_file_mut().flush()?;
        temporary.as_file_mut().sync_all()?;
        temporary
            .persist(&destination)
            .map_err(|error| error.error)?;
        parent = Some(id);
    }
    Ok(())
}

#[derive(Debug)]
pub(super) struct OverlayMount {
    target: PathBuf,
    upper: PathBuf,
    mounted: bool,
}

impl OverlayMount {
    fn new(lower: &[PathBuf], upper: &Path, work: &Path, target: &Path) -> Result<Self> {
        let options = overlay_options(lower, upper, work)?;
        mount(
            "overlay",
            target,
            "overlay",
            MountFlags::empty(),
            Some(options.as_c_str()),
        )
        .with_context(|| format!("failed to mount OverlayFS at {}", target.display()))?;
        Ok(Self {
            target: target.to_owned(),
            upper: upper.to_owned(),
            mounted: true,
        })
    }

    pub(super) fn upper(&self) -> &Path {
        &self.upper
    }

    pub(super) fn unmount(&mut self) -> Result<()> {
        if self.mounted {
            unmount(&self.target, UnmountFlags::empty()).with_context(|| {
                format!("failed to unmount OverlayFS at {}", self.target.display())
            })?;
            self.mounted = false;
        }
        Ok(())
    }
}

impl Drop for OverlayMount {
    fn drop(&mut self) {
        if self.mounted {
            let _ = unmount(&self.target, UnmountFlags::empty());
        }
    }
}

impl Rootfs {
    pub(crate) fn materialize_cached_in<F, R>(
        workspace: &Path,
        cache_root: &Path,
        layers: &[VerifiedLayer<'_>],
        limits: RootfsLimits,
        mut open_layer: F,
    ) -> std::result::Result<Self, RootfsError>
    where
        F: FnMut(&Descriptor) -> Result<R>,
        R: Read,
    {
        materialize_cached(workspace, cache_root, layers, limits, &mut open_layer)
            .map_err(|error| classify_materialization_error(error, RootfsErrorKind::Internal))
    }
}

fn materialize_cached<F, R>(
    workspace: &Path,
    cache_root: &Path,
    layers: &[VerifiedLayer<'_>],
    limits: RootfsLimits,
    open_layer: &mut F,
) -> Result<Rootfs>
where
    F: FnMut(&Descriptor) -> Result<R>,
    R: Read,
{
    enforce("layer count", limits.layers, usize_to_u64(layers.len()))
        .map_err(|error| classified(error, RootfsErrorKind::UnsupportedInput))?;
    validate_workspace(workspace).map_err(|error| classified(error, RootfsErrorKind::Internal))?;
    validate_workspace(cache_root).map_err(|error| classified(error, RootfsErrorKind::Internal))?;
    let chains_root = private_subdirectory(cache_root, "chains")?;
    let inventories_root = private_subdirectory(cache_root, "inventories")?;
    let empty = private_subdirectory(cache_root, "empty")?;

    let mut parent = None;
    let mut lower = Vec::with_capacity(layers.len().max(1));
    for layer in layers {
        let id = chain_id(parent.as_deref(), layer.expected_diff_id)?;
        ensure_chain(
            &chains_root,
            &empty,
            &lower,
            parent.as_deref(),
            &id,
            layer,
            limits,
            open_layer,
        )?;
        lower.push(chains_root.join(&id).join("upper"));
        parent = Some(id);
    }
    if lower.is_empty() {
        lower.push(empty);
    }
    let root_metadata = parent.as_deref().map_or_else(
        || Ok(default_directory()),
        |parent| {
            read_directories(&chains_root.join(parent).join("directories.bin"))?
                .remove(&FsPath(Box::default()))
                .context("snapshot directory cache has no root metadata")
        },
    )?;
    let inventory_key = parent.unwrap_or_else(|| "empty".to_owned());
    mount_run_rootfs(
        workspace,
        &lower,
        &inventories_root.join(format!("{inventory_key}.bin")),
        &root_metadata,
        limits,
    )
}

#[allow(
    clippy::too_many_arguments,
    reason = "the one-layer cache build keeps the verified layer, parent chain, and storage boundaries explicit"
)]
fn ensure_chain<F, R>(
    chains_root: &Path,
    empty: &Path,
    parent_lower: &[PathBuf],
    parent: Option<&str>,
    id: &str,
    layer: &VerifiedLayer<'_>,
    limits: RootfsLimits,
    open_layer: &mut F,
) -> Result<()>
where
    F: FnMut(&Descriptor) -> Result<R>,
    R: Read,
{
    let destination = chains_root.join(id);
    if valid_chain(&destination, id, parent, layer.expected_diff_id) {
        return Ok(());
    }
    if destination.exists() {
        bail!(
            "NativeEngine snapshot cache entry is incomplete: {}",
            destination.display()
        );
    }

    let build = tempfile::Builder::new()
        .prefix("build-")
        .tempdir_in(chains_root)?;
    let entry = private_subdirectory(build.path(), "entry")?;
    let scratch = private_subdirectory(build.path(), "scratch")?;
    let upper = private_subdirectory(&entry, "upper")?;
    let work = private_subdirectory(&scratch, "work")?;
    let merged = private_subdirectory(&scratch, "merged")?;
    let mut lower = parent_lower.iter().rev().cloned().collect::<Vec<_>>();
    if lower.is_empty() {
        lower.push(empty.to_owned());
    }
    let mut overlay = OverlayMount::new(&lower, &upper, &work, &merged)?;
    let root = open(
        &merged,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let mut directories = parent.map_or_else(
        || {
            Ok(BTreeMap::from([(
                FsPath(Box::default()),
                default_directory(),
            )]))
        },
        |parent| read_directories(&chains_root.join(parent).join("directories.bin")),
    )?;
    let mut budget = MaterializationBudget::new(limits);
    budget
        .compressed(layer.descriptor.size())
        .map_err(|error| classified(error, RootfsErrorKind::UnsupportedInput))?;
    let source = open_layer(layer.descriptor)
        .map_err(|error| classified(error, RootfsErrorKind::Content))?;
    let mut decoded = verify_and_stage(&scratch, layer, source, budget.remaining_uncompressed())
        .map_err(|error| classified(error, RootfsErrorKind::InvalidInput))?;
    let decoded_size = decoded.as_file().metadata()?.len();
    budget
        .uncompressed(decoded_size)
        .map_err(|error| classified(error, RootfsErrorKind::UnsupportedInput))?;
    let plan = scan_layer(decoded.as_file_mut(), &scratch, limits, &mut budget)
        .map_err(|error| classified(error, RootfsErrorKind::InvalidInput))?;
    let touched = touched_directories(&plan);
    update_directory_metadata(&mut directories, &plan)
        .map_err(|error| classified(error, RootfsErrorKind::InvalidInput))?;
    let mut cleanup = CleanupBudget::new(limits);
    apply_plan(&root, &plan, limits, &mut cleanup)
        .map_err(|error| classified(error, RootfsErrorKind::Internal))?;
    apply_directory_metadata(
        &root,
        directories
            .iter()
            .filter(|(path, _)| touched.contains(*path))
            .map(|(path, metadata)| (path.clone(), metadata.clone()))
            .collect(),
        limits,
    )
    .map_err(|error| classified(error, RootfsErrorKind::Internal))?;
    drop(root);
    overlay.unmount()?;
    publish_chain(
        &build,
        &destination,
        id,
        parent,
        layer.expected_diff_id,
        &directories,
    )
}

fn publish_chain(
    build: &tempfile::TempDir,
    destination: &Path,
    id: &str,
    parent: Option<&str>,
    diff_id: &Digest,
    directories: &BTreeMap<FsPath, Metadata>,
) -> Result<()> {
    let entry = build.path().join("entry");
    write_cache(&entry.join("directories.bin"), directories)?;
    write_cache(
        &entry.join("chain.bin"),
        &StoredChain {
            schema_version: CACHE_FORMAT_VERSION,
            chain_id: id.to_owned(),
            parent_chain_id: parent.map(ToOwned::to_owned),
            diff_id: diff_id.to_string(),
        },
    )?;
    match fs::rename(&entry, destination) {
        Ok(()) => Ok(()),
        Err(error) if destination.exists() => {
            if valid_chain(destination, id, parent, diff_id) {
                Ok(())
            } else {
                Err(error).with_context(|| {
                    format!(
                        "failed to publish snapshot cache entry {}",
                        destination.display()
                    )
                })
            }
        }
        Err(error) => Err(error).with_context(|| {
            format!(
                "failed to publish snapshot cache entry {}",
                destination.display()
            )
        }),
    }
}

fn mount_run_rootfs(
    workspace: &Path,
    lower: &[PathBuf],
    inventory_path: &Path,
    root_metadata: &Metadata,
    limits: RootfsLimits,
) -> Result<Rootfs> {
    let upper = private_subdirectory(workspace, "overlay-upper")?;
    let work = private_subdirectory(workspace, "overlay-work")?;
    let root_path = private_subdirectory(workspace, "rootfs")?;
    initialize_upper_root(&upper, root_metadata, limits)?;
    let lower = lower.iter().rev().cloned().collect::<Vec<_>>();
    let overlay = OverlayMount::new(&lower, &upper, &work, &root_path)?;
    let root = open(
        &root_path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let initial = if inventory_path.is_file() {
        read_inventory(inventory_path)?
    } else {
        let inventory = capture_stable(&root, workspace, limits, false)
            .map_err(|error| classified(error, RootfsErrorKind::Internal))?
            .inventory;
        publish_cache(inventory_path, &inventory)?;
        inventory
    };
    Ok(Rootfs::from_overlay(
        workspace.to_owned(),
        root_path,
        root,
        initial,
        limits,
        overlay,
    ))
}

fn initialize_upper_root(
    upper: &Path,
    root_metadata: &Metadata,
    limits: RootfsLimits,
) -> Result<()> {
    let root = open(
        upper,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    apply_directory_metadata(
        &root,
        BTreeMap::from([(FsPath(Box::default()), root_metadata.clone())]),
        limits,
    )
}

fn overlay_options(lower: &[PathBuf], upper: &Path, work: &Path) -> Result<CString> {
    if lower.is_empty() {
        bail!("OverlayFS requires at least one lower directory");
    }
    let mut bytes = b"lowerdir=".to_vec();
    for (index, path) in lower.iter().enumerate() {
        if index != 0 {
            bytes.push(b':');
        }
        append_overlay_path(&mut bytes, path)?;
    }
    bytes.extend_from_slice(b",upperdir=");
    append_overlay_path(&mut bytes, upper)?;
    bytes.extend_from_slice(b",workdir=");
    append_overlay_path(&mut bytes, work)?;
    bytes.extend_from_slice(b",index=off,metacopy=off,redirect_dir=off");
    if bytes.len() >= 4096 {
        bail!("OverlayFS snapshot chain exceeds the mount option size limit");
    }
    CString::new(bytes).context("OverlayFS options contain NUL")
}

fn append_overlay_path(options: &mut Vec<u8>, path: &Path) -> Result<()> {
    let path = path.as_os_str().as_bytes();
    if path.contains(&b',') || path.contains(&b':') {
        bail!("OverlayFS cache paths cannot contain ',' or ':'");
    }
    options.extend_from_slice(path);
    Ok(())
}

fn private_subdirectory(parent: &Path, name: &str) -> Result<PathBuf> {
    let path = parent.join(name);
    match fs::create_dir(&path) {
        Ok(()) => fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && path.is_dir() => {}
        Err(error) => return Err(error.into()),
    }
    Ok(path)
}

fn chain_id(parent: Option<&str>, diff_id: &Digest) -> Result<String> {
    if diff_id.algorithm().as_ref() != "sha256" {
        bail!("only sha256 DiffIDs can identify NativeEngine snapshots");
    }
    if let Some(parent) = parent {
        let mut hash = Sha256::new();
        hash.update(parent.as_bytes());
        hash.update(b" ");
        hash.update(diff_id.to_string().as_bytes());
        Ok(hex(&hash.finalize()))
    } else {
        Ok(diff_id.digest().to_owned())
    }
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(char::from(DIGITS[usize::from(byte >> 4)]));
        encoded.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn valid_chain(path: &Path, id: &str, parent: Option<&str>, diff_id: &Digest) -> bool {
    if !path.is_dir() || !path.join("upper").is_dir() {
        return false;
    }
    let marker: StoredChain = match read_cache(&path.join("chain.bin")) {
        Ok(marker) => marker,
        Err(_) => return false,
    };
    marker.schema_version == CACHE_FORMAT_VERSION
        && marker.chain_id == id
        && marker.parent_chain_id.as_deref() == parent
        && marker.diff_id == diff_id.to_string()
        && path.join("directories.bin").is_file()
}

fn touched_directories(plan: &LayerPlan) -> BTreeSet<FsPath> {
    let mut touched = BTreeSet::from([FsPath(Box::default())]);
    for path in plan.whiteouts.iter().chain(&plan.opaques) {
        include_ancestors(&mut touched, path.parent());
    }
    for entry in &plan.entries {
        let current = if matches!(entry.kind, LayerKind::Directory) {
            entry.path.clone()
        } else {
            entry.path.parent()
        };
        include_ancestors(&mut touched, current);
    }
    touched
}

fn include_ancestors(touched: &mut BTreeSet<FsPath>, mut current: FsPath) {
    loop {
        touched.insert(current.clone());
        if current.is_root() {
            break;
        }
        current = current.parent();
    }
}

fn classified(error: anyhow::Error, kind: RootfsErrorKind) -> anyhow::Error {
    classify_materialization_error(error, kind).into()
}

fn read_directories(path: &Path) -> Result<BTreeMap<FsPath, Metadata>> {
    read_cache(path)
}

fn read_inventory(path: &Path) -> Result<Inventory> {
    read_cache(path)
}

fn read_cache<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let mut file = File::open(path)?;
    bincode::serde::decode_from_std_read(&mut file, bincode::config::standard())
        .with_context(|| format!("invalid cache file {}", path.display()))
}

fn write_cache(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    bincode::serde::encode_into_std_write(value, &mut file, bincode::config::standard())?;
    file.flush()?;
    file.sync_all()?;
    Ok(())
}

fn publish_cache(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path.parent().context("cache file has no parent")?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    bincode::serde::encode_into_std_write(
        value,
        temporary.as_file_mut(),
        bincode::config::standard(),
    )?;
    temporary.as_file_mut().flush()?;
    temporary.as_file_mut().sync_all()?;
    match temporary.persist_noclobber(path) {
        Ok(_) => Ok(()),
        Err(_error) if path.is_file() => Ok(()),
        Err(error) => Err(error.error.into()),
    }
}

#[derive(Deserialize, Serialize)]
struct StoredChain {
    schema_version: u32,
    chain_id: String,
    parent_chain_id: Option<String>,
    diff_id: String,
}

#[derive(Eq, PartialEq, Deserialize, Serialize)]
struct StoredValidation {
    schema_version: u32,
    chain_id: String,
    descriptor_digest: String,
    descriptor_size: u64,
    descriptor_media_type: String,
    diff_id: String,
    uncompressed_size: u64,
}

impl StoredValidation {
    fn new(id: &str, layer: &crate::oci::VerifiedLayer) -> Self {
        Self {
            schema_version: VALIDATION_FORMAT_VERSION,
            chain_id: id.to_owned(),
            descriptor_digest: layer.descriptor().digest().to_string(),
            descriptor_size: layer.descriptor().size(),
            descriptor_media_type: layer.descriptor().media_type().to_string(),
            diff_id: layer.diff_id().to_string(),
            uncompressed_size: layer.uncompressed_size(),
        }
    }

    fn matches(&self, id: &str, descriptor: &Descriptor, diff_id: &Digest) -> bool {
        self.schema_version == VALIDATION_FORMAT_VERSION
            && self.chain_id == id
            && self.descriptor_digest == descriptor.digest().to_string()
            && self.descriptor_size == descriptor.size()
            && self.descriptor_media_type == descriptor.media_type().to_string()
            && self.diff_id == diff_id.to_string()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::str::FromStr as _;

    use super::*;

    #[test]
    fn chain_publication_excludes_build_scratch() {
        let root = tempfile::tempdir().expect("cache root");
        let chains = private_subdirectory(root.path(), "chains").expect("chains root");
        let build = tempfile::Builder::new()
            .prefix("build-")
            .tempdir_in(&chains)
            .expect("build workspace");
        let build_path = build.path().to_owned();
        let entry = private_subdirectory(build.path(), "entry").expect("entry");
        private_subdirectory(&entry, "upper").expect("upper");
        let scratch = private_subdirectory(build.path(), "scratch").expect("scratch");
        fs::write(scratch.join("decoded-layer.tar"), b"temporary").expect("scratch file");
        fs::write(scratch.join("staged-content"), b"temporary").expect("staged file");

        let id = "chain";
        let destination = chains.join(id);
        let diff_id =
            Digest::from_str(&format!("sha256:{}", "0".repeat(64))).expect("sha256 digest");
        let directories = BTreeMap::from([(FsPath(Box::default()), default_directory())]);

        publish_chain(&build, &destination, id, None, &diff_id, &directories)
            .expect("published chain");
        drop(build);

        let names = fs::read_dir(&destination)
            .expect("published entry")
            .map(|entry| entry.expect("entry name").file_name())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            names,
            BTreeSet::from(["chain.bin".into(), "directories.bin".into(), "upper".into(),])
        );
        assert!(!build_path.exists(), "build scratch survived publication");
        assert!(valid_chain(&destination, id, None, &diff_id));
    }
}
