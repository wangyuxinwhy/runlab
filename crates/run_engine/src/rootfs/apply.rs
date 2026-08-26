use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs::File;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;

use anyhow::{Context, Result, bail};
use rustix::fs::{
    AtFlags, Dir, FileType, Mode, OFlags, Timestamps, UTIME_OMIT, XattrFlags, chmodat, chownat,
    fchmod, fchown, fremovexattr, fsetxattr, futimens, linkat, lremovexattr, lsetxattr, makedev,
    mkdirat, mkfifoat, mknodat, openat, statat, symlinkat, unlinkat, utimensat,
};
use rustix::io::Errno;
use rustix::process::{Gid, Uid};
use rustix::time::Timespec;

use super::plan::{LayerEntry, LayerKind, LayerPlan};
use super::xattr::{list_fd_xattr_names, list_path_xattr_names, split_xattr_names};
use super::{
    FsPath, MaterializationFault, Metadata, RootfsLimits, Timestamp, Xattrs, checked_total,
    default_directory, enforce, internal_error, invalid_input, os, proc_path, reopen_directory,
    take_materialization_fault, usize_to_u64,
};

pub(super) struct CleanupBudget {
    entries: u64,
    limits: RootfsLimits,
}

impl CleanupBudget {
    pub(super) fn new(limits: RootfsLimits) -> Self {
        Self { entries: 0, limits }
    }

    fn visit(&mut self, depth: u64) -> Result<()> {
        enforce("cleanup depth", self.limits.depth, depth)?;
        self.entries = checked_total(
            self.entries,
            1,
            self.limits.cleanup_entries,
            "cleanup entries",
        )?;
        Ok(())
    }
}

pub(super) struct PendingHardlink<'a> {
    entry: &'a LayerEntry,
    target: &'a FsPath,
}

pub(super) fn apply_plan(
    root: &OwnedFd,
    plan: &LayerPlan,
    limits: RootfsLimits,
    cleanup: &mut CleanupBudget,
) -> Result<()> {
    for path in &plan.whiteouts {
        remove_if_present(root, path, cleanup)?;
    }
    for path in &plan.opaques {
        remove_children(root, path, cleanup)?;
    }
    let mut pending = BTreeMap::new();
    for entry in &plan.entries {
        match &entry.kind {
            LayerKind::Directory => ensure_directory(root, &entry.path, cleanup)?,
            LayerKind::Regular { content, size } => {
                apply_regular(root, entry, content, *size, cleanup, limits)?;
            }
            LayerKind::Symlink(target) => {
                apply_symlink(root, entry, target, cleanup, limits)?;
            }
            LayerKind::Hardlink(target) => {
                if !try_hardlink(root, entry, target, cleanup)? {
                    enforce(
                        "pending hardlinks",
                        limits.pending_hardlinks,
                        usize_to_u64(pending.len() + 1),
                    )?;
                    pending.insert(entry.path.clone(), PendingHardlink { entry, target });
                }
            }
            LayerKind::Fifo => apply_fifo(root, entry, cleanup, limits)?,
            LayerKind::Character { major, minor } => apply_device(
                root,
                entry,
                FileType::CharacterDevice,
                *major,
                *minor,
                cleanup,
                limits,
            )?,
            LayerKind::Block { major, minor } => {
                apply_device(
                    root,
                    entry,
                    FileType::BlockDevice,
                    *major,
                    *minor,
                    cleanup,
                    limits,
                )?;
            }
        }
    }
    resolve_hardlinks(root, pending, cleanup)
}

pub(super) fn resolve_hardlinks(
    root: &OwnedFd,
    mut pending: BTreeMap<FsPath, PendingHardlink<'_>>,
    cleanup: &mut CleanupBudget,
) -> Result<()> {
    while let Some(origin) = pending.keys().next().cloned() {
        let mut current = origin;
        let mut chain = Vec::new();
        let mut visiting = BTreeSet::new();
        loop {
            let target = pending
                .get(&current)
                .context("hardlink plan lost an entry")?
                .target;
            if !visiting.insert(current.clone()) {
                return Err(invalid_input(format!(
                    "unresolved OCI hardlink cycle: {} -> {}",
                    current.display(),
                    target.display()
                )));
            }
            chain.push(current);
            if pending.contains_key(target) {
                current = target.clone();
            } else {
                break;
            }
        }
        for path in chain.into_iter().rev() {
            let item = pending
                .remove(&path)
                .context("hardlink plan lost an entry")?;
            if !try_hardlink(root, item.entry, item.target, cleanup)? {
                return Err(invalid_input(format!(
                    "unresolved OCI hardlink: {} -> {}",
                    item.entry.path.display(),
                    item.target.display()
                )));
            }
        }
    }
    Ok(())
}

pub(super) fn apply_regular(
    root: &OwnedFd,
    entry: &LayerEntry,
    content: &Path,
    size: u64,
    cleanup: &mut CleanupBudget,
    limits: RootfsLimits,
) -> Result<()> {
    let parent = open_parent(root, &entry.path, true)?;
    let name = os(entry.path.basename());
    remove_at_if_present(&parent, name, cleanup)?;
    let fd = create_regular_file(&parent, name)?;
    let mut destination = File::from(fd);
    let mut source = File::open(content)?;
    let copied = std::io::copy(&mut source, &mut destination)?;
    if copied != size {
        bail!(
            "staged regular content size changed for {}",
            entry.path.display()
        );
    }
    destination.sync_all()?;
    apply_metadata_fd(&destination, &entry.metadata, limits)
}

fn create_regular_file(parent: &OwnedFd, name: &OsStr) -> Result<OwnedFd> {
    if take_materialization_fault(MaterializationFault::ApplySyscall) {
        return Err(internal_error(std::io::Error::from_raw_os_error(
            Errno::IO.raw_os_error(),
        )));
    }
    Ok(openat(
        parent,
        name,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )?)
}

pub(super) fn apply_symlink(
    root: &OwnedFd,
    entry: &LayerEntry,
    target: &[u8],
    cleanup: &mut CleanupBudget,
    limits: RootfsLimits,
) -> Result<()> {
    let parent = open_parent(root, &entry.path, true)?;
    let name = os(entry.path.basename());
    remove_at_if_present(&parent, name, cleanup)?;
    symlinkat(os(target), &parent, name)?;
    apply_metadata_path(&parent, name, &entry.metadata, limits)
}

pub(super) fn apply_fifo(
    root: &OwnedFd,
    entry: &LayerEntry,
    cleanup: &mut CleanupBudget,
    limits: RootfsLimits,
) -> Result<()> {
    let parent = open_parent(root, &entry.path, true)?;
    let name = os(entry.path.basename());
    remove_at_if_present(&parent, name, cleanup)?;
    mkfifoat(&parent, name, Mode::RUSR | Mode::WUSR)?;
    apply_metadata_path(&parent, name, &entry.metadata, limits)
}

pub(super) fn apply_device(
    root: &OwnedFd,
    entry: &LayerEntry,
    file_type: FileType,
    major: u32,
    minor: u32,
    cleanup: &mut CleanupBudget,
    limits: RootfsLimits,
) -> Result<()> {
    let parent = open_parent(root, &entry.path, true)?;
    let name = os(entry.path.basename());
    remove_at_if_present(&parent, name, cleanup)?;
    mknodat(
        &parent,
        name,
        file_type,
        Mode::RUSR | Mode::WUSR,
        makedev(major, minor),
    )?;
    apply_metadata_path(&parent, name, &entry.metadata, limits)
}

pub(super) fn try_hardlink(
    root: &OwnedFd,
    entry: &LayerEntry,
    target: &FsPath,
    cleanup: &mut CleanupBudget,
) -> Result<bool> {
    let Some(target_parent) = open_parent_existing(root, target)? else {
        return Ok(false);
    };
    match statat(
        &target_parent,
        os(target.basename()),
        AtFlags::SYMLINK_NOFOLLOW,
    ) {
        Ok(stat) if FileType::from_raw_mode(stat.st_mode) != FileType::Directory => {}
        Ok(_) => {
            return Err(invalid_input(format!(
                "OCI hardlink target is a directory: {}",
                target.display()
            )));
        }
        Err(Errno::NOENT) => return Ok(false),
        Err(error) => return Err(error.into()),
    }
    let parent = open_parent(root, &entry.path, true)?;
    remove_at_if_present(&parent, os(entry.path.basename()), cleanup)?;
    linkat(
        &target_parent,
        os(target.basename()),
        &parent,
        os(entry.path.basename()),
        AtFlags::empty(),
    )?;
    Ok(true)
}

pub(super) fn ensure_directory(
    root: &OwnedFd,
    path: &FsPath,
    cleanup: &mut CleanupBudget,
) -> Result<()> {
    if path.is_root() {
        return Ok(());
    }
    let parent = open_parent(root, path, true)?;
    let name = os(path.basename());
    match openat(
        &parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(_) => Ok(()),
        Err(Errno::NOTDIR | Errno::LOOP) => {
            remove_at_if_present(&parent, name, cleanup)?;
            mkdirat(&parent, name, Mode::RUSR | Mode::WUSR | Mode::XUSR)?;
            Ok(())
        }
        Err(Errno::NOENT) => {
            mkdirat(&parent, name, Mode::RUSR | Mode::WUSR | Mode::XUSR)?;
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

pub(super) fn open_parent(root: &OwnedFd, path: &FsPath, create: bool) -> Result<OwnedFd> {
    let components = path.components().collect::<Vec<_>>();
    let mut directory = reopen_directory(root)?;
    for component in components.iter().take(components.len().saturating_sub(1)) {
        match openat(
            &directory,
            os(component),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(child) => directory = child,
            Err(Errno::NOENT) if create => {
                mkdirat(
                    &directory,
                    os(component),
                    Mode::RUSR | Mode::WUSR | Mode::XUSR,
                )?;
                let child = openat(
                    &directory,
                    os(component),
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )?;
                apply_new_directory_metadata(&child, &default_directory())?;
                directory = child;
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(directory)
}

pub(super) fn open_parent_existing(root: &OwnedFd, path: &FsPath) -> Result<Option<OwnedFd>> {
    match open_parent(root, path, false) {
        Ok(parent) => Ok(Some(parent)),
        Err(error) if error.downcast_ref::<Errno>() == Some(&Errno::NOENT) => Ok(None),
        Err(error) => Err(error),
    }
}

pub(super) fn open_directory(root: &OwnedFd, path: &FsPath) -> Result<OwnedFd> {
    if path.is_root() {
        return reopen_directory(root);
    }
    let parent = open_parent(root, path, false)?;
    Ok(openat(
        &parent,
        os(path.basename()),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?)
}

pub(super) fn remove_if_present(
    root: &OwnedFd,
    path: &FsPath,
    budget: &mut CleanupBudget,
) -> Result<()> {
    let Some(parent) = open_parent_existing(root, path)? else {
        return Ok(());
    };
    remove_at_if_present(&parent, os(path.basename()), budget)
}

pub(super) fn remove_children(
    root: &OwnedFd,
    path: &FsPath,
    budget: &mut CleanupBudget,
) -> Result<()> {
    let directory = match open_directory(root, path) {
        Ok(directory) => directory,
        Err(error)
            if error.downcast_ref::<Errno>().is_some_and(|errno| {
                [Errno::NOENT, Errno::NOTDIR, Errno::LOOP].contains(errno)
            }) =>
        {
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    remove_directory_entries(&directory, 1, budget)
}

pub(super) fn remove_at_if_present(
    parent: &OwnedFd,
    name: &OsStr,
    budget: &mut CleanupBudget,
) -> Result<()> {
    match remove_at(parent, name, 1, budget) {
        Ok(()) => Ok(()),
        Err(error) if error.downcast_ref::<Errno>() == Some(&Errno::NOENT) => Ok(()),
        Err(error) => Err(error),
    }
}

pub(super) fn remove_at(
    parent: &OwnedFd,
    name: &OsStr,
    depth: u64,
    budget: &mut CleanupBudget,
) -> Result<()> {
    let stat = statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)?;
    budget.visit(depth)?;
    if FileType::from_raw_mode(stat.st_mode) == FileType::Directory {
        let directory = openat(
            parent,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        remove_directory_entries(
            &directory,
            depth.checked_add(1).context("cleanup depth overflow")?,
            budget,
        )?;
        unlinkat(parent, name, AtFlags::REMOVEDIR)?;
    } else {
        unlinkat(parent, name, AtFlags::empty())?;
    }
    Ok(())
}

pub(super) fn remove_directory_entries(
    directory: &OwnedFd,
    depth: u64,
    budget: &mut CleanupBudget,
) -> Result<()> {
    for result in Dir::read_from(directory)? {
        let entry = result?;
        let name = entry.file_name().to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        let name = name.to_vec();
        remove_at(directory, os(&name), depth, budget)?;
    }
    Ok(())
}

pub(super) fn apply_directory_metadata(
    root: &OwnedFd,
    entries: BTreeMap<FsPath, Metadata>,
    limits: RootfsLimits,
) -> Result<()> {
    let mut entries = entries.into_iter().collect::<Vec<_>>();
    entries.sort_by_key(|(path, _)| std::cmp::Reverse(path.components().count()));
    for (path, metadata) in entries {
        let fd = open_directory(root, &path)?;
        apply_metadata_fd(&fd, &metadata, limits)?;
    }
    Ok(())
}

pub(super) fn apply_metadata_fd(
    fd: impl AsFd,
    metadata: &Metadata,
    limits: RootfsLimits,
) -> Result<()> {
    fchown(
        &fd,
        Some(valid_uid(metadata.uid)?),
        Some(valid_gid(metadata.gid)?),
    )?;
    replace_fd_xattrs(&fd, &metadata.xattrs, limits)?;
    fchmod(&fd, Mode::from_raw_mode(metadata.mode))?;
    futimens(&fd, &timestamps(metadata.mtime))?;
    Ok(())
}

pub(super) fn apply_new_directory_metadata(fd: impl AsFd, metadata: &Metadata) -> Result<()> {
    fchown(
        &fd,
        Some(valid_uid(metadata.uid)?),
        Some(valid_gid(metadata.gid)?),
    )?;
    fchmod(&fd, Mode::from_raw_mode(metadata.mode))?;
    futimens(&fd, &timestamps(metadata.mtime))?;
    Ok(())
}

pub(super) fn apply_metadata_path(
    parent: &OwnedFd,
    name: &OsStr,
    metadata: &Metadata,
    limits: RootfsLimits,
) -> Result<()> {
    chownat(
        parent,
        name,
        Some(valid_uid(metadata.uid)?),
        Some(valid_gid(metadata.gid)?),
        AtFlags::SYMLINK_NOFOLLOW,
    )?;
    replace_path_xattrs(parent, name, &metadata.xattrs, limits)?;
    let stat = statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::Symlink {
        chmodat(
            parent,
            name,
            Mode::from_raw_mode(metadata.mode),
            AtFlags::empty(),
        )?;
    }
    utimensat(
        parent,
        name,
        &timestamps(metadata.mtime),
        AtFlags::SYMLINK_NOFOLLOW,
    )?;
    Ok(())
}

pub(super) fn replace_fd_xattrs(
    fd: impl AsFd,
    xattrs: &Xattrs,
    limits: RootfsLimits,
) -> Result<()> {
    let names = list_fd_xattr_names(&fd, limits.xattr_names_bytes)?;
    for name in split_xattr_names(&names)? {
        fremovexattr(&fd, name)?;
    }
    for (name, value) in xattrs {
        fsetxattr(&fd, name.as_ref(), value, XattrFlags::empty())?;
    }
    Ok(())
}

pub(super) fn replace_path_xattrs(
    parent: &OwnedFd,
    name: &OsStr,
    xattrs: &Xattrs,
    limits: RootfsLimits,
) -> Result<()> {
    let path = proc_path(parent, name.as_bytes());
    let names = list_path_xattr_names(&path, limits.xattr_names_bytes)?;
    for old in split_xattr_names(&names)? {
        lremovexattr(&path, old)?;
    }
    for (key, value) in xattrs {
        lsetxattr(&path, key.as_ref(), value, XattrFlags::empty())?;
    }
    Ok(())
}

pub(super) fn timestamps(timestamp: Timestamp) -> Timestamps {
    Timestamps {
        last_access: Timespec {
            tv_sec: 0,
            tv_nsec: UTIME_OMIT,
        },
        last_modification: Timespec {
            tv_sec: timestamp.seconds,
            tv_nsec: i64::from(timestamp.nanos),
        },
    }
}

pub(super) fn valid_uid(raw: u32) -> Result<Uid> {
    if raw == u32::MAX {
        return Err(invalid_input("OCI Layer uid is reserved"));
    }
    Ok(Uid::from_raw(raw))
}

pub(super) fn valid_gid(raw: u32) -> Result<Gid> {
    if raw == u32::MAX {
        return Err(invalid_input("OCI Layer gid is reserved"));
    }
    Ok(Gid::from_raw(raw))
}
