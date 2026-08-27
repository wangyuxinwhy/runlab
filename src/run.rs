use std::collections::BTreeMap;
#[cfg(target_os = "linux")]
use std::env;
use std::fmt;
use std::fs;
use std::num::NonZeroU64;
use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use chrono::Utc;
use run_engine::{CancellationToken, RunEngine};
use run_protocol::{
    Availability, EngineError, ExecutionInterval, ImageDescriptor, Network, OperationError,
    OperationReport, OperationStage, OperationStatus, ProcessResult, ProgramId, ProgramInput,
    ProgramOutput, RunInput, RunOutput, RuntimeConfig, StopActionResult, StopSignal, StreamFacts,
};
use serde::{Serialize, Serializer};
use serde_json::{Value, json};
use uuid::{Uuid, Version};

use crate::image::{ImageSelector, Images};
use crate::state::State;
use crate::storage::{Database, StoredRun};

const MAX_PAGE_SIZE: usize = 100;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct RunId(Uuid);

impl FromStr for RunId {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        let uuid = Uuid::parse_str(value).context("Run identity must be a UUID v4")?;
        if uuid.get_version() != Some(Version::Random) || value != uuid.hyphenated().to_string() {
            bail!("Run identity must use the canonical lowercase UUID v4 form");
        }
        Ok(Self(uuid))
    }
}

impl fmt::Display for RunId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.hyphenated().fmt(formatter)
    }
}

impl Serialize for RunId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

pub(crate) struct RunRequest {
    pub(crate) run_id: RunId,
    pub(crate) image: ImageSelector,
    pub(crate) runtime_config: Option<PathBuf>,
    pub(crate) stdin: Option<PathBuf>,
    pub(crate) execution_timeout_ms: Option<NonZeroU64>,
    pub(crate) network: Network,
}

pub(crate) struct Runs<'a> {
    database: &'a Database,
    images: &'a Images<'a>,
}

#[derive(Debug, Serialize)]
pub(crate) struct RunStartResult {
    schema_version: u32,
    created: bool,
    run: Value,
}

#[derive(Debug, Serialize)]
pub(crate) struct RunListResult {
    schema_version: u32,
    runs: Vec<RunSummary>,
    next_after: Option<String>,
}

#[derive(Debug, Serialize)]
struct RunSummary {
    run_id: String,
    accepted_at: String,
    lifecycle: &'static str,
    terminal_at: Option<String>,
    completion: Option<String>,
}

impl<'a> Runs<'a> {
    pub(crate) fn new(database: &'a Database, images: &'a Images<'a>) -> Self {
        Self { database, images }
    }

    pub(crate) fn generate_runtime_config(&self, image: &ImageSelector) -> Result<Vec<u8>> {
        let image = self.images.resolve(image)?;
        crate::runtime_config::generate(&image.image_configuration)
    }

    pub(crate) fn start(&self, state: &State, request: &RunRequest) -> Result<RunStartResult> {
        let image = self.images.resolve(&request.image)?;
        let runtime_bytes = request
            .runtime_config
            .as_ref()
            .map(|path| {
                fs::read(path).with_context(|| {
                    format!("failed to read Runtime Configuration {}", path.display())
                })
            })
            .transpose()?
            .map_or_else(
                || crate::runtime_config::generate(&image.image_configuration),
                Ok,
            )?;
        let runtime = RuntimeConfig::parse(runtime_bytes.clone())?;
        let stdin = request
            .stdin
            .as_ref()
            .map(|path| {
                fs::read(path).with_context(|| format!("failed to read stdin {}", path.display()))
            })
            .transpose()?
            .unwrap_or_default();
        let protocol_image = ImageDescriptor::new(image.manifest.clone())?;
        let program = ProgramInput::new(protocol_image, runtime.clone(), stdin.clone())?;
        let input = RunInput::new(
            BTreeMap::from([(ProgramId::primary(), program)]),
            request.execution_timeout_ms,
            request.network,
        )?;
        let input_json = input_json(
            &image.manifest,
            &runtime_bytes,
            &stdin,
            request.execution_timeout_ms,
            request.network,
        )?;
        let identity = input_identity_json(
            &image.manifest,
            runtime.as_json(),
            &stdin,
            request.execution_timeout_ms,
            request.network,
        )?;
        let run_id = request.run_id.to_string();

        let engine = native_engine(state)?;
        let accepted_at = Utc::now().to_rfc3339();
        let created = self
            .database
            .run_insert(&run_id, &accepted_at, &input_json, &identity)?;
        if !created {
            let existing = self
                .database
                .run_get(&run_id)?
                .context("Run disappeared during idempotency check")?;
            if existing.input_identity != identity {
                bail!("Run identity is already bound to a different RunInput: {run_id}");
            }
            return Ok(RunStartResult {
                schema_version: 1,
                created: false,
                run: record_json(&existing),
            });
        }

        let cancellation = CancellationToken::new();
        let signal = SignalCancellation::install(&cancellation)?;
        let result = engine.run(input, cancellation);
        signal.close()?;
        let completion = match result {
            Ok(output) => json!({
                "kind": "engine_returned",
                "result": {
                    "kind": "output",
                    "output": output_json(&output),
                }
            }),
            Err(error) => json!({
                "kind": "engine_returned",
                "result": {
                    "kind": "engine_error",
                    "error": engine_error_json(&error),
                }
            }),
        };
        let terminal_at = Utc::now().to_rfc3339();
        self.database
            .run_complete(&run_id, &terminal_at, &completion)?;
        let record = self
            .database
            .run_get(&run_id)?
            .context("completed Run disappeared")?;
        Ok(RunStartResult {
            schema_version: 1,
            created: true,
            run: record_json(&record),
        })
    }

    pub(crate) fn get(&self, run_id: RunId) -> Result<Value> {
        let run_id = run_id.to_string();
        let record = self
            .database
            .run_get(&run_id)?
            .with_context(|| format!("Run does not exist: {run_id}"))?;
        Ok(record_json(&record))
    }

    pub(crate) fn list(&self, limit: usize, after: Option<RunId>) -> Result<RunListResult> {
        if !(1..=MAX_PAGE_SIZE).contains(&limit) {
            bail!("--limit must be between 1 and {MAX_PAGE_SIZE}");
        }
        let after = after.map(|value| value.to_string());
        let mut records = self.database.run_list(limit + 1, after.as_deref())?;
        let has_more = records.len() > limit;
        if has_more {
            records.truncate(limit);
        }
        let next_after = has_more
            .then(|| records.last().map(|record| record.run_id.clone()))
            .flatten();
        Ok(RunListResult {
            schema_version: 1,
            runs: records.into_iter().map(run_summary).collect(),
            next_after,
        })
    }
}

#[cfg(target_os = "linux")]
fn native_engine(state: &State) -> Result<run_engine::NativeEngine> {
    let runc = executable_in_path("runc")?;
    Ok(run_engine::NativeEngine::new(
        state.oci(),
        state.engine_workspace(),
        runc,
        run_engine::OperationTimeouts::default(),
    ))
}

#[cfg(target_os = "linux")]
fn executable_in_path(name: &str) -> Result<PathBuf> {
    let path = env::var_os("PATH").context("PATH is not set")?;
    for directory in env::split_paths(&path) {
        let candidate = directory.join(name);
        if candidate.is_file() {
            return fs::canonicalize(&candidate)
                .with_context(|| format!("failed to resolve {}", candidate.display()));
        }
    }
    bail!("required executable is not available in PATH: {name}")
}

#[cfg(not(target_os = "linux"))]
fn native_engine(_state: &State) -> Result<UnavailableEngine> {
    bail!("run start requires Linux")
}

#[cfg(not(target_os = "linux"))]
struct UnavailableEngine;

#[cfg(not(target_os = "linux"))]
impl RunEngine for UnavailableEngine {
    fn run(
        &self,
        _input: RunInput,
        _cancellation: CancellationToken,
    ) -> Result<RunOutput, EngineError> {
        unreachable!("unavailable engine is never constructed")
    }
}

fn input_json(
    image: &oci_spec::image::Descriptor,
    runtime: &[u8],
    stdin: &[u8],
    timeout: Option<NonZeroU64>,
    network: Network,
) -> Result<Value> {
    Ok(json!({
        "programs": {
            "primary": {
                "initial_environment": serde_json::to_value(image)?,
                "runtime_config": {
                    "encoding": "base64",
                    "bytes": BASE64.encode(runtime),
                },
                "stdin": {
                    "encoding": "base64",
                    "bytes": BASE64.encode(stdin),
                }
            }
        },
        "execution_timeout_ms": timeout.map(NonZeroU64::get),
        "network": network_name(network),
    }))
}

fn input_identity_json(
    image: &oci_spec::image::Descriptor,
    runtime: &Value,
    stdin: &[u8],
    timeout: Option<NonZeroU64>,
    network: Network,
) -> Result<Value> {
    Ok(json!({
        "programs": {
            "primary": {
                "initial_environment": serde_json::to_value(image)?,
                "runtime_config": runtime,
                "stdin": BASE64.encode(stdin),
            }
        },
        "execution_timeout_ms": timeout.map(NonZeroU64::get),
        "network": network_name(network),
    }))
}

fn network_name(network: Network) -> &'static str {
    match network {
        Network::Isolated => "isolated",
        Network::Egress => "egress",
    }
}

fn record_json(record: &StoredRun) -> Value {
    json!({
        "schema_version": 1,
        "run_id": record.run_id,
        "accepted_at": record.accepted_at,
        "lifecycle": if record.completion.is_some() { "terminal" } else { "accepted" },
        "input": record.input,
        "terminal_at": record.terminal_at,
        "completion": record.completion,
    })
}

fn run_summary(record: StoredRun) -> RunSummary {
    let completion = record
        .completion
        .as_ref()
        .and_then(|value| value.get("result"))
        .and_then(|value| value.get("kind"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    RunSummary {
        run_id: record.run_id,
        accepted_at: record.accepted_at,
        lifecycle: if record.completion.is_some() {
            "terminal"
        } else {
            "accepted"
        },
        terminal_at: record.terminal_at,
        completion,
    }
}

fn engine_error_json(error: &EngineError) -> Value {
    let kind = match error {
        EngineError::InvalidInput { .. } => "invalid_input",
        EngineError::InputUnavailable { .. } => "input_unavailable",
        EngineError::UnsupportedInput { .. } => "unsupported_input",
        EngineError::Internal { .. } => "internal",
    };
    json!({
        "kind": kind,
        "path": error.path().map(ToString::to_string),
        "reason": error.reason(),
    })
}

fn output_json(output: &RunOutput) -> Value {
    let programs = output
        .programs()
        .iter()
        .map(|(id, output)| (id.as_str().to_owned(), program_output_json(output)))
        .collect::<serde_json::Map<_, _>>();
    json!({
        "execution": execution_json(output),
        "programs": programs,
    })
}

fn execution_json(output: &RunOutput) -> Value {
    let execution = output.execution();
    let interval = match execution.interval() {
        ExecutionInterval::NotEntered { reason } => json!({
            "kind": "not_entered",
            "reason": reason.as_str(),
        }),
        ExecutionInterval::Entered {
            started_at,
            ended_at,
        } => json!({
            "kind": "entered",
            "started_at": started_at.to_rfc3339(),
            "ended_at": ended_at.to_rfc3339(),
        }),
    };
    json!({
        "interval": interval,
        "timed_out": execution.timed_out(),
        "cancelled": execution.cancelled(),
        "errors": execution.errors().map(operation_error_json).collect::<Vec<_>>(),
    })
}

fn program_output_json(output: &ProgramOutput) -> Value {
    json!({
        "create": report_json(output.create(), |facts| json!({
            "completed_at": facts.completed_at().to_rfc3339(),
        })),
        "start": report_json(output.start(), |facts| json!({
            "started_at": facts.started_at().to_rfc3339(),
        })),
        "process": process_json(output.process()),
        "stdin": {
            "write": report_json(output.stdin().write(), |facts| json!({
                "bytes_written": facts.bytes_written(),
            })),
            "close": report_json(output.stdin().close(), |()| Value::Null),
        },
        "stdout": report_json(output.stdout(), stream_json),
        "stderr": report_json(output.stderr(), stream_json),
        "stop_actions": output.stop_actions().iter().map(|action| json!({
            "signal": match action.signal() {
                StopSignal::Term => "term",
                StopSignal::Kill => "kill",
            },
            "attempted_at": action.attempted_at().to_rfc3339(),
            "result": stop_result_json(action.result()),
        })).collect::<Vec<_>>(),
        "final_environment": availability_json(
            output.final_environment(),
            |image| serde_json::to_value(image.as_oci()).expect("Descriptor serialization"),
        ),
        "errors": output.errors().map(operation_error_json).collect::<Vec<_>>(),
    })
}

fn report_json<T>(report: &OperationReport<T>, facts: impl FnOnce(&T) -> Value) -> Value {
    json!({
        "status": status_name(report.status()),
        "facts": report.facts().map(facts),
        "reason": report.reason(),
    })
}

fn status_name(status: OperationStatus) -> &'static str {
    match status {
        OperationStatus::NotAttempted => "not_attempted",
        OperationStatus::Succeeded => "succeeded",
        OperationStatus::Failed => "failed",
        OperationStatus::Unknown => "unknown",
    }
}

fn stream_json(stream: &StreamFacts) -> Value {
    json!({
        "bytes": {
            "encoding": "base64",
            "value": BASE64.encode(stream.bytes()),
        },
        "omitted_after_limit": stream.omitted_after_limit(),
        "eof": stream.eof(),
    })
}

fn process_json(process: &ProcessResult) -> Value {
    match process {
        ProcessResult::NeverStarted { reason } => json!({
            "kind": "never_started",
            "reason": reason.as_str(),
        }),
        ProcessResult::Exited { code, ended_at } => json!({
            "kind": "exited",
            "code": code,
            "ended_at": ended_at.to_rfc3339(),
        }),
        ProcessResult::Signaled { signal, ended_at } => json!({
            "kind": "signaled",
            "signal": signal.get(),
            "ended_at": ended_at.to_rfc3339(),
        }),
        ProcessResult::Unknown { reason, ended_at } => json!({
            "kind": "unknown",
            "reason": reason.as_str(),
            "ended_at": availability_json(ended_at, |time| Value::String(time.to_rfc3339())),
        }),
    }
}

fn availability_json<T>(value: &Availability<T>, available: impl FnOnce(&T) -> Value) -> Value {
    match value {
        Availability::Available(value) => json!({
            "availability": "available",
            "value": available(value),
        }),
        Availability::Unavailable(reason) => json!({
            "availability": "unavailable",
            "reason": reason.as_str(),
        }),
    }
}

fn stop_result_json(result: &StopActionResult) -> Value {
    match result {
        StopActionResult::Accepted => json!({"status": "accepted"}),
        StopActionResult::Rejected(_) => json!({"status": "rejected"}),
        StopActionResult::Unknown { reason, .. } => json!({
            "status": "unknown",
            "reason": reason.as_str(),
        }),
    }
}

fn operation_error_json(error: &OperationError) -> Value {
    json!({
        "observed_at": error.observed_at().to_rfc3339(),
        "stage": operation_stage_name(error.stage()),
        "message": error.message(),
        "code": error.code(),
    })
}

fn operation_stage_name(stage: OperationStage) -> &'static str {
    match stage {
        OperationStage::Preparation => "preparation",
        OperationStage::Create => "create",
        OperationStage::Start => "start",
        OperationStage::ProcessSupervision => "process_supervision",
        OperationStage::StdinWrite => "stdin_write",
        OperationStage::StdinClose => "stdin_close",
        OperationStage::StdoutRead => "stdout_read",
        OperationStage::StderrRead => "stderr_read",
        OperationStage::Signal => "signal",
        OperationStage::Wait => "wait",
        OperationStage::RuntimeFilesystemRemoval => "runtime_filesystem_removal",
        OperationStage::FinalEnvironmentCapture => "final_environment_capture",
        OperationStage::Cleanup => "cleanup",
        OperationStage::Coordination => "coordination",
        OperationStage::Timing => "timing",
    }
}

struct SignalCancellation {
    #[cfg(unix)]
    handle: signal_hook::iterator::Handle,
    #[cfg(unix)]
    thread: std::thread::JoinHandle<()>,
}

impl SignalCancellation {
    fn install(cancellation: &CancellationToken) -> Result<Self> {
        #[cfg(unix)]
        {
            let mut signals = signal_hook::iterator::Signals::new([
                signal_hook::consts::SIGINT,
                signal_hook::consts::SIGTERM,
            ])?;
            let handle = signals.handle();
            let cancellation = cancellation.clone();
            let thread = std::thread::spawn(move || {
                if signals.forever().next().is_some() {
                    cancellation.cancel();
                }
            });
            Ok(Self { handle, thread })
        }
        #[cfg(not(unix))]
        {
            let _ = cancellation;
            Ok(Self {})
        }
    }

    fn close(self) -> Result<()> {
        #[cfg(unix)]
        {
            self.handle.close();
            self.thread
                .join()
                .map_err(|_| anyhow::anyhow!("signal handler thread panicked"))?;
        }
        Ok(())
    }
}
