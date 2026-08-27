use std::env;
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde::Serialize;

mod filesystem;
mod image;
mod run;
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
    #[arg(long, value_name = "DIRECTORY")]
    state: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
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

pub(crate) fn run() -> Result<u8> {
    let cli = Cli::parse();
    match cli.command {
        Command::Filesystem { command } => filesystem::execute(&resolve_state(cli.state)?, command),
        Command::Image { command } => image::execute(&resolve_state(cli.state)?, command),
        Command::Run { command } => run::execute(&resolve_state(cli.state)?, command),
        #[cfg(target_os = "macos")]
        Command::Vm { command } => {
            if cli.state.is_some() {
                anyhow::bail!("--state does not apply to managed VM lifecycle commands");
            }
            vm::execute(command)
        }
    }
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

pub(super) fn emit(value: &impl Serialize) -> Result<()> {
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    serde_json::to_writer(&mut stdout, value).context("failed to encode JSON output")?;
    writeln!(stdout).context("failed to write JSON output")
}

pub(super) fn emit_json_bytes(bytes: &[u8]) -> Result<()> {
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    stdout
        .write_all(bytes)
        .context("failed to write JSON output")
}
