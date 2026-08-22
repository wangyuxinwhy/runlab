use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail};

use crate::filesystem::{EntryKind, FsEntry, FsPath, Inventory, Metadata};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ChangeSet {
    root: Option<Metadata>,
    removals: BTreeSet<FsPath>,
    entries: BTreeMap<FsPath, FsEntry>,
}

impl ChangeSet {
    pub(crate) fn merged(root: Option<Metadata>, entries: BTreeMap<FsPath, FsEntry>) -> Self {
        Self {
            root,
            removals: BTreeSet::new(),
            entries,
        }
    }

    pub(crate) fn removals(&self) -> impl Iterator<Item = &FsPath> {
        self.removals.iter()
    }

    pub(crate) fn root(&self) -> Option<&Metadata> {
        self.root.as_ref()
    }

    pub(crate) fn entries(&self) -> impl Iterator<Item = (&FsPath, &FsEntry)> {
        self.entries.iter()
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.root.is_none() && self.removals.is_empty() && self.entries.is_empty()
    }
}

#[cfg_attr(
    not(any(test, target_os = "linux")),
    allow(dead_code, reason = "production tree comparison is Linux-only")
)]
pub(crate) fn compare(before: &Inventory, after: &Inventory) -> Result<ChangeSet> {
    before
        .validate()
        .context("invalid before filesystem inventory")?;
    after
        .validate()
        .context("invalid after filesystem inventory")?;
    let root = match (before.root(), after.root()) {
        (Some(left), Some(right)) if left != right => Some(right.clone()),
        (Some(_), Some(_)) | (None, None) => None,
        _ => {
            return Err(anyhow::anyhow!(
                "filesystem inventories disagree about root metadata"
            ));
        }
    };
    let paths = before
        .iter()
        .map(|(path, _)| path.clone())
        .chain(after.iter().map(|(path, _)| path.clone()))
        .collect::<BTreeSet<_>>();
    let mut changes = ChangeSet {
        root,
        ..ChangeSet::default()
    };
    for path in paths {
        match (before.get(&path), after.get(&path)) {
            (Some(left), Some(right)) if left == right => {}
            (None | Some(_), Some(right)) => {
                changes.entries.insert(path, right.clone());
            }
            (Some(_), None) => {
                if !replaced_directory_ancestor(before, after, &path) {
                    insert_removal(&mut changes.removals, path);
                }
            }
            (None, None) => unreachable!("path came from one of the inventories"),
        }
    }
    changes.entries.retain(|path, _| after.get(path).is_some());
    promote_hardlink_anchors(after, &mut changes.entries)?;
    Ok(changes)
}

fn replaced_directory_ancestor(before: &Inventory, after: &Inventory, path: &FsPath) -> bool {
    let mut ancestor = path.parent();
    while !ancestor.is_root() {
        if matches!(
            before.get(&ancestor).map(|entry| &entry.kind),
            Some(EntryKind::Directory)
        ) && matches!(
            after.get(&ancestor).map(|entry| &entry.kind),
            Some(kind) if !matches!(kind, EntryKind::Directory)
        ) {
            return true;
        }
        ancestor = ancestor.parent();
    }
    false
}

fn promote_hardlink_anchors(
    after: &Inventory,
    entries: &mut BTreeMap<FsPath, FsEntry>,
) -> Result<()> {
    let followers = entries
        .iter()
        .filter_map(|(path, entry)| match &entry.kind {
            EntryKind::Regular {
                digest,
                size,
                hardlink: Some(target),
            } => Some((
                path.clone(),
                target.clone(),
                digest.clone(),
                *size,
                entry.metadata.clone(),
            )),
            _ => None,
        })
        .collect::<Vec<_>>();
    for (path, target, digest, size, metadata) in followers {
        if target >= path {
            bail!(
                "hardlink target must be the raw-byte group anchor: {} -> {}",
                path.display(),
                target.display()
            );
        }
        let anchor = after
            .get(&target)
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
        if anchor_digest != &digest || *anchor_size != size || anchor.metadata != metadata {
            bail!(
                "hardlink group metadata or content disagrees: {} -> {}",
                path.display(),
                target.display()
            );
        }
        entries.insert(target, anchor.clone());
    }
    Ok(())
}

fn insert_removal(removals: &mut BTreeSet<FsPath>, path: FsPath) {
    if removals
        .iter()
        .any(|ancestor| path == *ancestor || path.is_descendant_of(ancestor))
    {
        return;
    }
    removals.retain(|descendant| !descendant.is_descendant_of(&path));
    removals.insert(path);
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::core::Digest;
    use crate::filesystem::{EntryKind, Metadata, Timestamp};

    #[test]
    fn comparison_is_raw_ordered_and_suppresses_descendant_removals() {
        let mut before = Inventory::default();
        before.insert(path(b"gone"), directory()).expect("gone");
        before
            .insert(path(b"gone/child"), regular('1', 1))
            .expect("child");
        before
            .insert(path(b"changed"), regular('2', 2))
            .expect("changed");
        let mut after = Inventory::default();
        after
            .insert(path(b"changed"), regular('3', 3))
            .expect("changed");
        after
            .insert(path(b"new-\xff"), regular('4', 4))
            .expect("raw");
        let changes = compare(&before, &after).expect("compare");
        assert_eq!(
            changes.removals().map(FsPath::as_bytes).collect::<Vec<_>>(),
            vec![b"gone".as_slice()]
        );
        assert_eq!(
            changes
                .entries()
                .map(|(path, _)| path.as_bytes())
                .collect::<Vec<_>>(),
            vec![b"changed".as_slice(), b"new-\xff".as_slice()]
        );
    }

    #[test]
    fn directory_replacement_does_not_emit_impossible_descendant_whiteouts() {
        let mut before = Inventory::default();
        before
            .insert(path(b"node"), directory())
            .expect("directory");
        before
            .insert(path(b"node/child"), regular('1', 1))
            .expect("child");
        let mut after = Inventory::default();
        after
            .insert(path(b"node"), regular('2', 2))
            .expect("replacement");
        let changes = compare(&before, &after).expect("compare");
        assert!(changes.removals().next().is_none());
        assert_eq!(
            changes
                .entries()
                .map(|(path, _)| path.as_bytes())
                .collect::<Vec<_>>(),
            vec![b"node".as_slice()]
        );
    }

    #[test]
    fn changed_hardlink_group_promotes_and_validates_its_anchor() {
        let digest = Digest::parse(format!("sha256:{}", "a".repeat(64))).expect("digest");
        let mut after = Inventory::default();
        after
            .insert(
                path(b"anchor"),
                FsEntry {
                    metadata: metadata(),
                    kind: EntryKind::Regular {
                        digest: digest.clone(),
                        size: 3,
                        hardlink: None,
                    },
                },
            )
            .expect("anchor");
        after
            .insert(
                path(b"follower"),
                FsEntry {
                    metadata: metadata(),
                    kind: EntryKind::Regular {
                        digest,
                        size: 3,
                        hardlink: Some(path(b"anchor")),
                    },
                },
            )
            .expect("follower");
        let changes = compare(&Inventory::default(), &after).expect("compare");
        assert_eq!(
            changes
                .entries()
                .map(|(path, _)| path.as_bytes())
                .collect::<Vec<_>>(),
            vec![b"anchor".as_slice(), b"follower".as_slice()]
        );
    }

    fn path(bytes: &[u8]) -> FsPath {
        FsPath::from_relative(bytes, 1024).expect("path")
    }

    fn metadata() -> Metadata {
        Metadata {
            mode: 0o644,
            uid: 0,
            gid: 0,
            mtime: Timestamp {
                seconds: 0,
                nanos: 0,
            },
            xattrs: BTreeMap::new(),
        }
    }

    fn directory() -> FsEntry {
        FsEntry {
            metadata: metadata(),
            kind: EntryKind::Directory,
        }
    }

    fn regular(digit: char, size: u64) -> FsEntry {
        FsEntry {
            metadata: metadata(),
            kind: EntryKind::Regular {
                digest: Digest::parse(format!("sha256:{}", digit.to_string().repeat(64)))
                    .expect("digest"),
                size,
                hardlink: None,
            },
        }
    }
}
