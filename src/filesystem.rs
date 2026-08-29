use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use oci_spec::image::Descriptor;
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest as _, Sha256};

use crate::image::{ImageFilesystem, ImageSelector, Images, layer_reader};
use crate::run::RunId;
use crate::storage::{Database, StoredRun};

pub(crate) enum FilesystemSource {
    Image(ImageSelector),
    Run { run_id: RunId, program: String },
}

pub(crate) struct Filesystems<'a> {
    images: &'a Images<'a>,
    database: &'a Database,
}

#[derive(Debug, Serialize)]
pub(crate) struct FilesystemGetResult {
    schema_version: u32,
    source: ResolvedSource,
    path: String,
    output: String,
    #[serde(flatten)]
    node: ExtractedNode,
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ResolvedSource {
    Image {
        requested: String,
        image: Descriptor,
    },
    Run {
        run_id: String,
        program: String,
        image: Descriptor,
    },
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ExtractedNode {
    File { digest: String, size: usize },
    Directory,
    Symlink { target: String },
}

#[derive(Debug, Serialize)]
pub(crate) struct FilesystemChangesResult {
    schema_version: u32,
    run_id: String,
    program: String,
    initial_image: Descriptor,
    final_image: Descriptor,
    changes: Vec<FilesystemChange>,
    next_after: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct FilesystemChange {
    path: String,
    change: &'static str,
    node_type: &'static str,
    size: Option<u64>,
    subtree: bool,
}

#[derive(Clone)]
struct NodeSummary {
    node_type: &'static str,
    size: Option<u64>,
}

#[derive(Default)]
struct ChangeCandidate {
    final_node: Option<NodeSummary>,
    subtree: bool,
}

#[derive(Clone)]
enum ResolvedNode {
    File(Vec<u8>),
    Directory,
    Symlink(PathBuf),
    HardLink(PathBuf),
    Unsupported(&'static str),
}

enum LayerDecision {
    Node(ResolvedNode),
    Removed,
    Blocked,
}

impl<'a> Filesystems<'a> {
    pub(crate) fn new(images: &'a Images<'a>, database: &'a Database) -> Self {
        Self { images, database }
    }

    pub(crate) fn get(
        &self,
        source: FilesystemSource,
        path: &str,
        output: &Path,
    ) -> Result<FilesystemGetResult> {
        let target = normalize_absolute_path(path)?;
        let (image, source) = self.resolve_source(source)?;
        let node = resolve_node(&image, &target)?.ok_or_else(|| {
            crate::error::classify(
                anyhow::anyhow!("Filesystem path does not exist: {path}"),
                crate::error::ErrorFacts::before_run(
                    crate::error::ErrorCategory::NotFound,
                    "filesystem_resolution",
                ),
            )
        })?;
        let output = new_output_path(output)?;
        let extracted = match node {
            ResolvedNode::File(bytes) => {
                write_file(&output, &bytes)?;
                ExtractedNode::File {
                    digest: sha256_digest(&bytes),
                    size: bytes.len(),
                }
            }
            ResolvedNode::Directory => {
                extract_directory(&image, &target, &output)?;
                ExtractedNode::Directory
            }
            ResolvedNode::Symlink(target) => {
                let target = target
                    .to_str()
                    .context("Filesystem symlink target is not valid UTF-8")?
                    .to_owned();
                write_symlink(&output, Path::new(&target))?;
                ExtractedNode::Symlink { target }
            }
            ResolvedNode::HardLink(_) => unreachable!("hard links are resolved to file content"),
            ResolvedNode::Unsupported(kind) => {
                bail!("Filesystem path has unsupported node type {kind}: {path}")
            }
        };
        Ok(FilesystemGetResult {
            schema_version: 1,
            source,
            path: path.to_owned(),
            output: output
                .to_str()
                .context("output path is not valid UTF-8")?
                .to_owned(),
            node: extracted,
        })
    }

    pub(crate) fn changes(
        &self,
        run_id: RunId,
        program: String,
        limit: usize,
        after: Option<&str>,
    ) -> Result<FilesystemChangesResult> {
        if !(1..=1000).contains(&limit) {
            bail!("--limit must be between 1 and 1000");
        }
        let after = after.map(normalize_absolute_path).transpose()?;
        let run_id = run_id.to_string();
        let record = self.database.run_get(&run_id)?.ok_or_else(|| {
            crate::error::classify(
                anyhow::anyhow!("Run does not exist: {run_id}"),
                crate::error::ErrorFacts::before_run(
                    crate::error::ErrorCategory::NotFound,
                    "run_lookup",
                ),
            )
        })?;
        let initial_image = initial_environment(&record, &program)?;
        let final_image = final_environment(&record, &program)?;
        let initial = self
            .images
            .filesystem_from_manifest(initial_image.clone())?;
        let final_filesystem = self.images.filesystem_from_manifest(final_image.clone())?;
        let mut changes = derive_changes(&initial, &final_filesystem)?;
        if let Some(after) = after {
            changes
                .retain(|change| Path::new(change.path.trim_start_matches('/')) > after.as_path());
        }
        let next_after = (changes.len() > limit)
            .then(|| changes.get(limit - 1).map(|change| change.path.clone()))
            .flatten();
        changes.truncate(limit);
        Ok(FilesystemChangesResult {
            schema_version: 1,
            run_id,
            program,
            initial_image,
            final_image,
            changes,
            next_after,
        })
    }

    fn resolve_source(
        &self,
        source: FilesystemSource,
    ) -> Result<(ImageFilesystem, ResolvedSource)> {
        match source {
            FilesystemSource::Image(selector) => {
                let requested = selector.to_string();
                let image = self.images.filesystem(&selector)?;
                let resolved = ResolvedSource::Image {
                    requested,
                    image: image.manifest.clone(),
                };
                Ok((image, resolved))
            }
            FilesystemSource::Run { run_id, program } => {
                let run_id = run_id.to_string();
                let record = self.database.run_get(&run_id)?.ok_or_else(|| {
                    crate::error::classify(
                        anyhow::anyhow!("Run does not exist: {run_id}"),
                        crate::error::ErrorFacts::before_run(
                            crate::error::ErrorCategory::NotFound,
                            "run_lookup",
                        ),
                    )
                })?;
                let manifest = final_environment(&record, &program)?;
                let image = self.images.filesystem_from_manifest(manifest.clone())?;
                let resolved = ResolvedSource::Run {
                    run_id,
                    program,
                    image: manifest,
                };
                Ok((image, resolved))
            }
        }
    }
}

fn initial_environment(record: &StoredRun, program: &str) -> Result<Descriptor> {
    serde_json::from_value(
        record
            .input
            .get("programs")
            .and_then(|programs| programs.get(program))
            .and_then(|program| program.get("initial_environment"))
            .with_context(|| format!("Run Program does not exist: {program}"))?
            .clone(),
    )
    .context("stored initial_environment descriptor is invalid")
}

pub(crate) fn final_environment(record: &StoredRun, program: &str) -> Result<Descriptor> {
    let completion = record
        .completion
        .as_ref()
        .with_context(|| format!("Run is not terminal: {}", record.run_id))?;
    let result = completion
        .get("result")
        .context("stored Run completion has no result")?;
    if result.get("kind").and_then(Value::as_str) != Some("output") {
        bail!("Run did not return a RunOutput: {}", record.run_id);
    }
    let programs = result
        .get("output")
        .and_then(|value| value.get("programs"))
        .and_then(Value::as_object)
        .context("stored RunOutput has invalid programs")?;
    let program_output = programs
        .get(program)
        .with_context(|| format!("Run Program does not exist: {program}"))?;
    let environment = program_output
        .get("final_environment")
        .context("stored Program output has no final_environment")?;
    match environment.get("availability").and_then(Value::as_str) {
        Some("available") => serde_json::from_value(
            environment
                .get("value")
                .context("available final_environment has no value")?
                .clone(),
        )
        .context("stored final_environment descriptor is invalid"),
        Some("unavailable") => {
            let reason = environment
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("no reason was recorded");
            bail!("Run Program final_environment is unavailable: {reason}")
        }
        _ => bail!("stored Program final_environment availability is invalid"),
    }
}

fn derive_changes(
    initial: &ImageFilesystem,
    final_filesystem: &ImageFilesystem,
) -> Result<Vec<FilesystemChange>> {
    ensure_layer_prefix(initial, final_filesystem)?;
    let mut candidates = BTreeMap::new();
    for layer in &final_filesystem.layers[initial.layers.len()..] {
        let bytes = final_filesystem.read_layer(layer)?;
        apply_change_layer(layer, &bytes, &mut candidates)?;
    }
    let mut initial_nodes = candidates
        .keys()
        .cloned()
        .map(|path| (path, None))
        .collect::<BTreeMap<_, _>>();
    for layer in &initial.layers {
        let bytes = initial.read_layer(layer)?;
        apply_initial_layer(layer, &bytes, &mut initial_nodes)?;
    }
    for (path, candidate) in &mut candidates {
        if candidate
            .final_node
            .as_ref()
            .is_some_and(|node| node.node_type == "file" && node.size.is_none())
            && let Some(ResolvedNode::File(bytes)) = resolve_node(final_filesystem, path)?
        {
            candidate.final_node = Some(NodeSummary {
                node_type: "file",
                size: Some(u64::try_from(bytes.len()).context("file size overflow")?),
            });
        }
    }
    Ok(candidates
        .into_iter()
        .filter_map(|(path, candidate)| {
            let initial = initial_nodes.remove(&path).flatten();
            let (change, node) = match (initial, candidate.final_node) {
                (None, None) => return None,
                (None, Some(node)) => ("added", node),
                (Some(node), None) => ("deleted", node),
                (Some(_), Some(node)) => ("modified", node),
            };
            Some(FilesystemChange {
                path: absolute_image_path(&path),
                change,
                node_type: node.node_type,
                size: node.size,
                subtree: candidate.subtree,
            })
        })
        .collect())
}

fn ensure_layer_prefix(
    initial: &ImageFilesystem,
    final_filesystem: &ImageFilesystem,
) -> Result<()> {
    ensure!(
        final_filesystem.layers.starts_with(&initial.layers),
        "Final Environment does not extend the Run's Initial Image layer chain"
    );
    Ok(())
}

fn apply_change_layer(
    descriptor: &Descriptor,
    bytes: &[u8],
    candidates: &mut BTreeMap<PathBuf, ChangeCandidate>,
) -> Result<()> {
    let reader = layer_reader(descriptor, bytes)?;
    let mut archive = tar::Archive::new(reader);
    for entry in archive
        .entries()
        .context("failed to read Final Image Layer tar")?
    {
        let entry = entry.context("failed to read Final Image Layer entry")?;
        let path = normalize_layer_path(&entry.path()?)?;
        if let Some(removed) = whiteout_target(&path) {
            candidates
                .retain(|candidate, _| candidate != &removed && !candidate.starts_with(&removed));
            candidates.insert(removed, ChangeCandidate::default());
        } else if is_opaque_whiteout(&path) {
            let directory = path
                .parent()
                .context("opaque whiteout has no parent")?
                .to_owned();
            candidates.retain(|candidate, _| {
                candidate == &directory || !candidate.starts_with(&directory)
            });
            candidates.entry(directory).or_default().subtree = true;
        } else {
            for (candidate_path, candidate) in candidates.iter_mut() {
                if path.starts_with(candidate_path) && path != *candidate_path {
                    candidate.final_node = Some(NodeSummary {
                        node_type: "directory",
                        size: None,
                    });
                }
            }
            let subtree = candidates
                .get(&path)
                .is_some_and(|candidate| candidate.subtree);
            candidates.insert(
                path,
                ChangeCandidate {
                    final_node: Some(summary_from_entry(&entry)?),
                    subtree,
                },
            );
        }
    }
    Ok(())
}

fn apply_initial_layer(
    descriptor: &Descriptor,
    bytes: &[u8],
    nodes: &mut BTreeMap<PathBuf, Option<NodeSummary>>,
) -> Result<()> {
    let reader = layer_reader(descriptor, bytes)?;
    let mut archive = tar::Archive::new(reader);
    for entry in archive
        .entries()
        .context("failed to read Initial Image Layer tar")?
    {
        let entry = entry.context("failed to read Initial Image Layer entry")?;
        let path = normalize_layer_path(&entry.path()?)?;
        if let Some(removed) = whiteout_target(&path) {
            clear_subtree(nodes, &removed, true);
        } else if is_opaque_whiteout(&path) {
            let directory = path.parent().context("opaque whiteout has no parent")?;
            clear_subtree(nodes, directory, false);
        } else {
            let summary = summary_from_entry(&entry)?;
            if let Some(node) = nodes.get_mut(&path) {
                *node = Some(summary.clone());
            }
            if summary.node_type != "directory" {
                clear_subtree(nodes, &path, false);
            }
            for (candidate, node) in nodes.iter_mut() {
                if path.starts_with(candidate) && path != *candidate {
                    *node = Some(NodeSummary {
                        node_type: "directory",
                        size: None,
                    });
                }
            }
        }
    }
    Ok(())
}

fn clear_subtree(
    nodes: &mut BTreeMap<PathBuf, Option<NodeSummary>>,
    root: &Path,
    include_root: bool,
) {
    for (path, node) in nodes {
        if (include_root && path == root) || (path != root && path.starts_with(root)) {
            *node = None;
        }
    }
}

fn summary_from_entry(entry: &tar::Entry<'_, Box<dyn Read + '_>>) -> Result<NodeSummary> {
    let entry_type = entry.header().entry_type();
    let (node_type, size) = if entry_type.is_file() {
        ("file", Some(entry.header().size()?))
    } else if entry_type.is_hard_link() {
        ("file", None)
    } else if entry_type.is_dir() {
        ("directory", None)
    } else if entry_type.is_symlink() {
        ("symlink", None)
    } else {
        ("special", None)
    };
    Ok(NodeSummary { node_type, size })
}

fn absolute_image_path(path: &Path) -> String {
    if path.as_os_str().is_empty() {
        "/".to_owned()
    } else {
        format!("/{}", path.display())
    }
}

fn resolve_node(image: &ImageFilesystem, target: &Path) -> Result<Option<ResolvedNode>> {
    let Some(last_layer) = image.layers.len().checked_sub(1) else {
        return Ok(None);
    };
    resolve_node_in_layers(image, target, last_layer, &mut Vec::new())
}

fn resolve_node_in_layers(
    image: &ImageFilesystem,
    target: &Path,
    last_layer: usize,
    followed: &mut Vec<PathBuf>,
) -> Result<Option<ResolvedNode>> {
    if followed.iter().any(|path| path == target) {
        bail!("Filesystem hard link cycle includes /{}", target.display());
    }
    followed.push(target.to_owned());
    for (layer_index, layer) in image.layers[..=last_layer].iter().enumerate().rev() {
        let bytes = image.read_layer(layer)?;
        match inspect_layer(layer, &bytes, target)? {
            Some(LayerDecision::Node(ResolvedNode::HardLink(link))) => {
                return resolve_node_in_layers(image, &link, layer_index, followed);
            }
            Some(LayerDecision::Node(node)) => return Ok(Some(node)),
            Some(LayerDecision::Removed) => return Ok(None),
            Some(LayerDecision::Blocked) => {
                bail!(
                    "Filesystem path traverses a non-directory node: /{}",
                    target.display()
                )
            }
            None => {}
        }
    }
    Ok(None)
}

fn inspect_layer(
    descriptor: &Descriptor,
    bytes: &[u8],
    target: &Path,
) -> Result<Option<LayerDecision>> {
    let reader = layer_reader(descriptor, bytes)?;
    let mut archive = tar::Archive::new(reader);
    let mut exact = None;
    let mut implies_directory = false;
    let mut removes_lower = false;
    let mut blocked = false;
    for entry in archive.entries().context("failed to read OCI Layer tar")? {
        let mut entry = entry.context("failed to read OCI Layer entry")?;
        let path = normalize_layer_path(&entry.path()?)?;
        if let Some(removed) = whiteout_target(&path) {
            if target.starts_with(&removed) {
                removes_lower = true;
            }
            continue;
        }
        if is_opaque_whiteout(&path) {
            if let Some(directory) = path.parent() {
                if directory == target {
                    implies_directory = true;
                } else if target.starts_with(directory) {
                    removes_lower = true;
                }
            }
            continue;
        }
        if path == target {
            exact = Some(node_from_entry(&mut entry)?);
        } else if path.starts_with(target) {
            implies_directory = true;
        } else if !path.as_os_str().is_empty()
            && target.starts_with(&path)
            && !entry.header().entry_type().is_dir()
        {
            blocked = true;
        }
    }
    Ok(exact
        .map(LayerDecision::Node)
        .or_else(|| implies_directory.then_some(LayerDecision::Node(ResolvedNode::Directory)))
        .or_else(|| blocked.then_some(LayerDecision::Blocked))
        .or_else(|| removes_lower.then_some(LayerDecision::Removed)))
}

fn node_from_entry(entry: &mut tar::Entry<'_, Box<dyn Read + '_>>) -> Result<ResolvedNode> {
    let entry_type = entry.header().entry_type();
    if entry_type.is_file() {
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes)?;
        Ok(ResolvedNode::File(bytes))
    } else if entry_type.is_dir() {
        Ok(ResolvedNode::Directory)
    } else if entry_type.is_symlink() {
        let target = entry
            .link_name()?
            .context("OCI symlink has no target")?
            .into_owned();
        Ok(ResolvedNode::Symlink(target))
    } else if entry_type.is_hard_link() {
        let target = entry.link_name()?.context("OCI hard link has no target")?;
        Ok(ResolvedNode::HardLink(normalize_layer_path(&target)?))
    } else {
        Ok(ResolvedNode::Unsupported("special"))
    }
}

fn extract_directory(image: &ImageFilesystem, target: &Path, output: &Path) -> Result<()> {
    let mut tree = BTreeMap::new();
    for layer in &image.layers {
        let bytes = image.read_layer(layer)?;
        apply_layer_to_tree(layer, &bytes, target, &mut tree)?;
    }
    if !matches!(tree.get(Path::new("")), Some(ResolvedNode::Directory)) {
        bail!("Filesystem directory extraction did not produce a directory");
    }
    write_tree(output, tree)
}

enum TreeChange {
    Node(PathBuf, ResolvedNode),
    BlocksTarget,
}

fn apply_layer_to_tree(
    descriptor: &Descriptor,
    bytes: &[u8],
    target: &Path,
    tree: &mut BTreeMap<PathBuf, ResolvedNode>,
) -> Result<()> {
    let reader = layer_reader(descriptor, bytes)?;
    let mut archive = tar::Archive::new(reader);
    let mut removals = Vec::new();
    let mut opaques = Vec::new();
    let mut changes = Vec::new();
    for entry in archive.entries().context("failed to read OCI Layer tar")? {
        let mut entry = entry.context("failed to read OCI Layer entry")?;
        let path = normalize_layer_path(&entry.path()?)?;
        if let Some(removed) = whiteout_target(&path) {
            removals.push(removed);
        } else if is_opaque_whiteout(&path) {
            opaques.push(
                path.parent()
                    .context("opaque whiteout has no parent")?
                    .to_owned(),
            );
        } else if path == target || path.starts_with(target) {
            let node = node_from_entry(&mut entry)?;
            changes.push(TreeChange::Node(
                path.strip_prefix(target)?.to_owned(),
                node,
            ));
        } else if !path.as_os_str().is_empty()
            && target.starts_with(&path)
            && !entry.header().entry_type().is_dir()
        {
            changes.push(TreeChange::BlocksTarget);
        }
    }
    for removed in removals {
        if target.starts_with(&removed) {
            tree.clear();
        } else if removed.starts_with(target) {
            remove_subtree(tree, removed.strip_prefix(target)?);
        }
    }
    for directory in opaques {
        if target.starts_with(&directory) && target != directory {
            tree.clear();
        } else if directory.starts_with(target) {
            let relative = directory.strip_prefix(target)?;
            remove_descendants(tree, relative);
            replace_tree_node(tree, relative.to_owned(), ResolvedNode::Directory);
        }
    }
    let mut hard_links = Vec::new();
    for change in changes {
        match change {
            TreeChange::Node(path, ResolvedNode::HardLink(target)) => {
                hard_links.push((path, target));
            }
            TreeChange::Node(path, node) => replace_tree_node(tree, path, node),
            TreeChange::BlocksTarget => tree.clear(),
        }
    }
    resolve_tree_hardlinks(tree, target, hard_links)?;
    Ok(())
}

fn resolve_tree_hardlinks(
    tree: &mut BTreeMap<PathBuf, ResolvedNode>,
    selected_root: &Path,
    mut pending: Vec<(PathBuf, PathBuf)>,
) -> Result<()> {
    while !pending.is_empty() {
        let mut unresolved = Vec::new();
        let mut progress = false;
        for (path, target) in pending {
            let relative_target = target.strip_prefix(selected_root).with_context(|| {
                format!(
                    "Filesystem directory hard link /{} points outside the selected directory",
                    path.display()
                )
            })?;
            match tree.get(relative_target).cloned() {
                Some(ResolvedNode::File(bytes)) => {
                    replace_tree_node(tree, path, ResolvedNode::File(bytes));
                    progress = true;
                }
                Some(ResolvedNode::HardLink(_)) | None => unresolved.push((path, target)),
                Some(_) => bail!(
                    "Filesystem hard link target is not a regular file: /{}",
                    target.display()
                ),
            }
        }
        if !progress {
            let (path, target) = &unresolved[0];
            bail!(
                "Filesystem hard link cannot be resolved: /{} -> /{}",
                path.display(),
                target.display()
            );
        }
        pending = unresolved;
    }
    Ok(())
}

fn replace_tree_node(
    tree: &mut BTreeMap<PathBuf, ResolvedNode>,
    path: PathBuf,
    node: ResolvedNode,
) {
    let mut ancestors = path.ancestors().skip(1).collect::<Vec<_>>();
    ancestors.reverse();
    for ancestor in ancestors {
        if !matches!(tree.get(ancestor), Some(ResolvedNode::Directory)) {
            remove_subtree(tree, ancestor);
            tree.insert(ancestor.to_owned(), ResolvedNode::Directory);
        }
    }
    if !matches!(
        (tree.get(&path), &node),
        (Some(ResolvedNode::Directory), ResolvedNode::Directory)
    ) {
        remove_subtree(tree, &path);
    }
    tree.insert(path, node);
}

fn remove_subtree(tree: &mut BTreeMap<PathBuf, ResolvedNode>, root: &Path) {
    tree.retain(|path, _| path != root && !path.starts_with(root));
}

fn remove_descendants(tree: &mut BTreeMap<PathBuf, ResolvedNode>, root: &Path) {
    tree.retain(|path, _| path == root || !path.starts_with(root));
}

fn write_tree(output: &Path, tree: BTreeMap<PathBuf, ResolvedNode>) -> Result<()> {
    fs::create_dir(output)
        .with_context(|| format!("failed to create output directory {}", output.display()))?;
    let result = (|| -> Result<()> {
        for (path, node) in tree {
            if path.as_os_str().is_empty() {
                continue;
            }
            let destination = output.join(path);
            match node {
                ResolvedNode::File(bytes) => write_file(&destination, &bytes)?,
                ResolvedNode::Directory => fs::create_dir(&destination)?,
                ResolvedNode::Symlink(target) => create_symlink(&target, &destination)?,
                ResolvedNode::HardLink(_) => {
                    unreachable!("directory hard links are resolved before publication")
                }
                ResolvedNode::Unsupported(kind) => {
                    bail!("Filesystem directory contains an unsupported {kind} node")
                }
            }
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(output);
    }
    result
}

fn write_file(output: &Path, bytes: &[u8]) -> Result<()> {
    let result = (|| -> Result<()> {
        let mut destination = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(output)
            .with_context(|| format!("failed to create output file {}", output.display()))?;
        destination.write_all(bytes)?;
        destination.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(output);
    }
    result
}

fn write_symlink(output: &Path, target: &Path) -> Result<()> {
    create_symlink(target, output)
        .with_context(|| format!("failed to create output symlink {}", output.display()))
}

#[cfg(unix)]
fn create_symlink(target: &Path, link: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, link)?;
    Ok(())
}

#[cfg(not(unix))]
fn create_symlink(_target: &Path, _link: &Path) -> Result<()> {
    bail!("Filesystem symlink extraction is unsupported on this host")
}

fn new_output_path(output: &Path) -> Result<PathBuf> {
    let name = output
        .file_name()
        .context("--output must identify a new local path")?;
    let parent = output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .canonicalize()
        .with_context(|| format!("failed to resolve output parent for {}", output.display()))?;
    let output = parent.join(name);
    match fs::symlink_metadata(&output) {
        Ok(_) => bail!("output path already exists: {}", output.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(output),
        Err(error) => Err(error.into()),
    }
}

fn normalize_absolute_path(value: &str) -> Result<PathBuf> {
    let path = Path::new(value);
    if !path.is_absolute() {
        bail!("Filesystem path must be absolute: {value}");
    }
    normalize_components(path.components().skip(1))
}

fn normalize_layer_path(path: &Path) -> Result<PathBuf> {
    normalize_components(path.components())
}

fn normalize_components<'a>(components: impl Iterator<Item = Component<'a>>) -> Result<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in components {
        match component {
            Component::Normal(value) => normalized.push(value),
            Component::CurDir | Component::RootDir => {}
            Component::ParentDir | Component::Prefix(_) => {
                bail!("OCI path escapes the Image root")
            }
        }
    }
    Ok(normalized)
}

fn whiteout_target(path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?.to_str()?;
    let removed = name.strip_prefix(".wh.")?;
    if removed == ".wh..opq" || removed.is_empty() {
        return None;
    }
    Some(path.parent().unwrap_or_else(|| Path::new("")).join(removed))
}

fn is_opaque_whiteout(path: &Path) -> bool {
    path.file_name() == Some(OsStr::new(".wh..wh..opq"))
}

fn sha256_digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    format!("sha256:{encoded}")
}
