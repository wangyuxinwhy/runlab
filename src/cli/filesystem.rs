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
    /// List bounded Final Environment changes relative to one Run's Initial Image.
    #[command(
        long_about = "List paths changed by one Program's Final Environment relative to its Initial Image. Results are sorted by absolute Image path and bounded by --limit. added and modified entries report the final node; deleted entries report the initial node. subtree:true on a directory means an OCI opaque whiteout replaced or removed its lower subtree.",
        after_long_help = "Example:\n  runlab filesystem changes --run 550e8400-e29b-41d4-a716-446655440000 --limit 100"
    )]
    Changes {
        /// Persistent Run whose Final Environment is compared.
        #[arg(long)]
        run: RunId,
        /// Program to compare; defaults to primary.
        #[arg(long, default_value = "primary")]
        program: String,
        /// Maximum paths returned; must be between 1 and 1000.
        #[arg(long, default_value_t = 100)]
        limit: usize,
        /// Continue strictly after this absolute Image path.
        #[arg(long)]
        after: Option<String>,
    },
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
        FilesystemCommand::Changes {
            run,
            program,
            limit,
            after,
        } => emit(&filesystems.changes(run, program, limit, after.as_deref())?)?,
    }
    Ok(0)
}

#[cfg(target_os = "macos")]
pub(super) fn execute_managed(command: FilesystemCommand) -> Result<u8> {
    let vm = crate::managed_vm::ManagedVm::new();
    match command {
        FilesystemCommand::Get(arguments) => {
            let run = arguments.run.map(|value| value.to_string());
            let image = arguments.image.map(|value| value.to_string());
            let output = vm.forward_filesystem_get(
                run.as_deref(),
                image.as_deref(),
                arguments.program.as_deref(),
                &arguments.path,
                &arguments.output,
            )?;
            super::emit_forwarded(&output)
        }
        FilesystemCommand::Changes {
            run,
            program,
            limit,
            after,
        } => {
            let output =
                vm.forward_filesystem_changes(&run.to_string(), &program, limit, after.as_deref())?;
            super::emit_forwarded(&output)
        }
    }
}
