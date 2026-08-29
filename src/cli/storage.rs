#[cfg(not(target_os = "macos"))]
use std::path::Path;

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
    Check,
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
        StorageCommand::Prune { command } => super::emit(&crate::storage_management::prune(
            &state,
            matches!(command, StoragePruneCommand::Apply),
        )?)?,
    }
    Ok(0)
}

#[cfg(target_os = "macos")]
pub(super) fn execute_managed(command: &StorageCommand) -> Result<u8> {
    let arguments = match command {
        StorageCommand::Status => vec!["storage", "status"],
        StorageCommand::Prune {
            command: StoragePruneCommand::Check,
        } => vec!["storage", "prune", "check"],
        StorageCommand::Prune {
            command: StoragePruneCommand::Apply,
        } => vec!["storage", "prune", "apply"],
    };
    let output = crate::managed_vm::ManagedVm::new().forward_storage(&arguments)?;
    super::emit_forwarded(&output)
}
