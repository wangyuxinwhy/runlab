use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use clap::{ArgGroup, Args, Subcommand};

use crate::filesystem::{FilesystemSource, Filesystems};
use crate::image::{ImageSelector, Images};
use crate::run::RunId;
use crate::state::State;

use super::emit;

#[derive(Debug, Subcommand)]
pub(super) enum FilesystemCommand {
    /// Copy one file, directory, or symlink to a new local path.
    #[command(
        group = ArgGroup::new("source").required(true).multiple(false),
        after_long_help = "Examples:\n  runlab filesystem get --run 550e8400-e29b-41d4-a716-446655440000 /artifacts/solution.patch --output ./solution.patch\n  runlab filesystem get --image agent-base /workspace --output ./workspace"
    )]
    Get(FilesystemGetArgs),
}

#[derive(Debug, Args)]
pub(super) struct FilesystemGetArgs {
    /// Final OCI Image of this persistent Run.
    #[arg(long, group = "source")]
    run: Option<RunId>,
    /// OCI Image selected by local name or complete sha256 Manifest digest.
    #[arg(long, group = "source")]
    image: Option<ImageSelector>,
    /// Program whose `final_environment` is read; defaults to primary with --run.
    #[arg(long, requires = "run")]
    program: Option<String>,
    /// Absolute path inside the selected filesystem.
    path: String,
    /// New local file, directory, or symlink; an existing path is never overwritten.
    #[arg(long)]
    output: PathBuf,
}

pub(super) fn execute(state_path: &Path, command: FilesystemCommand) -> Result<u8> {
    let state = State::open(state_path)?;
    let images = Images::new(state.oci(), state.database());
    let filesystems = Filesystems::new(&images, state.database());
    match command {
        FilesystemCommand::Get(arguments) => {
            let source = match (arguments.run, arguments.image) {
                (Some(run_id), None) => FilesystemSource::Run {
                    run_id,
                    program: arguments.program.unwrap_or_else(|| "primary".to_owned()),
                },
                (None, Some(image)) => FilesystemSource::Image(image),
                _ => bail!("exactly one of --run or --image is required"),
            };
            emit(&filesystems.get(source, &arguments.path, &arguments.output)?)?;
        }
    }
    Ok(0)
}
