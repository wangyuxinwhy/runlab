use std::fs::{self, File};
use std::os::unix::fs::MetadataExt as _;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use rustix::fs::{Mode, OFlags, open};

use crate::runtime::NativeFileMount;

const MAX_FILE_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    device: u64,
    inode: u64,
    size: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

impl FileIdentity {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            size: metadata.size(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
            changed_seconds: metadata.ctime(),
            changed_nanoseconds: metadata.ctime_nsec(),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct VerifiedSourceFile {
    mount_index: usize,
    source: PathBuf,
    destination: PathBuf,
    identity: FileIdentity,
    pinned_source: Arc<File>,
}

impl VerifiedSourceFile {
    #[must_use]
    pub(crate) fn mount_index(&self) -> usize {
        self.mount_index
    }

    #[must_use]
    pub(crate) fn destination(&self) -> &Path {
        &self.destination
    }

    pub(crate) fn verify_source(&self) -> Result<()> {
        let pinned = FileIdentity::from_metadata(
            &self
                .pinned_source
                .metadata()
                .context("failed to re-inspect pinned read-only file mount source")?,
        );
        let (_, observed) = inspect_source(&self.source, None)?;
        if pinned != self.identity || observed != self.identity {
            bail!(
                "native read-only file mount source identity changed after acceptance: {}",
                self.source.display()
            );
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct DestinationFileGuard {
    files: Vec<GuardedDestination>,
}

#[derive(Debug)]
struct GuardedDestination {
    path: PathBuf,
    identity: FileIdentity,
    pinned_file: File,
}

impl DestinationFileGuard {
    pub(crate) fn prepare(rootfs: &Path, files: &[VerifiedSourceFile]) -> Result<Self> {
        let canonical_rootfs = rootfs
            .canonicalize()
            .with_context(|| format!("failed to canonicalize rootfs {}", rootfs.display()))?;
        let mut guarded = Vec::with_capacity(files.len());
        for file in files {
            let relative = file
                .destination
                .strip_prefix("/")
                .expect("validated native mount destination is absolute");
            let destination = canonical_rootfs.join(relative);
            verify_no_symlink_components(&destination)?;
            let canonical_destination = destination.canonicalize().with_context(|| {
                format!(
                    "native read-only file mount destination must already exist in the Initial Image: {}",
                    file.destination.display()
                )
            })?;
            if canonical_destination != destination
                || !canonical_destination.starts_with(&canonical_rootfs)
            {
                bail!(
                    "native read-only file mount destination must be a normalized path inside the Initial Image: {}",
                    file.destination.display()
                );
            }
            let metadata = fs::symlink_metadata(&destination).with_context(|| {
                format!(
                    "failed to inspect native read-only file mount destination {}",
                    file.destination.display()
                )
            })?;
            if !metadata.is_file() {
                bail!(
                    "native read-only file mount destination must be a regular file in the Initial Image: {}",
                    file.destination.display()
                );
            }
            let pinned_file = File::from(
                open(
                    &destination,
                    OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .with_context(|| {
                    format!(
                        "failed to pin native read-only file mount destination {}",
                        file.destination.display()
                    )
                })?,
            );
            let identity = FileIdentity::from_metadata(&metadata);
            if FileIdentity::from_metadata(&pinned_file.metadata().with_context(|| {
                format!(
                    "failed to inspect pinned native read-only file mount destination {}",
                    file.destination.display()
                )
            })?) != identity
            {
                bail!(
                    "native read-only file mount destination changed while it was inspected: {}",
                    file.destination.display()
                );
            }
            guarded.push(GuardedDestination {
                path: destination,
                identity,
                pinned_file,
            });
        }
        Ok(Self { files: guarded })
    }

    pub(crate) fn verify_unchanged(&self) -> Result<()> {
        for guarded in &self.files {
            verify_no_symlink_components(&guarded.path)?;
            let metadata = fs::symlink_metadata(&guarded.path).with_context(|| {
                format!(
                    "failed to re-inspect native read-only file mount destination {}",
                    guarded.path.display()
                )
            })?;
            let pinned = guarded.pinned_file.metadata().with_context(|| {
                format!(
                    "failed to re-inspect pinned native read-only file mount destination {}",
                    guarded.path.display()
                )
            })?;
            if !metadata.is_file()
                || FileIdentity::from_metadata(&metadata) != guarded.identity
                || FileIdentity::from_metadata(&pinned) != guarded.identity
            {
                bail!(
                    "native read-only file mount destination changed during execution: {}",
                    guarded.path.display()
                );
            }
        }
        Ok(())
    }
}

pub(crate) fn verify_sources(
    mounts: &[NativeFileMount],
    state_root: &Path,
) -> Result<Vec<VerifiedSourceFile>> {
    let canonical_state_root = state_root.canonicalize().with_context(|| {
        format!(
            "failed to canonicalize RunLab state root {}",
            state_root.display()
        )
    })?;
    mounts
        .iter()
        .map(|mount| {
            let (source, identity) = inspect_source(mount.source(), Some(&canonical_state_root))?;
            Ok(VerifiedSourceFile {
                mount_index: mount.mount_index(),
                source: mount.source().to_path_buf(),
                destination: mount.destination().to_path_buf(),
                identity,
                pinned_source: Arc::new(source),
            })
        })
        .collect()
}

pub(crate) fn verify_all_sources(files: &[VerifiedSourceFile]) -> Result<()> {
    files.iter().try_for_each(VerifiedSourceFile::verify_source)
}

fn inspect_source(path: &Path, state_root: Option<&Path>) -> Result<(File, FileIdentity)> {
    verify_no_symlink_components(path)?;
    let canonical = path.canonicalize().with_context(|| {
        format!(
            "failed to canonicalize native read-only file mount source {}",
            path.display()
        )
    })?;
    if canonical != path {
        bail!(
            "native read-only file mount source must be an absolute canonical path without symbolic links: {}",
            path.display()
        );
    }
    if state_root.is_some_and(|root| canonical.starts_with(root)) {
        bail!(
            "native read-only file mount source must be outside the RunLab state root: {}",
            path.display()
        );
    }
    let path_metadata = fs::symlink_metadata(path).with_context(|| {
        format!(
            "failed to inspect native read-only file mount source {}",
            path.display()
        )
    })?;
    if !path_metadata.is_file() {
        bail!(
            "native read-only file mount source must be a regular file: {}",
            path.display()
        );
    }
    if path_metadata.size() > MAX_FILE_BYTES {
        bail!(
            "native read-only file mount source exceeds the {MAX_FILE_BYTES}-byte limit: {}",
            path.display()
        );
    }
    let expected_owner = rustix::process::geteuid().as_raw();
    if path_metadata.uid() != expected_owner {
        bail!(
            "native read-only file mount source must be owned by uid {expected_owner}: {}",
            path.display()
        );
    }
    if path_metadata.mode() & 0o077 != 0 {
        bail!(
            "native read-only file mount source must not grant group or other permissions: {}",
            path.display()
        );
    }
    let source = File::from(
        open(
            path,
            OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .with_context(|| {
            format!(
                "failed to pin native read-only file mount source {}",
                path.display()
            )
        })?,
    );
    let opened_metadata = source.metadata().with_context(|| {
        format!(
            "failed to inspect pinned native read-only file mount source {}",
            path.display()
        )
    })?;
    let identity = FileIdentity::from_metadata(&path_metadata);
    if FileIdentity::from_metadata(&opened_metadata) != identity {
        bail!(
            "native read-only file mount source changed while it was inspected: {}",
            path.display()
        );
    }
    Ok((source, identity))
}

fn verify_no_symlink_components(path: &Path) -> Result<()> {
    let mut current = PathBuf::new();
    for component in path.components() {
        match component {
            Component::RootDir => current.push(component.as_os_str()),
            Component::Normal(part) => {
                current.push(part);
                let metadata = fs::symlink_metadata(&current).with_context(|| {
                    format!("failed to inspect path component {}", current.display())
                })?;
                if metadata.file_type().is_symlink() {
                    bail!(
                        "path must not contain symbolic links: {}",
                        current.display()
                    );
                }
            }
            Component::CurDir | Component::ParentDir | Component::Prefix(_) => {
                bail!("path must be absolute and normalized: {}", path.display());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _, symlink};

    use super::*;

    fn private_file(path: &Path) {
        OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(path)
            .expect("private file");
    }

    fn mount(source: &Path, destination: &str) -> NativeFileMount {
        NativeFileMount::for_test(source.to_path_buf(), PathBuf::from(destination))
    }

    #[test]
    fn verifies_private_regular_source_and_existing_destination() {
        let workspace = tempfile::tempdir().expect("workspace");
        let state = workspace.path().join("state");
        let external = workspace.path().join("external");
        let rootfs = workspace.path().join("rootfs");
        fs::create_dir(&state).expect("state");
        fs::create_dir(&external).expect("external");
        fs::create_dir_all(rootfs.join("run/secrets")).expect("destination parent");
        let source = external.join("credential");
        private_file(&source);
        private_file(&rootfs.join("run/secrets/credential"));

        let verified = verify_sources(&[mount(&source, "/run/secrets/credential")], &state)
            .expect("verified source");
        verify_all_sources(&verified).expect("unchanged source");
        let destinations =
            DestinationFileGuard::prepare(&rootfs, &verified).expect("destination guard");
        destinations
            .verify_unchanged()
            .expect("unchanged destination");
    }

    #[test]
    fn rejects_symlink_non_private_oversized_and_state_sources() {
        let workspace = tempfile::tempdir().expect("workspace");
        let state = workspace.path().join("state");
        let external = workspace.path().join("external");
        fs::create_dir(&state).expect("state");
        fs::create_dir(&external).expect("external");

        let target = external.join("target");
        private_file(&target);
        let link = external.join("link");
        symlink(&target, &link).expect("symlink");
        assert!(verify_sources(&[mount(&link, "/secret")], &state).is_err());

        let exposed = external.join("exposed");
        private_file(&exposed);
        fs::set_permissions(&exposed, fs::Permissions::from_mode(0o640)).expect("permissions");
        assert!(verify_sources(&[mount(&exposed, "/secret")], &state).is_err());

        let oversized = external.join("oversized");
        private_file(&oversized);
        OpenOptions::new()
            .write(true)
            .open(&oversized)
            .expect("oversized file")
            .set_len(MAX_FILE_BYTES + 1)
            .expect("oversized length");
        assert!(verify_sources(&[mount(&oversized, "/secret")], &state).is_err());

        let internal = state.join("credential");
        private_file(&internal);
        assert!(verify_sources(&[mount(&internal, "/secret")], &state).is_err());
    }

    #[test]
    fn detects_source_and_destination_identity_changes() {
        let workspace = tempfile::tempdir().expect("workspace");
        let state = workspace.path().join("state");
        let external = workspace.path().join("external");
        let rootfs = workspace.path().join("rootfs");
        fs::create_dir(&state).expect("state");
        fs::create_dir(&external).expect("external");
        fs::create_dir(&rootfs).expect("rootfs");
        let source = external.join("credential");
        let destination = rootfs.join("credential");
        private_file(&source);
        private_file(&destination);
        let verified =
            verify_sources(&[mount(&source, "/credential")], &state).expect("verified source");
        let destinations =
            DestinationFileGuard::prepare(&rootfs, &verified).expect("destination guard");

        fs::remove_file(&source).expect("remove source");
        private_file(&source);
        assert!(verify_all_sources(&verified).is_err());

        fs::remove_file(&destination).expect("remove destination");
        private_file(&destination);
        assert!(destinations.verify_unchanged().is_err());
    }

    #[test]
    fn rejects_missing_or_non_regular_destination() {
        let workspace = tempfile::tempdir().expect("workspace");
        let state = workspace.path().join("state");
        let external = workspace.path().join("external");
        let rootfs = workspace.path().join("rootfs");
        fs::create_dir(&state).expect("state");
        fs::create_dir(&external).expect("external");
        fs::create_dir(&rootfs).expect("rootfs");
        let source = external.join("credential");
        private_file(&source);

        let missing =
            verify_sources(&[mount(&source, "/missing")], &state).expect("verified source");
        assert!(DestinationFileGuard::prepare(&rootfs, &missing).is_err());

        fs::create_dir(rootfs.join("directory")).expect("directory destination");
        let directory =
            verify_sources(&[mount(&source, "/directory")], &state).expect("verified source");
        assert!(DestinationFileGuard::prepare(&rootfs, &directory).is_err());
    }
}
