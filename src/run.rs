use std::collections::BTreeMap;
#[cfg(target_os = "linux")]
use std::env;
#[cfg(target_os = "linux")]
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::num::NonZeroU64;
#[cfg(target_os = "linux")]
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
#[cfg(target_os = "linux")]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(target_os = "linux")]
use std::thread;
#[cfg(target_os = "linux")]
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, SecondsFormat, Utc};
use run_engine::CancellationToken;
#[cfg(not(target_os = "linux"))]
use run_engine::RunEngine;
use run_protocol::{
    EngineError, ImageDescriptor, Network, ProgramId, ProgramInput, RunControls, RunInput,
    RunOutput, RuntimeConfig, Secrets,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Value, json};
use uuid::{Uuid, Version};

use crate::image::{ImageSelector, Images};
use crate::live_event::RunLiveEvent;
use crate::metadata::Metadata;
use crate::run_record::{CompletionRecord, EngineResultRecord, InputIdentityRecord, InputRecord};
use crate::state::State;
use crate::storage::{
    Database, ExecutionOwner, ExecutionPhase, NewRun, RunCancellation, RunInsertion, StoredRun,
};

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

impl<'de> Deserialize<'de> for RunId {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

pub(crate) struct RunRequest {
    pub(crate) run_id: RunId,
    pub(crate) metadata: Metadata,
    pub(crate) execution: ExecutionRequest,
}

pub(crate) struct ExecutionRequest {
    pub(crate) image: ImageSelector,
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

struct PreparedExecution {
    input: RunInput,
    input_record: InputRecord,
    identity: InputIdentityRecord,
}

#[derive(Debug, Serialize)]
pub(crate) struct RunStartResult {
    schema_version: u32,
    created: bool,
    run_id: String,
    initial_image_name: Option<String>,
    metadata: Metadata,
    lifecycle: &'static str,
    cancellation_requested_at: Option<String>,
    completion: Option<Value>,
}

#[derive(Debug, Serialize)]
pub(crate) struct RunCancelResult {
    schema_version: u32,
    run_id: String,
    lifecycle: &'static str,
    cancellation_requested: bool,
    cancellation_requested_at: Option<String>,
    terminal_at: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct RunReconcileResult {
    schema_version: u32,
    run_id: String,
    lifecycle: &'static str,
    outcome: &'static str,
    evidence: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ExecResult {
    schema_version: u32,
    result: Value,
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
    initial_image_name: Option<String>,
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
        let image = self.images.resolve_input(image)?;
        crate::runtime_config::generate(&image.image_configuration)
    }

    pub(crate) fn start(&self, state: &State, request: &RunRequest) -> Result<RunStartResult> {
        self.start_with_live_events(state, request, true, false)
    }

    pub(crate) fn start_detached_worker(
        &self,
        state: &State,
        request: &RunRequest,
    ) -> Result<RunStartResult> {
        self.start_with_live_events(state, request, false, true)
    }

    fn start_with_live_events(
        &self,
        state: &State,
        request: &RunRequest,
        observe: bool,
        signal_detached_ready: bool,
    ) -> Result<RunStartResult> {
        let prepared = self.prepare(&request.execution, true).map_err(|error| {
            crate::error::classify(
                error,
                crate::error::ErrorFacts::before_run(
                    crate::error::ErrorCategory::InvalidInput,
                    "input_preparation",
                ),
            )
        })?;
        let run_id = request.run_id.to_string();
        let engine = native_engine(state)?;
        let accepted_at = Utc::now().to_rfc3339();
        let owner = current_execution_owner()?;
        let insertion = self.database.run_insert(&NewRun {
            run_id: &run_id,
            accepted_at: &accepted_at,
            initial_image_name: request.execution.image.catalog_name(),
            metadata: &request.metadata,
            input: &prepared.input_record,
            input_identity: &prepared.identity,
            owner: &owner,
        })?;
        if let RunInsertion::Deleted(ref tombstone) = insertion {
            return Err(deleted_identity_error(&run_id, tombstone));
        }
        if matches!(insertion, RunInsertion::Existing) {
            let existing = self
                .database
                .run_get(&run_id)?
                .context("Run disappeared during idempotency check")?;
            if !matches_request(
                &existing,
                &prepared.identity,
                request.execution.image.catalog_name(),
                &request.metadata,
            ) {
                return Err(identity_conflict_error(&run_id));
            }
            let live_events = persistent_live_event(&run_id, observe, false);
            live_events.finish();
            return start_result(&existing, false);
        }
        debug_assert!(matches!(insertion, RunInsertion::Created));

        let accepted = (|| {
            let live_events = persistent_live_event(&run_id, observe, signal_detached_ready);
            live_events.stage("accepted");
            live_events.stage("preparing");
            let cancellation = CancellationToken::new();
            let signal = SignalCancellation::install(&cancellation)?;
            self.database.run_mark_engine_running(&run_id)?;
            let input = prepared.input;
            let (result, cancellation_monitor) = run_persistent_with_events(
                &engine,
                &input,
                &cancellation,
                live_events.engine_event_sink(),
                self.database,
                &run_id,
            );
            signal.close()?;
            live_events.report_dropped_events();
            live_events.stage("publishing");
            let completion = CompletionRecord::engine_returned(result);
            let terminal_at = Utc::now().to_rfc3339();
            self.database.run_stage_completion(&run_id, &completion)?;
            if !self.database.run_publish_staged(&run_id, &terminal_at)? {
                bail!("staged Run completion disappeared before publication");
            }
            live_events.stage("terminal");
            live_events.finish();
            cancellation_monitor?;
            let record = self
                .database
                .run_get(&run_id)?
                .context("completed Run disappeared")?;
            start_result(&record, true)
        })();
        accepted.map_err(|error| {
            crate::error::classify(
                error,
                crate::error::ErrorFacts {
                    category: crate::error::ErrorCategory::Internal,
                    stage: "accepted_run",
                    run_id: Some(run_id.clone()),
                    accepted: Some(true),
                    run_created: Some(true),
                    retryable: true,
                    recovery: Some(format!("runlab run get {run_id}")),
                },
            )
        })
    }

    pub(crate) fn exec(&self, state: &State, request: &ExecutionRequest) -> Result<ExecResult> {
        let prepared = self.prepare(request, false).map_err(|error| {
            crate::error::classify(
                error,
                crate::error::ErrorFacts::before_run(
                    crate::error::ErrorCategory::InvalidInput,
                    "input_preparation",
                ),
            )
        })?;
        let engine = native_engine(state)?;
        let live_events = RunLiveEvent::exec_stderr();
        live_events.stage("preparing");
        let cancellation = CancellationToken::new();
        let signal = SignalCancellation::install(&cancellation)?;
        let result = run_with_events(
            &engine,
            &prepared.input,
            &cancellation,
            live_events.engine_event_sink(),
        );
        signal.close()?;
        live_events.report_dropped_events();
        live_events.finish();
        Ok(ExecResult {
            schema_version: 1,
            result: serde_json::to_value(EngineResultRecord::from(result))?,
        })
    }

    fn prepare(
        &self,
        request: &ExecutionRequest,
        capture_final_environment: bool,
    ) -> Result<PreparedExecution> {
        let image = self.images.resolve_input(&request.image)?;
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
            RunControls::new(
                request.execution_timeout_ms,
                request.network,
                capture_final_environment,
            ),
        )?;
        let input_record = InputRecord::primary(
            &image.manifest,
            &runtime_bytes,
            &stdin,
            &request.secrets,
            request.execution_timeout_ms,
            request.network,
            capture_final_environment,
        );
        let identity = InputIdentityRecord::primary(
            &image.manifest,
            runtime.as_json(),
            &stdin,
            &request.secrets,
            request.execution_timeout_ms,
            request.network,
        );
        Ok(PreparedExecution {
            input,
            input_record,
            identity,
        })
    }

    pub(crate) fn get(&self, run_id: RunId) -> Result<Value> {
        let run_id = run_id.to_string();
        let Some(record) = self.database.run_get(&run_id)? else {
            let message = self.database.run_tombstone(&run_id)?.map_or_else(
                || format!("Run does not exist: {run_id}"),
                |tombstone| {
                    format!(
                        "Run was deleted at {} by operation {}: {run_id}",
                        tombstone.deleted_at, tombstone.operation_id
                    )
                },
            );
            return Err(crate::error::classify(
                anyhow::anyhow!(message),
                crate::error::ErrorFacts::before_run(
                    crate::error::ErrorCategory::NotFound,
                    "run_lookup",
                ),
            ));
        };
        Ok(record_json(&record))
    }

    pub(crate) fn cancel(&self, run_id: RunId) -> Result<RunCancelResult> {
        let run_id = run_id.to_string();
        let cancellation = self
            .database
            .run_cancel(&run_id, &Utc::now().to_rfc3339())?
            .ok_or_else(|| {
                crate::error::classify(
                    anyhow::anyhow!("Run does not exist: {run_id}"),
                    crate::error::ErrorFacts::before_run(
                        crate::error::ErrorCategory::NotFound,
                        "run_lookup",
                    ),
                )
            })?;
        Ok(match cancellation {
            RunCancellation::Requested { requested_at } => RunCancelResult {
                schema_version: 1,
                run_id,
                lifecycle: "accepted",
                cancellation_requested: true,
                cancellation_requested_at: Some(requested_at),
                terminal_at: None,
            },
            RunCancellation::Terminal {
                requested_at,
                terminal_at,
            } => RunCancelResult {
                schema_version: 1,
                run_id,
                lifecycle: "terminal",
                cancellation_requested: requested_at.is_some(),
                cancellation_requested_at: requested_at,
                terminal_at: Some(terminal_at),
            },
        })
    }

    pub(crate) fn list(&self, limit: usize, after: Option<RunId>) -> Result<RunListResult> {
        if !(1..=MAX_PAGE_SIZE).contains(&limit) {
            return Err(crate::error::invalid_input(
                anyhow::anyhow!("--limit must be between 1 and {MAX_PAGE_SIZE}"),
                "run_input",
            ));
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

    pub(crate) fn reconcile(&self, run_id: RunId) -> Result<RunReconcileResult> {
        let run_id = run_id.to_string();
        let record = self.database.run_get(&run_id)?.ok_or_else(|| {
            crate::error::classify(
                anyhow::anyhow!("Run does not exist: {run_id}"),
                crate::error::ErrorFacts::before_run(
                    crate::error::ErrorCategory::NotFound,
                    "run_lookup",
                ),
            )
        })?;
        if record.completion.is_some() {
            return Ok(RunReconcileResult {
                schema_version: 1,
                run_id,
                lifecycle: "terminal",
                outcome: "already_terminal",
                evidence: None,
            });
        }
        let Some(journal) = self.database.run_execution(&run_id)? else {
            return Ok(RunReconcileResult {
                schema_version: 1,
                run_id,
                lifecycle: "accepted",
                outcome: "evidence_incomplete",
                evidence: Some("no persistent execution journal exists for this Run".to_owned()),
            });
        };
        if journal.phase == ExecutionPhase::ResultStaged {
            if journal.completion.is_none() {
                bail!("staged Run execution has no durable completion: {run_id}");
            }
            if !self
                .database
                .run_publish_staged(&run_id, &Utc::now().to_rfc3339())?
            {
                bail!("staged Run completion disappeared during reconciliation");
            }
            return Ok(RunReconcileResult {
                schema_version: 1,
                run_id,
                lifecycle: "terminal",
                outcome: "published_staged_result",
                evidence: Some("durably staged Engine result".to_owned()),
            });
        }
        match execution_owner_state(&journal.owner)? {
            ExecutionOwnerState::Alive => Ok(RunReconcileResult {
                schema_version: 1,
                run_id,
                lifecycle: "accepted",
                outcome: "coordinator_alive",
                evidence: Some("boot ID, PID, and process start time match".to_owned()),
            }),
            ExecutionOwnerState::Dead(evidence) => {
                if journal.phase == ExecutionPhase::Accepted {
                    let evidence = format!(
                        "{evidence}; execution journal proves the Run Engine call was not begun"
                    );
                    let completion = CompletionRecord::interrupted_before_engine_start(
                        Utc::now().to_rfc3339(),
                        evidence.clone(),
                    );
                    self.database
                        .run_stage_pre_engine_interruption(&run_id, &completion)?;
                    if !self
                        .database
                        .run_publish_staged(&run_id, &Utc::now().to_rfc3339())?
                    {
                        bail!("staged Run interruption disappeared during reconciliation");
                    }
                    return Ok(RunReconcileResult {
                        schema_version: 1,
                        run_id,
                        lifecycle: "terminal",
                        outcome: "published_interrupted",
                        evidence: Some(evidence),
                    });
                }
                Ok(RunReconcileResult {
                    schema_version: 1,
                    run_id,
                    lifecycle: "accepted",
                    outcome: "evidence_incomplete",
                    evidence: Some(format!(
                        "{evidence}; Engine resource cleanup has not been proved, so interrupted was not published"
                    )),
                })
            }
        }
    }
}

fn identity_conflict_error(run_id: &str) -> anyhow::Error {
    crate::error::classify(
        anyhow::anyhow!("Run identity is already bound to a different request: {run_id}"),
        crate::error::ErrorFacts {
            category: crate::error::ErrorCategory::Conflict,
            stage: "acceptance",
            run_id: Some(run_id.to_owned()),
            accepted: Some(false),
            run_created: Some(false),
            retryable: false,
            recovery: None,
        },
    )
}

fn deleted_identity_error(run_id: &str, tombstone: &crate::storage::RunTombstone) -> anyhow::Error {
    crate::error::classify(
        anyhow::anyhow!(
            "Run identity was permanently retired at {} by deletion operation {}: {run_id}",
            tombstone.deleted_at,
            tombstone.operation_id
        ),
        crate::error::ErrorFacts {
            category: crate::error::ErrorCategory::Conflict,
            stage: "acceptance",
            run_id: Some(run_id.to_owned()),
            accepted: Some(false),
            run_created: Some(false),
            retryable: false,
            recovery: Some(format!(
                "choose a new Run UUID; query `run_deletions` for the retired identity {}",
                tombstone.run_id
            )),
        },
    )
}

enum ExecutionOwnerState {
    Alive,
    Dead(&'static str),
}

#[cfg(target_os = "linux")]
fn execution_owner_state(owner: &ExecutionOwner) -> Result<ExecutionOwnerState> {
    let boot_id = fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
    if boot_id.trim() != owner.boot_id {
        return Ok(ExecutionOwnerState::Dead("Linux boot identity changed"));
    }
    let pid = u32::try_from(owner.pid).context("stored coordinator PID is invalid")?;
    let stat = match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ExecutionOwnerState::Dead("coordinator process is absent"));
        }
        Err(error) => return Err(error.into()),
    };
    let suffix = stat
        .rfind(") ")
        .map(|index| &stat[index + 2..])
        .context("coordinator process identity is malformed")?;
    let start_ticks = suffix
        .split_whitespace()
        .nth(19)
        .context("coordinator process start time is absent")?
        .parse::<i64>()?;
    Ok(if start_ticks == owner.start_ticks {
        ExecutionOwnerState::Alive
    } else {
        ExecutionOwnerState::Dead("coordinator PID was reused")
    })
}

#[cfg(not(target_os = "linux"))]
fn execution_owner_state(_owner: &ExecutionOwner) -> Result<ExecutionOwnerState> {
    bail!("Run reconciliation requires Linux")
}

#[cfg(target_os = "linux")]
fn current_execution_owner() -> Result<ExecutionOwner> {
    let boot_id = fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .context("failed to read Linux boot identity")?
        .trim()
        .to_owned();
    let pid = std::process::id();
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))
        .context("failed to read coordinator process identity")?;
    let suffix = stat
        .rfind(") ")
        .map(|index| &stat[index + 2..])
        .context("coordinator process identity is malformed")?;
    let start_ticks = suffix
        .split_whitespace()
        .nth(19)
        .context("coordinator process start time is absent")?
        .parse::<i64>()
        .context("coordinator process start time is invalid")?;
    Ok(ExecutionOwner {
        boot_id,
        pid: i64::from(pid),
        start_ticks,
    })
}

#[cfg(not(target_os = "linux"))]
fn current_execution_owner() -> Result<ExecutionOwner> {
    bail!("persistent Run execution requires Linux")
}

fn persistent_live_event(run_id: &str, enabled: bool, signal_detached_ready: bool) -> RunLiveEvent {
    if signal_detached_ready {
        RunLiveEvent::detached_ready(run_id);
    }
    if enabled {
        RunLiveEvent::stderr(run_id)
    } else {
        RunLiveEvent::discarded()
    }
}

#[cfg(target_os = "linux")]
fn run_persistent_with_events(
    engine: &run_engine::NativeEngine,
    input: &RunInput,
    cancellation: &CancellationToken,
    event_sink: Arc<dyn run_engine::EngineEventSink>,
    database: &Database,
    run_id: &str,
) -> (Result<RunOutput, EngineError>, Result<()>) {
    let finished = AtomicBool::new(false);
    thread::scope(|scope| {
        let watcher = scope.spawn(|| -> Result<()> {
            while !finished.load(Ordering::Acquire) {
                if database.run_cancellation_requested(run_id)? {
                    cancellation.cancel();
                    return Ok(());
                }
                thread::park_timeout(Duration::from_millis(50));
            }
            Ok(())
        });
        let result = run_with_events(engine, input, cancellation, event_sink);
        finished.store(true, Ordering::Release);
        watcher.thread().unpark();
        let watcher = watcher
            .join()
            .map_err(|_| anyhow::anyhow!("Run cancellation monitor panicked"))
            .and_then(|result| result);
        (result, watcher)
    })
}

#[cfg(not(target_os = "linux"))]
fn run_persistent_with_events(
    _engine: &UnavailableEngine,
    _input: &RunInput,
    _cancellation: &CancellationToken,
    _event_sink: Arc<dyn run_engine::EngineEventSink>,
    _database: &Database,
    _run_id: &str,
) -> (Result<RunOutput, EngineError>, Result<()>) {
    unreachable!("unavailable engine is never constructed")
}

fn matches_request(
    existing: &StoredRun,
    identity: &InputIdentityRecord,
    initial_image_name: Option<&str>,
    metadata: &Metadata,
) -> bool {
    existing.input_identity == *identity
        && existing.initial_image_name.as_deref() == initial_image_name
        && existing.metadata == *metadata
}

#[cfg(target_os = "linux")]
fn run_with_events(
    engine: &run_engine::NativeEngine,
    input: &RunInput,
    cancellation: &CancellationToken,
    event_sink: Arc<dyn run_engine::EngineEventSink>,
) -> Result<RunOutput, EngineError> {
    engine.run_with_events(input, cancellation, event_sink)
}

#[cfg(not(target_os = "linux"))]
fn run_with_events(
    _engine: &UnavailableEngine,
    _input: &RunInput,
    _cancellation: &CancellationToken,
    _event_sink: Arc<dyn run_engine::EngineEventSink>,
) -> Result<RunOutput, EngineError> {
    unreachable!("unavailable engine is never constructed")
}

#[cfg(target_os = "linux")]
fn native_engine(state: &State) -> Result<run_engine::NativeEngine> {
    let current_executable =
        env::current_exe().context("failed to locate the RunLab executable")?;
    let runc = resolve_runc(&current_executable, env::var_os("PATH").as_deref())?;
    Ok(run_engine::NativeEngine::new(
        state.oci(),
        state.engine_workspace(),
        runc,
        run_engine::OperationTimeouts::default(),
    ))
}

#[cfg(target_os = "linux")]
fn resolve_runc(current_executable: &Path, search_path: Option<&OsStr>) -> Result<PathBuf> {
    let directory = current_executable
        .parent()
        .context("RunLab executable has no parent directory")?;
    let bundled = directory.join("runlab-runc");
    if bundled.is_file() {
        return fs::canonicalize(&bundled)
            .with_context(|| format!("failed to resolve bundled runc {}", bundled.display()));
    }
    executable_in_path("runc", search_path)
}

#[cfg(target_os = "linux")]
fn executable_in_path(name: &str, search_path: Option<&OsStr>) -> Result<PathBuf> {
    let path = search_path.context("PATH is not set")?;
    for directory in env::split_paths(path) {
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

fn record_json(record: &StoredRun) -> Value {
    json!({
        "schema_version": 1,
        "run_id": record.run_id,
        "initial_image_name": record.initial_image_name,
        "metadata": record.metadata,
        "accepted_at": record.accepted_at,
        "lifecycle": if record.completion.is_some() { "terminal" } else { "accepted" },
        "cancellation_requested_at": record.cancellation_requested_at,
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
        initial_image_name: record.initial_image_name.clone(),
        metadata: record.metadata.clone(),
        lifecycle: if record.completion.is_some() {
            "terminal"
        } else {
            "accepted"
        },
        cancellation_requested_at: record.cancellation_requested_at.clone(),
        completion: record
            .completion
            .as_ref()
            .map(CompletionRecord::summary)
            .transpose()?,
    })
}

fn run_summary(record: StoredRun) -> RunSummary {
    let completion = record
        .completion
        .as_ref()
        .and_then(CompletionRecord::result_kind)
        .map(str::to_owned);
    RunSummary {
        run_id: record.run_id,
        initial_image_name: record.initial_image_name,
        metadata: record.metadata,
        accepted_at: list_timestamp(&record.accepted_at),
        lifecycle: if record.completion.is_some() {
            "terminal"
        } else {
            "accepted"
        },
        terminal_at: record.terminal_at.as_deref().map(list_timestamp),
        completion,
    }
}

fn list_timestamp(value: &str) -> String {
    DateTime::parse_from_rfc3339(value).map_or_else(
        |_| value.to_owned(),
        |value| {
            value
                .with_timezone(&Utc)
                .to_rfc3339_opts(SecondsFormat::Secs, true)
        },
    )
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

    #[cfg(target_os = "linux")]
    #[test]
    fn bundled_runc_takes_precedence_over_path() {
        let root = tempfile::tempdir().expect("temporary directory");
        let installed = root.path().join("installed");
        let fallback = root.path().join("fallback");
        fs::create_dir_all(&installed).expect("installed directory");
        fs::create_dir_all(&fallback).expect("fallback directory");
        let executable = installed.join("runlab");
        let bundled = installed.join("runlab-runc");
        fs::write(&executable, b"runlab").expect("RunLab fixture");
        fs::write(&bundled, b"bundled").expect("bundled runc fixture");
        fs::write(fallback.join("runc"), b"fallback").expect("PATH runc fixture");
        let search_path = env::join_paths([fallback]).expect("search path");

        assert_eq!(
            resolve_runc(&executable, Some(&search_path)).expect("resolved runc"),
            fs::canonicalize(bundled).expect("canonical bundled runc")
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn path_runc_remains_the_source_build_fallback() {
        let root = tempfile::tempdir().expect("temporary directory");
        let installed = root.path().join("installed");
        let fallback = root.path().join("fallback");
        fs::create_dir_all(&installed).expect("installed directory");
        fs::create_dir_all(&fallback).expect("fallback directory");
        let executable = installed.join("runlab");
        let fallback_runc = fallback.join("runc");
        fs::write(&executable, b"runlab").expect("RunLab fixture");
        fs::write(&fallback_runc, b"fallback").expect("PATH runc fixture");
        let search_path = env::join_paths([fallback]).expect("search path");

        assert_eq!(
            resolve_runc(&executable, Some(&search_path)).expect("resolved runc"),
            fs::canonicalize(fallback_runc).expect("canonical PATH runc")
        );
    }

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
            InputRecord::primary(&image, b"{}", b"", &first, None, Network::Isolated, true);
        let encoded = serde_json::to_string(&record).expect("record JSON");
        assert!(!encoded.contains("plain-environment-secret"));
        assert!(!encoded.contains("plain-file-secret"));
        assert_eq!(
            serde_json::to_value(&record).expect("record value")["programs"]["primary"]["secrets"]
                ["env"]["TOKEN"]["retained"],
            false
        );

        let first_identity =
            InputIdentityRecord::primary(&image, &json!({}), b"", &first, None, Network::Isolated);
        let second_identity =
            InputIdentityRecord::primary(&image, &json!({}), b"", &second, None, Network::Isolated);
        assert_ne!(first_identity, second_identity);
        let identity = serde_json::to_string(&first_identity).expect("identity JSON");
        assert!(!identity.contains("plain-environment-secret"));
        assert!(!identity.contains("plain-file-secret"));
    }

    #[test]
    fn start_result_keeps_execution_facts_without_record_payloads() {
        let completion = json!({
            "kind": "engine_returned",
            "record_version": 1,
            "result": {
                "kind": "output",
                "output": {
                    "execution": {
                        "interval": {
                            "kind": "entered",
                            "started_at": "2026-08-27T00:00:00Z",
                            "ended_at": "2026-08-27T00:00:01Z"
                        },
                        "timed_out": false,
                        "cancelled": false,
                        "errors": [],
                    },
                    "programs": {
                        "primary": {
                            "create": {
                                "status": "succeeded",
                                "facts": {"completed_at": "2026-08-27T00:00:00Z"},
                                "reason": null
                            },
                            "start": {
                                "status": "succeeded",
                                "facts": {"started_at": "2026-08-27T00:00:00Z"},
                                "reason": null
                            },
                            "process": {
                                "kind": "exited",
                                "code": 0,
                                "ended_at": "2026-08-27T00:00:01Z"
                            },
                            "stdin": {
                                "write": {
                                    "status": "succeeded",
                                    "facts": {"bytes_written": 0},
                                    "reason": null
                                },
                                "close": {"status": "succeeded", "facts": null, "reason": null}
                            },
                            "stdout": {
                                "status": "succeeded",
                                "facts": {
                                    "bytes": {"encoding": "base64", "value": "bGFyZ2Utb3V0cHV0", "byte_length": 12},
                                    "omitted_after_limit": false,
                                    "eof": true
                                },
                                "reason": null
                            },
                            "stderr": {
                                "status": "succeeded",
                                "facts": {
                                    "bytes": {"encoding": "base64", "value": "bGFyZ2UtZXJyb3I=", "byte_length": 11},
                                    "omitted_after_limit": false,
                                    "eof": true
                                },
                                "reason": null
                            },
                            "stop_actions": [],
                            "final_environment": {
                                "availability": "available",
                                "value": {
                                    "mediaType": "application/vnd.oci.image.manifest.v1+json",
                                    "digest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                                    "size": 1
                                }
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

        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["created"], true);
        assert_eq!(value["lifecycle"], "terminal");
        assert_eq!(value["completion"]["kind"], "output");
        assert_eq!(
            value["completion"]["programs"]["primary"]["process"]["code"],
            0
        );
        assert_eq!(
            value["completion"]["programs"]["primary"]["final_environment"]["availability"],
            "available"
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
            "record_version": 1,
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
        let identity = existing.input_identity.clone();
        assert!(matches_request(
            &existing,
            &identity,
            Some("agent-base"),
            &Metadata::default()
        ));
        let changed = Metadata::new(
            Some("different intent".to_owned()),
            &["suite=swe-bench".parse().expect("label")],
        )
        .expect("metadata");
        assert!(!matches_request(
            &existing,
            &identity,
            Some("agent-base"),
            &changed
        ));
        assert!(!matches_request(
            &existing,
            &identity,
            Some("renamed"),
            &Metadata::default()
        ));
    }

    fn stored_run(completion: Option<Value>) -> StoredRun {
        let image: oci_spec::image::Descriptor = serde_json::from_value(json!({
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "size": 1
        }))
        .expect("descriptor");
        let secrets = Secrets::default();
        StoredRun {
            run_id: "550e8400-e29b-41d4-a716-446655440000".to_owned(),
            accepted_at: "2026-08-27T00:00:00Z".to_owned(),
            initial_image_name: Some("agent-base".to_owned()),
            metadata: Metadata::default(),
            input: InputRecord::primary(
                &image,
                b"{}",
                b"input-payload",
                &secrets,
                None,
                Network::Isolated,
                true,
            ),
            input_identity: InputIdentityRecord::primary(
                &image,
                &json!({}),
                b"input-payload",
                &secrets,
                None,
                Network::Isolated,
            ),
            cancellation_requested_at: None,
            terminal_at: completion
                .as_ref()
                .map(|_| "2026-08-27T00:00:01Z".to_owned()),
            completion: completion
                .map(|value| serde_json::from_value(value).expect("typed completion fixture")),
        }
    }
}
