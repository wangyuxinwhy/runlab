use std::collections::BTreeMap;
#[cfg(target_os = "linux")]
use std::env;
use std::fmt;
use std::fs;
use std::num::NonZeroU64;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use chrono::Utc;
use run_engine::CancellationToken;
#[cfg(not(target_os = "linux"))]
use run_engine::RunEngine;
use run_protocol::{
    Availability, EngineError, ExecutionInterval, ImageDescriptor, Network, OperationError,
    OperationReport, OperationStage, OperationStatus, ProcessResult, ProgramId, ProgramInput,
    ProgramOutput, RunInput, RunOutput, RuntimeConfig, Secrets, StopActionResult, StopSignal,
    StreamFacts,
};
use serde::{Serialize, Serializer};
use serde_json::{Value, json};
use uuid::{Uuid, Version};

use crate::image::{ImageSelector, Images};
use crate::metadata::Metadata;
use crate::observation::RunObservation;
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
    pub(crate) metadata: Metadata,
    pub(crate) runtime_config: Option<PathBuf>,
    pub(crate) stdin: Option<PathBuf>,
    pub(crate) secrets: Secrets,
    pub(crate) execution_timeout_ms: Option<NonZeroU64>,
    pub(crate) network: Network,
}

pub(crate) struct Runs<'a> {
    database: &'a Database,
    images: &'a Images<'a>,
}

struct PreparedRun {
    input: RunInput,
    input_json: Value,
    identity: Value,
}

#[derive(Debug, Serialize)]
pub(crate) struct RunStartResult {
    schema_version: u32,
    created: bool,
    run_id: String,
    metadata: Metadata,
    lifecycle: &'static str,
    completion: Option<Value>,
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
    metadata: Metadata,
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
        let prepared = self.prepare(request)?;
        let run_id = request.run_id.to_string();
        let engine = native_engine(state)?;
        let accepted_at = Utc::now().to_rfc3339();
        let created = self.database.run_insert(
            &run_id,
            &accepted_at,
            &request.metadata,
            &prepared.input_json,
            &prepared.identity,
        )?;
        if !created {
            let existing = self
                .database
                .run_get(&run_id)?
                .context("Run disappeared during idempotency check")?;
            if !matches_request(&existing, &prepared.identity, &request.metadata) {
                bail!("Run identity is already bound to a different request: {run_id}");
            }
            let observation = RunObservation::stderr(&run_id);
            observation.finish();
            return start_result(&existing, false);
        }

        let observation = RunObservation::stderr(&run_id);
        observation.stage("accepted");
        observation.stage("preparing");
        let cancellation = CancellationToken::new();
        let signal = SignalCancellation::install(&cancellation)?;
        let input = prepared.input;
        let result = run_observed(
            &engine,
            &input,
            &cancellation,
            observation.engine_observer(),
        );
        signal.close()?;
        observation.report_dropped_observation();
        observation.stage("publishing");
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
        observation.stage("terminal");
        observation.finish();
        let record = self
            .database
            .run_get(&run_id)?
            .context("completed Run disappeared")?;
        start_result(&record, true)
    }

    fn prepare(&self, request: &RunRequest) -> Result<PreparedRun> {
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
        let program = ProgramInput::new(
            protocol_image,
            runtime.clone(),
            stdin.clone(),
            request.secrets.clone(),
        )?;
        let input = RunInput::new(
            BTreeMap::from([(ProgramId::primary(), program)]),
            request.execution_timeout_ms,
            request.network,
        )?;
        let input_json = input_json(
            &image.manifest,
            &runtime_bytes,
            &stdin,
            &request.secrets,
            request.execution_timeout_ms,
            request.network,
        )?;
        let identity = input_identity_json(
            &image.manifest,
            runtime.as_json(),
            &stdin,
            &request.secrets,
            request.execution_timeout_ms,
            request.network,
        )?;
        Ok(PreparedRun {
            input,
            input_json,
            identity,
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

fn matches_request(existing: &StoredRun, identity: &Value, metadata: &Metadata) -> bool {
    existing.input_identity == *identity && existing.metadata == *metadata
}

#[cfg(target_os = "linux")]
fn run_observed(
    engine: &run_engine::NativeEngine,
    input: &RunInput,
    cancellation: &CancellationToken,
    observer: Arc<dyn run_engine::EngineObserver>,
) -> Result<RunOutput, EngineError> {
    engine.run_observed(input, cancellation, observer)
}

#[cfg(not(target_os = "linux"))]
fn run_observed(
    _engine: &UnavailableEngine,
    _input: &RunInput,
    _cancellation: &CancellationToken,
    _observer: Arc<dyn run_engine::EngineObserver>,
) -> Result<RunOutput, EngineError> {
    unreachable!("unavailable engine is never constructed")
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
    secrets: &Secrets,
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
                },
                "secrets": redacted_secrets(secrets),
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
    secrets: &Secrets,
    timeout: Option<NonZeroU64>,
    network: Network,
) -> Result<Value> {
    Ok(json!({
        "programs": {
            "primary": {
                "initial_environment": serde_json::to_value(image)?,
                "runtime_config": runtime,
                "stdin": BASE64.encode(stdin),
                "secrets": secret_identity(secrets),
            }
        },
        "execution_timeout_ms": timeout.map(NonZeroU64::get),
        "network": network_name(network),
    }))
}

fn redacted_secrets(secrets: &Secrets) -> Value {
    json!({
        "env": secrets
            .env()
            .keys()
            .map(|name| (name.clone(), json!({"retained": false})))
            .collect::<serde_json::Map<_, _>>(),
        "files": secrets
            .files()
            .keys()
            .map(|path| (path.clone(), json!({"retained": false})))
            .collect::<serde_json::Map<_, _>>(),
    })
}

fn secret_identity(secrets: &Secrets) -> Value {
    json!({
        "env": secrets
            .env()
            .iter()
            .map(|(name, value)| (name.clone(), Value::String(sha256_digest(value.as_bytes()))))
            .collect::<serde_json::Map<_, _>>(),
        "files": secrets
            .files()
            .iter()
            .map(|(path, value)| (path.clone(), Value::String(sha256_digest(value.as_bytes()))))
            .collect::<serde_json::Map<_, _>>(),
    })
}

fn sha256_digest(bytes: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};
    use std::fmt::Write as _;

    let mut encoded = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        write!(encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    format!("sha256:{encoded}")
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
        "metadata": record.metadata,
        "accepted_at": record.accepted_at,
        "lifecycle": if record.completion.is_some() { "terminal" } else { "accepted" },
        "input": record.input,
        "terminal_at": record.terminal_at,
        "completion": record.completion,
    })
}

fn start_result(record: &StoredRun, created: bool) -> Result<RunStartResult> {
    Ok(RunStartResult {
        schema_version: 1,
        created,
        run_id: record.run_id.clone(),
        metadata: record.metadata.clone(),
        lifecycle: if record.completion.is_some() {
            "terminal"
        } else {
            "accepted"
        },
        completion: record
            .completion
            .as_ref()
            .map(completion_summary)
            .transpose()?,
    })
}

fn completion_summary(completion: &Value) -> Result<Value> {
    let completion_kind = completion
        .get("kind")
        .and_then(Value::as_str)
        .context("persisted Run completion has no kind")?;
    if completion_kind != "engine_returned" {
        bail!("persisted Run completion has unsupported kind: {completion_kind}");
    }

    let result = completion
        .get("result")
        .and_then(Value::as_object)
        .context("persisted engine completion has no result")?;
    match result.get("kind").and_then(Value::as_str) {
        Some("output") => output_summary(
            result
                .get("output")
                .context("persisted engine output is missing")?,
        ),
        Some("engine_error") => Ok(json!({
            "kind": "engine_error",
            "error": result
                .get("error")
                .context("persisted EngineError is missing")?,
        })),
        Some(kind) => bail!("persisted engine result has unsupported kind: {kind}"),
        None => bail!("persisted engine result has no kind"),
    }
}

fn output_summary(output: &Value) -> Result<Value> {
    let execution = output
        .get("execution")
        .and_then(Value::as_object)
        .context("persisted RunOutput has no execution facts")?;
    let programs = output
        .get("programs")
        .and_then(Value::as_object)
        .context("persisted RunOutput has no programs")?;
    let program_summaries = programs
        .iter()
        .map(|(program_id, program)| {
            let program = program
                .as_object()
                .with_context(|| format!("persisted ProgramOutput is invalid: {program_id}"))?;
            Ok((
                program_id.clone(),
                json!({
                    "process": program
                        .get("process")
                        .with_context(|| format!("persisted ProgramOutput has no process facts: {program_id}"))?,
                    "final_environment": program
                        .get("final_environment")
                        .with_context(|| format!("persisted ProgramOutput has no final environment: {program_id}"))?,
                    "errors": program
                        .get("errors")
                        .with_context(|| format!("persisted ProgramOutput has no errors: {program_id}"))?,
                }),
            ))
        })
        .collect::<Result<serde_json::Map<_, _>>>()?;

    Ok(json!({
        "kind": "output",
        "execution": execution,
        "programs": program_summaries,
    }))
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
        metadata: record.metadata,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persisted_input_redacts_secret_bytes_but_identity_distinguishes_them() {
        let image: oci_spec::image::Descriptor = serde_json::from_value(json!({
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "size": 1
        }))
        .expect("descriptor");
        let first = Secrets::new(
            BTreeMap::from([(
                "TOKEN".to_owned(),
                run_protocol::SecretValue::new(b"plain-environment-secret".to_vec()),
            )]),
            BTreeMap::from([(
                "/run/secret".to_owned(),
                run_protocol::SecretValue::new(b"plain-file-secret".to_vec()),
            )]),
        )
        .expect("Secrets");
        let second = Secrets::new(
            BTreeMap::from([(
                "TOKEN".to_owned(),
                run_protocol::SecretValue::new(b"different".to_vec()),
            )]),
            BTreeMap::from([(
                "/run/secret".to_owned(),
                run_protocol::SecretValue::new(b"plain-file-secret".to_vec()),
            )]),
        )
        .expect("Secrets");

        let record =
            input_json(&image, b"{}", b"", &first, None, Network::Isolated).expect("record input");
        let encoded = serde_json::to_string(&record).expect("record JSON");
        assert!(!encoded.contains("plain-environment-secret"));
        assert!(!encoded.contains("plain-file-secret"));
        assert_eq!(
            record["programs"]["primary"]["secrets"]["env"]["TOKEN"]["retained"],
            false
        );

        let first_identity =
            input_identity_json(&image, &json!({}), b"", &first, None, Network::Isolated)
                .expect("first identity");
        let second_identity =
            input_identity_json(&image, &json!({}), b"", &second, None, Network::Isolated)
                .expect("second identity");
        assert_ne!(first_identity, second_identity);
        let identity = serde_json::to_string(&first_identity).expect("identity JSON");
        assert!(!identity.contains("plain-environment-secret"));
        assert!(!identity.contains("plain-file-secret"));
    }

    #[test]
    fn start_result_keeps_execution_facts_without_record_payloads() {
        let completion = json!({
            "kind": "engine_returned",
            "result": {
                "kind": "output",
                "output": {
                    "execution": {
                        "interval": {"kind": "entered"},
                        "timed_out": false,
                        "cancelled": false,
                        "errors": [],
                    },
                    "programs": {
                        "primary": {
                            "create": {"status": "succeeded"},
                            "start": {"status": "succeeded"},
                            "process": {"kind": "exited", "code": 0},
                            "stdin": {"write": {"status": "succeeded"}},
                            "stdout": {"facts": {"bytes": {"value": "large-output"}}},
                            "stderr": {"facts": {"bytes": {"value": "large-error"}}},
                            "final_environment": {
                                "availability": "available",
                                "value": {"digest": "sha256:final"},
                            },
                            "errors": [],
                        }
                    }
                }
            }
        });
        let record = stored_run(Some(completion));

        let value = serde_json::to_value(start_result(&record, true).expect("start result"))
            .expect("start result JSON");

        assert_eq!(
            value,
            json!({
                "schema_version": 1,
                "created": true,
                "run_id": "550e8400-e29b-41d4-a716-446655440000",
                "metadata": {
                    "description": null,
                    "labels": {},
                },
                "lifecycle": "terminal",
                "completion": {
                    "kind": "output",
                    "execution": {
                        "interval": {"kind": "entered"},
                        "timed_out": false,
                        "cancelled": false,
                        "errors": [],
                    },
                    "programs": {
                        "primary": {
                            "process": {"kind": "exited", "code": 0},
                            "final_environment": {
                                "availability": "available",
                                "value": {"digest": "sha256:final"},
                            },
                            "errors": [],
                        }
                    }
                }
            })
        );
        let encoded = serde_json::to_string(&value).expect("encoded start result");
        assert!(!encoded.contains("large-output"));
        assert!(!encoded.contains("large-error"));
        assert!(!encoded.contains("input-payload"));
    }

    #[test]
    fn start_result_summarizes_engine_error_and_idempotent_retry() {
        let completion = json!({
            "kind": "engine_returned",
            "result": {
                "kind": "engine_error",
                "error": {
                    "kind": "unsupported_input",
                    "path": "network",
                    "reason": "unsupported",
                }
            }
        });
        let record = stored_run(Some(completion));

        let value = serde_json::to_value(start_result(&record, false).expect("start result"))
            .expect("start result JSON");

        assert_eq!(value["created"], false);
        assert_eq!(value["lifecycle"], "terminal");
        assert_eq!(value["completion"]["kind"], "engine_error");
        assert_eq!(value["completion"]["error"]["kind"], "unsupported_input");
    }

    #[test]
    fn start_result_preserves_accepted_state_without_completion() {
        let record = stored_run(None);

        let value = serde_json::to_value(start_result(&record, false).expect("start result"))
            .expect("start result JSON");

        assert_eq!(value["created"], false);
        assert_eq!(value["lifecycle"], "accepted");
        assert!(value["completion"].is_null());
    }

    #[test]
    fn idempotent_request_requires_the_same_metadata() {
        let existing = stored_run(None);
        assert!(matches_request(&existing, &json!({}), &Metadata::default()));
        let changed = Metadata::new(
            Some("different intent".to_owned()),
            &["suite=swe-bench".parse().expect("label")],
        )
        .expect("metadata");
        assert!(!matches_request(&existing, &json!({}), &changed));
    }

    fn stored_run(completion: Option<Value>) -> StoredRun {
        StoredRun {
            run_id: "550e8400-e29b-41d4-a716-446655440000".to_owned(),
            accepted_at: "2026-08-27T00:00:00Z".to_owned(),
            metadata: Metadata::default(),
            input: json!({"stdin": "input-payload"}),
            input_identity: json!({}),
            terminal_at: completion
                .as_ref()
                .map(|_| "2026-08-27T00:00:01Z".to_owned()),
            completion,
        }
    }
}
