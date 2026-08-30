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
const DATABASE_SCHEMA_VERSION: i64 = 6;

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

pub(crate) struct NewObservation<'a> {
    pub(crate) observation_id: &'a str,
    pub(crate) run_id: &'a str,
    pub(crate) observation_type: &'a str,
    pub(crate) submitted_at: &'a str,
    pub(crate) method_json: &'a str,
    pub(crate) payload_json: &'a str,
    pub(crate) supersedes_observation_id: Option<&'a str>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StoredObservation {
    pub(crate) observation_id: String,
    pub(crate) run_id: String,
    pub(crate) observation_type: String,
    pub(crate) submitted_at: String,
    pub(crate) method_json: String,
    pub(crate) payload_json: String,
    pub(crate) supersedes_observation_id: Option<String>,
}

pub(crate) struct NewObservationType<'a> {
    pub(crate) observation_type: &'a str,
    pub(crate) registered_at: &'a str,
    pub(crate) title: &'a str,
    pub(crate) description: &'a str,
    pub(crate) payload_schema_json: &'a str,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StoredObservationType {
    pub(crate) observation_type: String,
    pub(crate) registered_at: String,
    pub(crate) title: String,
    pub(crate) description: String,
    pub(crate) payload_schema_json: String,
}

#[derive(Debug)]
pub(crate) enum ObservationTypeInsertion {
    Created(StoredObservationType),
    Existing(StoredObservationType),
    Conflict,
}

#[derive(Debug)]
pub(crate) enum ObservationInsertion {
    Created(StoredObservation),
    Existing(StoredObservation),
    IdentityConflict,
    RunNotFound,
    RunNotTerminal,
    SupersededNotFound,
    SupersededMismatch,
}

pub(crate) struct NewObservationRetraction<'a> {
    pub(crate) retraction_id: &'a str,
    pub(crate) observation_id: &'a str,
    pub(crate) retracted_at: &'a str,
    pub(crate) reason: &'a str,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StoredObservationRetraction {
    pub(crate) retraction_id: String,
    pub(crate) observation_id: String,
    pub(crate) retracted_at: String,
    pub(crate) reason: String,
}

#[derive(Debug)]
pub(crate) enum ObservationRetractionInsertion {
    Created(StoredObservationRetraction),
    Existing(StoredObservationRetraction),
    IdentityConflict,
    ObservationNotFound,
    ObservationInactive,
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
    pub(crate) observation_count: u64,
    pub(crate) observation_bytes: u64,
    observation_fingerprint: String,
}

impl StoredRunDeletionFacts {
    fn run_record_fingerprint(&self) -> String {
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

    pub(crate) fn asset_fingerprint(&self) -> String {
        let mut hasher = Sha256::new();
        for value in [
            self.run_record_fingerprint(),
            self.observation_fingerprint.clone(),
        ] {
            hasher.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
            hasher.update(value.as_bytes());
        }
        format_sha256(hasher)
    }

    pub(crate) fn run_record_bytes(&self) -> u64 {
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

    pub(crate) fn asset_bytes(&self) -> u64 {
        self.run_record_bytes()
            .saturating_add(self.observation_bytes)
    }
}

pub(crate) struct RunDeletionFacts {
    pub(crate) runs: BTreeMap<String, StoredRunDeletionFacts>,
    pub(crate) tombstones: BTreeMap<String, RunTombstone>,
    pub(crate) catalog_descriptors: Vec<(String, Value)>,
}

pub(crate) struct PlannedRunDeletion<'a> {
    pub(crate) run_id: &'a str,
    pub(crate) asset_fingerprint: &'a str,
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

    pub(crate) fn observation_type_insert(
        &self,
        observation_type: &NewObservationType<'_>,
    ) -> Result<ObservationTypeInsertion> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(stored) = transaction
            .query_row(
                "SELECT observation_type, registered_at, title, description, payload_schema_json
                 FROM main.observation_types WHERE observation_type = ?1",
                [observation_type.observation_type],
                read_observation_type,
            )
            .optional()?
        {
            let same = stored.title == observation_type.title
                && stored.description == observation_type.description
                && stored.payload_schema_json == observation_type.payload_schema_json;
            transaction.commit()?;
            return Ok(if same {
                ObservationTypeInsertion::Existing(stored)
            } else {
                ObservationTypeInsertion::Conflict
            });
        }
        transaction.execute(
            "INSERT INTO main.observation_types(
                observation_type, registered_at, title, description, payload_schema_json
             ) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                observation_type.observation_type,
                observation_type.registered_at,
                observation_type.title,
                observation_type.description,
                observation_type.payload_schema_json,
            ],
        )?;
        let stored = transaction.query_row(
            "SELECT observation_type, registered_at, title, description, payload_schema_json
             FROM main.observation_types WHERE observation_type = ?1",
            [observation_type.observation_type],
            read_observation_type,
        )?;
        transaction.commit()?;
        Ok(ObservationTypeInsertion::Created(stored))
    }

    pub(crate) fn observation_type_get(
        &self,
        observation_type: &str,
    ) -> Result<Option<StoredObservationType>> {
        self.lock()?
            .query_row(
                "SELECT observation_type, registered_at, title, description, payload_schema_json
                 FROM main.observation_types WHERE observation_type = ?1",
                [observation_type],
                read_observation_type,
            )
            .optional()
            .map_err(Into::into)
    }

    pub(crate) fn observation_type_list(
        &self,
        limit: usize,
        after: Option<&str>,
    ) -> Result<Vec<StoredObservationType>> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT observation_type, registered_at, title, description, payload_schema_json
             FROM main.observation_types
             WHERE (?1 IS NULL OR observation_type > ?1)
             ORDER BY observation_type ASC LIMIT ?2",
        )?;
        let limit = i64::try_from(limit).context("Observation Type list limit is too large")?;
        Ok(statement
            .query_map(params![after, limit], read_observation_type)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub(crate) fn observation_insert(
        &self,
        observation: &NewObservation<'_>,
    ) -> Result<ObservationInsertion> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(stored) = transaction
            .query_row(
                "SELECT observation_id, run_id, observation_type, submitted_at, method_json,
                        payload_json, supersedes_observation_id
                 FROM main.observations WHERE observation_id = ?1",
                [observation.observation_id],
                read_observation,
            )
            .optional()?
        {
            let same = stored.run_id == observation.run_id
                && stored.observation_type == observation.observation_type
                && stored.method_json == observation.method_json
                && stored.payload_json == observation.payload_json
                && stored.supersedes_observation_id.as_deref()
                    == observation.supersedes_observation_id;
            transaction.commit()?;
            return Ok(if same {
                ObservationInsertion::Existing(stored)
            } else {
                ObservationInsertion::IdentityConflict
            });
        }
        let run_terminal = transaction
            .query_row(
                "SELECT terminal_at IS NOT NULL AND completion_json IS NOT NULL
                 FROM main.runs WHERE run_id = ?1",
                [observation.run_id],
                |row| row.get::<_, bool>(0),
            )
            .optional()?;
        match run_terminal {
            None => return Ok(ObservationInsertion::RunNotFound),
            Some(false) => return Ok(ObservationInsertion::RunNotTerminal),
            Some(true) => {}
        }
        if let Some(superseded_id) = observation.supersedes_observation_id {
            let superseded = transaction
                .query_row(
                    "SELECT observation_id, run_id, observation_type, submitted_at, method_json,
                            payload_json, supersedes_observation_id
                     FROM main.observations WHERE observation_id = ?1",
                    [superseded_id],
                    read_observation,
                )
                .optional()?;
            let Some(superseded) = superseded else {
                return Ok(ObservationInsertion::SupersededNotFound);
            };
            let inactive: bool = transaction.query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM main.observations WHERE supersedes_observation_id = ?1
                 ) OR EXISTS(
                     SELECT 1 FROM main.observation_retractions WHERE observation_id = ?1
                 )",
                [superseded_id],
                |row| row.get(0),
            )?;
            if superseded.run_id != observation.run_id
                || superseded.observation_type != observation.observation_type
                || inactive
            {
                return Ok(ObservationInsertion::SupersededMismatch);
            }
        }
        transaction.execute(
            "INSERT INTO main.observations(
                observation_id, run_id, observation_type, submitted_at, method_json,
                payload_json, supersedes_observation_id
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                observation.observation_id,
                observation.run_id,
                observation.observation_type,
                observation.submitted_at,
                observation.method_json,
                observation.payload_json,
                observation.supersedes_observation_id,
            ],
        )?;
        let stored = transaction.query_row(
            "SELECT observation_id, run_id, observation_type, submitted_at, method_json,
                    payload_json, supersedes_observation_id
             FROM main.observations WHERE observation_id = ?1",
            [observation.observation_id],
            read_observation,
        )?;
        transaction.commit()?;
        Ok(ObservationInsertion::Created(stored))
    }

    pub(crate) fn observation_retract(
        &self,
        retraction: &NewObservationRetraction<'_>,
    ) -> Result<ObservationRetractionInsertion> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(stored) = transaction
            .query_row(
                "SELECT retraction_id, observation_id, retracted_at, reason
                 FROM main.observation_retractions WHERE retraction_id = ?1",
                [retraction.retraction_id],
                read_observation_retraction,
            )
            .optional()?
        {
            let same = stored.observation_id == retraction.observation_id
                && stored.reason == retraction.reason;
            transaction.commit()?;
            return Ok(if same {
                ObservationRetractionInsertion::Existing(stored)
            } else {
                ObservationRetractionInsertion::IdentityConflict
            });
        }
        let exists: bool = transaction.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM main.observations WHERE observation_id = ?1
             )",
            [retraction.observation_id],
            |row| row.get(0),
        )?;
        if !exists {
            return Ok(ObservationRetractionInsertion::ObservationNotFound);
        }
        let inactive: bool = transaction.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM main.observations WHERE supersedes_observation_id = ?1
             ) OR EXISTS(
                 SELECT 1 FROM main.observation_retractions WHERE observation_id = ?1
             )",
            [retraction.observation_id],
            |row| row.get(0),
        )?;
        if inactive {
            return Ok(ObservationRetractionInsertion::ObservationInactive);
        }
        transaction.execute(
            "INSERT INTO main.observation_retractions(
                retraction_id, observation_id, retracted_at, reason
             ) VALUES (?1, ?2, ?3, ?4)",
            params![
                retraction.retraction_id,
                retraction.observation_id,
                retraction.retracted_at,
                retraction.reason
            ],
        )?;
        let stored = transaction.query_row(
            "SELECT retraction_id, observation_id, retracted_at, reason
             FROM main.observation_retractions WHERE retraction_id = ?1",
            [retraction.retraction_id],
            read_observation_retraction,
        )?;
        transaction.commit()?;
        Ok(ObservationRetractionInsertion::Created(stored))
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
            if let Some(mut run) = transaction
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
                load_observation_deletion_facts(&transaction, &mut run)?;
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
            let Some(mut stored) = stored else {
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
            load_observation_deletion_facts(&transaction, &mut stored)?;
            if stored.asset_fingerprint() != candidate.asset_fingerprint {
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

fn read_observation(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredObservation> {
    Ok(StoredObservation {
        observation_id: row.get("observation_id")?,
        run_id: row.get("run_id")?,
        observation_type: row.get("observation_type")?,
        submitted_at: row.get("submitted_at")?,
        method_json: row.get("method_json")?,
        payload_json: row.get("payload_json")?,
        supersedes_observation_id: row.get("supersedes_observation_id")?,
    })
}

fn read_observation_type(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredObservationType> {
    Ok(StoredObservationType {
        observation_type: row.get("observation_type")?,
        registered_at: row.get("registered_at")?,
        title: row.get("title")?,
        description: row.get("description")?,
        payload_schema_json: row.get("payload_schema_json")?,
    })
}

fn read_observation_retraction(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<StoredObservationRetraction> {
    Ok(StoredObservationRetraction {
        retraction_id: row.get("retraction_id")?,
        observation_id: row.get("observation_id")?,
        retracted_at: row.get("retracted_at")?,
        reason: row.get("reason")?,
    })
}

fn load_observation_deletion_facts(
    connection: &Connection,
    run: &mut StoredRunDeletionFacts,
) -> Result<()> {
    let mut statement = connection.prepare(
        "SELECT observation.observation_id, observation.observation_type,
                observation.submitted_at, observation.method_json,
                observation.payload_json,
                observation.supersedes_observation_id,
                retraction.retraction_id, retraction.retracted_at, retraction.reason
         FROM main.observations AS observation
         LEFT JOIN main.observation_retractions AS retraction
           ON retraction.observation_id = observation.observation_id
         WHERE observation.run_id = ?1
         ORDER BY observation.observation_id",
    )?;
    let mut rows = statement.query([&run.run_id])?;
    let mut hasher = Sha256::new();
    let mut count = 0_u64;
    let mut bytes = 0_u64;
    while let Some(row) = rows.next()? {
        count = count.saturating_add(1);
        for index in 0..9 {
            let value = row.get::<_, Option<String>>(index)?;
            hash_optional_text(&mut hasher, value.as_deref());
            if let Some(value) = value {
                bytes = bytes.saturating_add(u64::try_from(value.len()).unwrap_or(u64::MAX));
            }
        }
    }
    run.observation_count = count;
    run.observation_bytes = bytes;
    run.observation_fingerprint = format_sha256(hasher);
    Ok(())
}

fn hash_optional_text(hasher: &mut Sha256, value: Option<&str>) {
    match value {
        Some(value) => {
            hasher.update([1]);
            hasher.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
            hasher.update(value.as_bytes());
        }
        None => hasher.update([0]),
    }
}

fn format_sha256(hasher: Sha256) -> String {
    let mut encoded = String::from("sha256:");
    for byte in hasher.finalize() {
        use std::fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    encoded
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
        observation_count: 0,
        observation_bytes: 0,
        observation_fingerprint:
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".to_owned(),
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
    ensure_database_tables(&transaction)?;
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
    ensure_builtin_observation_type(&transaction)?;
    if version == 5 {
        migrate_observations_v5(&transaction)?;
    }
    transaction.pragma_update(None, "user_version", DATABASE_SCHEMA_VERSION)?;
    transaction.commit()?;
    Ok(())
}

fn ensure_database_tables(connection: &Connection) -> Result<()> {
    connection.execute_batch(
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
               ON run_tombstones(operation_id, run_id);
           CREATE TABLE IF NOT EXISTS observation_types (
               observation_type TEXT PRIMARY KEY,
               registered_at TEXT NOT NULL,
               title TEXT NOT NULL,
               description TEXT NOT NULL,
               payload_schema_json TEXT NOT NULL
           ) STRICT;
           CREATE TABLE IF NOT EXISTS observations (
               observation_id TEXT PRIMARY KEY,
               run_id TEXT NOT NULL REFERENCES runs(run_id) ON DELETE CASCADE,
               observation_type TEXT NOT NULL
                   REFERENCES observation_types(observation_type),
               submitted_at TEXT NOT NULL,
               method_json TEXT NOT NULL,
               payload_json TEXT NOT NULL,
               supersedes_observation_id TEXT REFERENCES observations(observation_id),
               CHECK (
                   supersedes_observation_id IS NULL
                   OR supersedes_observation_id <> observation_id
               )
           ) STRICT;
           CREATE INDEX IF NOT EXISTS observations_run_order
               ON observations(run_id, submitted_at, observation_id);
           CREATE UNIQUE INDEX IF NOT EXISTS observations_one_replacement
               ON observations(supersedes_observation_id)
               WHERE supersedes_observation_id IS NOT NULL;
           CREATE TABLE IF NOT EXISTS observation_retractions (
               retraction_id TEXT PRIMARY KEY,
               observation_id TEXT NOT NULL UNIQUE
                   REFERENCES observations(observation_id) ON DELETE CASCADE,
               retracted_at TEXT NOT NULL,
               reason TEXT NOT NULL
           ) STRICT;"#,
    )?;
    Ok(())
}

fn ensure_builtin_observation_type(connection: &Connection) -> Result<()> {
    let (title, description, payload_schema_json) =
        crate::observation::builtin_token_usage_parts()?;
    let registered_at = chrono::Utc::now().to_rfc3339();
    connection.execute(
        "INSERT OR IGNORE INTO observation_types(
            observation_type, registered_at, title, description, payload_schema_json
         ) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            crate::observation::TOKEN_USAGE_TYPE,
            registered_at,
            title,
            description,
            payload_schema_json,
        ],
    )?;
    let stored = connection.query_row(
        "SELECT observation_type, registered_at, title, description, payload_schema_json
         FROM observation_types WHERE observation_type = ?1",
        [crate::observation::TOKEN_USAGE_TYPE],
        read_observation_type,
    )?;
    let (title, description, payload_schema_json) =
        crate::observation::builtin_token_usage_parts()?;
    if stored.title != title
        || stored.description != description
        || stored.payload_schema_json != payload_schema_json
    {
        bail!(
            "registered built-in Observation Type differs from this RunLab version: {}",
            crate::observation::TOKEN_USAGE_TYPE
        );
    }
    Ok(())
}

fn migrate_observations_v5(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        "CREATE TABLE observation_retractions_v5_backup AS
             SELECT retraction_id, observation_id, retracted_at, reason
             FROM observation_retractions;
         DROP TABLE observation_retractions;
         ALTER TABLE observations RENAME TO observations_v5;
         CREATE TABLE observations (
             observation_id TEXT PRIMARY KEY,
             run_id TEXT NOT NULL REFERENCES runs(run_id) ON DELETE CASCADE,
             observation_type TEXT NOT NULL REFERENCES observation_types(observation_type),
             submitted_at TEXT NOT NULL,
             method_json TEXT NOT NULL,
             payload_json TEXT NOT NULL,
             supersedes_observation_id TEXT REFERENCES observations(observation_id),
             CHECK (
                 supersedes_observation_id IS NULL
                 OR supersedes_observation_id <> observation_id
             )
         ) STRICT;
         INSERT INTO observations(
             observation_id, run_id, observation_type, submitted_at, method_json,
             payload_json, supersedes_observation_id
         )
         SELECT observation_id, run_id, observation_type, submitted_at, method_json,
                json_remove(payload_json, '$.total_tokens'), supersedes_observation_id
         FROM observations_v5;
         DROP TABLE observations_v5;
         CREATE INDEX observations_run_order
             ON observations(run_id, submitted_at, observation_id);
         CREATE UNIQUE INDEX observations_one_replacement
             ON observations(supersedes_observation_id)
             WHERE supersedes_observation_id IS NOT NULL;
         CREATE TABLE observation_retractions (
             retraction_id TEXT PRIMARY KEY,
             observation_id TEXT NOT NULL UNIQUE
                 REFERENCES observations(observation_id) ON DELETE CASCADE,
             retracted_at TEXT NOT NULL,
             reason TEXT NOT NULL
         ) STRICT;
         INSERT INTO observation_retractions(
             retraction_id, observation_id, retracted_at, reason
         )
         SELECT retraction_id, observation_id, retracted_at, reason
         FROM observation_retractions_v5_backup;
         DROP TABLE observation_retractions_v5_backup;",
    )?;
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
    use std::collections::{BTreeMap, BTreeSet};
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
        BUSY_TIMEOUT, DATABASE_SCHEMA_VERSION, Database, ExecutionOwner, NewObservation,
        NewObservationRetraction, NewObservationType, NewRun, ObservationInsertion,
        ObservationRetractionInsertion, ObservationTypeInsertion, PlannedRunDeletion,
        RunDeletionCommit, RunInsertion,
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
        assert_eq!(version, 6, "schema v6 is the intentional downgrade barrier");
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
        let observation_tables = database
            .with_connection(|connection| {
                connection
                    .query_row(
                        "SELECT count(*) FROM sqlite_schema
                         WHERE type = 'table'
                           AND name IN (
                               'observation_types', 'observations', 'observation_retractions'
                           )",
                        [],
                        |row| row.get::<_, i64>(0),
                    )
                    .map_err(Into::into)
            })
            .expect("Observation schema");
        assert_eq!(observation_tables, 3);
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
    fn migrates_v5_observations_to_the_generic_type_path() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("runlab.sqlite3");
        let connection = rusqlite::Connection::open(&path).expect("raw v5 database");
        connection
            .execute_batch(
                r#"CREATE TABLE runs (
                       run_id TEXT PRIMARY KEY,
                       accepted_at TEXT NOT NULL,
                       initial_image_name TEXT,
                       metadata_json TEXT NOT NULL,
                       input_json TEXT NOT NULL,
                       input_identity_json TEXT NOT NULL,
                       cancellation_requested_at TEXT,
                       terminal_at TEXT,
                       completion_json TEXT
                   ) STRICT;
                   INSERT INTO runs VALUES (
                       '550e8400-e29b-41d4-a716-446655440120',
                       '2026-08-29T00:00:00Z', NULL,
                       '{"description":null,"labels":{}}', '{}', '{}', NULL,
                       '2026-08-29T00:00:01Z', '{}'
                   );
                   CREATE TABLE observations (
                       observation_id TEXT PRIMARY KEY,
                       run_id TEXT NOT NULL REFERENCES runs(run_id) ON DELETE CASCADE,
                       observation_type TEXT NOT NULL,
                       submitted_at TEXT NOT NULL,
                       method_json TEXT NOT NULL,
                       source_refs_json TEXT NOT NULL,
                       payload_json TEXT NOT NULL,
                       supersedes_observation_id TEXT REFERENCES observations(observation_id)
                   ) STRICT;
                   INSERT INTO observations VALUES (
                       '550e8400-e29b-41d4-a716-446655440121',
                       '550e8400-e29b-41d4-a716-446655440120',
                       'runlab/token_usage@v1', '2026-08-29T00:00:02Z',
                       '{"name":"fixture","version":"1"}',
                       '[{"kind":"final_image_path"}]',
                       '{"coverage":"complete","input_tokens":10,"cached_input_tokens":null,"cache_write_input_tokens":null,"output_tokens":2,"reasoning_output_tokens":null,"total_tokens":12}',
                       NULL
                   );
                   CREATE TABLE observation_retractions (
                       retraction_id TEXT PRIMARY KEY,
                       observation_id TEXT NOT NULL UNIQUE
                           REFERENCES observations(observation_id) ON DELETE CASCADE,
                       retracted_at TEXT NOT NULL,
                       reason TEXT NOT NULL
                   ) STRICT;
                   PRAGMA user_version = 5;"#,
            )
            .expect("v5 fixture");
        drop(connection);

        let database = Database::open(&path).expect("migrate v5 database");
        database
            .with_connection(|connection| {
                let source_column: i64 = connection.query_row(
                    "SELECT instr(sql, 'source_refs_json') FROM main.sqlite_schema
                     WHERE type = 'table' AND name = 'observations'",
                    [],
                    |row| row.get(0),
                )?;
                assert_eq!(source_column, 0);
                let payload: String = connection.query_row(
                    "SELECT payload_json FROM main.observations",
                    [],
                    |row| row.get(0),
                )?;
                assert_eq!(
                    serde_json::from_str::<serde_json::Value>(&payload)?["total_tokens"],
                    serde_json::Value::Null
                );
                let registered: i64 = connection.query_row(
                    "SELECT COUNT(*) FROM main.observation_types
                     WHERE observation_type = 'runlab/token_usage@v1'",
                    [],
                    |row| row.get(0),
                )?;
                assert_eq!(registered, 1);
                Ok(())
            })
            .expect("generic Observation storage");
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
    fn observations_are_idempotent_and_corrections_preserve_history() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let database =
            Database::open(&directory.path().join("runlab.sqlite3")).expect("open database");
        let run_id = "550e8400-e29b-41d4-a716-446655440100";
        insert_terminal_run(&database, run_id);

        let original = new_observation(
            "550e8400-e29b-41d4-a716-446655440101",
            run_id,
            r#"{"input_tokens":10,"output_tokens":2,"total_tokens":12}"#,
            None,
        );
        assert!(matches!(
            database
                .observation_insert(&original)
                .expect("insert Observation"),
            ObservationInsertion::Created(_)
        ));
        assert!(matches!(
            database
                .observation_insert(&original)
                .expect("retry Observation"),
            ObservationInsertion::Existing(_)
        ));
        let conflicting = new_observation(
            original.observation_id,
            run_id,
            r#"{"input_tokens":11,"output_tokens":2,"total_tokens":13}"#,
            None,
        );
        assert!(matches!(
            database
                .observation_insert(&conflicting)
                .expect("detect identity conflict"),
            ObservationInsertion::IdentityConflict
        ));

        let replacement = new_observation(
            "550e8400-e29b-41d4-a716-446655440102",
            run_id,
            r#"{"input_tokens":11,"output_tokens":2,"total_tokens":13}"#,
            Some(original.observation_id),
        );
        assert!(matches!(
            database
                .observation_insert(&replacement)
                .expect("replace Observation"),
            ObservationInsertion::Created(_)
        ));
        let second_replacement = new_observation(
            "550e8400-e29b-41d4-a716-446655440103",
            run_id,
            r#"{"input_tokens":12,"output_tokens":2,"total_tokens":14}"#,
            Some(original.observation_id),
        );
        assert!(matches!(
            database
                .observation_insert(&second_replacement)
                .expect("reject second replacement"),
            ObservationInsertion::SupersededMismatch
        ));

        let original_retraction = NewObservationRetraction {
            retraction_id: "550e8400-e29b-41d4-a716-446655440104",
            observation_id: original.observation_id,
            retracted_at: "2026-08-27T00:00:06Z",
            reason: "superseded records cannot also be retracted",
        };
        assert!(matches!(
            database
                .observation_retract(&original_retraction)
                .expect("reject inactive retraction"),
            ObservationRetractionInsertion::ObservationInactive
        ));
        let replacement_retraction = NewObservationRetraction {
            retraction_id: "550e8400-e29b-41d4-a716-446655440105",
            observation_id: replacement.observation_id,
            retracted_at: "2026-08-27T00:00:07Z",
            reason: "source artifact was incomplete",
        };
        assert!(matches!(
            database
                .observation_retract(&replacement_retraction)
                .expect("retract replacement"),
            ObservationRetractionInsertion::Created(_)
        ));
        assert!(matches!(
            database
                .observation_retract(&replacement_retraction)
                .expect("retry retraction"),
            ObservationRetractionInsertion::Existing(_)
        ));
    }

    #[test]
    fn observation_type_registration_is_create_only_and_idempotent() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let database =
            Database::open(&directory.path().join("runlab.sqlite3")).expect("open database");
        let definition = NewObservationType {
            observation_type: "example/score@v1",
            registered_at: "2026-08-29T00:00:00Z",
            title: "Score",
            description: "A fixture score.",
            payload_schema_json: r#"{"$schema":"https://json-schema.org/draft/2020-12/schema","type":"number"}"#,
        };
        assert!(matches!(
            database
                .observation_type_insert(&definition)
                .expect("register Type"),
            ObservationTypeInsertion::Created(_)
        ));
        assert!(matches!(
            database
                .observation_type_insert(&definition)
                .expect("retry Type"),
            ObservationTypeInsertion::Existing(_)
        ));
        let changed = NewObservationType {
            description: "A different contract.",
            ..definition
        };
        assert!(matches!(
            database
                .observation_type_insert(&changed)
                .expect("conflicting Type"),
            ObservationTypeInsertion::Conflict
        ));
    }

    #[test]
    fn run_deletion_freezes_and_cascades_the_observation_set() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let database =
            Database::open(&directory.path().join("runlab.sqlite3")).expect("open database");
        let run_id = "550e8400-e29b-41d4-a716-446655440110";
        insert_terminal_run(&database, run_id);
        let requested = BTreeSet::from([run_id.to_owned()]);
        let before = database
            .run_deletion_facts(&requested)
            .expect("deletion facts before Observation");
        let before_fingerprint = before.runs[run_id].asset_fingerprint();

        let observation = new_observation(
            "550e8400-e29b-41d4-a716-446655440111",
            run_id,
            r#"{"input_tokens":10,"output_tokens":2,"total_tokens":12}"#,
            None,
        );
        assert!(matches!(
            database
                .observation_insert(&observation)
                .expect("insert Observation"),
            ObservationInsertion::Created(_)
        ));
        let replacement = new_observation(
            "550e8400-e29b-41d4-a716-446655440114",
            run_id,
            r#"{"input_tokens":11,"output_tokens":2,"total_tokens":13}"#,
            Some(observation.observation_id),
        );
        assert!(matches!(
            database
                .observation_insert(&replacement)
                .expect("insert replacement"),
            ObservationInsertion::Created(_)
        ));
        assert!(matches!(
            database
                .observation_retract(&NewObservationRetraction {
                    retraction_id: "550e8400-e29b-41d4-a716-446655440115",
                    observation_id: replacement.observation_id,
                    retracted_at: "2026-08-27T00:00:07Z",
                    reason: "source artifact was incomplete",
                })
                .expect("retract replacement"),
            ObservationRetractionInsertion::Created(_)
        ));
        let stale = [PlannedRunDeletion {
            run_id,
            asset_fingerprint: &before_fingerprint,
        }];
        let error = database
            .run_delete_apply(
                "550e8400-e29b-41d4-a716-446655440112",
                "2026-08-27T00:00:08Z",
                &stale,
                &[run_id],
            )
            .expect_err("new Observation makes deletion plan stale");
        assert!(error.to_string().contains("candidate changed"));
        assert!(database.run_get(run_id).expect("read Run").is_some());

        let after = database
            .run_deletion_facts(&requested)
            .expect("deletion facts after Observation");
        let after_run = &after.runs[run_id];
        assert_eq!(after_run.observation_count, 2);
        assert!(after_run.observation_bytes > 0);
        assert_ne!(after_run.asset_fingerprint(), before_fingerprint);
        let after_fingerprint = after_run.asset_fingerprint();
        let current = [PlannedRunDeletion {
            run_id,
            asset_fingerprint: &after_fingerprint,
        }];
        assert!(matches!(
            database
                .run_delete_apply(
                    "550e8400-e29b-41d4-a716-446655440113",
                    "2026-08-27T00:00:09Z",
                    &current,
                    &[run_id],
                )
                .expect("delete Run and Observation"),
            RunDeletionCommit::Applied { .. }
        ));
        let (runs, observations, retractions): (i64, i64, i64) = database
            .with_connection(|connection| {
                connection
                    .query_row(
                        "SELECT (SELECT count(*) FROM main.runs),
                                (SELECT count(*) FROM main.observations),
                                (SELECT count(*) FROM main.observation_retractions)",
                        [],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )
                    .map_err(Into::into)
            })
            .expect("count persisted assets");
        assert_eq!((runs, observations, retractions), (0, 0, 0));
        assert!(database.run_tombstone(run_id).expect("tombstone").is_some());
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

    fn insert_terminal_run(database: &Database, run_id: &str) {
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
        database
            .run_insert(&NewRun {
                run_id,
                accepted_at: "2026-08-27T00:00:00Z",
                initial_image_name: Some("base"),
                metadata: &Metadata::default(),
                input: &input_record,
                input_identity: &identity,
                owner: &ExecutionOwner {
                    boot_id: "test-boot".to_owned(),
                    pid: 1,
                    start_ticks: 1,
                },
            })
            .expect("insert Run");
        database
            .run_mark_engine_running(run_id)
            .expect("mark Engine running");
        database
            .run_stage_completion(
                run_id,
                &CompletionRecord::engine_returned(Ok(protocol_output(&input))),
            )
            .expect("stage completion");
        database
            .run_publish_staged(run_id, "2026-08-27T00:00:04Z")
            .expect("publish terminal Run");
    }

    fn new_observation<'a>(
        observation_id: &'a str,
        run_id: &'a str,
        payload_json: &'a str,
        supersedes_observation_id: Option<&'a str>,
    ) -> NewObservation<'a> {
        NewObservation {
            observation_id,
            run_id,
            observation_type: "runlab/token_usage@v1",
            submitted_at: "2026-08-27T00:00:05Z",
            method_json: r#"{"name":"test","version":"1"}"#,
            payload_json,
            supersedes_observation_id,
        }
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
