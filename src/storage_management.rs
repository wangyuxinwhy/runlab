use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use oci_spec::image::{Descriptor, ImageConfiguration, ImageManifest};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest as _, Sha256};

use crate::state::State;
use crate::storage::StorageDatabaseFacts;

#[derive(Debug, Serialize)]
pub(crate) struct StorageStatus {
    schema_version: u32,
    filesystem: FilesystemCapacity,
    usage: StorageUsage,
    assets: AssetFacts,
    reclaimable: ReclaimableFacts,
}

#[derive(Debug, Serialize)]
pub(crate) struct PruneResult {
    schema_version: u32,
    mode: &'static str,
    exclusive: bool,
    without_runs: Vec<String>,
    reference_graph_complete: bool,
    reference_issues: Vec<ReferenceIssue>,
    removed: ReclaimableFacts,
    remaining_reclaimable: ReclaimableFacts,
}

#[derive(Debug, Serialize)]
struct FilesystemCapacity {
    #[serde(rename = "total_bytes")]
    total: u64,
    #[serde(rename = "used_bytes")]
    used: u64,
    #[serde(rename = "available_bytes")]
    available: u64,
}

#[derive(Debug, Serialize)]
struct StorageUsage {
    #[serde(rename = "state_bytes")]
    state: u64,
    #[serde(rename = "database_bytes")]
    database: u64,
    #[serde(rename = "oci_bytes")]
    oci: u64,
    #[serde(rename = "snapshot_cache_bytes")]
    snapshot_cache: u64,
    #[serde(rename = "invocation_staging_bytes")]
    invocation_staging: u64,
    #[serde(rename = "other_state_bytes")]
    other_state: u64,
}

#[derive(Debug, Serialize)]
struct AssetFacts {
    catalog_images: u64,
    runs: u64,
    active_runs: u64,
    referenced_oci_blobs: usize,
    referenced_oci_bytes: u64,
    missing_referenced_blobs: Vec<String>,
    reference_graph_complete: bool,
    reference_issues: Vec<ReferenceIssue>,
}

#[derive(Clone, Debug, Default, Serialize)]
struct ReclaimableFacts {
    unreferenced_oci_blobs: usize,
    unreferenced_oci_bytes: u64,
    unreferenced_snapshot_chains: usize,
    snapshot_cache_bytes: u64,
    cold_cache_after_apply: bool,
    invocation_staging_bytes: u64,
    total_bytes: u64,
}

#[derive(Clone, Debug, Serialize)]
struct ReferenceIssue {
    kind: &'static str,
    digest: String,
    detail: String,
}

struct Inspection {
    status: StorageStatus,
    reference_graph_complete: bool,
    reference_issues: Vec<ReferenceIssue>,
    unreferenced_blobs: Vec<PathBuf>,
    unreferenced_snapshots: Vec<PathBuf>,
    invocation_entries: Vec<PathBuf>,
}

pub(crate) fn status(state: &State) -> Result<StorageStatus> {
    Ok(inspect(state, &BTreeSet::new())?.status)
}

pub(crate) fn prune(
    state: &State,
    apply: bool,
    without_runs: &BTreeSet<String>,
) -> Result<PruneResult> {
    let inspection = inspect(state, without_runs)?;
    let planned = inspection.status.reclaimable.clone();
    if !apply {
        return Ok(PruneResult {
            schema_version: 1,
            mode: "check",
            exclusive: false,
            without_runs: without_runs.iter().cloned().collect(),
            reference_graph_complete: inspection.reference_graph_complete,
            reference_issues: inspection.reference_issues,
            removed: ReclaimableFacts::default(),
            remaining_reclaimable: planned,
        });
    }
    if !inspection.reference_graph_complete {
        return Err(crate::error::classify(
            anyhow::anyhow!(
                "OCI reference graph is incomplete; storage prune refused to remove any content"
            ),
            crate::error::ErrorFacts {
                category: crate::error::ErrorCategory::Conflict,
                stage: "storage_prune",
                run_id: None,
                accepted: Some(false),
                run_created: Some(false),
                retryable: false,
                recovery: Some(
                    "run `runlab storage prune check` and inspect `reference_issues` before retrying"
                        .to_owned(),
                ),
            },
        ));
    }

    for path in &inspection.unreferenced_blobs {
        fs::remove_file(path).with_context(|| {
            format!("failed to remove unreferenced OCI blob {}", path.display())
        })?;
    }
    for path in &inspection.unreferenced_snapshots {
        remove_node(path)?;
    }
    for path in &inspection.invocation_entries {
        remove_node(path)?;
    }
    let remaining = inspect(state, &BTreeSet::new())?.status.reclaimable;
    Ok(PruneResult {
        schema_version: 1,
        mode: "apply",
        exclusive: true,
        without_runs: Vec::new(),
        reference_graph_complete: true,
        reference_issues: Vec::new(),
        removed: planned,
        remaining_reclaimable: remaining,
    })
}

fn inspect(state: &State, without_runs: &BTreeSet<String>) -> Result<Inspection> {
    let root = state.root();
    let facts = state.database().storage_facts()?;
    validate_excluded_runs(&facts, without_runs)?;
    let references = referenced_oci(state, &facts, without_runs)?;
    let blobs_root = root.join("oci/blobs/sha256");
    let blobs = blob_files(&blobs_root)?;
    let mut unreferenced_blobs = Vec::new();
    let mut unreferenced_oci_bytes = 0_u64;
    let mut referenced_oci_bytes = 0_u64;
    for (digest, path, bytes) in blobs {
        if references.digests.contains(&digest) {
            referenced_oci_bytes = referenced_oci_bytes.saturating_add(bytes);
        } else {
            unreferenced_oci_bytes = unreferenced_oci_bytes.saturating_add(bytes);
            unreferenced_blobs.push(path);
        }
    }

    let snapshot_cache = root.join("engine/snapshots-v3");
    let snapshot_usage_bytes = allocated_children(&snapshot_cache)?;
    let snapshot_reclaim =
        unreferenced_snapshot_entries(&snapshot_cache, &references.snapshot_chain_ids)?;
    let snapshot_cache_bytes = snapshot_reclaim
        .entries
        .iter()
        .try_fold(0_u64, |total, path| {
            Ok::<u64, anyhow::Error>(total.saturating_add(allocated_bytes(path)?))
        })?;
    let invocations = root.join("engine/invocations");
    let invocation_entries = children(&invocations)?;
    let invocation_staging_bytes = allocated_children(&invocations)?;
    let oci_bytes = allocated_bytes(&root.join("oci"))?;
    let database_bytes = database_bytes(root)?;
    let state_bytes = allocated_bytes(root)?;
    let known = database_bytes
        .saturating_add(oci_bytes)
        .saturating_add(snapshot_usage_bytes)
        .saturating_add(invocation_staging_bytes);
    let total_reclaimable = unreferenced_oci_bytes
        .saturating_add(snapshot_cache_bytes)
        .saturating_add(invocation_staging_bytes);
    let reclaimable = ReclaimableFacts {
        unreferenced_oci_blobs: unreferenced_blobs.len(),
        unreferenced_oci_bytes,
        unreferenced_snapshot_chains: snapshot_reclaim.chains,
        snapshot_cache_bytes,
        cold_cache_after_apply: snapshot_reclaim.affects_warm_cache,
        invocation_staging_bytes,
        total_bytes: total_reclaimable,
    };
    let reference_graph_complete = references.issues.is_empty() && references.missing.is_empty();
    Ok(Inspection {
        status: StorageStatus {
            schema_version: 1,
            filesystem: filesystem_capacity(root)?,
            usage: StorageUsage {
                state: state_bytes,
                database: database_bytes,
                oci: oci_bytes,
                snapshot_cache: snapshot_usage_bytes,
                invocation_staging: invocation_staging_bytes,
                other_state: state_bytes.saturating_sub(known),
            },
            assets: AssetFacts {
                catalog_images: facts.catalog_images,
                runs: facts.runs,
                active_runs: facts.active_runs,
                referenced_oci_blobs: references.digests.len(),
                referenced_oci_bytes,
                missing_referenced_blobs: references.missing.clone(),
                reference_graph_complete,
                reference_issues: references.issues.clone(),
            },
            reclaimable,
        },
        reference_graph_complete,
        reference_issues: references.issues,
        unreferenced_blobs,
        unreferenced_snapshots: snapshot_reclaim.entries,
        invocation_entries,
    })
}

struct OciReferences {
    digests: BTreeSet<String>,
    missing: Vec<String>,
    snapshot_chain_ids: BTreeSet<String>,
    issues: Vec<ReferenceIssue>,
}

fn validate_excluded_runs(
    facts: &StorageDatabaseFacts,
    without_runs: &BTreeSet<String>,
) -> Result<()> {
    let by_id = facts
        .run_descriptor_documents
        .iter()
        .map(|run| (run.run_id.as_str(), run))
        .collect::<BTreeMap<_, _>>();
    for run_id in without_runs {
        let run = by_id
            .get(run_id.as_str())
            .with_context(|| format!("--without-runs Run does not exist: {run_id}"))?;
        if !run.terminal {
            bail!("--without-runs requires a terminal Run: {run_id}");
        }
    }
    Ok(())
}

#[expect(
    clippy::too_many_lines,
    reason = "one fail-closed traversal keeps OCI and snapshot reachability decisions inseparable"
)]
fn referenced_oci(
    state: &State,
    facts: &StorageDatabaseFacts,
    without_runs: &BTreeSet<String>,
) -> Result<OciReferences> {
    let mut roots = VecDeque::new();
    for document in &facts.catalog_descriptor_documents {
        collect_manifest_descriptors(document, &mut roots)?;
    }
    for run in &facts.run_descriptor_documents {
        if without_runs.contains(&run.run_id) {
            continue;
        }
        for document in &run.descriptor_documents {
            collect_manifest_descriptors(document, &mut roots)?;
        }
    }
    let store = state.oci();
    let mut digests = BTreeSet::new();
    let mut missing = BTreeSet::new();
    let mut snapshot_chain_ids = BTreeSet::new();
    let mut issues = Vec::new();
    let mut checked_layers = BTreeSet::new();
    while let Some(descriptor) = roots.pop_front() {
        let digest = descriptor.digest().to_string();
        if !digests.insert(digest.clone()) {
            continue;
        }
        let bytes = match store.read(&descriptor) {
            Ok(bytes) => bytes,
            Err(error) => {
                record_unavailable(
                    &store,
                    &mut missing,
                    &mut issues,
                    "manifest_unavailable",
                    &descriptor,
                    &error,
                )?;
                continue;
            }
        };
        let manifest = match serde_json::from_slice::<ImageManifest>(&bytes) {
            Ok(manifest) => manifest,
            Err(error) => {
                issues.push(ReferenceIssue {
                    kind: "manifest_invalid",
                    digest,
                    detail: error.to_string(),
                });
                continue;
            }
        };
        let config_descriptor = manifest.config();
        digests.insert(config_descriptor.digest().to_string());
        digests.extend(
            manifest
                .layers()
                .iter()
                .map(|layer| layer.digest().to_string()),
        );
        for layer in manifest.layers() {
            if checked_layers.insert(layer.digest().to_string())
                && let Err(error) = store.check_available(layer)
            {
                record_unavailable(
                    &store,
                    &mut missing,
                    &mut issues,
                    "layer_unavailable",
                    layer,
                    &error,
                )?;
            }
        }
        let config_bytes = match store.read(config_descriptor) {
            Ok(bytes) => bytes,
            Err(error) => {
                record_unavailable(
                    &store,
                    &mut missing,
                    &mut issues,
                    "config_unavailable",
                    config_descriptor,
                    &error,
                )?;
                continue;
            }
        };
        let config = match serde_json::from_slice::<ImageConfiguration>(&config_bytes) {
            Ok(config) => config,
            Err(error) => {
                issues.push(ReferenceIssue {
                    kind: "config_invalid",
                    digest: config_descriptor.digest().to_string(),
                    detail: error.to_string(),
                });
                continue;
            }
        };
        if manifest.layers().len() != config.rootfs().diff_ids().len() {
            issues.push(ReferenceIssue {
                kind: "layer_diffid_count_mismatch",
                digest: descriptor.digest().to_string(),
                detail: format!(
                    "Manifest has {} Layers but Image Config has {} DiffIDs",
                    manifest.layers().len(),
                    config.rootfs().diff_ids().len()
                ),
            });
            continue;
        }
        let mut parent = None;
        for diff_id in config.rootfs().diff_ids() {
            match snapshot_chain_id(parent.as_deref(), diff_id) {
                Ok(id) => {
                    snapshot_chain_ids.insert(id.clone());
                    parent = Some(id);
                }
                Err(error) => {
                    issues.push(ReferenceIssue {
                        kind: "snapshot_chain_invalid",
                        digest: config_descriptor.digest().to_string(),
                        detail: error.to_string(),
                    });
                    break;
                }
            }
        }
    }
    for digest in &digests {
        if !store.blob_path(digest)?.is_file() {
            missing.insert(digest.clone());
        }
    }
    Ok(OciReferences {
        digests,
        missing: missing.into_iter().collect(),
        snapshot_chain_ids,
        issues,
    })
}

fn record_unavailable(
    store: &crate::storage::LocalOciStore,
    missing: &mut BTreeSet<String>,
    issues: &mut Vec<ReferenceIssue>,
    kind: &'static str,
    descriptor: &Descriptor,
    error: &anyhow::Error,
) -> Result<()> {
    let digest = descriptor.digest().to_string();
    if !store.blob_path(&digest)?.is_file() {
        missing.insert(digest.clone());
    }
    issues.push(ReferenceIssue {
        kind,
        digest,
        detail: error.to_string(),
    });
    Ok(())
}

fn collect_manifest_descriptors(value: &Value, roots: &mut VecDeque<Descriptor>) -> Result<()> {
    match value {
        Value::Object(object) => {
            if object.get("mediaType").and_then(Value::as_str)
                == Some("application/vnd.oci.image.manifest.v1+json")
            {
                roots.push_back(
                    serde_json::from_value(value.clone())
                        .context("stored OCI Image Manifest descriptor is invalid")?,
                );
                return Ok(());
            }
            for child in object.values() {
                collect_manifest_descriptors(child, roots)?;
            }
        }
        Value::Array(values) => {
            for child in values {
                collect_manifest_descriptors(child, roots)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn blob_files(root: &Path) -> Result<Vec<(String, PathBuf, u64)>> {
    let mut blobs = Vec::new();
    for path in children(root)? {
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        blobs.push((
            format!("sha256:{name}"),
            path,
            metadata.blocks().saturating_mul(512),
        ));
    }
    Ok(blobs)
}

fn filesystem_capacity(path: &Path) -> Result<FilesystemCapacity> {
    let output = Command::new("df")
        .args(["-B1", "--output=size,used,avail"])
        .arg(path)
        .output()
        .context("failed to inspect State filesystem capacity")?;
    if !output.status.success() {
        bail!("df failed while inspecting State filesystem capacity");
    }
    let line = std::str::from_utf8(&output.stdout)?
        .lines()
        .nth(1)
        .context("df returned no filesystem capacity row")?;
    let values = line
        .split_whitespace()
        .map(str::parse::<u64>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let [total_bytes, used_bytes, available_bytes] = values.as_slice() else {
        bail!("df returned an invalid filesystem capacity row");
    };
    Ok(FilesystemCapacity {
        total: *total_bytes,
        used: *used_bytes,
        available: *available_bytes,
    })
}

fn database_bytes(root: &Path) -> Result<u64> {
    ["runlab.sqlite3", "runlab.sqlite3-wal", "runlab.sqlite3-shm"]
        .into_iter()
        .try_fold(0_u64, |total, name| {
            Ok(total.saturating_add(allocated_bytes(&root.join(name))?))
        })
}

fn allocated_bytes(path: &Path) -> Result<u64> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error.into()),
    };
    let mut total = metadata.blocks().saturating_mul(512);
    if metadata.is_dir() {
        for child in fs::read_dir(path)? {
            total = total.saturating_add(allocated_bytes(&child?.path())?);
        }
    }
    Ok(total)
}

fn allocated_children(path: &Path) -> Result<u64> {
    children(path)?.into_iter().try_fold(0_u64, |total, child| {
        Ok(total.saturating_add(allocated_bytes(&child)?))
    })
}

struct SnapshotReclaim {
    entries: Vec<PathBuf>,
    chains: usize,
    affects_warm_cache: bool,
}

fn unreferenced_snapshot_entries(
    root: &Path,
    reachable: &BTreeSet<String>,
) -> Result<SnapshotReclaim> {
    let mut entries = Vec::new();
    let mut chains = 0_usize;
    let mut affects_warm_cache = false;
    for path in children(&root.join("chains"))? {
        let name = path.file_name().and_then(|name| name.to_str());
        if name.is_none_or(|name| !reachable.contains(name)) {
            entries.push(path);
            chains += 1;
            affects_warm_cache = true;
        }
    }
    for path in children(&root.join("inventories"))? {
        let chain = path
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        if !reachable.contains(chain) {
            entries.push(path);
            affects_warm_cache = true;
        }
    }
    for path in children(root)? {
        if !matches!(
            path.file_name().and_then(|name| name.to_str()),
            Some("chains" | "inventories" | "empty")
        ) {
            entries.push(path);
        }
    }
    Ok(SnapshotReclaim {
        entries,
        chains,
        affects_warm_cache,
    })
}

fn snapshot_chain_id(parent: Option<&str>, diff_id: &str) -> Result<String> {
    let encoded = diff_id
        .strip_prefix("sha256:")
        .context("snapshot DiffID does not use sha256")?;
    if parent.is_none() {
        return Ok(encoded.to_owned());
    }
    let mut hash = Sha256::new();
    hash.update(parent.expect("parent was checked").as_bytes());
    hash.update(b" ");
    hash.update(diff_id.as_bytes());
    let mut encoded = String::with_capacity(64);
    for byte in hash.finalize() {
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    Ok(encoded)
}

fn children(path: &Path) -> Result<Vec<PathBuf>> {
    match fs::read_dir(path) {
        Ok(entries) => entries
            .map(|entry| entry.map(|entry| entry.path()).map_err(Into::into))
            .collect(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error.into()),
    }
}

fn remove_node(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_dir() {
        fs::remove_dir_all(path)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(())
}
