use std::fs;
use std::fs::File;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension as _, Row, Transaction, TransactionBehavior, params,
};

use crate::core::{
    AcceptedRunRecord, ImageSlot, MAX_CAPTURED_STREAM_BYTES, RunId, RunRecord, StoredBytes,
    TerminalRunRecord,
};
use crate::integrity::{canonical_json, digest_bytes, ensure_private_directory};

const STORAGE_VERSION: i64 = 6;
const MAX_RETENTION_RUNS: usize = 100_000;
const MAX_RECORD_JSON_BYTES: u64 = 16 * 1024 * 1024;
const MAX_STORED_CONTENT_BYTES: u64 = 16 * 1024 * 1024;
const SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS schema_metadata (
    key TEXT PRIMARY KEY,
    value INTEGER NOT NULL
);

INSERT INTO schema_metadata(key, value)
VALUES ('storage_version', 6)
ON CONFLICT(key) DO NOTHING;

CREATE TABLE IF NOT EXISTS runs (
    run_id TEXT PRIMARY KEY,
    lifecycle TEXT NOT NULL CHECK (lifecycle IN ('accepted', 'terminal')),
    accepted_at TEXT NOT NULL,
    terminal_at TEXT,
    initial_manifest_digest TEXT NOT NULL,
    final_manifest_digest TEXT,
    service_initial_manifest_digest TEXT,
    service_final_manifest_digest TEXT,
    accepted_record_json BLOB NOT NULL,
    terminal_record_json BLOB,
    runtime_config BLOB NOT NULL,
    service_runtime_config BLOB,
    stdin BLOB NOT NULL,
    stdout BLOB,
    stderr BLOB,
    service_stdout BLOB,
    service_stderr BLOB,
    CHECK (
        (lifecycle = 'accepted' AND terminal_at IS NULL AND terminal_record_json IS NULL)
        OR
        (lifecycle = 'terminal' AND terminal_at IS NOT NULL AND terminal_record_json IS NOT NULL)
    ),
    CHECK (
        (service_initial_manifest_digest IS NULL AND service_runtime_config IS NULL)
        OR
        (service_initial_manifest_digest IS NOT NULL AND service_runtime_config IS NOT NULL)
    ),
    CHECK (service_initial_manifest_digest IS NOT NULL OR service_final_manifest_digest IS NULL),
    CHECK (service_initial_manifest_digest IS NOT NULL OR service_stdout IS NULL),
    CHECK (service_initial_manifest_digest IS NOT NULL OR service_stderr IS NULL)
);

CREATE TRIGGER IF NOT EXISTS terminal_runs_are_immutable
BEFORE UPDATE ON runs
WHEN OLD.lifecycle = 'terminal'
BEGIN
    SELECT RAISE(ABORT, 'terminal Run Record is immutable');
END;
";

#[derive(Debug, Clone, Copy)]
pub enum RunBytesField {
    Stdout,
    Stderr,
    ManagedServiceStdout,
    ManagedServiceStderr,
}

impl RunBytesField {
    const fn column(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
            Self::ManagedServiceStdout => "service_stdout",
            Self::ManagedServiceStderr => "service_stderr",
        }
    }
}

#[derive(Debug, Clone)]
pub struct RunDatabase {
    path: PathBuf,
    read_only: bool,
}

pub struct RunRecordPage {
    pub records: Vec<RunRecord>,
    pub has_more: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StoredRunLifecycle {
    Accepted,
    Terminal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RunImageParticipant {
    Primary,
    ManagedService { name: crate::core::ServiceName },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunImageSlot {
    Initial,
    Final,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunImageRoot {
    pub run_id: RunId,
    pub participant: RunImageParticipant,
    pub slot: RunImageSlot,
    pub descriptor: crate::core::OciDescriptor,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VerifiedRunRetention {
    pub run_id: RunId,
    pub lifecycle: StoredRunLifecycle,
    pub image_roots: Vec<RunImageRoot>,
    pub verified_stored_bytes_count: usize,
    pub verified_stored_bytes_size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunRetentionSnapshot {
    pub runs: Vec<VerifiedRunRetention>,
    pub accepted_count: usize,
    pub verified_stored_bytes_count: usize,
    pub verified_stored_bytes_size: u64,
}

impl RunDatabase {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            ensure_private_directory(parent)?;
        }
        let database = Self {
            path: path.to_path_buf(),
            read_only: false,
        };
        let connection = database.connect()?;
        let has_metadata = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'schema_metadata')",
                [],
                |row| row.get::<_, bool>(0),
            )
            .context("failed to inspect Run database schema")?;
        if has_metadata {
            verify_storage_version(&connection)?;
        }
        connection
            .execute_batch(SCHEMA)
            .context("failed to initialize Run database schema")?;
        verify_storage_version(&connection)?;
        secure_file(path)?;
        Ok(database)
    }

    pub(crate) fn open_existing(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let metadata = fs::symlink_metadata(path)
            .with_context(|| format!("failed to inspect Run database {}", path.display()))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!("Run database is not a regular file: {}", path.display());
        }
        verify_read_only_database_format(path)?;
        let database = Self {
            path: path.to_path_buf(),
            read_only: true,
        };
        let connection = database.connect()?;
        verify_storage_version(&connection)?;
        Ok(database)
    }

    pub(crate) fn retention_snapshot(&self) -> Result<RunRetentionSnapshot> {
        self.query_retention_snapshot(None)
    }

    pub(crate) fn retention_snapshot_for(&self, run_id: RunId) -> Result<VerifiedRunRetention> {
        let mut snapshot = self.query_retention_snapshot(Some(run_id))?;
        snapshot
            .runs
            .pop()
            .with_context(|| format!("Run is unknown: {run_id}"))
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn accept(
        &self,
        record: &AcceptedRunRecord,
        runtime_config: &[u8],
        stdin: &[u8],
    ) -> Result<()> {
        self.accept_with_managed_service(record, runtime_config, stdin, None)
    }

    pub fn accept_with_managed_service(
        &self,
        record: &AcceptedRunRecord,
        runtime_config: &[u8],
        stdin: &[u8],
        managed_service_runtime_config: Option<&[u8]>,
    ) -> Result<()> {
        record.validate()?;
        verify_stored_bytes(&record.runtime_config, runtime_config, "Runtime Config")?;
        verify_stored_bytes(&record.controls.stdin, stdin, "stdin")?;
        verify_managed_service_runtime(
            record.managed_service.as_ref(),
            managed_service_runtime_config,
        )?;
        let accepted_json = canonical_json(record)?;
        ensure_content_limit(
            &accepted_json,
            MAX_RECORD_JSON_BYTES,
            "accepted_record_json",
        )?;
        ensure_content_limit(runtime_config, MAX_STORED_CONTENT_BYTES, "runtime_config")?;
        ensure_optional_content_limit(
            managed_service_runtime_config,
            MAX_STORED_CONTENT_BYTES,
            "service_runtime_config",
        )?;
        ensure_content_limit(stdin, MAX_STORED_CONTENT_BYTES, "stdin")?;
        let service_initial_digest = record
            .managed_service
            .as_ref()
            .map(|service| service.initial_image.digest.to_string());
        let mut connection = self.connect()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .context("failed to start Run acceptance transaction")?;
        ensure_run_capacity(&transaction, MAX_RETENTION_RUNS)?;
        let result = transaction.execute(
            r"
            INSERT INTO runs (
                run_id, lifecycle, accepted_at, initial_manifest_digest,
                service_initial_manifest_digest, accepted_record_json,
                runtime_config, service_runtime_config, stdin
            ) VALUES (?1, 'accepted', ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            ",
            params![
                record.run_id.to_string(),
                record.accepted_at.to_rfc3339(),
                record.initial_image.digest.to_string(),
                service_initial_digest,
                accepted_json,
                runtime_config,
                managed_service_runtime_config,
                stdin,
            ],
        );
        match result {
            Ok(1) => transaction
                .commit()
                .context("failed to commit Run acceptance"),
            Ok(rows) => bail!("Run acceptance inserted an unexpected {rows} rows"),
            Err(error) if error.to_string().contains("UNIQUE constraint failed") => {
                bail!("Run identity already exists: {}", record.run_id)
            }
            Err(error) => Err(error).context("failed to insert accepted Run"),
        }
    }

    pub fn terminal(
        &self,
        record: &TerminalRunRecord,
        stdout: Option<&[u8]>,
        stderr: Option<&[u8]>,
    ) -> Result<()> {
        self.terminal_with_managed_service(record, stdout, stderr, None, None)
    }

    pub fn terminal_with_managed_service(
        &self,
        record: &TerminalRunRecord,
        stdout: Option<&[u8]>,
        stderr: Option<&[u8]>,
        managed_service_stdout: Option<&[u8]>,
        managed_service_stderr: Option<&[u8]>,
    ) -> Result<()> {
        record.validate()?;
        verify_optional_stored_bytes(&record.stdout, stdout, "stdout")?;
        verify_optional_stored_bytes(&record.stderr, stderr, "stderr")?;
        verify_managed_service_streams(
            record.managed_service.as_ref(),
            managed_service_stdout,
            managed_service_stderr,
        )?;
        let terminal_json = canonical_json(record)?;
        ensure_content_limit(
            &terminal_json,
            MAX_RECORD_JSON_BYTES,
            "terminal_record_json",
        )?;
        for (bytes, name) in [
            (stdout, "stdout"),
            (stderr, "stderr"),
            (managed_service_stdout, "service_stdout"),
            (managed_service_stderr, "service_stderr"),
        ] {
            ensure_optional_content_limit(bytes, MAX_CAPTURED_STREAM_BYTES, name)?;
        }
        let final_digest = match &record.final_image {
            ImageSlot::Available { manifest } => Some(manifest.digest.to_string()),
            ImageSlot::Unavailable { .. } | ImageSlot::NotApplicable => None,
        };
        let service_final_digest = record.managed_service.as_ref().and_then(|service| {
            if let ImageSlot::Available { manifest } = &service.final_image {
                Some(manifest.digest.to_string())
            } else {
                None
            }
        });
        let mut connection = self.connect()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .context("failed to start Run terminal transaction")?;
        let accepted_json: Vec<u8> = transaction
            .query_row(
                "SELECT accepted_record_json FROM runs WHERE run_id = ?1 AND lifecycle = 'accepted'",
                [record.run_id.to_string()],
                |row| row.get(0),
            )
            .optional()
            .context("failed to read accepted Run")?
            .with_context(|| {
                format!(
                    "Run Record is missing or already immutable terminal: {}",
                    record.run_id
                )
            })?;
        let accepted: AcceptedRunRecord =
            serde_json::from_slice(&accepted_json).context("stored accepted Run is invalid")?;
        accepted.validate()?;
        verify_terminal_identity(&accepted, record)?;
        let rows = transaction
            .execute(
                r"
                UPDATE runs SET
                    lifecycle = 'terminal',
                    terminal_at = ?1,
                    final_manifest_digest = ?2,
                    service_final_manifest_digest = ?3,
                    terminal_record_json = ?4,
                    stdout = ?5,
                    stderr = ?6,
                    service_stdout = ?7,
                    service_stderr = ?8
                WHERE run_id = ?9 AND lifecycle = 'accepted'
                ",
                params![
                    record.terminal_at.to_rfc3339(),
                    final_digest,
                    service_final_digest,
                    terminal_json,
                    stdout,
                    stderr,
                    managed_service_stdout,
                    managed_service_stderr,
                    record.run_id.to_string(),
                ],
            )
            .context("failed to update terminal Run")?;
        if rows != 1 {
            bail!("terminal Run Record is immutable: {}", record.run_id);
        }
        transaction
            .commit()
            .context("failed to commit terminal Run")
    }

    pub fn get(&self, run_id: RunId) -> Result<RunRecord> {
        self.find(run_id)?
            .with_context(|| format!("Run is unknown: {run_id}"))
    }

    pub fn list(
        &self,
        lifecycle: Option<&str>,
        after: Option<RunId>,
        limit: usize,
    ) -> Result<RunRecordPage> {
        if let Some(lifecycle) = lifecycle
            && !matches!(lifecycle, "accepted" | "terminal")
        {
            bail!("unsupported Run lifecycle filter: {lifecycle}");
        }
        let connection = self.connect()?;
        let mut statement = connection
            .prepare(
                r"
                SELECT lifecycle, accepted_record_json, terminal_record_json
                FROM runs
                WHERE (?1 IS NULL OR lifecycle = ?1)
                  AND (?2 IS NULL OR run_id < ?2)
                ORDER BY run_id DESC
                LIMIT ?3
                ",
            )
            .context("failed to prepare Run list query")?;
        let fetch_limit = limit.checked_add(1).context("Run list limit overflow")?;
        let rows = statement
            .query_map(
                params![
                    lifecycle,
                    after.map(|run_id| run_id.to_string()),
                    i64::try_from(fetch_limit).context("Run list limit is too large")?
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, Option<Vec<u8>>>(2)?,
                    ))
                },
            )
            .context("failed to query Runs")?;
        let mut records = rows
            .map(|row| decode_record(row.context("failed to read Run list row")?))
            .collect::<Result<Vec<_>>>()?;
        let has_more = records.len() > limit;
        if has_more {
            records.truncate(limit);
        }
        Ok(RunRecordPage { records, has_more })
    }

    pub(crate) fn find(&self, run_id: RunId) -> Result<Option<RunRecord>> {
        let connection = self.connect()?;
        let Some(row) = connection
            .query_row(
                "SELECT lifecycle, accepted_record_json, terminal_record_json FROM runs WHERE run_id = ?1",
                [run_id.to_string()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, Option<Vec<u8>>>(2)?,
                    ))
                },
            )
            .optional()
            .context("failed to read Run")?
        else {
            return Ok(None);
        };
        decode_record(row)
            .map(Some)
            .with_context(|| format!("failed to decode Run {run_id}"))
    }

    pub fn bytes(&self, run_id: RunId, field: RunBytesField) -> Result<Vec<u8>> {
        let connection = self.connect()?;
        let sql = format!("SELECT {} FROM runs WHERE run_id = ?1", field.column());
        let bytes = connection
            .query_row(&sql, [run_id.to_string()], |row| {
                row.get::<_, Option<Vec<u8>>>(0)
            })
            .optional()
            .context("failed to read Run bytes")?
            .with_context(|| format!("Run is unknown: {run_id}"))?
            .with_context(|| format!("Run field is unavailable: {run_id} {}", field.column()))?;
        let record = self.get(run_id)?;
        let expected = match (&record, field) {
            (RunRecord::Terminal(record), RunBytesField::Stdout) => &record.stdout,
            (RunRecord::Terminal(record), RunBytesField::Stderr) => &record.stderr,
            (RunRecord::Terminal(record), RunBytesField::ManagedServiceStdout) => {
                &record
                    .managed_service
                    .as_ref()
                    .with_context(|| format!("Run has no Managed Service: {run_id}"))?
                    .stdout
            }
            (RunRecord::Terminal(record), RunBytesField::ManagedServiceStderr) => {
                &record
                    .managed_service
                    .as_ref()
                    .with_context(|| format!("Run has no Managed Service: {run_id}"))?
                    .stderr
            }
            (
                RunRecord::Accepted(_),
                RunBytesField::Stdout
                | RunBytesField::Stderr
                | RunBytesField::ManagedServiceStdout
                | RunBytesField::ManagedServiceStderr,
            ) => {
                bail!("Run field is unavailable: {run_id} {}", field.column())
            }
        };
        verify_stored_bytes(expected, &bytes, field.column())?;
        Ok(bytes)
    }

    fn query_retention_snapshot(&self, run_id: Option<RunId>) -> Result<RunRetentionSnapshot> {
        let mut connection = self.connect()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Deferred)
            .context("failed to start Run verification transaction")?;
        let runs = query_verified_runs(&transaction, run_id)?;
        transaction
            .commit()
            .context("failed to finish Run verification transaction")?;
        summarize_retention(runs)
    }

    fn connect(&self) -> Result<Connection> {
        let connection = if self.read_only {
            Connection::open_with_flags(&self.path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        } else {
            Connection::open(&self.path)
        }
        .with_context(|| format!("failed to open Run database {}", self.path.display()))?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .context("failed to set SQLite busy timeout")?;
        if self.read_only {
            connection
                .execute_batch("PRAGMA query_only=ON; PRAGMA foreign_keys=ON;")
                .context("failed to configure read-only Run database")?;
        } else {
            connection
                .execute_batch(
                    "PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;",
                )
                .context("failed to configure Run database")?;
        }
        Ok(connection)
    }
}

fn verify_read_only_database_format(path: &Path) -> Result<()> {
    let mut header = [0_u8; 20];
    File::open(path)
        .with_context(|| format!("failed to open Run database header {}", path.display()))?
        .read_exact(&mut header)
        .with_context(|| format!("failed to read Run database header {}", path.display()))?;
    if &header[..16] != b"SQLite format 3\0" {
        bail!(
            "Run database has an invalid SQLite header: {}",
            path.display()
        );
    }
    if header[18] == 2 || header[19] == 2 {
        bail!(
            "read-only Run database inspection does not support WAL mode: {}",
            path.display()
        );
    }
    if header[18] != 1 || header[19] != 1 {
        bail!(
            "Run database has unsupported SQLite payload versions: {}",
            path.display()
        );
    }
    Ok(())
}

const RETENTION_SELECT: &str = r"
SELECT
    run_id,
    lifecycle,
    accepted_at,
    terminal_at,
    initial_manifest_digest,
    final_manifest_digest,
    service_initial_manifest_digest,
    service_final_manifest_digest,
    accepted_record_json,
    terminal_record_json,
    runtime_config,
    service_runtime_config,
    stdin,
    stdout,
    stderr,
    service_stdout,
    service_stderr,
    length(accepted_record_json),
    length(terminal_record_json),
    length(runtime_config),
    length(service_runtime_config),
    length(stdin),
    length(stdout),
    length(stderr),
    length(service_stdout),
    length(service_stderr)
FROM runs
";

struct StoredRunRow {
    run_id: String,
    lifecycle: String,
    accepted_at: String,
    terminal_at: Option<String>,
    initial_manifest_digest: String,
    final_manifest_digest: Option<String>,
    service_initial_manifest_digest: Option<String>,
    service_final_manifest_digest: Option<String>,
    accepted_record_json: Vec<u8>,
    terminal_record_json: Option<Vec<u8>>,
    runtime_config: Vec<u8>,
    service_runtime_config: Option<Vec<u8>>,
    stdin: Vec<u8>,
    stdout: Option<Vec<u8>>,
    stderr: Option<Vec<u8>>,
    service_stdout: Option<Vec<u8>>,
    service_stderr: Option<Vec<u8>>,
}

impl StoredRunRow {
    fn read(row: &Row<'_>) -> Result<Self> {
        validate_required_blob_length(row, 17, MAX_RECORD_JSON_BYTES, "accepted_record_json")?;
        validate_optional_blob_length(row, 18, MAX_RECORD_JSON_BYTES, "terminal_record_json")?;
        validate_required_blob_length(row, 19, MAX_STORED_CONTENT_BYTES, "runtime_config")?;
        validate_optional_blob_length(row, 20, MAX_STORED_CONTENT_BYTES, "service_runtime_config")?;
        validate_required_blob_length(row, 21, MAX_STORED_CONTENT_BYTES, "stdin")?;
        for (index, name) in [
            (22, "stdout"),
            (23, "stderr"),
            (24, "service_stdout"),
            (25, "service_stderr"),
        ] {
            validate_optional_blob_length(row, index, MAX_CAPTURED_STREAM_BYTES, name)?;
        }
        Ok(Self {
            run_id: row.get(0)?,
            lifecycle: row.get(1)?,
            accepted_at: row.get(2)?,
            terminal_at: row.get(3)?,
            initial_manifest_digest: row.get(4)?,
            final_manifest_digest: row.get(5)?,
            service_initial_manifest_digest: row.get(6)?,
            service_final_manifest_digest: row.get(7)?,
            accepted_record_json: row.get(8)?,
            terminal_record_json: row.get(9)?,
            runtime_config: row.get(10)?,
            service_runtime_config: row.get(11)?,
            stdin: row.get(12)?,
            stdout: row.get(13)?,
            stderr: row.get(14)?,
            service_stdout: row.get(15)?,
            service_stderr: row.get(16)?,
        })
    }
}

fn validate_required_blob_length(
    row: &Row<'_>,
    index: usize,
    limit: u64,
    name: &str,
) -> Result<()> {
    let length = row
        .get::<_, Option<i64>>(index)
        .with_context(|| format!("failed to inspect stored {name} length"))?
        .with_context(|| format!("stored {name} is NULL"))?;
    validate_blob_length(length, limit, name)
}

fn validate_optional_blob_length(
    row: &Row<'_>,
    index: usize,
    limit: u64,
    name: &str,
) -> Result<()> {
    if let Some(length) = row
        .get::<_, Option<i64>>(index)
        .with_context(|| format!("failed to inspect stored {name} length"))?
    {
        validate_blob_length(length, limit, name)?;
    }
    Ok(())
}

fn validate_blob_length(length: i64, limit: u64, name: &str) -> Result<()> {
    let length =
        u64::try_from(length).with_context(|| format!("stored {name} has invalid length"))?;
    if length > limit {
        bail!("stored {name} exceeds the {limit}-byte verification limit");
    }
    Ok(())
}

fn ensure_content_limit(bytes: &[u8], limit: u64, name: &str) -> Result<()> {
    let length = u64::try_from(bytes.len()).with_context(|| format!("{name} size overflow"))?;
    if length > limit {
        bail!("{name} exceeds the {limit}-byte storage limit");
    }
    Ok(())
}

fn ensure_optional_content_limit(bytes: Option<&[u8]>, limit: u64, name: &str) -> Result<()> {
    if let Some(bytes) = bytes {
        ensure_content_limit(bytes, limit, name)?;
    }
    Ok(())
}

fn ensure_run_capacity(transaction: &Transaction<'_>, limit: usize) -> Result<()> {
    let count = transaction
        .query_row("SELECT count(*) FROM runs", [], |row| row.get::<_, i64>(0))
        .context("failed to count Runs before acceptance")?;
    let count = usize::try_from(count).context("stored Run count is invalid")?;
    if count >= limit {
        bail!("Run database reached the {limit}-Run storage limit");
    }
    Ok(())
}

#[derive(Default)]
struct VerifiedBytes {
    count: usize,
    size: u64,
}

impl VerifiedBytes {
    fn required(&mut self, reference: &StoredBytes, bytes: &[u8], name: &str) -> Result<()> {
        verify_stored_bytes(reference, bytes, name)?;
        self.record(bytes)
    }

    fn optional(
        &mut self,
        reference: &StoredBytes,
        bytes: Option<&[u8]>,
        name: &str,
    ) -> Result<()> {
        verify_optional_stored_bytes(reference, bytes, name)?;
        if let Some(bytes) = bytes {
            self.record(bytes)?;
        }
        Ok(())
    }

    fn record(&mut self, bytes: &[u8]) -> Result<()> {
        self.count = self
            .count
            .checked_add(1)
            .context("stored bytes count overflow")?;
        self.size = self
            .size
            .checked_add(u64::try_from(bytes.len()).context("stored bytes size overflow")?)
            .context("stored bytes total size overflow")?;
        Ok(())
    }
}

fn query_verified_runs(
    transaction: &Transaction<'_>,
    run_id: Option<RunId>,
) -> Result<Vec<VerifiedRunRetention>> {
    if run_id.is_none() {
        let run_count = transaction
            .query_row("SELECT count(*) FROM runs", [], |row| row.get::<_, i64>(0))
            .context("failed to count Runs for verification")?;
        let run_count = usize::try_from(run_count).context("stored Run count is invalid")?;
        if run_count > MAX_RETENTION_RUNS {
            bail!("Run database exceeds the {MAX_RETENTION_RUNS}-Run verification limit");
        }
    }
    let sql = if run_id.is_some() {
        format!("{RETENTION_SELECT} WHERE run_id = ?1")
    } else {
        format!("{RETENTION_SELECT} ORDER BY run_id")
    };
    let mut statement = transaction
        .prepare(&sql)
        .context("failed to prepare Run verification query")?;
    let run_id = run_id.map(|run_id| run_id.to_string());
    let mut rows = match run_id.as_ref() {
        Some(run_id) => statement.query([run_id]),
        None => statement.query([]),
    }
    .context("failed to query Runs for verification")?;
    let mut verified = Vec::new();
    while let Some(row) = rows.next().context("failed to read Run verification row")? {
        if verified.len() >= MAX_RETENTION_RUNS {
            bail!("Run database exceeds the {MAX_RETENTION_RUNS}-Run verification limit");
        }
        let row = StoredRunRow::read(row).context("failed to decode Run verification row")?;
        let run_id = row.run_id.clone();
        verified.push(
            verify_retained_run(&row)
                .with_context(|| format!("failed to verify stored Run {run_id}"))?,
        );
    }
    Ok(verified)
}

fn verify_retained_run(row: &StoredRunRow) -> Result<VerifiedRunRetention> {
    let accepted: AcceptedRunRecord = serde_json::from_slice(&row.accepted_record_json)
        .context("stored accepted Run Record is invalid")?;
    accepted.validate()?;
    verify_common_projections(row, &accepted)?;

    let mut verified_bytes = VerifiedBytes::default();
    verified_bytes.required(
        &accepted.runtime_config,
        &row.runtime_config,
        "Runtime Config",
    )?;
    verified_bytes.required(&accepted.controls.stdin, &row.stdin, "stdin")?;
    verify_retained_service_runtime(&accepted, row, &mut verified_bytes)?;

    let mut image_roots = initial_image_roots(&accepted);
    let lifecycle = match (row.lifecycle.as_str(), row.terminal_record_json.as_deref()) {
        ("accepted", None) => {
            verify_accepted_only_columns(row)?;
            StoredRunLifecycle::Accepted
        }
        ("terminal", Some(terminal_json)) => {
            let terminal: TerminalRunRecord = serde_json::from_slice(terminal_json)
                .context("stored terminal Run Record is invalid")?;
            terminal.validate()?;
            verify_terminal_identity(&accepted, &terminal)?;
            verify_terminal_projections(row, &terminal)?;
            verify_terminal_bytes(row, &terminal, &mut verified_bytes)?;
            append_final_image_roots(&mut image_roots, &terminal);
            StoredRunLifecycle::Terminal
        }
        (lifecycle, _) => {
            bail!("stored Run lifecycle and record are inconsistent: {lifecycle}")
        }
    };

    Ok(VerifiedRunRetention {
        run_id: accepted.run_id,
        lifecycle,
        image_roots,
        verified_stored_bytes_count: verified_bytes.count,
        verified_stored_bytes_size: verified_bytes.size,
    })
}

fn verify_common_projections(row: &StoredRunRow, accepted: &AcceptedRunRecord) -> Result<()> {
    if row.run_id != accepted.run_id.to_string() {
        bail!("run_id projection does not match accepted Run Record");
    }
    if row.accepted_at != accepted.accepted_at.to_rfc3339() {
        bail!("accepted_at projection does not match accepted Run Record");
    }
    verify_digest_projection(
        Some(&row.initial_manifest_digest),
        Some(&accepted.initial_image.digest),
        "initial_manifest_digest",
    )?;
    verify_digest_projection(
        row.service_initial_manifest_digest.as_deref(),
        accepted
            .managed_service
            .as_ref()
            .map(|service| &service.initial_image.digest),
        "service_initial_manifest_digest",
    )
}

fn verify_accepted_only_columns(row: &StoredRunRow) -> Result<()> {
    if row.terminal_at.is_some()
        || row.final_manifest_digest.is_some()
        || row.service_final_manifest_digest.is_some()
        || row.stdout.is_some()
        || row.stderr.is_some()
        || row.service_stdout.is_some()
        || row.service_stderr.is_some()
    {
        bail!("accepted Run contains terminal projections or bytes");
    }
    Ok(())
}

fn verify_terminal_projections(row: &StoredRunRow, terminal: &TerminalRunRecord) -> Result<()> {
    if row.run_id != terminal.run_id.to_string() {
        bail!("run_id projection does not match terminal Run Record");
    }
    if row.terminal_at.as_deref() != Some(terminal.terminal_at.to_rfc3339().as_str()) {
        bail!("terminal_at projection does not match terminal Run Record");
    }
    verify_digest_projection(
        row.final_manifest_digest.as_deref(),
        available_manifest(&terminal.final_image).map(|manifest| &manifest.digest),
        "final_manifest_digest",
    )?;
    verify_digest_projection(
        row.service_final_manifest_digest.as_deref(),
        terminal
            .managed_service
            .as_ref()
            .and_then(|service| available_manifest(&service.final_image))
            .map(|manifest| &manifest.digest),
        "service_final_manifest_digest",
    )
}

fn verify_digest_projection(
    actual: Option<&str>,
    expected: Option<&crate::core::Digest>,
    name: &str,
) -> Result<()> {
    if actual != expected.map(ToString::to_string).as_deref() {
        bail!("{name} projection does not match stored Run Record");
    }
    Ok(())
}

fn verify_retained_service_runtime(
    accepted: &AcceptedRunRecord,
    row: &StoredRunRow,
    verified_bytes: &mut VerifiedBytes,
) -> Result<()> {
    match (
        accepted.managed_service.as_ref(),
        row.service_runtime_config.as_deref(),
    ) {
        (Some(service), Some(bytes)) => verified_bytes.required(
            &service.runtime_config,
            bytes,
            "Managed Service Runtime Config",
        ),
        (None, None) => Ok(()),
        (Some(_), None) => bail!("Managed Service Runtime Config bytes are missing"),
        (None, Some(_)) => bail!("Managed Service Runtime Config bytes have no accepted service"),
    }
}

fn verify_terminal_bytes(
    row: &StoredRunRow,
    terminal: &TerminalRunRecord,
    verified_bytes: &mut VerifiedBytes,
) -> Result<()> {
    verified_bytes.optional(&terminal.stdout, row.stdout.as_deref(), "stdout")?;
    verified_bytes.optional(&terminal.stderr, row.stderr.as_deref(), "stderr")?;
    match (
        terminal.managed_service.as_ref(),
        row.service_stdout.as_deref(),
        row.service_stderr.as_deref(),
    ) {
        (Some(service), stdout, stderr) => {
            verified_bytes.optional(&service.stdout, stdout, "Managed Service stdout")?;
            verified_bytes.optional(&service.stderr, stderr, "Managed Service stderr")
        }
        (None, None, None) => Ok(()),
        (None, _, _) => bail!("Managed Service stream bytes have no terminal service"),
    }
}

fn initial_image_roots(accepted: &AcceptedRunRecord) -> Vec<RunImageRoot> {
    let mut roots = vec![RunImageRoot {
        run_id: accepted.run_id,
        participant: RunImageParticipant::Primary,
        slot: RunImageSlot::Initial,
        descriptor: accepted.initial_image.clone(),
    }];
    if let Some(service) = &accepted.managed_service {
        roots.push(RunImageRoot {
            run_id: accepted.run_id,
            participant: RunImageParticipant::ManagedService {
                name: service.name.clone(),
            },
            slot: RunImageSlot::Initial,
            descriptor: service.initial_image.clone(),
        });
    }
    roots
}

fn append_final_image_roots(roots: &mut Vec<RunImageRoot>, terminal: &TerminalRunRecord) {
    if let Some(manifest) = available_manifest(&terminal.final_image) {
        roots.push(RunImageRoot {
            run_id: terminal.run_id,
            participant: RunImageParticipant::Primary,
            slot: RunImageSlot::Final,
            descriptor: manifest.clone(),
        });
    }
    if let Some(service) = &terminal.managed_service
        && let Some(manifest) = available_manifest(&service.final_image)
    {
        roots.push(RunImageRoot {
            run_id: terminal.run_id,
            participant: RunImageParticipant::ManagedService {
                name: service.name.clone(),
            },
            slot: RunImageSlot::Final,
            descriptor: manifest.clone(),
        });
    }
}

fn available_manifest(slot: &ImageSlot) -> Option<&crate::core::OciDescriptor> {
    match slot {
        ImageSlot::Available { manifest } => Some(manifest),
        ImageSlot::Unavailable { .. } | ImageSlot::NotApplicable => None,
    }
}

fn summarize_retention(runs: Vec<VerifiedRunRetention>) -> Result<RunRetentionSnapshot> {
    let accepted_count = runs
        .iter()
        .filter(|run| run.lifecycle == StoredRunLifecycle::Accepted)
        .count();
    let verified_stored_bytes_count = runs.iter().try_fold(0_usize, |total, run| {
        total
            .checked_add(run.verified_stored_bytes_count)
            .context("stored bytes count overflow")
    })?;
    let verified_stored_bytes_size = runs.iter().try_fold(0_u64, |total, run| {
        total
            .checked_add(run.verified_stored_bytes_size)
            .context("stored bytes total size overflow")
    })?;
    Ok(RunRetentionSnapshot {
        runs,
        accepted_count,
        verified_stored_bytes_count,
        verified_stored_bytes_size,
    })
}

fn decode_record(row: (String, Vec<u8>, Option<Vec<u8>>)) -> Result<RunRecord> {
    match row {
        (lifecycle, accepted, None) if lifecycle == "accepted" => {
            let record: AcceptedRunRecord = serde_json::from_slice(&accepted)
                .context("stored accepted Run Record is invalid")?;
            record.validate()?;
            Ok(RunRecord::Accepted(Box::new(record)))
        }
        (lifecycle, _, Some(terminal)) if lifecycle == "terminal" => {
            let record: TerminalRunRecord = serde_json::from_slice(&terminal)
                .context("stored terminal Run Record is invalid")?;
            record.validate()?;
            Ok(RunRecord::Terminal(Box::new(record)))
        }
        (lifecycle, _, _) => bail!("stored Run lifecycle and record are inconsistent: {lifecycle}"),
    }
}

fn verify_storage_version(connection: &Connection) -> Result<()> {
    let storage_version: i64 = connection
        .query_row(
            "SELECT value FROM schema_metadata WHERE key = 'storage_version'",
            [],
            |row| row.get(0),
        )
        .context("failed to read Run database storage version")?;
    if storage_version != STORAGE_VERSION {
        bail!(
            "unsupported Run database storage version: expected {STORAGE_VERSION}, received {storage_version}"
        );
    }
    Ok(())
}

fn verify_terminal_identity(
    accepted: &AcceptedRunRecord,
    terminal: &TerminalRunRecord,
) -> Result<()> {
    if accepted.run_id != terminal.run_id
        || accepted.accepted_at != terminal.accepted_at
        || accepted.requested_image_reference != terminal.requested_image_reference
        || accepted.initial_image != terminal.initial_image
        || accepted.runtime_config != terminal.runtime_config
        || accepted.controls != terminal.controls
        || !managed_service_identity_matches(
            accepted.managed_service.as_ref(),
            terminal.managed_service.as_ref(),
        )
    {
        bail!("terminal Run does not preserve its accepted identity");
    }
    Ok(())
}

fn managed_service_identity_matches(
    accepted: Option<&crate::core::ManagedServiceCondition>,
    terminal: Option<&crate::core::ManagedServiceFacts>,
) -> bool {
    match (accepted, terminal) {
        (None, None) => true,
        (Some(accepted), Some(terminal)) => {
            accepted.name == terminal.name
                && accepted.requested_image_reference == terminal.requested_image_reference
                && accepted.initial_image == terminal.initial_image
                && accepted.runtime_config == terminal.runtime_config
                && accepted.readiness == terminal.readiness_condition
        }
        (None, Some(_)) | (Some(_), None) => false,
    }
}

fn verify_managed_service_runtime(
    service: Option<&crate::core::ManagedServiceCondition>,
    bytes: Option<&[u8]>,
) -> Result<()> {
    match (service, bytes) {
        (Some(service), Some(bytes)) => verify_stored_bytes(
            &service.runtime_config,
            bytes,
            "Managed Service Runtime Config",
        ),
        (None, None) => Ok(()),
        (Some(_), None) => bail!("Managed Service Runtime Config bytes are missing"),
        (None, Some(_)) => bail!("Managed Service Runtime Config bytes have no accepted service"),
    }
}

fn verify_managed_service_streams(
    service: Option<&crate::core::ManagedServiceFacts>,
    stdout: Option<&[u8]>,
    stderr: Option<&[u8]>,
) -> Result<()> {
    let Some(service) = service else {
        if stdout.is_some() || stderr.is_some() {
            bail!("Managed Service stream bytes have no terminal service");
        }
        return Ok(());
    };
    verify_optional_stored_bytes(&service.stdout, stdout, "Managed Service stdout")?;
    verify_optional_stored_bytes(&service.stderr, stderr, "Managed Service stderr")
}

fn verify_optional_stored_bytes(
    reference: &StoredBytes,
    bytes: Option<&[u8]>,
    name: &str,
) -> Result<()> {
    match (reference, bytes) {
        (StoredBytes::Available { .. } | StoredBytes::Partial { .. }, Some(bytes)) => {
            verify_stored_bytes(reference, bytes, name)
        }
        (StoredBytes::Unavailable { .. } | StoredBytes::NotApplicable, None) => Ok(()),
        _ => bail!("{name} availability does not match stored bytes"),
    }
}

fn verify_stored_bytes(reference: &StoredBytes, bytes: &[u8], name: &str) -> Result<()> {
    let Some(expected_digest) = reference.digest() else {
        bail!("{name} is not an available content slot");
    };
    let Some(expected_size) = reference.size() else {
        bail!("{name} lacks a stored size");
    };
    let actual_digest = digest_bytes(bytes);
    let actual_size = u64::try_from(bytes.len()).context("stored bytes size overflow")?;
    if &actual_digest != expected_digest || actual_size != expected_size {
        bail!("{name} bytes do not match their digest and size");
    }
    Ok(())
}

fn secure_file(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to secure Run database {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;
    use crate::core::{
        ACCEPTED_RUN_RECORD_SCHEMA_VERSION, AcceptedLifecycle, Architecture, BackendDetails,
        BackendFacts, Digest, ManagedServiceCondition, ManagedServiceFacts,
        ManagedServiceReadiness, NetworkControl, OciDescriptor, OperationError,
        OperationErrorScope, Platform, ProcessFacts, ProcessOutcome, ProcessSlot, RunControls,
        RunNetworkFacts, RunNetworkRealization, ServiceName, TERMINAL_RUN_RECORD_SCHEMA_VERSION,
        TcpReadinessCondition, TerminalLifecycle,
    };

    #[test]
    fn read_only_database_access_creates_no_wal_sidecars() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("runs.sqlite3");
        let writer = RunDatabase::open(&path).expect("Run database");
        let fixture = ManagedFixture::new();
        fixture.accept(&writer).expect("accepted Run");
        drop(writer);
        let before = fs::read(&path).expect("database bytes");
        assert!(!directory.path().join("runs.sqlite3-wal").exists());
        assert!(!directory.path().join("runs.sqlite3-shm").exists());

        let database = RunDatabase::open_existing(&path).expect("read-only Run database");
        let snapshot = database.retention_snapshot().expect("read-only snapshot");
        assert_eq!(snapshot.accepted_count, 1);
        assert_eq!(snapshot.runs[0].run_id, fixture.accepted.run_id);
        assert_eq!(fs::read(&path).expect("database bytes"), before);
        assert!(!directory.path().join("runs.sqlite3-wal").exists());
        assert!(!directory.path().join("runs.sqlite3-shm").exists());
    }

    #[test]
    fn read_only_database_rejects_wal_without_creating_sidecars() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("runs.sqlite3");
        let connection = Connection::open(&path).expect("SQLite database");
        connection
            .execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE fixture (value INTEGER);")
            .expect("WAL database");
        drop(connection);
        let before = fs::read(&path).expect("database bytes");
        assert!(!directory.path().join("runs.sqlite3-wal").exists());
        assert!(!directory.path().join("runs.sqlite3-shm").exists());

        let error = RunDatabase::open_existing(&path).expect_err("WAL must fail closed");
        assert!(error.to_string().contains("does not support WAL mode"));
        assert_eq!(fs::read(&path).expect("database bytes"), before);
        assert!(!directory.path().join("runs.sqlite3-wal").exists());
        assert!(!directory.path().join("runs.sqlite3-shm").exists());
    }

    #[test]
    fn terminal_run_is_immutable_and_bytes_are_verified() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let database =
            RunDatabase::open(directory.path().join("runs.sqlite3")).expect("Run database");
        let run_id = RunId::new();
        let manifest = OciDescriptor {
            digest: Digest::parse(format!("sha256:{}", "1".repeat(64))).expect("digest"),
            size: 123,
            media_type: "application/vnd.oci.image.manifest.v1+json".to_owned(),
        };
        let config = b"{\"ociVersion\":\"1.2.0\"}\n";
        let stdin = b"";
        let config_slot = StoredBytes::Available {
            digest: digest_bytes(config),
            size: u64::try_from(config.len()).expect("size"),
        };
        let controls = RunControls {
            stdin: StoredBytes::Available {
                digest: digest_bytes(stdin),
                size: 0,
            },
            timeout_seconds: 60,
            stdout_limit_bytes: 1024,
            stderr_limit_bytes: 1024,
            network: NetworkControl::None,
        };
        let accepted_at = Utc::now();
        let accepted = AcceptedRunRecord {
            schema_version: ACCEPTED_RUN_RECORD_SCHEMA_VERSION,
            run_id,
            lifecycle: AcceptedLifecycle::Accepted,
            accepted_at,
            requested_image_reference: Some("runlab/primary:latest".to_owned()),
            initial_image: manifest.clone(),
            runtime_config: config_slot.clone(),
            controls: controls.clone(),
            managed_service: None,
        };
        assert_outdated_accepted_rejected(&database, &accepted, config, stdin);
        database.accept(&accepted, config, stdin).expect("accept");
        let stdout = b"hello\n";
        let stderr = b"";
        let terminal = TerminalRunRecord {
            schema_version: TERMINAL_RUN_RECORD_SCHEMA_VERSION,
            run_id,
            lifecycle: TerminalLifecycle::Terminal,
            accepted_at,
            terminal_at: Utc::now(),
            requested_image_reference: accepted.requested_image_reference.clone(),
            initial_image: manifest.clone(),
            runtime_config: config_slot,
            controls,
            backend: Some(docker_backend()),
            process: ProcessSlot::available(ProcessFacts {
                terminal_outcome: ProcessOutcome::ProcessExited,
                exit_code: Some(0),
                started_at: Some(accepted_at),
                ended_at: Some(accepted_at),
                oom_killed: Some(false),
                backend_error: None,
            }),
            stdout: StoredBytes::Available {
                digest: digest_bytes(stdout),
                size: u64::try_from(stdout.len()).expect("size"),
            },
            stderr: StoredBytes::Available {
                digest: digest_bytes(stderr),
                size: 0,
            },
            final_image: ImageSlot::Available { manifest },
            operation_errors: vec![OperationError {
                scope: OperationErrorScope::Primary,
                phase: "capture_cleanup".to_owned(),
                message: "injected cleanup failure".to_owned(),
            }],
            managed_service: None,
        };
        assert_outdated_terminal_rejected(&database, &terminal, stdout, stderr);
        database
            .terminal(&terminal, Some(stdout), Some(stderr))
            .expect("terminalize");
        assert!(
            database
                .terminal(&terminal, Some(stdout), Some(stderr))
                .is_err()
        );
        assert_eq!(
            database
                .bytes(run_id, RunBytesField::Stdout)
                .expect("stdout"),
            stdout
        );
        let stored = database.get(run_id).expect("stored terminal Run");
        let RunRecord::Terminal(stored) = stored else {
            panic!("expected terminal Run");
        };
        assert_terminal_assets(&stored, &terminal);
    }

    fn assert_outdated_terminal_rejected(
        database: &RunDatabase,
        terminal: &TerminalRunRecord,
        stdout: &[u8],
        stderr: &[u8],
    ) {
        let mut outdated = terminal.clone();
        outdated.schema_version = TERMINAL_RUN_RECORD_SCHEMA_VERSION - 1;
        let error = database
            .terminal(&outdated, Some(stdout), Some(stderr))
            .expect_err("outdated terminal schema must fail");
        assert!(
            error
                .to_string()
                .contains("unsupported terminal Run Record schema version")
        );
    }

    fn assert_outdated_accepted_rejected(
        database: &RunDatabase,
        accepted: &AcceptedRunRecord,
        runtime_config: &[u8],
        stdin: &[u8],
    ) {
        let mut outdated = accepted.clone();
        outdated.schema_version = ACCEPTED_RUN_RECORD_SCHEMA_VERSION - 1;
        let error = database
            .accept(&outdated, runtime_config, stdin)
            .expect_err("outdated accepted schema must fail");
        assert!(
            error
                .to_string()
                .contains("unsupported accepted Run Record schema version")
        );
    }

    fn assert_terminal_assets(stored: &TerminalRunRecord, expected: &TerminalRunRecord) {
        assert_eq!(stored.final_image, expected.final_image);
        assert_eq!(stored.operation_errors, expected.operation_errors);
    }

    #[test]
    fn managed_service_round_trips_all_content_and_retention_digests() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let database =
            RunDatabase::open(directory.path().join("runs.sqlite3")).expect("Run database");
        let fixture = ManagedFixture::new();
        fixture.accept(&database).expect("accept service Run");
        fixture
            .terminal(&database)
            .expect("terminalize service Run");

        let stored = database.get(fixture.accepted.run_id).expect("stored Run");
        let RunRecord::Terminal(stored) = stored else {
            panic!("expected terminal Run");
        };
        assert_eq!(*stored, fixture.terminal);
        assert_eq!(
            database
                .bytes(fixture.accepted.run_id, RunBytesField::ManagedServiceStdout)
                .expect("service stdout"),
            fixture.service_stdout
        );
        assert_eq!(
            database
                .bytes(fixture.accepted.run_id, RunBytesField::ManagedServiceStderr)
                .expect("service stderr"),
            fixture.service_stderr
        );

        let connection = Connection::open(database.path()).expect("connection");
        let digests = connection
            .query_row(
                "SELECT service_initial_manifest_digest, service_final_manifest_digest, service_runtime_config FROM runs WHERE run_id = ?1",
                [fixture.accepted.run_id.to_string()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, Vec<u8>>(2)?)),
            )
            .expect("service retention columns");
        let service = fixture.accepted.managed_service.as_ref().expect("service");
        let terminal_service = fixture
            .terminal
            .managed_service
            .as_ref()
            .expect("service facts");
        let ImageSlot::Available { manifest } = &terminal_service.final_image else {
            panic!("expected service Final Image");
        };
        assert_eq!(digests.0, service.initial_image.digest.to_string());
        assert_eq!(digests.1, manifest.digest.to_string());
        assert_eq!(digests.2, fixture.service_config);
        assert!(fixture.terminal(&database).is_err());
    }

    #[test]
    fn managed_service_retention_snapshot_has_verified_roots_and_bytes() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let database =
            RunDatabase::open(directory.path().join("runs.sqlite3")).expect("Run database");
        let fixture = ManagedFixture::new();
        fixture.accept(&database).expect("accept service Run");

        let accepted = database
            .retention_snapshot()
            .expect("accepted retention snapshot");
        assert_eq!(accepted.accepted_count, 1);
        assert_eq!(accepted.verified_stored_bytes_count, 3);
        assert_eq!(accepted.runs.len(), 1);
        assert_eq!(accepted.runs[0].lifecycle, StoredRunLifecycle::Accepted);
        assert_eq!(accepted.runs[0].image_roots.len(), 2);

        fixture
            .terminal(&database)
            .expect("terminalize service Run");
        let snapshot = database
            .retention_snapshot()
            .expect("terminal retention snapshot");
        let run = database
            .retention_snapshot_for(fixture.accepted.run_id)
            .expect("single Run retention snapshot");
        assert_eq!(snapshot.accepted_count, 0);
        assert_eq!(snapshot.runs, vec![run.clone()]);
        assert_eq!(run.lifecycle, StoredRunLifecycle::Terminal);
        assert_eq!(run.verified_stored_bytes_count, 7);
        let expected_size: u64 = [
            &fixture.primary_config,
            &fixture.service_config,
            &fixture.stdin,
            &fixture.stdout,
            &fixture.stderr,
            &fixture.service_stdout,
            &fixture.service_stderr,
        ]
        .iter()
        .map(|bytes| u64::try_from(bytes.len()).expect("size"))
        .sum();
        assert_eq!(run.verified_stored_bytes_size, expected_size);
        assert_eq!(snapshot.verified_stored_bytes_count, 7);
        assert_eq!(snapshot.verified_stored_bytes_size, expected_size);

        let service = fixture.accepted.managed_service.as_ref().expect("service");
        let terminal_service = fixture
            .terminal
            .managed_service
            .as_ref()
            .expect("terminal service");
        let ImageSlot::Available {
            manifest: service_final,
        } = &terminal_service.final_image
        else {
            panic!("service Final Image must be available");
        };
        assert_eq!(
            run.image_roots,
            vec![
                RunImageRoot {
                    run_id: fixture.accepted.run_id,
                    participant: RunImageParticipant::Primary,
                    slot: RunImageSlot::Initial,
                    descriptor: fixture.accepted.initial_image.clone(),
                },
                RunImageRoot {
                    run_id: fixture.accepted.run_id,
                    participant: RunImageParticipant::ManagedService {
                        name: service.name.clone(),
                    },
                    slot: RunImageSlot::Initial,
                    descriptor: service.initial_image.clone(),
                },
                RunImageRoot {
                    run_id: fixture.accepted.run_id,
                    participant: RunImageParticipant::ManagedService {
                        name: service.name.clone(),
                    },
                    slot: RunImageSlot::Final,
                    descriptor: service_final.clone(),
                },
            ]
        );
    }

    #[test]
    fn retention_snapshot_rejects_digest_projection_mismatch() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let database =
            RunDatabase::open(directory.path().join("runs.sqlite3")).expect("Run database");
        let fixture = ManagedFixture::new();
        fixture.accept(&database).expect("accept service Run");
        Connection::open(database.path())
            .expect("connection")
            .execute(
                "UPDATE runs SET initial_manifest_digest = ?1 WHERE run_id = ?2",
                params![
                    format!("sha256:{}", "f".repeat(64)),
                    fixture.accepted.run_id.to_string()
                ],
            )
            .expect("corrupt digest projection");

        let error = database
            .retention_snapshot_for(fixture.accepted.run_id)
            .expect_err("projection mismatch must fail closed");
        assert!(
            format!("{error:#}")
                .contains("initial_manifest_digest projection does not match stored Run Record")
        );
    }

    #[test]
    fn retention_snapshot_rejects_an_oversized_blob_before_decoding_it() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let database =
            RunDatabase::open(directory.path().join("runs.sqlite3")).expect("Run database");
        let fixture = ManagedFixture::new();
        fixture.accept(&database).expect("accept service Run");
        Connection::open(database.path())
            .expect("connection")
            .execute(
                "UPDATE runs SET runtime_config = zeroblob(?1) WHERE run_id = ?2",
                params![
                    i64::try_from(MAX_STORED_CONTENT_BYTES + 1).expect("limit"),
                    fixture.accepted.run_id.to_string()
                ],
            )
            .expect("oversized stored Runtime Config");

        let error = database
            .retention_snapshot_for(fixture.accepted.run_id)
            .expect_err("oversized content must fail closed");
        assert!(
            format!("{error:#}")
                .contains("stored runtime_config exceeds the 16777216-byte verification limit")
        );
    }

    #[test]
    fn run_capacity_is_checked_inside_the_acceptance_transaction() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let database =
            RunDatabase::open(directory.path().join("runs.sqlite3")).expect("Run database");
        let fixture = ManagedFixture::new();
        fixture.accept(&database).expect("accept service Run");
        let mut connection = database.connect().expect("connection");
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .expect("acceptance transaction");

        let error = ensure_run_capacity(&transaction, 1).expect_err("full database must reject");
        assert_eq!(
            error.to_string(),
            "Run database reached the 1-Run storage limit"
        );
    }

    #[test]
    fn retention_snapshot_rejects_terminal_identity_mismatch() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let database =
            RunDatabase::open(directory.path().join("runs.sqlite3")).expect("Run database");
        let fixture = ManagedFixture::new();
        fixture.accept(&database).expect("accept service Run");
        fixture
            .terminal(&database)
            .expect("terminalize service Run");
        let mut terminal = fixture.terminal.clone();
        terminal.requested_image_reference = Some("runlab/tampered:latest".to_owned());
        let connection = Connection::open(database.path()).expect("connection");
        connection
            .execute_batch("DROP TRIGGER terminal_runs_are_immutable")
            .expect("remove fixture immutability trigger");
        connection
            .execute(
                "UPDATE runs SET terminal_record_json = ?1 WHERE run_id = ?2",
                params![
                    canonical_json(&terminal).expect("terminal JSON"),
                    fixture.accepted.run_id.to_string()
                ],
            )
            .expect("corrupt terminal identity");

        let error = database
            .retention_snapshot_for(fixture.accepted.run_id)
            .expect_err("terminal identity mismatch must fail closed");
        assert!(
            format!("{error:#}").contains("terminal Run does not preserve its accepted identity")
        );
    }

    #[test]
    fn retention_snapshot_rejects_stored_byte_mismatch() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let database =
            RunDatabase::open(directory.path().join("runs.sqlite3")).expect("Run database");
        let fixture = ManagedFixture::new();
        fixture.accept(&database).expect("accept service Run");
        Connection::open(database.path())
            .expect("connection")
            .execute(
                "UPDATE runs SET service_runtime_config = ?1 WHERE run_id = ?2",
                params![b"tampered", fixture.accepted.run_id.to_string()],
            )
            .expect("corrupt stored bytes");

        let error = database
            .retention_snapshot_for(fixture.accepted.run_id)
            .expect_err("stored byte mismatch must fail closed");
        assert!(
            format!("{error:#}").contains(
                "Managed Service Runtime Config bytes do not match their digest and size"
            )
        );
    }

    #[test]
    fn managed_service_identity_mismatch_leaves_run_accepted() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let database =
            RunDatabase::open(directory.path().join("runs.sqlite3")).expect("Run database");
        let fixture = ManagedFixture::new();
        fixture.accept(&database).expect("accept service Run");
        let mut terminal = fixture.terminal.clone();
        terminal
            .managed_service
            .as_mut()
            .expect("service")
            .readiness_condition
            .port = 5433;
        let error = database
            .terminal_with_managed_service(
                &terminal,
                Some(&fixture.stdout),
                Some(&fixture.stderr),
                Some(&fixture.service_stdout),
                Some(&fixture.service_stderr),
            )
            .expect_err("identity mismatch must fail");
        assert!(
            error
                .to_string()
                .contains("does not preserve its accepted identity")
        );
        assert!(matches!(
            database.get(fixture.accepted.run_id).expect("accepted Run"),
            RunRecord::Accepted(_)
        ));
    }

    #[test]
    fn managed_service_requires_durable_run_network_identity_after_process_start() {
        let fixture = ManagedFixture::new();
        let mut terminal = fixture.terminal;
        terminal.backend = None;
        let error = terminal
            .validate()
            .expect_err("network identity must be present");
        assert!(
            error
                .to_string()
                .contains("require Run network facts unless both processes were not started")
        );
    }

    #[test]
    fn managed_network_setup_failure_can_terminalize_before_process_start() {
        let fixture = ManagedFixture::new();
        let mut terminal = fixture.terminal;
        terminal.backend.as_mut().expect("backend").run_network = None;
        terminal.process = ProcessSlot::available(ProcessFacts::not_started());
        terminal.managed_service.as_mut().expect("service").process =
            ProcessSlot::available(ProcessFacts::not_started());
        terminal.validate().expect("pre-start network failure");
    }

    #[test]
    fn managed_service_byte_mismatch_never_publishes_a_terminal_record() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let database =
            RunDatabase::open(directory.path().join("runs.sqlite3")).expect("Run database");
        let fixture = ManagedFixture::new();
        assert!(
            database
                .accept_with_managed_service(
                    &fixture.accepted,
                    &fixture.primary_config,
                    &fixture.stdin,
                    Some(b"wrong service config"),
                )
                .is_err()
        );
        assert!(database.get(fixture.accepted.run_id).is_err());

        fixture.accept(&database).expect("accept service Run");
        assert!(
            database
                .terminal_with_managed_service(
                    &fixture.terminal,
                    Some(&fixture.stdout),
                    Some(&fixture.stderr),
                    Some(b"wrong service stdout"),
                    Some(&fixture.service_stderr),
                )
                .is_err()
        );
        assert!(matches!(
            database.get(fixture.accepted.run_id).expect("accepted Run"),
            RunRecord::Accepted(_)
        ));
    }

    #[test]
    fn rejects_an_unsupported_storage_version() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("runs.sqlite3");
        let connection = Connection::open(&path).expect("connection");
        connection
            .execute_batch(
                "CREATE TABLE schema_metadata (key TEXT PRIMARY KEY, value INTEGER NOT NULL);",
            )
            .expect("future schema");
        connection
            .execute(
                "INSERT INTO schema_metadata(key, value) VALUES ('storage_version', ?1)",
                [STORAGE_VERSION + 1],
            )
            .expect("future storage version");
        drop(connection);
        let error = RunDatabase::open(&path).expect_err("unsupported version");
        assert!(
            error
                .to_string()
                .contains("unsupported Run database storage version")
        );
        let connection = Connection::open(&path).expect("connection");
        let runs_table: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'runs')",
                [],
                |row| row.get(0),
            )
            .expect("inspect schema");
        assert!(!runs_table);
    }

    struct ManagedFixture {
        accepted: AcceptedRunRecord,
        terminal: TerminalRunRecord,
        primary_config: Vec<u8>,
        service_config: Vec<u8>,
        stdin: Vec<u8>,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
        service_stdout: Vec<u8>,
        service_stderr: Vec<u8>,
    }

    impl ManagedFixture {
        #[allow(
            clippy::too_many_lines,
            reason = "the fixture keeps one complete accepted-to-terminal record visible"
        )]
        fn new() -> Self {
            let primary_config = b"primary config".to_vec();
            let service_config = b"service config".to_vec();
            let stdin = b"input".to_vec();
            let stdout = b"primary stdout".to_vec();
            let stderr = b"primary stderr".to_vec();
            let service_stdout = b"service stdout".to_vec();
            let service_stderr = b"service stderr".to_vec();
            let condition = ManagedServiceCondition {
                name: ServiceName::parse("postgres").expect("service name"),
                requested_image_reference: Some("runlab/postgres:latest".to_owned()),
                initial_image: descriptor('2'),
                runtime_config: stored(&service_config),
                readiness: TcpReadinessCondition {
                    port: 5432,
                    timeout_seconds: 30,
                },
            };
            let accepted_at = Utc::now();
            let accepted = AcceptedRunRecord {
                schema_version: ACCEPTED_RUN_RECORD_SCHEMA_VERSION,
                run_id: RunId::new(),
                lifecycle: AcceptedLifecycle::Accepted,
                accepted_at,
                requested_image_reference: Some("runlab/primary:latest".to_owned()),
                initial_image: descriptor('1'),
                runtime_config: stored(&primary_config),
                controls: RunControls {
                    stdin: stored(&stdin),
                    timeout_seconds: 60,
                    stdout_limit_bytes: 1024,
                    stderr_limit_bytes: 1024,
                    network: NetworkControl::None,
                },
                managed_service: Some(condition.clone()),
            };
            let terminal = TerminalRunRecord {
                schema_version: TERMINAL_RUN_RECORD_SCHEMA_VERSION,
                run_id: accepted.run_id,
                lifecycle: TerminalLifecycle::Terminal,
                accepted_at,
                terminal_at: Utc::now(),
                requested_image_reference: accepted.requested_image_reference.clone(),
                initial_image: accepted.initial_image.clone(),
                runtime_config: accepted.runtime_config.clone(),
                controls: accepted.controls.clone(),
                backend: Some(BackendFacts {
                    name: "native_linux".to_owned(),
                    version: "test".to_owned(),
                    platform: Platform::linux(Architecture::Arm64),
                    network: NetworkControl::None,
                    run_network: Some(RunNetworkFacts {
                        namespace_device: 4,
                        namespace_inode: 42,
                        realization: RunNetworkRealization::LoopbackOnly,
                    }),
                    details: BackendDetails::NativeLinux {
                        runtime_name: "runc".to_owned(),
                        runtime_version: "1.3.6".to_owned(),
                        runtime_commit: "fixture".to_owned(),
                        runtime_spec: "1.2.1".to_owned(),
                        runtime_digest: digest_bytes(b"runc fixture"),
                        runtime_size: 12,
                        kernel_release: "fixture".to_owned(),
                        runtime_invocation: crate::core::NativeRuntimeInvocation::Direct,
                        runtime_config: crate::core::NativeRuntimeConfigRealization::Accepted,
                        filesystem: crate::core::NativeFilesystemRealization::OverlayFs {
                            profile: "fixture".to_owned(),
                        },
                    },
                }),
                process: exited_process(),
                stdout: stored(&stdout),
                stderr: stored(&stderr),
                final_image: ImageSlot::Unavailable {
                    error: "primary capture unavailable".to_owned(),
                },
                operation_errors: Vec::new(),
                managed_service: Some(ManagedServiceFacts {
                    name: condition.name,
                    requested_image_reference: condition.requested_image_reference,
                    initial_image: condition.initial_image,
                    runtime_config: condition.runtime_config,
                    readiness_condition: condition.readiness,
                    readiness: ManagedServiceReadiness::Ready {
                        observed_at: Utc::now(),
                        attempts: 2,
                    },
                    process: exited_process(),
                    stdout: stored(&service_stdout),
                    stderr: stored(&service_stderr),
                    final_image: ImageSlot::Available {
                        manifest: descriptor('3'),
                    },
                    operation_errors: vec![OperationError {
                        scope: OperationErrorScope::ManagedService,
                        phase: "cleanup".to_owned(),
                        message: "fixture cleanup warning".to_owned(),
                    }],
                }),
            };
            Self {
                accepted,
                terminal,
                primary_config,
                service_config,
                stdin,
                stdout,
                stderr,
                service_stdout,
                service_stderr,
            }
        }

        fn accept(&self, database: &RunDatabase) -> Result<()> {
            database.accept_with_managed_service(
                &self.accepted,
                &self.primary_config,
                &self.stdin,
                Some(&self.service_config),
            )
        }

        fn terminal(&self, database: &RunDatabase) -> Result<()> {
            database.terminal_with_managed_service(
                &self.terminal,
                Some(&self.stdout),
                Some(&self.stderr),
                Some(&self.service_stdout),
                Some(&self.service_stderr),
            )
        }
    }

    fn descriptor(digit: char) -> OciDescriptor {
        OciDescriptor {
            digest: Digest::parse(format!("sha256:{}", digit.to_string().repeat(64)))
                .expect("digest"),
            size: 123,
            media_type: "application/vnd.oci.image.manifest.v1+json".to_owned(),
        }
    }

    fn stored(bytes: &[u8]) -> StoredBytes {
        StoredBytes::Available {
            digest: digest_bytes(bytes),
            size: u64::try_from(bytes.len()).expect("size"),
        }
    }

    fn exited_process() -> ProcessSlot {
        let observed_at = Utc::now();
        ProcessSlot::available(ProcessFacts {
            terminal_outcome: ProcessOutcome::ProcessExited,
            exit_code: Some(0),
            started_at: Some(observed_at),
            ended_at: Some(observed_at),
            oom_killed: None,
            backend_error: None,
        })
    }

    fn docker_backend() -> BackendFacts {
        BackendFacts {
            name: "docker".to_owned(),
            version: "test".to_owned(),
            platform: Platform::linux(Architecture::Arm64),
            network: NetworkControl::None,
            run_network: None,
            details: BackendDetails::Docker {
                context: "default".to_owned(),
                endpoint_kind: "unix_socket".to_owned(),
                engine_id: "test".to_owned(),
            },
        }
    }
}
