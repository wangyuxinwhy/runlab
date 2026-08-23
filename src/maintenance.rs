//! Verifying and reclaiming local state: checking that a Run Record still
//! matches its stored bytes, and planning and applying garbage collection.
//!
//! Collection is two steps by design. A plan is a document you can read before
//! anything is deleted, and applying one only removes what the plan named.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::catalog::LocalImageCatalog;
use crate::core::{Digest, OciDescriptor, RunId};
use crate::image::ImageService;
use crate::integrity::{canonical_json, digest_bytes};
use crate::oci::{OciLayout, StoredBlob};
use crate::storage::{
    RunDatabase, RunImageParticipant, RunImageSlot, RunRetentionSnapshot, StoredRunLifecycle,
};

pub(crate) const MAX_STATE_GC_PLAN_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct RunVerifyResult {
    pub(crate) schema_version: u32,
    pub(crate) run_id: RunId,
    pub(crate) lifecycle: &'static str,
    pub(crate) valid: bool,
    pub(crate) image_roots: u64,
    pub(crate) verified_stored_bytes: u64,
    pub(crate) verified_stored_bytes_size: u64,
    pub(crate) verified_oci_blobs: u64,
    pub(crate) verified_oci_bytes: u64,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct StateVerifyResult {
    pub(crate) schema_version: u32,
    pub(crate) valid: bool,
    pub(crate) catalog_entries: u64,
    pub(crate) runs: u64,
    pub(crate) accepted_runs: u64,
    pub(crate) image_roots: u64,
    pub(crate) rooted_manifests: u64,
    pub(crate) verified_stored_bytes: u64,
    pub(crate) verified_stored_bytes_size: u64,
    pub(crate) reachable_oci_blobs: u64,
    pub(crate) reachable_oci_bytes: u64,
    pub(crate) orphan_oci_blobs: u64,
    pub(crate) orphan_oci_bytes: u64,
    pub(crate) staging_entries: u64,
    pub(crate) recovery_entries: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct StateGcPlan {
    pub(crate) schema_version: u32,
    pub(crate) created_at: DateTime<Utc>,
    pub(crate) roots_digest: Digest,
    pub(crate) roots: Vec<StateGcRoot>,
    pub(crate) reachable_oci_blobs: u64,
    pub(crate) reachable_oci_bytes: u64,
    pub(crate) delete: Vec<StateGcBlob>,
    pub(crate) plan_digest: Digest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct StateGcRoot {
    pub(crate) owner: StateGcRootOwner,
    pub(crate) manifest: OciDescriptor,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case", tag = "kind")]
pub(crate) enum StateGcRootOwner {
    OciIndex {
        reference: Option<String>,
    },
    Run {
        run_id: RunId,
        participant: StateGcParticipant,
        slot: StateGcImageSlot,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case", tag = "kind")]
pub(crate) enum StateGcParticipant {
    Primary {},
    ManagedService { name: crate::core::ServiceName },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StateGcImageSlot {
    Initial,
    Final,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct StateGcBlob {
    pub(crate) digest: Digest,
    pub(crate) size: u64,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct StateGcPlanResult {
    pub(crate) schema_version: u32,
    pub(crate) output: String,
    pub(crate) plan_digest: Digest,
    pub(crate) roots: u64,
    pub(crate) reachable_oci_blobs: u64,
    pub(crate) reachable_oci_bytes: u64,
    pub(crate) delete_oci_blobs: u64,
    pub(crate) delete_oci_bytes: u64,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct StateGcApplyResult {
    pub(crate) schema_version: u32,
    pub(crate) plan_digest: Digest,
    pub(crate) deleted_oci_blobs: u64,
    pub(crate) deleted_oci_bytes: u64,
    pub(crate) already_absent_oci_blobs: u64,
    pub(crate) already_absent_oci_bytes: u64,
    pub(crate) skipped_reachable_oci_blobs: u64,
    pub(crate) skipped_reachable_oci_bytes: u64,
    pub(crate) failed: u64,
    pub(crate) failures: Vec<StateGcFailure>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct StateGcFailure {
    pub(crate) digest: Option<Digest>,
    pub(crate) message: String,
}

struct StateSnapshot {
    layout: OciLayout,
    catalog_entries: usize,
    runs: RunRetentionSnapshot,
    roots: Vec<StateGcRoot>,
    graph: BTreeMap<Digest, OciDescriptor>,
    blobs: Vec<StoredBlob>,
    staging_entries: u64,
    recovery_entries: u64,
}

#[derive(Serialize)]
struct StateGcPlanPayload {
    schema_version: u32,
    created_at: DateTime<Utc>,
    roots_digest: Digest,
    roots: Vec<StateGcRoot>,
    reachable_oci_blobs: u64,
    reachable_oci_bytes: u64,
    delete: Vec<StateGcBlob>,
}

impl StateGcPlan {
    pub(crate) fn encoded(&self) -> Result<Vec<u8>> {
        let bytes = canonical_json(self)?;
        if u64::try_from(bytes.len()).context("state GC plan size overflow")?
            > MAX_STATE_GC_PLAN_BYTES
        {
            bail!("state GC plan exceeds {MAX_STATE_GC_PLAN_BYTES} bytes");
        }
        Ok(bytes)
    }

    pub(crate) fn verify(&self) -> Result<()> {
        if self.schema_version != 1 {
            bail!(
                "unsupported state GC plan schema version: {}",
                self.schema_version
            );
        }
        let roots_digest = digest_bytes(&canonical_json(&self.roots)?);
        if roots_digest != self.roots_digest {
            bail!("state GC plan roots digest mismatch");
        }
        if self
            .delete
            .windows(2)
            .any(|pair| pair[0].digest >= pair[1].digest)
        {
            bail!("state GC plan delete candidates must be strictly digest-sorted");
        }
        let payload = StateGcPlanPayload {
            schema_version: self.schema_version,
            created_at: self.created_at,
            roots_digest: self.roots_digest.clone(),
            roots: self.roots.clone(),
            reachable_oci_blobs: self.reachable_oci_blobs,
            reachable_oci_bytes: self.reachable_oci_bytes,
            delete: self.delete.clone(),
        };
        if digest_bytes(&canonical_json(&payload)?) != self.plan_digest {
            bail!("state GC plan digest mismatch");
        }
        Ok(())
    }

    pub(crate) fn delete_bytes(&self) -> Result<u64> {
        gc_blob_bytes(self.delete.iter())
    }
}

pub(crate) fn verify_run(state: &Path, run_id: RunId) -> Result<RunVerifyResult> {
    let database = RunDatabase::open_existing(state.join("runs.sqlite3"))?;
    let run = database.retention_snapshot_for(run_id)?;
    let layout = OciLayout::open_existing(state.join("oci"))?;
    let graph = verify_image_graphs(
        &layout,
        run.image_roots.iter().map(|root| root.descriptor.clone()),
    )?;
    Ok(RunVerifyResult {
        schema_version: 1,
        run_id,
        lifecycle: lifecycle_name(run.lifecycle),
        valid: true,
        image_roots: count(run.image_roots.len(), "Run Image root count")?,
        verified_stored_bytes: count(run.verified_stored_bytes_count, "Run stored bytes count")?,
        verified_stored_bytes_size: run.verified_stored_bytes_size,
        verified_oci_blobs: count(graph.len(), "Run OCI blob count")?,
        verified_oci_bytes: descriptor_bytes(graph.values())?,
    })
}

pub(crate) fn verify_state(state: &Path) -> Result<StateVerifyResult> {
    let snapshot = state_snapshot(state)?;
    let stored = snapshot
        .blobs
        .iter()
        .map(|blob| (blob.digest.clone(), blob))
        .collect::<BTreeMap<_, _>>();
    for descriptor in snapshot.graph.values() {
        let blob = stored
            .get(&descriptor.digest)
            .with_context(|| format!("reachable OCI blob is unavailable: {}", descriptor.digest))?;
        if blob.size != descriptor.size {
            bail!(
                "reachable OCI blob size conflicts with descriptor: {}",
                descriptor.digest
            );
        }
    }
    let orphans = snapshot
        .blobs
        .iter()
        .filter(|blob| !snapshot.graph.contains_key(&blob.digest))
        .collect::<Vec<_>>();
    Ok(StateVerifyResult {
        schema_version: 1,
        valid: true,
        catalog_entries: count(snapshot.catalog_entries, "Catalog entry count")?,
        runs: count(snapshot.runs.runs.len(), "Run count")?,
        accepted_runs: count(snapshot.runs.accepted_count, "accepted Run count")?,
        image_roots: count(snapshot.roots.len(), "state Image root count")?,
        rooted_manifests: count(
            snapshot
                .graph
                .values()
                .filter(|descriptor| descriptor.media_type == crate::core::OCI_IMAGE_MANIFEST)
                .count(),
            "rooted Manifest count",
        )?,
        verified_stored_bytes: count(
            snapshot.runs.verified_stored_bytes_count,
            "stored bytes count",
        )?,
        verified_stored_bytes_size: snapshot.runs.verified_stored_bytes_size,
        reachable_oci_blobs: count(snapshot.graph.len(), "reachable OCI blob count")?,
        reachable_oci_bytes: descriptor_bytes(snapshot.graph.values())?,
        orphan_oci_blobs: count(orphans.len(), "orphan OCI blob count")?,
        orphan_oci_bytes: orphans.iter().try_fold(0_u64, |total, blob| {
            total
                .checked_add(blob.size)
                .context("orphan OCI byte count overflow")
        })?,
        staging_entries: snapshot.staging_entries,
        recovery_entries: snapshot.recovery_entries,
    })
}

pub(crate) fn plan_gc(state: &Path) -> Result<StateGcPlan> {
    let snapshot = state_snapshot(state)?;
    require_quiescent_runs(&snapshot)?;
    let delete = snapshot
        .blobs
        .iter()
        .filter(|blob| !snapshot.graph.contains_key(&blob.digest))
        .map(|blob| StateGcBlob {
            digest: blob.digest.clone(),
            size: blob.size,
        })
        .collect::<Vec<_>>();
    let roots_digest = digest_bytes(&canonical_json(&snapshot.roots)?);
    let payload = StateGcPlanPayload {
        schema_version: 1,
        created_at: Utc::now(),
        roots_digest,
        roots: snapshot.roots,
        reachable_oci_blobs: count(snapshot.graph.len(), "reachable OCI blob count")?,
        reachable_oci_bytes: descriptor_bytes(snapshot.graph.values())?,
        delete,
    };
    let plan_digest = digest_bytes(&canonical_json(&payload)?);
    let plan = StateGcPlan {
        schema_version: payload.schema_version,
        created_at: payload.created_at,
        roots_digest: payload.roots_digest,
        roots: payload.roots,
        reachable_oci_blobs: payload.reachable_oci_blobs,
        reachable_oci_bytes: payload.reachable_oci_bytes,
        delete: payload.delete,
        plan_digest,
    };
    plan.encoded()?;
    Ok(plan)
}

pub(crate) fn apply_gc(state: &Path, plan: &StateGcPlan) -> Result<StateGcApplyResult> {
    plan.verify()?;
    let snapshot = state_snapshot(state)?;
    require_quiescent_runs(&snapshot)?;
    let mut pending = Vec::new();
    let mut already_absent = Vec::new();
    let mut skipped_reachable = Vec::new();
    for candidate in &plan.delete {
        if snapshot.graph.contains_key(&candidate.digest) {
            skipped_reachable.push(candidate);
            continue;
        }
        let stored = StoredBlob {
            digest: candidate.digest.clone(),
            size: candidate.size,
        };
        if snapshot.layout.verify_stored_blob(&stored)? {
            pending.push(candidate);
        } else {
            already_absent.push(candidate);
        }
    }

    let mut deleted = Vec::new();
    let mut failures = Vec::new();
    let mut failed = 0_u64;
    for candidate in pending {
        match snapshot.layout.remove_blob(&candidate.digest) {
            Ok(true) => deleted.push(candidate),
            Ok(false) => already_absent.push(candidate),
            Err(error) => {
                failed = failed.checked_add(1).context("GC failure count overflow")?;
                if failures.len() < 100 {
                    failures.push(StateGcFailure {
                        digest: Some(candidate.digest.clone()),
                        message: format!("{error:#}"),
                    });
                }
            }
        }
    }
    if (!deleted.is_empty() || !already_absent.is_empty())
        && let Err(error) = snapshot.layout.sync_blob_directory()
    {
        failed = failed.checked_add(1).context("GC failure count overflow")?;
        if failures.len() < 100 {
            failures.push(StateGcFailure {
                digest: None,
                message: format!("{error:#}"),
            });
        }
    }
    Ok(StateGcApplyResult {
        schema_version: 1,
        plan_digest: plan.plan_digest.clone(),
        deleted_oci_blobs: count(deleted.len(), "deleted OCI blob count")?,
        deleted_oci_bytes: gc_blob_bytes(deleted.iter().copied())?,
        already_absent_oci_blobs: count(already_absent.len(), "already absent OCI blob count")?,
        already_absent_oci_bytes: gc_blob_bytes(already_absent.iter().copied())?,
        skipped_reachable_oci_blobs: count(
            skipped_reachable.len(),
            "skipped reachable OCI blob count",
        )?,
        skipped_reachable_oci_bytes: gc_blob_bytes(skipped_reachable.iter().copied())?,
        failed,
        failures,
    })
}

fn state_snapshot(state: &Path) -> Result<StateSnapshot> {
    let layout = OciLayout::open_existing(state.join("oci"))?;
    let catalog_entries = LocalImageCatalog::new(&layout).list()?.len();
    let index_roots = layout.manifest_root_entries()?;
    let runs = retention_snapshot_if_present(state)?;
    let mut roots = index_roots
        .into_iter()
        .map(|root| StateGcRoot {
            owner: StateGcRootOwner::OciIndex {
                reference: root.reference,
            },
            manifest: root.descriptor,
        })
        .collect::<Vec<_>>();
    for run in &runs.runs {
        roots.extend(run.image_roots.iter().map(|root| StateGcRoot {
            owner: StateGcRootOwner::Run {
                run_id: root.run_id,
                participant: match &root.participant {
                    RunImageParticipant::Primary => StateGcParticipant::Primary {},
                    RunImageParticipant::ManagedService { name } => {
                        StateGcParticipant::ManagedService { name: name.clone() }
                    }
                },
                slot: match root.slot {
                    RunImageSlot::Initial => StateGcImageSlot::Initial,
                    RunImageSlot::Final => StateGcImageSlot::Final,
                },
            },
            manifest: root.descriptor.clone(),
        }));
    }
    roots = sort_by_canonical_json(roots)?;
    let graph = verify_image_graphs(&layout, roots.iter().map(|root| root.manifest.clone()))?;
    let blobs = layout.stored_blobs()?;
    let stored = blobs
        .iter()
        .map(|blob| (blob.digest.clone(), blob))
        .collect::<BTreeMap<_, _>>();
    for descriptor in graph.values() {
        let blob = stored
            .get(&descriptor.digest)
            .with_context(|| format!("reachable OCI blob is unavailable: {}", descriptor.digest))?;
        if blob.size != descriptor.size {
            bail!(
                "reachable OCI blob size conflicts with descriptor: {}",
                descriptor.digest
            );
        }
    }
    Ok(StateSnapshot {
        catalog_entries,
        runs,
        roots,
        graph,
        blobs,
        staging_entries: layout.staging_entries()?,
        recovery_entries: recovery_entries(state)?,
        layout,
    })
}

fn require_quiescent_runs(snapshot: &StateSnapshot) -> Result<()> {
    if snapshot.runs.accepted_count > 0 {
        bail!(
            "state GC requires every accepted Run to become terminal or be reconciled: {} accepted",
            snapshot.runs.accepted_count
        );
    }
    if snapshot.recovery_entries > 0 {
        bail!(
            "state GC requires recovery attempts to be reconciled: {} entries",
            snapshot.recovery_entries
        );
    }
    Ok(())
}

fn recovery_entries(state: &Path) -> Result<u64> {
    let root = state.join("recovery/native");
    match fs::symlink_metadata(&root) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                bail!(
                    "native recovery root is not a real directory: {}",
                    root.display()
                );
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error).context("failed to inspect native recovery root"),
    }
    let mut count = 0_u64;
    for entry in fs::read_dir(&root).context("failed to list native recovery root")? {
        entry.context("failed to read native recovery entry")?;
        count = count
            .checked_add(1)
            .context("recovery entry count overflow")?;
        if count > 1_000_000 {
            bail!("native recovery root exceeds 1000000 entries");
        }
    }
    Ok(count)
}

fn sort_by_canonical_json<T: Serialize>(values: Vec<T>) -> Result<Vec<T>> {
    let mut keyed = values
        .into_iter()
        .map(|value| Ok((canonical_json(&value)?, value)))
        .collect::<Result<Vec<_>>>()?;
    keyed.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(keyed.into_iter().map(|(_, value)| value).collect())
}

fn gc_blob_bytes<'a>(blobs: impl IntoIterator<Item = &'a StateGcBlob>) -> Result<u64> {
    blobs.into_iter().try_fold(0_u64, |total, blob| {
        total
            .checked_add(blob.size)
            .context("GC OCI byte count overflow")
    })
}

fn retention_snapshot_if_present(state: &Path) -> Result<RunRetentionSnapshot> {
    let path = state.join("runs.sqlite3");
    match fs::symlink_metadata(&path) {
        Ok(_) => RunDatabase::open_existing(path)?.retention_snapshot(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(RunRetentionSnapshot {
            runs: Vec::new(),
            accepted_count: 0,
            verified_stored_bytes_count: 0,
            verified_stored_bytes_size: 0,
        }),
        Err(error) => Err(error).context("failed to inspect Run database"),
    }
}

fn verify_image_graphs(
    layout: &OciLayout,
    roots: impl IntoIterator<Item = OciDescriptor>,
) -> Result<BTreeMap<Digest, OciDescriptor>> {
    let images = ImageService::new(layout.clone());
    let mut graph = BTreeMap::new();
    let mut manifests = BTreeMap::new();
    for root in roots {
        insert_descriptor(&mut manifests, root)?;
    }
    for root in manifests.values() {
        let image = images.inspect(&root.digest)?;
        if image.manifest != *root {
            bail!(
                "Image root descriptor does not match stored Manifest: {}",
                root.digest
            );
        }
        insert_descriptor(&mut graph, image.manifest)?;
        insert_descriptor(&mut graph, image.config)?;
        for layer in image.layers {
            insert_descriptor(&mut graph, layer)?;
        }
    }
    Ok(graph)
}

fn insert_descriptor(
    descriptors: &mut BTreeMap<Digest, OciDescriptor>,
    descriptor: OciDescriptor,
) -> Result<()> {
    if let Some(existing) = descriptors.get(&descriptor.digest) {
        if existing != &descriptor {
            bail!(
                "conflicting OCI descriptors share digest {}",
                descriptor.digest
            );
        }
        return Ok(());
    }
    descriptors.insert(descriptor.digest.clone(), descriptor);
    Ok(())
}

fn descriptor_bytes<'a>(descriptors: impl IntoIterator<Item = &'a OciDescriptor>) -> Result<u64> {
    descriptors
        .into_iter()
        .try_fold(0_u64, |total, descriptor| {
            total
                .checked_add(descriptor.size)
                .context("OCI byte count overflow")
        })
}

fn lifecycle_name(lifecycle: StoredRunLifecycle) -> &'static str {
    match lifecycle {
        StoredRunLifecycle::Accepted => "accepted",
        StoredRunLifecycle::Terminal => "terminal",
    }
}

fn count(value: usize, name: &str) -> Result<u64> {
    u64::try_from(value).with_context(|| format!("{name} overflow"))
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::StateGcPlan;

    fn plan_with_root(owner: &Value) -> Value {
        json!({
            "schema_version": 1,
            "created_at": "2026-08-22T00:00:00Z",
            "roots_digest": format!("sha256:{}", "1".repeat(64)),
            "roots": [{
                "owner": owner,
                "manifest": {
                    "digest": format!("sha256:{}", "2".repeat(64)),
                    "size": 1,
                    "media_type": "application/vnd.oci.image.manifest.v1+json"
                }
            }],
            "reachable_oci_blobs": 1,
            "reachable_oci_bytes": 1,
            "delete": [],
            "plan_digest": format!("sha256:{}", "3".repeat(64))
        })
    }

    #[test]
    fn gc_plan_rejects_unknown_nested_owner_fields() {
        let value = plan_with_root(&json!({
            "kind": "oci_index",
            "reference": null,
            "ignored": true
        }));
        let error = serde_json::from_value::<StateGcPlan>(value)
            .expect_err("unknown owner field must fail");
        assert!(error.to_string().contains("unknown field `ignored`"));
    }

    #[test]
    fn gc_plan_rejects_unknown_descriptor_fields() {
        let mut value = plan_with_root(&json!({"kind": "oci_index", "reference": null}));
        value["roots"][0]["manifest"]["ignored"] = json!(true);
        let error = serde_json::from_value::<StateGcPlan>(value)
            .expect_err("unknown descriptor field must fail");
        assert!(error.to_string().contains("unknown field `ignored`"));
    }

    #[test]
    fn gc_plan_rejects_unknown_nested_participant_fields() {
        let value = plan_with_root(&json!({
            "kind": "run",
            "run_id": "run-018f0c90-7b8a-7000-8000-000000000001",
            "participant": {"kind": "primary", "ignored": true},
            "slot": "initial"
        }));
        let error = serde_json::from_value::<StateGcPlan>(value)
            .expect_err("unknown participant field must fail");
        assert!(error.to_string().contains("unknown field `ignored`"));
    }
}
