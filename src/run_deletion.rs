use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use uuid::{Uuid, Version};

use crate::run_record::decode_completion;
use crate::storage::{
    Database, PlannedRunDeletion, RunDeletionCommit, RunDeletionConflict, RunTombstone,
    StoredRunDeletionFacts,
};

const PLAN_SCHEMA_VERSION: u32 = 2;
const PLAN_KIND: &str = "run_delete_plan";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OperationId(Uuid);

impl FromStr for OperationId {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        let uuid = Uuid::parse_str(value).context("operation ID must be a UUID v4")?;
        if uuid.get_version() != Some(Version::Random) || value != uuid.hyphenated().to_string() {
            bail!("operation ID must use the canonical lowercase UUID v4 form");
        }
        Ok(Self(uuid))
    }
}

impl fmt::Display for OperationId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.hyphenated().fmt(formatter)
    }
}

impl Serialize for OperationId {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for OperationId {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RunDeletePlan {
    schema_version: u32,
    kind: String,
    operation_id: OperationId,
    requested_runs: usize,
    eligible: bool,
    candidate_asset_bytes: u64,
    candidates: Vec<RunDeleteCandidate>,
    already_deleted: Vec<DeletedRun>,
    blocked: Vec<BlockedRun>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RunDeleteCandidate {
    run_id: String,
    accepted_at: String,
    terminal_at: String,
    asset_fingerprint: String,
    run_record_bytes: u64,
    observation_count: u64,
    observation_bytes: u64,
    asset_bytes: u64,
    catalog_final_images: Vec<CatalogFinalImage>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CatalogFinalImage {
    program: String,
    manifest_digest: String,
    catalog_names: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DeletedRun {
    run_id: String,
    deleted_at: String,
    operation_id: String,
}

impl From<RunTombstone> for DeletedRun {
    fn from(value: RunTombstone) -> Self {
        Self {
            run_id: value.run_id,
            deleted_at: value.deleted_at,
            operation_id: value.operation_id,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct BlockedRun {
    run_id: String,
    reason: String,
    recovery: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct RunDeleteResult {
    schema_version: u32,
    mode: &'static str,
    operation_id: OperationId,
    deleted_at: Option<String>,
    deleted_runs: Vec<String>,
    already_deleted: Vec<DeletedRun>,
    recovery: &'static str,
}

pub(crate) fn check(
    database: &Database,
    operation_id: OperationId,
    run_ids: &BTreeSet<String>,
) -> Result<RunDeletePlan> {
    let facts = database.run_deletion_facts(run_ids)?;
    let catalog_names = catalog_names_by_digest(facts.catalog_descriptors)?;
    let mut candidates = Vec::new();
    let mut already_deleted = Vec::new();
    let mut blocked = Vec::new();
    let mut candidate_asset_bytes = 0_u64;
    for run_id in run_ids {
        if let Some(run) = facts.runs.get(run_id) {
            if run.terminal_at.is_none() || run.completion_json.is_none() {
                blocked.push(BlockedRun {
                    run_id: run_id.clone(),
                    reason: "not_terminal".to_owned(),
                    recovery: format!("runlab run reconcile {run_id}"),
                });
                continue;
            }
            let asset_bytes = run.asset_bytes();
            candidate_asset_bytes = candidate_asset_bytes.saturating_add(asset_bytes);
            candidates.push(candidate(run, &catalog_names)?);
        } else if let Some(tombstone) = facts.tombstones.get(run_id) {
            already_deleted.push(tombstone.clone().into());
        } else {
            blocked.push(BlockedRun {
                run_id: run_id.clone(),
                reason: "not_found".to_owned(),
                recovery: "verify the Run identity and regenerate the bounded selection".to_owned(),
            });
        }
    }
    Ok(RunDeletePlan {
        schema_version: PLAN_SCHEMA_VERSION,
        kind: PLAN_KIND.to_owned(),
        operation_id,
        requested_runs: run_ids.len(),
        eligible: blocked.is_empty(),
        candidate_asset_bytes,
        candidates,
        already_deleted,
        blocked,
    })
}

fn candidate(
    run: &StoredRunDeletionFacts,
    catalog_names: &BTreeMap<String, Vec<String>>,
) -> Result<RunDeleteCandidate> {
    let completion = decode_completion(
        run.completion_json
            .as_deref()
            .context("terminal Run has no completion")?,
    )?;
    let mut catalog_final_images = completion
        .available_final_environments()
        .into_iter()
        .filter_map(|(program, descriptor)| {
            let digest = descriptor.digest().to_string();
            catalog_names.get(&digest).map(|names| CatalogFinalImage {
                program: program.to_owned(),
                manifest_digest: digest,
                catalog_names: names.clone(),
            })
        })
        .collect::<Vec<_>>();
    catalog_final_images.sort_by(|left, right| {
        left.program
            .cmp(&right.program)
            .then(left.manifest_digest.cmp(&right.manifest_digest))
    });
    Ok(RunDeleteCandidate {
        run_id: run.run_id.clone(),
        accepted_at: run.accepted_at.clone(),
        terminal_at: run
            .terminal_at
            .clone()
            .context("terminal Run has no terminal_at")?,
        asset_fingerprint: run.asset_fingerprint(),
        run_record_bytes: run.run_record_bytes(),
        observation_count: run.observation_count,
        observation_bytes: run.observation_bytes,
        asset_bytes: run.asset_bytes(),
        catalog_final_images,
    })
}

fn catalog_names_by_digest(
    descriptors: Vec<(String, serde_json::Value)>,
) -> Result<BTreeMap<String, Vec<String>>> {
    let mut names = BTreeMap::<String, Vec<String>>::new();
    for (name, descriptor) in descriptors {
        let digest = descriptor
            .get("digest")
            .and_then(serde_json::Value::as_str)
            .with_context(|| format!("stored Catalog descriptor for {name:?} has no digest"))?;
        names.entry(digest.to_owned()).or_default().push(name);
    }
    Ok(names)
}

pub(crate) fn parse_plan(bytes: &[u8]) -> Result<RunDeletePlan> {
    let plan: RunDeletePlan =
        serde_json::from_slice(bytes).context("Run deletion plan is not valid JSON")?;
    plan.validate()?;
    Ok(plan)
}

impl RunDeletePlan {
    pub(crate) fn operation_id(&self) -> &OperationId {
        &self.operation_id
    }

    fn validate(&self) -> Result<()> {
        if self.schema_version != PLAN_SCHEMA_VERSION || self.kind != PLAN_KIND {
            bail!("Run deletion plan kind or schema version is unsupported");
        }
        if !self.eligible || !self.blocked.is_empty() {
            bail!("Run deletion plan is blocked; resolve every blocked Run and run check again");
        }
        if self.requested_runs
            != self
                .candidates
                .len()
                .saturating_add(self.already_deleted.len())
        {
            bail!("Run deletion plan counts are inconsistent");
        }
        let mut previous = None;
        let mut ids = BTreeSet::new();
        let mut bytes = 0_u64;
        for candidate in &self.candidates {
            crate::run::RunId::from_str(&candidate.run_id)
                .context("Run deletion plan contains an invalid candidate identity")?;
            if previous.is_some_and(|previous| previous >= candidate.run_id.as_str()) {
                bail!("Run deletion plan candidates must be sorted and unique");
            }
            previous = Some(candidate.run_id.as_str());
            ids.insert(candidate.run_id.as_str());
            if candidate.asset_bytes
                != candidate
                    .run_record_bytes
                    .saturating_add(candidate.observation_bytes)
            {
                bail!("Run deletion plan candidate byte counts are inconsistent");
            }
            bytes = bytes.saturating_add(candidate.asset_bytes);
        }
        for deleted in &self.already_deleted {
            crate::run::RunId::from_str(&deleted.run_id)
                .context("Run deletion plan contains an invalid deleted identity")?;
            if !ids.insert(deleted.run_id.as_str()) {
                bail!("Run deletion plan repeats a Run identity");
            }
        }
        if bytes != self.candidate_asset_bytes {
            bail!("Run deletion plan asset byte total is inconsistent");
        }
        Ok(())
    }
}

pub(crate) fn apply(database: &Database, plan: RunDeletePlan) -> Result<RunDeleteResult> {
    plan.validate()?;
    let operation_id = plan.operation_id.clone();
    let candidates = plan
        .candidates
        .iter()
        .map(|candidate| PlannedRunDeletion {
            run_id: &candidate.run_id,
            asset_fingerprint: &candidate.asset_fingerprint,
        })
        .collect::<Vec<_>>();
    let operation_id_text = operation_id.to_string();
    let mut operation_run_ids = plan
        .candidates
        .iter()
        .map(|candidate| candidate.run_id.as_str())
        .chain(
            plan.already_deleted
                .iter()
                .filter(|deleted| deleted.operation_id == operation_id_text)
                .map(|deleted| deleted.run_id.as_str()),
        )
        .collect::<Vec<_>>();
    operation_run_ids.sort_unstable();
    let commit = database
        .run_delete_apply(
            &operation_id_text,
            &Utc::now().to_rfc3339(),
            &candidates,
            &operation_run_ids,
        )
        .map_err(|error| classify_apply_error(error, &operation_id))?;
    let (mode, deleted_at, deleted_runs) = match commit {
        RunDeletionCommit::Applied {
            deleted_at,
            run_ids,
        } => ("applied", Some(deleted_at), run_ids),
        RunDeletionCommit::AlreadyApplied {
            deleted_at,
            run_ids,
        } => ("already_applied", Some(deleted_at), run_ids),
        RunDeletionCommit::NoChange => ("no_change", None, Vec::new()),
    };
    Ok(RunDeleteResult {
        schema_version: 1,
        mode,
        operation_id,
        deleted_at,
        deleted_runs,
        already_deleted: plan.already_deleted,
        recovery: "runlab storage prune check",
    })
}

fn classify_apply_error(error: anyhow::Error, operation_id: &OperationId) -> anyhow::Error {
    let busy = is_database_busy(&error);
    let conflict = error
        .chain()
        .any(|cause| cause.downcast_ref::<RunDeletionConflict>().is_some());
    if !busy && !conflict {
        return error;
    }
    crate::error::classify(
        error,
        crate::error::ErrorFacts {
            category: crate::error::ErrorCategory::Conflict,
            stage: "run_delete_apply",
            run_id: None,
            accepted: Some(false),
            run_created: Some(false),
            retryable: busy,
            recovery: Some(if busy {
                format!(
                    "retry the same plan with deletion operation ID {operation_id} after the concurrent database writer finishes"
                )
            } else {
                format!("rerun `runlab run delete check` with the same operation ID {operation_id}")
            }),
        },
    )
}

pub(crate) fn classify_open_error(
    error: anyhow::Error,
    operation_id: &OperationId,
) -> anyhow::Error {
    if !is_database_busy(&error) {
        return error;
    }
    crate::error::classify(
        error,
        crate::error::ErrorFacts {
            category: crate::error::ErrorCategory::Conflict,
            stage: "run_delete_apply",
            run_id: None,
            accepted: Some(false),
            run_created: Some(false),
            retryable: true,
            recovery: Some(format!(
                "retry the same plan with deletion operation ID {operation_id} after the concurrent database writer finishes"
            )),
        },
    )
}

fn is_database_busy(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        let Some(rusqlite::Error::SqliteFailure(failure, _)) =
            cause.downcast_ref::<rusqlite::Error>()
        else {
            return false;
        };
        matches!(
            failure.code,
            rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
        )
    })
}
