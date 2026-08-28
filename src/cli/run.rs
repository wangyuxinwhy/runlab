#[cfg(not(target_os = "macos"))]
use std::collections::BTreeMap;
#[cfg(not(target_os = "macos"))]
use std::env;
#[cfg(not(target_os = "macos"))]
use std::fs;
use std::num::NonZeroU64;
#[cfg(not(target_os = "macos"))]
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand, ValueEnum};
#[cfg(not(target_os = "macos"))]
use run_protocol::{SecretValue, Secrets};

use crate::image::ImageSelector;
#[cfg(not(target_os = "macos"))]
use crate::image::Images;
use crate::run::RunId;
#[cfg(not(target_os = "macos"))]
use crate::run::{RunRequest, Runs};
#[cfg(not(target_os = "macos"))]
use crate::state::State;

use super::MetadataArgs;
#[cfg(not(target_os = "macos"))]
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
        long_about = "Start one persistent Run and wait for the Engine to return. Optional description and labels are stored as immutable caller-provided Run metadata; they help Agents select Runs, are not execution facts, and are not passed to the Run Engine. Reusing a Run identity requires both the same semantic Run input and the same metadata. stderr emits an NDJSON observation stream with Run stages and Program stdout/stderr while execution is active. Success writes one compact JSON summary to stdout containing the Run identity, metadata, lifecycle, execution facts, process results, final environments, and errors. Use run get for the complete persisted Run record, including captured streams and exact input bytes.",
        after_long_help = "Examples:\n  runlab run start --id 550e8400-e29b-41d4-a716-446655440000 --image agent-base\n  runlab run start --id 550e8400-e29b-41d4-a716-446655440000 --image agent-base --description 'SWE-bench django__django-11099 with pi' --label suite=swe-bench --label agent=pi\n  runlab run start --id 550e8400-e29b-41d4-a716-446655440000 --image agent-base --runtime-config config.json --network egress\n  runlab run start --id 550e8400-e29b-41d4-a716-446655440000 --image agent-base --secret-env API_KEY --secret-file ./auth.json=/run/secrets/auth.json"
    )]
    Start(RunStartArgs),
    /// Read one complete Run record, including caller-provided metadata, by identity.
    Get {
        /// Canonical lowercase UUID v4 assigned when the Run was started.
        run_id: RunId,
    },
    /// List Run summaries and caller-provided metadata in reverse acceptance order.
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
    #[command(flatten)]
    metadata: MetadataArgs,
    /// Exact OCI Runtime Configuration 1.3.0 JSON file; generated from the Image when omitted.
    #[arg(long, value_name = "FILE")]
    runtime_config: Option<PathBuf>,
    /// Exact bytes delivered to the primary Program; omitted means empty stdin.
    #[arg(long, value_name = "FILE")]
    stdin: Option<PathBuf>,
    /// Read one environment variable from the caller and deliver it to the primary Program without retaining its value in the Run record.
    #[arg(long, value_name = "NAME")]
    secret_env: Vec<String>,
    /// Read one host file and expose its exact bytes as a read-only regular file in the primary Program.
    #[arg(long, value_name = "HOST_FILE=CONTAINER_PATH")]
    secret_file: Vec<SecretFileArg>,
    /// Internal managed-VM transport source for a Secret environment value.
    #[cfg(target_os = "linux")]
    #[arg(long, value_name = "NAME=FILE", hide = true)]
    secret_env_file: Vec<SecretEnvFileArg>,
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

#[derive(Clone, Debug)]
pub(crate) struct SecretFileArg {
    pub(crate) source: PathBuf,
    pub(crate) destination: String,
}

impl FromStr for SecretFileArg {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        let (source, destination) = value
            .split_once('=')
            .context("Secret file must use HOST_FILE=CONTAINER_PATH")?;
        if source.is_empty() || destination.is_empty() {
            bail!("Secret file must use non-empty HOST_FILE=CONTAINER_PATH");
        }
        Ok(Self {
            source: PathBuf::from(source),
            destination: destination.to_owned(),
        })
    }
}

#[cfg(target_os = "linux")]
#[derive(Clone, Debug)]
struct SecretEnvFileArg {
    name: String,
    source: PathBuf,
}

#[cfg(target_os = "linux")]
impl FromStr for SecretEnvFileArg {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        let (name, source) = value
            .split_once('=')
            .context("Secret environment source must use NAME=FILE")?;
        if name.is_empty() || source.is_empty() {
            bail!("Secret environment source must use non-empty NAME=FILE");
        }
        Ok(Self {
            name: name.to_owned(),
            source: PathBuf::from(source),
        })
    }
}

#[cfg(not(target_os = "macos"))]
pub(super) fn execute(state_path: &Path, command: RunCommand) -> Result<u8> {
    let state = State::open(state_path)?;
    let images = Images::new(state.oci(), state.database());
    let runs = Runs::new(state.database(), &images);
    match command {
        RunCommand::Config {
            command: RunConfigCommand::Generate { image },
        } => emit_json_bytes(&runs.generate_runtime_config(&image)?)?,
        RunCommand::Start(arguments) => {
            let secrets = resolve_secrets(&arguments)?;
            let request = RunRequest {
                run_id: arguments.id,
                image: arguments.image,
                metadata: arguments.metadata.resolve()?,
                runtime_config: arguments.runtime_config,
                stdin: arguments.stdin,
                secrets,
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
        RunCommand::Start(arguments) => {
            let id = arguments.id.to_string();
            let image = arguments.image.to_string();
            vm.forward_run_start(&crate::managed_vm::ForwardRunStart {
                id: &id,
                image: &image,
                metadata: &arguments.metadata.resolve()?,
                runtime_config: arguments.runtime_config.as_deref(),
                stdin: arguments.stdin.as_deref(),
                secret_env: &arguments.secret_env,
                secret_files: &arguments.secret_file,
                execution_timeout_ms: arguments.execution_timeout_ms,
                network: match arguments.network {
                    NetworkArg::Isolated => "isolated",
                    NetworkArg::Egress => "egress",
                },
            })?
        }
        RunCommand::Get { run_id } => vm.forward_run_get(&run_id.to_string())?,
        RunCommand::List { limit, after } => {
            let after = after.map(|value| value.to_string());
            vm.forward_run_list(limit, after.as_deref())?
        }
    };
    super::emit_forwarded(&output)
}

#[cfg(not(target_os = "macos"))]
fn resolve_secrets(arguments: &RunStartArgs) -> Result<Secrets> {
    let mut environment = BTreeMap::new();
    for name in &arguments.secret_env {
        let value = env::var(name)
            .with_context(|| format!("Secret environment variable is unavailable: {name}"))?;
        if environment
            .insert(name.clone(), SecretValue::new(value.into_bytes()))
            .is_some()
        {
            bail!("Secret environment name is duplicated: {name}");
        }
    }
    #[cfg(target_os = "linux")]
    for source in &arguments.secret_env_file {
        let value = fs::read(&source.source).with_context(|| {
            format!(
                "failed to read Secret environment source {}",
                source.source.display()
            )
        })?;
        if environment
            .insert(source.name.clone(), SecretValue::new(value))
            .is_some()
        {
            bail!("Secret environment name is duplicated: {}", source.name);
        }
    }

    let mut files = BTreeMap::new();
    for source in &arguments.secret_file {
        let value = fs::read(&source.source)
            .with_context(|| format!("failed to read Secret file {}", source.source.display()))?;
        if files
            .insert(source.destination.clone(), SecretValue::new(value))
            .is_some()
        {
            bail!(
                "Secret file destination is duplicated: {}",
                source.destination
            );
        }
    }
    Secrets::new(environment, files).map_err(Into::into)
}
