use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

use crate::metadata::Metadata;
use crate::run_record::{
    CompletionRecord, InputIdentityRecord, InputRecord, decode_completion, decode_identity,
    decode_input, migrate_completion, migrate_identity, migrate_input,
};

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const DATABASE_SCHEMA_VERSION: i64 = 4;

#[derive(Clone, Debug)]
pub(crate) struct ExecutionOwner {
    pub(crate) boot_id: String,
    pub(crate) pid: i64,
    pub(crate) start_ticks: i64,
}

#[derive(Debug)]
pub(crate) struct ExecutionJournal {
    pub(crate) owner: ExecutionOwner,
    pub(crate) phase: ExecutionPhase,
    pub(crate) completion: Option<CompletionRecord>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExecutionPhase {
    Accepted,
    EngineRunning,
    ResultStaged,
    Terminal,
}

impl ExecutionPhase {
    fn decode(value: &str) -> Result<Self> {
        match value {
            "accepted" => Ok(Self::Accepted),
            "engine_running" => Ok(Self::EngineRunning),
            "result_staged" => Ok(Self::ResultStaged),
            "terminal" => Ok(Self::Terminal),
            _ => bail!("stored Run execution phase is invalid: {value}"),
        }
    }
}

pub(crate) struct NewRun<'a> {
    pub(crate) run_id: &'a str,
    pub(crate) accepted_at: &'a str,
    pub(crate) initial_image_name: Option<&'a str>,
    pub(crate) metadata: &'a Metadata,
    pub(crate) input: &'a InputRecord,
    pub(crate) input_identity: &'a InputIdentityRecord,
    pub(crate) owner: &'a ExecutionOwner,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RunTombstone {
    pub(crate) run_id: String,
    pub(crate) deleted_at: String,
    pub(crate) operation_id: String,
}

#[derive(Debug)]
pub(crate) enum RunInsertion {
    Created,
    Existing,
    Deleted(RunTombstone),
}

#[derive(Clone, Debug)]
pub(crate) struct StoredRunDeletionFacts {
    pub(crate) run_id: String,
    pub(crate) accepted_at: String,
    pub(crate) initial_image_name: Option<String>,
    pub(crate) metadata_json: String,
    pub(crate) input_json: String,
    pub(crate) input_identity_json: String,
    pub(crate) cancellation_requested_at: Option<String>,
    pub(crate) terminal_at: Option<String>,
    pub(crate) completion_json: Option<String>,
}

impl StoredRunDeletionFacts {
    pub(crate) fn record_fingerprint(&self) -> String {
        let mut hasher = Sha256::new();
        for value in [
            Some(self.run_id.as_str()),
            Some(self.accepted_at.as_str()),
            self.initial_image_name.as_deref(),
            Some(self.metadata_json.as_str()),
            Some(self.input_json.as_str()),
            Some(self.input_identity_json.as_str()),
            self.cancellation_requested_at.as_deref(),
            self.terminal_at.as_deref(),
            self.completion_json.as_deref(),
        ] {
            match value {
                Some(value) => {
                    hasher.update([1]);
                    hasher.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
                    hasher.update(value.as_bytes());
                }
                None => hasher.update([0]),
            }
        }
        let mut encoded = String::from("sha256:");
        for byte in hasher.finalize() {
            use std::fmt::Write as _;
            write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
        }
        encoded
    }

    pub(crate) fn record_bytes(&self) -> u64 {
        [
            Some(self.run_id.as_str()),
            Some(self.accepted_at.as_str()),
            self.initial_image_name.as_deref(),
            Some(self.metadata_json.as_str()),
            Some(self.input_json.as_str()),
            Some(self.input_identity_json.as_str()),
            self.cancellation_requested_at.as_deref(),
            self.terminal_at.as_deref(),
            self.completion_json.as_deref(),
        ]
        .into_iter()
        .flatten()
        .fold(0_u64, |total, value| {
            total.saturating_add(u64::try_from(value.len()).unwrap_or(u64::MAX))
        })
    }
}

pub(crate) struct RunDeletionFacts {
    pub(crate) runs: BTreeMap<String, StoredRunDeletionFacts>,
    pub(crate) tombstones: BTreeMap<String, RunTombstone>,
    pub(crate) catalog_descriptors: Vec<(String, Value)>,
}

pub(crate) struct PlannedRunDeletion<'a> {
    pub(crate) run_id: &'a str,
    pub(crate) record_fingerprint: &'a str,
}

#[derive(Debug)]
pub(crate) enum RunDeletionCommit {
    Applied {
        deleted_at: String,
        run_ids: Vec<String>,
    },
    AlreadyApplied {
        deleted_at: String,
        run_ids: Vec<String>,
    },
    NoChange,
}

#[derive(Debug)]
pub(crate) struct RunDeletionConflict {
    message: String,
}

impl fmt::Display for RunDeletionConflict {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for RunDeletionConflict {}

fn deletion_conflict(message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(RunDeletionConflict {
        message: message.into(),
    })
}

pub(crate) struct Database {
    connection: Mutex<Connection>,
}

#[derive(Debug)]
pub(crate) struct StoredRun {
    pub(crate) run_id: String,
    pub(crate) accepted_at: String,
    pub(crate) initial_image_name: Option<String>,
    pub(crate) metadata: Metadata,
    pub(crate) input: InputRecord,
    pub(crate) input_identity: InputIdentityRecord,
    pub(crate) cancellation_requested_at: Option<String>,
    pub(crate) terminal_at: Option<String>,
    pub(crate) completion: Option<CompletionRecord>,
}

#[derive(Debug)]
pub(crate) enum RunCancellation {
    Requested {
        requested_at: String,
    },
    Terminal {
        requested_at: Option<String>,
        terminal_at: String,
    },
}

#[cfg(target_os = "linux")]
pub(crate) struct StorageDatabaseFacts {
    pub(crate) catalog_images: u64,
    pub(crate) runs: u64,
    /// Runs for which no terminal completion has been published yet.
    pub(crate) active_runs: u64,
    pub(crate) catalog_descriptor_documents: Vec<Value>,
    pub(crate) run_descriptor_documents: Vec<RunStorageDocuments>,
}

#[cfg(target_os = "linux")]
pub(crate) struct RunStorageDocuments {
    pub(crate) run_id: String,
    pub(crate) terminal: bool,
    pub(crate) descriptor_documents: Vec<Value>,
}

struct RunRow {
    run_id: String,
    accepted_at: String,
    initial_image_name: Option<String>,
    metadata_json: String,
    input_json: String,
    input_identity_json: String,
    cancellation_requested_at: Option<String>,
    terminal_at: Option<String>,
    completion_json: Option<String>,
}

impl Database {
    #[cfg(target_os = "linux")]
    pub(crate) fn storage_facts(&self) -> Result<StorageDatabaseFacts> {
        let connection = self.lock()?;
        let catalog_images: i64 =
            connection.query_row("SELECT count(*) FROM main.catalog", [], |row| row.get(0))?;
        let (runs, active_runs): (i64, i64) = connection.query_row(
            "SELECT count(*), count(*) FILTER (WHERE terminal_at IS NULL) FROM main.runs",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let mut catalog_descriptor_documents = Vec::new();
        {
            let mut statement = connection.prepare("SELECT descriptor_json FROM main.catalog")?;
            let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
            for row in rows {
                catalog_descriptor_documents.push(
                    serde_json::from_str(&row?).context("stored Image descriptor is invalid")?,
                );
            }
        }
        let mut run_descriptor_documents = Vec::new();
        {
            let mut statement = connection.prepare(
                "SELECT run_id, terminal_at IS NOT NULL, input_json, completion_json
                 FROM main.runs",
            )?;
            let rows = statement.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, bool>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })?;
            for row in rows {
                let (run_id, terminal, input, completion) = row?;
                let mut descriptor_documents = vec![serde_json::to_value(decode_input(&input)?)?];
                if let Some(completion) = completion {
                    descriptor_documents
                        .push(serde_json::to_value(decode_completion(&completion)?)?);
                }
                run_descriptor_documents.push(RunStorageDocuments {
                    run_id,
                    terminal,
                    descriptor_documents,
                });
            }
        }
        Ok(StorageDatabaseFacts {
            catalog_images: u64::try_from(catalog_images).context("Catalog count is negative")?,
            runs: u64::try_from(runs).context("Run count is negative")?,
            active_runs: u64::try_from(active_runs).context("active Run count is negative")?,
            catalog_descriptor_documents,
            run_descriptor_documents,
        })
    }

    pub(crate) fn open(path: &Path) -> Result<Self> {
        let mut connection = Connection::open(path)
            .with_context(|| format!("failed to open RunLab database {}", path.display()))?;
        connection
            .busy_timeout(BUSY_TIMEOUT)
            .context("failed to configure RunLab database busy timeout")?;
        connection.execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON;")?;
        migrate_database(&mut connection)?;
        crate::public_schema::install(&connection)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    pub(crate) fn catalog_set(
        &self,
        name: &str,
        descriptor: &Value,
        metadata: &Metadata,
        updated_at: &str,
    ) -> Result<()> {
        let descriptor = serde_json::to_string(descriptor)?;
        self.lock()?.execute(
            "INSERT INTO catalog(name, descriptor_json, metadata_json, updated_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(name) DO UPDATE SET
                 descriptor_json = excluded.descriptor_json,
                 metadata_json = excluded.metadata_json,
                 updated_at = excluded.updated_at",
            params![
                name,
                descriptor,
                serde_json::to_string(metadata)?,
                updated_at
            ],
        )?;
        Ok(())
    }

    pub(crate) fn catalog_get(&self, name: &str) -> Result<Option<(Value, Metadata)>> {
        let encoded = self
            .lock()?
            .query_row(
                "SELECT descriptor_json, metadata_json FROM catalog WHERE name = ?1",
                [name],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        encoded
            .map(|(descriptor, metadata)| {
                Ok((
                    serde_json::from_str(&descriptor)
                        .context("stored Image descriptor is invalid")?,
                    serde_json::from_str(&metadata).context("stored Image metadata is invalid")?,
                ))
            })
            .transpose()
    }

    pub(crate) fn catalog_list(
        &self,
        limit: usize,
        after: Option<&str>,
    ) -> Result<Vec<(String, Value, Metadata)>> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT name, descriptor_json, metadata_json FROM catalog
             WHERE (?1 IS NULL OR name > ?1)
             ORDER BY name ASC LIMIT ?2",
        )?;
        let limit = i64::try_from(limit).context("Image page size overflow")?;
        let rows = statement.query_map(params![after, limit], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        rows.map(|row| {
            let (name, descriptor, metadata) = row?;
            Ok((
                name,
                serde_json::from_str(&descriptor)?,
                serde_json::from_str(&metadata)?,
            ))
        })
        .collect()
    }

    pub(crate) fn run_insert(&self, run: &NewRun<'_>) -> Result<RunInsertion> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        let changed = transaction.execute(
            "INSERT OR IGNORE INTO main.runs(
                run_id, accepted_at, initial_image_name, metadata_json, input_json,
                input_identity_json
             ) SELECT ?1, ?2, ?3, ?4, ?5, ?6
             WHERE NOT EXISTS (
                 SELECT 1 FROM main.run_tombstones WHERE run_id = ?1
             )",
            params![
                run.run_id,
                run.accepted_at,
                run.initial_image_name,
                serde_json::to_string(run.metadata)?,
                serde_json::to_string(run.input)?,
                serde_json::to_string(run.input_identity)?,
            ],
        )?;
        if changed == 1 {
            transaction.execute(
                "INSERT INTO main.run_executions(
                    run_id, owner_boot_id, owner_pid, owner_start_ticks, phase
                 ) VALUES (?1, ?2, ?3, ?4, 'accepted')",
                params![
                    run.run_id,
                    run.owner.boot_id,
                    run.owner.pid,
                    run.owner.start_ticks
                ],
            )?;
        }
        let tombstone = if changed == 0 {
            transaction
                .query_row(
                    "SELECT run_id, deleted_at, operation_id
                     FROM main.run_tombstones WHERE run_id = ?1",
                    [run.run_id],
                    read_tombstone,
                )
                .optional()?
        } else {
            None
        };
        transaction.commit()?;
        Ok(if changed == 1 {
            RunInsertion::Created
        } else if let Some(tombstone) = tombstone {
            RunInsertion::Deleted(tombstone)
        } else {
            RunInsertion::Existing
        })
    }

    pub(crate) fn run_tombstone(&self, run_id: &str) -> Result<Option<RunTombstone>> {
        self.lock()?
            .query_row(
                "SELECT run_id, deleted_at, operation_id
                 FROM main.run_tombstones WHERE run_id = ?1",
                [run_id],
                read_tombstone,
            )
            .optional()
            .map_err(Into::into)
    }

    pub(crate) fn run_deletion_facts(
        &self,
        run_ids: &BTreeSet<String>,
    ) -> Result<RunDeletionFacts> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        let mut runs = BTreeMap::new();
        let mut tombstones = BTreeMap::new();
        for run_id in run_ids {
            if let Some(run) = transaction
                .query_row(
                    "SELECT run_id, accepted_at, initial_image_name, metadata_json, input_json,
                            input_identity_json, cancellation_requested_at,
                            terminal_at, completion_json
                     FROM main.runs WHERE run_id = ?1",
                    [run_id],
                    read_run_deletion_facts,
                )
                .optional()?
            {
                runs.insert(run_id.clone(), run);
                continue;
            }
            if let Some(tombstone) = transaction
                .query_row(
                    "SELECT run_id, deleted_at, operation_id
                     FROM main.run_tombstones WHERE run_id = ?1",
                    [run_id],
                    read_tombstone,
                )
                .optional()?
            {
                tombstones.insert(run_id.clone(), tombstone);
            }
        }
        let catalog_descriptors = {
            let mut statement = transaction
                .prepare("SELECT name, descriptor_json FROM main.catalog ORDER BY name ASC")?;
            statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .map(|row| {
                    let (name, descriptor) = row?;
                    Ok((
                        name,
                        serde_json::from_str(&descriptor)
                            .context("stored Image descriptor is invalid")?,
                    ))
                })
                .collect::<Result<Vec<_>>>()?
        };
        transaction.commit()?;
        Ok(RunDeletionFacts {
            runs,
            tombstones,
            catalog_descriptors,
        })
    }

    pub(crate) fn run_delete_apply(
        &self,
        operation_id: &str,
        deleted_at: &str,
        candidates: &[PlannedRunDeletion<'_>],
        operation_run_ids: &[&str],
    ) -> Result<RunDeletionCommit> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let operation_tombstones = tombstones_for_operation(&transaction, operation_id)?;
        let planned_ids = candidates
            .iter()
            .map(|candidate| candidate.run_id.to_owned())
            .collect::<Vec<_>>();
        let intended_operation_ids = operation_run_ids
            .iter()
            .map(|run_id| (*run_id).to_owned())
            .collect::<Vec<_>>();
        if !operation_tombstones.is_empty() {
            let existing_ids = operation_tombstones
                .iter()
                .map(|tombstone| tombstone.run_id.clone())
                .collect::<Vec<_>>();
            if existing_ids != intended_operation_ids {
                return Err(deletion_conflict(format!(
                    "deletion operation identity is already bound to a different Run set: {operation_id}"
                )));
            }
            let committed_at = operation_tombstones[0].deleted_at.clone();
            if operation_tombstones
                .iter()
                .any(|tombstone| tombstone.deleted_at != committed_at)
            {
                bail!("stored Run deletion operation has inconsistent commit times");
            }
            transaction.commit()?;
            return Ok(RunDeletionCommit::AlreadyApplied {
                deleted_at: committed_at,
                run_ids: existing_ids,
            });
        }
        if intended_operation_ids != planned_ids {
            return Err(deletion_conflict(
                "Run deletion plan refers to missing tombstones for this operation",
            ));
        }
        if candidates.is_empty() {
            transaction.commit()?;
            return Ok(RunDeletionCommit::NoChange);
        }
        for candidate in candidates {
            let stored = transaction
                .query_row(
                    "SELECT run_id, accepted_at, initial_image_name, metadata_json, input_json,
                            input_identity_json, cancellation_requested_at,
                            terminal_at, completion_json
                     FROM main.runs WHERE run_id = ?1",
                    [candidate.run_id],
                    read_run_deletion_facts,
                )
                .optional()?;
            let Some(stored) = stored else {
                return Err(deletion_conflict(format!(
                    "Run deletion plan is stale because a candidate no longer exists: {}",
                    candidate.run_id
                )));
            };
            if stored.terminal_at.is_none() || stored.completion_json.is_none() {
                return Err(deletion_conflict(format!(
                    "Run deletion plan is stale because a candidate is not terminal: {}",
                    candidate.run_id
                )));
            }
            if stored.record_fingerprint() != candidate.record_fingerprint {
                return Err(deletion_conflict(format!(
                    "Run deletion plan is stale because a candidate changed: {}",
                    candidate.run_id
                )));
            }
        }
        for candidate in candidates {
            transaction.execute(
                "DELETE FROM main.run_executions WHERE run_id = ?1",
                [candidate.run_id],
            )?;
            let changed = transaction.execute(
                "DELETE FROM main.runs WHERE run_id = ?1",
                [candidate.run_id],
            )?;
            if changed != 1 {
                bail!("Run disappeared during deletion: {}", candidate.run_id);
            }
            transaction.execute(
                "INSERT INTO main.run_tombstones(run_id, deleted_at, operation_id)
                 VALUES (?1, ?2, ?3)",
                params![candidate.run_id, deleted_at, operation_id],
            )?;
        }
        transaction.commit()?;
        Ok(RunDeletionCommit::Applied {
            deleted_at: deleted_at.to_owned(),
            run_ids: planned_ids,
        })
    }

    pub(crate) fn run_mark_engine_running(&self, run_id: &str) -> Result<()> {
        let changed = self.lock()?.execute(
            "UPDATE main.run_executions SET phase = 'engine_running'
             WHERE run_id = ?1 AND phase = 'accepted'",
            [run_id],
        )?;
        if changed != 1 {
            bail!("Run Engine cannot start from its current state: {run_id}");
        }
        Ok(())
    }

    pub(crate) fn run_stage_pre_engine_interruption(
        &self,
        run_id: &str,
        completion: &CompletionRecord,
    ) -> Result<()> {
        if !matches!(completion, CompletionRecord::Interrupted { .. }) {
            bail!("pre-Engine interruption requires an interrupted completion");
        }
        let changed = self.lock()?.execute(
            "UPDATE main.run_executions
             SET phase = 'result_staged', completion_json = ?2
             WHERE run_id = ?1 AND phase = 'accepted'",
            params![run_id, serde_json::to_string(completion)?],
        )?;
        if changed != 1 {
            bail!("pre-Engine interruption cannot be staged from its current state: {run_id}");
        }
        Ok(())
    }

    pub(crate) fn run_stage_completion(
        &self,
        run_id: &str,
        completion: &CompletionRecord,
    ) -> Result<()> {
        let changed = self.lock()?.execute(
            "UPDATE main.run_executions
             SET phase = 'result_staged', completion_json = ?2
             WHERE run_id = ?1 AND phase = 'engine_running'",
            params![run_id, serde_json::to_string(completion)?],
        )?;
        if changed != 1 {
            bail!("Run completion cannot be staged from its current state: {run_id}");
        }
        Ok(())
    }

    pub(crate) fn run_publish_staged(&self, run_id: &str, terminal_at: &str) -> Result<bool> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        let completion = transaction
            .query_row(
                "SELECT completion_json FROM main.run_executions
                 WHERE run_id = ?1 AND phase = 'result_staged'",
                [run_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let Some(completion) = completion else {
            return Ok(false);
        };
        let changed = transaction.execute(
            "UPDATE main.runs SET terminal_at = ?2, completion_json = ?3
             WHERE run_id = ?1 AND terminal_at IS NULL",
            params![run_id, terminal_at, completion],
        )?;
        if changed != 1 {
            bail!("Run cannot publish completion from its current state: {run_id}");
        }
        transaction.execute(
            "UPDATE main.run_executions SET phase = 'terminal' WHERE run_id = ?1",
            [run_id],
        )?;
        transaction.commit()?;
        Ok(true)
    }

    pub(crate) fn run_execution(&self, run_id: &str) -> Result<Option<ExecutionJournal>> {
        let encoded = self
            .lock()?
            .query_row(
                "SELECT owner_boot_id, owner_pid, owner_start_ticks, phase, completion_json
                 FROM main.run_executions WHERE run_id = ?1",
                [run_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<String>>(4)?,
                    ))
                },
            )
            .optional()?;
        encoded
            .map(|(boot_id, pid, start_ticks, phase, completion)| {
                Ok(ExecutionJournal {
                    owner: ExecutionOwner {
                        boot_id,
                        pid,
                        start_ticks,
                    },
                    phase: ExecutionPhase::decode(&phase)?,
                    completion: completion
                        .map(|value| decode_completion(&value))
                        .transpose()?,
                })
            })
            .transpose()
    }

    pub(crate) fn run_cancel(
        &self,
        run_id: &str,
        requested_at: &str,
    ) -> Result<Option<RunCancellation>> {
        let connection = self.lock()?;
        let requested_at = connection
            .query_row(
                "UPDATE main.runs
                 SET cancellation_requested_at = COALESCE(cancellation_requested_at, ?2)
                 WHERE run_id = ?1 AND terminal_at IS NULL
                 RETURNING cancellation_requested_at",
                params![run_id, requested_at],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if let Some(requested_at) = requested_at {
            return Ok(Some(RunCancellation::Requested { requested_at }));
        }
        connection
            .query_row(
                "SELECT cancellation_requested_at, terminal_at
                 FROM main.runs WHERE run_id = ?1",
                [run_id],
                |row| {
                    Ok(RunCancellation::Terminal {
                        requested_at: row.get(0)?,
                        terminal_at: row.get(1)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub(crate) fn run_cancellation_requested(&self, run_id: &str) -> Result<bool> {
        Ok(self.lock()?.query_row(
            "SELECT cancellation_requested_at IS NOT NULL
                 FROM main.runs WHERE run_id = ?1",
            [run_id],
            |row| row.get(0),
        )?)
    }

    pub(crate) fn run_get(&self, run_id: &str) -> Result<Option<StoredRun>> {
        let row = self
            .lock()?
            .query_row(
                "SELECT run_id, accepted_at, initial_image_name, metadata_json, input_json,
                        input_identity_json, cancellation_requested_at,
                        terminal_at, completion_json
                 FROM main.runs WHERE run_id = ?1",
                [run_id],
                read_run_row,
            )
            .optional()?;
        row.map(decode_run).transpose()
    }

    pub(crate) fn run_list(&self, limit: usize, after: Option<&str>) -> Result<Vec<StoredRun>> {
        let connection = self.lock()?;
        let after_position = after
            .map(|run_id| {
                connection
                    .query_row(
                        "SELECT accepted_at, run_id FROM main.runs WHERE run_id = ?1",
                        [run_id],
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                    )
                    .optional()
            })
            .transpose()?
            .flatten();
        if after_position.is_none()
            && let Some(run_id) = after
        {
            if let Some(tombstone) = connection
                .query_row(
                    "SELECT run_id, deleted_at, operation_id
                     FROM main.run_tombstones WHERE run_id = ?1",
                    [run_id],
                    read_tombstone,
                )
                .optional()?
            {
                bail!(
                    "--after Run was deleted at {} by operation {}; restart pagination without --after",
                    tombstone.deleted_at,
                    tombstone.operation_id
                );
            }
            bail!(
                "--after Run does not exist; it may have been deleted; restart pagination without --after"
            );
        }
        let (after_at, after_id) =
            after_position.map_or((None, None), |(at, id)| (Some(at), Some(id)));
        let mut statement = connection.prepare(
            "SELECT run_id, accepted_at, initial_image_name, metadata_json, input_json,
                    input_identity_json, cancellation_requested_at,
                    terminal_at, completion_json
             FROM main.runs
             WHERE (?1 IS NULL OR accepted_at < ?1 OR (accepted_at = ?1 AND run_id < ?2))
             ORDER BY accepted_at DESC, run_id DESC LIMIT ?3",
        )?;
        let limit = i64::try_from(limit).context("Run page size overflow")?;
        let rows = statement.query_map(params![after_at, after_id, limit], read_run_row)?;
        rows.map(|row| decode_run(row?)).collect()
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.connection
            .lock()
            .map_err(|_| anyhow::anyhow!("RunLab database lock is poisoned"))
    }

    pub(crate) fn with_connection<T>(
        &self,
        operation: impl FnOnce(&Connection) -> Result<T>,
    ) -> Result<T> {
        let connection = self.lock()?;
        operation(&connection)
    }
}

fn read_run_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RunRow> {
    Ok(RunRow {
        run_id: row.get("run_id")?,
        accepted_at: row.get("accepted_at")?,
        initial_image_name: row.get("initial_image_name")?,
        metadata_json: row.get("metadata_json")?,
        input_json: row.get("input_json")?,
        input_identity_json: row.get("input_identity_json")?,
        cancellation_requested_at: row.get("cancellation_requested_at")?,
        terminal_at: row.get("terminal_at")?,
        completion_json: row.get("completion_json")?,
    })
}

fn read_run_deletion_facts(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredRunDeletionFacts> {
    Ok(StoredRunDeletionFacts {
        run_id: row.get("run_id")?,
        accepted_at: row.get("accepted_at")?,
        initial_image_name: row.get("initial_image_name")?,
        metadata_json: row.get("metadata_json")?,
        input_json: row.get("input_json")?,
        input_identity_json: row.get("input_identity_json")?,
        cancellation_requested_at: row.get("cancellation_requested_at")?,
        terminal_at: row.get("terminal_at")?,
        completion_json: row.get("completion_json")?,
    })
}

fn read_tombstone(row: &rusqlite::Row<'_>) -> rusqlite::Result<RunTombstone> {
    Ok(RunTombstone {
        run_id: row.get("run_id")?,
        deleted_at: row.get("deleted_at")?,
        operation_id: row.get("operation_id")?,
    })
}

fn tombstones_for_operation(
    connection: &Connection,
    operation_id: &str,
) -> Result<Vec<RunTombstone>> {
    let mut statement = connection.prepare(
        "SELECT run_id, deleted_at, operation_id FROM main.run_tombstones
         WHERE operation_id = ?1 ORDER BY run_id ASC",
    )?;
    Ok(statement
        .query_map([operation_id], read_tombstone)?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

fn decode_run(row: RunRow) -> Result<StoredRun> {
    Ok(StoredRun {
        run_id: row.run_id,
        accepted_at: row.accepted_at,
        initial_image_name: row.initial_image_name,
        metadata: serde_json::from_str(&row.metadata_json)
            .context("stored Run metadata is invalid")?,
        input: decode_input(&row.input_json)?,
        input_identity: decode_identity(&row.input_identity_json)?,
        cancellation_requested_at: row.cancellation_requested_at,
        terminal_at: row.terminal_at,
        completion: row
            .completion_json
            .map(|value| decode_completion(&value))
            .transpose()?,
    })
}

fn migrate_database(connection: &mut Connection) -> Result<()> {
    let transaction = connection.transaction()?;
    let version: i64 = transaction.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version > DATABASE_SCHEMA_VERSION {
        bail!(
            "RunLab database schema version {version} is newer than supported version {DATABASE_SCHEMA_VERSION}"
        );
    }
    transaction.execute_batch(
        r#"CREATE TABLE IF NOT EXISTS catalog (
               name TEXT PRIMARY KEY,
               descriptor_json TEXT NOT NULL,
               metadata_json TEXT NOT NULL DEFAULT '{"description":null,"labels":{}}',
               updated_at TEXT NOT NULL
           ) STRICT;
           CREATE TABLE IF NOT EXISTS runs (
               run_id TEXT PRIMARY KEY,
               accepted_at TEXT NOT NULL,
               initial_image_name TEXT,
               metadata_json TEXT NOT NULL DEFAULT '{"description":null,"labels":{}}',
               input_json TEXT NOT NULL,
               input_identity_json TEXT NOT NULL,
               cancellation_requested_at TEXT,
               terminal_at TEXT,
               completion_json TEXT
           ) STRICT;
           CREATE INDEX IF NOT EXISTS runs_acceptance_order
               ON runs(accepted_at DESC, run_id DESC);
           CREATE TABLE IF NOT EXISTS run_executions (
               run_id TEXT PRIMARY KEY REFERENCES runs(run_id),
               owner_boot_id TEXT NOT NULL,
               owner_pid INTEGER NOT NULL,
               owner_start_ticks INTEGER NOT NULL,
               phase TEXT NOT NULL CHECK (phase IN ('accepted', 'engine_running', 'result_staged', 'terminal')),
               completion_json TEXT
           ) STRICT;
           CREATE TABLE IF NOT EXISTS run_tombstones (
               run_id TEXT PRIMARY KEY,
               deleted_at TEXT NOT NULL,
               operation_id TEXT NOT NULL
           ) STRICT;
           CREATE INDEX IF NOT EXISTS run_tombstones_operation
               ON run_tombstones(operation_id, run_id);"#,
    )?;
    ensure_metadata_column(&transaction, "catalog")?;
    ensure_metadata_column(&transaction, "runs")?;
    ensure_column(
        &transaction,
        "runs",
        "initial_image_name",
        "ALTER TABLE runs ADD COLUMN initial_image_name TEXT",
    )?;
    ensure_column(
        &transaction,
        "runs",
        "cancellation_requested_at",
        "ALTER TABLE runs ADD COLUMN cancellation_requested_at TEXT",
    )?;
    if version == 0 {
        migrate_run_records(&transaction)?;
    }
    if version == 2 {
        migrate_execution_journal_v2(&transaction)?;
    }
    transaction.pragma_update(None, "user_version", DATABASE_SCHEMA_VERSION)?;
    transaction.commit()?;
    Ok(())
}

fn migrate_execution_journal_v2(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        "ALTER TABLE run_executions RENAME TO run_executions_v2;
         CREATE TABLE run_executions (
             run_id TEXT PRIMARY KEY REFERENCES runs(run_id),
             owner_boot_id TEXT NOT NULL,
             owner_pid INTEGER NOT NULL,
             owner_start_ticks INTEGER NOT NULL,
             phase TEXT NOT NULL CHECK (
                 phase IN ('accepted', 'engine_running', 'result_staged', 'terminal')
             ),
             completion_json TEXT
         ) STRICT;
         INSERT INTO run_executions(
             run_id, owner_boot_id, owner_pid, owner_start_ticks, phase, completion_json
         )
         SELECT run_id, owner_boot_id, owner_pid, owner_start_ticks, phase, completion_json
         FROM run_executions_v2;
         DROP TABLE run_executions_v2;",
    )?;
    Ok(())
}

fn migrate_run_records(connection: &Connection) -> Result<()> {
    let records = {
        let mut statement = connection.prepare(
            "SELECT run_id, input_json, input_identity_json, completion_json FROM main.runs",
        )?;
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (run_id, input, identity, completion) in records {
        let input = migrate_input(&input)
            .with_context(|| format!("failed to migrate RunInput for {run_id}"))?;
        let identity = migrate_identity(&identity)
            .with_context(|| format!("failed to migrate RunInput identity for {run_id}"))?;
        let completion = completion
            .map(|value| {
                migrate_completion(&value)
                    .with_context(|| format!("failed to migrate Run completion for {run_id}"))
            })
            .transpose()?;
        connection.execute(
            "UPDATE main.runs
             SET input_json = ?2, input_identity_json = ?3, completion_json = ?4
             WHERE run_id = ?1",
            params![run_id, input, identity, completion],
        )?;
    }
    Ok(())
}

fn ensure_metadata_column(connection: &Connection, table: &str) -> Result<()> {
    ensure_column(
        connection,
        table,
        "metadata_json",
        &format!(
            "ALTER TABLE {table} ADD COLUMN metadata_json TEXT NOT NULL \
             DEFAULT '{{\"description\":null,\"labels\":{{}}}}'"
        ),
    )
}

fn ensure_column(
    connection: &Connection,
    table: &str,
    column_name: &str,
    migration: &str,
) -> Result<()> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = statement.query_map([], |row| row.get::<_, String>(1))?;
    for column in columns {
        if column? == column_name {
            return Ok(());
        }
    }
    connection.execute(migration, [])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::Duration;

    use chrono::{DateTime, FixedOffset};
    use oci_spec::image::Descriptor;
    use run_protocol::{
        CreateFacts, ExecutionInterval, ExecutionOutput, FinalEnvironment, ImageDescriptor,
        Network, OperationReport, ProcessResult, ProgramId, ProgramInput, ProgramOutput,
        RunControls, RunInput, RuntimeConfig, Secrets, StartFacts, StdinOutput, StdinWriteFacts,
        StreamFacts,
    };
    use serde_json::json;

    use super::{
        BUSY_TIMEOUT, DATABASE_SCHEMA_VERSION, Database, ExecutionOwner, NewRun, RunInsertion,
    };
    use crate::metadata::Metadata;
    use crate::run_record::{CompletionRecord, InputIdentityRecord, InputRecord};

    #[test]
    fn configures_the_project_owned_busy_timeout() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let database =
            Database::open(&directory.path().join("runlab.sqlite3")).expect("open database");
        let milliseconds = database
            .with_connection(|connection| {
                connection
                    .query_row("PRAGMA busy_timeout", [], |row| row.get::<_, i64>(0))
                    .map_err(Into::into)
            })
            .expect("busy timeout");
        assert_eq!(
            milliseconds,
            i64::try_from(BUSY_TIMEOUT.as_millis()).expect("timeout fits i64")
        );
    }

    #[test]
    fn concurrent_writer_waits_for_the_project_owned_busy_timeout() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("runlab.sqlite3");
        let database = Database::open(&path).expect("open waiting database");
        let blocker = rusqlite::Connection::open(&path).expect("open blocking connection");
        blocker
            .execute_batch("BEGIN IMMEDIATE")
            .expect("hold database write lock");

        let barrier = Arc::new(Barrier::new(2));
        let worker_barrier = Arc::clone(&barrier);
        let writer = thread::spawn(move || {
            worker_barrier.wait();
            database.with_connection(|connection| {
                connection.execute(
                    "INSERT INTO catalog(name, descriptor_json, updated_at) VALUES (
                        'waited', '{\"digest\":\"sha256:test\"}', '2026-08-29T00:00:00Z'
                     )",
                    [],
                )?;
                Ok(())
            })
        });
        barrier.wait();
        thread::sleep(Duration::from_millis(100));
        blocker.execute_batch("COMMIT").expect("release write lock");
        writer
            .join()
            .expect("writer thread")
            .expect("waiting write succeeds");
    }

    #[test]
    fn projects_a_real_run_output_through_the_versioned_record() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("runlab.sqlite3");
        let database = Database::open(&path).expect("open database");
        let (input, runtime_bytes, descriptor) = protocol_input();
        let input_record = InputRecord::primary(
            &descriptor,
            &runtime_bytes,
            b"",
            &Secrets::empty(),
            None,
            Network::Isolated,
            true,
        );
        let identity = InputIdentityRecord::primary(
            &descriptor,
            input.programs()[&ProgramId::primary()]
                .runtime_config()
                .as_json(),
            b"",
            &Secrets::empty(),
            None,
            Network::Isolated,
        );
        let metadata = Metadata::default();
        let owner = ExecutionOwner {
            boot_id: "test-boot".to_owned(),
            pid: 1,
            start_ticks: 1,
        };
        database
            .run_insert(&NewRun {
                run_id: "550e8400-e29b-41d4-a716-446655440000",
                accepted_at: "2026-08-27T00:00:00Z",
                initial_image_name: Some("base"),
                metadata: &metadata,
                input: &input_record,
                input_identity: &identity,
                owner: &owner,
            })
            .expect("insert Run");
        database
            .run_mark_engine_running("550e8400-e29b-41d4-a716-446655440000")
            .expect("mark Engine running");
        let completion = CompletionRecord::engine_returned(Ok(protocol_output(&input)));
        database
            .run_stage_completion("550e8400-e29b-41d4-a716-446655440000", &completion)
            .expect("stage completion");
        drop(database);

        let database = Database::open(&path).expect("reopen database after staged completion");
        let journal = database
            .run_execution("550e8400-e29b-41d4-a716-446655440000")
            .expect("read execution journal")
            .expect("execution journal exists");
        assert!(journal.completion.is_some());
        database
            .run_publish_staged(
                "550e8400-e29b-41d4-a716-446655440000",
                "2026-08-27T00:00:04Z",
            )
            .expect("complete Run");

        let facts = database
            .with_connection(|connection| {
                connection
                    .query_row(
                        "SELECT initial_image_digest, primary_exit_code, primary_stdout_bytes \
                         FROM temp.runs",
                        [],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, i64>(1)?,
                                row.get::<_, i64>(2)?,
                            ))
                        },
                    )
                    .map_err(Into::into)
            })
            .expect("public relation");
        assert_eq!(facts.0, descriptor.digest().to_string());
        assert_eq!(facts.1, 0);
        assert_eq!(facts.2, 5);
    }

    #[test]
    fn records_database_schema_version_and_rejects_future_versions() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("runlab.sqlite3");
        let database = Database::open(&path).expect("open database");
        let version = database
            .with_connection(|connection| {
                connection
                    .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                    .map_err(Into::into)
            })
            .expect("schema version");
        assert_eq!(version, DATABASE_SCHEMA_VERSION);
        assert_eq!(version, 4, "schema v4 is the intentional downgrade barrier");
        let tombstones_exist = database
            .with_connection(|connection| {
                connection
                    .query_row(
                        "SELECT count(*) FROM sqlite_schema
                         WHERE type = 'table' AND name = 'run_tombstones'",
                        [],
                        |row| row.get::<_, i64>(0),
                    )
                    .map_err(Into::into)
            })
            .expect("tombstone schema");
        assert_eq!(tombstones_exist, 1);
        drop(database);

        let connection = rusqlite::Connection::open(&path).expect("raw connection");
        connection
            .pragma_update(None, "user_version", DATABASE_SCHEMA_VERSION + 1)
            .expect("future version");
        drop(connection);
        let Err(error) = Database::open(&path) else {
            panic!("future schema must be rejected");
        };
        assert!(error.to_string().contains("newer than supported"));
    }

    #[test]
    fn tombstone_permanently_blocks_run_identity_reuse() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("runlab.sqlite3");
        let database = Database::open(&path).expect("open database");
        let (input, runtime_bytes, descriptor) = protocol_input();
        let input_record = InputRecord::primary(
            &descriptor,
            &runtime_bytes,
            b"",
            &Secrets::empty(),
            None,
            Network::Isolated,
            true,
        );
        let identity = InputIdentityRecord::primary(
            &descriptor,
            input.programs()[&ProgramId::primary()]
                .runtime_config()
                .as_json(),
            b"",
            &Secrets::empty(),
            None,
            Network::Isolated,
        );
        let metadata = Metadata::default();
        let owner = ExecutionOwner {
            boot_id: "test-boot".to_owned(),
            pid: 1,
            start_ticks: 1,
        };
        let run_id = "550e8400-e29b-41d4-a716-446655440099";
        database
            .with_connection(|connection| {
                connection.execute(
                    "INSERT INTO run_tombstones(run_id, deleted_at, operation_id)
                     VALUES (?1, ?2, ?3)",
                    [
                        run_id,
                        "2026-08-29T00:00:00Z",
                        "550e8400-e29b-41d4-a716-446655440098",
                    ],
                )?;
                Ok(())
            })
            .expect("insert tombstone");
        let insertion = database
            .run_insert(&NewRun {
                run_id,
                accepted_at: "2026-08-29T00:00:01Z",
                initial_image_name: Some("base"),
                metadata: &metadata,
                input: &input_record,
                input_identity: &identity,
                owner: &owner,
            })
            .expect("check tombstone during insertion");
        let RunInsertion::Deleted(tombstone) = insertion else {
            panic!("tombstoned Run identity was not rejected");
        };
        assert_eq!(tombstone.run_id, run_id);
        assert!(database.run_get(run_id).expect("read Run").is_none());
    }

    #[test]
    fn migrates_v2_execution_journal_to_the_accepted_phase() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("runlab.sqlite3");
        drop(Database::open(&path).expect("create current database"));
        let connection = rusqlite::Connection::open(&path).expect("raw connection");
        connection
            .execute_batch(
                "DROP TABLE run_executions;
                 CREATE TABLE run_executions (
                     run_id TEXT PRIMARY KEY REFERENCES runs(run_id),
                     owner_boot_id TEXT NOT NULL,
                     owner_pid INTEGER NOT NULL,
                     owner_start_ticks INTEGER NOT NULL,
                     phase TEXT NOT NULL CHECK (
                         phase IN ('engine_running', 'result_staged', 'terminal')
                     ),
                     completion_json TEXT
                 ) STRICT;
                 PRAGMA user_version = 2;",
            )
            .expect("v2 execution journal");
        drop(connection);

        let database = Database::open(&path).expect("migrate v2 database");
        let table_sql: String = database
            .with_connection(|connection| {
                connection
                    .query_row(
                        "SELECT sql FROM sqlite_schema WHERE name = 'run_executions'",
                        [],
                        |row| row.get(0),
                    )
                    .map_err(Into::into)
            })
            .expect("execution journal schema");
        assert!(table_sql.contains("'accepted'"));
    }

    #[test]
    fn leaves_schema_version_unchanged_when_record_migration_fails() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("runlab.sqlite3");
        let connection = rusqlite::Connection::open(&path).expect("raw connection");
        connection
            .execute_batch(
                "CREATE TABLE runs (
                    run_id TEXT PRIMARY KEY,
                    accepted_at TEXT NOT NULL,
                    input_json TEXT NOT NULL,
                    input_identity_json TEXT NOT NULL,
                    terminal_at TEXT,
                    completion_json TEXT
                 ) STRICT;
                 INSERT INTO runs VALUES (
                    '550e8400-e29b-41d4-a716-446655440000',
                    '2026-08-27T00:00:00Z', '{}', '{}', NULL, NULL
                 );",
            )
            .expect("legacy database");
        drop(connection);

        assert!(Database::open(&path).is_err());
        let connection = rusqlite::Connection::open(&path).expect("inspect database");
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("schema version");
        assert_eq!(version, 0);
        let columns: Vec<String> = connection
            .prepare("PRAGMA table_info(runs)")
            .expect("table info")
            .query_map([], |row| row.get(1))
            .expect("columns")
            .collect::<rusqlite::Result<_>>()
            .expect("column names");
        assert!(!columns.iter().any(|column| column == "metadata_json"));
    }

    fn at(second: u32) -> DateTime<FixedOffset> {
        DateTime::parse_from_rfc3339(&format!("2026-08-27T00:00:{second:02}Z")).expect("timestamp")
    }

    fn descriptor(byte: char) -> Descriptor {
        serde_json::from_value(json!({
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": format!("sha256:{}", byte.to_string().repeat(64)),
            "size": 1
        }))
        .expect("descriptor")
    }

    fn protocol_input() -> (RunInput, Vec<u8>, Descriptor) {
        let descriptor = descriptor('a');
        let runtime_bytes = serde_json::to_vec(&json!({
            "ociVersion": "1.3.0",
            "root": {"path": "rootfs"},
            "process": {
                "terminal": false,
                "args": ["/bin/true"],
                "cwd": "/",
                "user": {"uid": 0, "gid": 0}
            },
            "linux": {}
        }))
        .expect("runtime JSON");
        let runtime = RuntimeConfig::parse(runtime_bytes.clone()).expect("runtime config");
        let program = ProgramInput::new(
            ImageDescriptor::new(descriptor.clone()).expect("image"),
            runtime,
            Vec::new(),
            Secrets::empty(),
        )
        .expect("program input");
        let input = RunInput::new(
            BTreeMap::from([(ProgramId::primary(), program)]),
            RunControls::new(None, Network::Isolated, true),
        )
        .expect("RunInput");
        (input, runtime_bytes, descriptor)
    }

    fn protocol_output(input: &RunInput) -> run_protocol::RunOutput {
        let program = ProgramOutput::new(
            OperationReport::succeeded(CreateFacts::new(at(1))),
            OperationReport::succeeded(StartFacts::new(at(2))),
            ProcessResult::Exited {
                code: 0,
                ended_at: at(3),
            },
            StdinOutput::new(
                OperationReport::succeeded(StdinWriteFacts::new(0)),
                OperationReport::succeeded(()),
            ),
            OperationReport::succeeded(
                StreamFacts::new(b"hello".to_vec(), false, true).expect("stdout"),
            ),
            OperationReport::succeeded(StreamFacts::new(Vec::new(), false, true).expect("stderr")),
            [],
            FinalEnvironment::captured(ImageDescriptor::new(descriptor('b')).expect("final image")),
            [],
        )
        .expect("ProgramOutput");
        run_protocol::RunOutput::new(
            input,
            ExecutionOutput::new(ExecutionInterval::entered(at(2), at(3)), false, false, [])
                .expect("execution output"),
            BTreeMap::from([(ProgramId::primary(), program)]),
        )
        .expect("RunOutput")
    }
}
