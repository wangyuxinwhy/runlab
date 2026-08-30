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

const OBSERVATION_TYPES_VIEW_SQL: &str = r"
CREATE TEMP VIEW observation_types AS
SELECT
    observation_type AS type,
    registered_at,
    title,
    description,
    payload_schema_json AS payload_schema
FROM main.observation_types;
";

const OBSERVATIONS_VIEW_SQL: &str = r"
CREATE TEMP VIEW observations AS
SELECT
    observation.observation_id,
    observation.run_id,
    observation.observation_type AS type,
    observation.submitted_at,
    json_extract(observation.method_json, '$.name') AS method_name,
    json_extract(observation.method_json, '$.version') AS method_version,
    observation.payload_json AS payload,
    observation.supersedes_observation_id,
    replacement.observation_id AS superseded_by_observation_id,
    retraction.retraction_id,
    retraction.retracted_at,
    retraction.reason AS retraction_reason,
    CASE
        WHEN retraction.retraction_id IS NOT NULL THEN 'retracted'
        WHEN replacement.observation_id IS NOT NULL THEN 'superseded'
        ELSE 'active'
    END AS state
FROM main.observations AS observation
LEFT JOIN main.observations AS replacement
  ON replacement.supersedes_observation_id = observation.observation_id
LEFT JOIN main.observation_retractions AS retraction
  ON retraction.observation_id = observation.observation_id;
";

const OBSERVATION_RETRACTIONS_VIEW_SQL: &str = r"
CREATE TEMP VIEW observation_retractions AS
SELECT
    retraction.retraction_id,
    observation.run_id,
    retraction.observation_id,
    retraction.retracted_at,
    retraction.reason
FROM main.observation_retractions AS retraction
JOIN main.observations AS observation USING (observation_id);
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

const OBSERVATION_TYPES_COLUMNS: &[Column] = &[
    Column {
        name: "type",
        data_type: "TEXT",
        nullable: false,
        description: "Immutable versioned Observation Type identity.",
    },
    Column {
        name: "registered_at",
        data_type: "TEXT",
        nullable: false,
        description: "Exact RFC 3339 time when this State first registered the Type.",
    },
    Column {
        name: "title",
        data_type: "TEXT",
        nullable: false,
        description: "Short human-readable Type title.",
    },
    Column {
        name: "description",
        data_type: "TEXT",
        nullable: false,
        description: "Complete semantic contract for producers and consumers of the Type.",
    },
    Column {
        name: "payload_schema",
        data_type: "JSON",
        nullable: false,
        description: "Self-contained JSON Schema Draft 2020-12 used to validate every payload.",
    },
];

const OBSERVATIONS_COLUMNS: &[Column] = &[
    Column {
        name: "observation_id",
        data_type: "TEXT",
        nullable: false,
        description: "Caller-owned canonical UUID v4 identifying this immutable Observation.",
    },
    Column {
        name: "run_id",
        data_type: "TEXT",
        nullable: false,
        description: "Canonical identity of the single terminal Run being observed.",
    },
    Column {
        name: "type",
        data_type: "TEXT",
        nullable: false,
        description: "Registered versioned Observation Type identity.",
    },
    Column {
        name: "submitted_at",
        data_type: "TEXT",
        nullable: false,
        description: "Exact RFC 3339 time when RunLab first stored the Observation.",
    },
    Column {
        name: "method_name",
        data_type: "TEXT",
        nullable: false,
        description: "Method identity declared by the Observation producer.",
    },
    Column {
        name: "method_version",
        data_type: "TEXT",
        nullable: false,
        description: "Method version declared by the Observation producer.",
    },
    Column {
        name: "payload",
        data_type: "JSON",
        nullable: false,
        description: "Complete JSON payload validated against the registered Type schema.",
    },
    Column {
        name: "supersedes_observation_id",
        data_type: "TEXT",
        nullable: true,
        description: "Older active Observation replaced by this Observation, when present.",
    },
    Column {
        name: "superseded_by_observation_id",
        data_type: "TEXT",
        nullable: true,
        description: "Newer Observation that replaced this Observation, when present.",
    },
    Column {
        name: "retraction_id",
        data_type: "TEXT",
        nullable: true,
        description: "Immutable retraction identity when this Observation was withdrawn.",
    },
    Column {
        name: "retracted_at",
        data_type: "TEXT",
        nullable: true,
        description: "Exact RFC 3339 retraction time, when retracted.",
    },
    Column {
        name: "retraction_reason",
        data_type: "TEXT",
        nullable: true,
        description: "Caller-provided reason explaining the retraction.",
    },
    Column {
        name: "state",
        data_type: "TEXT",
        nullable: false,
        description: "Derived state: active, superseded, or retracted.",
    },
];

const OBSERVATION_RETRACTIONS_COLUMNS: &[Column] = &[
    Column {
        name: "retraction_id",
        data_type: "TEXT",
        nullable: false,
        description: "Caller-owned canonical UUID v4 identifying the immutable retraction.",
    },
    Column {
        name: "run_id",
        data_type: "TEXT",
        nullable: false,
        description: "Run identity inherited from the retracted Observation.",
    },
    Column {
        name: "observation_id",
        data_type: "TEXT",
        nullable: false,
        description: "Identity of the Observation that was withdrawn.",
    },
    Column {
        name: "retracted_at",
        data_type: "TEXT",
        nullable: false,
        description: "Exact RFC 3339 time when RunLab first stored the retraction.",
    },
    Column {
        name: "reason",
        data_type: "TEXT",
        nullable: false,
        description: "Caller-provided reason explaining the retraction.",
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
    Relation {
        name: "observation_types",
        description: "Immutable Observation Type definitions used by the same validation and query path for built-in and externally registered Types.",
        columns: OBSERVATION_TYPES_COLUMNS,
    },
    Relation {
        name: "observations",
        description: "Immutable structured Observations attached to one terminal Run, including generic JSON payloads and derived correction state.",
        columns: OBSERVATIONS_COLUMNS,
    },
    Relation {
        name: "observation_retractions",
        description: "Immutable reasons withdrawing previously active Observations.",
        columns: OBSERVATION_RETRACTIONS_COLUMNS,
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
             DROP VIEW IF EXISTS temp.run_deletions;
             DROP VIEW IF EXISTS temp.observation_types;
             DROP VIEW IF EXISTS temp.observations;
             DROP VIEW IF EXISTS temp.observation_retractions;",
        )
        .context("failed to replace public Relations")?;
    connection
        .execute_batch(&format!(
            "{RUNS_VIEW_SQL}{RUN_DELETIONS_VIEW_SQL}{OBSERVATION_TYPES_VIEW_SQL}{OBSERVATIONS_VIEW_SQL}{OBSERVATION_RETRACTIONS_VIEW_SQL}"
        ))
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
