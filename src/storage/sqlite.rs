use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;

use crate::metadata::Metadata;

pub(crate) struct Database {
    connection: Mutex<Connection>,
}

#[derive(Debug)]
pub(crate) struct StoredRun {
    pub(crate) run_id: String,
    pub(crate) accepted_at: String,
    pub(crate) metadata: Metadata,
    pub(crate) input: Value,
    pub(crate) input_identity: Value,
    pub(crate) terminal_at: Option<String>,
    pub(crate) completion: Option<Value>,
}

type RunRow = (
    String,
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
);

impl Database {
    pub(crate) fn open(path: &Path) -> Result<Self> {
        let connection = Connection::open(path)
            .with_context(|| format!("failed to open RunLab database {}", path.display()))?;
        connection.execute_batch(
            r#"PRAGMA journal_mode = WAL;
             PRAGMA foreign_keys = ON;
             CREATE TABLE IF NOT EXISTS catalog (
                 name TEXT PRIMARY KEY,
                 descriptor_json TEXT NOT NULL,
                 metadata_json TEXT NOT NULL DEFAULT '{"description":null,"labels":{}}',
                 updated_at TEXT NOT NULL
             ) STRICT;
             CREATE TABLE IF NOT EXISTS runs (
                 run_id TEXT PRIMARY KEY,
                 accepted_at TEXT NOT NULL,
                 metadata_json TEXT NOT NULL DEFAULT '{"description":null,"labels":{}}',
                 input_json TEXT NOT NULL,
                 input_identity_json TEXT NOT NULL,
                 terminal_at TEXT,
                 completion_json TEXT
             ) STRICT;
             CREATE INDEX IF NOT EXISTS runs_acceptance_order
                 ON runs(accepted_at DESC, run_id DESC);"#,
        )?;
        ensure_metadata_column(&connection, "catalog")?;
        ensure_metadata_column(&connection, "runs")?;
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

    pub(crate) fn run_insert(
        &self,
        run_id: &str,
        accepted_at: &str,
        metadata: &Metadata,
        input: &Value,
        input_identity: &Value,
    ) -> Result<bool> {
        let changed = self.lock()?.execute(
            "INSERT OR IGNORE INTO runs(
                run_id, accepted_at, metadata_json, input_json, input_identity_json
             ) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                run_id,
                accepted_at,
                serde_json::to_string(metadata)?,
                serde_json::to_string(input)?,
                serde_json::to_string(input_identity)?,
            ],
        )?;
        Ok(changed == 1)
    }

    pub(crate) fn run_complete(
        &self,
        run_id: &str,
        terminal_at: &str,
        completion: &Value,
    ) -> Result<()> {
        let changed = self.lock()?.execute(
            "UPDATE runs SET terminal_at = ?2, completion_json = ?3
             WHERE run_id = ?1 AND terminal_at IS NULL",
            params![run_id, terminal_at, serde_json::to_string(completion)?],
        )?;
        if changed != 1 {
            bail!("Run cannot be completed from its current state: {run_id}");
        }
        Ok(())
    }

    pub(crate) fn run_get(&self, run_id: &str) -> Result<Option<StoredRun>> {
        let row = self
            .lock()?
            .query_row(
                "SELECT run_id, accepted_at, metadata_json, input_json, input_identity_json,
                        terminal_at, completion_json
                 FROM runs WHERE run_id = ?1",
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
                        "SELECT accepted_at, run_id FROM runs WHERE run_id = ?1",
                        [run_id],
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                    )
                    .optional()
            })
            .transpose()?
            .flatten();
        if after.is_some() && after_position.is_none() {
            bail!("--after Run does not exist");
        }
        let (after_at, after_id) =
            after_position.map_or((None, None), |(at, id)| (Some(at), Some(id)));
        let mut statement = connection.prepare(
            "SELECT run_id, accepted_at, metadata_json, input_json, input_identity_json,
                    terminal_at, completion_json
             FROM runs
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
}

fn read_run_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RunRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
    ))
}

fn decode_run(row: RunRow) -> Result<StoredRun> {
    Ok(StoredRun {
        run_id: row.0,
        accepted_at: row.1,
        metadata: serde_json::from_str(&row.2).context("stored Run metadata is invalid")?,
        input: serde_json::from_str(&row.3).context("stored RunInput is invalid")?,
        input_identity: serde_json::from_str(&row.4)
            .context("stored RunInput identity is invalid")?,
        terminal_at: row.5,
        completion: row
            .6
            .map(|value| serde_json::from_str(&value).context("stored completion is invalid"))
            .transpose()?,
    })
}

fn ensure_metadata_column(connection: &Connection, table: &str) -> Result<()> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = statement.query_map([], |row| row.get::<_, String>(1))?;
    for column in columns {
        if column? == "metadata_json" {
            return Ok(());
        }
    }
    connection.execute(
        &format!(
            "ALTER TABLE {table} ADD COLUMN metadata_json TEXT NOT NULL \
             DEFAULT '{{\"description\":null,\"labels\":{{}}}}'"
        ),
        [],
    )?;
    Ok(())
}
