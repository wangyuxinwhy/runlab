use std::env;
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand, error::ErrorKind};
use serde::Serialize;

use crate::metadata::{Label, Metadata};

mod docs;
#[cfg_attr(target_os = "macos", allow(dead_code))]
mod filesystem;
#[cfg_attr(target_os = "macos", allow(dead_code))]
mod image;
mod query;
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub(crate) mod run;
mod schema;
mod storage;
#[cfg(target_os = "macos")]
mod vm;

#[derive(Debug, Parser)]
#[command(
    name = "runlab",
    version,
    about = "Execute OCI environments and preserve selected executions as immutable Runs.",
    after_help = "Start with `runlab docs get start-here` for one complete Image-to-Final-Environment workflow."
)]
struct Cli {
    /// State Directory for filesystem, Image, Run, schema, and query commands; defaults to `RUNLAB_STATE`, `$XDG_DATA_HOME/runlab`, or `$HOME/.local/share/runlab`.
    #[cfg(not(target_os = "macos"))]
    #[arg(long, value_name = "DIRECTORY")]
    state: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    #[cfg(target_os = "linux")]
    #[command(name = "__managed-vm-handshake", hide = true)]
    ManagedVmHandshake,
    /// Read version-matched guidance bundled with this CLI.
    Docs {
        #[command(subcommand)]
        command: docs::DocsCommand,
    },
    /// Execute once without creating a persistent Run or Final Image.
    #[command(
        long_about = "Execute one Run Protocol invocation for immediate observation without creating a persistent Run. The command uses the same Image resolution, Runtime Configuration, stdin, Secret, timeout, and network behavior as run start, but it has no run_id, accepted record, metadata, query/get surface, recovery, or Final Image. stderr emits the NDJSON observation stream with run_id:null. Success writes the complete bounded RunOutput or EngineError JSON to stdout because there is no later run get. Program and external side effects are real; this is not a dry run.",
        after_long_help = "Use exec to inspect an environment or command before a persistent Run matters. Do not present a later run start as a first attempt when earlier exec calls used the same evaluation task.\n\nExamples:\n  runlab exec --image base\n  runlab exec --image pi --stdin prompt.txt --network egress --secret-env DEEPSEEK_API_KEY\n  runlab exec --image base --runtime-config config.json >execution.json"
    )]
    Exec(run::ExecArgs),
    /// Read paths from OCI Image filesystem views.
    Filesystem {
        #[command(subcommand)]
        command: filesystem::FilesystemCommand,
    },
    /// Import, discover, and read OCI Images.
    Image {
        #[command(subcommand)]
        command: image::ImageCommand,
    },
    /// Start, cancel, and read persistent Runs.
    Run {
        #[command(subcommand)]
        command: run::RunCommand,
    },
    /// Inspect the stable public SQL schema.
    Schema {
        #[command(subcommand)]
        command: schema::SchemaCommand,
    },
    /// Query Runs through bounded read-only SQL.
    Query {
        #[command(subcommand)]
        command: query::QueryCommand,
    },
    /// Inspect and reclaim local storage without deleting immutable assets.
    Storage {
        #[command(subcommand)]
        command: storage::StorageCommand,
    },
    /// Manage the local Linux execution VM.
    #[cfg(target_os = "macos")]
    Vm {
        #[command(subcommand)]
        command: vm::VmCommand,
    },
}

#[derive(Clone, Debug, Default, Args)]
pub(crate) struct MetadataArgs {
    /// Short caller-provided description stored for Agent selection. Combined metadata must fit in 8 KiB.
    #[arg(long, value_name = "TEXT")]
    description: Option<String>,
    /// Caller-defined metadata label as KEY=VALUE; repeatable. Keys are not interpreted.
    #[arg(long = "label", value_name = "KEY=VALUE")]
    labels: Vec<Label>,
}

impl MetadataArgs {
    pub(crate) fn resolve(&self) -> Result<Metadata> {
        Metadata::new(self.description.clone(), &self.labels)
    }
}

pub(crate) fn run() -> Result<u8> {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) =>
        {
            error.print()?;
            return Ok(0);
        }
        Err(error) => {
            return Err(crate::error::classify(
                error.into(),
                crate::error::ErrorFacts::before_run(
                    crate::error::ErrorCategory::InvalidInput,
                    "arguments",
                ),
            ));
        }
    };
    #[cfg(not(target_os = "macos"))]
    let state = cli.state.as_ref();
    #[cfg(target_os = "macos")]
    let state = None;
    match cli.command {
        #[cfg(target_os = "linux")]
        Command::ManagedVmHandshake => {
            emit(&crate::managed_vm::guest_handshake())?;
            Ok(0)
        }
        Command::Docs { command } => docs::execute(command),
        Command::Exec(arguments) => execute_exec(state, arguments),
        Command::Filesystem { command } => execute_filesystem(state, command),
        Command::Image { command } => execute_image(state, command),
        Command::Run { command } => execute_run(state, command),
        Command::Schema { command } => execute_schema(state, command),
        Command::Query { command } => execute_query(state, command),
        Command::Storage { command } => execute_storage(state, &command),
        #[cfg(target_os = "macos")]
        Command::Vm { command } => vm::execute(command),
    }
}

fn execute_storage(state: Option<&PathBuf>, command: &storage::StorageCommand) -> Result<u8> {
    #[cfg(target_os = "macos")]
    {
        reject_managed_state(state)?;
        storage::execute_managed(command)
    }
    #[cfg(not(target_os = "macos"))]
    storage::execute(&resolve_state(state.cloned())?, command.clone())
}

fn execute_exec(state: Option<&PathBuf>, arguments: run::ExecArgs) -> Result<u8> {
    #[cfg(target_os = "macos")]
    {
        reject_managed_state(state)?;
        run::execute_exec_managed(arguments)
    }
    #[cfg(not(target_os = "macos"))]
    run::execute_exec(&resolve_state(state.cloned())?, arguments)
}

fn execute_schema(state: Option<&PathBuf>, command: schema::SchemaCommand) -> Result<u8> {
    #[cfg(target_os = "macos")]
    {
        reject_managed_state(state)?;
        schema::execute_managed(command)
    }
    #[cfg(not(target_os = "macos"))]
    schema::execute(&resolve_state(state.cloned())?, command)
}

fn execute_query(state: Option<&PathBuf>, command: query::QueryCommand) -> Result<u8> {
    #[cfg(target_os = "macos")]
    {
        reject_managed_state(state)?;
        query::execute_managed(command)
    }
    #[cfg(not(target_os = "macos"))]
    query::execute(&resolve_state(state.cloned())?, command)
}

fn execute_filesystem(
    state: Option<&PathBuf>,
    command: filesystem::FilesystemCommand,
) -> Result<u8> {
    #[cfg(target_os = "macos")]
    {
        reject_managed_state(state)?;
        filesystem::execute_managed(command)
    }
    #[cfg(not(target_os = "macos"))]
    filesystem::execute(&resolve_state(state.cloned())?, command)
}

fn execute_image(state: Option<&PathBuf>, command: image::ImageCommand) -> Result<u8> {
    #[cfg(target_os = "macos")]
    {
        reject_managed_state(state)?;
        image::execute_managed(command)
    }
    #[cfg(not(target_os = "macos"))]
    image::execute(&resolve_state(state.cloned())?, command)
}

fn execute_run(state: Option<&PathBuf>, command: run::RunCommand) -> Result<u8> {
    #[cfg(target_os = "macos")]
    {
        reject_managed_state(state)?;
        run::execute_managed(command)
    }
    #[cfg(not(target_os = "macos"))]
    run::execute(&resolve_state(state.cloned())?, command)
}

#[cfg(target_os = "macos")]
fn reject_managed_state(state: Option<&PathBuf>) -> Result<()> {
    debug_assert!(state.is_none());
    if env::var_os("RUNLAB_STATE").is_some() {
        anyhow::bail!(
            "custom State does not apply on macOS; RunLab State is managed in the Linux VM"
        );
    }
    Ok(())
}

#[cfg_attr(target_os = "macos", allow(dead_code))]
fn resolve_state(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path);
    }
    if let Some(path) = env::var_os("RUNLAB_STATE") {
        return Ok(PathBuf::from(path));
    }
    if let Some(path) = env::var_os("XDG_DATA_HOME") {
        return Ok(PathBuf::from(path).join("runlab"));
    }
    let home = env::var_os("HOME").context("HOME is not set and no --state was supplied")?;
    Ok(PathBuf::from(home).join(".local/share/runlab"))
}

pub(super) fn emit(value: &impl Serialize) -> Result<()> {
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    serde_json::to_writer(&mut stdout, value).context("failed to encode JSON output")?;
    writeln!(stdout).context("failed to write JSON output")
}

#[cfg_attr(target_os = "macos", allow(dead_code))]
pub(super) fn emit_json_bytes(bytes: &[u8]) -> Result<()> {
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    stdout
        .write_all(bytes)
        .context("failed to write JSON output")
}

#[cfg(target_os = "macos")]
pub(super) fn emit_forwarded(output: &crate::managed_vm::ForwardedOutput) -> Result<u8> {
    std::io::stderr()
        .write_all(&output.stderr)
        .context("failed to write guest stderr")?;
    std::io::stdout()
        .write_all(&output.stdout)
        .context("failed to write guest stdout")?;
    Ok(0)
}
