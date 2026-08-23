#[cfg(target_os = "linux")]
use std::fs;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
#[cfg(target_os = "linux")]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(target_os = "linux")]
use std::thread;
#[cfg(target_os = "linux")]
use std::time::{Duration, Instant};

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
#[cfg(target_os = "linux")]
use crate::core::{
    ManagedServiceCondition, ManagedServiceReadiness, NetworkControl, ServiceName,
    TcpReadinessCondition,
};
use crate::docker::{AttachedResult, DockerBackend, DockerImageAdapter, DockerRuntime, StopReason};
use crate::image::{CaptureResult, ImageService};
use crate::integrity::{digest_bytes, ensure_private_directory};
use crate::runtime::RuntimeConfig;
use crate::storage::RunDatabase;

#[cfg(target_os = "linux")]
mod managed;
#[cfg(target_os = "linux")]
use crate::{
    bundle::OciBundle,
    filesystem::{Inventory, TreeCapture},
    materialize::MaterializedRootfs,
    native::backend::{
        NativeBackend, NativeExecutionMode, NativePreflight, PreparedRuncRun, RuncCaptureLimits,
        RuncExecution, RuncOperationErrorKind, RuncRunFailure, RuncRunResult, RuncRunner,
        RuncStopReason,
    },
    native::fs::OverlayRootfs,
    native::network::{
        EgressNetworkTools, NativeNetworkBinding, NativeNetworkTools, NetworkHolderHandle,
        RunNetwork, RunNetworkMode,
    },
    native::read_only_file::{DestinationFileGuard, VerifiedSourceFile},
    native::recovery::{
        ManagedTerminalCheckpoint, NativeAttempt, NativeParticipant, NativeRecoveryPhase,
        NativeRecoveryStore, SharedNetworkCheckpoint, TerminalCheckpoint,
    },
    native::resolver::{
        ResolverConfig, ResolverProjection, ResolverProjectionPlan, ResolverSourceFile,
    },
    signal::TerminationFlag,
};

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

#[cfg(target_os = "linux")]
pub struct ManagedServiceInput<'a> {
    pub name: ServiceName,
    pub requested_image_reference: Option<&'a str>,
    pub initial_manifest: &'a Digest,
    pub runtime: &'a RuntimeConfig,
    pub runtime_bytes: &'a [u8],
    pub readiness: TcpReadinessCondition,
}

#[cfg(target_os = "linux")]
pub struct ManagedPrimaryInput<'a> {
    pub initial_manifest: &'a Digest,
    pub requested_image_reference: Option<&'a str>,
    pub runtime: &'a RuntimeConfig,
    pub runtime_bytes: &'a [u8],
    pub controls: RunControls,
    pub stdin: &'a [u8],
}

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

    #[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
struct PreparedNativeBackend {
    runner: RuncRunner,
    mode: NativeExecutionMode,
    realized_runtime: Option<RuntimeConfig>,
    capture_limits: RuncCaptureLimits,
    read_only_files: Vec<VerifiedSourceFile>,
    native_network_tools: Option<NativeNetworkTools>,
    egress_network_tools: Option<EgressNetworkTools>,
    resolver: Option<ResolverConfig>,
    resolver_source: Option<ResolverSourceFile>,
}

#[cfg(target_os = "linux")]
struct NativeExecution<'a> {
    runner: &'a RuncRunner,
    mode: NativeExecutionMode,
    initial_manifest: &'a Digest,
    bundle_runtime: &'a RuntimeConfig,
    controls: &'a RunControls,
    stdin: &'a [u8],
    run_id: RunId,
    capture_limits: RuncCaptureLimits,
    cancelled: &'a AtomicBool,
    lifecycle_stop: &'a AtomicBool,
    participant: NativeParticipant,
    network: Option<&'a NativeNetworkBinding>,
    read_only_files: &'a [VerifiedSourceFile],
    resolver_source: Option<&'a ResolverSourceFile>,
}

#[cfg(target_os = "linux")]
struct NativeProcessObservation {
    started_at: chrono::DateTime<Utc>,
    ended_at: chrono::DateTime<Utc>,
    result: std::result::Result<RuncRunResult, RuncRunFailure>,
}

#[cfg(target_os = "linux")]
struct ManagedNativeInput<'a> {
    run_id: RunId,
    controls: &'a RunControls,
    stdin: &'a [u8],
    primary_manifest: &'a Digest,
    primary_runtime: &'a RuntimeConfig,
    service_manifest: &'a Digest,
    service_runtime: &'a RuntimeConfig,
    capture_limits: RuncCaptureLimits,
    cancelled: &'a AtomicBool,
    service_timeout_seconds: u64,
    primary_files: &'a [VerifiedSourceFile],
    service_files: &'a [VerifiedSourceFile],
    resolver_source: Option<&'a ResolverSourceFile>,
}

#[cfg(target_os = "linux")]
struct ManagedPreparation {
    primary_image: ImageView,
    service_image: ImageView,
    service_timeout_seconds: u64,
    state_root: PathBuf,
    preflight: NativePreflight,
}

#[cfg(target_os = "linux")]
struct ManagedAcceptance {
    accepted_at: chrono::DateTime<Utc>,
    primary_image: OciDescriptor,
    primary_runtime: StoredBytes,
    condition: ManagedServiceCondition,
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy)]
struct ManagedExecutions<'a, 'execution> {
    primary: &'a NativeExecution<'execution>,
    service: &'a NativeExecution<'execution>,
}

#[cfg(target_os = "linux")]
struct PreparedManagedEnvironments<'runc> {
    primary: NativeEnvironment,
    primary_before: Inventory,
    service: NativeEnvironment,
    service_before: Inventory,
    service_runtime: Option<PreparedRuncRun<'runc>>,
}

#[cfg(target_os = "linux")]
struct ManagedObservations {
    primary: Option<NativeProcessObservation>,
    service: Option<NativeProcessObservation>,
}

#[cfg(target_os = "linux")]
struct ManagedExecutionStates<'a> {
    attempt: &'a mut NativeAttempt,
    primary: &'a mut ParticipantState,
    managed: &'a mut ManagedRunState,
}

struct PreparedExecution<'a> {
    initial_manifest: &'a Digest,
    #[cfg(target_os = "linux")]
    runtime: &'a RuntimeConfig,
    controls: &'a RunControls,
    stdin: &'a [u8],
    run_id: RunId,
    directory: &'a Path,
    #[cfg(target_os = "linux")]
    network: Option<&'a NativeNetworkBinding>,
}

struct TerminalInput {
    run_id: RunId,
    accepted_at: chrono::DateTime<Utc>,
    requested_image_reference: Option<String>,
    initial_image: OciDescriptor,
    runtime_config: StoredBytes,
    controls: RunControls,
}

#[cfg(target_os = "linux")]
struct ManagedRunState {
    condition: ManagedServiceCondition,
    readiness: Option<ManagedServiceReadiness>,
    participant: ParticipantState,
}

#[cfg(target_os = "linux")]
impl ManagedRunState {
    fn new(condition: ManagedServiceCondition) -> Self {
        Self {
            condition,
            readiness: None,
            participant: ParticipantState::new(OperationErrorScope::ManagedService),
        }
    }
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

    #[cfg(target_os = "linux")]
    #[must_use]
    pub(crate) const fn native(
        database: &'a RunDatabase,
        images: &'a ImageService,
        backend: &'a NativeBackend,
    ) -> Self {
        Self {
            database,
            images,
            backend: RunnerBackend::Native(backend),
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
                let rootless = !rustix::process::geteuid().is_root();
                if !rootless && controls.network == NetworkControl::Egress {
                    runtime.validate_native_resolver_destination()?;
                    self.images.verify_native_resolver_target(initial)?;
                }
                let state_root = self
                    .database
                    .path()
                    .parent()
                    .context("Run database path has no state root")?;
                let preflight = backend.preflight(runtime, controls, state_root)?;
                if preflight.mode.is_rootless() {
                    self.images.verify_rootless_image(
                        initial,
                        state_root,
                        preflight.mode.ownership(),
                    )?;
                }
                (
                    preflight.facts,
                    PreparedBackend::Native(Box::new(PreparedNativeBackend {
                        runner: preflight.runner,
                        mode: preflight.mode,
                        realized_runtime: preflight.realized_runtime,
                        capture_limits: preflight.capture_limits,
                        read_only_files: preflight.primary_files,
                        native_network_tools: preflight.native_network_tools,
                        egress_network_tools: preflight.egress_network_tools,
                        resolver: preflight.resolver,
                        resolver_source: None,
                    })),
                )
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

    fn execute_prepared(
        &self,
        prepared: PreparedBackend,
        execution: &PreparedExecution<'_>,
        #[cfg(target_os = "linux")] native_cancellation: Option<&TerminationFlag>,
        #[cfg(target_os = "linux")] native_attempt: Option<&mut NativeAttempt>,
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
                let lifecycle_stop = AtomicBool::new(false);
                self.execute_native(
                    &NativeExecution {
                        runner: &prepared.runner,
                        mode: prepared.mode,
                        initial_manifest: execution.initial_manifest,
                        bundle_runtime: prepared
                            .realized_runtime
                            .as_ref()
                            .unwrap_or(execution.runtime),
                        controls: execution.controls,
                        stdin: execution.stdin,
                        run_id: execution.run_id,
                        capture_limits: prepared.capture_limits,
                        cancelled: native_cancellation
                            .expect("native cancellation is registered")
                            .flag(),
                        lifecycle_stop: &lifecycle_stop,
                        participant: NativeParticipant::Primary,
                        network: execution.network,
                        read_only_files: &prepared.read_only_files,
                        resolver_source: prepared.resolver_source.as_ref(),
                    },
                    native_attempt.expect("native recovery attempt is owned"),
                    state,
                );
            }
        }
    }

    fn terminalize(
        &self,
        input: TerminalInput,
        mut state: RunState,
        #[cfg(target_os = "linux")] mut native_attempt: Option<NativeAttempt>,
        #[cfg(target_os = "linux")] native_network: Option<RunNetwork>,
    ) -> Result<RunResult> {
        self.cleanup(&mut state);

        #[cfg(target_os = "linux")]
        if native_attempt.is_some()
            && state
                .primary
                .operation_errors
                .iter()
                .any(|error| error.phase == "native_recovery")
        {
            bail!("native resources require explicit reconciliation before terminalization");
        }

        let terminal_at = Utc::now();
        #[cfg(target_os = "linux")]
        let mut network_cleanup_error = None;
        #[cfg(target_os = "linux")]
        if let Some(attempt) = native_attempt.as_mut() {
            if attempt.journal().shared_network().is_some()
                && let Err(error) = finish_run_network(attempt, native_network)
            {
                let operation_error = run_network_cleanup_error(&error);
                network_cleanup_error = Some(operation_error.message.clone());
                state.primary.operation_errors.push(operation_error);
            }
            attempt
                .prepare_terminal(TerminalCheckpoint {
                    terminal_at,
                    process: state.primary.process.clone(),
                    stdout: state.primary.stdout.clone(),
                    stderr: state.primary.stderr.clone(),
                    stdout_bytes: state.primary.stdout_bytes.as_deref(),
                    stderr_bytes: state.primary.stderr_bytes.as_deref(),
                    final_image: state.primary.final_image.clone(),
                    operation_errors: state.primary.operation_errors.clone(),
                })
                .context("failed to prepare native terminal recovery checkpoint")?;
        }
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
        #[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
        let mut cleanup = RunCleanup::complete();
        #[cfg(target_os = "linux")]
        if let Some(attempt) = native_attempt {
            if let Some(error) = network_cleanup_error {
                cleanup = RunCleanup::pending(error);
            } else if let Err(error) = attempt.remove_after_terminal() {
                cleanup = RunCleanup::pending(format!(
                    "terminal Run recovery cleanup is pending: {error:#}"
                ));
            }
        }
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
        #[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
        let (backend, mut prepared) = self.prepare_backend(&initial, runtime, &controls)?;

        #[cfg(target_os = "linux")]
        let native_cancellation = match prepared {
            PreparedBackend::Native(_) => Some(TerminationFlag::register()?),
            PreparedBackend::Docker(_) => None,
        };

        let run_id = RunId::new();
        #[cfg(target_os = "linux")]
        let mut native_attempt = self.prepare_native_attempt(run_id, &backend, &mut prepared)?;
        let directory = create_run_directory(
            run_id,
            #[cfg(target_os = "linux")]
            native_attempt.as_ref(),
        )?;
        let (accepted_at, runtime_slot) = self.accept_single_run(
            run_id,
            &initial.manifest,
            requested_image_reference,
            runtime_bytes,
            &controls,
            stdin,
        )?;

        let mut state = RunState::new(backend);
        #[cfg(target_os = "linux")]
        let native_ready = if let Some(attempt) = native_attempt.as_mut() {
            match attempt.advance_phase(NativeRecoveryPhase::Accepted) {
                Ok(()) => true,
                Err(error) => {
                    state
                        .primary
                        .fail_before_start("recovery_checkpoint", &error);
                    false
                }
            }
        } else {
            true
        };
        #[cfg(not(target_os = "linux"))]
        let native_ready = true;
        #[cfg(target_os = "linux")]
        let (native_network, native_binding, execution_ready) = self.prepare_single_run_network(
            &prepared,
            controls.network,
            native_ready,
            native_attempt.as_mut(),
            &mut state,
        )?;
        #[cfg(not(target_os = "linux"))]
        let execution_ready = native_ready;
        if execution_ready {
            self.execute_prepared(
                prepared,
                &PreparedExecution {
                    initial_manifest: &initial.manifest.digest,
                    #[cfg(target_os = "linux")]
                    runtime,
                    controls: &controls,
                    stdin,
                    run_id,
                    directory: directory.path(),
                    #[cfg(target_os = "linux")]
                    network: native_binding.as_ref(),
                },
                #[cfg(target_os = "linux")]
                native_cancellation.as_ref(),
                #[cfg(target_os = "linux")]
                native_attempt.as_mut(),
                &mut state,
            );
        }
        let result = self.terminalize(
            TerminalInput {
                run_id,
                accepted_at,
                requested_image_reference: requested_image_reference.map(ToOwned::to_owned),
                initial_image: initial.manifest,
                runtime_config: runtime_slot,
                controls,
            },
            state,
            #[cfg(target_os = "linux")]
            native_attempt,
            #[cfg(target_os = "linux")]
            native_network,
        )?;
        #[cfg(target_os = "linux")]
        drop(native_cancellation);
        Ok(result)
    }

    #[cfg(target_os = "linux")]
    fn prepare_native_attempt(
        &self,
        run_id: RunId,
        backend: &BackendFacts,
        prepared: &mut PreparedBackend,
    ) -> Result<Option<NativeAttempt>> {
        let PreparedBackend::Native(prepared) = prepared else {
            return Ok(None);
        };
        let state_root = self
            .database
            .path()
            .parent()
            .context("Run database path has no state root")?;
        let mut attempt =
            NativeRecoveryStore::open(state_root)?.prepare(run_id, backend.clone())?;
        let Some(resolver) = prepared.resolver.as_ref() else {
            return Ok(Some(attempt));
        };
        match attempt.prepare_resolver(resolver) {
            Ok(source) => {
                prepared.resolver_source = Some(source);
                Ok(Some(attempt))
            }
            Err(error) => match attempt.remove() {
                Ok(()) => Err(error),
                Err(cleanup) => Err(anyhow::anyhow!(
                    "{error:#}; pre-acceptance recovery cleanup also failed: {cleanup:#}"
                )),
            },
        }
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

    #[cfg(target_os = "linux")]
    fn prepare_single_run_network(
        &self,
        prepared: &PreparedBackend,
        control: crate::core::NetworkControl,
        native_ready: bool,
        attempt: Option<&mut NativeAttempt>,
        state: &mut RunState,
    ) -> Result<(Option<RunNetwork>, Option<NativeNetworkBinding>, bool)> {
        if !native_ready
            || control != crate::core::NetworkControl::Egress
            || !matches!(prepared, PreparedBackend::Native(_))
        {
            return Ok((None, None, native_ready));
        }
        let attempt = attempt.expect("native recovery attempt is owned");
        let state_root = self
            .database
            .path()
            .parent()
            .context("Run database path has no state root")?;
        let store = NativeRecoveryStore::open(state_root)?;
        let PreparedBackend::Native(prepared) = prepared else {
            unreachable!("native network preparation requires the native backend")
        };
        match start_run_network(
            &store,
            attempt,
            control,
            prepared
                .native_network_tools
                .clone()
                .expect("egress network tools were preflighted"),
            prepared.egress_network_tools.clone(),
            prepared.resolver.as_ref().map(ResolverConfig::facts),
        ) {
            Ok((network, binding)) => {
                state.backend = attempt.journal().backend().clone();
                Ok((Some(network), Some(binding), true))
            }
            Err(error) => {
                state.primary.fail_before_start("network_setup", &error);
                finish_run_network(attempt, None)?;
                Ok((None, None, false))
            }
        }
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
        let docker = match self.backend {
            RunnerBackend::Docker(docker) => docker,
            #[cfg(target_os = "linux")]
            RunnerBackend::Native(_) => {
                unreachable!("Docker preparation requires the Docker backend")
            }
        };
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

    #[cfg(target_os = "linux")]
    fn execute_native(
        &self,
        execution: &NativeExecution<'_>,
        attempt: &mut NativeAttempt,
        state: &mut RunState,
    ) {
        let Some((mut environment, before)) =
            self.prepare_native_environment(execution, attempt, &mut state.primary)
        else {
            return;
        };
        let Some((result, started_at, ended_at)) = Self::run_native_process(
            execution.runner,
            execution,
            attempt,
            &mut environment,
            &mut state.primary,
        ) else {
            return;
        };
        self.complete_native_participant(
            execution,
            attempt,
            &mut state.primary,
            environment,
            &before,
            &result,
            started_at,
            ended_at,
        );
    }

    #[cfg(target_os = "linux")]
    #[allow(
        clippy::too_many_arguments,
        reason = "completion atomically relates one participant's process and filesystem facts"
    )]
    fn complete_native_participant(
        &self,
        execution: &NativeExecution<'_>,
        attempt: &mut NativeAttempt,
        state: &mut ParticipantState,
        mut environment: NativeEnvironment,
        before: &Inventory,
        result: &RuncRunResult,
        started_at: chrono::DateTime<Utc>,
        ended_at: chrono::DateTime<Utc>,
    ) {
        if let Err(error) = attempt
            .advance_participant_phase(execution.participant, NativeRecoveryPhase::RuntimeActive)
        {
            state.error("recovery_checkpoint", &error);
            environment.cleanup_all_or_preserve(
                attempt,
                execution.participant,
                state,
                "runtime checkpoint cleanup failed",
            );
            return;
        }
        record_runc_result(state, result, started_at, ended_at, execution.controls);
        if let Err(error) = attempt.record_participant_process(
            execution.participant,
            state.process.clone(),
            state.stdout.clone(),
            state.stderr.clone(),
            state.stdout_bytes.as_deref(),
            state.stderr_bytes.as_deref(),
            state.operation_errors.clone(),
        ) {
            state.error("recovery_checkpoint", &error);
            environment.cleanup_all_or_preserve(
                attempt,
                execution.participant,
                state,
                "process checkpoint cleanup failed",
            );
            return;
        }
        if result.recovery.is_some() {
            state.operation_errors.push(OperationError {
                scope: state.scope,
                phase: "runtime_cleanup".to_owned(),
                message: "runc cleanup requires explicit reconciliation".to_owned(),
            });
            environment.preserve(state, "runc cleanup was incomplete");
            return;
        }
        if let Err(error) = environment.cleanup_resolver(attempt, execution.participant) {
            state.error("resolver_cleanup", &error);
            environment.preserve(state, "resolver projection cleanup failed");
            return;
        }
        if let Err(error) = environment.verify_runtime_mounts_removed() {
            state.error("runtime_mount_cleanup", &error);
            environment.preserve(state, "runtime mounts remain below rootfs");
            return;
        }
        if let Err(error) = environment.verify_file_destinations_unchanged() {
            state.error("read_only_file_mount_cleanup", &error);
            environment.preserve(
                state,
                "read-only file mount destination verification failed",
            );
            return;
        }
        self.capture_native_final(execution, attempt, state, environment, before);
    }

    #[cfg(target_os = "linux")]
    fn prepare_native_environment(
        &self,
        execution: &NativeExecution<'_>,
        attempt: &mut NativeAttempt,
        state: &mut ParticipantState,
    ) -> Option<(NativeEnvironment, Inventory)> {
        let (materialized, before) = self.prepare_native_lower(execution, attempt, state)?;
        let bundle_directory = match attempt.participant_bundle_directory(execution.participant) {
            Ok(path) => path,
            Err(error) => {
                state.fail_before_start("recovery_workspace", &error);
                return None;
            }
        };
        let bundle = match OciBundle::create_at(&bundle_directory, execution.bundle_runtime) {
            Ok(bundle) => bundle,
            Err(error) => {
                state.fail_before_start("bundle_create", &error);
                return None;
            }
        };
        if let Err(error) = attempt.advance_participant_phase(
            execution.participant,
            NativeRecoveryPhase::ExecutionPrepared,
        ) {
            state.fail_before_start("recovery_checkpoint", &error);
            return None;
        }
        if native_cancelled(execution.cancelled, state) {
            return None;
        }
        if let Err(error) = attempt.advance_participant_phase(
            execution.participant,
            NativeRecoveryPhase::OverlayMountPending,
        ) {
            state.fail_before_start("recovery_checkpoint", &error);
            return None;
        }
        let bundle_rootfs = match bundle.rootfs() {
            Ok(rootfs) => rootfs.to_path_buf(),
            Err(error) => {
                state.fail_before_start("bundle_verify", &error);
                return None;
            }
        };
        let filesystem_workspace =
            match attempt.participant_overlay_workspace(execution.participant) {
                Ok(path) => path,
                Err(error) => {
                    state.fail_before_start("recovery_workspace", &error);
                    return None;
                }
            };
        let mut environment = self.create_native_environment(
            execution,
            state,
            materialized,
            bundle,
            &bundle_rootfs,
            &filesystem_workspace,
        )?;
        if let Err(error) = environment.prepare_file_destinations(execution.read_only_files) {
            state.fail_before_start("read_only_file_mount_destination", &error);
            environment
                .cleanup_or_preserve(state, "read-only file mount destination validation failed");
            return None;
        }
        if let Err(error) = attempt
            .advance_participant_phase(execution.participant, NativeRecoveryPhase::OverlayMounted)
        {
            state.fail_before_start("recovery_checkpoint", &error);
            environment.preserve(state, "OverlayFS checkpoint failed");
            return None;
        }
        if !Self::prepare_resolver_projection(execution, attempt, state, &mut environment) {
            return None;
        }
        if native_cancelled(execution.cancelled, state) {
            environment.cleanup_all_or_preserve(
                attempt,
                execution.participant,
                state,
                "cancellation cleanup failed",
            );
            return None;
        }
        Some((environment, before))
    }

    #[cfg(target_os = "linux")]
    fn create_native_environment(
        &self,
        execution: &NativeExecution<'_>,
        state: &mut ParticipantState,
        materialized: MaterializedRootfs,
        bundle: OciBundle,
        bundle_rootfs: &Path,
        filesystem_workspace: &Path,
    ) -> Option<NativeEnvironment> {
        match execution.mode {
            NativeExecutionMode::Rootful => {
                let overlay = match OverlayRootfs::mount_at(
                    materialized.path(),
                    bundle_rootfs,
                    filesystem_workspace,
                ) {
                    Ok(overlay) => overlay,
                    Err(error) => {
                        state.fail_before_start("overlay_mount", &error);
                        return None;
                    }
                };
                Some(NativeEnvironment::new_overlay(
                    materialized,
                    bundle,
                    overlay,
                ))
            }
            NativeExecutionMode::Rootless { .. } => {
                let writable = match self.images.materialize_rootfs_at_with_ownership(
                    execution.initial_manifest,
                    filesystem_workspace,
                    execution.mode.ownership(),
                ) {
                    Ok(rootfs) => rootfs,
                    Err(error) => {
                        state.fail_before_start("writable_rootfs_materialize", &error);
                        return None;
                    }
                };
                if let Err(error) = adopt_writable_rootfs(&bundle, writable) {
                    state.fail_before_start("writable_rootfs_prepare", &error);
                    return None;
                }
                Some(NativeEnvironment::new_writable(materialized, bundle))
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn prepare_resolver_projection(
        execution: &NativeExecution<'_>,
        attempt: &mut NativeAttempt,
        state: &mut ParticipantState,
        environment: &mut NativeEnvironment,
    ) -> bool {
        let Some(source) = execution.resolver_source else {
            return true;
        };
        let plan = match environment
            .rootfs()
            .and_then(|rootfs| ResolverProjectionPlan::prepare(source.clone(), rootfs))
        {
            Ok(plan) => plan,
            Err(error) => {
                state.fail_before_start("resolver_projection", &error);
                environment.cleanup_or_preserve(state, "resolver projection preparation failed");
                return false;
            }
        };
        if let Err(error) =
            attempt.begin_resolver_mount(execution.participant, plan.pending().clone())
        {
            state.fail_before_start("recovery_checkpoint", &error);
            environment.cleanup_or_preserve(state, "resolver projection checkpoint failed");
            return false;
        }
        let projection = match plan.install() {
            Ok(projection) => projection,
            Err(error) => {
                state.fail_before_start("resolver_projection", &error);
                environment.preserve_borrowed(
                    state,
                    "resolver projection installation requires reconciliation",
                );
                return false;
            }
        };
        let mounted = projection.mounted();
        environment.resolver = Some(projection);
        if let Err(error) = attempt.record_resolver_mounted(execution.participant, mounted) {
            state.fail_before_start("recovery_checkpoint", &error);
            environment.preserve_borrowed(
                state,
                "resolver projection mount checkpoint requires reconciliation",
            );
            return false;
        }
        true
    }

    #[cfg(target_os = "linux")]
    fn prepare_native_lower(
        &self,
        execution: &NativeExecution<'_>,
        attempt: &NativeAttempt,
        state: &mut ParticipantState,
    ) -> Option<(MaterializedRootfs, Inventory)> {
        if native_cancelled(execution.cancelled, state) {
            return None;
        }
        let lower_workspace = match attempt.participant_lower_workspace(execution.participant) {
            Ok(path) => path,
            Err(error) => {
                state.fail_before_start("recovery_workspace", &error);
                return None;
            }
        };
        let materialized = match self.images.materialize_rootfs_at_with_ownership(
            execution.initial_manifest,
            &lower_workspace,
            execution.mode.ownership(),
        ) {
            Ok(rootfs) => rootfs,
            Err(error) => {
                state.fail_before_start("materialize", &error);
                return None;
            }
        };
        if native_cancelled(execution.cancelled, state) {
            return None;
        }
        let before = match TreeCapture::with_ownership(execution.mode.ownership())
            .capture_inventory(materialized.path())
        {
            Ok(capture) => capture,
            Err(error) => {
                state.fail_before_start("initial_filesystem_capture", &error);
                return None;
            }
        };
        if native_cancelled(execution.cancelled, state) {
            return None;
        }
        Some((materialized, before))
    }

    #[cfg(target_os = "linux")]
    fn run_native_process(
        runc: &RuncRunner,
        execution: &NativeExecution<'_>,
        attempt: &mut NativeAttempt,
        environment: &mut NativeEnvironment,
        state: &mut ParticipantState,
    ) -> Option<(RuncRunResult, chrono::DateTime<Utc>, chrono::DateTime<Utc>)> {
        let Some(prepared) = prepare_native_process_start(
            runc,
            execution.mode,
            attempt,
            execution.participant,
            state,
        ) else {
            environment.cleanup_all_or_preserve(
                attempt,
                execution.participant,
                state,
                "runtime preparation cleanup failed",
            );
            return None;
        };
        let process_terminal_observed = AtomicBool::new(false);
        let observation = run_native_process_observation(
            prepared,
            environment.bundle(),
            execution,
            &process_terminal_observed,
        );
        if let Some(observed) = observe_native_process(observation, execution.cancelled, state) {
            Some(observed)
        } else {
            environment.cleanup_all_or_preserve(
                attempt,
                execution.participant,
                state,
                "runc execution did not produce complete process facts",
            );
            None
        }
    }

    #[cfg(target_os = "linux")]
    fn capture_native_final(
        &self,
        execution: &NativeExecution<'_>,
        attempt: &mut NativeAttempt,
        state: &mut ParticipantState,
        mut environment: NativeEnvironment,
        before: &Inventory,
    ) {
        let captured_at = Utc::now();
        if let Err(error) = attempt.begin_participant_capture(execution.participant, captured_at) {
            state.error("recovery_checkpoint", &error);
            environment.cleanup_or_preserve(state, "capture checkpoint cleanup failed");
            return;
        }
        let workspace = match attempt.participant_workspace(execution.participant) {
            Ok(path) => path,
            Err(error) => {
                state.error("recovery_workspace", &error);
                environment.preserve(state, "capture workspace resolution failed");
                return;
            }
        };
        match environment.rootfs().and_then(|rootfs| {
            TreeCapture::with_ownership(execution.mode.ownership()).capture_in(rootfs, &workspace)
        }) {
            Ok(after) => {
                record_final_capture(
                    state,
                    self.images.capture_filesystem(
                        execution.initial_manifest,
                        before,
                        &after,
                        &execution.run_id,
                        captured_at,
                        &workspace,
                    ),
                );
            }
            Err(error) => state.error("final_filesystem_capture", &error),
        }
        if let Err(error) =
            attempt.record_participant_final(execution.participant, state.final_image.clone())
        {
            state.error("recovery_checkpoint", &error);
            environment.preserve(state, "Final Image checkpoint failed");
            return;
        }
        if let Err(error) = attempt
            .advance_participant_phase(execution.participant, NativeRecoveryPhase::CleanupPending)
        {
            state.error("recovery_checkpoint", &error);
            environment.preserve(state, "cleanup checkpoint failed");
            return;
        }
        environment.cleanup_or_preserve(state, "OverlayFS unmount failed");
        if !state
            .operation_errors
            .iter()
            .any(|error| matches!(error.phase.as_str(), "overlay_cleanup" | "native_recovery"))
            && let Err(error) = attempt.advance_participant_phase(
                execution.participant,
                NativeRecoveryPhase::CleanupComplete,
            )
        {
            state.error("recovery_checkpoint", &error);
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
        let docker = match self.backend {
            RunnerBackend::Docker(docker) => docker,
            #[cfg(target_os = "linux")]
            RunnerBackend::Native(_) => {
                state.primary.operation_errors.push(OperationError {
                    scope: OperationErrorScope::Primary,
                    phase: "container_cleanup".to_owned(),
                    message: "Docker container identity was retained by a non-Docker backend"
                        .to_owned(),
                });
                return;
            }
        };
        if let Err(error) = docker.remove_container(&container) {
            state.primary.error("container_cleanup", &error);
        }
    }
}

#[cfg(target_os = "linux")]
fn run_managed_observations(
    runc: &RuncRunner,
    network: &NativeNetworkBinding,
    input: &ManagedNativeInput<'_>,
    executions: ManagedExecutions<'_, '_>,
    prepared: &mut PreparedManagedEnvironments<'_>,
    states: ManagedExecutionStates<'_>,
) -> ManagedObservations {
    let ManagedExecutionStates {
        attempt,
        primary,
        managed,
    } = states;
    let service_finished = AtomicBool::new(false);
    let service_terminal_observed = AtomicBool::new(false);
    let primary_terminal_observed = AtomicBool::new(false);
    let primary_observation_complete = AtomicBool::new(false);
    let service_requested_primary_stop = AtomicBool::new(false);
    let mut primary_observation = None;
    let service_runtime = prepared
        .service_runtime
        .take()
        .expect("Managed Service runtime resources are prepared");
    let service_observation = thread::scope(|scope| {
        let service_handle = scope.spawn(|| {
            let observation = run_native_process_observation(
                service_runtime,
                prepared.service.bundle(),
                executions.service,
                &service_terminal_observed,
            );
            service_finished.store(true, Ordering::Release);
            observation
        });
        let readiness = wait_for_managed_readiness(
            network,
            &managed.condition.readiness,
            &service_finished,
            input.cancelled,
        );
        managed.readiness = Some(readiness.clone());
        let readiness_persisted = match attempt.record_managed_readiness(readiness.clone()) {
            Ok(()) => true,
            Err(error) => {
                managed.participant.error("recovery_checkpoint", &error);
                primary.fail_before_start("recovery_checkpoint", &error);
                false
            }
        };
        if readiness_persisted && matches!(readiness, ManagedServiceReadiness::Ready { .. }) {
            if let Some(primary_runtime) = prepare_native_process_start(
                runc,
                NativeExecutionMode::Rootful,
                attempt,
                NativeParticipant::Primary,
                primary,
            ) {
                let watcher = scope.spawn(|| {
                    let requested = request_primary_stop_after_service_exit(
                        &service_terminal_observed,
                        &primary_terminal_observed,
                        &primary_observation_complete,
                        executions.primary.lifecycle_stop,
                    );
                    service_requested_primary_stop.store(requested, Ordering::Release);
                });
                primary_observation = Some(run_native_process_observation(
                    primary_runtime,
                    prepared.primary.bundle(),
                    executions.primary,
                    &primary_terminal_observed,
                ));
                primary_observation_complete.store(true, Ordering::Release);
                let _ = watcher.join();
            }
        } else if readiness_persisted {
            if matches!(readiness, ManagedServiceReadiness::Cancelled { .. }) {
                primary.cancel_before_start();
            } else {
                primary.not_started(readiness_failure_message(&readiness));
                primary.operation_errors.push(OperationError {
                    scope: OperationErrorScope::Run,
                    phase: "managed_service_readiness".to_owned(),
                    message: readiness_failure_message(&readiness).to_owned(),
                });
            }
        }
        executions
            .service
            .lifecycle_stop
            .store(true, Ordering::Release);
        service_handle.join()
    });
    finish_managed_observations(
        service_observation,
        service_requested_primary_stop.load(Ordering::Acquire),
        primary_observation,
        primary,
        managed,
    )
}

#[cfg(target_os = "linux")]
fn finish_managed_observations(
    service_observation: thread::Result<NativeProcessObservation>,
    service_requested_primary_stop: bool,
    primary_observation: Option<NativeProcessObservation>,
    primary: &mut ParticipantState,
    managed: &mut ManagedRunState,
) -> ManagedObservations {
    let service = if let Ok(observation) = service_observation {
        Some(observation)
    } else {
        managed.participant.fail_before_start(
            "process_execute",
            &anyhow::anyhow!("Managed Service execution thread panicked"),
        );
        None
    };
    let primary_stop_reason = primary_observation
        .as_ref()
        .and_then(|observation| observation.result.as_ref().ok())
        .and_then(|result| result.stop_reason);
    record_managed_service_loss(
        managed_service_loss_confirmed(service_requested_primary_stop, primary_stop_reason),
        primary,
    );
    ManagedObservations {
        primary: primary_observation,
        service,
    }
}

#[cfg(target_os = "linux")]
fn request_primary_stop_after_service_exit(
    service_terminal_observed: &AtomicBool,
    primary_terminal_observed: &AtomicBool,
    primary_observation_complete: &AtomicBool,
    primary_lifecycle_stop: &AtomicBool,
) -> bool {
    loop {
        if primary_terminal_observed.load(Ordering::Acquire)
            || primary_observation_complete.load(Ordering::Acquire)
        {
            return false;
        }
        if service_terminal_observed.load(Ordering::Acquire) {
            if primary_terminal_observed.load(Ordering::Acquire)
                || primary_observation_complete.load(Ordering::Acquire)
            {
                return false;
            }
            primary_lifecycle_stop.store(true, Ordering::Release);
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(target_os = "linux")]
fn record_managed_service_loss(lost: bool, primary: &mut ParticipantState) {
    if lost {
        primary.operation_errors.push(OperationError {
            scope: OperationErrorScope::Run,
            phase: "managed_service_lost".to_owned(),
            message: "Managed Service exited after readiness and before Primary completion"
                .to_owned(),
        });
    }
}

#[cfg(target_os = "linux")]
fn managed_service_loss_confirmed(
    service_requested_primary_stop: bool,
    primary_stop_reason: Option<RuncStopReason>,
) -> bool {
    service_requested_primary_stop && primary_stop_reason == Some(RuncStopReason::LifecycleStop)
}

#[cfg(target_os = "linux")]
fn run_native_process_observation(
    prepared: PreparedRuncRun<'_>,
    bundle: &OciBundle,
    execution: &NativeExecution<'_>,
    process_terminal_observed: &AtomicBool,
) -> NativeProcessObservation {
    let started_at = Utc::now();
    let runc_execution = RuncExecution {
        stdin: execution.stdin,
        timeout: Duration::from_secs(execution.controls.timeout_seconds),
        capture_limits: execution.capture_limits,
        cancelled: execution.cancelled,
        lifecycle_stop: execution.lifecycle_stop,
        process_terminal_observed,
        read_only_files: execution.read_only_files,
    };
    let result = prepared.execute(bundle, runc_execution, execution.network);
    NativeProcessObservation {
        started_at,
        ended_at: Utc::now(),
        result,
    }
}

#[cfg(target_os = "linux")]
fn prepare_native_process_start<'runc>(
    runc: &'runc RuncRunner,
    mode: NativeExecutionMode,
    attempt: &mut NativeAttempt,
    participant: NativeParticipant,
    state: &mut ParticipantState,
) -> Option<PreparedRuncRun<'runc>> {
    let runtime_root = match attempt.participant_runtime_root(participant) {
        Ok(path) => path,
        Err(error) => {
            state.fail_before_start("recovery_workspace", &error);
            return None;
        }
    };
    let runtime_id = match attempt.participant_runtime_id(participant) {
        Ok(id) => id.to_owned(),
        Err(error) => {
            state.fail_before_start("recovery_workspace", &error);
            return None;
        }
    };
    let prepared = match runc.prepare_at(&runtime_root, &runtime_id, mode.is_rootless()) {
        Ok(prepared) => prepared,
        Err(error) => {
            state.fail_before_start("runtime_prepare", &error);
            return None;
        }
    };
    if let Err(error) =
        attempt.advance_participant_phase(participant, NativeRecoveryPhase::RuntimeStartPending)
    {
        state.fail_before_start("recovery_checkpoint", &error);
        if let Err(cleanup) = prepared.cleanup() {
            state.error("runtime_cleanup", &cleanup);
        }
        return None;
    }
    Some(prepared)
}

#[cfg(target_os = "linux")]
fn wait_for_managed_readiness(
    network: &NativeNetworkBinding,
    condition: &TcpReadinessCondition,
    service_finished: &AtomicBool,
    cancelled: &AtomicBool,
) -> ManagedServiceReadiness {
    let started = Instant::now();
    let timeout = Duration::from_secs(condition.timeout_seconds);
    let Some(deadline) = started.checked_add(timeout) else {
        return readiness_probe_error(0, "Managed Service readiness deadline overflow");
    };
    let executable = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            return readiness_probe_error(
                0,
                &format!("failed to resolve readiness probe executable: {error}"),
            );
        }
    };
    let mut attempts = 0_u32;
    loop {
        if cancelled.load(Ordering::Acquire) {
            return ManagedServiceReadiness::Cancelled {
                observed_at: Utc::now(),
                attempts,
            };
        }
        if service_finished.load(Ordering::Acquire) {
            return ManagedServiceReadiness::ServiceExited {
                observed_at: Utc::now(),
                attempts,
            };
        }
        let now = Instant::now();
        if now >= deadline {
            return ManagedServiceReadiness::TimedOut {
                observed_at: Utc::now(),
                attempts,
            };
        }
        attempts = match attempts.checked_add(1) {
            Some(value) => value,
            None => return readiness_probe_error(attempts, "readiness attempt count overflow"),
        };
        let remaining = deadline.saturating_duration_since(now);
        let probe_timeout = remaining.min(Duration::from_millis(250));
        let arguments = [
            "__internal-tcp-probe".to_owned(),
            "--port".to_owned(),
            condition.port.to_string(),
            "--timeout-milliseconds".to_owned(),
            probe_timeout.as_millis().max(1).to_string(),
        ];
        match network.invoke(
            &executable,
            arguments,
            probe_timeout + Duration::from_secs(1),
        ) {
            Ok(output) if output.status.success() => {
                return ManagedServiceReadiness::Ready {
                    observed_at: Utc::now(),
                    attempts,
                };
            }
            Ok(output) if output.status.code() == Some(75) => {}
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
                let message = if stderr.is_empty() {
                    format!("readiness probe exited with {}", output.status)
                } else {
                    format!(
                        "readiness probe exited with {}; stderr: {stderr}",
                        output.status
                    )
                };
                return readiness_probe_error(attempts, &message);
            }
            Err(error) => {
                return readiness_probe_error(
                    attempts,
                    &format!("readiness probe failed: {error}"),
                );
            }
        }
        thread::sleep(
            deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(50)),
        );
    }
}

#[cfg(target_os = "linux")]
fn readiness_probe_error(attempts: u32, error: &str) -> ManagedServiceReadiness {
    ManagedServiceReadiness::ProbeError {
        observed_at: Utc::now(),
        attempts,
        error: error.to_owned(),
    }
}

#[cfg(target_os = "linux")]
fn readiness_failure_message(readiness: &ManagedServiceReadiness) -> &'static str {
    match readiness {
        ManagedServiceReadiness::Ready { .. } => "Managed Service is ready",
        ManagedServiceReadiness::TimedOut { .. } => "Managed Service readiness timed out",
        ManagedServiceReadiness::ServiceExited { .. } => "Managed Service exited before readiness",
        ManagedServiceReadiness::Cancelled { .. } => {
            "Run was cancelled during Managed Service readiness"
        }
        ManagedServiceReadiness::ProbeError { .. } => "Managed Service readiness probe failed",
    }
}

#[cfg(target_os = "linux")]
fn start_run_network(
    recovery: &NativeRecoveryStore,
    attempt: &mut NativeAttempt,
    control: crate::core::NetworkControl,
    native_tools: NativeNetworkTools,
    egress_tools: Option<EgressNetworkTools>,
    resolver: Option<crate::core::RunResolverFacts>,
) -> Result<(RunNetwork, NativeNetworkBinding)> {
    if (control == crate::core::NetworkControl::Egress) != egress_tools.is_some() {
        bail!("preflighted egress network tools do not match the accepted network control");
    }
    let reservation = recovery.reserve_network_plan(
        attempt,
        control,
        egress_tools.as_ref(),
        Duration::from_secs(5),
    )?;
    let holder = NetworkHolderHandle::prepare(&attempt.workspace(), attempt.journal().run_id())
        .context("failed to prepare the durable Run network holder")?;
    let mut network = RunNetwork::start_from_persisted_plan_durable(
        reservation.plan(),
        holder,
        native_tools,
        egress_tools,
        reservation.host_lock(),
        Duration::from_secs(5),
    )
    .context("failed to create Run network")?;
    let resources = network.resources().clone();
    let binding = match network.binding() {
        Ok(binding) => binding,
        Err(error) => {
            drop(reservation);
            let cleanup = network.finish();
            return match cleanup {
                Ok(()) => Err(error).context("failed to bind the Run network namespace"),
                Err(cleanup) => Err(anyhow::anyhow!(
                    "failed to bind the Run network namespace: {error}; cleanup also failed: {cleanup}"
                )),
            };
        }
    };
    let checkpoint = match SharedNetworkCheckpoint::from_resources(&resources, resolver) {
        Ok(checkpoint) => checkpoint,
        Err(error) => {
            drop(reservation);
            let cleanup = network.finish();
            return match cleanup {
                Ok(()) => Err(error).context("failed to construct the Run network checkpoint"),
                Err(cleanup) => Err(anyhow::anyhow!(
                    "failed to construct the Run network checkpoint: {error:#}; cleanup also failed: {cleanup}"
                )),
            };
        }
    };
    if let Err(error) = attempt.record_shared_network(checkpoint) {
        drop(reservation);
        let cleanup = network.finish();
        return match cleanup {
            Ok(()) => Err(error).context("failed to checkpoint the active Run network"),
            Err(cleanup) => Err(anyhow::anyhow!(
                "failed to checkpoint the active Run network: {error:#}; cleanup also failed: {cleanup}"
            )),
        };
    }
    drop(reservation);
    Ok((network, binding))
}

fn create_run_directory(
    run_id: RunId,
    #[cfg(target_os = "linux")] attempt: Option<&NativeAttempt>,
) -> Result<tempfile::TempDir> {
    let mut builder = tempfile::Builder::new();
    let prefix = format!("{run_id}-");
    builder.prefix(&prefix);
    #[cfg(target_os = "linux")]
    let directory = match attempt {
        Some(attempt) => builder.tempdir_in(attempt.workspace()),
        None => builder.tempdir(),
    };
    #[cfg(not(target_os = "linux"))]
    let directory = builder.tempdir();
    let directory = directory.context("failed to create Run working directory")?;
    ensure_private_directory(directory.path())?;
    Ok(directory)
}

#[cfg(target_os = "linux")]
fn finish_run_network(attempt: &mut NativeAttempt, network: Option<RunNetwork>) -> Result<()> {
    let journal = attempt
        .journal()
        .shared_network()
        .context("native attempt has no Run network")?;
    if journal.phase() == crate::native::recovery::NativeSharedNetworkPhase::CleanupComplete {
        return Ok(());
    }
    let plan = journal.plan().cloned();
    attempt.begin_shared_network_cleanup()?;
    if let Some(network) = network {
        network.finish().context("failed to stop Run network")?;
    } else {
        if let Some(plan) = plan
            && plan.mode() == RunNetworkMode::EgressIpv4
        {
            EgressNetworkTools::discover()
                .context("native egress cleanup tools are unavailable")?
                .cleanup_plan(&plan, Duration::from_secs(5))
                .context("failed to clean Run egress resources")?;
        }
        if let Some(holder) =
            NetworkHolderHandle::open(&attempt.workspace(), attempt.journal().run_id())
                .context("failed to open the durable Run network holder")?
        {
            holder
                .request_stop(Duration::from_secs(5))
                .context("failed to stop the durable Run network holder")?;
        }
    }
    attempt.record_shared_network_cleanup(Utc::now())
}

#[cfg(target_os = "linux")]
fn run_network_cleanup_error(error: &anyhow::Error) -> OperationError {
    OperationError {
        scope: OperationErrorScope::Run,
        phase: "resource_cleanup".to_owned(),
        message: format!("run_network_cleanup: {error:#}"),
    }
}

#[cfg(target_os = "linux")]
fn prepare_resolver_source(
    attempt: &mut NativeAttempt,
    resolver: Option<&ResolverConfig>,
) -> Result<Option<ResolverSourceFile>> {
    resolver
        .map(|resolver| attempt.prepare_resolver(resolver))
        .transpose()
}

#[cfg(target_os = "linux")]
fn abandon_managed_pre_acceptance(
    attempt: NativeAttempt,
    error: anyhow::Error,
) -> Result<RunResult> {
    match attempt.remove() {
        Ok(()) => Err(error),
        Err(cleanup) => Err(anyhow::anyhow!(
            "{error:#}; pre-acceptance recovery cleanup also failed: {cleanup:#}"
        )),
    }
}

#[cfg(target_os = "linux")]
fn verify_platform(participant: &str, image: &ImageView, backend: &BackendFacts) -> Result<()> {
    if image.platform != backend.platform {
        bail!(
            "{participant} OCI Image platform does not match the execution backend: image {}, backend {}",
            image.platform,
            backend.platform
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn mark_managed_accepted(
    attempt: &mut NativeAttempt,
    primary: &mut ParticipantState,
    service: &mut ParticipantState,
) -> bool {
    let checkpoint = attempt
        .advance_participant_phase(NativeParticipant::Primary, NativeRecoveryPhase::Accepted)
        .and_then(|()| {
            attempt.advance_participant_phase(
                NativeParticipant::ManagedService,
                NativeRecoveryPhase::Accepted,
            )
        });
    if let Err(error) = checkpoint {
        primary.fail_before_start("recovery_checkpoint", &error);
        service.fail_before_start("recovery_checkpoint", &error);
        return false;
    }
    true
}

#[cfg(target_os = "linux")]
fn observe_native_process(
    observation: NativeProcessObservation,
    cancelled: &AtomicBool,
    state: &mut ParticipantState,
) -> Option<(RuncRunResult, chrono::DateTime<Utc>, chrono::DateTime<Utc>)> {
    let result = match observation.result {
        Ok(result) => result,
        Err(error) => {
            let started = error.init_pid.is_some();
            state.process = if started {
                ProcessSlot::Unavailable {
                    error: format!(
                        "native process terminal facts are unavailable after runtime failure: {error:#}"
                    ),
                }
            } else if cancelled.load(std::sync::atomic::Ordering::Acquire) {
                ProcessSlot::available(ProcessFacts {
                    terminal_outcome: ProcessOutcome::Cancelled,
                    exit_code: None,
                    started_at: None,
                    ended_at: Some(observation.ended_at),
                    oom_killed: None,
                    backend_error: Some(format!("{error:#}")),
                })
            } else {
                ProcessSlot::available(ProcessFacts {
                    terminal_outcome: ProcessOutcome::NotStarted,
                    exit_code: None,
                    started_at: None,
                    ended_at: Some(observation.ended_at),
                    oom_killed: None,
                    backend_error: Some(format!("{error:#}")),
                })
            };
            state.operation_errors.push(OperationError {
                scope: state.scope,
                phase: "process_execute".to_owned(),
                message: error.to_string(),
            });
            return None;
        }
    };
    if result.init_pid.is_none() {
        let diagnostic = String::from_utf8_lossy(&result.stderr.bytes)
            .trim()
            .to_owned();
        state.process = ProcessSlot::available(ProcessFacts {
            terminal_outcome: ProcessOutcome::NotStarted,
            exit_code: None,
            started_at: None,
            ended_at: Some(observation.ended_at),
            oom_killed: None,
            backend_error: Some(if diagnostic.is_empty() {
                "runc exited without proving that the configured process started".to_owned()
            } else {
                format!("runc failed before process start: {diagnostic}")
            }),
        });
        state.operation_errors.push(OperationError {
            scope: state.scope,
            phase: "process_start".to_owned(),
            message: "runc exited without creating its init pid file".to_owned(),
        });
        return None;
    }
    Some((result, observation.started_at, observation.ended_at))
}

#[cfg(target_os = "linux")]
fn native_cancelled(cancelled: &AtomicBool, state: &mut ParticipantState) -> bool {
    if cancelled.load(std::sync::atomic::Ordering::Acquire) {
        state.cancel_before_start();
        true
    } else {
        false
    }
}

#[cfg(target_os = "linux")]
fn record_runc_result(
    state: &mut ParticipantState,
    result: &RuncRunResult,
    started_at: chrono::DateTime<Utc>,
    ended_at: chrono::DateTime<Utc>,
    controls: &RunControls,
) {
    if result.state_before_delete.is_none()
        && !result
            .operation_errors
            .iter()
            .any(|error| error.kind == RuncOperationErrorKind::StateObservation)
    {
        state.operation_errors.push(OperationError {
            scope: state.scope,
            phase: "process_observation".to_owned(),
            message: "runc returned neither stopped state nor an observation error".to_owned(),
        });
    }
    let outcome = match result.stop_reason {
        Some(RuncStopReason::Cancelled) => ProcessOutcome::Cancelled,
        Some(RuncStopReason::DeadlineExceeded) => ProcessOutcome::TimedOut,
        Some(RuncStopReason::StdoutLimitExceeded | RuncStopReason::StderrLimitExceeded) => {
            ProcessOutcome::CaptureLimitExceeded
        }
        Some(RuncStopReason::LifecycleStop) | None => ProcessOutcome::ProcessExited,
    };
    let facts = ProcessFacts {
        terminal_outcome: outcome,
        exit_code: result.foreground_status.code(),
        started_at: Some(started_at),
        ended_at: Some(ended_at),
        oom_killed: result.oom_killed,
        backend_error: None,
    };
    state.process = match facts.validate() {
        Ok(()) => ProcessSlot::available(facts),
        Err(error) => ProcessSlot::Unavailable {
            error: format!("native process terminal facts are incomplete: {error:#}"),
        },
    };
    let stdout_reason = (result.stop_reason == Some(RuncStopReason::StdoutLimitExceeded))
        .then_some("stdout_limit_exceeded");
    let stderr_reason = (result.stop_reason == Some(RuncStopReason::StderrLimitExceeded))
        .then_some("stderr_limit_exceeded");
    state.stdout = runc_stream_slot(&result.stdout, controls.stdout_limit_bytes, stdout_reason);
    state.stderr = runc_stream_slot(&result.stderr, controls.stderr_limit_bytes, stderr_reason);
    state.stdout_bytes = Some(result.stdout.bytes.clone());
    state.stderr_bytes = Some(result.stderr.bytes.clone());
    for error in &result.operation_errors {
        let phase = match error.kind {
            RuncOperationErrorKind::StateObservation => "process_observation",
            RuncOperationErrorKind::OomObservation => "oom_observation",
            RuncOperationErrorKind::StdoutCapture => "stdout_capture",
            RuncOperationErrorKind::StderrCapture => "stderr_capture",
            RuncOperationErrorKind::Cleanup => "runtime_cleanup",
        };
        state.operation_errors.push(OperationError {
            scope: state.scope,
            phase: phase.to_owned(),
            message: error.message.clone(),
        });
    }
}

#[cfg(target_os = "linux")]
fn runc_stream_slot(
    capture: &crate::native::backend::RuncStreamCapture,
    limit: u64,
    stop_reason: Option<&str>,
) -> StoredBytes {
    let digest = digest_bytes(&capture.bytes);
    let size = capture.bytes.len() as u64;
    if capture.partial || capture.observed_bytes > limit || stop_reason.is_some() {
        StoredBytes::Partial {
            digest,
            size,
            limit_bytes: limit,
            reason: stop_reason
                .unwrap_or("stream_capture_incomplete")
                .to_owned(),
        }
    } else {
        StoredBytes::Available { digest, size }
    }
}

#[cfg(target_os = "linux")]
struct NativeEnvironment {
    overlay: Option<OverlayRootfs>,
    bundle: Option<OciBundle>,
    materialized: Option<MaterializedRootfs>,
    file_destinations: Option<DestinationFileGuard>,
    resolver: Option<ResolverProjection>,
}

#[cfg(target_os = "linux")]
impl NativeEnvironment {
    fn new_overlay(
        materialized: MaterializedRootfs,
        bundle: OciBundle,
        overlay: OverlayRootfs,
    ) -> Self {
        Self {
            overlay: Some(overlay),
            bundle: Some(bundle),
            materialized: Some(materialized),
            file_destinations: None,
            resolver: None,
        }
    }

    fn new_writable(materialized: MaterializedRootfs, bundle: OciBundle) -> Self {
        Self {
            overlay: None,
            bundle: Some(bundle),
            materialized: Some(materialized),
            file_destinations: None,
            resolver: None,
        }
    }

    fn prepare_file_destinations(&mut self, files: &[VerifiedSourceFile]) -> Result<()> {
        self.file_destinations = Some(DestinationFileGuard::prepare(self.rootfs()?, files)?);
        Ok(())
    }

    fn verify_file_destinations_unchanged(&mut self) -> Result<()> {
        let destinations = self
            .file_destinations
            .take()
            .expect("native file destinations are prepared");
        let result = destinations.verify_unchanged();
        drop(destinations);
        result
    }

    fn bundle(&self) -> &OciBundle {
        self.bundle.as_ref().expect("native bundle is owned")
    }

    fn rootfs(&self) -> Result<&Path> {
        self.bundle().rootfs()
    }

    fn verify_runtime_mounts_removed(&self) -> Result<()> {
        match self.overlay.as_ref() {
            Some(overlay) => overlay.verify_runtime_mounts_removed(),
            None => crate::native::fs::ensure_no_mounts_at_or_below(self.rootfs()?),
        }
    }

    fn cleanup_resolver(
        &mut self,
        attempt: &mut NativeAttempt,
        participant: NativeParticipant,
    ) -> Result<()> {
        if attempt.journal().resolver().is_none() {
            if self.resolver.is_some() {
                bail!("native environment owns a resolver projection without durable facts");
            }
            return Ok(());
        }
        attempt.begin_resolver_cleanup(participant)?;
        let Some(mut resolver) = self.resolver.take() else {
            return Ok(());
        };
        if let Err(error) = resolver.unmount() {
            self.resolver = Some(resolver);
            return Err(error);
        }
        if let Err(error) = attempt.record_resolver_cleanup(participant) {
            self.resolver = Some(resolver);
            return Err(error);
        }
        Ok(())
    }

    fn cleanup_all_or_preserve(
        &mut self,
        attempt: &mut NativeAttempt,
        participant: NativeParticipant,
        state: &mut ParticipantState,
        context: &str,
    ) {
        if let Err(error) = self.cleanup_resolver(attempt, participant) {
            state.error("resolver_cleanup", &error);
            self.preserve_in_place();
            state.operation_errors.push(OperationError {
                scope: state.scope,
                phase: "native_recovery".to_owned(),
                message: format!("{context}; native resources require explicit reconciliation"),
            });
            return;
        }
        self.cleanup_or_preserve(state, context);
    }

    fn cleanup_or_preserve(&mut self, state: &mut ParticipantState, context: &str) {
        let Some(overlay) = self.overlay.as_mut() else {
            return;
        };
        let result = overlay.unmount();
        if let Err(error) = result {
            state.error("overlay_cleanup", &error);
            self.preserve_in_place();
            state.operation_errors.push(OperationError {
                scope: state.scope,
                phase: "native_recovery".to_owned(),
                message: format!("{context}; native resources require explicit reconciliation"),
            });
        }
    }

    fn preserve(mut self, state: &mut ParticipantState, context: &str) {
        self.preserve_borrowed(state, context);
    }

    fn preserve_borrowed(&mut self, state: &mut ParticipantState, context: &str) {
        self.preserve_in_place();
        state.operation_errors.push(OperationError {
            scope: state.scope,
            phase: "native_recovery".to_owned(),
            message: format!("{context}; native resources require explicit reconciliation"),
        });
    }

    fn preserve_in_place(&mut self) {
        if let Some(resolver) = self.resolver.take() {
            resolver.preserve();
        }
        if let Some(overlay) = self.overlay.take() {
            let _ = overlay.preserve();
        }
        if let Some(bundle) = self.bundle.take() {
            let _ = bundle.preserve();
        }
        if let Some(materialized) = self.materialized.take() {
            let _ = materialized.preserve();
        }
    }
}

#[cfg(target_os = "linux")]
impl Drop for NativeEnvironment {
    fn drop(&mut self) {
        if self.resolver.is_some() {
            self.preserve_in_place();
            return;
        }
        let Some(overlay) = self.overlay.as_mut() else {
            return;
        };
        if overlay.unmount().is_err() {
            self.preserve_in_place();
        }
    }
}

#[cfg(target_os = "linux")]
fn adopt_writable_rootfs(bundle: &OciBundle, materialized: MaterializedRootfs) -> Result<()> {
    let source = materialized.preserve();
    let destination = bundle.rootfs()?.to_path_buf();
    fs::remove_dir(&destination).context("failed to remove empty OCI bundle rootfs")?;
    fs::rename(&source, &destination)
        .context("failed to move writable materialized rootfs into OCI bundle")?;
    Ok(())
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

    #[cfg(target_os = "linux")]
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

    #[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
fn managed_run_cli_exit_code(
    process: &ProcessSlot,
    primary_errors: &[OperationError],
    service_errors: &[OperationError],
) -> u8 {
    run_cli_exit_code_with_errors(
        process,
        !primary_errors.is_empty() || !service_errors.is_empty(),
    )
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

    #[cfg(target_os = "linux")]
    #[test]
    fn managed_service_loss_requires_a_confirmed_lifecycle_stop() {
        assert!(managed_service_loss_confirmed(
            true,
            Some(RuncStopReason::LifecycleStop)
        ));
        assert!(!managed_service_loss_confirmed(
            false,
            Some(RuncStopReason::LifecycleStop)
        ));
        assert!(!managed_service_loss_confirmed(true, None));
        assert!(!managed_service_loss_confirmed(
            true,
            Some(RuncStopReason::Cancelled)
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn completed_primary_process_is_not_stopped_during_cleanup() {
        let service_terminal = AtomicBool::new(true);
        let primary_terminal = AtomicBool::new(true);
        let primary_complete = AtomicBool::new(false);
        let lifecycle_stop = AtomicBool::new(false);

        assert!(!request_primary_stop_after_service_exit(
            &service_terminal,
            &primary_terminal,
            &primary_complete,
            &lifecycle_stop,
        ));
        assert!(!lifecycle_stop.load(Ordering::Acquire));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn service_exit_requests_stop_for_an_observed_running_primary() {
        let service_terminal = AtomicBool::new(true);
        let primary_terminal = AtomicBool::new(false);
        let primary_complete = AtomicBool::new(false);
        let lifecycle_stop = AtomicBool::new(false);

        assert!(request_primary_stop_after_service_exit(
            &service_terminal,
            &primary_terminal,
            &primary_complete,
            &lifecycle_stop,
        ));
        assert!(lifecycle_stop.load(Ordering::Acquire));
    }

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

    #[cfg(target_os = "linux")]
    #[test]
    fn managed_cli_exit_status_includes_service_operation_errors() {
        let exited = ProcessSlot::available(ProcessFacts {
            terminal_outcome: ProcessOutcome::ProcessExited,
            exit_code: Some(0),
            started_at: None,
            ended_at: None,
            oom_killed: None,
            backend_error: None,
        });
        let service_errors = vec![OperationError {
            scope: OperationErrorScope::ManagedService,
            phase: "final_image_capture".to_owned(),
            message: "failed".to_owned(),
        }];
        assert_eq!(managed_run_cli_exit_code(&exited, &[], &service_errors), 1);

        let cancelled = ProcessSlot::available(ProcessFacts {
            terminal_outcome: ProcessOutcome::Cancelled,
            exit_code: None,
            started_at: None,
            ended_at: None,
            oom_killed: None,
            backend_error: None,
        });
        assert_eq!(
            managed_run_cli_exit_code(&cancelled, &[], &service_errors),
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
