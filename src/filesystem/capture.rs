use std::collections::BTreeMap;
use std::ffi::{CStr, OsStr};
use std::fs::File;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;

use anyhow::{Context, Result, bail};
use rustix::fs::{
    AtFlags, Dir, FileType, Mode, OFlags, Stat, fgetxattr, flistxattr, fstat, lgetxattr,
    llistxattr, major, minor, open, openat, readlinkat, statat,
};

use crate::changeset::ContentStore;

use super::inventory::{Timestamp, Xattrs};
use super::{EntryKind, FilesystemOwnership, FsEntry, FsPath, Inventory, Metadata};

#[derive(Debug, Clone, Copy)]
pub(crate) struct CaptureLimits {
    pub(crate) entries: u64,
    pub(crate) path_bytes: u64,
    pub(crate) total_path_bytes: u64,
    pub(crate) xattr_names_bytes: usize,
    pub(crate) xattr_value_bytes: usize,
    pub(crate) total_xattr_bytes: u64,
    pub(crate) total_content_bytes: u64,
    pub(crate) depth: u64,
}

impl Default for CaptureLimits {
    fn default() -> Self {
        Self {
            entries: 1_000_000,
            path_bytes: 16 * 1024,
            total_path_bytes: 1024 * 1024 * 1024,
            xattr_names_bytes: 1024 * 1024,
            xattr_value_bytes: 16 * 1024 * 1024,
            total_xattr_bytes: 1024 * 1024 * 1024,
            total_content_bytes: 64 * 1024 * 1024 * 1024,
            depth: 1024,
        }
    }
}

#[derive(Debug)]
struct CaptureBudget {
    limits: CaptureLimits,
    entries: u64,
    path_bytes: u64,
    xattr_bytes: u64,
    content_bytes: u64,
}

impl CaptureBudget {
    fn new(limits: CaptureLimits) -> Self {
        Self {
            limits,
            entries: 0,
            path_bytes: 0,
            xattr_bytes: 0,
            content_bytes: 0,
        }
    }

    fn reserve_entry(&mut self, parent: Option<&FsPath>, name: &[u8], depth: u64) -> Result<()> {
        if depth > self.limits.depth {
            bail!(
                "filesystem capture depth limit exceeded: limit {}, observed {depth}",
                self.limits.depth
            );
        }
        self.entries = checked_total(self.entries, 1, self.limits.entries, "entry count")?;
        let separator = u64::from(parent.is_some_and(|path| !path.is_root()));
        let path_bytes = parent
            .map_or(0, |path| usize_to_u64(path.as_bytes().len()))
            .checked_add(separator)
            .and_then(|value| value.checked_add(usize_to_u64(name.len())))
            .context("filesystem path byte count overflow")?;
        if path_bytes > self.limits.path_bytes {
            bail!(
                "filesystem path exceeds capture limit: limit {}, observed {path_bytes}",
                self.limits.path_bytes
            );
        }
        self.path_bytes = checked_total(
            self.path_bytes,
            path_bytes,
            self.limits.total_path_bytes,
            "path byte count",
        )?;
        Ok(())
    }

    fn reserve_xattrs(&mut self, bytes: usize) -> Result<()> {
        self.xattr_bytes = checked_total(
            self.xattr_bytes,
            usize_to_u64(bytes),
            self.limits.total_xattr_bytes,
            "xattr byte count",
        )?;
        Ok(())
    }

    fn reserve_content(&mut self, bytes: u64) -> Result<()> {
        self.content_bytes = checked_total(
            self.content_bytes,
            bytes,
            self.limits.total_content_bytes,
            "content byte count",
        )?;
        Ok(())
    }
}

struct CaptureState {
    inventory: Inventory,
    contents: ContentStore,
    hardlinks: BTreeMap<(u128, u128), Vec<FsPath>>,
    budget: CaptureBudget,
}

struct EntryCapture<'a> {
    name: &'a CStr,
    raw_name: &'a [u8],
    path: &'a FsPath,
    depth: u64,
    initial: &'a Stat,
}

#[derive(Debug)]
pub(crate) struct CapturedTree {
    pub(crate) inventory: Inventory,
    pub(crate) contents: ContentStore,
}

#[derive(Debug, Default)]
pub(crate) struct TreeCapture {
    limits: CaptureLimits,
    ownership: FilesystemOwnership,
}

impl TreeCapture {
    #[cfg(target_os = "linux")]
    pub(crate) fn with_ownership(ownership: FilesystemOwnership) -> Self {
        Self {
            limits: CaptureLimits::default(),
            ownership,
        }
    }

    #[cfg(test)]
    pub(crate) fn capture(&self, root: &Path) -> Result<CapturedTree> {
        let root_fd = open_root(root)?;
        self.capture_fd_with(&root_fd, ContentStore::new)
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn capture_in(&self, root: &Path, content_parent: &Path) -> Result<CapturedTree> {
        let root_fd = open_root(root)?;
        self.capture_fd_with(&root_fd, || ContentStore::new_in(content_parent))
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn capture_inventory(&self, root: &Path) -> Result<Inventory> {
        let root_fd = open_root(root)?;
        self.capture_fd_with(&root_fd, || Ok(ContentStore::digest_only()))
            .map(|captured| captured.inventory)
    }

    fn capture_fd_with(
        &self,
        root: &OwnedFd,
        mut new_content_store: impl FnMut() -> Result<ContentStore>,
    ) -> Result<CapturedTree> {
        let first = self.capture_pass(&reopen_directory(root)?, new_content_store()?)?;
        let second = self.capture_pass(&reopen_directory(root)?, new_content_store()?)?;
        if first.inventory != second.inventory {
            bail!("filesystem changed between complete capture passes");
        }
        Ok(second)
    }

    fn capture_pass(&self, root: &OwnedFd, contents: ContentStore) -> Result<CapturedTree> {
        let mut state = CaptureState {
            inventory: Inventory::default(),
            contents,
            hardlinks: BTreeMap::new(),
            budget: CaptureBudget::new(self.limits),
        };
        let initial = fstat(root).context("failed to stat filesystem root")?;
        let initial_xattrs = read_fd_xattrs(root, self.limits, Some(&mut state.budget))?;
        self.walk_directory(root, None, 0, &mut state)?;
        let final_stat = fstat(root).context("failed to restat filesystem root")?;
        let final_xattrs = read_fd_xattrs(root, self.limits, None)?;
        ensure_stable(&initial, &final_stat, &initial_xattrs, &final_xattrs, "/")?;
        state
            .inventory
            .set_root(metadata(&initial, initial_xattrs, self.ownership)?)?;
        normalize_hardlinks(&mut state.inventory, state.hardlinks)?;
        Ok(CapturedTree {
            inventory: state.inventory,
            contents: state.contents,
        })
    }

    fn walk_directory(
        &self,
        directory: &OwnedFd,
        parent_path: Option<&FsPath>,
        depth: u64,
        state: &mut CaptureState,
    ) -> Result<()> {
        let names = Self::directory_names(directory, parent_path, depth, &mut state.budget)?;
        for name in names {
            let path = match parent_path {
                Some(parent) => parent.join_component(&name, self.limits.path_bytes)?,
                None => FsPath::from_relative(&name, self.limits.path_bytes)?,
            };
            let nul_name = nul_terminated(&name);
            let c_name = CStr::from_bytes_with_nul(&nul_name)
                .context("directory entry name contains NUL")?;
            let initial = statat(directory, c_name, AtFlags::SYMLINK_NOFOLLOW)
                .with_context(|| format!("failed to stat {}", path.display()))?;
            let source = EntryCapture {
                name: c_name,
                raw_name: &name,
                path: &path,
                depth: depth + 1,
                initial: &initial,
            };
            let entry = self.capture_entry(directory, &source, state)?;
            state.inventory.insert(path, entry)?;
        }
        Ok(())
    }

    fn directory_names(
        directory: &OwnedFd,
        parent_path: Option<&FsPath>,
        depth: u64,
        budget: &mut CaptureBudget,
    ) -> Result<Vec<Vec<u8>>> {
        let entry_depth = depth
            .checked_add(1)
            .context("filesystem capture depth overflow")?;
        let mut names = Vec::new();
        for entry in Dir::read_from(directory).context("failed to open directory stream")? {
            let entry = entry.context("failed to read directory entry")?;
            let name = entry.file_name().to_bytes();
            if name == b"." || name == b".." {
                continue;
            }
            budget.reserve_entry(parent_path, name, entry_depth)?;
            names.push(name.to_vec());
        }
        names.sort();
        Ok(names)
    }

    fn capture_entry(
        &self,
        directory: &OwnedFd,
        source: &EntryCapture<'_>,
        state: &mut CaptureState,
    ) -> Result<FsEntry> {
        match FileType::from_raw_mode(source.initial.st_mode) {
            FileType::RegularFile => self.capture_regular(directory, source, state),
            FileType::Directory => self.capture_directory(directory, source, state),
            FileType::Symlink => self.capture_symlink(directory, source, state),
            kind @ (FileType::Fifo | FileType::CharacterDevice | FileType::BlockDevice) => {
                self.capture_special(directory, source, kind, state)
            }
            FileType::Socket | FileType::Unknown => {
                bail!("unsupported filesystem object at {}", source.path.display())
            }
        }
    }

    fn capture_regular(
        &self,
        directory: &OwnedFd,
        source: &EntryCapture<'_>,
        state: &mut CaptureState,
    ) -> Result<FsEntry> {
        let fd = openat(
            directory,
            source.name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .with_context(|| format!("failed to open {}", source.path.display()))?;
        let opened = fstat(&fd)?;
        ensure_same_object(source.initial, &opened, source.path)?;
        let initial_xattrs = read_fd_xattrs(&fd, self.limits, Some(&mut state.budget))?;
        let declared_size = u64::try_from(opened.st_size)
            .with_context(|| format!("negative regular-file size at {}", source.path.display()))?;
        state.budget.reserve_content(declared_size)?;
        let mut file = File::from(fd);
        let (digest, size) = state.contents.put_reader(&mut file)?;
        let final_stat = fstat(&file)?;
        let final_xattrs = read_fd_xattrs(&file, self.limits, None)?;
        ensure_stable(
            &opened,
            &final_stat,
            &initial_xattrs,
            &final_xattrs,
            &source.path.display(),
        )?;
        if size != declared_size {
            bail!(
                "regular file changed while capturing {}",
                source.path.display()
            );
        }
        if opened.st_nlink > 1 {
            #[cfg(target_os = "linux")]
            let identity = stat_identity(&opened);
            #[cfg(not(target_os = "linux"))]
            let identity = stat_identity(&opened)?;
            state
                .hardlinks
                .entry(identity)
                .or_default()
                .push(source.path.clone());
        }
        Ok(FsEntry {
            metadata: metadata(&opened, initial_xattrs, self.ownership)?,
            kind: EntryKind::Regular {
                digest,
                size,
                hardlink: None,
            },
        })
    }

    fn capture_directory(
        &self,
        directory: &OwnedFd,
        source: &EntryCapture<'_>,
        state: &mut CaptureState,
    ) -> Result<FsEntry> {
        let fd = openat(
            directory,
            source.name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .with_context(|| format!("failed to open {}", source.path.display()))?;
        let opened = fstat(&fd)?;
        ensure_same_object(source.initial, &opened, source.path)?;
        let initial_xattrs = read_fd_xattrs(&fd, self.limits, Some(&mut state.budget))?;
        self.walk_directory(&fd, Some(source.path), source.depth, state)?;
        let final_stat = fstat(&fd)?;
        let final_xattrs = read_fd_xattrs(&fd, self.limits, None)?;
        ensure_stable(
            &opened,
            &final_stat,
            &initial_xattrs,
            &final_xattrs,
            &source.path.display(),
        )?;
        Ok(FsEntry {
            metadata: metadata(&opened, initial_xattrs, self.ownership)?,
            kind: EntryKind::Directory,
        })
    }

    fn capture_symlink(
        &self,
        directory: &OwnedFd,
        source: &EntryCapture<'_>,
        state: &mut CaptureState,
    ) -> Result<FsEntry> {
        let initial_xattrs = read_path_xattrs(
            directory,
            source.raw_name,
            self.limits,
            Some(&mut state.budget),
        )?;
        let target = readlinkat(directory, source.name, Vec::new())?.into_bytes();
        let final_stat = statat(directory, source.name, AtFlags::SYMLINK_NOFOLLOW)?;
        let final_xattrs = read_path_xattrs(directory, source.raw_name, self.limits, None)?;
        ensure_stable(
            source.initial,
            &final_stat,
            &initial_xattrs,
            &final_xattrs,
            &source.path.display(),
        )?;
        if readlinkat(directory, source.name, Vec::new())?.into_bytes() != target {
            bail!("symlink changed while capturing {}", source.path.display());
        }
        Ok(FsEntry {
            metadata: metadata(source.initial, initial_xattrs, self.ownership)?,
            kind: EntryKind::Symlink {
                target: target.into_boxed_slice(),
            },
        })
    }

    fn capture_special(
        &self,
        directory: &OwnedFd,
        source: &EntryCapture<'_>,
        kind: FileType,
        state: &mut CaptureState,
    ) -> Result<FsEntry> {
        let initial_xattrs = read_path_xattrs(
            directory,
            source.raw_name,
            self.limits,
            Some(&mut state.budget),
        )?;
        let final_stat = statat(directory, source.name, AtFlags::SYMLINK_NOFOLLOW)?;
        let final_xattrs = read_path_xattrs(directory, source.raw_name, self.limits, None)?;
        ensure_stable(
            source.initial,
            &final_stat,
            &initial_xattrs,
            &final_xattrs,
            &source.path.display(),
        )?;
        let kind = match kind {
            FileType::Fifo => EntryKind::Fifo,
            FileType::CharacterDevice | FileType::BlockDevice if self.ownership.is_single_id() => {
                bail!(
                    "rootless native execution does not support device nodes at {}",
                    source.path.display()
                )
            }
            FileType::CharacterDevice => EntryKind::Character {
                major: major(source.initial.st_rdev),
                minor: minor(source.initial.st_rdev),
            },
            FileType::BlockDevice => EntryKind::Block {
                major: major(source.initial.st_rdev),
                minor: minor(source.initial.st_rdev),
            },
            _ => unreachable!(),
        };
        Ok(FsEntry {
            metadata: metadata(source.initial, initial_xattrs, self.ownership)?,
            kind,
        })
    }
}

fn open_root(root: &Path) -> Result<OwnedFd> {
    open(
        root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .with_context(|| format!("failed to open filesystem root {}", root.display()))
}

fn reopen_directory(directory: &OwnedFd) -> Result<OwnedFd> {
    openat(
        directory,
        c".",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .context("failed to reopen filesystem root")
}

fn metadata(stat: &Stat, xattrs: Xattrs, ownership: FilesystemOwnership) -> Result<Metadata> {
    ownership.validate_xattrs(&xattrs)?;
    let (uid, gid) = ownership.logical_ids(stat.st_uid, stat.st_gid)?;
    Ok(Metadata {
        mode: permission_mode(stat),
        uid,
        gid,
        mtime: Timestamp {
            seconds: stat.st_mtime,
            nanos: u32::try_from(stat.st_mtime_nsec).context("mtime nanoseconds overflow")?,
        },
        xattrs,
    })
}

#[cfg(target_os = "linux")]
const fn permission_mode(stat: &Stat) -> u32 {
    stat.st_mode & 0o7777
}

#[cfg(not(target_os = "linux"))]
fn permission_mode(stat: &Stat) -> u32 {
    u32::from(stat.st_mode & 0o7777)
}

fn ensure_same_object(initial: &Stat, opened: &Stat, path: &FsPath) -> Result<()> {
    if initial.st_dev != opened.st_dev
        || initial.st_ino != opened.st_ino
        || FileType::from_raw_mode(initial.st_mode) != FileType::from_raw_mode(opened.st_mode)
    {
        bail!("filesystem object changed while opening {}", path.display());
    }
    Ok(())
}

fn ensure_stable(
    initial: &Stat,
    final_stat: &Stat,
    initial_xattrs: &Xattrs,
    final_xattrs: &Xattrs,
    path: &str,
) -> Result<()> {
    if initial.st_dev != final_stat.st_dev
        || initial.st_ino != final_stat.st_ino
        || initial.st_mode != final_stat.st_mode
        || initial.st_nlink != final_stat.st_nlink
        || initial.st_uid != final_stat.st_uid
        || initial.st_gid != final_stat.st_gid
        || initial.st_rdev != final_stat.st_rdev
        || initial.st_size != final_stat.st_size
        || initial.st_mtime != final_stat.st_mtime
        || initial.st_mtime_nsec != final_stat.st_mtime_nsec
        || initial.st_ctime != final_stat.st_ctime
        || initial.st_ctime_nsec != final_stat.st_ctime_nsec
        || initial_xattrs != final_xattrs
    {
        bail!("filesystem object changed while capturing {path}");
    }
    Ok(())
}

fn read_fd_xattrs(
    fd: impl AsFd,
    limits: CaptureLimits,
    budget: Option<&mut CaptureBudget>,
) -> Result<Xattrs> {
    let mut empty = [0_u8; 0];
    let required = flistxattr(&fd, &mut empty)?;
    if required > limits.xattr_names_bytes {
        bail!("filesystem xattr name list exceeds capture limit");
    }
    let mut budget = budget;
    if let Some(budget) = &mut budget {
        budget.reserve_xattrs(required)?;
    }
    let mut names = vec![0_u8; required];
    let read = flistxattr(&fd, &mut names)?;
    names.truncate(read);
    read_xattr_values(&names, limits, budget, |name, buffer| {
        fgetxattr(&fd, name, buffer).map_err(Into::into)
    })
}

fn read_path_xattrs(
    parent: &OwnedFd,
    name: &[u8],
    limits: CaptureLimits,
    budget: Option<&mut CaptureBudget>,
) -> Result<Xattrs> {
    let mut proc_path = format!("/proc/self/fd/{}/", parent.as_raw_fd()).into_bytes();
    proc_path.extend_from_slice(name);
    let path = Path::new(OsStr::from_bytes(&proc_path));
    let mut empty = [0_u8; 0];
    let required = llistxattr(path, &mut empty)?;
    if required > limits.xattr_names_bytes {
        bail!("filesystem xattr name list exceeds capture limit");
    }
    let mut budget = budget;
    if let Some(budget) = &mut budget {
        budget.reserve_xattrs(required)?;
    }
    let mut names = vec![0_u8; required];
    let read = llistxattr(path, &mut names)?;
    names.truncate(read);
    read_xattr_values(&names, limits, budget, |name, buffer| {
        lgetxattr(path, name, buffer).map_err(Into::into)
    })
}

fn read_xattr_values(
    names: &[u8],
    limits: CaptureLimits,
    mut budget: Option<&mut CaptureBudget>,
    mut get: impl FnMut(&[u8], &mut [u8]) -> Result<usize>,
) -> Result<Xattrs> {
    if !names.is_empty() && !names.ends_with(&[0]) {
        bail!("filesystem returned a malformed xattr name list");
    }
    let mut result = BTreeMap::new();
    for name in names
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
    {
        let required = get(name, &mut [])?;
        if required > limits.xattr_value_bytes {
            bail!("filesystem xattr value exceeds capture limit");
        }
        if let Some(budget) = &mut budget {
            budget.reserve_xattrs(required)?;
        }
        let mut value = vec![0_u8; required];
        let read = get(name, &mut value)?;
        value.truncate(read);
        if result
            .insert(name.into(), value.into_boxed_slice())
            .is_some()
        {
            bail!("filesystem returned a duplicate xattr name");
        }
    }
    Ok(result)
}

fn nul_terminated(bytes: &[u8]) -> Vec<u8> {
    bytes.iter().copied().chain(std::iter::once(0)).collect()
}

fn checked_total(current: u64, added: u64, limit: u64, name: &str) -> Result<u64> {
    let observed = current
        .checked_add(added)
        .with_context(|| format!("filesystem capture {name} overflow"))?;
    if observed > limit {
        bail!("filesystem capture {name} limit exceeded: limit {limit}, observed {observed}");
    }
    Ok(observed)
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn normalize_hardlinks(
    inventory: &mut Inventory,
    groups: BTreeMap<(u128, u128), Vec<FsPath>>,
) -> Result<()> {
    for mut paths in groups.into_values() {
        if paths.len() < 2 {
            continue;
        }
        paths.sort();
        let anchor = paths[0].clone();
        let anchor_entry = inventory
            .get(&anchor)
            .with_context(|| format!("hardlink path disappeared: {}", anchor.display()))?
            .clone();
        if !matches!(
            &anchor_entry.kind,
            EntryKind::Regular { hardlink: None, .. }
        ) {
            bail!(
                "hardlink anchor is not a regular file: {}",
                anchor.display()
            );
        }
        for path in &paths[1..] {
            let entry = inventory
                .get(path)
                .with_context(|| format!("hardlink path disappeared: {}", path.display()))?;
            if entry != &anchor_entry {
                bail!(
                    "hardlink group changed while capturing {} and {}",
                    anchor.display(),
                    path.display()
                );
            }
        }
        for path in paths.into_iter().skip(1) {
            let entry = inventory
                .get_mut(&path)
                .with_context(|| format!("hardlink path disappeared: {}", path.display()))?;
            let EntryKind::Regular { hardlink, .. } = &mut entry.kind else {
                bail!("hardlink path is not a regular file: {}", path.display());
            };
            *hardlink = Some(anchor.clone());
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn stat_identity(stat: &Stat) -> (u128, u128) {
    (u128::from(stat.st_dev), u128::from(stat.st_ino))
}

#[cfg(not(target_os = "linux"))]
fn stat_identity(stat: &Stat) -> Result<(u128, u128)> {
    Ok((
        u128::try_from(stat.st_dev).context("filesystem device identity is negative")?,
        u128::from(stat.st_ino),
    ))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::core::Digest;

    #[test]
    fn captures_regular_directory_bytes_and_root_metadata() {
        let root = tempfile::tempdir().expect("root");
        fs::create_dir(root.path().join("nested")).expect("directory");
        fs::write(root.path().join("nested/value"), b"captured bytes").expect("file");
        let captured = TreeCapture::default()
            .capture(root.path())
            .expect("capture");
        assert!(captured.inventory.root().is_some());
        let path = FsPath::from_relative(b"nested/value", 1024).expect("path");
        let entry = captured.inventory.get(&path).expect("entry");
        let EntryKind::Regular { digest, size, .. } = &entry.kind else {
            panic!("expected regular file");
        };
        assert_eq!(*size, 14);
        assert_eq!(*digest, crate::integrity::digest_bytes(b"captured bytes"));
        let file = captured.contents.open(digest, *size).expect("content");
        assert_eq!(crate::oci::digest_reader(file).expect("digest").0, *digest);
    }

    #[test]
    fn content_budget_is_cumulative_across_the_tree() {
        let root = tempfile::tempdir().expect("root");
        fs::write(root.path().join("a"), b"aa").expect("a");
        fs::write(root.path().join("b"), b"bb").expect("b");
        let capture = TreeCapture {
            limits: CaptureLimits {
                total_content_bytes: 3,
                ..CaptureLimits::default()
            },
            ownership: FilesystemOwnership::Native,
        };
        let error = capture.capture(root.path()).expect_err("content limit");
        assert!(
            format!("{error:#}").contains("content byte count limit exceeded"),
            "{error:#}"
        );
    }

    #[test]
    fn entry_budget_is_consumed_while_reading_directory_names() {
        let root = tempfile::tempdir().expect("root");
        fs::write(root.path().join("a"), b"").expect("a");
        fs::write(root.path().join("b"), b"").expect("b");
        let capture = TreeCapture {
            limits: CaptureLimits {
                entries: 1,
                ..CaptureLimits::default()
            },
            ownership: FilesystemOwnership::Native,
        };
        let error = capture.capture(root.path()).expect_err("entry limit");
        assert!(
            format!("{error:#}").contains("entry count limit exceeded"),
            "{error:#}"
        );
    }

    #[test]
    fn path_budget_is_consumed_before_directory_names_are_stored() {
        let root = tempfile::tempdir().expect("root");
        fs::write(root.path().join("aa"), b"").expect("aa");
        fs::write(root.path().join("bb"), b"").expect("bb");
        let capture = TreeCapture {
            limits: CaptureLimits {
                total_path_bytes: 3,
                ..CaptureLimits::default()
            },
            ownership: FilesystemOwnership::Native,
        };
        let error = capture.capture(root.path()).expect_err("path limit");
        assert!(
            format!("{error:#}").contains("path byte count limit exceeded"),
            "{error:#}"
        );
    }

    #[test]
    fn depth_budget_rejects_a_descendant_before_recursing() {
        let root = tempfile::tempdir().expect("root");
        fs::create_dir(root.path().join("a")).expect("a");
        fs::create_dir(root.path().join("a/b")).expect("b");
        let capture = TreeCapture {
            limits: CaptureLimits {
                depth: 1,
                ..CaptureLimits::default()
            },
            ownership: FilesystemOwnership::Native,
        };
        let error = capture.capture(root.path()).expect_err("depth limit");
        assert!(
            format!("{error:#}").contains("depth limit exceeded"),
            "{error:#}"
        );
    }

    #[test]
    fn xattr_budget_accumulates_between_entries() {
        let limits = CaptureLimits {
            total_xattr_bytes: 3,
            ..CaptureLimits::default()
        };
        let mut budget = CaptureBudget::new(limits);
        budget.reserve_xattrs(2).expect("first xattr");
        let error = budget.reserve_xattrs(2).expect_err("xattr limit");
        assert!(
            format!("{error:#}").contains("xattr byte count limit exceeded"),
            "{error:#}"
        );
    }

    #[test]
    fn contradictory_hardlink_members_fail_before_normalization() {
        let first = FsPath::from_relative(b"a", 1024).expect("a");
        let second = FsPath::from_relative(b"b", 1024).expect("b");
        let mut inventory = Inventory::default();
        inventory
            .insert(first.clone(), regular_entry('1'))
            .expect("first");
        inventory
            .insert(second.clone(), regular_entry('2'))
            .expect("second");
        let groups = BTreeMap::from([((1, 2), vec![first, second])]);
        let error = normalize_hardlinks(&mut inventory, groups).expect_err("hardlink mismatch");
        assert!(
            format!("{error:#}").contains("hardlink group changed while capturing"),
            "{error:#}"
        );
    }

    fn regular_entry(digit: char) -> FsEntry {
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
                digest: Digest::parse(format!("sha256:{}", digit.to_string().repeat(64)))
                    .expect("digest"),
                size: 1,
                hardlink: None,
            },
        }
    }
}
