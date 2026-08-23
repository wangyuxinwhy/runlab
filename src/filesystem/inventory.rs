use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};

use crate::core::Digest;

use super::FsPath;

pub(crate) type Xattrs = BTreeMap<Box<[u8]>, Box<[u8]>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Timestamp {
    pub(crate) seconds: i64,
    pub(crate) nanos: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Metadata {
    pub(crate) mode: u32,
    pub(crate) uid: u32,
    pub(crate) gid: u32,
    pub(crate) mtime: Timestamp,
    pub(crate) xattrs: Xattrs,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EntryKind {
    Regular {
        digest: Digest,
        size: u64,
        hardlink: Option<FsPath>,
    },
    Directory,
    Symlink {
        target: Box<[u8]>,
    },
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FsEntry {
    pub(crate) metadata: Metadata,
    pub(crate) kind: EntryKind,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Inventory {
    root: Option<Metadata>,
    entries: BTreeMap<FsPath, FsEntry>,
}

impl Inventory {
    #[cfg_attr(
        not(any(test, target_os = "linux")),
        allow(dead_code, reason = "production filesystem capture is Linux-only")
    )]
    pub(crate) fn set_root(&mut self, metadata: Metadata) -> Result<()> {
        if self.root.replace(metadata).is_some() {
            bail!("filesystem inventory root metadata was already set");
        }
        Ok(())
    }

    pub(crate) fn root(&self) -> Option<&Metadata> {
        self.root.as_ref()
    }

    #[cfg_attr(
        not(any(test, target_os = "linux")),
        allow(dead_code, reason = "production filesystem capture is Linux-only")
    )]
    pub(crate) fn insert(&mut self, path: FsPath, entry: FsEntry) -> Result<()> {
        if path.is_root() {
            bail!("filesystem inventory cannot contain the root path");
        }
        let display = path.display();
        if self.entries.insert(path, entry).is_some() {
            bail!("duplicate filesystem inventory path: {display}");
        }
        Ok(())
    }

    pub(crate) fn get(&self, path: &FsPath) -> Option<&FsEntry> {
        self.entries.get(path)
    }

    #[cfg_attr(
        not(any(test, target_os = "linux")),
        allow(dead_code, reason = "production filesystem capture is Linux-only")
    )]
    pub(crate) fn get_mut(&mut self, path: &FsPath) -> Option<&mut FsEntry> {
        self.entries.get_mut(path)
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&FsPath, &FsEntry)> {
        self.entries.iter()
    }

    pub(crate) fn validate(&self) -> Result<()> {
        if let Some(root) = &self.root {
            validate_metadata(root, "/")?;
        }
        for (path, entry) in &self.entries {
            validate_metadata(&entry.metadata, &path.display())?;
            validate_parent_directories(self, path)?;
            match &entry.kind {
                EntryKind::Regular {
                    digest,
                    size,
                    hardlink: Some(target),
                } => validate_hardlink(self, path, entry, target, digest, *size)?,
                EntryKind::Symlink { target } if target.contains(&0) => {
                    bail!("symlink target contains NUL: {}", path.display());
                }
                _ => {}
            }
        }
        Ok(())
    }
}

fn validate_metadata(metadata: &Metadata, path: &str) -> Result<()> {
    if metadata.mode > 0o7777 {
        bail!("filesystem mode exceeds permission and special bits at {path}");
    }
    if metadata.mtime.nanos >= 1_000_000_000 {
        bail!("filesystem mtime nanoseconds are out of range at {path}");
    }
    for name in metadata.xattrs.keys() {
        if name.is_empty() || name.len() > 255 || name.contains(&0) {
            bail!("filesystem xattr name is invalid at {path}");
        }
    }
    Ok(())
}

fn validate_parent_directories(inventory: &Inventory, path: &FsPath) -> Result<()> {
    let mut parent = path.parent();
    while !parent.is_root() {
        let entry = inventory
            .get(&parent)
            .with_context(|| format!("filesystem parent is absent: {}", parent.display()))?;
        if !matches!(entry.kind, EntryKind::Directory) {
            bail!("filesystem parent is not a directory: {}", parent.display());
        }
        parent = parent.parent();
    }
    Ok(())
}

fn validate_hardlink(
    inventory: &Inventory,
    path: &FsPath,
    follower: &FsEntry,
    target: &FsPath,
    digest: &Digest,
    size: u64,
) -> Result<()> {
    if target >= path {
        bail!(
            "hardlink target must be the raw-byte group anchor: {} -> {}",
            path.display(),
            target.display()
        );
    }
    let anchor = inventory
        .get(target)
        .with_context(|| format!("hardlink target is absent: {}", target.display()))?;
    let EntryKind::Regular {
        digest: anchor_digest,
        size: anchor_size,
        hardlink: None,
    } = &anchor.kind
    else {
        bail!(
            "hardlink target is not a group anchor: {}",
            target.display()
        );
    };
    if anchor_digest != digest || *anchor_size != size || anchor.metadata != follower.metadata {
        bail!(
            "hardlink group metadata or content disagrees: {} -> {}",
            path.display(),
            target.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_rejects_missing_parent_and_invalid_timestamp() {
        let mut missing_parent = Inventory::default();
        missing_parent
            .insert(path(b"missing/value"), regular(None))
            .expect("entry");
        assert!(
            missing_parent
                .validate()
                .expect_err("missing parent")
                .to_string()
                .contains("parent is absent")
        );

        let mut invalid_timestamp = Inventory::default();
        let mut entry = regular(None);
        entry.metadata.mtime.nanos = 1_000_000_000;
        invalid_timestamp
            .insert(path(b"value"), entry)
            .expect("entry");
        assert!(
            invalid_timestamp
                .validate()
                .expect_err("timestamp")
                .to_string()
                .contains("nanoseconds")
        );

        let mut invalid_xattr = Inventory::default();
        let mut entry = regular(None);
        entry
            .metadata
            .xattrs
            .insert(vec![b'x'; 256].into_boxed_slice(), Box::new([]));
        invalid_xattr.insert(path(b"value"), entry).expect("entry");
        assert!(
            invalid_xattr
                .validate()
                .expect_err("xattr name")
                .to_string()
                .contains("xattr name")
        );
    }

    #[test]
    fn validation_rejects_inconsistent_hardlink_group() {
        let mut inventory = Inventory::default();
        inventory
            .insert(path(b"anchor"), regular(None))
            .expect("anchor");
        let mut follower = regular(Some(path(b"anchor")));
        let EntryKind::Regular { size, .. } = &mut follower.kind else {
            unreachable!("the fixture above is a regular file")
        };
        *size = 4;
        inventory
            .insert(path(b"follower"), follower)
            .expect("follower");
        assert!(
            inventory
                .validate()
                .expect_err("hardlink mismatch")
                .to_string()
                .contains("hardlink group")
        );
    }

    fn path(bytes: &[u8]) -> FsPath {
        FsPath::from_relative(bytes, 1024).expect("path")
    }

    fn regular(hardlink: Option<FsPath>) -> FsEntry {
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
                digest: Digest::parse(format!("sha256:{}", "a".repeat(64))).expect("digest"),
                size: 3,
                hardlink,
            },
        }
    }
}
