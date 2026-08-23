//! The noun-verb command surface: parse arguments, print one JSON document on
//! stdout, and return an exit status.
//!
//! This layer owns argument shapes and output shapes and nothing else. Lifecycle
//! decisions belong to `execution`, Image decisions to `image`, and durability to
//! `storage`; a handler here reads inputs, calls one of those, and emits the
//! result. Errors go to stderr as plain text so stdout stays machine-readable.
//!
//! This module is the composition root: `Cli`, the top-level `Command`, and the
//! few helpers every subcommand needs. Each submodule owns one subcommand
//! completely -- the arguments it accepts, the handler that runs it, and the
//! shape it prints -- so a change to one command touches one file.

use std::env;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde::Serialize;

use crate::image::ImageService;
use crate::integrity::ensure_private_directory;
use crate::oci::OciLayout;
use crate::state::StateOperation;
use crate::storage::RunDatabase;
use crate::subprocess::{NETWORK_HOLDER_COMMAND, TCP_PROBE_COMMAND};

mod image;
mod inputs;
mod run;
mod schema;
mod vm;

use image::{DockerCommand, ImageCommand, run_docker_with_state, run_image};
use inputs::{
    ManagedServiceCommand, RuntimeConfigCommand, check_runtime_config, run_managed_service,
    run_runtime_config,
};
use run::{RunCommand, StateCommand, run_run, run_state};
use schema::{SchemaCommand, run_schema};
use vm::{VmCommand, run_vm};

#[derive(Debug, Parser)]
#[command(
    name = "runlab",
    version,
    about = "Execute OCI Images and preserve immutable Run Records."
)]
struct Cli {
    /// Local OCI Layout and Run database; `vm` rejects host state and requires `--namespace`.
    #[arg(long, global = true, value_name = "DIRECTORY")]
    state: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    #[command(flatten)]
    InternalVm(vm::GuestCommand),
    #[command(name = NETWORK_HOLDER_COMMAND, hide = true)]
    InternalNetworkHolder {
        #[arg(long)]
        directory: PathBuf,
        #[arg(long)]
        run_id: String,
    },
    #[command(name = TCP_PROBE_COMMAND, hide = true)]
    InternalTcpProbe {
        #[arg(long)]
        port: u16,
        #[arg(long)]
        timeout_milliseconds: u64,
    },
    /// Run Linux `RunLab` in a managed Lima VM without mounting host state.
    Vm {
        #[command(subcommand)]
        command: VmCommand,
    },
    /// Operate on standard OCI Images in the local OCI Image Layout.
    Image {
        #[command(subcommand)]
        command: ImageCommand,
    },
    /// Use the explicit Docker compatibility adapter.
    Docker {
        #[command(subcommand)]
        command: DockerCommand,
    },
    /// Create or check standard OCI Runtime config.json files.
    #[command(name = "runtime-config")]
    RuntimeConfig {
        #[command(subcommand)]
        command: RuntimeConfigCommand,
    },
    /// Validate a bounded Managed Service participant declaration.
    #[command(name = "managed-service")]
    ManagedService {
        #[command(subcommand)]
        command: ManagedServiceCommand,
    },
    /// Execute one OCI Image with one OCI Runtime config.json.
    Run {
        #[command(subcommand)]
        command: RunCommand,
    },
    /// Verify or maintain the complete local `RunLab` state.
    State {
        #[command(subcommand)]
        command: StateCommand,
    },
    /// Inspect versioned `RunLab` public JSON schemas.
    Schema {
        #[command(subcommand)]
        command: SchemaCommand,
    },
}

pub fn run() -> Result<u8> {
    let cli = Cli::parse();
    match cli.command {
        Command::InternalNetworkHolder { directory, run_id } => {
            run_internal_network_holder(&directory, &run_id)
        }
        Command::InternalTcpProbe {
            port,
            timeout_milliseconds,
        } => run_internal_tcp_probe(port, timeout_milliseconds),
        Command::InternalVm(command) => vm::run_guest(command),
        Command::Vm { command } => run_vm(cli.state.as_ref(), command),
        Command::Image { command } => run_image(&resolve_state(cli.state)?, command),
        Command::Docker { command } => run_docker_with_state(cli.state, command),
        Command::RuntimeConfig {
            command: RuntimeConfigCommand::Check { path },
        } => check_runtime_config(&path),
        Command::RuntimeConfig { command } => {
            with_existing_state(cli.state, |state| run_runtime_config(state, command))
        }
        Command::ManagedService { command } => {
            with_existing_state(cli.state, |state| run_managed_service(state, command))
        }
        Command::Run { command } => run_run(&resolve_state(cli.state)?, command),
        Command::State { command } => run_state(&resolve_state(cli.state)?, command),
        Command::Schema { command } => run_schema(command),
    }
}

fn with_existing_state(
    explicit: Option<PathBuf>,
    operation: impl FnOnce(&Path) -> Result<u8>,
) -> Result<u8> {
    let state = resolve_state(explicit)?;
    let _operation = StateOperation::enter_existing(&state)?;
    operation(&state)
}

#[cfg(target_os = "linux")]
fn run_internal_network_holder(directory: &Path, run_id: &str) -> Result<u8> {
    let run_id = crate::core::RunId::parse(run_id)
        .context("internal network holder Run identity is invalid")?;
    crate::native::network::hold_network_namespace(directory, run_id)
        .context("internal network holder failed")?;
    Ok(0)
}

#[cfg(not(target_os = "linux"))]
fn run_internal_network_holder(_directory: &Path, _run_id: &str) -> Result<u8> {
    anyhow::bail!("internal network holder requires Linux")
}

#[cfg(target_os = "linux")]
fn run_internal_tcp_probe(port: u16, timeout_milliseconds: u64) -> Result<u8> {
    use std::io::ErrorKind;

    match crate::native::network::connect_loopback_tcp(
        port,
        std::time::Duration::from_millis(timeout_milliseconds),
    ) {
        Ok(()) => Ok(0),
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::ConnectionRefused | ErrorKind::TimedOut | ErrorKind::WouldBlock
            ) =>
        {
            Ok(75)
        }
        Err(error) => Err(error).context("internal TCP readiness probe failed"),
    }
}

#[cfg(not(target_os = "linux"))]
fn run_internal_tcp_probe(_port: u16, _timeout_milliseconds: u64) -> Result<u8> {
    anyhow::bail!("internal TCP readiness probe requires Linux")
}

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

fn image_service(state: &Path) -> Result<ImageService> {
    ensure_private_directory(state)?;
    Ok(ImageService::new(OciLayout::open(state.join("oci"))?))
}

fn run_database(state: &Path) -> Result<RunDatabase> {
    ensure_private_directory(state)?;
    RunDatabase::open(state.join("runs.sqlite3"))
}

fn emit(value: &impl Serialize) -> Result<()> {
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    serde_json::to_writer(&mut lock, value).context("failed to serialize JSON output")?;
    writeln!(lock).context("failed to write JSON output")
}

fn absolute_path(path: &Path) -> Result<String> {
    path.canonicalize()
        .with_context(|| format!("failed to canonicalize {}", path.display()))?
        .to_str()
        .map(ToOwned::to_owned)
        .with_context(|| format!("path is not valid UTF-8: {}", path.display()))
}
