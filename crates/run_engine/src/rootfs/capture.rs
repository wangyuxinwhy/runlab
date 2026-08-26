use std::collections::BTreeMap;
use std::ffi::CStr;
use std::fs::File;
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use oci_spec::image::MediaType;
use rustix::fs::{
    AtFlags, Dir, FileType, Mode, OFlags, Stat, fgetxattr, fstat, lgetxattr, major, minor, openat,
    readlinkat, statat,
};

use super::super::CapturedLayer;
use super::diff::compare;
use super::digest::copy_and_digest;
use super::encode::encode_layer;
use super::xattr::{list_fd_xattr_names, list_path_xattr_names, split_xattr_names};
use super::{
    EntryKind, FsEntry, FsPath, Inventory, Metadata, Rootfs, RootfsLimits, Timestamp, Xattrs,
    c_name, checked_total, enforce, proc_path, reopen_directory, usize_to_u64,
};

impl Rootfs {
    /// Captures the complete stopped tree after proving all mounts are gone.
    pub(crate) fn capture(&self) -> Result<CapturedLayer> {
        self.ensure_no_mounts()?;
        let after = capture_stable(&self.root, &self.workspace, self.limits, true)?;
        self.ensure_no_mounts()?;
        let changes = compare(&self.initial, &after.inventory)?;
        let (path, size, diff_id) =
            encode_layer(&changes, &after.contents, &self.workspace, self.limits)?;
        Ok(CapturedLayer {
            media_type: MediaType::ImageLayer,
            path,
            size,
            diff_id,
        })
    }
}
#[derive(Default)]
pub(super) struct CapturedTree {
    pub(super) inventory: Inventory,
    contents: BTreeMap<String, tempfile::TempPath>,
}

pub(super) struct CaptureBudget {
    limits: RootfsLimits,
    entries: u64,
    path_bytes: u64,
    xattr_bytes: u64,
    content_bytes: u64,
}

impl CaptureBudget {
    fn new(limits: RootfsLimits) -> Self {
        Self {
            limits,
            entries: 0,
            path_bytes: 0,
            xattr_bytes: 0,
            content_bytes: 0,
        }
    }

    fn entry(&mut self, parent: Option<&FsPath>, name: &[u8], depth: u64) -> Result<()> {
        enforce("capture depth", self.limits.depth, depth)?;
        self.entries = checked_total(self.entries, 1, self.limits.entries, "capture entries")?;
        let separator = u64::from(parent.is_some_and(|path| !path.is_root()));
        let path_bytes = parent
            .map_or(0, |path| usize_to_u64(path.as_bytes().len()))
            .checked_add(separator)
            .and_then(|bytes| bytes.checked_add(usize_to_u64(name.len())))
            .context("capture path byte count overflow")?;
        enforce("capture path bytes", self.limits.path_bytes, path_bytes)?;
        self.path_bytes = checked_total(
            self.path_bytes,
            path_bytes,
            self.limits.total_path_bytes,
            "capture path bytes",
        )?;
        Ok(())
    }

    fn xattrs(&mut self, bytes: usize) -> Result<()> {
        self.xattr_bytes = checked_total(
            self.xattr_bytes,
            usize_to_u64(bytes),
            self.limits.total_xattr_bytes,
            "capture xattr bytes",
        )?;
        Ok(())
    }

    fn content(&mut self, bytes: u64) -> Result<()> {
        self.content_bytes = checked_total(
            self.content_bytes,
            bytes,
            self.limits.total_content_bytes,
            "capture content bytes",
        )?;
        Ok(())
    }
}

pub(super) struct CaptureState {
    tree: CapturedTree,
    hardlinks: BTreeMap<(u128, u128), Vec<FsPath>>,
    budget: CaptureBudget,
    keep_contents: bool,
    content_parent: PathBuf,
}

#[derive(Clone, Copy)]
struct EntryObservation<'a> {
    directory: &'a OwnedFd,
    name: &'a CStr,
    raw_name: &'a [u8],
    path: &'a FsPath,
    depth: u64,
    initial: &'a Stat,
}

pub(super) fn capture_stable(
    root: &OwnedFd,
    content_parent: &Path,
    limits: RootfsLimits,
    keep_contents: bool,
) -> Result<CapturedTree> {
    let first = capture_pass(root, content_parent, limits, false)?;
    let second = capture_pass(root, content_parent, limits, keep_contents)?;
    if first.inventory != second.inventory {
        bail!("filesystem changed between complete capture passes");
    }
    Ok(second)
}

pub(super) fn capture_pass(
    root: &OwnedFd,
    content_parent: &Path,
    limits: RootfsLimits,
    keep_contents: bool,
) -> Result<CapturedTree> {
    let mut state = CaptureState {
        tree: CapturedTree::default(),
        hardlinks: BTreeMap::new(),
        budget: CaptureBudget::new(limits),
        keep_contents,
        content_parent: content_parent.to_path_buf(),
    };
    let directory = reopen_directory(root)?;
    let initial = fstat(&directory)?;
    let initial_xattrs = read_fd_xattrs(&directory, limits, Some(&mut state.budget))?;
    walk_directory(&directory, None, 0, limits, &mut state)?;
    let final_stat = fstat(&directory)?;
    let final_xattrs = read_fd_xattrs(&directory, limits, None)?;
    ensure_stable(&initial, &final_stat, &initial_xattrs, &final_xattrs, "/")?;
    state.tree.inventory.root = Some(metadata(&initial, initial_xattrs)?);
    normalize_hardlinks(&mut state.tree.inventory, state.hardlinks)?;
    Ok(state.tree)
}

pub(super) fn walk_directory(
    directory: &OwnedFd,
    parent: Option<&FsPath>,
    depth: u64,
    limits: RootfsLimits,
    state: &mut CaptureState,
) -> Result<()> {
    let children = capture_directory_entries(directory, parent, depth, limits, &mut state.budget)?;
    for (name, path, child_depth) in children {
        let c_name = c_name(&name)?;
        let initial = statat(directory, &c_name, AtFlags::SYMLINK_NOFOLLOW)
            .with_context(|| format!("failed to stat {}", path.display()))?;
        let entry = capture_entry(
            EntryObservation {
                directory,
                name: &c_name,
                raw_name: &name,
                path: &path,
                depth: child_depth,
                initial: &initial,
            },
            limits,
            state,
        )?;
        if state
            .tree
            .inventory
            .entries
            .insert(path.clone(), entry)
            .is_some()
        {
            bail!("duplicate captured filesystem path: {}", path.display());
        }
    }
    Ok(())
}

pub(super) fn capture_directory_entries(
    directory: &OwnedFd,
    parent: Option<&FsPath>,
    depth: u64,
    limits: RootfsLimits,
    budget: &mut CaptureBudget,
) -> Result<Vec<(Vec<u8>, FsPath, u64)>> {
    let child_depth = depth.checked_add(1).context("capture depth overflow")?;
    let mut children = Vec::new();
    for result in Dir::read_from(directory)? {
        let entry = result?;
        let name = entry.file_name().to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        budget.entry(parent, name, child_depth)?;
        let path = parent.map_or_else(
            || FsPath::from_relative(name, limits.path_bytes),
            |parent| parent.join(name, limits.path_bytes),
        )?;
        children.push((name.to_vec(), path, child_depth));
    }
    children.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(children)
}

fn capture_entry(
    observation: EntryObservation<'_>,
    limits: RootfsLimits,
    state: &mut CaptureState,
) -> Result<FsEntry> {
    match FileType::from_raw_mode(observation.initial.st_mode) {
        FileType::RegularFile => capture_regular(
            observation.directory,
            observation.name,
            observation.path,
            observation.initial,
            limits,
            state,
        ),
        FileType::Directory => capture_directory(&observation, limits, state),
        FileType::Symlink => capture_symlink(
            observation.directory,
            observation.name,
            observation.raw_name,
            observation.path,
            observation.initial,
            limits,
            state,
        ),
        FileType::Fifo => capture_special(
            observation.directory,
            observation.raw_name,
            observation.path,
            observation.initial,
            limits,
            state,
            EntryKind::Fifo,
        ),
        FileType::CharacterDevice => capture_special(
            observation.directory,
            observation.raw_name,
            observation.path,
            observation.initial,
            limits,
            state,
            EntryKind::Character {
                major: major(observation.initial.st_rdev),
                minor: minor(observation.initial.st_rdev),
            },
        ),
        FileType::BlockDevice => capture_special(
            observation.directory,
            observation.raw_name,
            observation.path,
            observation.initial,
            limits,
            state,
            EntryKind::Block {
                major: major(observation.initial.st_rdev),
                minor: minor(observation.initial.st_rdev),
            },
        ),
        FileType::Socket | FileType::Unknown => {
            bail!(
                "unsupported filesystem object at {}",
                observation.path.display()
            )
        }
    }
}

pub(super) fn capture_regular(
    directory: &OwnedFd,
    name: &CStr,
    path: &FsPath,
    initial: &Stat,
    limits: RootfsLimits,
    state: &mut CaptureState,
) -> Result<FsEntry> {
    let fd = openat(
        directory,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let opened = fstat(&fd)?;
    ensure_same_object(initial, &opened, path)?;
    let initial_xattrs = read_fd_xattrs(&fd, limits, Some(&mut state.budget))?;
    let declared = u64::try_from(opened.st_size)
        .with_context(|| format!("negative regular size at {}", path.display()))?;
    state.budget.content(declared)?;
    let mut file = File::from(fd);
    let (digest, size) = if state.keep_contents {
        let mut staged = tempfile::NamedTempFile::new_in(&state.content_parent)?;
        let result = copy_and_digest(&mut file, staged.as_file_mut(), Some(declared))?;
        staged.as_file_mut().sync_all()?;
        state
            .tree
            .contents
            .entry(result.0.to_string())
            .or_insert_with(|| staged.into_temp_path());
        result
    } else {
        copy_and_digest(&mut file, std::io::sink(), Some(declared))?
    };
    let final_stat = fstat(&file)?;
    let final_xattrs = read_fd_xattrs(&file, limits, None)?;
    ensure_stable(
        &opened,
        &final_stat,
        &initial_xattrs,
        &final_xattrs,
        &path.display(),
    )?;
    if size != declared {
        bail!("regular file changed while capturing {}", path.display());
    }
    if opened.st_nlink > 1 {
        state
            .hardlinks
            .entry((u128::from(opened.st_dev), u128::from(opened.st_ino)))
            .or_default()
            .push(path.clone());
    }
    Ok(FsEntry {
        metadata: metadata(&opened, initial_xattrs)?,
        kind: EntryKind::Regular {
            digest,
            size,
            hardlink: None,
        },
    })
}

fn capture_directory(
    observation: &EntryObservation<'_>,
    limits: RootfsLimits,
    state: &mut CaptureState,
) -> Result<FsEntry> {
    let fd = openat(
        observation.directory,
        observation.name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let opened = fstat(&fd)?;
    ensure_same_object(observation.initial, &opened, observation.path)?;
    let initial_xattrs = read_fd_xattrs(&fd, limits, Some(&mut state.budget))?;
    walk_directory(
        &fd,
        Some(observation.path),
        observation.depth,
        limits,
        state,
    )?;
    let final_stat = fstat(&fd)?;
    let final_xattrs = read_fd_xattrs(&fd, limits, None)?;
    ensure_stable(
        &opened,
        &final_stat,
        &initial_xattrs,
        &final_xattrs,
        &observation.path.display(),
    )?;
    Ok(FsEntry {
        metadata: metadata(&opened, initial_xattrs)?,
        kind: EntryKind::Directory,
    })
}

pub(super) fn capture_symlink(
    directory: &OwnedFd,
    name: &CStr,
    raw_name: &[u8],
    path: &FsPath,
    initial: &Stat,
    limits: RootfsLimits,
    state: &mut CaptureState,
) -> Result<FsEntry> {
    let initial_xattrs = read_path_xattrs(directory, raw_name, limits, Some(&mut state.budget))?;
    let target = readlinkat(directory, name, Vec::new())?.into_bytes();
    enforce(
        "captured link target",
        limits.link_target_bytes,
        usize_to_u64(target.len()),
    )?;
    let final_stat = statat(directory, name, AtFlags::SYMLINK_NOFOLLOW)?;
    let final_xattrs = read_path_xattrs(directory, raw_name, limits, None)?;
    ensure_stable(
        initial,
        &final_stat,
        &initial_xattrs,
        &final_xattrs,
        &path.display(),
    )?;
    if readlinkat(directory, name, Vec::new())?.into_bytes() != target {
        bail!("symlink changed while capturing {}", path.display());
    }
    Ok(FsEntry {
        metadata: metadata(initial, initial_xattrs)?,
        kind: EntryKind::Symlink(target.into_boxed_slice()),
    })
}

pub(super) fn capture_special(
    directory: &OwnedFd,
    raw_name: &[u8],
    path: &FsPath,
    initial: &Stat,
    limits: RootfsLimits,
    state: &mut CaptureState,
    kind: EntryKind,
) -> Result<FsEntry> {
    let initial_xattrs = read_path_xattrs(directory, raw_name, limits, Some(&mut state.budget))?;
    let c_name = c_name(raw_name)?;
    let final_stat = statat(directory, &c_name, AtFlags::SYMLINK_NOFOLLOW)?;
    let final_xattrs = read_path_xattrs(directory, raw_name, limits, None)?;
    ensure_stable(
        initial,
        &final_stat,
        &initial_xattrs,
        &final_xattrs,
        &path.display(),
    )?;
    Ok(FsEntry {
        metadata: metadata(initial, initial_xattrs)?,
        kind,
    })
}

pub(super) fn normalize_hardlinks(
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
            .entries
            .get(&anchor)
            .with_context(|| format!("hardlink anchor disappeared: {}", anchor.display()))?
            .clone();
        for path in paths.into_iter().skip(1) {
            let entry = inventory
                .entries
                .get_mut(&path)
                .with_context(|| format!("hardlink member disappeared: {}", path.display()))?;
            if entry.metadata != anchor_entry.metadata {
                bail!(
                    "hardlink metadata disagrees: {} -> {}",
                    path.display(),
                    anchor.display()
                );
            }
            let (
                EntryKind::Regular {
                    digest,
                    size,
                    hardlink,
                },
                EntryKind::Regular {
                    digest: anchor_digest,
                    size: anchor_size,
                    ..
                },
            ) = (&mut entry.kind, &anchor_entry.kind)
            else {
                bail!("hardlink member is not regular: {}", path.display());
            };
            if digest != anchor_digest || size != anchor_size {
                bail!(
                    "hardlink content disagrees: {} -> {}",
                    path.display(),
                    anchor.display()
                );
            }
            *hardlink = Some(anchor.clone());
        }
    }
    Ok(())
}

pub(super) fn metadata(stat: &Stat, xattrs: Xattrs) -> Result<Metadata> {
    Ok(Metadata {
        mode: stat.st_mode & 0o7777,
        uid: stat.st_uid,
        gid: stat.st_gid,
        mtime: Timestamp {
            seconds: stat.st_mtime,
            nanos: u32::try_from(stat.st_mtime_nsec).context("mtime nanoseconds overflow")?,
        },
        xattrs,
    })
}

pub(super) fn ensure_same_object(initial: &Stat, opened: &Stat, path: &FsPath) -> Result<()> {
    if initial.st_dev != opened.st_dev
        || initial.st_ino != opened.st_ino
        || FileType::from_raw_mode(initial.st_mode) != FileType::from_raw_mode(opened.st_mode)
    {
        bail!("filesystem object changed while opening {}", path.display());
    }
    Ok(())
}

pub(super) fn ensure_stable(
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

pub(super) fn read_fd_xattrs(
    fd: impl AsFd,
    limits: RootfsLimits,
    budget: Option<&mut CaptureBudget>,
) -> Result<Xattrs> {
    let names = list_fd_xattr_names(&fd, limits.xattr_names_bytes)?;
    read_xattr_values(&names, limits, budget, |name, buffer| {
        fgetxattr(&fd, name, buffer).map_err(Into::into)
    })
}

pub(super) fn read_path_xattrs(
    parent: &OwnedFd,
    name: &[u8],
    limits: RootfsLimits,
    budget: Option<&mut CaptureBudget>,
) -> Result<Xattrs> {
    let path = proc_path(parent, name);
    let names = list_path_xattr_names(&path, limits.xattr_names_bytes)?;
    read_xattr_values(&names, limits, budget, |name, buffer| {
        lgetxattr(&path, name, buffer).map_err(Into::into)
    })
}

pub(super) fn read_xattr_values(
    names: &[u8],
    limits: RootfsLimits,
    mut budget: Option<&mut CaptureBudget>,
    mut get: impl FnMut(&[u8], &mut [u8]) -> Result<usize>,
) -> Result<Xattrs> {
    let mut result = Xattrs::new();
    if let Some(budget) = &mut budget {
        budget.xattrs(names.len())?;
    }
    for name in split_xattr_names(names)? {
        let required = get(name, &mut [])?;
        if required > limits.xattr_value_bytes {
            bail!("filesystem xattr value exceeds capture limit");
        }
        if let Some(budget) = &mut budget {
            budget.xattrs(required)?;
        }
        let mut value = vec![0_u8; required];
        let read = get(name, &mut value)?;
        value.truncate(read);
        if result
            .insert(name.into(), value.into_boxed_slice())
            .is_some()
        {
            bail!("filesystem returned duplicate xattr name");
        }
    }
    Ok(result)
}
