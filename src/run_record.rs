use std::collections::BTreeMap;
use std::num::NonZeroU64;

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use oci_spec::image::Descriptor;
use run_protocol::{
    Availability, EngineError, ExecutionInterval, FinalEnvironment, Network, OperationError,
    OperationReport, OperationStage, OperationStatus, ProcessResult, ProgramOutput, RunOutput,
    Secrets, StopAction, StopActionResult, StopSignal, StreamFacts,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub(crate) const RECORD_VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(crate) struct InputRecord {
    record_version: u32,
    pub(crate) programs: BTreeMap<String, ProgramInputRecord>,
    controls: ControlsRecord,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(crate) struct ProgramInputRecord {
    pub(crate) initial_environment: Descriptor,
    runtime_config: EncodedBytes,
    stdin: EncodedBytes,
    secrets: RedactedSecrets,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct EncodedBytes {
    encoding: Encoding,
    bytes: String,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Encoding {
    Base64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct RedactedSecrets {
    env: BTreeMap<String, RetainedSecret>,
    files: BTreeMap<String, RetainedSecret>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
struct RetainedSecret {
    retained: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
struct ControlsRecord {
    execution_timeout_ms: Option<u64>,
    network: NetworkRecord,
    capture_final_environment: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum NetworkRecord {
    Isolated,
    Egress,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(crate) struct InputIdentityRecord {
    record_version: u32,
    programs: BTreeMap<String, ProgramIdentityRecord>,
    execution_timeout_ms: Option<u64>,
    network: NetworkRecord,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct ProgramIdentityRecord {
    initial_environment: Descriptor,
    runtime_config: Value,
    stdin: String,
    secrets: SecretIdentityRecord,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct SecretIdentityRecord {
    env: BTreeMap<String, String>,
    files: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum CompletionRecord {
    EngineReturned {
        record_version: u32,
        result: EngineResultRecord,
    },
    /// Settled target Record shape. The current Coordinator must not construct
    /// it until recovery has proved the original Engine call cannot return and
    /// has completed every safely available collection and cleanup action.
    Interrupted {
        record_version: u32,
        interruption: RunInterruptionRecord,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum EngineResultRecord {
    Output { output: OutputRecord },
    EngineError { error: EngineErrorRecord },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(crate) struct EngineErrorRecord {
    kind: EngineErrorKindRecord,
    path: Option<String>,
    reason: String,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum EngineErrorKindRecord {
    InvalidInput,
    InputUnavailable,
    UnsupportedInput,
    Internal,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(crate) struct OutputRecord {
    pub(crate) execution: ExecutionRecord,
    pub(crate) programs: BTreeMap<String, ProgramOutputRecord>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(crate) struct ExecutionRecord {
    interval: ExecutionIntervalRecord,
    timed_out: bool,
    cancelled: bool,
    errors: Vec<OperationErrorRecord>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ExecutionIntervalRecord {
    NotEntered {
        reason: String,
    },
    Entered {
        started_at: String,
        ended_at: String,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(crate) struct ProgramOutputRecord {
    create: OperationReportRecord<CreateFactsRecord>,
    start: OperationReportRecord<StartFactsRecord>,
    process: ProcessRecord,
    stdin: StdinRecord,
    stdout: OperationReportRecord<StreamFactsRecord>,
    stderr: OperationReportRecord<StreamFactsRecord>,
    stop_actions: Vec<StopActionRecord>,
    pub(crate) final_environment: FinalEnvironmentRecord,
    errors: Vec<OperationErrorRecord>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct OperationReportRecord<T> {
    status: OperationStatusRecord,
    facts: Option<T>,
    reason: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum OperationStatusRecord {
    NotAttempted,
    Succeeded,
    Failed,
    Unknown,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct CreateFactsRecord {
    completed_at: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct StartFactsRecord {
    started_at: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ProcessRecord {
    NeverStarted {
        reason: String,
    },
    Exited {
        code: i32,
        ended_at: String,
    },
    Signaled {
        signal: u32,
        ended_at: String,
    },
    Unknown {
        reason: String,
        ended_at: AvailabilityRecord<String>,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct StdinRecord {
    write: OperationReportRecord<StdinWriteFactsRecord>,
    close: OperationReportRecord<Value>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
struct StdinWriteFactsRecord {
    bytes_written: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct StreamFactsRecord {
    bytes: StreamBytesRecord,
    omitted_after_limit: bool,
    eof: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct StreamBytesRecord {
    encoding: Encoding,
    value: String,
    byte_length: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "availability", rename_all = "snake_case")]
enum AvailabilityRecord<T> {
    Available { value: T },
    Unavailable { reason: String },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "availability", rename_all = "snake_case")]
pub(crate) enum FinalEnvironmentRecord {
    NotRequested,
    Available { value: Box<Descriptor> },
    Unavailable { reason: String },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct StopActionRecord {
    signal: StopSignalRecord,
    attempted_at: String,
    result: StopResultRecord,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum StopSignalRecord {
    Term,
    Kill,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum StopResultRecord {
    Accepted,
    Rejected,
    Unknown { reason: String },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct OperationErrorRecord {
    observed_at: String,
    stage: OperationStageRecord,
    message: String,
    code: Option<i64>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum OperationStageRecord {
    Preparation,
    Create,
    Start,
    ProcessSupervision,
    StdinWrite,
    StdinClose,
    StdoutRead,
    StderrRead,
    Signal,
    Wait,
    RuntimeFilesystemRemoval,
    FinalEnvironmentCapture,
    Cleanup,
    Coordination,
    Timing,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(crate) struct RunInterruptionRecord {
    reason: String,
    observed_at: String,
    evidence_source: String,
    unavailable_results: BTreeMap<String, String>,
}

impl InputRecord {
    #[allow(
        clippy::too_many_arguments,
        reason = "the persisted input mirrors distinct Run Protocol input facts"
    )]
    pub(crate) fn primary(
        image: &Descriptor,
        runtime: &[u8],
        stdin: &[u8],
        secrets: &Secrets,
        timeout: Option<NonZeroU64>,
        network: Network,
        capture_final_environment: bool,
    ) -> Self {
        Self {
            record_version: RECORD_VERSION,
            programs: BTreeMap::from([(
                "primary".to_owned(),
                ProgramInputRecord {
                    initial_environment: image.clone(),
                    runtime_config: EncodedBytes {
                        encoding: Encoding::Base64,
                        bytes: BASE64.encode(runtime),
                    },
                    stdin: EncodedBytes {
                        encoding: Encoding::Base64,
                        bytes: BASE64.encode(stdin),
                    },
                    secrets: RedactedSecrets {
                        env: secrets
                            .env()
                            .keys()
                            .map(|name| (name.clone(), RetainedSecret { retained: false }))
                            .collect(),
                        files: secrets
                            .files()
                            .keys()
                            .map(|path| (path.clone(), RetainedSecret { retained: false }))
                            .collect(),
                    },
                },
            )]),
            controls: ControlsRecord {
                execution_timeout_ms: timeout.map(NonZeroU64::get),
                network: network.into(),
                capture_final_environment,
            },
        }
    }

    pub(crate) fn initial_environment(&self, program: &str) -> Option<&Descriptor> {
        self.programs
            .get(program)
            .map(|program| &program.initial_environment)
    }
}

impl InputIdentityRecord {
    pub(crate) fn primary(
        image: &Descriptor,
        runtime: &Value,
        stdin: &[u8],
        secrets: &Secrets,
        timeout: Option<NonZeroU64>,
        network: Network,
    ) -> Self {
        Self {
            record_version: RECORD_VERSION,
            programs: BTreeMap::from([(
                "primary".to_owned(),
                ProgramIdentityRecord {
                    initial_environment: image.clone(),
                    runtime_config: runtime.clone(),
                    stdin: BASE64.encode(stdin),
                    secrets: SecretIdentityRecord {
                        env: secrets
                            .env()
                            .iter()
                            .map(|(name, value)| (name.clone(), sha256_digest(value.as_bytes())))
                            .collect(),
                        files: secrets
                            .files()
                            .iter()
                            .map(|(path, value)| (path.clone(), sha256_digest(value.as_bytes())))
                            .collect(),
                    },
                },
            )]),
            execution_timeout_ms: timeout.map(NonZeroU64::get),
            network: network.into(),
        }
    }
}

impl CompletionRecord {
    pub(crate) fn engine_returned(result: Result<RunOutput, EngineError>) -> Self {
        Self::EngineReturned {
            record_version: RECORD_VERSION,
            result: result.into(),
        }
    }

    pub(crate) fn interrupted_before_engine_start(
        observed_at: String,
        evidence_source: String,
    ) -> Self {
        Self::Interrupted {
            record_version: RECORD_VERSION,
            interruption: RunInterruptionRecord {
                reason: "Coordinator ended before the Run Engine call began".to_owned(),
                observed_at,
                evidence_source,
                unavailable_results: BTreeMap::from([(
                    "engine_result".to_owned(),
                    "Run Engine was not invoked, so no RunOutput or EngineError exists".to_owned(),
                )]),
            },
        }
    }

    pub(crate) fn result_kind(&self) -> Option<&'static str> {
        match self {
            Self::EngineReturned {
                result: EngineResultRecord::Output { .. },
                ..
            } => Some("output"),
            Self::EngineReturned {
                result: EngineResultRecord::EngineError { .. },
                ..
            } => Some("engine_error"),
            Self::Interrupted { .. } => None,
        }
    }

    pub(crate) fn output(&self) -> Option<&OutputRecord> {
        match self {
            Self::EngineReturned {
                result: EngineResultRecord::Output { output },
                ..
            } => Some(output),
            Self::EngineReturned { .. } | Self::Interrupted { .. } => None,
        }
    }

    pub(crate) fn summary(&self) -> Result<Value> {
        match self {
            Self::EngineReturned { result, .. } => match result {
                EngineResultRecord::Output { output } => {
                    let programs = output
                        .programs
                        .iter()
                        .map(|(id, program)| {
                            (
                                id.clone(),
                                ProgramSummary {
                                    process: &program.process,
                                    final_environment: &program.final_environment,
                                    errors: &program.errors,
                                },
                            )
                        })
                        .collect();
                    serde_json::to_value(OutputSummary {
                        kind: "output",
                        execution: &output.execution,
                        programs,
                    })
                    .context("failed to serialize persisted Run summary")
                }
                EngineResultRecord::EngineError { error } => {
                    serde_json::to_value(EngineErrorSummary {
                        kind: "engine_error",
                        error,
                    })
                    .context("failed to serialize persisted EngineError summary")
                }
            },
            Self::Interrupted { interruption, .. } => serde_json::to_value(InterruptionSummary {
                kind: "interrupted",
                interruption,
            })
            .context("failed to serialize persisted interruption summary"),
        }
    }

    pub(crate) fn final_environment(&self, program: &str) -> Option<&FinalEnvironmentRecord> {
        self.output()?
            .programs
            .get(program)
            .map(|program| &program.final_environment)
    }
}

#[derive(Serialize)]
struct OutputSummary<'a> {
    kind: &'static str,
    execution: &'a ExecutionRecord,
    programs: BTreeMap<String, ProgramSummary<'a>>,
}

#[derive(Serialize)]
struct ProgramSummary<'a> {
    process: &'a ProcessRecord,
    final_environment: &'a FinalEnvironmentRecord,
    errors: &'a [OperationErrorRecord],
}

#[derive(Serialize)]
struct EngineErrorSummary<'a> {
    kind: &'static str,
    error: &'a EngineErrorRecord,
}

#[derive(Serialize)]
struct InterruptionSummary<'a> {
    kind: &'static str,
    interruption: &'a RunInterruptionRecord,
}

impl From<Network> for NetworkRecord {
    fn from(value: Network) -> Self {
        match value {
            Network::Isolated => Self::Isolated,
            Network::Egress => Self::Egress,
        }
    }
}

impl From<Result<RunOutput, EngineError>> for EngineResultRecord {
    fn from(value: Result<RunOutput, EngineError>) -> Self {
        match value {
            Ok(output) => Self::Output {
                output: OutputRecord::from(&output),
            },
            Err(error) => Self::EngineError {
                error: EngineErrorRecord::from(&error),
            },
        }
    }
}

impl From<&EngineError> for EngineErrorRecord {
    fn from(error: &EngineError) -> Self {
        let kind = match error {
            EngineError::InvalidInput { .. } => EngineErrorKindRecord::InvalidInput,
            EngineError::InputUnavailable { .. } => EngineErrorKindRecord::InputUnavailable,
            EngineError::UnsupportedInput { .. } => EngineErrorKindRecord::UnsupportedInput,
            EngineError::Internal { .. } => EngineErrorKindRecord::Internal,
        };
        Self {
            kind,
            path: error.path().map(ToString::to_string),
            reason: error.reason().to_owned(),
        }
    }
}

impl From<&RunOutput> for OutputRecord {
    fn from(output: &RunOutput) -> Self {
        Self {
            execution: ExecutionRecord::from(output),
            programs: output
                .programs()
                .iter()
                .map(|(id, output)| (id.as_str().to_owned(), ProgramOutputRecord::from(output)))
                .collect(),
        }
    }
}

impl From<&RunOutput> for ExecutionRecord {
    fn from(output: &RunOutput) -> Self {
        let execution = output.execution();
        Self {
            interval: match execution.interval() {
                ExecutionInterval::NotEntered { reason } => ExecutionIntervalRecord::NotEntered {
                    reason: reason.as_str().to_owned(),
                },
                ExecutionInterval::Entered {
                    started_at,
                    ended_at,
                } => ExecutionIntervalRecord::Entered {
                    started_at: started_at.to_rfc3339(),
                    ended_at: ended_at.to_rfc3339(),
                },
            },
            timed_out: execution.timed_out(),
            cancelled: execution.cancelled(),
            errors: execution.errors().map(OperationErrorRecord::from).collect(),
        }
    }
}

impl From<&ProgramOutput> for ProgramOutputRecord {
    fn from(output: &ProgramOutput) -> Self {
        Self {
            create: report_record(output.create(), |facts| CreateFactsRecord {
                completed_at: facts.completed_at().to_rfc3339(),
            }),
            start: report_record(output.start(), |facts| StartFactsRecord {
                started_at: facts.started_at().to_rfc3339(),
            }),
            process: ProcessRecord::from(output.process()),
            stdin: StdinRecord {
                write: report_record(output.stdin().write(), |facts| StdinWriteFactsRecord {
                    bytes_written: facts.bytes_written(),
                }),
                close: report_record(output.stdin().close(), |()| Value::Null),
            },
            stdout: report_record(output.stdout(), |facts| StreamFactsRecord::from(facts)),
            stderr: report_record(output.stderr(), |facts| StreamFactsRecord::from(facts)),
            stop_actions: output
                .stop_actions()
                .iter()
                .map(StopActionRecord::from)
                .collect(),
            final_environment: FinalEnvironmentRecord::from(output.final_environment()),
            errors: output.errors().map(OperationErrorRecord::from).collect(),
        }
    }
}

fn report_record<T, R>(
    report: &OperationReport<T>,
    facts: impl FnOnce(&T) -> R,
) -> OperationReportRecord<R> {
    OperationReportRecord {
        status: report.status().into(),
        facts: report.facts().map(facts),
        reason: report.reason().map(str::to_owned),
    }
}

impl From<OperationStatus> for OperationStatusRecord {
    fn from(value: OperationStatus) -> Self {
        match value {
            OperationStatus::NotAttempted => Self::NotAttempted,
            OperationStatus::Succeeded => Self::Succeeded,
            OperationStatus::Failed => Self::Failed,
            OperationStatus::Unknown => Self::Unknown,
        }
    }
}

impl From<&ProcessResult> for ProcessRecord {
    fn from(value: &ProcessResult) -> Self {
        match value {
            ProcessResult::NeverStarted { reason } => Self::NeverStarted {
                reason: reason.as_str().to_owned(),
            },
            ProcessResult::Exited { code, ended_at } => Self::Exited {
                code: *code,
                ended_at: ended_at.to_rfc3339(),
            },
            ProcessResult::Signaled { signal, ended_at } => Self::Signaled {
                signal: signal.get(),
                ended_at: ended_at.to_rfc3339(),
            },
            ProcessResult::Unknown { reason, ended_at } => Self::Unknown {
                reason: reason.as_str().to_owned(),
                ended_at: availability_record(ended_at, chrono::DateTime::to_rfc3339),
            },
        }
    }
}

fn availability_record<T, R>(
    value: &Availability<T>,
    available: impl FnOnce(&T) -> R,
) -> AvailabilityRecord<R> {
    match value {
        Availability::Available(value) => AvailabilityRecord::Available {
            value: available(value),
        },
        Availability::Unavailable(reason) => AvailabilityRecord::Unavailable {
            reason: reason.as_str().to_owned(),
        },
    }
}

impl From<&StreamFacts> for StreamFactsRecord {
    fn from(value: &StreamFacts) -> Self {
        Self {
            bytes: StreamBytesRecord {
                encoding: Encoding::Base64,
                value: BASE64.encode(value.bytes()),
                byte_length: u64::try_from(value.bytes().len()).expect("stream limit fits u64"),
            },
            omitted_after_limit: value.omitted_after_limit(),
            eof: value.eof(),
        }
    }
}

impl From<&FinalEnvironment> for FinalEnvironmentRecord {
    fn from(value: &FinalEnvironment) -> Self {
        match value {
            FinalEnvironment::NotRequested => Self::NotRequested,
            FinalEnvironment::Captured(image) => Self::Available {
                value: Box::new(image.as_oci().clone()),
            },
            FinalEnvironment::Unavailable(reason) => Self::Unavailable {
                reason: reason.as_str().to_owned(),
            },
        }
    }
}

impl From<&StopAction> for StopActionRecord {
    fn from(value: &StopAction) -> Self {
        Self {
            signal: match value.signal() {
                StopSignal::Term => StopSignalRecord::Term,
                StopSignal::Kill => StopSignalRecord::Kill,
            },
            attempted_at: value.attempted_at().to_rfc3339(),
            result: match value.result() {
                StopActionResult::Accepted => StopResultRecord::Accepted,
                StopActionResult::Rejected(_) => StopResultRecord::Rejected,
                StopActionResult::Unknown { reason, .. } => StopResultRecord::Unknown {
                    reason: reason.as_str().to_owned(),
                },
            },
        }
    }
}

impl From<&OperationError> for OperationErrorRecord {
    fn from(value: &OperationError) -> Self {
        Self {
            observed_at: value.observed_at().to_rfc3339(),
            stage: value.stage().into(),
            message: value.message().to_owned(),
            code: value.code(),
        }
    }
}

impl From<OperationStage> for OperationStageRecord {
    fn from(value: OperationStage) -> Self {
        match value {
            OperationStage::Preparation => Self::Preparation,
            OperationStage::Create => Self::Create,
            OperationStage::Start => Self::Start,
            OperationStage::ProcessSupervision => Self::ProcessSupervision,
            OperationStage::StdinWrite => Self::StdinWrite,
            OperationStage::StdinClose => Self::StdinClose,
            OperationStage::StdoutRead => Self::StdoutRead,
            OperationStage::StderrRead => Self::StderrRead,
            OperationStage::Signal => Self::Signal,
            OperationStage::Wait => Self::Wait,
            OperationStage::RuntimeFilesystemRemoval => Self::RuntimeFilesystemRemoval,
            OperationStage::FinalEnvironmentCapture => Self::FinalEnvironmentCapture,
            OperationStage::Cleanup => Self::Cleanup,
            OperationStage::Coordination => Self::Coordination,
            OperationStage::Timing => Self::Timing,
        }
    }
}

pub(crate) fn decode_input(encoded: &str) -> Result<InputRecord> {
    let record: InputRecord =
        serde_json::from_str(encoded).context("stored RunInput is invalid")?;
    validate_version(record.record_version)?;
    Ok(record)
}

pub(crate) fn decode_identity(encoded: &str) -> Result<InputIdentityRecord> {
    let record: InputIdentityRecord =
        serde_json::from_str(encoded).context("stored RunInput identity is invalid")?;
    validate_version(record.record_version)?;
    Ok(record)
}

pub(crate) fn decode_completion(encoded: &str) -> Result<CompletionRecord> {
    let record: CompletionRecord =
        serde_json::from_str(encoded).context("stored Run completion is invalid")?;
    let version = match &record {
        CompletionRecord::EngineReturned { record_version, .. }
        | CompletionRecord::Interrupted { record_version, .. } => *record_version,
    };
    validate_version(version)?;
    Ok(record)
}

pub(crate) fn migrate_input(encoded: &str) -> Result<String> {
    migrate_versioned_object(encoded, "stored RunInput", |value| {
        add_empty_secrets(value, "stored RunInput")?;
        if !value.contains_key("controls") {
            let execution_timeout_ms = value
                .get("execution_timeout_ms")
                .cloned()
                .context("legacy stored RunInput has no execution_timeout_ms")?;
            let network = value
                .get("network")
                .cloned()
                .context("legacy stored RunInput has no network")?;
            value.remove("execution_timeout_ms");
            value.remove("network");
            value.insert(
                "controls".to_owned(),
                serde_json::json!({
                    "execution_timeout_ms": execution_timeout_ms,
                    "network": network,
                    // Before non-persistent `exec` existed, every stored Run
                    // requested a Final Environment.
                    "capture_final_environment": true,
                }),
            );
        }
        value.insert("record_version".to_owned(), Value::from(RECORD_VERSION));
        Ok(())
    })
    .and_then(|encoded| {
        decode_input(&encoded)?;
        Ok(encoded)
    })
}

pub(crate) fn migrate_identity(encoded: &str) -> Result<String> {
    migrate_versioned_object(encoded, "stored RunInput identity", |value| {
        add_empty_secrets(value, "stored RunInput identity")?;
        value.insert("record_version".to_owned(), Value::from(RECORD_VERSION));
        Ok(())
    })
    .and_then(|encoded| {
        decode_identity(&encoded)?;
        Ok(encoded)
    })
}

fn add_empty_secrets(value: &mut serde_json::Map<String, Value>, subject: &str) -> Result<()> {
    let programs = value
        .get_mut("programs")
        .and_then(Value::as_object_mut)
        .with_context(|| format!("{subject} has invalid programs"))?;
    for (program_id, program) in programs {
        let program = program
            .as_object_mut()
            .with_context(|| format!("{subject} has invalid Program {program_id}"))?;
        program
            .entry("secrets")
            .or_insert_with(|| serde_json::json!({"env": {}, "files": {}}));
    }
    Ok(())
}

pub(crate) fn migrate_completion(encoded: &str) -> Result<String> {
    migrate_versioned_object(encoded, "stored Run completion", |value| {
        value.insert("record_version".to_owned(), Value::from(RECORD_VERSION));
        if value.get("kind").and_then(Value::as_str) != Some("engine_returned")
            || value
                .get("result")
                .and_then(|result| result.get("kind"))
                .and_then(Value::as_str)
                != Some("output")
        {
            return Ok(());
        }
        let programs = value
            .get_mut("result")
            .and_then(|result| result.get_mut("output"))
            .and_then(|output| output.get_mut("programs"))
            .and_then(Value::as_object_mut)
            .context("stored RunOutput has invalid programs")?;
        for program in programs.values_mut() {
            for stream in ["stdout", "stderr"] {
                let Some(bytes) = program
                    .get_mut(stream)
                    .and_then(|stream| stream.get_mut("facts"))
                    .and_then(|facts| facts.get_mut("bytes"))
                    .and_then(Value::as_object_mut)
                else {
                    continue;
                };
                if bytes.contains_key("byte_length") {
                    continue;
                }
                let encoded = bytes
                    .get("value")
                    .and_then(Value::as_str)
                    .context("stored stream bytes have no base64 value")?;
                let length = BASE64
                    .decode(encoded)
                    .context("stored stream bytes are not valid base64")?
                    .len();
                bytes.insert(
                    "byte_length".to_owned(),
                    Value::from(u64::try_from(length).context("stream byte length overflow")?),
                );
            }
        }
        Ok(())
    })
    .and_then(|encoded| {
        decode_completion(&encoded)?;
        Ok(encoded)
    })
}

fn migrate_versioned_object(
    encoded: &str,
    subject: &str,
    migrate_v0: impl FnOnce(&mut serde_json::Map<String, Value>) -> Result<()>,
) -> Result<String> {
    let mut value: Value =
        serde_json::from_str(encoded).with_context(|| format!("{subject} is invalid"))?;
    let object = value
        .as_object_mut()
        .with_context(|| format!("{subject} must be a JSON object"))?;
    match object.get("record_version").and_then(Value::as_u64) {
        None => migrate_v0(object)?,
        Some(version) if version == u64::from(RECORD_VERSION) => {}
        Some(version) => {
            bail!("unsupported Run Record version {version}; this CLI supports {RECORD_VERSION}")
        }
    }
    serde_json::to_string(&value).with_context(|| format!("failed to encode {subject}"))
}

fn validate_version(version: u32) -> Result<()> {
    if version != RECORD_VERSION {
        bail!("unsupported Run Record version {version}; this CLI supports {RECORD_VERSION}");
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::{migrate_identity, migrate_input};

    #[test]
    fn migrates_pre_controls_and_pre_secrets_input_records() {
        let input = json!({
            "programs": {
                "dependency": legacy_input_program('a'),
                "primary": legacy_input_program('b'),
            },
            "execution_timeout_ms": null,
            "network": "isolated",
        });
        let migrated = migrate_input(&input.to_string()).expect("migrate legacy input");
        let migrated: Value = serde_json::from_str(&migrated).expect("migrated input JSON");

        assert_eq!(migrated["record_version"], 1);
        assert_eq!(migrated["controls"]["execution_timeout_ms"], Value::Null);
        assert_eq!(migrated["controls"]["network"], "isolated");
        assert_eq!(migrated["controls"]["capture_final_environment"], true);
        assert!(migrated.get("execution_timeout_ms").is_none());
        assert!(migrated.get("network").is_none());
        for program in migrated["programs"].as_object().expect("programs").values() {
            assert_eq!(program["secrets"], json!({"env": {}, "files": {}}));
        }
    }

    #[test]
    fn migrates_pre_secrets_identity_records() {
        let identity = json!({
            "programs": {
                "dependency": legacy_identity_program('a'),
                "primary": legacy_identity_program('b'),
            },
            "execution_timeout_ms": 100,
            "network": "egress",
        });
        let migrated = migrate_identity(&identity.to_string()).expect("migrate legacy identity");
        let migrated: Value = serde_json::from_str(&migrated).expect("migrated identity JSON");

        assert_eq!(migrated["record_version"], 1);
        for program in migrated["programs"].as_object().expect("programs").values() {
            assert_eq!(program["secrets"], json!({"env": {}, "files": {}}));
        }
    }

    fn legacy_input_program(digest_byte: char) -> Value {
        json!({
            "initial_environment": descriptor(digest_byte),
            "runtime_config": {"encoding": "base64", "bytes": "e30="},
            "stdin": {"encoding": "base64", "bytes": ""},
        })
    }

    fn legacy_identity_program(digest_byte: char) -> Value {
        json!({
            "initial_environment": descriptor(digest_byte),
            "runtime_config": {},
            "stdin": "",
        })
    }

    fn descriptor(digest_byte: char) -> Value {
        json!({
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": format!("sha256:{}", digest_byte.to_string().repeat(64)),
            "size": 1,
        })
    }
}
