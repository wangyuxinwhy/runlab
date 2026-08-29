#[cfg(not(target_os = "macos"))]
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::num::NonZeroU64;
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::str::FromStr;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand, ValueEnum};
#[cfg(not(target_os = "macos"))]
use run_protocol::{SecretValue, Secrets};
use serde::Serialize;

use crate::image::ImageSelector;
#[cfg(not(target_os = "macos"))]
use crate::image::Images;
use crate::run::RunId;
#[cfg(not(target_os = "macos"))]
use crate::run::{ExecutionRequest, RunRequest, Runs};
use crate::run_deletion::OperationId;
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
    /// Start one persistent Run, optionally returning after acceptance.
    #[command(
        long_about = "Start one persistent Run. By default the CLI waits for the Engine to return and streams NDJSON observations: stderr emits an NDJSON observation stream and stdout receives one compact JSON summary. Use run get for the complete persisted Run record. --detach starts the same Coordinator in an independent process group, waits until Run acceptance is observable, then returns the Run ID and recovery command without streaming Program output; use run get or query afterwards. The selected Catalog name, description, and labels are immutable caller facts and are not execution facts. Reusing a Run identity requires the same semantic input and metadata.",
        after_long_help = "Examples:\n  runlab run start --id 550e8400-e29b-41d4-a716-446655440000 --image agent-base\n  runlab run start --detach --id 550e8400-e29b-41d4-a716-446655440000 --image agent-base\n  runlab run start --id 550e8400-e29b-41d4-a716-446655440000 --image agent-base --description 'SWE-bench django__django-11099 with pi' --label suite=swe-bench --label agent=pi\n  runlab run start --id 550e8400-e29b-41d4-a716-446655440000 --image agent-base --runtime-config config.json --network egress\n  runlab run start --id 550e8400-e29b-41d4-a716-446655440000 --image agent-base --secret-env API_KEY --secret-file ./auth.json=/run/secrets/auth.json"
    )]
    Start(Box<RunStartArgs>),
    /// Request cancellation of one active persistent Run.
    #[command(
        long_about = "Persist an idempotent cancellation request for one Run. If the Run is active, its Coordinator delivers the request to the current Engine invocation. Success confirms the request was stored, not that execution has already stopped; use run get for the final RunOutput cancellation and stop facts. A terminal Run is returned unchanged.",
        after_long_help = "Example:\n  runlab run cancel 550e8400-e29b-41d4-a716-446655440000"
    )]
    Cancel {
        /// Canonical lowercase UUID v4 of the Run to cancel.
        run_id: RunId,
    },
    /// Reconcile one non-terminal Run from durable execution evidence.
    Reconcile {
        /// Canonical lowercase UUID v4 of the Run to reconcile.
        run_id: RunId,
    },
    /// Permanently retire complete Run assets through a checked, idempotent plan.
    Delete {
        #[command(subcommand)]
        command: RunDeleteCommand,
    },
    /// Read one complete Run record, including caller-provided metadata, by identity.
    Get {
        /// Canonical lowercase UUID v4 assigned when the Run was started.
        run_id: RunId,
    },
    /// List up to 10 recent Run summaries by default; use query run for selection or aggregation.
    List {
        /// Maximum number of Run summaries returned; must be between 1 and 100.
        #[arg(long, default_value_t = 10)]
        limit: usize,
        /// Continue strictly after this canonical Run UUID.
        #[arg(long)]
        after: Option<RunId>,
    },
}

#[derive(Debug, Subcommand)]
pub(super) enum RunDeleteCommand {
    /// Validate a bounded Run ID set and write an auditable deletion plan to stdout.
    #[command(
        long_about = "Validate a bounded caller-selected Run ID set without deleting anything. The caller-owned operation ID identifies this deletion intent across retries; each candidate fingerprint freezes the database fact that apply will verify. Use storage prune check --without-runs first when reclaimable content matters.",
        after_long_help = "Example:\n  runlab run delete check --operation-id 550e8400-e29b-41d4-a716-446655440000 --ids run-ids.txt >delete-plan.json"
    )]
    Check {
        /// Caller-owned canonical UUID v4; reuse it for every retry of this deletion intent.
        #[arg(long)]
        operation_id: OperationId,
        /// Newline-delimited canonical Run UUIDs; use - for stdin, at most 1000 entries.
        #[arg(long, value_name = "FILE")]
        ids: PathBuf,
    },
    /// Atomically delete every candidate in a checked plan; use - to read the plan from stdin.
    #[command(
        long_about = "Apply one checked Run deletion plan in a short SQLite write transaction. The candidate set is all-or-nothing. The operation ID makes retry after a lost success response idempotent; candidate fingerprints reject stale records. OCI blobs and snapshot cache are not removed by this command.",
        after_long_help = "Example:\n  runlab run delete apply --plan delete-plan.json\n  runlab run delete check --operation-id 550e8400-e29b-41d4-a716-446655440000 --ids run-ids.txt | runlab run delete apply --plan -"
    )]
    Apply {
        /// Exact JSON plan emitted by run delete check; use - for stdin.
        #[arg(long, value_name = "FILE")]
        plan: PathBuf,
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
    /// Return after the Run is accepted; continue with run get or query run.
    #[arg(long)]
    detach: bool,
    /// Internal child process that owns a detached Run.
    #[arg(long, hide = true)]
    detached_worker: bool,
    #[command(flatten)]
    execution: ExecutionArgs,
    #[command(flatten)]
    metadata: MetadataArgs,
}

#[derive(Debug, Args)]
pub(super) struct ExecArgs {
    #[command(flatten)]
    execution: ExecutionArgs,
}

#[derive(Debug, Args)]
struct ExecutionArgs {
    /// Initial OCI Image selected by local name or Manifest digest.
    #[arg(long)]
    image: ImageSelector,
    /// Exact OCI Runtime Configuration 1.3.0 JSON file; generated from the Image when omitted. On macOS, non-scaffold bind sources are local Host paths and must be read-only regular files or directories.
    #[arg(long, value_name = "FILE")]
    runtime_config: Option<PathBuf>,
    /// Exact bytes delivered to the primary Program; omitted means empty stdin.
    #[arg(long, value_name = "FILE")]
    stdin: Option<PathBuf>,
    /// Read one environment variable from the caller and deliver it to the primary Program.
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

#[derive(Debug, Serialize)]
struct DetachedStartResult {
    schema_version: u32,
    detached: bool,
    created: bool,
    run_id: String,
    lifecycle: &'static str,
    recovery: String,
}

fn execute_detached(
    run_id: &str,
    mut lookup: impl FnMut() -> Result<Option<&'static str>>,
) -> Result<u8> {
    let preexisting = lookup()?.is_some();
    let stdout = tempfile::NamedTempFile::new().context("failed to stage detached stdout")?;
    let stderr = tempfile::NamedTempFile::new().context("failed to stage detached stderr")?;
    let mut command =
        Command::new(env::current_exe().context("current executable is unavailable")?);
    command
        .args(env::args_os().skip(1))
        .arg("--detached-worker")
        .stdin(Stdio::null())
        .stdout(stdout.reopen()?)
        .stderr(stderr.reopen()?)
        .process_group(0);
    let mut child = command
        .spawn()
        .context("failed to start detached Run worker")?;
    let deadline = Instant::now() + Duration::from_mins(2);
    loop {
        if let Some(status) = child
            .try_wait()
            .context("failed to inspect detached Run worker")?
        {
            return detached_child_result(status.success(), stdout.path(), stderr.path());
        }
        if !preexisting && detached_ready(stderr.path(), run_id)? {
            super::emit(&DetachedStartResult {
                schema_version: 1,
                detached: true,
                created: true,
                run_id: run_id.to_owned(),
                lifecycle: "accepted",
                recovery: format!("runlab run get {run_id}"),
            })?;
            return Ok(0);
        }
        if !preexisting && let Some(lifecycle) = lookup()? {
            super::emit(&DetachedStartResult {
                schema_version: 1,
                detached: true,
                created: true,
                run_id: run_id.to_owned(),
                lifecycle,
                recovery: format!("runlab run get {run_id}"),
            })?;
            return Ok(0);
        }
        if Instant::now() >= deadline {
            return Err(crate::error::classify(
                anyhow::anyhow!("timed out waiting for detached Run acceptance"),
                crate::error::ErrorFacts {
                    category: crate::error::ErrorCategory::Unavailable,
                    stage: "acceptance_wait",
                    run_id: Some(run_id.to_owned()),
                    accepted: None,
                    run_created: None,
                    retryable: true,
                    recovery: Some(format!("runlab run get {run_id}")),
                },
            ));
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn detached_ready(path: &Path, run_id: &str) -> Result<bool> {
    let bytes = fs::read(path)?;
    Ok(bytes.split(|byte| *byte == b'\n').any(|line| {
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(line) else {
            return false;
        };
        value.get("kind").and_then(serde_json::Value::as_str) == Some("transport.detached_ready")
            && value.get("run_id").and_then(serde_json::Value::as_str) == Some(run_id)
    }))
}

fn detached_child_result(success: bool, stdout: &Path, stderr: &Path) -> Result<u8> {
    if success {
        let mut value: serde_json::Value = serde_json::from_slice(&fs::read(stdout)?)
            .context("detached Run worker returned invalid JSON")?;
        value["detached"] = serde_json::Value::Bool(true);
        super::emit(&value)?;
        return Ok(0);
    }
    let stderr = fs::read(stderr)?;
    if let Some(error) = crate::error::parse_remote(&stderr, false) {
        return Err(error.into());
    }
    bail!(
        "detached Run worker failed: {}",
        String::from_utf8_lossy(&stderr).trim()
    )
}

#[cfg(not(target_os = "macos"))]
pub(super) fn execute(state_path: &Path, command: RunCommand) -> Result<u8> {
    if let RunCommand::Delete {
        command: RunDeleteCommand::Apply { plan },
    } = &command
    {
        return execute_delete_apply(state_path, plan);
    }
    let state = State::open(state_path)?;
    let images = Images::new(state.oci(), state.database());
    let runs = Runs::new(state.database(), &images);
    match command {
        RunCommand::Config {
            command: RunConfigCommand::Generate { image },
        } => emit_json_bytes(&runs.generate_runtime_config(&image)?)?,
        RunCommand::Start(arguments) => {
            let arguments = *arguments;
            if arguments.detach && !arguments.detached_worker {
                let run_id = arguments.id.to_string();
                return execute_detached(&run_id, || {
                    Ok(state.database().run_get(&run_id)?.map(|record| {
                        if record.completion.is_some() {
                            "terminal"
                        } else {
                            "accepted"
                        }
                    }))
                });
            }
            let secrets = resolve_secrets(&arguments.execution)?;
            let request = RunRequest {
                run_id: arguments.id,
                metadata: arguments.metadata.resolve()?,
                execution: execution_request(arguments.execution, secrets),
            };
            let result = if arguments.detached_worker {
                runs.start_detached_worker(&state, &request)?
            } else {
                runs.start(&state, &request)?
            };
            emit(&result)?;
        }
        RunCommand::Cancel { run_id } => emit(&runs.cancel(run_id)?)?,
        RunCommand::Reconcile { run_id } => emit(&runs.reconcile(run_id)?)?,
        RunCommand::Delete { command } => match command {
            RunDeleteCommand::Check { operation_id, ids } => {
                let run_ids = super::input::read_required_run_ids(&ids)
                    .map_err(|error| crate::error::invalid_input(error, "run_delete_input"))?;
                emit(&crate::run_deletion::check(
                    state.database(),
                    operation_id,
                    &run_ids,
                )?)?;
            }
            RunDeleteCommand::Apply { .. } => {
                unreachable!("Run deletion apply is handled before opening State")
            }
        },
        RunCommand::Get { run_id } => emit(&runs.get(run_id)?)?,
        RunCommand::List { limit, after } => emit(&runs.list(limit, after)?)?,
    }
    Ok(0)
}

#[cfg(not(target_os = "macos"))]
fn execute_delete_apply(state_path: &Path, plan: &Path) -> Result<u8> {
    let bytes = super::input::read_bounded(plan, 8 * 1024 * 1024, "Run deletion plan")
        .map_err(|error| crate::error::invalid_input(error, "run_delete_input"))?;
    let plan = crate::run_deletion::parse_plan(&bytes)
        .map_err(|error| crate::error::invalid_input(error, "run_delete_input"))?;
    let state = State::open(state_path)
        .map_err(|error| crate::run_deletion::classify_open_error(error, plan.operation_id()))?;
    emit(&crate::run_deletion::apply(state.database(), plan)?)?;
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
            let arguments = *arguments;
            let id = arguments.id.to_string();
            if arguments.detach && !arguments.detached_worker {
                return execute_detached(&id, || match vm.forward_run_get(&id) {
                    Ok(output) => {
                        let record: serde_json::Value = serde_json::from_slice(&output.stdout)
                            .context("managed VM returned invalid Run JSON")?;
                        Ok(Some(
                            if record.get("lifecycle").and_then(serde_json::Value::as_str)
                                == Some("terminal")
                            {
                                "terminal"
                            } else {
                                "accepted"
                            },
                        ))
                    }
                    Err(error) if crate::error::is_not_found(&error) => Ok(None),
                    Err(error) => Err(error),
                });
            }
            let image = arguments.execution.image.to_string();
            vm.forward_run_start(&crate::managed_vm::ForwardRunStart {
                id: &id,
                detached_worker: arguments.detached_worker,
                image: &image,
                metadata: &arguments.metadata.resolve()?,
                runtime_config: arguments.execution.runtime_config.as_deref(),
                stdin: arguments.execution.stdin.as_deref(),
                secret_env: &arguments.execution.secret_env,
                secret_files: &arguments.execution.secret_file,
                execution_timeout_ms: arguments.execution.execution_timeout_ms,
                network: match arguments.execution.network {
                    NetworkArg::Isolated => "isolated",
                    NetworkArg::Egress => "egress",
                },
            })?
        }
        RunCommand::Cancel { run_id } => vm.forward_run_cancel(&run_id.to_string())?,
        RunCommand::Reconcile { run_id } => vm.forward_run_reconcile(&run_id.to_string())?,
        RunCommand::Delete { command } => match command {
            RunDeleteCommand::Check { operation_id, ids } => {
                let run_ids = super::input::read_required_run_ids(&ids)
                    .map_err(|error| crate::error::invalid_input(error, "run_delete_input"))?;
                vm.forward_run_delete_check(&operation_id.to_string(), &run_ids)?
            }
            RunDeleteCommand::Apply { plan } => {
                let bytes = super::input::read_bounded(&plan, 8 * 1024 * 1024, "Run deletion plan")
                    .map_err(|error| crate::error::invalid_input(error, "run_delete_input"))?;
                crate::run_deletion::parse_plan(&bytes)
                    .map_err(|error| crate::error::invalid_input(error, "run_delete_input"))?;
                vm.forward_run_delete_apply(&bytes)?
            }
        },
        RunCommand::Get { run_id } => vm.forward_run_get(&run_id.to_string())?,
        RunCommand::List { limit, after } => {
            let after = after.map(|value| value.to_string());
            vm.forward_run_list(limit, after.as_deref())?
        }
    };
    super::emit_forwarded(&output)
}

#[cfg(not(target_os = "macos"))]
pub(super) fn execute_exec(state_path: &Path, arguments: ExecArgs) -> Result<u8> {
    let state = State::open(state_path)?;
    let images = Images::new(state.oci(), state.database());
    let runs = Runs::new(state.database(), &images);
    let secrets = resolve_secrets(&arguments.execution)?;
    emit(&runs.exec(&state, &execution_request(arguments.execution, secrets))?)?;
    Ok(0)
}

#[cfg(target_os = "macos")]
pub(super) fn execute_exec_managed(arguments: ExecArgs) -> Result<u8> {
    let ExecArgs { execution } = arguments;
    let ExecutionArgs {
        image,
        runtime_config,
        stdin,
        secret_env,
        secret_file,
        execution_timeout_ms,
        network,
    } = execution;
    let image = image.to_string();
    let output =
        crate::managed_vm::ManagedVm::new().forward_exec(&crate::managed_vm::ForwardExecution {
            image: &image,
            runtime_config: runtime_config.as_deref(),
            stdin: stdin.as_deref(),
            secret_env: &secret_env,
            secret_files: &secret_file,
            execution_timeout_ms,
            network: match network {
                NetworkArg::Isolated => "isolated",
                NetworkArg::Egress => "egress",
            },
        })?;
    super::emit_forwarded(&output)
}

#[cfg(not(target_os = "macos"))]
fn execution_request(arguments: ExecutionArgs, secrets: Secrets) -> ExecutionRequest {
    ExecutionRequest {
        image: arguments.image,
        runtime_config: arguments.runtime_config,
        stdin: arguments.stdin,
        secrets,
        execution_timeout_ms: arguments.execution_timeout_ms,
        network: match arguments.network {
            NetworkArg::Isolated => run_protocol::Network::Isolated,
            NetworkArg::Egress => run_protocol::Network::Egress,
        },
    }
}

#[cfg(not(target_os = "macos"))]
fn resolve_secrets(arguments: &ExecutionArgs) -> Result<Secrets> {
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
