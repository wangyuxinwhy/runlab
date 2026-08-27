use std::num::NonZeroU64;
use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::{Args, Subcommand, ValueEnum};

use crate::image::{ImageSelector, Images};
use crate::run::{RunId, RunRequest, Runs};
use crate::state::State;

use super::{emit, emit_json_bytes};

#[derive(Debug, Subcommand)]
pub(super) enum RunCommand {
    /// Generate the OCI Runtime Configuration used by a Run.
    Config {
        #[command(subcommand)]
        command: RunConfigCommand,
    },
    /// Start one persistent Run and wait for the Engine to return.
    #[command(
        long_about = "Start one persistent Run and wait for the Engine to return. Success writes a compact JSON summary containing the Run identity, lifecycle, execution facts, process results, final environments, and errors. Use run get for the complete persisted Run record, including captured streams and exact input bytes.",
        after_long_help = "Examples:\n  runlab run start --id 550e8400-e29b-41d4-a716-446655440000 --image agent-base\n  runlab run start --id 550e8400-e29b-41d4-a716-446655440000 --image agent-base --runtime-config config.json --network egress"
    )]
    Start(RunStartArgs),
    /// Read one Run record by identity.
    Get {
        /// Canonical lowercase UUID v4 assigned when the Run was started.
        run_id: RunId,
    },
    /// List Run summaries in reverse acceptance order.
    List {
        /// Maximum number of Run summaries returned; must be between 1 and 100.
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Continue strictly after this canonical Run UUID.
        #[arg(long)]
        after: Option<RunId>,
    },
}

#[derive(Debug, Subcommand)]
pub(super) enum RunConfigCommand {
    /// Write a complete OCI Runtime Configuration to stdout.
    #[command(
        long_about = "Write a complete OCI Runtime Configuration 1.3.0 JSON document to stdout. Image Config Entrypoint, Cmd, Env, WorkingDir, and User are combined with RunLab's fixed Linux execution scaffold. The result contains a new network namespace but no Run Protocol network policy.",
        after_long_help = "The same generator is used when run start omits --runtime-config. Modify the standard JSON with ordinary tools, for example:\n  runlab run config generate --image agent-base >base.json\n  jq '.process.args = [\"python\", \"-m\", \"agent\"]' base.json >config.json"
    )]
    Generate {
        /// Initial OCI Image selected by local name or Manifest digest.
        #[arg(long)]
        image: ImageSelector,
    },
}

#[derive(Debug, Args)]
pub(super) struct RunStartArgs {
    /// Caller-generated canonical lowercase UUID v4 used for idempotent creation.
    #[arg(long)]
    id: RunId,
    /// Initial OCI Image selected by local name or Manifest digest.
    #[arg(long)]
    image: ImageSelector,
    /// Exact OCI Runtime Configuration 1.3.0 JSON file; generated from the Image when omitted.
    #[arg(long, value_name = "FILE")]
    runtime_config: Option<PathBuf>,
    /// Exact bytes delivered to the primary Program; omitted means empty stdin.
    #[arg(long, value_name = "FILE")]
    stdin: Option<PathBuf>,
    /// Optional complete execution timeout in milliseconds.
    #[arg(long)]
    execution_timeout_ms: Option<NonZeroU64>,
    /// Cross-boundary network policy: isolated blocks traffic; egress permits outbound connections and replies.
    #[arg(long, value_enum, default_value_t = NetworkArg::Isolated)]
    network: NetworkArg,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum NetworkArg {
    Isolated,
    Egress,
}

pub(super) fn execute(state_path: &Path, command: RunCommand) -> Result<u8> {
    let state = State::open(state_path)?;
    let images = Images::new(state.oci(), state.database());
    let runs = Runs::new(state.database(), &images);
    match command {
        RunCommand::Config {
            command: RunConfigCommand::Generate { image },
        } => emit_json_bytes(&runs.generate_runtime_config(&image)?)?,
        RunCommand::Start(arguments) => {
            let request = RunRequest {
                run_id: arguments.id,
                image: arguments.image,
                runtime_config: arguments.runtime_config,
                stdin: arguments.stdin,
                execution_timeout_ms: arguments.execution_timeout_ms,
                network: match arguments.network {
                    NetworkArg::Isolated => run_protocol::Network::Isolated,
                    NetworkArg::Egress => run_protocol::Network::Egress,
                },
            };
            emit(&runs.start(&state, &request)?)?;
        }
        RunCommand::Get { run_id } => emit(&runs.get(run_id)?)?,
        RunCommand::List { limit, after } => emit(&runs.list(limit, after)?)?,
    }
    Ok(0)
}

#[cfg(target_os = "macos")]
pub(super) fn execute_managed(command: RunCommand) -> Result<u8> {
    let vm = crate::managed_vm::ManagedVm::new();
    let output = match command {
        RunCommand::Config {
            command: RunConfigCommand::Generate { image },
        } => vm.forward_run_config(&image.to_string())?,
        RunCommand::Start(arguments) => vm.forward_run_start(
            &arguments.id.to_string(),
            &arguments.image.to_string(),
            arguments.runtime_config.as_deref(),
            arguments.stdin.as_deref(),
            arguments.execution_timeout_ms,
            match arguments.network {
                NetworkArg::Isolated => "isolated",
                NetworkArg::Egress => "egress",
            },
        )?,
        RunCommand::Get { run_id } => vm.forward_run_get(&run_id.to_string())?,
        RunCommand::List { limit, after } => {
            let after = after.map(|value| value.to_string());
            vm.forward_run_list(limit, after.as_deref())?
        }
    };
    super::emit_forwarded(&output)
}
