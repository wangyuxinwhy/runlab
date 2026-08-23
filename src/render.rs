//! Reading verified Layers into a filesystem view.
//!
//! Applies Layers in order with OCI whiteout semantics and resolves hardlinks,
//! producing a view that can be listed, diffed, or streamed a file at a time
//! without materializing anything. `materialize` is the separate step that
//! writes such a view to disk.
//!
//! Every traversal is bounded: `RenderLimits` caps entry counts, path depth and
//! sizes so a hostile Image cannot exhaust memory or time.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::File;
use std::io::{Read, Write};

use anyhow::{Context, Result, bail};
use flate2::read::MultiGzDecoder;
use schemars::JsonSchema;
use serde::Serialize;
use sha2::{Digest as _, Sha256};
use tar::{Archive, EntryType};
use thiserror::Error;

use crate::core::{
    Digest, ImageView, OCI_LAYER_GZIP, OCI_LAYER_TAR, OCI_LAYER_ZSTD, OciDescriptor,
};
use crate::filesystem::pax::{self, PaxError, PaxRecords, TarPaxIndex, TarPaxLimits};
use crate::filesystem::{
    ContentStore, EntryKind, FilesystemTarWriter, FsEntry, FsPath, FsPathError, Metadata,
};
use crate::integrity::digest_reader;
use crate::integrity::finish_sha256;
use crate::oci::{MAX_IMAGE_LAYERS, OciLayout};

const COPY_BUFFER_BYTES: usize = 1024 * 1024;

#[cfg(test)]
thread_local! {
    static MATERIALIZATION_CONTENT_PASSES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[derive(Debug, Clone, Copy)]
pub struct RenderLimits {
    pub layers: u64,
    pub entries: u64,
    pub total_uncompressed_bytes: u64,
    pub entry_bytes: u64,
    pub path_bytes: u64,
    pub link_target_bytes: u64,
    pub pax_bytes: u64,
    pub pax_index_bytes: u64,
    pub view_bytes: u64,
    pub pending_hardlinks: u64,
    pub symlink_hops: u64,
    #[cfg(target_os = "linux")]
    pub cleanup_entries: u64,
    #[cfg(target_os = "linux")]
    pub cleanup_depth: u64,
}

impl Default for RenderLimits {
    fn default() -> Self {
        Self {
            layers: MAX_IMAGE_LAYERS as u64,
            entries: 1_000_000,
            total_uncompressed_bytes: 64 * 1024 * 1024 * 1024,
            entry_bytes: 64 * 1024 * 1024 * 1024,
            path_bytes: 16 * 1024,
            link_target_bytes: 16 * 1024,
            pax_bytes: 1024 * 1024,
            pax_index_bytes: 64 * 1024 * 1024,
            view_bytes: 512 * 1024 * 1024,
            pending_hardlinks: 1_000_000,
            symlink_hops: 40,
            #[cfg(target_os = "linux")]
            cleanup_entries: 1_000_000,
            #[cfg(target_os = "linux")]
            cleanup_depth: 1024,
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum RenderError {
    #[error("unsafe OCI Layer path: {path}")]
    UnsafePath { path: String },
    #[error("duplicate OCI Layer path: {path}")]
    DuplicatePath { path: String },
    #[error("invalid OCI whiteout: {path}")]
    InvalidWhiteout { path: String },
    #[error("OCI Layer path has a non-directory ancestor: {path}")]
    NonDirectoryAncestor { path: String },
    #[error("unresolved OCI hardlink: {path} -> {target}")]
    UnresolvedHardlink { path: String, target: String },
    #[error("unsupported OCI Layer entry type {kind} at {path}")]
    UnsupportedEntry { path: String, kind: String },
    #[error("image path does not exist: {path}")]
    MissingPath { path: String },
    #[error("image path is not a regular file: {path} ({kind})")]
    NotRegular { path: String, kind: String },
    #[error("image symlink resolution exceeded {limit} hops: {path}")]
    SymlinkLimit { path: String, limit: u64 },
    #[error("render limit {name} exceeded: limit {limit}, observed {observed}")]
    LimitExceeded {
        name: &'static str,
        limit: u64,
        observed: u64,
    },
}

type ImagePath = FsPath;

#[derive(Debug, Clone)]
pub(crate) struct EntryLocation {
    pub(crate) descriptor: OciDescriptor,
    pub(crate) ordinal: u64,
    pub(crate) path: ImagePath,
    pub(crate) size: u64,
    pub(crate) content_digest: Option<Digest>,
}

#[derive(Debug, Clone)]
struct ResolvedRegular {
    location: EntryLocation,
    metadata: Metadata,
}

#[derive(Debug, Clone)]
enum NodeKind {
    Regular(EntryLocation),
    Directory,
    Symlink(Vec<u8>),
    PendingHardlink(ImagePath),
    Character { major: u32, minor: u32 },
    Block { major: u32, minor: u32 },
    Fifo,
}

impl NodeKind {
    fn name(&self) -> &'static str {
        match self {
            Self::Regular(_) => "regular",
            Self::Directory => "directory",
            Self::Symlink(_) => "symlink",
            Self::PendingHardlink(_) => "hardlink",
            Self::Character { .. } => "character-device",
            Self::Block { .. } => "block-device",
            Self::Fifo => "fifo",
        }
    }
}

#[derive(Debug, Clone)]
struct FsNode {
    kind: NodeKind,
    metadata: Metadata,
}

#[derive(Debug, Clone)]
pub(crate) enum LayerEntryKind {
    Regular(EntryLocation),
    Directory,
    Symlink(Vec<u8>),
    Hardlink(ImagePath),
    Character { major: u32, minor: u32 },
    Block { major: u32, minor: u32 },
    Fifo,
}

/// Where a planned entry's content sits in its Layer.
///
/// The content pass reads bytes, so it only ever visits regular files. Planning
/// in terms of the location rather than the entry is what states that in the
/// type and keeps the pass free of arms it can never reach.
pub(crate) fn regular_location(entry: &LayerEntry) -> Result<&EntryLocation> {
    let LayerEntryKind::Regular(location) = &entry.kind else {
        bail!(
            "OCI Layer content plan contains a non-regular entry: {}",
            display_path(&entry.path)
        );
    };
    Ok(location)
}

#[derive(Debug, Clone)]
pub(crate) struct LayerEntry {
    pub(crate) path: ImagePath,
    pub(crate) kind: LayerEntryKind,
    pub(crate) metadata: Metadata,
}

#[derive(Debug, Default)]
pub(crate) struct LayerPlan {
    pub(crate) whiteouts: Vec<ImagePath>,
    pub(crate) opaques: Vec<ImagePath>,
    pub(crate) entries: Vec<LayerEntry>,
}

#[derive(Debug, Clone, Copy)]
struct LayerScanOptions {
    limits: RenderLimits,
    hash_regular_contents: bool,
}

#[derive(Debug, Default)]
struct FilesystemView {
    nodes: BTreeMap<ImagePath, FsNode>,
    retained_bytes: u64,
}

impl FilesystemView {
    #[cfg(target_os = "linux")]
    fn verify_regular_path_without_symlinks(
        &self,
        requested: &ImagePath,
        limits: RenderLimits,
    ) -> Result<()> {
        let mut components = requested.components().peekable();
        let mut current = FsPath::from_relative(b"", limits.path_bytes)
            .context("resolver projection path is invalid")?;
        while let Some(component) = components.next() {
            current = current
                .join_component(component, limits.path_bytes)
                .context("resolver projection path is invalid")?;
            let last = components.peek().is_none();
            match (self.nodes.get(&current).map(|node| &node.kind), last) {
                (Some(NodeKind::Directory), false) | (Some(NodeKind::Regular(_)), true) => {}
                (None, false) if self.has_descendant(&current) => {}
                (Some(NodeKind::Symlink(_)), _) => bail!(
                    "resolver projection path contains a symbolic link in the Initial Image: {}",
                    display_path(&current)
                ),
                (Some(_), true) => bail!(
                    "resolver projection target is not a regular file in the Initial Image: {}",
                    display_path(requested)
                ),
                (Some(_), false) => bail!(
                    "resolver projection parent is not a directory in the Initial Image: {}",
                    display_path(&current)
                ),
                (None, _) => bail!(
                    "resolver projection target is absent from the Initial Image: {}; missing {}",
                    display_path(requested),
                    display_path(&current)
                ),
            }
        }
        Ok(())
    }

    fn apply(&mut self, plan: LayerPlan, limits: RenderLimits) -> Result<()> {
        for path in plan.whiteouts {
            self.ensure_directory_ancestors(&path)?;
            self.remove_subtree(&path)?;
        }
        for path in plan.opaques {
            self.ensure_directory_ancestors(&path)?;
            self.remove_descendants(&path)?;
        }

        for entry in plan.entries {
            self.ensure_directory_ancestors(&entry.path)?;
            let node = match entry.kind {
                LayerEntryKind::Regular(location) => FsNode {
                    kind: NodeKind::Regular(location),
                    metadata: entry.metadata,
                },
                LayerEntryKind::Directory => FsNode {
                    kind: NodeKind::Directory,
                    metadata: entry.metadata,
                },
                LayerEntryKind::Symlink(target) => FsNode {
                    kind: NodeKind::Symlink(target),
                    metadata: entry.metadata,
                },
                LayerEntryKind::Hardlink(target) => match self.nodes.get(&target) {
                    Some(node) if !matches!(node.kind, NodeKind::Directory) => node.clone(),
                    _ => FsNode {
                        kind: NodeKind::PendingHardlink(target),
                        metadata: entry.metadata,
                    },
                },
                LayerEntryKind::Character { major, minor } => FsNode {
                    kind: NodeKind::Character { major, minor },
                    metadata: entry.metadata,
                },
                LayerEntryKind::Block { major, minor } => FsNode {
                    kind: NodeKind::Block { major, minor },
                    metadata: entry.metadata,
                },
                LayerEntryKind::Fifo => FsNode {
                    kind: NodeKind::Fifo,
                    metadata: entry.metadata,
                },
            };
            let previous = self.nodes.get(&entry.path).map(|node| &node.kind);
            let merges_directory = matches!(node.kind, NodeKind::Directory)
                && (entry.path.is_root()
                    || matches!(previous, Some(NodeKind::Directory))
                    || (previous.is_none() && self.has_descendant(&entry.path)));
            if !merges_directory {
                self.remove_subtree(&entry.path)?;
            }
            self.insert_node(entry.path, node, limits)?;
        }

        let pending = self
            .nodes
            .iter()
            .filter_map(|(path, node)| {
                matches!(node.kind, NodeKind::PendingHardlink(_)).then_some(path.clone())
            })
            .collect::<Vec<_>>();
        enforce_limit(
            "max_pending_hardlinks",
            limits.pending_hardlinks,
            usize_to_u64(pending.len()),
        )?;
        self.resolve_pending_hardlinks(pending, limits)
    }

    fn resolve_pending_hardlinks(
        &mut self,
        pending: Vec<ImagePath>,
        limits: RenderLimits,
    ) -> Result<()> {
        for origin in pending {
            if !matches!(
                self.nodes.get(&origin).map(|node| &node.kind),
                Some(NodeKind::PendingHardlink(_))
            ) {
                continue;
            }
            let mut chain = Vec::new();
            let mut visiting = BTreeSet::new();
            let mut current = origin;
            let resolved = loop {
                let node = self
                    .nodes
                    .get(&current)
                    .ok_or_else(|| RenderError::MissingPath {
                        path: display_path(&current),
                    })?;
                match &node.kind {
                    NodeKind::PendingHardlink(target) => {
                        if !visiting.insert(current.clone()) {
                            return Err(RenderError::UnresolvedHardlink {
                                path: display_path(&current),
                                target: display_path(target),
                            }
                            .into());
                        }
                        chain.push(current);
                        enforce_limit(
                            "max_pending_hardlinks",
                            limits.pending_hardlinks,
                            usize_to_u64(chain.len()),
                        )?;
                        let Some(target_node) = self.nodes.get(target) else {
                            let path = chain.last().expect("hardlink chain is not empty");
                            return Err(RenderError::UnresolvedHardlink {
                                path: display_path(path),
                                target: display_path(target),
                            }
                            .into());
                        };
                        if matches!(target_node.kind, NodeKind::Directory) {
                            let path = chain.last().expect("hardlink chain is not empty");
                            return Err(RenderError::UnresolvedHardlink {
                                path: display_path(path),
                                target: display_path(target),
                            }
                            .into());
                        }
                        current = target.clone();
                    }
                    NodeKind::Directory => {
                        unreachable!(
                            "a directory target is rejected before the chain advances to it"
                        )
                    }
                    _ => break node.clone(),
                }
            };
            for path in chain.into_iter().rev() {
                self.insert_node(path, resolved.clone(), limits)?;
            }
        }
        Ok(())
    }

    fn regular_location(
        &self,
        request: &ImagePath,
        limits: RenderLimits,
    ) -> Result<ResolvedRegular> {
        let resolved = self.resolve_symlinks(request, limits)?;
        let node = self
            .nodes
            .get(&resolved)
            .ok_or_else(|| RenderError::MissingPath {
                path: display_path(request),
            })?;
        match &node.kind {
            NodeKind::Regular(location) => Ok(ResolvedRegular {
                location: location.clone(),
                metadata: node.metadata.clone(),
            }),
            kind => Err(RenderError::NotRegular {
                path: display_path(request),
                kind: kind.name().to_owned(),
            }
            .into()),
        }
    }

    fn resolve_symlinks(&self, request: &ImagePath, limits: RenderLimits) -> Result<ImagePath> {
        let mut pending = request
            .components()
            .map(<[u8]>::to_vec)
            .collect::<VecDeque<_>>();
        let mut resolved = Vec::<Vec<u8>>::new();
        let mut hops = 0_u64;
        while let Some(component) = pending.pop_front() {
            let candidate = path_from_components(
                &resolved
                    .iter()
                    .cloned()
                    .chain(std::iter::once(component.clone()))
                    .collect::<Vec<_>>(),
                limits,
            )?;
            match self.nodes.get(&candidate).map(|node| &node.kind) {
                Some(NodeKind::Symlink(target)) => {
                    hops = hops.checked_add(1).context("symlink hop overflow")?;
                    if hops > limits.symlink_hops {
                        return Err(RenderError::SymlinkLimit {
                            path: display_path(request),
                            limit: limits.symlink_hops,
                        }
                        .into());
                    }
                    let mut replacement = if target.starts_with(b"/") {
                        Vec::new()
                    } else {
                        resolved.clone()
                    };
                    apply_link_components(&mut replacement, target);
                    replacement.extend(pending);
                    pending = replacement.into();
                    resolved.clear();
                }
                Some(NodeKind::Directory) => resolved.push(component),
                Some(kind) if pending.is_empty() => {
                    resolved.push(component);
                    if matches!(kind, NodeKind::PendingHardlink(_)) {
                        bail!(
                            "internal unresolved hardlink at {}",
                            display_path(&candidate)
                        );
                    }
                }
                Some(kind) => {
                    return Err(RenderError::NotRegular {
                        path: display_path(request),
                        kind: kind.name().to_owned(),
                    }
                    .into());
                }
                None if self.has_descendant(&candidate) => resolved.push(component),
                None => {
                    return Err(RenderError::MissingPath {
                        path: display_path(request),
                    }
                    .into());
                }
            }
        }
        Ok(path_from_components(&resolved, limits)?)
    }

    fn ensure_directory_ancestors(&self, path: &ImagePath) -> Result<()> {
        let components = path.components().map(<[u8]>::to_vec).collect::<Vec<_>>();
        for end in 1..components.len() {
            let ancestor = FsPath::from_normalized_components(
                &components[..end],
                usize_to_u64(path.as_bytes().len()),
            )?;
            if let Some(node) = self.nodes.get(&ancestor)
                && !matches!(node.kind, NodeKind::Directory)
            {
                return Err(RenderError::NonDirectoryAncestor {
                    path: display_path(path),
                }
                .into());
            }
        }
        Ok(())
    }

    fn has_descendant(&self, path: &ImagePath) -> bool {
        if path.is_root() {
            return self.nodes.keys().any(|candidate| !candidate.is_root());
        }
        let (start, end) = descendant_bounds(path);
        self.nodes.range(start..end).next().is_some()
    }

    fn insert_node(&mut self, path: ImagePath, node: FsNode, limits: RenderLimits) -> Result<()> {
        let removed = self
            .nodes
            .get(&path)
            .map(|previous| retained_node_bytes(&path, previous))
            .transpose()?
            .unwrap_or(0);
        let added = retained_node_bytes(&path, &node)?;
        let retained_bytes = self
            .retained_bytes
            .checked_sub(removed)
            .and_then(|bytes| bytes.checked_add(added))
            .context("filesystem view retained byte count overflow")?;
        enforce_limit("max_view_bytes", limits.view_bytes, retained_bytes)?;
        self.nodes.insert(path, node);
        self.retained_bytes = retained_bytes;
        Ok(())
    }

    fn remove_subtree(&mut self, path: &ImagePath) -> Result<()> {
        let mut removed = self.descendant_paths(path);
        if self.nodes.contains_key(path) {
            removed.push(path.clone());
        }
        self.remove_nodes(removed)
    }

    fn remove_descendants(&mut self, path: &ImagePath) -> Result<()> {
        let removed = self.descendant_paths(path);
        self.remove_nodes(removed)
    }

    fn descendant_paths(&self, path: &ImagePath) -> Vec<ImagePath> {
        if path.is_root() {
            return self
                .nodes
                .keys()
                .filter(|candidate| !candidate.is_root())
                .cloned()
                .collect();
        }
        let (start, end) = descendant_bounds(path);
        self.nodes
            .range(start..end)
            .map(|(candidate, _)| candidate.clone())
            .collect()
    }

    fn remove_nodes(&mut self, paths: Vec<ImagePath>) -> Result<()> {
        for path in paths {
            let node = self
                .nodes
                .remove(&path)
                .context("filesystem view removal lost a node")?;
            self.retained_bytes = self
                .retained_bytes
                .checked_sub(retained_node_bytes(&path, &node)?)
                .context("filesystem view retained byte count underflow")?;
        }
        Ok(())
    }
}

pub(crate) fn descendant_bounds(path: &ImagePath) -> (ImagePath, ImagePath) {
    let mut start = path.as_bytes().to_vec();
    start.extend_from_slice(b"/\x01");
    let mut end = path.as_bytes().to_vec();
    end.push(b'0');
    (
        FsPath::from_relative(&start, u64::MAX).expect("descendant lower bound is valid"),
        FsPath::from_relative(&end, u64::MAX).expect("descendant upper bound is valid"),
    )
}

fn retained_node_bytes(path: &ImagePath, node: &FsNode) -> Result<u64> {
    let mut bytes = usize_to_u64(path.as_bytes().len());
    for (name, value) in &node.metadata.xattrs {
        bytes = bytes
            .checked_add(usize_to_u64(name.len()))
            .and_then(|bytes| bytes.checked_add(usize_to_u64(value.len())))
            .context("filesystem view retained byte count overflow")?;
    }
    let kind_bytes = match &node.kind {
        NodeKind::Regular(location) => usize_to_u64(location.path.as_bytes().len())
            .checked_add(usize_to_u64(location.descriptor.digest.as_str().len()))
            .and_then(|bytes| {
                bytes.checked_add(usize_to_u64(location.descriptor.media_type.len()))
            }),
        NodeKind::Symlink(target) => Some(usize_to_u64(target.len())),
        NodeKind::PendingHardlink(target) => Some(usize_to_u64(target.as_bytes().len())),
        NodeKind::Directory
        | NodeKind::Character { .. }
        | NodeKind::Block { .. }
        | NodeKind::Fifo => Some(0),
    }
    .context("filesystem view retained byte count overflow")?;
    bytes
        .checked_add(kind_bytes)
        .context("filesystem view retained byte count overflow")
}

#[derive(Debug, Clone)]
pub struct ImageRenderer {
    layout: OciLayout,
    limits: RenderLimits,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub(crate) struct FilesystemDiff {
    pub(crate) changes: Vec<FilesystemChange>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub(crate) struct FilesystemChange {
    pub(crate) change: FilesystemChangeKind,
    pub(crate) path: String,
    pub(crate) path_hex: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) before: Option<FilesystemNode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) after: Option<FilesystemNode>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FilesystemChangeKind {
    Added,
    Removed,
    Modified,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub(crate) struct FilesystemNode {
    pub(crate) kind: &'static str,
    pub(crate) mode: u32,
    pub(crate) uid: u32,
    pub(crate) gid: u32,
    pub(crate) mtime_seconds: i64,
    pub(crate) mtime_nanos: u32,
    pub(crate) xattrs: Vec<FilesystemXattr>,
    #[serde(flatten)]
    pub(crate) details: FilesystemNodeDetails,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "details")]
pub(crate) enum FilesystemNodeDetails {
    None,
    Regular { digest: Digest, size: u64 },
    Symlink { target: String, target_hex: String },
    Device { major: u32, minor: u32 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub(crate) struct FilesystemXattr {
    pub(crate) name_hex: String,
    pub(crate) value_hex: String,
}

impl ImageRenderer {
    #[must_use]
    pub fn new(layout: OciLayout) -> Self {
        Self {
            layout,
            limits: RenderLimits::default(),
        }
    }

    pub fn copy_file(
        &self,
        image: &ImageView,
        source: &[u8],
        destination: &mut File,
    ) -> Result<(Digest, u64)> {
        enforce_limit(
            "max_layers",
            self.limits.layers,
            usize_to_u64(image.layers.len()),
        )?;
        let requested = path_from_request(source, self.limits)?;
        if requested.is_root() {
            return Err(RenderError::NotRegular {
                path: "/".to_owned(),
                kind: "directory".to_owned(),
            }
            .into());
        }

        let view = self.filesystem_view(image, false)?;
        let resolved = view.regular_location(&requested, self.limits)?;
        copy_regular(&self.layout, &resolved, destination, self.limits)
    }

    pub(crate) fn verify(&self, image: &ImageView) -> Result<()> {
        self.filesystem_view(image, false).map(|_| ())
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn verify_regular_path_without_symlinks(
        &self,
        image: &ImageView,
        path: &[u8],
    ) -> Result<()> {
        let requested = path_from_request(path, self.limits)?;
        if requested.is_root() {
            bail!("resolver projection target must not be the filesystem root");
        }
        let view = self.filesystem_view(image, false)?;
        view.verify_regular_path_without_symlinks(&requested, self.limits)
    }

    pub(crate) fn diff(&self, before: &ImageView, after: &ImageView) -> Result<FilesystemDiff> {
        let before = self.filesystem_nodes(before)?;
        let after = self.filesystem_nodes(after)?;
        let paths = before
            .keys()
            .chain(after.keys())
            .cloned()
            .collect::<BTreeSet<_>>();
        let changes = paths
            .into_iter()
            .filter_map(|path| {
                let before = before.get(&path).cloned();
                let after = after.get(&path).cloned();
                let change = match (&before, &after) {
                    (None, Some(_)) => FilesystemChangeKind::Added,
                    (Some(_), None) => FilesystemChangeKind::Removed,
                    (Some(left), Some(right)) if left != right => FilesystemChangeKind::Modified,
                    _ => return None,
                };
                Some(FilesystemChange {
                    change,
                    path: display_path(&path),
                    path_hex: hex_path(&path),
                    before,
                    after,
                })
            })
            .collect();
        Ok(FilesystemDiff { changes })
    }

    pub(crate) fn export_tar(&self, image: &ImageView, destination: &mut File) -> Result<()> {
        let view = self.filesystem_view(image, true)?;
        let (tree, mut regulars) = export_tree(view)?;
        let mut contents = ContentStore::new()?;
        for descriptor in &image.layers {
            let Some(expected) = regulars.remove(&descriptor.digest) else {
                continue;
            };
            let expected = expected
                .iter()
                .map(regular_location)
                .collect::<Result<Vec<_>>>()?;
            visit_layer_regulars(
                &self.layout,
                descriptor,
                &expected,
                self.limits,
                self.limits.total_uncompressed_bytes,
                |location, reader| {
                    let (digest, size) = contents.put_reader(reader)?;
                    if location.content_digest.as_ref() != Some(&digest) || location.size != size {
                        bail!(
                            "OCI Layer regular entry changed while exporting {}",
                            display_path(&location.path)
                        );
                    }
                    Ok(())
                },
            )?;
        }
        if !regulars.is_empty() {
            bail!("resolved filesystem refers to an unavailable OCI Layer");
        }
        let mut writer = FilesystemTarWriter::new(destination);
        if let Some(root) = &tree.root {
            writer.append_root(root)?;
        }
        for (path, entry) in &tree.entries {
            writer.append_entry(path, entry, &contents)?;
        }
        writer.finish()
    }

    fn filesystem_nodes(&self, image: &ImageView) -> Result<BTreeMap<ImagePath, FilesystemNode>> {
        let view = self.filesystem_view(image, true)?;
        view.nodes
            .into_iter()
            .map(|(path, node)| Ok((path, filesystem_node(node))))
            .collect()
    }

    fn filesystem_view(
        &self,
        image: &ImageView,
        hash_regular_contents: bool,
    ) -> Result<FilesystemView> {
        enforce_limit(
            "max_layers",
            self.limits.layers,
            usize_to_u64(image.layers.len()),
        )?;
        let mut view = FilesystemView::default();
        let mut total_entries = 0_u64;
        let mut total_uncompressed = 0_u64;
        for descriptor in &image.layers {
            let remaining = self
                .limits
                .total_uncompressed_bytes
                .checked_sub(total_uncompressed)
                .ok_or(RenderError::LimitExceeded {
                    name: "max_total_uncompressed_bytes",
                    limit: self.limits.total_uncompressed_bytes,
                    observed: total_uncompressed,
                })?;
            let (plan, entries, uncompressed) = scan_layer_with_mode(
                &self.layout,
                descriptor,
                self.limits,
                remaining,
                hash_regular_contents,
            )?;
            total_entries = total_entries
                .checked_add(entries)
                .context("Layer entry count overflow")?;
            enforce_limit("max_entries", self.limits.entries, total_entries)?;
            total_uncompressed = total_uncompressed
                .checked_add(uncompressed)
                .context("Layer byte count overflow")?;
            view.apply(plan, self.limits)?;
        }
        Ok(view)
    }
}

struct ExportTree {
    root: Option<Metadata>,
    entries: BTreeMap<FsPath, FsEntry>,
}

fn export_tree(view: FilesystemView) -> Result<(ExportTree, BTreeMap<Digest, Vec<LayerEntry>>)> {
    let mut root = None;
    let mut entries = BTreeMap::new();
    let mut anchors = BTreeMap::<(Digest, u64), ImagePath>::new();
    let mut regulars = BTreeMap::<Digest, Vec<LayerEntry>>::new();
    for (path, node) in view.nodes {
        let FsNode { kind, metadata } = node;
        if path.is_root() {
            if !matches!(kind, NodeKind::Directory) {
                bail!("resolved filesystem root is not a directory");
            }
            if root.replace(metadata).is_some() {
                bail!("resolved filesystem contains duplicate root metadata");
            }
            continue;
        }
        let entry_kind = match kind {
            NodeKind::Regular(location) => {
                let digest = location
                    .content_digest
                    .clone()
                    .context("resolved regular file lacks a content digest")?;
                let key = (location.descriptor.digest.clone(), location.ordinal);
                let hardlink = anchors.get(&key).cloned();
                if hardlink.is_none() {
                    anchors.insert(key, path.clone());
                    regulars
                        .entry(location.descriptor.digest.clone())
                        .or_default()
                        .push(LayerEntry {
                            path: location.path.clone(),
                            kind: LayerEntryKind::Regular(location.clone()),
                            metadata: metadata.clone(),
                        });
                }
                EntryKind::Regular {
                    digest,
                    size: location.size,
                    hardlink,
                }
            }
            NodeKind::Directory => EntryKind::Directory,
            NodeKind::Symlink(target) => EntryKind::Symlink {
                target: target.into_boxed_slice(),
            },
            NodeKind::Character { major, minor } => EntryKind::Character { major, minor },
            NodeKind::Block { major, minor } => EntryKind::Block { major, minor },
            NodeKind::Fifo => EntryKind::Fifo,
            NodeKind::PendingHardlink(_) => unreachable!("filesystem view resolves hardlinks"),
        };
        entries.insert(
            path,
            FsEntry {
                metadata,
                kind: entry_kind,
            },
        );
    }
    for entries in regulars.values_mut() {
        entries.sort_by_key(|entry| match &entry.kind {
            LayerEntryKind::Regular(location) => location.ordinal,
            _ => unreachable!("regular export plan contains only regular entries"),
        });
    }
    Ok((ExportTree { root, entries }, regulars))
}

fn filesystem_node(node: FsNode) -> FilesystemNode {
    let kind = node.kind.name();
    let details = match node.kind {
        NodeKind::Regular(location) => FilesystemNodeDetails::Regular {
            digest: location
                .content_digest
                .expect("diff view hashes every regular file"),
            size: location.size,
        },
        NodeKind::Symlink(target) => FilesystemNodeDetails::Symlink {
            target: display_bytes(&target),
            target_hex: hex_bytes(&target),
        },
        NodeKind::Character { major, minor } | NodeKind::Block { major, minor } => {
            FilesystemNodeDetails::Device { major, minor }
        }
        NodeKind::Directory | NodeKind::Fifo => FilesystemNodeDetails::None,
        NodeKind::PendingHardlink(_) => unreachable!("filesystem view resolves hardlinks"),
    };
    FilesystemNode {
        kind,
        mode: node.metadata.mode,
        uid: node.metadata.uid,
        gid: node.metadata.gid,
        mtime_seconds: node.metadata.mtime.seconds,
        mtime_nanos: node.metadata.mtime.nanos,
        xattrs: node
            .metadata
            .xattrs
            .into_iter()
            .map(|(name, value)| FilesystemXattr {
                name_hex: hex_bytes(&name),
                value_hex: hex_bytes(&value),
            })
            .collect(),
        details,
    }
}

pub fn layer_diff_id(layout: &OciLayout, descriptor: &OciDescriptor) -> Result<Digest> {
    let reader = open_decoded_layer(layout, descriptor)?;
    let bounded = BoundedReader::new(reader, RenderLimits::default().total_uncompressed_bytes);
    digest_reader(bounded)
        .map(|(digest, _)| digest)
        .with_context(|| {
            format!(
                "failed to verify OCI Layer DiffID for {}",
                descriptor.digest
            )
        })
}

#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "rootfs materialization is Linux-only")
)]
pub(crate) fn scan_layer(
    layout: &OciLayout,
    descriptor: &OciDescriptor,
    limits: RenderLimits,
    remaining_uncompressed: u64,
) -> Result<(LayerPlan, u64, u64)> {
    scan_layer_with_mode(layout, descriptor, limits, remaining_uncompressed, false)
}

fn scan_layer_with_mode(
    layout: &OciLayout,
    descriptor: &OciDescriptor,
    limits: RenderLimits,
    remaining_uncompressed: u64,
    hash_regular_contents: bool,
) -> Result<(LayerPlan, u64, u64)> {
    let pax_reader = open_decoded_layer(layout, descriptor)?;
    let pax_index = scan_pax_index(pax_reader, limits, remaining_uncompressed)
        .context("failed to index OCI Layer PAX metadata")?;
    let reader = open_decoded_layer(layout, descriptor)?;
    scan_decoded_layer(
        reader,
        descriptor,
        limits,
        remaining_uncompressed,
        &pax_index,
        hash_regular_contents,
    )
    .with_context(|| format!("failed to scan OCI Layer {}", descriptor.digest))
}

fn scan_pax_index(
    reader: impl Read,
    limits: RenderLimits,
    remaining_uncompressed: u64,
) -> Result<TarPaxIndex> {
    pax::scan_tar(
        reader,
        TarPaxLimits {
            entries: limits.entries,
            total_bytes: remaining_uncompressed,
            pax_bytes: limits.pax_bytes,
            index_bytes: limits.pax_index_bytes,
        },
    )
    .map_err(|error| match error.downcast::<PaxError>() {
        Ok(PaxError::EntryLimit { limit, observed }) => RenderError::LimitExceeded {
            name: "max_entries",
            limit,
            observed,
        }
        .into(),
        Ok(PaxError::IndexBytesLimit { limit, observed }) => RenderError::LimitExceeded {
            name: "max_pax_index_bytes",
            limit,
            observed,
        }
        .into(),
        Err(error) => error,
    })
}

fn scan_decoded_layer(
    reader: impl Read,
    descriptor: &OciDescriptor,
    limits: RenderLimits,
    remaining_uncompressed: u64,
    pax_index: &TarPaxIndex,
    hash_regular_contents: bool,
) -> Result<(LayerPlan, u64, u64)> {
    let bounded = BoundedReader::new(reader, remaining_uncompressed);
    let mut archive = Archive::new(bounded);
    let mut plan = LayerPlan::default();
    let mut seen = BTreeSet::new();
    let mut entry_count = 0_u64;
    for (ordinal, entry) in archive
        .entries()
        .context("failed to read OCI Layer tar")?
        .enumerate()
    {
        let mut entry = entry.context("failed to read OCI Layer entry")?;
        entry_count = entry_count
            .checked_add(1)
            .context("Layer entry count overflow")?;
        enforce_limit("max_entries", limits.entries, entry_count)?;
        let ordinal = u64::try_from(ordinal).context("Layer entry ordinal overflow")?;
        let empty_records = PaxRecords::default();
        let records = pax_index.get(ordinal)?.unwrap_or(&empty_records);
        collect_layer_entry(
            &mut entry,
            descriptor,
            ordinal,
            records,
            LayerScanOptions {
                limits,
                hash_regular_contents,
            },
            &mut seen,
            &mut plan,
        )?;
    }
    if usize::try_from(entry_count)? != pax_index.len() {
        bail!("tar entry count differs from its PAX metadata index");
    }
    let mut bounded = archive.into_inner();
    std::io::copy(&mut bounded, &mut std::io::sink())
        .context("failed to finish OCI Layer decompression")?;
    Ok((plan, entry_count, bounded.consumed()))
}

fn collect_layer_entry<R: Read>(
    entry: &mut tar::Entry<'_, R>,
    descriptor: &OciDescriptor,
    ordinal: u64,
    records: &PaxRecords,
    options: LayerScanOptions,
    seen: &mut BTreeSet<ImagePath>,
    plan: &mut LayerPlan,
) -> Result<()> {
    let raw_path = entry.path_bytes();
    let path = path_from_layer(&raw_path, options.limits)?;
    let metadata = layer_metadata(entry, records)?;
    if path.is_root() {
        if entry.header().entry_type() == EntryType::Directory {
            if !seen.insert(path.clone()) {
                return Err(RenderError::DuplicatePath {
                    path: display_path(&path),
                }
                .into());
            }
            plan.entries.push(LayerEntry {
                path,
                kind: LayerEntryKind::Directory,
                metadata,
            });
            return Ok(());
        }
        return Err(RenderError::UnsafePath {
            path: display_bytes(&raw_path),
        }
        .into());
    }
    if !seen.insert(path.clone()) {
        return Err(RenderError::DuplicatePath {
            path: display_path(&path),
        }
        .into());
    }
    let entry_type = entry.header().entry_type();
    let size = entry.size();
    enforce_limit("max_entry_bytes", options.limits.entry_bytes, size)?;
    if collect_whiteout(&path, entry_type, size, plan)? {
        return Ok(());
    }
    let kind = layer_entry_kind(
        entry,
        descriptor,
        ordinal,
        &path,
        options.limits,
        options.hash_regular_contents,
    )?;
    plan.entries.push(LayerEntry {
        path,
        kind,
        metadata,
    });
    Ok(())
}

fn layer_metadata<R: Read>(entry: &tar::Entry<'_, R>, records: &PaxRecords) -> Result<Metadata> {
    let header = entry.header();
    let mode = header.mode().context("invalid OCI Layer mode")?;
    if mode > 0o7777 {
        bail!("OCI Layer mode exceeds permission and special bits: {mode:o}");
    }
    Ok(Metadata {
        mode,
        uid: u32::try_from(header.uid().context("invalid OCI Layer uid")?)
            .context("OCI Layer uid exceeds u32")?,
        gid: u32::try_from(header.gid().context("invalid OCI Layer gid")?)
            .context("OCI Layer gid exceeds u32")?,
        mtime: records.mtime(header.mtime().context("invalid OCI Layer mtime")?)?,
        xattrs: pax::decode_xattrs(records).context("failed to decode OCI Layer xattrs")?,
    })
}

fn collect_whiteout(
    path: &ImagePath,
    entry_type: EntryType,
    size: u64,
    plan: &mut LayerPlan,
) -> Result<bool> {
    if !path.basename().starts_with(b".wh.") {
        return Ok(false);
    }
    if entry_type != EntryType::Regular || size != 0 {
        return Err(RenderError::InvalidWhiteout {
            path: display_path(path),
        }
        .into());
    }
    if path.basename() == b".wh..wh..opq" {
        plan.opaques.push(path.parent());
        return Ok(true);
    }
    let target = &path.basename()[4..];
    if target.is_empty() || target.starts_with(b".wh.") {
        return Err(RenderError::InvalidWhiteout {
            path: display_path(path),
        }
        .into());
    }
    plan.whiteouts.push(
        path.parent()
            .join_component(target, usize_to_u64(path.as_bytes().len()))?,
    );
    Ok(true)
}

fn layer_entry_kind<R: Read>(
    entry: &mut tar::Entry<'_, R>,
    descriptor: &OciDescriptor,
    ordinal: u64,
    path: &ImagePath,
    limits: RenderLimits,
    hash_regular_contents: bool,
) -> Result<LayerEntryKind> {
    Ok(match entry.header().entry_type() {
        EntryType::Regular => {
            let size = entry.size();
            let content_digest = hash_regular_contents
                .then(|| digest_reader(&mut *entry))
                .transpose()?
                .map(|(digest, observed)| {
                    if observed != size {
                        bail!(
                            "OCI Layer regular entry size changed while reading {}",
                            display_path(path)
                        );
                    }
                    Ok(digest)
                })
                .transpose()?;
            LayerEntryKind::Regular(EntryLocation {
                descriptor: descriptor.clone(),
                ordinal,
                path: path.clone(),
                size,
                content_digest,
            })
        }
        EntryType::Directory => LayerEntryKind::Directory,
        EntryType::Symlink => {
            let target = entry
                .link_name_bytes()
                .ok_or_else(|| RenderError::UnsafePath {
                    path: display_path(path),
                })?;
            enforce_limit(
                "max_link_target_bytes",
                limits.link_target_bytes,
                usize_to_u64(target.len()),
            )?;
            if target.contains(&0) {
                return Err(RenderError::UnsafePath {
                    path: display_bytes(&target),
                }
                .into());
            }
            LayerEntryKind::Symlink(target.into_owned())
        }
        EntryType::Link => {
            let target =
                entry
                    .link_name_bytes()
                    .ok_or_else(|| RenderError::UnresolvedHardlink {
                        path: display_path(path),
                        target: "<missing>".to_owned(),
                    })?;
            LayerEntryKind::Hardlink(path_from_link(&target, limits)?)
        }
        EntryType::Char => LayerEntryKind::Character {
            major: entry
                .header()
                .device_major()
                .context("invalid OCI Layer device major")?
                .context("OCI character device lacks a major number")?,
            minor: entry
                .header()
                .device_minor()
                .context("invalid OCI Layer device minor")?
                .context("OCI character device lacks a minor number")?,
        },
        EntryType::Block => LayerEntryKind::Block {
            major: entry
                .header()
                .device_major()
                .context("invalid OCI Layer device major")?
                .context("OCI block device lacks a major number")?,
            minor: entry
                .header()
                .device_minor()
                .context("invalid OCI Layer device minor")?
                .context("OCI block device lacks a minor number")?,
        },
        EntryType::Fifo => LayerEntryKind::Fifo,
        other => {
            return Err(RenderError::UnsupportedEntry {
                path: display_path(path),
                kind: format!("{other:?}"),
            }
            .into());
        }
    })
}

fn copy_regular(
    layout: &OciLayout,
    resolved: &ResolvedRegular,
    destination: &mut File,
    limits: RenderLimits,
) -> Result<(Digest, u64)> {
    let location = &resolved.location;
    let pax_reader = open_decoded_layer(layout, &location.descriptor)?;
    let pax_index = pax::scan_tar(
        pax_reader,
        TarPaxLimits {
            entries: limits.entries,
            total_bytes: limits.total_uncompressed_bytes,
            pax_bytes: limits.pax_bytes,
            index_bytes: limits.pax_index_bytes,
        },
    )?;
    let empty_records = PaxRecords::default();
    let records = pax_index.get(location.ordinal)?.unwrap_or(&empty_records);
    let actual_xattrs = pax::decode_xattrs(records)?;
    if actual_xattrs != resolved.metadata.xattrs {
        bail!(
            "OCI Layer xattrs changed while reading {}",
            display_path(&location.path)
        );
    }
    let reader = open_decoded_layer(layout, &location.descriptor)?;
    let bounded = BoundedReader::new(reader, limits.total_uncompressed_bytes);
    let mut archive = Archive::new(bounded);
    for (ordinal, entry) in archive
        .entries()
        .context("failed to read OCI Layer tar")?
        .enumerate()
    {
        let mut entry = entry.context("failed to read OCI Layer entry")?;
        let ordinal = u64::try_from(ordinal).context("Layer entry ordinal overflow")?;
        if ordinal != location.ordinal {
            continue;
        }
        let path = path_from_layer(&entry.path_bytes(), limits)?;
        if path != location.path
            || entry.header().entry_type() != EntryType::Regular
            || entry.size() != location.size
        {
            bail!(
                "OCI Layer entry changed while reading {}",
                display_path(&location.path)
            );
        }
        let copied =
            copy_and_digest(&mut entry, destination, location.size).with_context(|| {
                format!("failed to read image file {}", display_path(&location.path))
            })?;
        if location
            .content_digest
            .as_ref()
            .is_some_and(|digest| copied.0 != *digest)
        {
            bail!(
                "OCI Layer regular entry content changed while reading {}",
                display_path(&location.path)
            );
        }
        return Ok(copied);
    }
    bail!(
        "OCI Layer no longer contains image file {}",
        display_path(&location.path)
    )
}

pub(crate) fn visit_layer_regulars(
    layout: &OciLayout,
    descriptor: &OciDescriptor,
    expected: &[&EntryLocation],
    limits: RenderLimits,
    max_uncompressed: u64,
    mut visitor: impl FnMut(&EntryLocation, &mut dyn Read) -> Result<()>,
) -> Result<()> {
    #[cfg(test)]
    MATERIALIZATION_CONTENT_PASSES.set(
        MATERIALIZATION_CONTENT_PASSES
            .get()
            .checked_add(1)
            .expect("materialization content pass count overflow"),
    );
    let reader = open_decoded_layer(layout, descriptor)?;
    let bounded = BoundedReader::new(reader, max_uncompressed);
    let mut archive = Archive::new(bounded);
    let mut expected = expected.iter().peekable();
    {
        let entries = archive.entries().context("failed to read OCI Layer tar")?;
        for (ordinal, entry) in entries.enumerate() {
            let mut entry = entry.context("failed to read OCI Layer entry")?;
            let ordinal = u64::try_from(ordinal).context("Layer entry ordinal overflow")?;
            let Some(next) = expected.peek() else {
                continue;
            };
            let location = *next;
            if location.descriptor != *descriptor {
                bail!("materialization content plan refers to a different OCI Layer")
            }
            if location.ordinal < ordinal {
                bail!(
                    "OCI Layer no longer contains image file {}",
                    display_path(&location.path)
                );
            }
            if location.ordinal != ordinal {
                continue;
            }
            let path = path_from_layer(&entry.path_bytes(), limits)?;
            if path != location.path
                || entry.header().entry_type() != EntryType::Regular
                || entry.size() != location.size
            {
                bail!(
                    "OCI Layer entry changed while reading {}",
                    display_path(&location.path)
                );
            }
            visitor(next, &mut entry).with_context(|| {
                format!("failed to materialize image file {}", display_path(&path))
            })?;
            expected.next();
        }
    }
    if let Some(entry) = expected.next() {
        bail!(
            "OCI Layer no longer contains image file {}",
            display_path(&entry.path)
        );
    }
    let mut bounded = archive.into_inner();
    std::io::copy(&mut bounded, &mut std::io::sink())
        .context("failed to finish OCI Layer decompression")?;
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
pub(crate) fn reset_materialization_content_passes() {
    MATERIALIZATION_CONTENT_PASSES.set(0);
}

#[cfg(all(test, target_os = "linux"))]
pub(crate) fn materialization_content_passes() -> u64 {
    MATERIALIZATION_CONTENT_PASSES.get()
}

fn copy_and_digest(
    reader: &mut impl Read,
    writer: &mut impl Write,
    expected_size: u64,
) -> Result<(Digest, u64)> {
    let mut hasher = Sha256::new();
    let mut size = 0_u64;
    let mut buffer = vec![0_u8; COPY_BUFFER_BYTES];
    loop {
        let read = reader
            .read(&mut buffer)
            .context("failed to read image file")?;
        if read == 0 {
            break;
        }
        writer
            .write_all(&buffer[..read])
            .context("failed to write image file")?;
        hasher.update(&buffer[..read]);
        size = size
            .checked_add(usize_to_u64(read))
            .context("image file size overflow")?;
    }
    if size != expected_size {
        bail!("OCI Layer file size changed: expected {expected_size}, received {size}");
    }
    Ok((finish_sha256(hasher), size))
}

fn open_decoded_layer(layout: &OciLayout, descriptor: &OciDescriptor) -> Result<Box<dyn Read>> {
    let file = layout.open_descriptor(descriptor)?;
    match descriptor.media_type.as_str() {
        OCI_LAYER_TAR => Ok(Box::new(file)),
        OCI_LAYER_GZIP => Ok(Box::new(MultiGzDecoder::new(file))),
        OCI_LAYER_ZSTD => Ok(Box::new(
            zstd::stream::read::Decoder::new(file)
                .context("failed to initialize zstd OCI Layer decoder")?,
        )),
        media_type => bail!("unsupported OCI Layer mediaType: {media_type}"),
    }
}

fn apply_link_components(base: &mut Vec<Vec<u8>>, target: &[u8]) {
    for component in target.split(|byte| *byte == b'/') {
        if component.is_empty() || component == b"." {
            continue;
        }
        if component == b".." {
            base.pop();
        } else {
            base.push(component.to_vec());
        }
    }
}

fn enforce_limit(
    name: &'static str,
    limit: u64,
    observed: u64,
) -> std::result::Result<(), RenderError> {
    if observed > limit {
        return Err(RenderError::LimitExceeded {
            name,
            limit,
            observed,
        });
    }
    Ok(())
}

fn path_from_layer(
    raw: &[u8],
    limits: RenderLimits,
) -> std::result::Result<ImagePath, RenderError> {
    FsPath::from_relative(raw, limits.path_bytes).map_err(path_error)
}

fn path_from_request(
    raw: &[u8],
    limits: RenderLimits,
) -> std::result::Result<ImagePath, RenderError> {
    FsPath::from_absolute(raw, limits.path_bytes).map_err(path_error)
}

fn path_from_link(raw: &[u8], limits: RenderLimits) -> std::result::Result<ImagePath, RenderError> {
    enforce_limit(
        "max_link_target_bytes",
        limits.link_target_bytes,
        usize_to_u64(raw.len()),
    )?;
    FsPath::from_relative(raw, limits.path_bytes).map_err(path_error)
}

fn path_from_components(
    components: &[Vec<u8>],
    limits: RenderLimits,
) -> std::result::Result<ImagePath, RenderError> {
    FsPath::from_normalized_components(components, limits.path_bytes).map_err(path_error)
}

fn path_error(error: FsPathError) -> RenderError {
    match error {
        FsPathError::Unsafe(path) => RenderError::UnsafePath { path },
        FsPathError::TooLong { limit, observed } => RenderError::LimitExceeded {
            name: "max_path_bytes",
            limit,
            observed,
        },
    }
}

fn display_path(path: &ImagePath) -> String {
    path.display()
}

fn display_bytes(bytes: &[u8]) -> String {
    bytes
        .iter()
        .flat_map(|byte| std::ascii::escape_default(*byte))
        .map(char::from)
        .collect()
}

fn hex_path(path: &ImagePath) -> String {
    let mut absolute = Vec::with_capacity(path.as_bytes().len() + 1);
    absolute.push(b'/');
    absolute.extend_from_slice(path.as_bytes());
    hex_bytes(&absolute)
}

fn hex_bytes(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

struct BoundedReader<R> {
    inner: R,
    limit: u64,
    consumed: u64,
}

impl<R> BoundedReader<R> {
    const fn new(inner: R, limit: u64) -> Self {
        Self {
            inner,
            limit,
            consumed: 0,
        }
    }

    const fn consumed(&self) -> u64 {
        self.consumed
    }
}

impl<R: Read> Read for BoundedReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let remaining = self.limit.saturating_sub(self.consumed);
        let allowed = remaining.saturating_add(1).min(usize_to_u64(buffer.len()));
        let buffer_len = buffer.len();
        let allowed = usize::try_from(allowed).unwrap_or(buffer_len);
        let read = self.inner.read(&mut buffer[..allowed])?;
        self.consumed = self
            .consumed
            .checked_add(usize_to_u64(read))
            .ok_or_else(|| std::io::Error::other("uncompressed Layer byte count overflow"))?;
        if self.consumed > self.limit {
            return Err(std::io::Error::other(format!(
                "render limit max_total_uncompressed_bytes exceeded: limit {}, observed {}",
                self.limit, self.consumed
            )));
        }
        Ok(read)
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::io::Cursor;
    use std::os::unix::ffi::OsStrExt as _;

    use super::*;
    use crate::filesystem::{Timestamp, Xattrs};

    fn path(raw: &[u8]) -> ImagePath {
        path_from_layer(raw, RenderLimits::default()).expect("valid path")
    }

    fn regular(path: &[u8], ordinal: u64) -> LayerEntry {
        LayerEntry {
            path: self::path(path),
            kind: LayerEntryKind::Regular(EntryLocation {
                descriptor: descriptor(),
                ordinal,
                path: self::path(path),
                size: 0,
                content_digest: None,
            }),
            metadata: metadata(Xattrs::new()),
        }
    }

    fn directory(path: &[u8]) -> LayerEntry {
        LayerEntry {
            path: self::path(path),
            kind: LayerEntryKind::Directory,
            metadata: metadata(Xattrs::new()),
        }
    }

    fn hardlink(path: &[u8], target: &[u8]) -> LayerEntry {
        LayerEntry {
            path: self::path(path),
            kind: LayerEntryKind::Hardlink(self::path(target)),
            metadata: metadata(Xattrs::new()),
        }
    }

    fn metadata(xattrs: Xattrs) -> Metadata {
        Metadata {
            mode: 0o644,
            uid: 0,
            gid: 0,
            mtime: Timestamp {
                seconds: 0,
                nanos: 0,
            },
            xattrs,
        }
    }

    #[test]
    fn whiteouts_are_applied_before_same_layer_entries() {
        let mut view = FilesystemView::default();
        view.apply(
            LayerPlan {
                entries: vec![regular(b"value", 0), regular(b"opaque/old", 1)],
                ..LayerPlan::default()
            },
            RenderLimits::default(),
        )
        .expect("lower Layer");
        view.apply(
            LayerPlan {
                whiteouts: vec![path(b"value")],
                opaques: vec![path(b"opaque")],
                entries: vec![regular(b"value", 0), regular(b"opaque/new", 1)],
            },
            RenderLimits::default(),
        )
        .expect("upper Layer");
        assert!(view.nodes.contains_key(&path(b"value")));
        assert!(!view.nodes.contains_key(&path(b"opaque/old")));
        assert!(view.nodes.contains_key(&path(b"opaque/new")));
    }

    #[test]
    fn root_directory_xattrs_replace_metadata_without_removing_children() {
        let mut view = FilesystemView::default();
        view.apply(
            LayerPlan {
                entries: vec![regular(b"value", 0)],
                ..LayerPlan::default()
            },
            RenderLimits::default(),
        )
        .expect("lower Layer");
        let root_xattrs = Xattrs::from([(
            b"user.root".to_vec().into_boxed_slice(),
            b"upper".to_vec().into_boxed_slice(),
        )]);
        view.apply(
            LayerPlan {
                entries: vec![LayerEntry {
                    path: path(b"."),
                    kind: LayerEntryKind::Directory,
                    metadata: metadata(root_xattrs.clone()),
                }],
                ..LayerPlan::default()
            },
            RenderLimits::default(),
        )
        .expect("root metadata Layer");
        assert!(view.nodes.contains_key(&path(b"value")));
        assert_eq!(view.nodes[&path(b".")].metadata.xattrs, root_xattrs);
    }

    #[test]
    fn same_layer_directory_after_child_preserves_the_child() {
        let mut view = FilesystemView::default();
        view.apply(
            LayerPlan {
                entries: vec![
                    regular(b"dir/child", 0),
                    regular(b"dir-other", 1),
                    directory(b"dir"),
                ],
                ..LayerPlan::default()
            },
            RenderLimits::default(),
        )
        .expect("Layer");
        assert!(view.nodes.contains_key(&path(b"dir/child")));
        assert!(view.nodes.contains_key(&path(b"dir-other")));
        assert!(matches!(
            view.nodes[&path(b"dir")].kind,
            NodeKind::Directory
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn resolver_target_accepts_implicit_directories_but_rejects_symlinks() {
        let limits = RenderLimits::default();
        let target = path(b"etc/resolv.conf");
        let mut implicit = FilesystemView::default();
        implicit
            .apply(
                LayerPlan {
                    entries: vec![regular(b"etc/resolv.conf", 0)],
                    ..LayerPlan::default()
                },
                limits,
            )
            .expect("filesystem view");
        implicit
            .verify_regular_path_without_symlinks(&target, limits)
            .expect("implicit parent directory");

        let mut explicit = FilesystemView::default();
        explicit
            .apply(
                LayerPlan {
                    entries: vec![directory(b"etc"), regular(b"etc/resolv.conf", 0)],
                    ..LayerPlan::default()
                },
                limits,
            )
            .expect("filesystem view");
        explicit
            .verify_regular_path_without_symlinks(&target, limits)
            .expect("explicit parent directory");

        let mut symlink = FilesystemView::default();
        symlink
            .apply(
                LayerPlan {
                    entries: vec![LayerEntry {
                        path: path(b"etc"),
                        kind: LayerEntryKind::Symlink(b"real-etc".to_vec()),
                        metadata: metadata(Xattrs::new()),
                    }],
                    ..LayerPlan::default()
                },
                limits,
            )
            .expect("filesystem view");
        let error = symlink
            .verify_regular_path_without_symlinks(&target, limits)
            .expect_err("symlink parent must be rejected");
        assert!(
            format!("{error:#}").contains("contains a symbolic link"),
            "{error:#}"
        );

        let mut target_symlink = FilesystemView::default();
        target_symlink
            .apply(
                LayerPlan {
                    entries: vec![
                        directory(b"etc"),
                        LayerEntry {
                            path: target.clone(),
                            kind: LayerEntryKind::Symlink(b"../run/resolv.conf".to_vec()),
                            metadata: metadata(Xattrs::new()),
                        },
                    ],
                    ..LayerPlan::default()
                },
                limits,
            )
            .expect("filesystem view");
        let error = target_symlink
            .verify_regular_path_without_symlinks(&target, limits)
            .expect_err("symlink target must be rejected");
        assert!(
            format!("{error:#}").contains("contains a symbolic link"),
            "{error:#}"
        );
    }

    #[test]
    fn forward_hardlink_chain_resolves_without_recursive_stack_growth() {
        let count = 4096_u64;
        let mut entries = (0..count)
            .map(|index| {
                hardlink(
                    format!("link-{index}").as_bytes(),
                    format!("link-{}", index + 1).as_bytes(),
                )
            })
            .collect::<Vec<_>>();
        entries.push(regular(format!("link-{count}").as_bytes(), count));
        let mut view = FilesystemView::default();
        view.apply(
            LayerPlan {
                entries,
                ..LayerPlan::default()
            },
            RenderLimits::default(),
        )
        .expect("forward hardlink chain");
        let NodeKind::Regular(location) = &view.nodes[&path(b"link-0")].kind else {
            panic!("hardlink must resolve to regular content")
        };
        assert_eq!(location.ordinal, count);
    }

    #[test]
    fn hardlink_cycle_and_missing_target_remain_explicit_errors() {
        for entries in [
            vec![hardlink(b"a", b"b"), hardlink(b"b", b"a")],
            vec![hardlink(b"a", b"missing")],
        ] {
            let error = FilesystemView::default()
                .apply(
                    LayerPlan {
                        entries,
                        ..LayerPlan::default()
                    },
                    RenderLimits::default(),
                )
                .expect_err("unresolved hardlink");
            assert_render_error(&error, |error| {
                matches!(error, RenderError::UnresolvedHardlink { .. })
            });
        }
    }

    #[test]
    fn filesystem_view_enforces_aggregate_retained_byte_budget() {
        let error = FilesystemView::default()
            .apply(
                LayerPlan {
                    entries: vec![regular(b"value", 0)],
                    ..LayerPlan::default()
                },
                RenderLimits {
                    view_bytes: 1,
                    ..RenderLimits::default()
                },
            )
            .expect_err("view byte limit");
        assert_render_error(&error, |error| {
            matches!(
                error,
                RenderError::LimitExceeded {
                    name: "max_view_bytes",
                    limit: 1,
                    ..
                }
            )
        });
    }

    #[test]
    fn non_utf8_paths_remain_distinct() {
        let raw = path(b"data/ff-\xff");
        let replacement = path("data/ff-�".as_bytes());
        assert_ne!(raw, replacement);
        let mut view = FilesystemView::default();
        view.apply(
            LayerPlan {
                entries: vec![
                    regular(raw.as_bytes(), 0),
                    regular(replacement.as_bytes(), 1),
                ],
                ..LayerPlan::default()
            },
            RenderLimits::default(),
        )
        .expect("Layer");
        assert_eq!(view.nodes.len(), 2);
    }

    #[test]
    fn paths_reject_escape_and_detect_normalized_duplicates() {
        assert!(matches!(
            path_from_layer(b"../escape", RenderLimits::default()),
            Err(RenderError::UnsafePath { .. })
        ));
        assert!(matches!(
            path_from_layer(b"/absolute", RenderLimits::default()),
            Err(RenderError::UnsafePath { .. })
        ));
        assert_eq!(path(b"a/./b"), path(b"a//b"));
    }

    #[test]
    fn bounded_reader_fails_after_the_first_byte_over_limit() {
        let mut reader = BoundedReader::new(Cursor::new(vec![0_u8; 5]), 4);
        let mut bytes = Vec::new();
        let error = reader.read_to_end(&mut bytes).expect_err("limit failure");
        assert!(error.to_string().contains("limit 4, observed 5"));
        assert_eq!(reader.consumed(), 5);
    }

    #[test]
    fn scanner_rejects_duplicate_unsafe_and_malformed_whiteout_paths() {
        let duplicate = tar_bytes(&[RawEntry::file(b"a", b"one"), RawEntry::file(b"a", b"two")]);
        assert_render_error(
            &scan_tar(&duplicate, RenderLimits::default()).expect_err("duplicate"),
            |error| matches!(error, RenderError::DuplicatePath { .. }),
        );

        let escape = tar_bytes(&[RawEntry::file(b"../escape", b"value")]);
        assert_render_error(
            &scan_tar(&escape, RenderLimits::default()).expect_err("escape"),
            |error| matches!(error, RenderError::UnsafePath { .. }),
        );

        let whiteout = tar_bytes(&[RawEntry::file(b".wh.", b"")]);
        assert_render_error(
            &scan_tar(&whiteout, RenderLimits::default()).expect_err("whiteout"),
            |error| matches!(error, RenderError::InvalidWhiteout { .. }),
        );
    }

    #[test]
    fn scanner_preserves_non_utf8_paths_and_enforces_entry_limits() {
        let bytes = tar_bytes(&[
            RawEntry::file(b"ff-\xff", b"raw"),
            RawEntry::file("ff-�".as_bytes(), b"utf8"),
        ]);
        let exact = RenderLimits {
            entries: 2,
            ..RenderLimits::default()
        };
        let (plan, entries, _) = scan_tar(&bytes, exact).expect("exact limit");
        assert_eq!(entries, 2);
        assert_ne!(plan.entries[0].path, plan.entries[1].path);

        let exceeded = RenderLimits {
            entries: 1,
            ..RenderLimits::default()
        };
        assert_render_error(
            &scan_tar(&bytes, exceeded).expect_err("entry limit"),
            |error| {
                matches!(
                    error,
                    RenderError::LimitExceeded {
                        name: "max_entries",
                        limit: 1,
                        observed: 2
                    }
                )
            },
        );
    }

    #[test]
    fn scanner_preserves_binary_xattrs_from_length_aware_pax_records() {
        let xattrs = Xattrs::from([(
            b"user.percent%=\xff".to_vec().into_boxed_slice(),
            b"line\nzero\0tail".to_vec().into_boxed_slice(),
        )]);
        let mut records = PaxRecords::default();
        pax::insert_xattrs(&mut records, &xattrs).expect("xattrs");
        let mut bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut bytes);
            pax::append_header(&mut builder, &records, RenderLimits::default().pax_bytes)
                .expect("PAX header");
            let mut header = tar::Header::new_old();
            header.set_entry_type(EntryType::Regular);
            header.set_size(5);
            header.set_mode(0o644);
            header.set_uid(0);
            header.set_gid(0);
            header.set_mtime(0);
            header.set_path("value").expect("path");
            header.set_cksum();
            builder.append(&header, b"value".as_slice()).expect("file");
            builder.finish().expect("finish");
        }

        let (plan, entries, _) = scan_tar(&bytes, RenderLimits::default()).expect("scan");
        assert_eq!(entries, 1);
        assert_eq!(plan.entries[0].metadata.xattrs, xattrs);
        let mut view = FilesystemView::default();
        view.apply(plan, RenderLimits::default()).expect("apply");
        assert_eq!(view.nodes[&path(b"value")].metadata.xattrs, xattrs);
    }

    #[test]
    fn hardlink_content_identity_survives_target_replacement() {
        let mut view = FilesystemView::default();
        let linked_xattrs = Xattrs::from([(
            b"user.identity".to_vec().into_boxed_slice(),
            b"lower".to_vec().into_boxed_slice(),
        )]);
        let mut lower = regular(b"target", 1);
        lower.metadata.xattrs.clone_from(&linked_xattrs);
        view.apply(
            LayerPlan {
                entries: vec![lower],
                ..LayerPlan::default()
            },
            RenderLimits::default(),
        )
        .expect("lower Layer");
        view.apply(
            LayerPlan {
                entries: vec![
                    LayerEntry {
                        path: path(b"link"),
                        kind: LayerEntryKind::Hardlink(path(b"target")),
                        metadata: metadata(Xattrs::new()),
                    },
                    regular(b"target", 2),
                ],
                ..LayerPlan::default()
            },
            RenderLimits::default(),
        )
        .expect("upper Layer");
        let NodeKind::Regular(link) = &view.nodes[&path(b"link")].kind else {
            panic!("hardlink must resolve to regular content");
        };
        let NodeKind::Regular(target) = &view.nodes[&path(b"target")].kind else {
            panic!("target must be regular content");
        };
        assert_eq!(link.ordinal, 1);
        assert_eq!(target.ordinal, 2);
        assert_eq!(view.nodes[&path(b"link")].metadata.xattrs, linked_xattrs);
    }

    #[test]
    fn symlink_resolution_stays_rooted_and_detects_cycles() {
        let mut view = FilesystemView::default();
        view.apply(
            LayerPlan {
                entries: vec![
                    regular(b"target", 1),
                    LayerEntry {
                        path: path(b"dir/link"),
                        kind: LayerEntryKind::Symlink(b"../../../../target".to_vec()),
                        metadata: metadata(Xattrs::new()),
                    },
                ],
                ..LayerPlan::default()
            },
            RenderLimits::default(),
        )
        .expect("symlink Layer");
        assert_eq!(
            view.regular_location(&path(b"dir/link"), RenderLimits::default())
                .expect("rooted target")
                .location
                .ordinal,
            1
        );

        view.nodes.insert(
            path(b"cycle-a"),
            FsNode {
                kind: NodeKind::Symlink(b"cycle-b".to_vec()),
                metadata: metadata(Xattrs::new()),
            },
        );
        view.nodes.insert(
            path(b"cycle-b"),
            FsNode {
                kind: NodeKind::Symlink(b"cycle-a".to_vec()),
                metadata: metadata(Xattrs::new()),
            },
        );
        let error = view
            .regular_location(&path(b"cycle-a"), RenderLimits::default())
            .expect_err("symlink cycle");
        assert_render_error(&error, |value| {
            matches!(value, RenderError::SymlinkLimit { .. })
        });
    }

    struct RawEntry<'a> {
        path: &'a [u8],
        entry_type: EntryType,
        contents: &'a [u8],
        link: Option<&'a [u8]>,
    }

    impl<'a> RawEntry<'a> {
        const fn file(path: &'a [u8], contents: &'a [u8]) -> Self {
            Self {
                path,
                entry_type: EntryType::Regular,
                contents,
                link: None,
            }
        }
    }

    fn descriptor() -> OciDescriptor {
        OciDescriptor {
            digest: Digest::parse(format!("sha256:{}", "1".repeat(64))).expect("digest"),
            size: 0,
            media_type: OCI_LAYER_TAR.to_owned(),
        }
    }

    fn tar_bytes(entries: &[RawEntry<'_>]) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut bytes);
            for entry in entries {
                let mut header = tar::Header::new_old();
                header.set_entry_type(entry.entry_type);
                header.set_size(usize_to_u64(entry.contents.len()));
                header.set_mode(0o644);
                header.set_uid(0);
                header.set_gid(0);
                header.set_mtime(0);
                let raw = header.as_mut_bytes();
                raw[..entry.path.len()].copy_from_slice(entry.path);
                if let Some(link) = entry.link {
                    header
                        .set_link_name(OsStr::from_bytes(link))
                        .expect("link target");
                }
                header.set_cksum();
                builder
                    .append(&header, entry.contents)
                    .expect("raw tar entry");
            }
            builder.finish().expect("finish tar");
        }
        bytes
    }

    fn scan_tar(bytes: &[u8], limits: RenderLimits) -> Result<(LayerPlan, u64, u64)> {
        let index = scan_pax_index(Cursor::new(bytes), limits, usize_to_u64(bytes.len()))?;
        scan_decoded_layer(
            Cursor::new(bytes),
            &descriptor(),
            limits,
            usize_to_u64(bytes.len()),
            &index,
            false,
        )
    }

    fn assert_render_error(error: &anyhow::Error, predicate: impl FnOnce(&RenderError) -> bool) {
        let render = error.downcast_ref::<RenderError>().expect("RenderError");
        assert!(predicate(render), "unexpected error: {render}");
    }
}
