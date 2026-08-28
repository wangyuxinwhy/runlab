use std::env;
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use serde::Serialize;

use crate::metadata::{Label, Metadata};

#[cfg_attr(target_os = "macos", allow(dead_code))]
mod filesystem;
#[cfg_attr(target_os = "macos", allow(dead_code))]
mod image;
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub(crate) mod run;
#[cfg(target_os = "macos")]
mod vm;

#[derive(Debug, Parser)]
#[command(
    name = "runlab",
    version,
    about = "Run OCI environments and preserve immutable Run records."
)]
struct Cli {
    /// State Directory for filesystem, Image, and Run commands; defaults to `RUNLAB_STATE`, `$XDG_DATA_HOME/runlab`, or `$HOME/.local/share/runlab`.
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
    /// Start and read persistent Runs.
    Run {
        #[command(subcommand)]
        command: run::RunCommand,
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
    let cli = Cli::parse();
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
        Command::Filesystem { command } => execute_filesystem(state, command),
        Command::Image { command } => execute_image(state, command),
        Command::Run { command } => execute_run(state, command),
        #[cfg(target_os = "macos")]
        Command::Vm { command } => vm::execute(command),
    }
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
