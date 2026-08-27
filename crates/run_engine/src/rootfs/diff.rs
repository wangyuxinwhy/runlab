use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail};

use super::{EntryKind, FsEntry, FsPath, Inventory, Metadata};

#[derive(Default)]
pub(super) struct ChangeSet {
    pub(super) root: Option<Metadata>,
    pub(super) removals: BTreeSet<FsPath>,
    pub(super) opaques: BTreeSet<FsPath>,
    pub(super) entries: BTreeMap<FsPath, FsEntry>,
}

pub(super) fn compare(before: &Inventory, after: &Inventory) -> Result<ChangeSet> {
    let root = match (&before.root, &after.root) {
        (Some(left), Some(right)) if left != right => Some(right.clone()),
        (Some(_), Some(_)) | (None, None) => None,
        _ => bail!("filesystem inventories disagree about root metadata"),
    };
    let all_paths = before
        .entries
        .keys()
        .chain(after.entries.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut changes = ChangeSet {
        root,
        ..ChangeSet::default()
    };
    for path in all_paths {
        match (before.entries.get(&path), after.entries.get(&path)) {
            (Some(left), Some(right)) if left == right => {}
            (_, Some(right)) => {
                changes.entries.insert(path, right.clone());
            }
            (Some(_), None) => insert_removal(&mut changes.removals, path),
            (None, None) => unreachable!(),
        }
    }
    select_opaque_removals(before, after, &mut changes);
    promote_changed_hardlink_anchors(after, &mut changes.entries)?;
    Ok(changes)
}

pub(super) fn compare_overlay(
    before: &Inventory,
    observed: &Inventory,
    removals: &BTreeSet<FsPath>,
    opaques: &BTreeSet<FsPath>,
) -> Result<ChangeSet> {
    let root = match (&before.root, &observed.root) {
        (Some(left), Some(right)) if left != right => Some(right.clone()),
        (Some(_), Some(_)) => None,
        _ => bail!("filesystem inventories disagree about root metadata"),
    };
    let mut changes = ChangeSet {
        root,
        ..ChangeSet::default()
    };
    for (path, entry) in &observed.entries {
        if before.entries.get(path) != Some(entry) {
            changes.entries.insert(path.clone(), entry.clone());
        }
    }
    for path in removals {
        if before.entries.contains_key(path) {
            insert_removal(&mut changes.removals, path.clone());
        }
    }
    for directory in opaques {
        if before
            .entries
            .keys()
            .any(|path| path.is_descendant_of(directory))
        {
            changes.opaques.insert(directory.clone());
            changes
                .removals
                .retain(|path| !path.is_descendant_of(directory));
        }
    }
    promote_changed_hardlink_anchors(observed, &mut changes.entries)?;
    Ok(changes)
}

pub(super) fn insert_removal(removals: &mut BTreeSet<FsPath>, path: FsPath) {
    if removals
        .iter()
        .any(|parent| path == *parent || path.is_descendant_of(parent))
    {
        return;
    }
    removals.retain(|child| !child.is_descendant_of(&path));
    removals.insert(path);
}

pub(super) fn select_opaque_removals(
    before: &Inventory,
    after: &Inventory,
    changes: &mut ChangeSet,
) {
    for (directory, entry) in &after.entries {
        if !matches!(entry.kind, EntryKind::Directory)
            || !matches!(
                before.entries.get(directory).map(|entry| &entry.kind),
                Some(EntryKind::Directory)
            )
        {
            continue;
        }
        let had_children = before
            .entries
            .keys()
            .any(|path| path.is_descendant_of(directory));
        let retained_old_child = before
            .entries
            .keys()
            .any(|path| path.is_descendant_of(directory) && after.entries.contains_key(path));
        if had_children && !retained_old_child {
            changes.opaques.insert(directory.clone());
            changes
                .removals
                .retain(|path| !path.is_descendant_of(directory));
        }
    }
}

pub(super) fn promote_changed_hardlink_anchors(
    after: &Inventory,
    entries: &mut BTreeMap<FsPath, FsEntry>,
) -> Result<()> {
    let anchors = entries
        .values()
        .filter_map(|entry| match &entry.kind {
            EntryKind::Regular {
                hardlink: Some(anchor),
                ..
            } => Some(anchor.clone()),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    for anchor in anchors {
        let entry = after
            .entries
            .get(&anchor)
            .with_context(|| format!("hardlink anchor is absent: {}", anchor.display()))?;
        entries.insert(anchor, entry.clone());
    }
    Ok(())
}
