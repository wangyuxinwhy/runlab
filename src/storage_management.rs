use std::collections::{BTreeSet, VecDeque};
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
}

#[derive(Clone, Debug, Default, Serialize)]
struct ReclaimableFacts {
    unreferenced_oci_blobs: usize,
    unreferenced_oci_bytes: u64,
    snapshot_cache_bytes: u64,
    invocation_staging_bytes: u64,
    total_bytes: u64,
}

struct Inspection {
    status: StorageStatus,
    unreferenced_blobs: Vec<PathBuf>,
    unreferenced_snapshots: Vec<PathBuf>,
    invocation_entries: Vec<PathBuf>,
}

pub(crate) fn status(state: &State) -> Result<StorageStatus> {
    Ok(inspect(state)?.status)
}

pub(crate) fn prune(state: &State, apply: bool) -> Result<PruneResult> {
    let inspection = inspect(state)?;
    let planned = inspection.status.reclaimable.clone();
    if !apply {
        return Ok(PruneResult {
            schema_version: 1,
            mode: "check",
            exclusive: false,
            removed: ReclaimableFacts::default(),
            remaining_reclaimable: planned,
        });
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
    let remaining = inspect(state)?.status.reclaimable;
    Ok(PruneResult {
        schema_version: 1,
        mode: "apply",
        exclusive: true,
        removed: planned,
        remaining_reclaimable: remaining,
    })
}

fn inspect(state: &State) -> Result<Inspection> {
    let root = state.root();
    let facts = state.database().storage_facts()?;
    let references = referenced_oci(state, &facts)?;
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
    let unreferenced_snapshots =
        unreferenced_snapshot_entries(&snapshot_cache, &references.snapshot_chain_ids)?;
    let snapshot_cache_bytes = unreferenced_snapshots
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
        snapshot_cache_bytes,
        invocation_staging_bytes,
        total_bytes: total_reclaimable,
    };
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
                missing_referenced_blobs: references.missing,
            },
            reclaimable,
        },
        unreferenced_blobs,
        unreferenced_snapshots,
        invocation_entries,
    })
}

struct OciReferences {
    digests: BTreeSet<String>,
    missing: Vec<String>,
    snapshot_chain_ids: BTreeSet<String>,
}

fn referenced_oci(state: &State, facts: &StorageDatabaseFacts) -> Result<OciReferences> {
    let mut roots = VecDeque::new();
    for document in &facts.descriptor_documents {
        collect_manifest_descriptors(document, &mut roots)?;
    }
    let store = state.oci();
    let mut digests = BTreeSet::new();
    let mut missing = BTreeSet::new();
    let mut snapshot_chain_ids = BTreeSet::new();
    while let Some(descriptor) = roots.pop_front() {
        let digest = descriptor.digest().to_string();
        if !digests.insert(digest.clone()) {
            continue;
        }
        let Ok(bytes) = store.read(&descriptor) else {
            missing.insert(digest);
            continue;
        };
        let Ok(manifest) = serde_json::from_slice::<ImageManifest>(&bytes) else {
            missing.insert(digest);
            continue;
        };
        let config_descriptor = manifest.config();
        if let Ok(config_bytes) = store.read(config_descriptor)
            && let Ok(config) = serde_json::from_slice::<ImageConfiguration>(&config_bytes)
            && manifest.layers().len() == config.rootfs().diff_ids().len()
        {
            let mut parent = None;
            for diff_id in config.rootfs().diff_ids() {
                let id = snapshot_chain_id(parent.as_deref(), diff_id)?;
                snapshot_chain_ids.insert(id.clone());
                parent = Some(id);
            }
        }
        digests.insert(manifest.config().digest().to_string());
        digests.extend(
            manifest
                .layers()
                .iter()
                .map(|layer| layer.digest().to_string()),
        );
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
    })
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

fn unreferenced_snapshot_entries(
    root: &Path,
    reachable: &BTreeSet<String>,
) -> Result<Vec<PathBuf>> {
    let mut entries = Vec::new();
    for path in children(&root.join("chains"))? {
        let name = path.file_name().and_then(|name| name.to_str());
        if name.is_none_or(|name| !reachable.contains(name)) {
            entries.push(path);
        }
    }
    for path in children(&root.join("inventories"))? {
        let chain = path
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        if !reachable.contains(chain) {
            entries.push(path);
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
    Ok(entries)
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
