use std::io::Read as _;
#[cfg(not(target_os = "macos"))]
use std::path::Path;
use std::path::PathBuf;
#[cfg(not(target_os = "macos"))]
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{ArgGroup, Args, Subcommand};

#[cfg(not(target_os = "macos"))]
use crate::state::State;

const MAX_SQL_INPUT_BYTES: usize = 1024 * 1024;
const MAX_BOUND: usize = 1024 * 1024;

#[derive(Debug, Subcommand)]
pub(super) enum QueryCommand {
    /// Run one bounded read-only SQL statement against public Relations.
    #[command(
        after_long_help = "Behavior:\n  Reads the existing Run catalog without changing it. Only the public Relations\n  shown by schema list/get are accessible. Output is bounded by rows, cell bytes,\n  total serialized row bytes, and time.\n\nExamples:\n  runlab query run \"SELECT run_id, initial_image_name, lifecycle FROM runs ORDER BY accepted_at DESC LIMIT 10\"\n  runlab query run --stdin <<'SQL'\n  SELECT run_id, primary_exit_code\n  FROM runs\n  WHERE json_extract(labels, '$.suite') = 'swe-bench'\n  ORDER BY accepted_at DESC;\n  SQL"
    )]
    Run(QueryRunArgs),
}

#[derive(Debug, Args)]
#[command(group(
    ArgGroup::new("input")
        .required(true)
        .multiple(false)
        .args(["sql", "file", "stdin"])
))]
pub(super) struct QueryRunArgs {
    /// SQL statement to execute. Prefer --file or --stdin for multiline SQL.
    sql: Option<String>,
    /// Read the SQL statement from a UTF-8 file.
    #[arg(long, value_name = "FILE")]
    file: Option<PathBuf>,
    /// Read the SQL statement from standard input.
    #[arg(long)]
    stdin: bool,
    /// Maximum rows returned; must be between 1 and 10000.
    #[arg(long, default_value_t = 100)]
    pub(crate) limit: usize,
    /// Maximum UTF-8 bytes returned for one text or blob cell; at most 1 MiB.
    #[arg(long, default_value_t = 64 * 1024)]
    pub(crate) max_cell_bytes: usize,
    /// Maximum serialized bytes returned across query rows; at most 1 MiB.
    #[arg(long, default_value_t = 64 * 1024)]
    pub(crate) max_output_bytes: usize,
    /// Abort the query after this many seconds; must be between 1 and 300.
    #[arg(long, default_value_t = 30)]
    pub(crate) timeout_seconds: u64,
}

#[cfg(not(target_os = "macos"))]
pub(super) fn execute(state_path: &Path, command: QueryCommand) -> Result<u8> {
    let QueryCommand::Run(arguments) = command;
    arguments.validate()?;
    let sql = arguments.read_sql()?;
    let state = State::open(state_path)?;
    super::emit(&crate::query::run(
        state.database(),
        &sql,
        arguments.limit,
        arguments.max_cell_bytes,
        arguments.max_output_bytes,
        Duration::from_secs(arguments.timeout_seconds),
    )?)?;
    Ok(0)
}

#[cfg(target_os = "macos")]
pub(super) fn execute_managed(command: QueryCommand) -> Result<u8> {
    let QueryCommand::Run(arguments) = command;
    arguments.validate()?;
    let sql = arguments.read_sql()?;
    let output = crate::managed_vm::ManagedVm::new().forward_query_run(
        &sql,
        arguments.limit,
        arguments.max_cell_bytes,
        arguments.max_output_bytes,
        arguments.timeout_seconds,
    )?;
    super::emit_forwarded(&output)
}

impl QueryRunArgs {
    fn validate(&self) -> Result<()> {
        if !(1..=10_000).contains(&self.limit) {
            bail!("--limit must be between 1 and 10000");
        }
        for (name, value) in [
            ("--max-cell-bytes", self.max_cell_bytes),
            ("--max-output-bytes", self.max_output_bytes),
        ] {
            if !(1..=MAX_BOUND).contains(&value) {
                bail!("{name} must be between 1 and {MAX_BOUND}");
            }
        }
        if !(1..=300).contains(&self.timeout_seconds) {
            bail!("--timeout-seconds must be between 1 and 300");
        }
        Ok(())
    }

    fn read_sql(&self) -> Result<String> {
        if let Some(sql) = &self.sql {
            if sql.len() > MAX_SQL_INPUT_BYTES {
                bail!("inline SQL exceeds the {MAX_SQL_INPUT_BYTES}-byte input limit");
            }
            return Ok(sql.clone());
        }
        if let Some(path) = &self.file {
            let file = std::fs::File::open(path)
                .with_context(|| format!("failed to open SQL file {}", path.display()))?;
            return read_bounded(file, &format!("SQL file {}", path.display()));
        }
        if self.stdin {
            return read_bounded(std::io::stdin().lock(), "SQL from stdin");
        }
        unreachable!("clap requires exactly one SQL input")
    }
}

fn read_bounded(reader: impl std::io::Read, source: &str) -> Result<String> {
    let mut bytes = Vec::new();
    reader
        .take(u64::try_from(MAX_SQL_INPUT_BYTES)? + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("failed to read {source}"))?;
    if bytes.len() > MAX_SQL_INPUT_BYTES {
        bail!("{source} exceeds the {MAX_SQL_INPUT_BYTES}-byte input limit");
    }
    String::from_utf8(bytes).with_context(|| format!("{source} is not valid UTF-8"))
}
