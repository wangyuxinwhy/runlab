use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
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

enum ResolvedNode {
    File(Vec<u8>),
    Directory,
    Symlink(PathBuf),
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
        let node = resolve_node(&image, &target)?
            .with_context(|| format!("Filesystem path does not exist: {path}"))?;
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
                let record = self
                    .database
                    .run_get(&run_id)?
                    .with_context(|| format!("Run does not exist: {run_id}"))?;
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

fn final_environment(record: &StoredRun, program: &str) -> Result<Descriptor> {
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

fn resolve_node(image: &ImageFilesystem, target: &Path) -> Result<Option<ResolvedNode>> {
    for layer in image.layers.iter().rev() {
        let bytes = image.read_layer(layer)?;
        match inspect_layer(layer, &bytes, target)? {
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
        Ok(ResolvedNode::Unsupported("hard_link"))
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
    for change in changes {
        match change {
            TreeChange::Node(path, node) => replace_tree_node(tree, path, node),
            TreeChange::BlocksTarget => tree.clear(),
        }
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
