//! One Run, from acceptance to a terminal Run Record.
//!
//! The order is fixed: accept the Run so an interrupted process still leaves a
//! record, execute it, then terminalize. This file owns the part of that shape
//! which is the same on every host — the records, the exit-status contract, and
//! the choice of backend. Native mechanics live in `native`, behind one
//! platform gate.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use schemars::JsonSchema;
use serde::Serialize;

use crate::core::{
    ACCEPTED_RUN_RECORD_SCHEMA_VERSION, AcceptedLifecycle, AcceptedRunRecord, BackendFacts, Digest,
    ImageSlot, ImageView, ManagedServiceFacts, OciDescriptor, OperationError, OperationErrorScope,
    ProcessFacts, ProcessOutcome, ProcessSlot, RunControls, RunId, StoredBytes,
    TERMINAL_RUN_RECORD_SCHEMA_VERSION, TerminalLifecycle, TerminalRunRecord,
};
use crate::docker::{AttachedResult, DockerBackend, DockerImageAdapter, DockerRuntime, StopReason};
use crate::image::{CaptureResult, ImageService};
use crate::integrity::{digest_bytes, ensure_private_directory};
use crate::runtime::RuntimeConfig;
use crate::storage::RunDatabase;

#[cfg(target_os = "linux")]
mod native;
#[cfg(target_os = "linux")]
use crate::native::backend::NativeBackend;
#[cfg(target_os = "linux")]
pub use native::{ManagedPrimaryInput, ManagedServiceInput};
#[cfg(target_os = "linux")]
use native::{PreparedNativeBackend, RunScope};

/// A Run holds no host resources beyond the ones its backend owns. Only the
/// native backend owns any, so on other hosts the scope is empty and every
/// operation on it is a no-op. `run_selected` and `terminalize` therefore read
/// the same on every platform.
#[cfg(not(target_os = "linux"))]
#[derive(Default)]
struct RunScope;

#[cfg(not(target_os = "linux"))]
#[allow(
    clippy::unnecessary_wraps,
    clippy::unused_self,
    clippy::needless_pass_by_ref_mut,
    reason = "the stub mirrors the native scope API exactly so callers stay platform-neutral"
)]
impl RunScope {
    fn open(
        _runner: &Runner<'_>,
        _run_id: RunId,
        _backend: &BackendFacts,
        _prepared: &mut PreparedBackend,
    ) -> Result<Self> {
        Ok(Self)
    }

    fn workspace(&self) -> Option<PathBuf> {
        None
    }

    fn mark_accepted(&mut self, _state: &mut RunState) -> bool {
        true
    }

    fn start_network(
        &mut self,
        _runner: &Runner<'_>,
        _prepared: &PreparedBackend,
        _network: crate::core::NetworkControl,
        ready: bool,
        _state: &mut RunState,
    ) -> Result<bool> {
        Ok(ready)
    }

    fn requires_reconciliation(&self, _state: &RunState) -> bool {
        false
    }

    fn checkpoint_terminal(
        &mut self,
        _terminal_at: chrono::DateTime<Utc>,
        _state: &mut RunState,
    ) -> Result<Option<String>> {
        Ok(None)
    }

    fn close(&mut self, _network_cleanup_error: Option<String>) -> RunCleanup {
        RunCleanup::complete()
    }
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct RunStartResult {
    pub schema_version: u32,
    pub run_id: RunId,
    pub database: PathBuf,
    pub process: ProcessSlot,
    pub initial_image: OciDescriptor,
    pub final_image: ImageSlot,
    pub stdout: StoredBytes,
    pub stderr: StoredBytes,
    pub operation_errors: Vec<OperationError>,
    pub managed_service: Option<ManagedServiceFacts>,
    pub cleanup: RunCleanup,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RunCleanup {
    pub resources_absent: bool,
    pub errors: Vec<String>,
}

const RUN_START_RESULT_SCHEMA_VERSION: u32 = 3;

#[derive(Debug)]
pub struct RunResult {
    pub record: TerminalRunRecord,
    pub database: PathBuf,
    pub cli_exit_code: u8,
    pub cleanup: RunCleanup,
}

impl RunCleanup {
    const fn complete() -> Self {
        Self {
            resources_absent: true,
            errors: Vec::new(),
        }
    }

    #[cfg_attr(
        not(target_os = "linux"),
        allow(
            dead_code,
            reason = "only the native backend can leave resources behind"
        )
    )]
    fn pending(error: impl Into<String>) -> Self {
        Self {
            resources_absent: false,
            errors: vec![error.into()],
        }
    }

    fn validate(&self) -> Result<()> {
        if self.resources_absent == self.errors.is_empty() {
            Ok(())
        } else {
            bail!("Run cleanup status and errors are inconsistent")
        }
    }
}

pub struct Runner<'a> {
    database: &'a RunDatabase,
    images: &'a ImageService,
    backend: RunnerBackend<'a>,
}

enum RunnerBackend<'a> {
    Docker(&'a DockerBackend),
    #[cfg(target_os = "linux")]
    Native(&'a NativeBackend),
}

enum PreparedBackend {
    Docker(Box<DockerRuntime>),
    #[cfg(target_os = "linux")]
    Native(Box<PreparedNativeBackend>),
}

struct PreparedExecution<'a> {
    initial_manifest: &'a Digest,
    #[cfg_attr(
        not(target_os = "linux"),
        allow(
            dead_code,
            reason = "only the native backend realizes an OCI Runtime config"
        )
    )]
    runtime: &'a RuntimeConfig,
    controls: &'a RunControls,
    stdin: &'a [u8],
    run_id: RunId,
    directory: &'a Path,
}

struct TerminalInput {
    run_id: RunId,
    accepted_at: chrono::DateTime<Utc>,
    requested_image_reference: Option<String>,
    initial_image: OciDescriptor,
    runtime_config: StoredBytes,
    controls: RunControls,
}

impl<'a> Runner<'a> {
    #[must_use]
    pub const fn docker(
        database: &'a RunDatabase,
        images: &'a ImageService,
        docker: &'a DockerBackend,
    ) -> Self {
        Self {
            database,
            images,
            backend: RunnerBackend::Docker(docker),
        }
    }

    /// The Docker backend this Runner drives, if any. One place answers the
    /// question so the Docker paths do not each restate the backend match.
    #[cfg_attr(
        not(target_os = "linux"),
        allow(
            clippy::unnecessary_wraps,
            reason = "the native backend is the other answer on Linux"
        )
    )]
    fn docker_backend(&self) -> Option<&'a DockerBackend> {
        match self.backend {
            RunnerBackend::Docker(docker) => Some(docker),
            #[cfg(target_os = "linux")]
            RunnerBackend::Native(_) => None,
        }
    }

    fn prepare_backend(
        &self,
        initial: &ImageView,
        runtime: &RuntimeConfig,
        controls: &RunControls,
    ) -> Result<(BackendFacts, PreparedBackend)> {
        let prepared = match self.backend {
            RunnerBackend::Docker(docker) => {
                let preflight = docker.preflight_run(
                    runtime,
                    &self.images.image_config(&initial.manifest.digest)?,
                    controls.network,
                )?;
                (
                    preflight.facts,
                    PreparedBackend::Docker(Box::new(preflight.runtime)),
                )
            }
            #[cfg(target_os = "linux")]
            RunnerBackend::Native(backend) => {
                self.prepare_native_backend(backend, runtime, controls, initial)?
            }
        };
        if prepared.0.platform != initial.platform {
            bail!(
                "OCI Image platform does not match the execution backend: image {}, backend {}",
                initial.platform,
                prepared.0.platform
            );
        }
        Ok(prepared)
    }

    #[cfg_attr(
        not(target_os = "linux"),
        allow(unused_variables, reason = "only the native backend holds a Run scope")
    )]
    fn execute_prepared(
        &self,
        prepared: PreparedBackend,
        execution: &PreparedExecution<'_>,
        scope: &mut RunScope,
        state: &mut RunState,
    ) {
        match prepared {
            PreparedBackend::Docker(runtime) => self.execute_docker(
                execution.initial_manifest,
                &runtime,
                execution.controls,
                execution.stdin,
                execution.run_id,
                execution.directory,
                state,
            ),
            #[cfg(target_os = "linux")]
            PreparedBackend::Native(prepared) => {
                self.execute_native_primary(&prepared, execution, scope, state);
            }
        }
    }

    fn terminalize(
        &self,
        input: TerminalInput,
        mut state: RunState,
        scope: &mut RunScope,
    ) -> Result<RunResult> {
        self.cleanup(&mut state);
        if scope.requires_reconciliation(&state) {
            bail!("native resources require explicit reconciliation before terminalization");
        }
        let terminal_at = Utc::now();
        let network_cleanup_error = scope.checkpoint_terminal(terminal_at, &mut state)?;
        let record = TerminalRunRecord {
            schema_version: TERMINAL_RUN_RECORD_SCHEMA_VERSION,
            run_id: input.run_id,
            lifecycle: TerminalLifecycle::Terminal,
            accepted_at: input.accepted_at,
            terminal_at,
            requested_image_reference: input.requested_image_reference,
            initial_image: input.initial_image,
            runtime_config: input.runtime_config,
            controls: input.controls,
            backend: Some(state.backend),
            process: state.primary.process,
            stdout: state.primary.stdout,
            stderr: state.primary.stderr,
            final_image: state.primary.final_image,
            operation_errors: state.primary.operation_errors,
            managed_service: None,
        };
        self.database.terminal(
            &record,
            state.primary.stdout_bytes.as_deref(),
            state.primary.stderr_bytes.as_deref(),
        )?;
        let cleanup = scope.close(network_cleanup_error);
        cleanup.validate()?;
        let cli_exit_code = run_cli_exit_code_with_errors(
            &record.process,
            !record.operation_errors.is_empty() || !cleanup.resources_absent,
        );
        Ok(RunResult {
            record,
            database: self.database.path().to_path_buf(),
            cli_exit_code,
            cleanup,
        })
    }

    pub fn run_selected(
        &self,
        initial_manifest: &Digest,
        requested_image_reference: Option<&str>,
        runtime: &RuntimeConfig,
        runtime_bytes: &[u8],
        controls: RunControls,
        stdin: &[u8],
    ) -> Result<RunResult> {
        let initial = self.images.inspect(initial_manifest)?;
        let (backend, mut prepared) = self.prepare_backend(&initial, runtime, &controls)?;
        let run_id = RunId::new();
        let mut scope = RunScope::open(self, run_id, &backend, &mut prepared)?;
        let directory = create_run_directory(run_id, scope.workspace().as_deref())?;
        let (accepted_at, runtime_slot) = self.accept_single_run(
            run_id,
            &initial.manifest,
            requested_image_reference,
            runtime_bytes,
            &controls,
            stdin,
        )?;

        let mut state = RunState::new(backend);
        let accepted = scope.mark_accepted(&mut state);
        let execution_ready =
            scope.start_network(self, &prepared, controls.network, accepted, &mut state)?;
        if execution_ready {
            self.execute_prepared(
                prepared,
                &PreparedExecution {
                    initial_manifest: &initial.manifest.digest,
                    runtime,
                    controls: &controls,
                    stdin,
                    run_id,
                    directory: directory.path(),
                },
                &mut scope,
                &mut state,
            );
        }
        self.terminalize(
            TerminalInput {
                run_id,
                accepted_at,
                requested_image_reference: requested_image_reference.map(ToOwned::to_owned),
                initial_image: initial.manifest,
                runtime_config: runtime_slot,
                controls,
            },
            state,
            &mut scope,
        )
    }

    fn accept_single_run(
        &self,
        run_id: RunId,
        initial_image: &OciDescriptor,
        requested_image_reference: Option<&str>,
        runtime_bytes: &[u8],
        controls: &RunControls,
        stdin: &[u8],
    ) -> Result<(chrono::DateTime<Utc>, StoredBytes)> {
        let accepted_at = Utc::now();
        let runtime_config = available_bytes(runtime_bytes)?;
        let record = AcceptedRunRecord {
            schema_version: ACCEPTED_RUN_RECORD_SCHEMA_VERSION,
            run_id,
            lifecycle: AcceptedLifecycle::Accepted,
            accepted_at,
            requested_image_reference: requested_image_reference.map(ToOwned::to_owned),
            initial_image: initial_image.clone(),
            runtime_config: runtime_config.clone(),
            controls: controls.clone(),
            managed_service: None,
        };
        self.database.accept(&record, runtime_bytes, stdin)?;
        Ok((accepted_at, runtime_config))
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the explicit accepted inputs and mutable Run facts make the lifecycle boundary visible"
    )]
    #[allow(
        clippy::infallible_destructuring_match,
        reason = "the backend sum has one variant on non-Linux and two variants on Linux"
    )]
    fn execute_docker(
        &self,
        initial_manifest: &Digest,
        runtime: &DockerRuntime,
        controls: &RunControls,
        stdin: &[u8],
        run_id: RunId,
        directory: &Path,
        state: &mut RunState,
    ) {
        let docker = self
            .docker_backend()
            .expect("Docker execution requires the Docker backend");
        let image_adapter = DockerImageAdapter::new(self.images, docker);
        let docker_image = match image_adapter.materialize(initial_manifest) {
            Ok(image) => image,
            Err(error) => {
                state.primary.fail_before_start("materialize", &error);
                return;
            }
        };
        let container =
            match docker.create_run_container(&docker_image, run_id, runtime, controls.network) {
                Ok(container) => container,
                Err(error) => {
                    state.primary.fail_before_start("container_create", &error);
                    return;
                }
            };
        state.container = Some(container.clone());
        let stdout_path = directory.join("stdout");
        let stderr_path = directory.join("stderr");
        let attached = match docker.start_attached(
            &container,
            stdin.to_vec(),
            &stdout_path,
            &stderr_path,
            controls,
        ) {
            Ok(attached) => attached,
            Err(error) => {
                state.primary.fail_before_start("process_start", &error);
                return;
            }
        };
        state.primary.process =
            Self::docker_process_facts(docker, &container, &attached, &mut state.primary);
        let stdout = capture_stream(
            &stdout_path,
            controls.stdout_limit_bytes,
            "stdout",
            attached.stop_reason == Some(StopReason::StdoutLimit),
        );
        let stderr = capture_stream(
            &stderr_path,
            controls.stderr_limit_bytes,
            "stderr",
            attached.stop_reason == Some(StopReason::StderrLimit),
        );
        match stdout {
            Ok((slot, bytes)) => {
                state.primary.stdout = slot;
                state.primary.stdout_bytes = bytes;
            }
            Err(error) => state.primary.error("stdout_capture", &error),
        }
        match stderr {
            Ok((slot, bytes)) => {
                state.primary.stderr = slot;
                state.primary.stderr_bytes = bytes;
            }
            Err(error) => state.primary.error("stderr_capture", &error),
        }
        record_final_capture(
            &mut state.primary,
            image_adapter.freeze_run(&container, initial_manifest, &run_id.to_string()),
        );
    }

    fn docker_process_facts(
        docker: &DockerBackend,
        container: &str,
        attached: &AttachedResult,
        state: &mut ParticipantState,
    ) -> ProcessSlot {
        for message in &attached.operation_errors {
            state.operation_errors.push(OperationError {
                scope: OperationErrorScope::Primary,
                phase: "process_attach".to_owned(),
                message: message.clone(),
            });
        }
        let outcome = match attached.stop_reason {
            Some(StopReason::Cancelled) => ProcessOutcome::Cancelled,
            Some(StopReason::Timeout) => ProcessOutcome::TimedOut,
            Some(StopReason::StdoutLimit | StopReason::StderrLimit) => {
                ProcessOutcome::CaptureLimitExceeded
            }
            None => ProcessOutcome::ProcessExited,
        };
        match docker.inspect_container_state(container) {
            Ok(container_state) => {
                if !container_state.started {
                    return ProcessSlot::available(ProcessFacts {
                        terminal_outcome: ProcessOutcome::NotStarted,
                        exit_code: None,
                        started_at: None,
                        ended_at: Some(attached.ended_at),
                        oom_killed: None,
                        backend_error: container_state.error,
                    });
                }
                if attached.stop_reason.is_none()
                    && let Some(client_status) = attached.client_status
                    && client_status.code() != Some(container_state.exit_code)
                {
                    state.operation_errors.push(OperationError {
                        scope: OperationErrorScope::Primary,
                        phase: "process_wait".to_owned(),
                        message: format!(
                            "Docker client status {} differs from container exit code {}",
                            client_status, container_state.exit_code
                        ),
                    });
                }
                ProcessSlot::available(ProcessFacts {
                    terminal_outcome: outcome,
                    exit_code: Some(container_state.exit_code),
                    started_at: Some(attached.started_at),
                    ended_at: Some(attached.ended_at),
                    oom_killed: Some(container_state.oom_killed),
                    backend_error: container_state.error,
                })
            }
            Err(error) => {
                let message = format!("{error:#}");
                state.operation_errors.push(OperationError {
                    scope: state.scope,
                    phase: "process_inspect".to_owned(),
                    message: message.clone(),
                });
                ProcessSlot::Unavailable {
                    error: format!(
                        "Docker process facts are unavailable because container inspection failed: {message}"
                    ),
                }
            }
        }
    }

    #[allow(
        clippy::infallible_destructuring_match,
        reason = "the backend sum has one variant on non-Linux and two variants on Linux"
    )]
    fn cleanup(&self, state: &mut RunState) {
        let Some(container) = state.container.take() else {
            return;
        };
        let Some(docker) = self.docker_backend() else {
            state.primary.operation_errors.push(OperationError {
                scope: OperationErrorScope::Primary,
                phase: "container_cleanup".to_owned(),
                message: "Docker container identity was retained by a non-Docker backend"
                    .to_owned(),
            });
            return;
        };
        if let Err(error) = docker.remove_container(&container) {
            state.primary.error("container_cleanup", &error);
        }
    }
}

fn create_run_directory(run_id: RunId, workspace: Option<&Path>) -> Result<tempfile::TempDir> {
    let mut builder = tempfile::Builder::new();
    let prefix = format!("{run_id}-");
    builder.prefix(&prefix);
    let directory = match workspace {
        Some(workspace) => builder.tempdir_in(workspace),
        None => builder.tempdir(),
    };
    let directory = directory.context("failed to create Run working directory")?;
    ensure_private_directory(directory.path())?;
    Ok(directory)
}

fn record_final_capture(state: &mut ParticipantState, result: Result<CaptureResult>) {
    match result {
        Ok(capture) => {
            state.final_image = ImageSlot::Available {
                manifest: capture.image.manifest,
            };
            if let Some(message) = capture.cleanup_error {
                state.operation_errors.push(OperationError {
                    scope: OperationErrorScope::Primary,
                    phase: "capture_cleanup".to_owned(),
                    message,
                });
            }
        }
        Err(error) => {
            let message = format!("{error:#}");
            state.final_image = ImageSlot::Unavailable {
                error: message.clone(),
            };
            state.operation_errors.push(OperationError {
                scope: state.scope,
                phase: "final_image_capture".to_owned(),
                message,
            });
        }
    }
}

impl From<&RunResult> for RunStartResult {
    fn from(result: &RunResult) -> Self {
        Self {
            schema_version: RUN_START_RESULT_SCHEMA_VERSION,
            run_id: result.record.run_id,
            database: result.database.clone(),
            process: result.record.process.clone(),
            initial_image: result.record.initial_image.clone(),
            final_image: result.record.final_image.clone(),
            stdout: result.record.stdout.clone(),
            stderr: result.record.stderr.clone(),
            operation_errors: result.record.operation_errors.clone(),
            managed_service: result.record.managed_service.clone(),
            cleanup: result.cleanup.clone(),
        }
    }
}

struct RunState {
    backend: BackendFacts,
    primary: ParticipantState,
    container: Option<String>,
}

struct ParticipantState {
    scope: OperationErrorScope,
    process: ProcessSlot,
    stdout: StoredBytes,
    stderr: StoredBytes,
    stdout_bytes: Option<Vec<u8>>,
    stderr_bytes: Option<Vec<u8>>,
    final_image: ImageSlot,
    operation_errors: Vec<OperationError>,
}

impl RunState {
    fn new(backend: BackendFacts) -> Self {
        Self {
            backend,
            primary: ParticipantState::new(OperationErrorScope::Primary),
            container: None,
        }
    }
}

impl ParticipantState {
    fn new(scope: OperationErrorScope) -> Self {
        Self {
            scope,
            process: ProcessSlot::available(ProcessFacts::not_started()),
            stdout: StoredBytes::NotApplicable,
            stderr: StoredBytes::NotApplicable,
            stdout_bytes: None,
            stderr_bytes: None,
            final_image: ImageSlot::Unavailable {
                error: "process filesystem was not captured".to_owned(),
            },
            operation_errors: Vec::new(),
        }
    }

    fn fail_before_start(&mut self, phase: &str, error: &anyhow::Error) {
        let message = format!("{error:#}");
        self.process = ProcessSlot::available(ProcessFacts {
            terminal_outcome: ProcessOutcome::NotStarted,
            exit_code: None,
            started_at: None,
            ended_at: Some(Utc::now()),
            oom_killed: None,
            backend_error: Some(message.clone()),
        });
        self.operation_errors.push(OperationError {
            scope: self.scope,
            phase: phase.to_owned(),
            message,
        });
    }

    #[cfg_attr(
        not(target_os = "linux"),
        allow(dead_code, reason = "only the native backend reports this outcome")
    )]
    fn not_started(&mut self, reason: &str) {
        self.process = ProcessSlot::available(ProcessFacts {
            terminal_outcome: ProcessOutcome::NotStarted,
            exit_code: None,
            started_at: None,
            ended_at: Some(Utc::now()),
            oom_killed: None,
            backend_error: Some(reason.to_owned()),
        });
    }

    #[cfg_attr(
        not(target_os = "linux"),
        allow(dead_code, reason = "only the native backend reports this outcome")
    )]
    fn cancel_before_start(&mut self) {
        self.process = ProcessSlot::available(ProcessFacts {
            terminal_outcome: ProcessOutcome::Cancelled,
            exit_code: None,
            started_at: None,
            ended_at: Some(Utc::now()),
            oom_killed: None,
            backend_error: None,
        });
    }

    fn error(&mut self, phase: &str, error: &anyhow::Error) {
        self.operation_errors.push(OperationError {
            scope: self.scope,
            phase: phase.to_owned(),
            message: format!("{error:#}"),
        });
    }
}

fn available_bytes(bytes: &[u8]) -> Result<StoredBytes> {
    Ok(StoredBytes::Available {
        digest: digest_bytes(bytes),
        size: u64::try_from(bytes.len()).context("stored bytes size overflow")?,
    })
}

#[cfg(test)]
fn run_cli_exit_code(process: &ProcessSlot, operation_errors: &[OperationError]) -> u8 {
    run_cli_exit_code_with_errors(process, !operation_errors.is_empty())
}

fn run_cli_exit_code_with_errors(process: &ProcessSlot, has_operation_errors: bool) -> u8 {
    match process.facts() {
        Some(facts) if facts.terminal_outcome == ProcessOutcome::Cancelled => 130,
        Some(_) if !has_operation_errors => 0,
        Some(_) | None => 1,
    }
}

fn capture_stream(
    path: &Path,
    limit: u64,
    name: &str,
    stopped_at_limit: bool,
) -> Result<(StoredBytes, Option<Vec<u8>>)> {
    if !path.is_file() {
        return Ok((
            StoredBytes::Unavailable {
                error: format!("process {name} stream was not created"),
            },
            None,
        ));
    }
    let actual_size = path
        .metadata()
        .with_context(|| format!("failed to inspect process {name}"))?
        .len();
    let mut bytes = Vec::with_capacity(
        usize::try_from(actual_size.min(limit)).context("stream capture is too large")?,
    );
    File::open(path)
        .with_context(|| format!("failed to open process {name}"))?
        .take(limit)
        .read_to_end(&mut bytes)
        .with_context(|| format!("failed to read process {name}"))?;
    let digest = digest_bytes(&bytes);
    let size = u64::try_from(bytes.len()).context("stream capture size overflow")?;
    let slot = if actual_size > limit || stopped_at_limit {
        StoredBytes::Partial {
            digest,
            size,
            limit_bytes: limit,
            reason: format!("{name}_limit_exceeded"),
        }
    } else {
        StoredBytes::Available { digest, size }
    };
    Ok((slot, Some(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{Architecture, BackendDetails, ImageView, Platform};

    #[test]
    fn cleanup_failure_does_not_hide_published_final_asset() {
        let manifest = descriptor('1', "application/vnd.oci.image.manifest.v1+json");
        let capture = CaptureResult {
            image: ImageView {
                manifest: manifest.clone(),
                config: descriptor('2', "application/vnd.oci.image.config.v1+json"),
                platform: Platform::linux(Architecture::Arm64),
                layers: Vec::new(),
                diff_ids: Vec::new(),
                parent_manifest: None,
                added_layers: Vec::new(),
            },
            cleanup_error: Some("injected cleanup failure".to_owned()),
        };
        let mut state = RunState::new(BackendFacts {
            name: "docker".to_owned(),
            version: "test".to_owned(),
            platform: Platform::linux(Architecture::Arm64),
            network: crate::core::NetworkControl::None,
            run_network: None,
            details: BackendDetails::Docker {
                context: "default".to_owned(),
                endpoint_kind: "unix_socket".to_owned(),
                engine_id: "test".to_owned(),
            },
        });
        record_final_capture(&mut state.primary, Ok(capture));
        assert_eq!(state.primary.final_image, ImageSlot::Available { manifest });
        assert_eq!(state.primary.operation_errors.len(), 1);
        assert_eq!(state.primary.operation_errors[0].phase, "capture_cleanup");
    }

    #[test]
    fn cleanup_result_cannot_claim_absence_without_matching_evidence() {
        RunCleanup::complete().validate().expect("complete cleanup");
        RunCleanup {
            resources_absent: false,
            errors: vec!["attempt removal failed".to_owned()],
        }
        .validate()
        .expect("pending cleanup");
        assert!(
            RunCleanup {
                resources_absent: false,
                errors: Vec::new(),
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn cli_exit_status_separates_process_and_operation_outcomes() {
        let exited = ProcessSlot::available(ProcessFacts {
            terminal_outcome: ProcessOutcome::ProcessExited,
            exit_code: Some(7),
            started_at: None,
            ended_at: None,
            oom_killed: None,
            backend_error: None,
        });
        assert_eq!(run_cli_exit_code(&exited, &[]), 0);
        assert_eq!(
            run_cli_exit_code(
                &exited,
                &[OperationError {
                    scope: OperationErrorScope::Primary,
                    phase: "capture".to_owned(),
                    message: "failed".to_owned(),
                }]
            ),
            1
        );

        let unavailable = ProcessSlot::Unavailable {
            error: "evidence missing".to_owned(),
        };
        assert_eq!(run_cli_exit_code(&unavailable, &[]), 1);

        let cancelled = ProcessSlot::available(ProcessFacts {
            terminal_outcome: ProcessOutcome::Cancelled,
            exit_code: None,
            started_at: None,
            ended_at: None,
            oom_killed: None,
            backend_error: None,
        });
        assert_eq!(
            run_cli_exit_code(
                &cancelled,
                &[OperationError {
                    scope: OperationErrorScope::Primary,
                    phase: "cleanup".to_owned(),
                    message: "failed".to_owned(),
                }]
            ),
            130
        );
    }

    fn descriptor(digit: char, media_type: &str) -> OciDescriptor {
        OciDescriptor {
            digest: Digest::parse(format!("sha256:{}", digit.to_string().repeat(64)))
                .expect("digest"),
            size: 1,
            media_type: media_type.to_owned(),
        }
    }
}
