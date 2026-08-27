use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fmt;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use oci_spec::image::Digest;
use rustix::fs::{AtFlags, FileType, Mode, OFlags, openat, statat, unlinkat};
use rustix::io::Errno;

use super::{RootfsError, RootfsErrorKind, RootfsLimits};

type Xattrs = BTreeMap<Box<[u8]>, Box<[u8]>>;

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct FsPath(Box<[u8]>);

impl FsPath {
    fn from_relative(raw: &[u8], limit: u64) -> Result<Self> {
        let observed = normalized_relative_len(raw)?;
        enforce("path bytes", limit, observed)?;
        let mut normalized = Vec::with_capacity(usize::try_from(observed)?);
        for component in raw.split(|byte| *byte == b'/') {
            if component.is_empty() || component == b"." {
                continue;
            }
            if !normalized.is_empty() {
                normalized.push(b'/');
            }
            normalized.extend_from_slice(component);
        }
        Ok(Self(normalized.into_boxed_slice()))
    }

    fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    fn components(&self) -> impl Iterator<Item = &[u8]> {
        self.0
            .split(|byte| *byte == b'/')
            .filter(|part| !part.is_empty())
    }

    fn parent(&self) -> Self {
        self.0.iter().rposition(|byte| *byte == b'/').map_or_else(
            || Self(Box::default()),
            |split| Self(self.0[..split].into()),
        )
    }

    fn basename(&self) -> &[u8] {
        self.0
            .iter()
            .rposition(|byte| *byte == b'/')
            .map_or(&self.0, |split| &self.0[split + 1..])
    }

    fn join(&self, component: &[u8], limit: u64) -> Result<Self> {
        if component.is_empty()
            || component == b"."
            || component == b".."
            || component.contains(&b'/')
            || component.contains(&0)
        {
            bail!("unsafe filesystem component: {}", display_bytes(component));
        }
        let mut bytes = self.0.to_vec();
        if !bytes.is_empty() {
            bytes.push(b'/');
        }
        bytes.extend_from_slice(component);
        Self::from_relative(&bytes, limit)
    }

    fn is_descendant_of(&self, ancestor: &Self) -> bool {
        if ancestor.is_root() {
            return !self.is_root();
        }
        self.0.len() > ancestor.0.len()
            && self.0.starts_with(&ancestor.0)
            && self.0[ancestor.0.len()] == b'/'
    }

    fn display(&self) -> String {
        format!("/{}", display_bytes(&self.0))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Timestamp {
    seconds: i64,
    nanos: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Metadata {
    mode: u32,
    uid: u32,
    gid: u32,
    mtime: Timestamp,
    xattrs: Xattrs,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum EntryKind {
    Regular {
        digest: Digest,
        size: u64,
        hardlink: Option<FsPath>,
    },
    Directory,
    Symlink(Box<[u8]>),
    Fifo,
    Character {
        major: u32,
        minor: u32,
    },
    Block {
        major: u32,
        minor: u32,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FsEntry {
    metadata: Metadata,
    kind: EntryKind,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct Inventory {
    root: Option<Metadata>,
    entries: BTreeMap<FsPath, FsEntry>,
}

/// A materialized rootfs and the immutable logical snapshot it came from.
///
/// This object never recursively removes `workspace` or `rootfs`.  The
/// execution engine owns the ordered unmount-and-cleanup boundary.
#[derive(Debug)]
pub(crate) struct Rootfs {
    workspace: PathBuf,
    root_path: PathBuf,
    root: OwnedFd,
    initial: Inventory,
    limits: RootfsLimits,
}

impl Rootfs {
    pub(crate) fn path(&self) -> &Path {
        &self.root_path
    }

    /// Fails unless `/proc/self/mountinfo` proves no mount remains below rootfs.
    pub(crate) fn ensure_no_mounts(&self) -> Result<()> {
        mountinfo::ensure_no_mounts(&self.root)
    }

    /// Removes one empty Engine-created mountpoint without resolving any
    /// component outside the retained rootfs descriptor.
    pub(crate) fn remove_mount_artifact(&self, relative: &Path) -> Result<()> {
        let raw = relative.as_os_str().as_bytes();
        enforce(
            "runtime-created mount artifact path bytes",
            self.limits.path_bytes,
            usize_to_u64(raw.len()),
        )
        .context("rootfs instability: mount artifact path exceeded its verified bound")?;
        let path = FsPath::from_relative(raw, self.limits.path_bytes)
            .context("rootfs instability: invalid runtime-created mount artifact path")?;
        if path.is_root() || path.as_bytes() != raw {
            bail!(
                "rootfs instability: runtime-created mount artifact must be a normalized non-root relative path: {}",
                display_bytes(raw)
            );
        }

        let Some(parent) = apply::open_parent_existing(&self.root, &path).with_context(|| {
            format!(
                "rootfs instability: could not traverse mount artifact {} without following symlinks",
                path.display()
            )
        })?
        else {
            return Ok(());
        };
        let name = os(path.basename());
        let stat = match statat(&parent, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => stat,
            Err(Errno::NOENT) => return Ok(()),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "rootfs instability: could not inspect mount artifact {}",
                        path.display()
                    )
                });
            }
        };
        let flags = match FileType::from_raw_mode(stat.st_mode) {
            FileType::Directory => AtFlags::REMOVEDIR,
            FileType::RegularFile if stat.st_size == 0 => AtFlags::empty(),
            _ => {
                bail!(
                    "rootfs instability: runtime-created mount artifact {} is not an empty file or directory",
                    path.display()
                );
            }
        };
        unlinkat(&parent, name, flags).with_context(|| {
            format!(
                "rootfs instability: runtime-created mount artifact {} was not proved empty and removed",
                path.display()
            )
        })?;
        Ok(())
    }
}

fn reopen_directory(directory: &OwnedFd) -> Result<OwnedFd> {
    Ok(openat(
        directory,
        c".",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?)
}

fn proc_path(parent: &OwnedFd, name: &[u8]) -> PathBuf {
    let mut bytes = format!("/proc/self/fd/{}/", parent.as_raw_fd()).into_bytes();
    bytes.extend_from_slice(name);
    PathBuf::from(OsStr::from_bytes(&bytes))
}

fn c_name(bytes: &[u8]) -> Result<std::ffi::CString> {
    std::ffi::CString::new(bytes).context("filesystem name contains NUL")
}

fn os(bytes: &[u8]) -> &OsStr {
    OsStr::from_bytes(bytes)
}

fn path_buf(path: &FsPath) -> PathBuf {
    PathBuf::from(os(path.as_bytes()))
}

fn default_directory() -> Metadata {
    Metadata {
        mode: 0o755,
        uid: 0,
        gid: 0,
        mtime: Timestamp {
            seconds: 0,
            nanos: 0,
        },
        xattrs: Xattrs::new(),
    }
}

fn enforce(noun: &str, limit: u64, observed: u64) -> Result<()> {
    if observed > limit {
        return Err(classified_anyhow(
            RootfsErrorKind::UnsupportedInput,
            anyhow!("{noun} limit exceeded: limit {limit}, observed {observed}"),
        ));
    }
    Ok(())
}

fn checked_total(current: u64, added: u64, limit: u64, noun: &str) -> Result<u64> {
    let observed = current.checked_add(added).ok_or_else(|| {
        classified_anyhow(
            RootfsErrorKind::UnsupportedInput,
            anyhow!("{noun} overflow"),
        )
    })?;
    enforce(noun, limit, observed)?;
    Ok(observed)
}

fn classified_anyhow(kind: RootfsErrorKind, source: anyhow::Error) -> anyhow::Error {
    RootfsError::new(kind, source).into()
}

fn invalid_input(message: impl fmt::Display) -> anyhow::Error {
    classified_anyhow(RootfsErrorKind::InvalidInput, anyhow!(message.to_string()))
}

fn unsupported_input(message: impl fmt::Display) -> anyhow::Error {
    classified_anyhow(
        RootfsErrorKind::UnsupportedInput,
        anyhow!(message.to_string()),
    )
}

fn internal_error(source: impl Into<anyhow::Error>) -> anyhow::Error {
    classified_anyhow(RootfsErrorKind::Internal, source.into())
}

fn classify_io_error(error: std::io::Error, fallback: RootfsErrorKind) -> std::io::Error {
    if error_chain_has_rootfs_error(&error) {
        error
    } else {
        std::io::Error::new(error.kind(), RootfsError::new(fallback, error.into()))
    }
}

fn error_chain_has_rootfs_error(error: &(dyn std::error::Error + 'static)) -> bool {
    rootfs_error_kind_in_chain(error).is_some()
}

fn rootfs_error_kind_in_chain(
    error: &(dyn std::error::Error + 'static),
) -> Option<RootfsErrorKind> {
    if let Some(classified) = error.downcast_ref::<RootfsError>() {
        return Some(classified.kind());
    }
    if let Some(io_error) = error.downcast_ref::<std::io::Error>()
        && let Some(inner) = io_error.get_ref()
        && let Some(kind) = rootfs_error_kind_in_chain(inner)
    {
        return Some(kind);
    }
    error.source().and_then(rootfs_error_kind_in_chain)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MaterializationFault {
    CompressedRead,
    DecodedRead,
    ApplySyscall,
}

#[cfg(test)]
thread_local! {
    static MATERIALIZATION_FAULT: std::cell::Cell<Option<MaterializationFault>> = const {
        std::cell::Cell::new(None)
    };
}

#[cfg(test)]
pub(crate) fn with_materialization_fault<T>(
    fault: MaterializationFault,
    operation: impl FnOnce() -> T,
) -> T {
    struct Reset(Option<MaterializationFault>);

    impl Drop for Reset {
        fn drop(&mut self) {
            MATERIALIZATION_FAULT.set(self.0);
        }
    }

    let previous = MATERIALIZATION_FAULT.replace(Some(fault));
    let _reset = Reset(previous);
    operation()
}

#[cfg(test)]
fn take_materialization_fault(fault: MaterializationFault) -> bool {
    MATERIALIZATION_FAULT.with(|active| {
        if active.get() == Some(fault) {
            active.set(None);
            true
        } else {
            false
        }
    })
}

#[cfg(not(test))]
const fn take_materialization_fault(_fault: MaterializationFault) -> bool {
    false
}

fn classify_materialization_error(source: anyhow::Error, fallback: RootfsErrorKind) -> RootfsError {
    let kind = rootfs_error_kind_in_chain(source.as_ref()).unwrap_or(fallback);
    RootfsError::new(kind, source)
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn normalized_relative_len(raw: &[u8]) -> Result<u64> {
    if raw.contains(&0) || raw.starts_with(b"/") {
        bail!("unsafe filesystem path: {}", display_bytes(raw));
    }
    let mut length = 0_u64;
    for component in raw.split(|byte| *byte == b'/') {
        if component.is_empty() || component == b"." {
            continue;
        }
        if component == b".." {
            bail!("unsafe filesystem path: {}", display_bytes(raw));
        }
        length = length
            .checked_add(u64::from(length != 0))
            .and_then(|bytes| bytes.checked_add(usize_to_u64(component.len())))
            .context("filesystem path length overflow")?;
    }
    Ok(length)
}

fn display_bytes(bytes: &[u8]) -> String {
    bytes
        .iter()
        .flat_map(|byte| std::ascii::escape_default(*byte))
        .map(char::from)
        .collect()
}

mod apply;
mod capture;
mod diff;
mod digest;
mod encode;
mod layer;
mod mountinfo;
mod plan;
mod preflight;
mod xattr;

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
