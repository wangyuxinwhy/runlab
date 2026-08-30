use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use rusqlite::Connection;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::types::ValueRef;
use serde::Serialize;

use crate::storage::Database;

#[derive(Debug, Serialize)]
pub(crate) struct QueryReport {
    schema_version: u32,
    complete: bool,
    incomplete_reason: Option<&'static str>,
    returned: usize,
    cells_truncated: usize,
    columns: Vec<String>,
    rows: Vec<BTreeMap<String, serde_json::Value>>,
}

pub(crate) fn run(
    database: &Database,
    sql: &str,
    limit: usize,
    max_cell_bytes: usize,
    max_output_bytes: usize,
    timeout: Duration,
) -> Result<QueryReport> {
    if sql.trim().is_empty() {
        return Err(crate::error::invalid_input(
            anyhow::anyhow!("SQL must not be empty"),
            "query_input",
        ));
    }
    database.with_connection(|connection| {
        let started = Instant::now();
        connection.progress_handler(10_000, Some(move || started.elapsed() >= timeout))?;
        let result = run_authorized(
            connection,
            sql,
            limit,
            max_cell_bytes,
            max_output_bytes,
        );
        connection.progress_handler(0, None::<fn() -> bool>)?;
        match result {
            Err(error) if is_interrupted(&error) => Err(error.context(format!(
                "query exceeded the {}-second timeout; narrow the Run set or aggregate before returning rows",
                timeout.as_secs()
            ))),
            result => result,
        }
    })
}

fn run_authorized(
    connection: &Connection,
    sql: &str,
    limit: usize,
    max_cell_bytes: usize,
    max_output_bytes: usize,
) -> Result<QueryReport> {
    let executing = Arc::new(AtomicBool::new(false));
    let authorizer_executing = Arc::clone(&executing);
    let public_count = Arc::new(AtomicBool::new(false));
    let authorizer_public_count = Arc::clone(&public_count);
    connection.authorizer(Some(move |context: AuthContext<'_>| {
        authorize(
            context,
            authorizer_executing.load(Ordering::Relaxed),
            &authorizer_public_count,
        )
    }))?;
    let result = execute_query(
        connection,
        sql,
        limit,
        max_cell_bytes,
        max_output_bytes,
        &executing,
    );
    connection.authorizer(None::<fn(AuthContext<'_>) -> Authorization>)?;
    result
}

fn execute_query(
    connection: &Connection,
    sql: &str,
    limit: usize,
    max_cell_bytes: usize,
    max_output_bytes: usize,
    executing: &AtomicBool,
) -> Result<QueryReport> {
    let mut statement = connection
        .prepare(sql)
        .map_err(|error| crate::error::invalid_input(anyhow::Error::from(error), "query_input"))?;
    if !statement.readonly() {
        return Err(crate::error::invalid_input(
            anyhow::anyhow!("only read-only SQL against public Relations is allowed"),
            "query_input",
        ));
    }
    let columns = statement
        .column_names()
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if columns.iter().collect::<BTreeSet<_>>().len() != columns.len() {
        return Err(crate::error::invalid_input(
            anyhow::anyhow!("query output has duplicate column names; use SQL aliases"),
            "query_input",
        ));
    }

    executing.store(true, Ordering::Relaxed);
    let result = (|| {
        let mut query = statement.query([])?;
        let mut rows = Vec::with_capacity(limit);
        let mut output_bytes = 0_usize;
        let mut cells_truncated = 0_usize;
        let mut incomplete_reason = None;
        while rows.len() < limit {
            let Some(row) = query.next()? else {
                break;
            };
            let mut object = BTreeMap::new();
            let mut row_truncated = 0_usize;
            for (index, name) in columns.iter().enumerate() {
                let (value, truncated) = value_to_json(row.get_ref(index)?, max_cell_bytes);
                row_truncated += usize::from(truncated);
                object.insert(name.clone(), value);
            }
            let row_bytes = serde_json::to_vec(&object)?.len() + usize::from(!rows.is_empty());
            if output_bytes.saturating_add(row_bytes) > max_output_bytes {
                incomplete_reason = Some("output_budget");
                break;
            }
            output_bytes += row_bytes;
            cells_truncated += row_truncated;
            rows.push(object);
        }
        if incomplete_reason.is_none() && query.next()?.is_some() {
            incomplete_reason = Some("row_limit");
        }
        Ok(QueryReport {
            schema_version: 1,
            complete: incomplete_reason.is_none(),
            incomplete_reason,
            returned: rows.len(),
            cells_truncated,
            columns,
            rows,
        })
    })();
    executing.store(false, Ordering::Relaxed);
    result
}

fn authorize(
    context: AuthContext<'_>,
    executing: bool,
    public_count: &AtomicBool,
) -> Authorization {
    match context.action {
        AuthAction::Select
            if matches!(
                context.accessor,
                Some(
                    "runs"
                        | "run_deletions"
                        | "observation_types"
                        | "observations"
                        | "observation_retractions",
                )
            ) =>
        {
            public_count.store(true, Ordering::Relaxed);
            Authorization::Allow
        }
        AuthAction::Select | AuthAction::Recursive => Authorization::Allow,
        AuthAction::Function { function_name }
            if !matches!(function_name, "load_extension" | "readfile" | "writefile") =>
        {
            Authorization::Allow
        }
        AuthAction::Read {
            table_name:
                "runs"
                | "run_tombstones"
                | "observation_types"
                | "observations"
                | "observation_retractions",
            column_name: "",
        } if context.database_name == Some("main")
            && context.accessor.is_none()
            && public_count.swap(false, Ordering::Relaxed) =>
        {
            Authorization::Allow
        }
        AuthAction::Read { table_name, .. }
            if (context.database_name == Some("temp")
                && matches!(
                    table_name,
                    "runs"
                        | "run_deletions"
                        | "observation_types"
                        | "observations"
                        | "observation_retractions"
                ))
                || (context.database_name == Some("main")
                    && table_name == "runs"
                    && context.accessor == Some("runs"))
                || (context.database_name == Some("main")
                    && table_name == "run_tombstones"
                    && context.accessor == Some("run_deletions"))
                || (context.database_name == Some("main")
                    && table_name == "observation_types"
                    && context.accessor == Some("observation_types"))
                || (context.database_name == Some("main")
                    && table_name == "observations"
                    && matches!(
                        context.accessor,
                        Some("observations" | "observation_retractions")
                    ))
                || (context.database_name == Some("main")
                    && table_name == "observation_retractions"
                    && matches!(
                        context.accessor,
                        Some("observations" | "observation_retractions")
                    )) =>
        {
            Authorization::Allow
        }
        AuthAction::Pragma {
            pragma_name: "data_version",
            pragma_value: None,
        } if executing => Authorization::Allow,
        _ => Authorization::Deny,
    }
}

fn value_to_json(value: ValueRef<'_>, max_bytes: usize) -> (serde_json::Value, bool) {
    match value {
        ValueRef::Null => (serde_json::Value::Null, false),
        ValueRef::Integer(value) => (serde_json::json!(value), false),
        ValueRef::Real(value) => (serde_json::json!(value), false),
        ValueRef::Text(bytes) => {
            let text = String::from_utf8_lossy(bytes);
            let (text, truncated) = truncate_utf8(&text, max_bytes);
            (serde_json::Value::String(text.to_owned()), truncated)
        }
        ValueRef::Blob(bytes) => {
            let visible = &bytes[..bytes.len().min(max_bytes)];
            let mut encoded = String::with_capacity(visible.len().saturating_mul(2));
            for byte in visible {
                write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
            }
            (
                serde_json::json!({
                    "encoding": "hex",
                    "byte_length": bytes.len(),
                    "value": encoded,
                }),
                visible.len() != bytes.len(),
            )
        }
    }
}

fn truncate_utf8(value: &str, max_bytes: usize) -> (&str, bool) {
    if value.len() <= max_bytes {
        return (value, false);
    }
    let mut boundary = max_bytes;
    while boundary > 0 && !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    (&value[..boundary], true)
}

fn is_interrupted(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<rusqlite::Error>()
            .is_some_and(|error| {
                matches!(
                    error,
                    rusqlite::Error::SqliteFailure(sqlite_error, _)
                        if sqlite_error.code == rusqlite::ErrorCode::OperationInterrupted
                )
            })
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use run_protocol::{EngineError, Network, Secrets};
    use serde_json::json;
    use tempfile::tempdir;

    use super::run;
    use crate::run_record::{CompletionRecord, InputIdentityRecord, InputRecord};
    use crate::storage::Database;

    fn database() -> Database {
        let directory = tempdir().expect("temporary directory");
        let path = directory.keep().join("runlab.sqlite3");
        let database = Database::open(&path).expect("database");
        let descriptor = serde_json::from_value(json!({
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "size": 1
        }))
        .expect("descriptor");
        let input = InputRecord::primary(
            &descriptor,
            b"{}",
            b"",
            &Secrets::empty(),
            None,
            Network::Isolated,
            true,
        );
        let identity = InputIdentityRecord::primary(
            &descriptor,
            &json!({}),
            b"",
            &Secrets::empty(),
            None,
            Network::Isolated,
        );
        let completion = CompletionRecord::engine_returned(Err(EngineError::internal(
            "query fixture EngineError",
        )));
        database
            .with_connection(|connection| {
                connection.execute(
                    "INSERT INTO main.runs(
                        run_id, accepted_at, initial_image_name, metadata_json,
                        input_json, input_identity_json, terminal_at, completion_json
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    rusqlite::params![
                        "550e8400-e29b-41d4-a716-446655440000",
                        "2026-08-28T01:02:03.123456789Z",
                        "pi",
                        r#"{"description":"task","labels":{"suite":"swe-bench"}}"#,
                        serde_json::to_string(&input)?,
                        serde_json::to_string(&identity)?,
                        "2026-08-28T01:02:04.123456789Z",
                        serde_json::to_string(&completion)?,
                    ],
                )?;
                Ok(())
            })
            .expect("fixture");
        database
    }

    fn execute(database: &Database, sql: &str) -> anyhow::Result<super::QueryReport> {
        run(
            database,
            sql,
            100,
            64 * 1024,
            64 * 1024,
            Duration::from_secs(1),
        )
    }

    #[test]
    fn reads_only_the_public_runs_relation() {
        let database = database();
        let report = execute(
            &database,
            "SELECT run_id, initial_image_name, initial_image_digest, description, \
                    json_extract(labels, '$.suite') AS suite, completion_kind \
             FROM runs",
        )
        .expect("public query");
        assert_eq!(report.returned, 1);
        assert_eq!(report.rows[0]["initial_image_name"], "pi");
        assert_eq!(
            report.rows[0]["initial_image_digest"],
            "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
        assert_eq!(report.rows[0]["suite"], "swe-bench");
        assert_eq!(report.rows[0]["completion_kind"], "engine_error");
        let terminal = execute(
            &database,
            "SELECT terminal_at, terminal_unix_seconds FROM runs",
        )
        .expect("terminal range value");
        assert_eq!(
            terminal.rows[0]["terminal_at"],
            "2026-08-28T01:02:04.123456789Z"
        );
        assert!(terminal.rows[0]["terminal_unix_seconds"].as_f64().is_some());

        let count =
            execute(&database, "SELECT COUNT(*) AS run_count FROM runs").expect("public count");
        assert_eq!(count.rows[0]["run_count"], 1);
    }

    #[test]
    fn reads_the_public_run_deletions_relation() {
        let database = database();
        database
            .with_connection(|connection| {
                connection.execute(
                    "INSERT INTO main.run_tombstones(run_id, deleted_at, operation_id)
                     VALUES (?1, ?2, ?3)",
                    [
                        "550e8400-e29b-41d4-a716-446655440001",
                        "2026-08-29T00:00:00Z",
                        "550e8400-e29b-41d4-a716-446655440002",
                    ],
                )?;
                Ok(())
            })
            .expect("tombstone fixture");
        let deletions = execute(
            &database,
            "SELECT run_id, deleted_at, operation_id FROM run_deletions",
        )
        .expect("public deletion query");
        assert_eq!(deletions.returned, 1);
        assert_eq!(
            deletions.rows[0]["operation_id"],
            "550e8400-e29b-41d4-a716-446655440002"
        );
        let deletion_count = execute(&database, "SELECT COUNT(*) AS n FROM run_deletions")
            .expect("public deletion count");
        assert_eq!(deletion_count.rows[0]["n"], 1);
    }

    #[test]
    fn reads_observation_history_through_public_relations() {
        let database = database();
        let types = execute(
            &database,
            "SELECT type, json_extract(payload_schema, '$.properties.input_tokens.type') AS input_kind FROM observation_types",
        )
        .expect("public Observation Type query");
        assert_eq!(types.rows[0]["type"], "runlab/token_usage@v1");
        assert_eq!(types.rows[0]["input_kind"], "integer");
        database
            .with_connection(|connection| {
                for (id, supersedes, total) in [
                    ("550e8400-e29b-41d4-a716-446655440010", None, 15_400),
                    (
                        "550e8400-e29b-41d4-a716-446655440011",
                        Some("550e8400-e29b-41d4-a716-446655440010"),
                        15_200,
                    ),
                ] {
                    connection.execute(
                        "INSERT INTO main.observations(
                            observation_id, run_id, observation_type, submitted_at,
                            method_json, payload_json,
                            supersedes_observation_id
                         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                        rusqlite::params![
                            id,
                            "550e8400-e29b-41d4-a716-446655440000",
                            "runlab/token_usage@v1",
                            "2026-08-29T01:00:00Z",
                            r#"{"name":"fixture","version":"1"}"#,
                            serde_json::to_string(&serde_json::json!({
                                "coverage": "complete",
                                "input_tokens": total - 400,
                                "cached_input_tokens": 8000,
                                "cache_write_input_tokens": null,
                                "output_tokens": 400,
                                "reasoning_output_tokens": 100,
                            }))?,
                            supersedes,
                        ],
                    )?;
                }
                connection.execute(
                    "INSERT INTO main.observation_retractions(
                        retraction_id, observation_id, retracted_at, reason
                     ) VALUES (?1, ?2, ?3, ?4)",
                    [
                        "550e8400-e29b-41d4-a716-446655440012",
                        "550e8400-e29b-41d4-a716-446655440011",
                        "2026-08-29T02:00:00Z",
                        "fixture retraction",
                    ],
                )?;
                Ok(())
            })
            .expect("Observation fixture");
        let observations = execute(
            &database,
            "SELECT observation_id, json_extract(payload, '$.input_tokens') + \
                    json_extract(payload, '$.output_tokens') AS total_tokens, \
                    state, superseded_by_observation_id, \
                    retraction_reason FROM observations ORDER BY observation_id",
        )
        .expect("public Observation query");
        assert_eq!(observations.returned, 2);
        assert_eq!(observations.rows[0]["state"], "superseded");
        assert_eq!(observations.rows[1]["state"], "retracted");
        assert_eq!(observations.rows[1]["total_tokens"], 15_200);
        let retractions = execute(
            &database,
            "SELECT run_id, observation_id, reason FROM observation_retractions",
        )
        .expect("public retraction query");
        assert_eq!(retractions.returned, 1);
        assert_eq!(retractions.rows[0]["reason"], "fixture retraction");
    }

    #[test]
    fn rejects_private_or_mutating_sql() {
        let database = database();
        for sql in [
            "SELECT * FROM main.runs",
            "SELECT COUNT(*) FROM main.runs",
            "SELECT * FROM main.run_tombstones",
            "SELECT * FROM main.observation_types",
            "SELECT * FROM main.observations",
            "SELECT * FROM main.observation_retractions",
            "SELECT (SELECT COUNT(*) FROM main.runs) AS leaked FROM runs",
            "SELECT * FROM sqlite_schema",
            "DELETE FROM runs",
            "ATTACH ':memory:' AS other",
            "SELECT load_extension('x')",
        ] {
            assert!(
                execute(&database, sql).is_err(),
                "private or mutating SQL was allowed: {sql}"
            );
        }
    }

    #[test]
    fn reports_row_and_cell_bounds() {
        let database = database();
        let report = run(
            &database,
            "SELECT description FROM runs UNION ALL SELECT 'second'",
            1,
            2,
            64 * 1024,
            Duration::from_secs(1),
        )
        .expect("bounded query");
        assert!(!report.complete);
        assert_eq!(report.incomplete_reason, Some("row_limit"));
        assert_eq!(report.cells_truncated, 1);
        assert_eq!(report.rows[0]["description"], "ta");
    }
}
