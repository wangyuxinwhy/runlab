#[cfg(not(target_os = "macos"))]
use std::path::Path;
use std::path::PathBuf;

use anyhow::Result;
use clap::Subcommand;

#[derive(Clone, Debug, Subcommand)]
pub(super) enum StorageCommand {
    /// Report VM filesystem capacity, State usage, references, and reclaimable cache.
    Status,
    /// Plan or apply deletion of rebuildable cache and unreferenced OCI content.
    Prune {
        #[command(subcommand)]
        command: StoragePruneCommand,
    },
}

#[derive(Clone, Debug, Subcommand)]
pub(super) enum StoragePruneCommand {
    /// Show exactly how much rebuildable or unreferenced data would be removed.
    Check {
        /// Assume the terminal Run IDs listed in this file are not retention roots; use - for stdin.
        #[arg(long, value_name = "FILE")]
        without_runs: Option<PathBuf>,
    },
    /// Remove only rebuildable Engine cache, stale invocations, and unreferenced OCI blobs.
    Apply,
}

#[cfg(not(target_os = "macos"))]
pub(super) fn execute(state_path: &Path, command: StorageCommand) -> Result<u8> {
    let apply = matches!(
        command,
        StorageCommand::Prune {
            command: StoragePruneCommand::Apply
        }
    );
    let state = if apply {
        crate::state::State::open_exclusive(state_path).map_err(|error| {
            crate::error::classify(
                error,
                crate::error::ErrorFacts {
                    category: crate::error::ErrorCategory::Conflict,
                    stage: "storage_prune",
                    run_id: None,
                    accepted: Some(false),
                    run_created: Some(false),
                    retryable: true,
                    recovery: Some(
                        "retry `runlab storage prune apply` after active State commands finish"
                            .to_owned(),
                    ),
                },
            )
        })?
    } else {
        crate::state::State::open(state_path)?
    };
    match command {
        StorageCommand::Status => super::emit(&crate::storage_management::status(&state)?)?,
        StorageCommand::Prune { command } => {
            let without_runs = match &command {
                StoragePruneCommand::Check { without_runs } => {
                    super::input::read_optional_run_ids(without_runs.as_deref())?
                }
                StoragePruneCommand::Apply => std::collections::BTreeSet::new(),
            };
            super::emit(&crate::storage_management::prune(
                &state,
                matches!(command, StoragePruneCommand::Apply),
                &without_runs,
            )?)?;
        }
    }
    Ok(0)
}

#[cfg(target_os = "macos")]
pub(super) fn execute_managed(command: &StorageCommand) -> Result<u8> {
    let output = match command {
        StorageCommand::Status => {
            crate::managed_vm::ManagedVm::new().forward_storage(&["storage", "status"])
        }
        StorageCommand::Prune {
            command: StoragePruneCommand::Check { without_runs },
        } => crate::managed_vm::ManagedVm::new().forward_storage_prune_check(
            &super::input::read_optional_run_ids(without_runs.as_deref())?,
        ),
        StorageCommand::Prune {
            command: StoragePruneCommand::Apply,
        } => crate::managed_vm::ManagedVm::new().forward_storage(&["storage", "prune", "apply"]),
    };
    super::emit_forwarded(&output?)
}
