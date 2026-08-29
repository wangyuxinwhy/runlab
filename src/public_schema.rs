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
    unixepoch(terminal_at, 'subsec') AS terminal_unix_seconds,
    CASE json_extract(completion_json, '$.kind')
        WHEN 'interrupted' THEN 'interrupted'
        ELSE json_extract(completion_json, '$.result.kind')
    END AS completion_kind,
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
    ,json_extract(completion_json, '$.result.output.programs.primary.stdout.facts.bytes.byte_length')
        AS primary_stdout_bytes
    ,json_extract(completion_json, '$.result.output.programs.primary.stderr.facts.bytes.byte_length')
        AS primary_stderr_bytes
    ,json_extract(completion_json, '$.result.output.programs.primary.final_environment.value.digest')
        AS primary_final_image_digest
FROM main.runs;
";

const RUN_DELETIONS_VIEW_SQL: &str = r"
CREATE TEMP VIEW run_deletions AS
SELECT run_id, deleted_at, operation_id
FROM main.run_tombstones;
";

#[derive(Clone, Copy)]
struct Column {
    name: &'static str,
    data_type: &'static str,
    nullable: bool,
    description: &'static str,
}

const RUNS_COLUMNS: &[Column] = &[
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
        name: "terminal_unix_seconds",
        data_type: "REAL",
        nullable: true,
        description: "Millisecond-rounded Unix seconds for range selection only; terminal_at is the exact fact.",
    },
    Column {
        name: "completion_kind",
        data_type: "TEXT",
        nullable: true,
        description: "Completion kind: output, engine_error, or interrupted.",
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

const RUN_DELETIONS_COLUMNS: &[Column] = &[
    Column {
        name: "run_id",
        data_type: "TEXT",
        nullable: false,
        description: "Canonical identity of the deleted Run.",
    },
    Column {
        name: "deleted_at",
        data_type: "TEXT",
        nullable: false,
        description: "Exact RFC 3339 time when the Run deletion committed.",
    },
    Column {
        name: "operation_id",
        data_type: "TEXT",
        nullable: false,
        description: "Caller-owned UUID v4 identifying the deletion operation.",
    },
];

struct Relation {
    name: &'static str,
    description: &'static str,
    columns: &'static [Column],
}

const RELATIONS: &[Relation] = &[
    Relation {
        name: "runs",
        description: "Immutable accepted Run metadata and bounded terminal outcome facts. Use run get for the complete Run record.",
        columns: RUNS_COLUMNS,
    },
    Relation {
        name: "run_deletions",
        description: "Durable deletion facts for removed Run identities.",
        columns: RUN_DELETIONS_COLUMNS,
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
        .execute_batch(
            "DROP VIEW IF EXISTS temp.runs;
             DROP VIEW IF EXISTS temp.run_deletions;",
        )
        .context("failed to replace public Relations")?;
    connection
        .execute_batch(&format!("{RUNS_VIEW_SQL}{RUN_DELETIONS_VIEW_SQL}"))
        .context("failed to create public Relations")?;
    validate(connection)
}

pub(crate) fn report(
    connection: &Connection,
    object: Option<&str>,
    include_descriptions: bool,
) -> Result<SchemaReport> {
    let selected = object
        .map(|name| {
            RELATIONS.iter().find(|relation| relation.name == name).ok_or_else(|| {
                anyhow::anyhow!(
                    "unknown public relation {name:?}\nHint: run `runlab schema list` to discover valid public relation names"
                )
            })
        })
        .transpose()?;
    validate(connection)?;
    Ok(SchemaReport {
        schema_version: 1,
        objects: RELATIONS
            .iter()
            .filter(|relation| selected.is_none_or(|selected| selected.name == relation.name))
            .map(|relation| SchemaObject {
                name: relation.name,
                description: include_descriptions.then_some(relation.description),
                columns: selected.map_or_else(Vec::new, |_| {
                    relation
                        .columns
                        .iter()
                        .map(|column| SchemaColumn {
                            name: column.name,
                            data_type: column.data_type,
                            nullable: column.nullable,
                            description: include_descriptions.then_some(column.description),
                        })
                        .collect()
                }),
            })
            .collect(),
    })
}

fn validate(connection: &Connection) -> Result<()> {
    for relation in RELATIONS {
        let actual = connection
            .prepare(&format!(
                "SELECT name FROM pragma_table_info('{}') ORDER BY cid",
                relation.name
            ))?
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let expected = relation
            .columns
            .iter()
            .map(|column| column.name.to_owned())
            .collect::<Vec<_>>();
        if actual != expected {
            bail!(
                "public {} Relation does not match the compiled schema",
                relation.name
            );
        }
        connection
            .prepare(&format!("SELECT * FROM temp.{} LIMIT 0", relation.name))
            .with_context(|| format!("public {} Relation cannot be queried", relation.name))?;
    }
    Ok(())
}
