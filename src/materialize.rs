//! Writing verified Layers into a private rootfs on disk.
//!
//! Reproduces ownership, modes, timestamps and extended attributes as the
//! Layers describe them, under either native ownership or a rootless single-ID
//! mapping. Linux-only: the metadata this preserves has no faithful
//! representation elsewhere.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs::File;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rustix::fs::{
    AtFlags, Dir, FileType, Mode, OFlags, Timestamps, UTIME_OMIT, XattrFlags, chmodat, chownat,
    fchmod, fchown, flistxattr, fremovexattr, fsetxattr, futimens, linkat, llistxattr,
    lremovexattr, lsetxattr, makedev, mkdirat, mkfifoat, mknodat, open, openat, statat, symlinkat,
    unlinkat, utimensat,
};
use rustix::io::Errno;
use rustix::process::{Gid, Uid};
use rustix::time::Timespec;
#[cfg(test)]
use tempfile::TempDir;
use tempfile::{NamedTempFile, TempPath};

use crate::core::{ImageView, OciDescriptor};
use crate::filesystem::{FilesystemOwnership, FsPath, Metadata, Timestamp, Xattrs};
use crate::integrity::ensure_private_directory;
use crate::oci::OciLayout;
use crate::render::{
    LayerEntry, LayerEntryKind, LayerPlan, RenderError, RenderLimits, descendant_bounds,
    scan_layer, visit_layer_regulars,
};

#[derive(Debug)]
pub(crate) struct MaterializedRootfs {
    workspace: MaterializationWorkspace,
    rootfs: PathBuf,
}

#[derive(Debug)]
enum MaterializationWorkspace {
    #[cfg(test)]
    Temporary(TempDir),
    External(PathBuf),
}

impl MaterializationWorkspace {
    fn path(&self) -> &Path {
        match self {
            #[cfg(test)]
            Self::Temporary(directory) => directory.path(),
            Self::External(path) => path,
        }
    }
}

impl MaterializedRootfs {
    #[must_use]
    pub(crate) fn path(&self) -> &Path {
        &self.rootfs
    }

    pub(crate) fn preserve(self) -> PathBuf {
        let rootfs = self.rootfs;
        match self.workspace {
            #[cfg(test)]
            MaterializationWorkspace::Temporary(workspace) => {
                let _ = workspace.keep();
            }
            MaterializationWorkspace::External(_) => {}
        }
        rootfs
    }
}

#[cfg(test)]
pub(crate) fn materialize(
    layout: &OciLayout,
    image: &ImageView,
    limits: RenderLimits,
) -> Result<MaterializedRootfs> {
    let workspace = tempfile::Builder::new()
        .prefix("runlab-rootfs-")
        .tempdir()?;
    materialize_in(
        layout,
        image,
        limits,
        MaterializationWorkspace::Temporary(workspace),
        test_ownership(),
    )
}

#[cfg(test)]
fn test_ownership() -> FilesystemOwnership {
    let uid = rustix::process::geteuid();
    if uid.is_root() {
        FilesystemOwnership::Native
    } else {
        FilesystemOwnership::SingleId {
            host_uid: uid.as_raw(),
            host_gid: rustix::process::getegid().as_raw(),
        }
    }
}

pub(crate) fn materialize_at_with_ownership(
    layout: &OciLayout,
    image: &ImageView,
    limits: RenderLimits,
    workspace: &Path,
    ownership: FilesystemOwnership,
) -> Result<MaterializedRootfs> {
    std::fs::create_dir(workspace).with_context(|| {
        format!(
            "failed to create materialization workspace {}",
            workspace.display()
        )
    })?;
    ensure_private_directory(workspace)?;
    materialize_in(
        layout,
        image,
        limits,
        MaterializationWorkspace::External(workspace.to_path_buf()),
        ownership,
    )
}

fn materialize_in(
    layout: &OciLayout,
    image: &ImageView,
    limits: RenderLimits,
    workspace: MaterializationWorkspace,
    ownership: FilesystemOwnership,
) -> Result<MaterializedRootfs> {
    enforce_limit(
        "max_layers",
        limits.layers,
        usize_to_u64(image.layers.len()),
    )?;
    let mut plans = Vec::with_capacity(image.layers.len());
    let mut entries = 0_u64;
    let mut uncompressed = 0_u64;
    for descriptor in &image.layers {
        let remaining = limits
            .total_uncompressed_bytes
            .checked_sub(uncompressed)
            .context("Layer byte count exceeds materialization limit")?;
        let (plan, layer_entries, layer_bytes) = scan_layer(layout, descriptor, limits, remaining)?;
        entries = entries
            .checked_add(layer_entries)
            .context("Layer entry count overflow")?;
        enforce_limit("max_entries", limits.entries, entries)?;
        uncompressed = uncompressed
            .checked_add(layer_bytes)
            .context("Layer byte count overflow")?;
        plans.push((descriptor.clone(), plan, layer_bytes));
    }

    let rootfs = workspace.path().join("rootfs");
    let staging = workspace.path().join("content");
    std::fs::create_dir(&rootfs)
        .with_context(|| format!("failed to create rootfs {}", rootfs.display()))?;
    std::fs::create_dir(&staging)
        .with_context(|| format!("failed to create content staging {}", staging.display()))?;
    let root = open(
        &rootfs,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let writer = RootfsWriter {
        root,
        staging,
        layout,
        limits,
        cleanup: RefCell::new(CleanupBudget::new(limits)),
        ownership,
    };
    let mut directory_metadata = DirectoryMetadata::new();
    for (descriptor, plan, layer_bytes) in plans {
        directory_metadata.apply(&plan)?;
        writer.apply(&descriptor, plan, layer_bytes)?;
    }
    writer.apply_directory_metadata(directory_metadata.entries)?;
    Ok(MaterializedRootfs { workspace, rootfs })
}

struct RootfsWriter<'a> {
    root: OwnedFd,
    staging: PathBuf,
    layout: &'a OciLayout,
    limits: RenderLimits,
    cleanup: RefCell<CleanupBudget>,
    ownership: FilesystemOwnership,
}

struct CleanupBudget {
    entries: u64,
    entry_limit: u64,
    depth_limit: u64,
}

struct DirectoryMetadata {
    entries: BTreeMap<FsPath, Metadata>,
}

impl DirectoryMetadata {
    fn new() -> Self {
        Self {
            entries: BTreeMap::from([(
                FsPath::from_relative(b"", 0).expect("root path is valid"),
                default_directory_metadata(),
            )]),
        }
    }

    fn apply(&mut self, plan: &LayerPlan) -> Result<()> {
        for path in &plan.whiteouts {
            self.remove_subtree(path, true);
        }
        for path in &plan.opaques {
            self.remove_subtree(path, false);
        }
        for entry in &plan.entries {
            self.ensure_ancestors(&entry.path)?;
            if matches!(entry.kind, LayerEntryKind::Directory) {
                self.entries
                    .insert(entry.path.clone(), entry.metadata.clone());
            } else {
                self.remove_subtree(&entry.path, true);
            }
        }
        Ok(())
    }

    fn ensure_ancestors(&mut self, path: &FsPath) -> Result<()> {
        let components = path.components().map(<[u8]>::to_vec).collect::<Vec<_>>();
        for end in 0..components.len() {
            let ancestor = FsPath::from_normalized_components(
                &components[..end],
                u64::try_from(path.as_bytes().len()).unwrap_or(u64::MAX),
            )?;
            self.entries
                .entry(ancestor)
                .or_insert_with(default_directory_metadata);
        }
        Ok(())
    }

    fn remove_subtree(&mut self, path: &FsPath, include_root: bool) {
        let descendants = if path.is_root() {
            self.entries
                .keys()
                .filter(|candidate| !candidate.is_root())
                .cloned()
                .collect::<Vec<_>>()
        } else {
            let (start, end) = descendant_bounds(path);
            self.entries
                .range(start..end)
                .map(|(candidate, _)| candidate.clone())
                .collect()
        };
        for descendant in descendants {
            self.entries.remove(&descendant);
        }
        if include_root {
            self.entries.remove(path);
        }
    }
}

impl CleanupBudget {
    const fn new(limits: RenderLimits) -> Self {
        Self {
            entries: 0,
            entry_limit: limits.cleanup_entries,
            depth_limit: limits.cleanup_depth,
        }
    }

    fn visit(&mut self, depth: u64) -> Result<()> {
        enforce_limit("max_cleanup_depth", self.depth_limit, depth)?;
        self.entries = self
            .entries
            .checked_add(1)
            .context("filesystem cleanup entry count overflow")?;
        enforce_limit("max_cleanup_entries", self.entry_limit, self.entries)
    }
}

impl RootfsWriter<'_> {
    fn apply(&self, descriptor: &OciDescriptor, plan: LayerPlan, layer_bytes: u64) -> Result<()> {
        let mut staged = self.stage_regulars(descriptor, &plan.entries, layer_bytes)?;
        for path in plan.whiteouts {
            self.remove_if_present(&path)?;
        }
        for path in plan.opaques {
            self.remove_children(&path)?;
        }

        let mut pending = BTreeMap::new();
        for entry in plan.entries {
            if matches!(entry.kind, LayerEntryKind::Directory) {
                self.ensure_directory(&entry.path)?;
            } else if matches!(entry.kind, LayerEntryKind::Hardlink(_)) {
                if !self.try_hardlink(&entry)? {
                    pending.insert(entry.path.clone(), entry);
                }
            } else if let LayerEntryKind::Regular(location) = &entry.kind {
                let source = staged
                    .remove(&location.ordinal)
                    .context("materialization content plan lost a regular file")?;
                self.apply_regular(&entry, &source)?;
            } else {
                self.apply_entry(&entry)?;
            }
        }
        self.apply_pending_hardlinks(pending)?;
        if !staged.is_empty() {
            bail!("materialization content plan left a regular file unapplied");
        }
        Ok(())
    }

    fn apply_pending_hardlinks(&self, mut pending: BTreeMap<FsPath, LayerEntry>) -> Result<()> {
        while let Some(origin) = pending.keys().next().cloned() {
            let mut current = origin;
            let mut chain = Vec::new();
            let mut visiting = BTreeSet::new();
            loop {
                let entry = pending
                    .get(&current)
                    .context("materialization hardlink plan lost an entry")?;
                let LayerEntryKind::Hardlink(target) = &entry.kind else {
                    unreachable!()
                };
                if !visiting.insert(current.clone()) {
                    return Err(RenderError::UnresolvedHardlink {
                        path: current.display(),
                        target: target.display(),
                    }
                    .into());
                }
                chain.push(current);
                if pending.contains_key(target) {
                    current = target.clone();
                    continue;
                }
                break;
            }
            for path in chain.into_iter().rev() {
                let entry = pending
                    .remove(&path)
                    .context("materialization hardlink plan lost an entry")?;
                if !self.try_hardlink(&entry)? {
                    let LayerEntryKind::Hardlink(target) = &entry.kind else {
                        unreachable!()
                    };
                    return Err(RenderError::UnresolvedHardlink {
                        path: entry.path.display(),
                        target: target.display(),
                    }
                    .into());
                }
            }
        }
        Ok(())
    }

    fn apply_directory_metadata(&self, entries: BTreeMap<FsPath, Metadata>) -> Result<()> {
        let mut entries = entries.into_iter().collect::<Vec<_>>();
        entries.sort_by_key(|(path, _)| std::cmp::Reverse(path.components().count()));
        for (path, metadata) in entries {
            let fd = self.open_directory(&path, false)?;
            self.apply_metadata_fd(&fd, &metadata)?;
        }
        Ok(())
    }

    fn stage_regulars(
        &self,
        descriptor: &OciDescriptor,
        entries: &[LayerEntry],
        layer_bytes: u64,
    ) -> Result<BTreeMap<u64, TempPath>> {
        let expected = entries
            .iter()
            .filter(|entry| matches!(entry.kind, LayerEntryKind::Regular(_)))
            .cloned()
            .collect::<Vec<_>>();
        let mut staged = BTreeMap::new();
        visit_layer_regulars(
            self.layout,
            descriptor,
            &expected,
            self.limits,
            layer_bytes,
            |entry, source| {
                let LayerEntryKind::Regular(location) = &entry.kind else {
                    unreachable!()
                };
                let mut file = NamedTempFile::new_in(&self.staging)?;
                let copied = std::io::copy(source, file.as_file_mut())?;
                if copied != location.size {
                    bail!(
                        "OCI Layer file size changed: expected {}, received {copied}",
                        location.size
                    );
                }
                file.as_file_mut().sync_all()?;
                if staged
                    .insert(location.ordinal, file.into_temp_path())
                    .is_some()
                {
                    bail!("duplicate regular-file ordinal in materialization content plan");
                }
                Ok(())
            },
        )?;
        Ok(staged)
    }

    fn apply_entry(&self, entry: &LayerEntry) -> Result<()> {
        let parent = self.open_parent(&entry.path, true)?;
        let name = os(entry.path.basename());
        self.remove_at_if_present(&parent, name)?;
        match &entry.kind {
            LayerEntryKind::Symlink(target) => {
                symlinkat(os(target), &parent, name)?;
                self.apply_metadata_path(&parent, name, &entry.metadata)?;
            }
            LayerEntryKind::Fifo => {
                mkfifoat(&parent, name, Mode::RUSR | Mode::WUSR)?;
                self.apply_metadata_path(&parent, name, &entry.metadata)?;
            }
            LayerEntryKind::Character { major, minor } => {
                if self.ownership.is_single_id() {
                    bail!("rootless native execution does not support character devices in Images");
                }
                mknodat(
                    &parent,
                    name,
                    FileType::CharacterDevice,
                    Mode::RUSR | Mode::WUSR,
                    makedev(*major, *minor),
                )?;
                self.apply_metadata_path(&parent, name, &entry.metadata)?;
            }
            LayerEntryKind::Block { major, minor } => {
                if self.ownership.is_single_id() {
                    bail!("rootless native execution does not support block devices in Images");
                }
                mknodat(
                    &parent,
                    name,
                    FileType::BlockDevice,
                    Mode::RUSR | Mode::WUSR,
                    makedev(*major, *minor),
                )?;
                self.apply_metadata_path(&parent, name, &entry.metadata)?;
            }
            LayerEntryKind::Regular(_)
            | LayerEntryKind::Directory
            | LayerEntryKind::Hardlink(_) => unreachable!(),
        }
        Ok(())
    }

    fn apply_regular(&self, entry: &LayerEntry, source: &Path) -> Result<()> {
        let LayerEntryKind::Regular(location) = &entry.kind else {
            unreachable!()
        };
        let parent = self.open_parent(&entry.path, true)?;
        let name = os(entry.path.basename());
        self.remove_at_if_present(&parent, name)?;
        let destination = openat(
            &parent,
            name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )?;
        let mut destination = File::from(destination);
        let mut source = File::open(source).context("failed to open staged Layer content")?;
        let copied = std::io::copy(&mut source, &mut destination)?;
        if copied != location.size {
            bail!(
                "staged OCI Layer file size changed: expected {}, received {copied}",
                location.size
            );
        }
        destination.sync_all()?;
        self.apply_metadata_fd(&destination, &entry.metadata)
    }

    fn try_hardlink(&self, entry: &LayerEntry) -> Result<bool> {
        let LayerEntryKind::Hardlink(target) = &entry.kind else {
            unreachable!()
        };
        let Some(target_parent) = self.open_parent_existing(target)? else {
            return Ok(false);
        };
        if statat(
            &target_parent,
            os(target.basename()),
            AtFlags::SYMLINK_NOFOLLOW,
        )
        .is_err_and(|error| error == Errno::NOENT)
        {
            return Ok(false);
        }
        let parent = self.open_parent(&entry.path, true)?;
        self.remove_at_if_present(&parent, os(entry.path.basename()))?;
        linkat(
            &target_parent,
            os(target.basename()),
            &parent,
            os(entry.path.basename()),
            AtFlags::empty(),
        )?;
        Ok(true)
    }

    fn ensure_directory(&self, path: &FsPath) -> Result<()> {
        if path.is_root() {
            return Ok(());
        }
        let parent = self.open_parent(path, true)?;
        let name = os(path.basename());
        match openat(
            &parent,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(_) => Ok(()),
            Err(Errno::NOTDIR | Errno::LOOP) => {
                self.remove_at_if_present(&parent, name)?;
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

    fn open_parent(&self, path: &FsPath, create: bool) -> Result<OwnedFd> {
        let components = path.components().collect::<Vec<_>>();
        let mut directory = self.open_directory(&FsPath::from_relative(b"", 0)?, false)?;
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
                    self.apply_metadata_fd(&child, &default_directory_metadata())?;
                    directory = child;
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(directory)
    }

    fn open_parent_existing(&self, path: &FsPath) -> Result<Option<OwnedFd>> {
        match self.open_parent(path, false) {
            Ok(parent) => Ok(Some(parent)),
            Err(error) if error.downcast_ref::<Errno>() == Some(&Errno::NOENT) => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn open_directory(&self, path: &FsPath, create: bool) -> Result<OwnedFd> {
        if path.is_root() {
            return Ok(openat(
                &self.root,
                c".",
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )?);
        }
        if create {
            self.ensure_directory(path)?;
        }
        let parent = self.open_parent(path, create)?;
        Ok(openat(
            &parent,
            os(path.basename()),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?)
    }

    fn remove_if_present(&self, path: &FsPath) -> Result<()> {
        let Some(parent) = self.open_parent_existing(path)? else {
            return Ok(());
        };
        self.remove_at_if_present(&parent, os(path.basename()))
    }

    fn remove_children(&self, path: &FsPath) -> Result<()> {
        let directory = match self.open_directory(path, false) {
            Ok(directory) => directory,
            Err(error) if error.downcast_ref::<Errno>() == Some(&Errno::NOENT) => return Ok(()),
            Err(error) => return Err(error),
        };
        for name in directory_names(&directory)? {
            let mut budget = self.cleanup.borrow_mut();
            Self::remove_at(&directory, os(&name), 1, &mut budget)?;
        }
        Ok(())
    }

    fn remove_at_if_present(&self, parent: &OwnedFd, name: &OsStr) -> Result<()> {
        let mut budget = self.cleanup.borrow_mut();
        match Self::remove_at(parent, name, 1, &mut budget) {
            Ok(()) => Ok(()),
            Err(error) if error.downcast_ref::<Errno>() == Some(&Errno::NOENT) => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn remove_at(
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
            for child in directory_names(&directory)? {
                Self::remove_at(
                    &directory,
                    os(&child),
                    depth
                        .checked_add(1)
                        .context("filesystem cleanup depth overflow")?,
                    budget,
                )?;
            }
            unlinkat(parent, name, AtFlags::REMOVEDIR)?;
        } else {
            unlinkat(parent, name, AtFlags::empty())?;
        }
        Ok(())
    }

    fn apply_metadata_fd(&self, fd: &impl std::os::fd::AsFd, metadata: &Metadata) -> Result<()> {
        self.ownership.validate_xattrs(&metadata.xattrs)?;
        let (uid_value, gid_value) = self
            .ownership
            .materialized_ids(metadata.uid, metadata.gid)?;
        fchown(fd, Some(uid(uid_value)?), Some(gid(gid_value)?))?;
        replace_fd_xattrs(fd, &metadata.xattrs)?;
        fchmod(fd, Mode::from_raw_mode(metadata.mode))?;
        futimens(fd, &timestamps(metadata.mtime))?;
        Ok(())
    }

    fn apply_metadata_path(
        &self,
        parent: &OwnedFd,
        name: &OsStr,
        metadata: &Metadata,
    ) -> Result<()> {
        self.ownership.validate_xattrs(&metadata.xattrs)?;
        let (uid_value, gid_value) = self
            .ownership
            .materialized_ids(metadata.uid, metadata.gid)?;
        chownat(
            parent,
            name,
            Some(uid(uid_value)?),
            Some(gid(gid_value)?),
            AtFlags::SYMLINK_NOFOLLOW,
        )?;
        replace_path_xattrs(parent, name, &metadata.xattrs)?;
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
}

fn replace_fd_xattrs(fd: &impl std::os::fd::AsFd, xattrs: &Xattrs) -> Result<()> {
    let mut empty = [0_u8; 0];
    let required = flistxattr(fd, &mut empty)?;
    let mut names = vec![0_u8; required];
    let read = flistxattr(fd, &mut names)?;
    names.truncate(read);
    for name in xattr_names(&names)? {
        fremovexattr(fd, name)?;
    }
    for (name, value) in xattrs {
        fsetxattr(fd, name.as_ref(), value, XattrFlags::empty())?;
    }
    Ok(())
}

fn replace_path_xattrs(parent: &OwnedFd, name: &OsStr, xattrs: &Xattrs) -> Result<()> {
    let path = proc_path(parent, name);
    let mut empty = [0_u8; 0];
    let required = llistxattr(&path, &mut empty)?;
    let mut names = vec![0_u8; required];
    let read = llistxattr(&path, &mut names)?;
    names.truncate(read);
    for name in xattr_names(&names)? {
        lremovexattr(&path, name)?;
    }
    for (name, value) in xattrs {
        lsetxattr(&path, name.as_ref(), value, XattrFlags::empty())?;
    }
    Ok(())
}

fn xattr_names(bytes: &[u8]) -> Result<impl Iterator<Item = &[u8]>> {
    if !bytes.is_empty() && !bytes.ends_with(&[0]) {
        bail!("filesystem returned a malformed xattr name list");
    }
    Ok(bytes
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty()))
}

fn proc_path(parent: &OwnedFd, name: &OsStr) -> PathBuf {
    let mut bytes = format!("/proc/self/fd/{}/", parent.as_raw_fd()).into_bytes();
    bytes.extend_from_slice(name.as_bytes());
    PathBuf::from(OsStr::from_bytes(&bytes))
}

fn timestamps(timestamp: Timestamp) -> Timestamps {
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

fn uid(raw: u32) -> Result<Uid> {
    if raw == u32::MAX {
        bail!("OCI Layer uid is reserved")
    }
    Ok(Uid::from_raw(raw))
}

fn gid(raw: u32) -> Result<Gid> {
    if raw == u32::MAX {
        bail!("OCI Layer gid is reserved")
    }
    Ok(Gid::from_raw(raw))
}

fn default_directory_metadata() -> Metadata {
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

fn directory_names(directory: &OwnedFd) -> Result<Vec<Vec<u8>>> {
    let mut names = Vec::new();
    for entry in Dir::read_from(directory)? {
        let entry = entry?;
        let name = entry.file_name().to_bytes();
        if name != b"." && name != b".." {
            names.push(name.to_vec());
        }
    }
    Ok(names)
}

fn os(bytes: &[u8]) -> &OsStr {
    OsStr::from_bytes(bytes)
}

fn enforce_limit(name: &'static str, limit: u64, observed: u64) -> Result<()> {
    if observed > limit {
        return Err(RenderError::LimitExceeded {
            name,
            limit,
            observed,
        }
        .into());
    }
    Ok(())
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};

    use tar::{EntryType, Header};

    use crate::changeset::{LayerEncoder, compare};
    use crate::core::{Architecture, Digest, OCI_IMAGE_CONFIG, OCI_LAYER_TAR, Platform};
    use crate::filesystem::{ContentStore, EntryKind, FsEntry, Inventory, TreeCapture};
    use crate::integrity::digest_bytes;
    use crate::render::{materialization_content_passes, reset_materialization_content_passes};

    use super::*;

    #[test]
    fn ordered_layers_materialize_to_the_semantic_inventory() {
        let state = tempfile::tempdir().expect("state");
        let layout = OciLayout::open(state.path()).expect("layout");
        let (image, expected) = fixture(&layout);

        reset_materialization_content_passes();
        let rootfs = materialize(&layout, &image, RenderLimits::default()).expect("materialize");
        let captured = TreeCapture::with_ownership(test_ownership())
            .capture(rootfs.path())
            .expect("capture rootfs");
        assert_eq!(captured.inventory, expected);
        assert_eq!(materialization_content_passes(), 2);
    }

    #[test]
    fn opaque_whiteout_removes_only_lower_children_in_one_layer_pass() {
        let state = tempfile::tempdir().expect("state");
        let layout = OciLayout::open(state.path()).expect("layout");
        let lower = raw_layer(&[
            RawEntry::directory(b"dir/"),
            RawEntry::file(b"dir/old-a", b"a"),
            RawEntry::file(b"dir/old-b", b"b"),
        ]);
        let upper = raw_layer(&[
            RawEntry::file(b"dir/.wh..wh..opq", b""),
            RawEntry::file(b"dir/new", b"new"),
        ]);
        let image = raw_image(&layout, &[lower, upper]);

        reset_materialization_content_passes();
        let rootfs = materialize(&layout, &image, RenderLimits::default()).expect("materialize");
        assert_eq!(
            std::fs::read(rootfs.path().join("dir/new")).expect("new"),
            b"new"
        );
        assert!(!rootfs.path().join("dir/old-a").exists());
        assert!(!rootfs.path().join("dir/old-b").exists());
        assert_eq!(materialization_content_passes(), 2);
    }

    #[test]
    fn lower_directory_metadata_is_replayed_after_upper_children_change() {
        let state = tempfile::tempdir().expect("state");
        let layout = OciLayout::open(state.path()).expect("layout");
        let lower = raw_layer(&[RawEntry::directory(b"."), RawEntry::directory(b"dir/")]);
        let upper = raw_layer(&[RawEntry::file(b"dir/value", b"value")]);
        let image = raw_image(&layout, &[lower, upper]);

        let rootfs = materialize(&layout, &image, RenderLimits::default()).expect("materialize");
        let directories = [rootfs.path().to_path_buf(), rootfs.path().join("dir")];
        for directory in directories {
            let metadata = std::fs::metadata(directory).expect("directory metadata");
            assert_eq!(metadata.mode() & 0o7777, 0o755);
            assert_eq!(metadata.mtime(), 0);
        }
    }

    #[test]
    fn forward_hardlinks_materialize_in_dependency_order() {
        let state = tempfile::tempdir().expect("state");
        let layout = OciLayout::open(state.path()).expect("layout");
        let layer = raw_layer(&[
            RawEntry::hardlink(b"first", b"second"),
            RawEntry::hardlink(b"second", b"target"),
            RawEntry::file(b"target", b"value"),
        ]);
        let image = raw_image(&layout, &[layer]);

        let rootfs = materialize(&layout, &image, RenderLimits::default()).expect("materialize");
        let first = std::fs::metadata(rootfs.path().join("first")).expect("first");
        let second = std::fs::metadata(rootfs.path().join("second")).expect("second");
        let target = std::fs::metadata(rootfs.path().join("target")).expect("target");
        assert_eq!(first.ino(), target.ino());
        assert_eq!(second.ino(), target.ino());
    }

    #[test]
    fn hardlink_cycle_fails_without_repeated_fixed_point_passes() {
        let state = tempfile::tempdir().expect("state");
        let layout = OciLayout::open(state.path()).expect("layout");
        let layer = raw_layer(&[
            RawEntry::hardlink(b"first", b"second"),
            RawEntry::hardlink(b"second", b"first"),
        ]);
        let image = raw_image(&layout, &[layer]);

        let error =
            materialize(&layout, &image, RenderLimits::default()).expect_err("hardlink cycle");
        assert!(matches!(
            error.downcast_ref::<RenderError>(),
            Some(RenderError::UnresolvedHardlink { .. })
        ));
    }

    #[test]
    fn recursive_cleanup_enforces_depth_and_entry_budgets() {
        let state = tempfile::tempdir().expect("state");
        let layout = OciLayout::open(state.path()).expect("layout");
        let lower = raw_layer(&[
            RawEntry::directory(b"tree/"),
            RawEntry::directory(b"tree/branch/"),
            RawEntry::file(b"tree/branch/leaf", b"leaf"),
            RawEntry::file(b"tree/peer", b"peer"),
        ]);
        let upper = raw_layer(&[RawEntry::file(b".wh.tree", b"")]);
        let image = raw_image(&layout, &[lower, upper]);

        let depth_error = materialize(
            &layout,
            &image,
            RenderLimits {
                cleanup_depth: 1,
                ..RenderLimits::default()
            },
        )
        .expect_err("cleanup depth");
        assert!(format!("{depth_error:#}").contains("max_cleanup_depth"));

        let entry_error = materialize(
            &layout,
            &image,
            RenderLimits {
                cleanup_entries: 2,
                ..RenderLimits::default()
            },
        )
        .expect_err("cleanup entries");
        assert!(format!("{entry_error:#}").contains("max_cleanup_entries"));
    }

    #[test]
    #[ignore = "requires rootful Linux with CAP_MKNOD"]
    fn rootful_materialization_creates_character_device_1_3() {
        let state = tempfile::tempdir().expect("state");
        let layout = OciLayout::open(state.path()).expect("layout");
        let layer = raw_layer(&[
            RawEntry::directory(b"dev/"),
            RawEntry::character(b"dev/null", 1, 3),
        ]);
        let image = raw_image(&layout, &[layer]);

        let rootfs = materialize(&layout, &image, RenderLimits::default()).expect("materialize");
        let metadata = std::fs::symlink_metadata(rootfs.path().join("dev/null")).expect("device");
        assert!(metadata.file_type().is_char_device());
        assert_eq!(rustix::fs::major(metadata.rdev()), 1);
        assert_eq!(rustix::fs::minor(metadata.rdev()), 3);
    }

    fn fixture(layout: &OciLayout) -> (ImageView, Inventory) {
        let mut contents = ContentStore::new().expect("contents");
        let target = contents.put_bytes(b"target\0bytes").expect("target");
        let old = contents.put_bytes(b"old").expect("old");
        let replacement = contents.put_bytes(b"child").expect("child");
        let new = contents.put_bytes(b"new").expect("new");
        let raw = contents.put_bytes(b"raw").expect("raw");
        let root = metadata(0o755, 0);
        let mut empty = Inventory::default();
        empty.set_root(root.clone()).expect("empty root");
        let (lower, target_entry) = lower_inventory(root, &target, old, replacement);
        let first_changes = compare(&empty, &lower).expect("lower diff");
        let first = LayerEncoder::default()
            .encode(layout, &first_changes, &contents)
            .expect("lower Layer");
        let after = final_inventory(&target, target_entry, new, raw);
        let second_changes = compare(&lower, &after).expect("upper diff");
        let second = LayerEncoder::default()
            .encode(layout, &second_changes, &contents)
            .expect("upper Layer");
        let image = image(
            layout,
            vec![first.descriptor, second.descriptor],
            vec![first.diff_id, second.diff_id],
        );
        (image, after)
    }

    fn lower_inventory(
        root: Metadata,
        target: &Digest,
        old: Digest,
        replacement: Digest,
    ) -> (Inventory, FsEntry) {
        let mut lower = Inventory::default();
        lower.set_root(root).expect("lower root");
        lower
            .insert(path(b"dir"), directory(0o700, 1))
            .expect("dir");
        lower
            .insert(path(b"dir/old"), regular(old, 3, 0o640, 2))
            .expect("old");
        lower
            .insert(
                path(b"obsolete"),
                regular(digest_bytes(b"old"), 3, 0o600, 3),
            )
            .expect("obsolete");
        lower
            .insert(path(b"replace"), directory(0o755, 4))
            .expect("replace dir");
        lower
            .insert(path(b"replace/child"), regular(replacement, 5, 0o644, 5))
            .expect("replace child");
        let mut target_entry = regular(target.clone(), 12, 0o650, 6);
        target_entry.metadata.xattrs.insert(
            b"user.binary".to_vec().into_boxed_slice(),
            b"line\nzero\0tail".to_vec().into_boxed_slice(),
        );
        lower
            .insert(path(b"anchor"), target_entry.clone())
            .expect("anchor");
        (lower, target_entry)
    }

    fn final_inventory(
        target: &Digest,
        target_entry: FsEntry,
        new: Digest,
        raw: Digest,
    ) -> Inventory {
        let mut after = Inventory::default();
        let mut final_root = metadata(0o711, 9);
        final_root.xattrs.insert(
            b"user.root".to_vec().into_boxed_slice(),
            b"final".to_vec().into_boxed_slice(),
        );
        after.set_root(final_root).expect("final root");
        let mut final_dir = directory(0o750, 10);
        final_dir.metadata.xattrs.insert(
            b"user.dir".to_vec().into_boxed_slice(),
            b"final\0value".to_vec().into_boxed_slice(),
        );
        after.insert(path(b"dir"), final_dir).expect("final dir");
        after
            .insert(path(b"dir/new"), regular(new, 3, 0o604, 11))
            .expect("new");
        after
            .insert(
                path(b"hard"),
                FsEntry {
                    metadata: target_entry.metadata.clone(),
                    kind: EntryKind::Regular {
                        digest: target.clone(),
                        size: 12,
                        hardlink: Some(path(b"anchor")),
                    },
                },
            )
            .expect("hardlink");
        after
            .insert(path(b"pipe"), special(EntryKind::Fifo, 0o620, 12))
            .expect("fifo");
        after
            .insert(
                path(b"replace"),
                special(
                    EntryKind::Symlink {
                        target: b"anchor".to_vec().into_boxed_slice(),
                    },
                    0o777,
                    13,
                ),
            )
            .expect("replacement symlink");
        after
            .insert(path(b"anchor"), target_entry)
            .expect("final anchor");
        after
            .insert(path(b"raw-\xff"), regular(raw, 3, 0o600, 14))
            .expect("raw path");
        after
    }

    fn image(
        layout: &OciLayout,
        layers: Vec<crate::core::OciDescriptor>,
        diff_ids: Vec<Digest>,
    ) -> ImageView {
        let config = layout
            .put_bytes(b"{}", OCI_IMAGE_CONFIG)
            .expect("config descriptor");
        ImageView {
            manifest: config.clone(),
            config,
            platform: Platform::linux(Architecture::Amd64),
            layers,
            diff_ids,
            parent_manifest: None,
            added_layers: Vec::new(),
        }
    }

    fn raw_image(layout: &OciLayout, layers: &[Vec<u8>]) -> ImageView {
        let descriptors = layers
            .iter()
            .map(|bytes| layout.put_bytes(bytes, OCI_LAYER_TAR).expect("Layer"))
            .collect();
        let diff_ids = layers.iter().map(|bytes| digest_bytes(bytes)).collect();
        image(layout, descriptors, diff_ids)
    }

    struct RawEntry<'a> {
        path: &'a [u8],
        kind: EntryType,
        contents: &'a [u8],
        device: Option<(u32, u32)>,
        link: Option<&'a [u8]>,
    }

    impl<'a> RawEntry<'a> {
        const fn file(path: &'a [u8], contents: &'a [u8]) -> Self {
            Self {
                path,
                kind: EntryType::Regular,
                contents,
                device: None,
                link: None,
            }
        }

        const fn directory(path: &'a [u8]) -> Self {
            Self {
                path,
                kind: EntryType::Directory,
                contents: b"",
                device: None,
                link: None,
            }
        }

        const fn hardlink(path: &'a [u8], target: &'a [u8]) -> Self {
            Self {
                path,
                kind: EntryType::Link,
                contents: b"",
                device: None,
                link: Some(target),
            }
        }

        const fn character(path: &'a [u8], major: u32, minor: u32) -> Self {
            Self {
                path,
                kind: EntryType::Char,
                contents: b"",
                device: Some((major, minor)),
                link: None,
            }
        }
    }

    fn raw_layer(entries: &[RawEntry<'_>]) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut bytes);
            for entry in entries {
                let mut header = Header::new_ustar();
                header.set_entry_type(entry.kind);
                header.set_size(usize_to_u64(entry.contents.len()));
                header.set_mode(if entry.kind == EntryType::Directory {
                    0o755
                } else {
                    0o644
                });
                header.set_uid(0);
                header.set_gid(0);
                header.set_mtime(0);
                header.as_mut_bytes()[..entry.path.len()].copy_from_slice(entry.path);
                if let Some((major, minor)) = entry.device {
                    header.set_device_major(major).expect("device major");
                    header.set_device_minor(minor).expect("device minor");
                }
                if let Some(target) = entry.link {
                    header
                        .set_link_name(OsStr::from_bytes(target))
                        .expect("hardlink target");
                }
                header.set_cksum();
                builder.append(&header, entry.contents).expect("tar entry");
            }
            builder.finish().expect("finish tar");
        }
        bytes
    }

    fn path(bytes: &[u8]) -> FsPath {
        FsPath::from_relative(bytes, 16 * 1024).expect("path")
    }

    fn metadata(mode: u32, seconds: i64) -> Metadata {
        Metadata {
            mode,
            uid: 0,
            gid: 0,
            mtime: Timestamp { seconds, nanos: 0 },
            xattrs: Xattrs::new(),
        }
    }

    fn directory(mode: u32, seconds: i64) -> FsEntry {
        special(EntryKind::Directory, mode, seconds)
    }

    fn regular(digest: Digest, size: u64, mode: u32, seconds: i64) -> FsEntry {
        FsEntry {
            metadata: metadata(mode, seconds),
            kind: EntryKind::Regular {
                digest,
                size,
                hardlink: None,
            },
        }
    }

    fn special(kind: EntryKind, mode: u32, seconds: i64) -> FsEntry {
        FsEntry {
            metadata: metadata(mode, seconds),
            kind,
        }
    }
}
