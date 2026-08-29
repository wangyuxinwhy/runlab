use anyhow::{Context, Result, bail};
use rusqlite::Connection;
use serde::Serialize;

const RUNS_VIEW_SQL: &str = r"
CREATE TEMP VIEW runs AS
SELECT
    run_id,
    accepted_at,
    initial_image_name,
    json_extract(input_json, '$.programs.primary.initial_environment.digest')
        AS initial_image_digest,
    json_extract(metadata_json, '$.description') AS description,
    json_extract(metadata_json, '$.labels') AS labels,
    CASE WHEN completion_json IS NULL THEN 'accepted' ELSE 'terminal' END AS lifecycle,
    terminal_at,
    json_extract(completion_json, '$.result.kind') AS completion_kind,
    json_extract(completion_json, '$.result.output.programs.primary.process.kind')
        AS primary_process_kind,
    json_extract(completion_json, '$.result.output.programs.primary.process.code')
        AS primary_exit_code,
    json_extract(completion_json, '$.result.output.execution.timed_out') AS timed_out,
    json_extract(completion_json, '$.result.output.execution.cancelled') AS cancelled
    ,json_extract(completion_json, '$.result.output.programs.primary.start.facts.started_at')
        AS primary_started_at
    ,json_extract(completion_json, '$.result.output.programs.primary.process.ended_at')
        AS primary_ended_at
    ,ROUND((
        unixepoch(json_extract(completion_json, '$.result.output.programs.primary.process.ended_at'), 'subsec')
        - unixepoch(json_extract(completion_json, '$.result.output.programs.primary.start.facts.started_at'), 'subsec')
    ) * 1000.0, 3) AS primary_duration_ms
    ,ROUND((
        unixepoch(json_extract(completion_json, '$.result.output.programs.primary.start.facts.started_at'), 'subsec')
        - unixepoch(accepted_at, 'subsec')
    ) * 1000.0, 3) AS accepted_to_primary_start_ms
    ,ROUND((
        unixepoch(terminal_at, 'subsec')
        - unixepoch(json_extract(completion_json, '$.result.output.programs.primary.process.ended_at'), 'subsec')
    ) * 1000.0, 3) AS primary_end_to_terminal_ms
    ,CASE
        WHEN json_extract(completion_json, '$.result.output.programs.primary.stdout.status') = 'succeeded'
        THEN (length(json_extract(completion_json, '$.result.output.programs.primary.stdout.facts.bytes.value')) * 3 / 4)
            - CASE
                WHEN json_extract(completion_json, '$.result.output.programs.primary.stdout.facts.bytes.value') LIKE '%==' THEN 2
                WHEN json_extract(completion_json, '$.result.output.programs.primary.stdout.facts.bytes.value') LIKE '%=' THEN 1
                ELSE 0
              END
        ELSE NULL
     END AS primary_stdout_bytes
    ,CASE
        WHEN json_extract(completion_json, '$.result.output.programs.primary.stderr.status') = 'succeeded'
        THEN (length(json_extract(completion_json, '$.result.output.programs.primary.stderr.facts.bytes.value')) * 3 / 4)
            - CASE
                WHEN json_extract(completion_json, '$.result.output.programs.primary.stderr.facts.bytes.value') LIKE '%==' THEN 2
                WHEN json_extract(completion_json, '$.result.output.programs.primary.stderr.facts.bytes.value') LIKE '%=' THEN 1
                ELSE 0
              END
        ELSE NULL
     END AS primary_stderr_bytes
    ,json_extract(completion_json, '$.result.output.programs.primary.final_environment.value.digest')
        AS primary_final_image_digest
FROM main.runs;
";

#[derive(Clone, Copy)]
struct Column {
    name: &'static str,
    data_type: &'static str,
    nullable: bool,
    description: &'static str,
}

const COLUMNS: &[Column] = &[
    Column {
        name: "run_id",
        data_type: "TEXT",
        nullable: false,
        description: "Canonical Run UUID assigned by the caller.",
    },
    Column {
        name: "accepted_at",
        data_type: "TEXT",
        nullable: false,
        description: "Exact RFC 3339 time when RunLab accepted the Run.",
    },
    Column {
        name: "initial_image_name",
        data_type: "TEXT",
        nullable: true,
        description: "Catalog name used at acceptance; NULL for digest selection or older Runs.",
    },
    Column {
        name: "initial_image_digest",
        data_type: "TEXT",
        nullable: false,
        description: "Content digest of the accepted primary Program Initial Image.",
    },
    Column {
        name: "description",
        data_type: "TEXT",
        nullable: true,
        description: "Caller-provided Run description.",
    },
    Column {
        name: "labels",
        data_type: "JSON",
        nullable: false,
        description: "Caller-provided string labels encoded as a JSON object.",
    },
    Column {
        name: "lifecycle",
        data_type: "TEXT",
        nullable: false,
        description: "Persisted lifecycle: accepted or terminal.",
    },
    Column {
        name: "terminal_at",
        data_type: "TEXT",
        nullable: true,
        description: "Exact RFC 3339 terminal publication time, when present.",
    },
    Column {
        name: "completion_kind",
        data_type: "TEXT",
        nullable: true,
        description: "Engine result kind: output or engine_error.",
    },
    Column {
        name: "primary_process_kind",
        data_type: "TEXT",
        nullable: true,
        description: "Primary Program process result kind, when output exists.",
    },
    Column {
        name: "primary_exit_code",
        data_type: "INTEGER",
        nullable: true,
        description: "Primary Program exit code when process kind is exited.",
    },
    Column {
        name: "timed_out",
        data_type: "INTEGER",
        nullable: true,
        description: "RunOutput timeout fact as SQLite 0 or 1.",
    },
    Column {
        name: "cancelled",
        data_type: "INTEGER",
        nullable: true,
        description: "RunOutput cancellation fact as SQLite 0 or 1.",
    },
    Column {
        name: "primary_started_at",
        data_type: "TEXT",
        nullable: true,
        description: "Exact primary Program start time, when start succeeded.",
    },
    Column {
        name: "primary_ended_at",
        data_type: "TEXT",
        nullable: true,
        description: "Exact primary Program process-result time, when observed.",
    },
    Column {
        name: "primary_duration_ms",
        data_type: "REAL",
        nullable: true,
        description: "Milliseconds from primary Program start to process result.",
    },
    Column {
        name: "accepted_to_primary_start_ms",
        data_type: "REAL",
        nullable: true,
        description: "Milliseconds from Run acceptance to primary Program start.",
    },
    Column {
        name: "primary_end_to_terminal_ms",
        data_type: "REAL",
        nullable: true,
        description: "Milliseconds from primary process result to terminal publication.",
    },
    Column {
        name: "primary_stdout_bytes",
        data_type: "INTEGER",
        nullable: true,
        description: "Exact retained primary stdout byte count when stream capture succeeded.",
    },
    Column {
        name: "primary_stderr_bytes",
        data_type: "INTEGER",
        nullable: true,
        description: "Exact retained primary stderr byte count when stream capture succeeded.",
    },
    Column {
        name: "primary_final_image_digest",
        data_type: "TEXT",
        nullable: true,
        description: "Primary Final Environment OCI Manifest digest when available.",
    },
];

#[derive(Debug, Serialize)]
pub(crate) struct SchemaReport {
    schema_version: u32,
    objects: Vec<SchemaObject>,
}

#[derive(Debug, Serialize)]
struct SchemaObject {
    name: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<&'static str>,
    columns: Vec<SchemaColumn>,
}

#[derive(Debug, Serialize)]
struct SchemaColumn {
    name: &'static str,
    data_type: &'static str,
    nullable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<&'static str>,
}

pub(crate) fn install(connection: &Connection) -> Result<()> {
    connection
        .execute_batch("DROP VIEW IF EXISTS temp.runs;")
        .context("failed to replace public runs Relation")?;
    connection
        .execute_batch(RUNS_VIEW_SQL)
        .context("failed to create public runs Relation")?;
    validate(connection)
}

pub(crate) fn report(
    connection: &Connection,
    object: Option<&str>,
    include_descriptions: bool,
) -> Result<SchemaReport> {
    if let Some(name) = object
        && name != "runs"
    {
        bail!(
            "unknown public relation {name:?}\nHint: run `runlab schema list` to discover valid public relation names"
        );
    }
    validate(connection)?;
    let columns = object.map_or_else(Vec::new, |_| {
        COLUMNS
            .iter()
            .map(|column| SchemaColumn {
                name: column.name,
                data_type: column.data_type,
                nullable: column.nullable,
                description: include_descriptions.then_some(column.description),
            })
            .collect()
    });
    Ok(SchemaReport {
        schema_version: 1,
        objects: vec![SchemaObject {
            name: "runs",
            description: include_descriptions.then_some(
                "Immutable accepted Run metadata and bounded terminal outcome facts. Use run get for the complete Run record.",
            ),
            columns,
        }],
    })
}

fn validate(connection: &Connection) -> Result<()> {
    let actual = connection
        .prepare("SELECT name FROM pragma_table_info('runs') ORDER BY cid")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let expected = COLUMNS
        .iter()
        .map(|column| column.name.to_owned())
        .collect::<Vec<_>>();
    if actual != expected {
        bail!("public runs Relation does not match the compiled schema");
    }
    connection
        .prepare("SELECT * FROM temp.runs LIMIT 0")
        .context("public runs Relation cannot be queried")?;
    Ok(())
}
