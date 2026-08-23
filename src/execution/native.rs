//! Native Linux execution of a Run.
//!
//! `execution` owns the platform-neutral shape of a Run: acceptance, the
//! terminal Run Record, and the exit-status contract. Everything that is
//! specific to running an OCI bundle through runc on this host lives here,
//! behind a single `#[cfg(target_os = "linux")]` on the `mod native;`
//! declaration. Nothing inside needs a per-item platform gate.
//!
//! The Linux-only halves of `Runner` are inherent methods declared in this
//! module rather than in the parent, so the parent file describes only the
//! path that exists on every host.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};

use crate::bundle::OciBundle;
use crate::core::{
    BackendFacts, Digest, ImageView, ManagedServiceCondition, ManagedServiceReadiness,
    NetworkControl, OciDescriptor, OperationError, OperationErrorScope, ProcessFacts,
    ProcessOutcome, ProcessSlot, RunControls, RunId, ServiceName, StoredBytes,
    TcpReadinessCondition,
};
use crate::filesystem::{Inventory, TreeCapture};
use crate::image::ImageService;
use crate::integrity::digest_bytes;
use crate::materialize::MaterializedRootfs;
use crate::native::backend::{
    NativeBackend, NativeExecutionMode, NativePreflight, PreparedRuncRun, RuncCaptureLimits,
    RuncExecution, RuncOperationErrorKind, RuncRunFailure, RuncRunResult, RuncRunner,
    RuncStopReason, verify_resolver_target, verify_rootless_image,
};
use crate::native::fs::OverlayRootfs;
use crate::native::network::{
    EgressNetworkTools, NativeNetworkBinding, NativeNetworkTools, NetworkHolderHandle, RunNetwork,
    RunNetworkMode,
};
use crate::native::read_only_file::{DestinationFileGuard, VerifiedSourceFile};
use crate::native::recovery::{
    NativeAttempt, NativeParticipant, NativeRecoveryPhase, NativeRecoveryStore,
    SharedNetworkCheckpoint, TerminalCheckpoint,
};
use crate::native::resolver::{
    ResolverConfig, ResolverProjection, ResolverProjectionPlan, ResolverSourceFile,
};
use crate::runtime::RuntimeConfig;
use crate::signal::TerminationFlag;
use crate::storage::RunDatabase;

use super::{
    ParticipantState, PreparedBackend, PreparedExecution, RunCleanup, RunResult, RunState, Runner,
    RunnerBackend, TerminalInput, available_bytes, record_final_capture,
    run_cli_exit_code_with_errors,
};

mod managed;

pub struct ManagedServiceInput<'a> {
    pub name: ServiceName,
    pub requested_image_reference: Option<&'a str>,
    pub initial_manifest: &'a Digest,
    pub runtime: &'a RuntimeConfig,
    pub runtime_bytes: &'a [u8],
    pub readiness: TcpReadinessCondition,
}
pub struct ManagedPrimaryInput<'a> {
    pub initial_manifest: &'a Digest,
    pub requested_image_reference: Option<&'a str>,
    pub runtime: &'a RuntimeConfig,
    pub runtime_bytes: &'a [u8],
    pub controls: RunControls,
    pub stdin: &'a [u8],
}
pub(super) struct PreparedNativeBackend {
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
pub(super) struct NativeExecution<'a> {
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
struct NativeProcessObservation {
    started_at: chrono::DateTime<Utc>,
    ended_at: chrono::DateTime<Utc>,
    result: std::result::Result<RuncRunResult, RuncRunFailure>,
}
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
struct ManagedPreparation {
    primary_image: ImageView,
    service_image: ImageView,
    service_timeout_seconds: u64,
    state_root: PathBuf,
    preflight: NativePreflight,
}
struct ManagedAcceptance {
    accepted_at: chrono::DateTime<Utc>,
    primary_image: OciDescriptor,
    primary_runtime: StoredBytes,
    condition: ManagedServiceCondition,
}
#[derive(Clone, Copy)]
struct ManagedExecutions<'a, 'execution> {
    primary: &'a NativeExecution<'execution>,
    service: &'a NativeExecution<'execution>,
}
struct PreparedManagedEnvironments<'runc> {
    primary: NativeEnvironment,
    primary_before: Inventory,
    service: NativeEnvironment,
    service_before: Inventory,
    service_runtime: Option<PreparedRuncRun<'runc>>,
}
struct ManagedObservations {
    primary: Option<NativeProcessObservation>,
    service: Option<NativeProcessObservation>,
}
struct ManagedExecutionStates<'a> {
    attempt: &'a mut NativeAttempt,
    primary: &'a mut ParticipantState,
    managed: &'a mut ManagedRunState,
}
struct ManagedRunState {
    condition: ManagedServiceCondition,
    readiness: Option<ManagedServiceReadiness>,
    participant: ParticipantState,
}
impl ManagedRunState {
    fn new(condition: ManagedServiceCondition) -> Self {
        Self {
            condition,
            readiness: None,
            participant: ParticipantState::new(OperationErrorScope::ManagedService),
        }
    }
}
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
fn managed_service_loss_confirmed(
    service_requested_primary_stop: bool,
    primary_stop_reason: Option<RuncStopReason>,
) -> bool {
    service_requested_primary_stop && primary_stop_reason == Some(RuncStopReason::LifecycleStop)
}
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
            crate::subprocess::TCP_PROBE_COMMAND.to_owned(),
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
fn readiness_probe_error(attempts: u32, error: &str) -> ManagedServiceReadiness {
    ManagedServiceReadiness::ProbeError {
        observed_at: Utc::now(),
        attempts,
        error: error.to_owned(),
    }
}
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
pub(super) fn finish_run_network(
    attempt: &mut NativeAttempt,
    network: Option<RunNetwork>,
) -> Result<()> {
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
pub(super) fn run_network_cleanup_error(error: &anyhow::Error) -> OperationError {
    OperationError {
        scope: OperationErrorScope::Run,
        phase: "resource_cleanup".to_owned(),
        message: format!("run_network_cleanup: {error:#}"),
    }
}
fn prepare_resolver_source(
    attempt: &mut NativeAttempt,
    resolver: Option<&ResolverConfig>,
) -> Result<Option<ResolverSourceFile>> {
    resolver
        .map(|resolver| attempt.prepare_resolver(resolver))
        .transpose()
}
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
fn native_cancelled(cancelled: &AtomicBool, state: &mut ParticipantState) -> bool {
    if cancelled.load(std::sync::atomic::Ordering::Acquire) {
        state.cancel_before_start();
        true
    } else {
        false
    }
}
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
struct NativeEnvironment {
    overlay: Option<OverlayRootfs>,
    bundle: Option<OciBundle>,
    materialized: Option<MaterializedRootfs>,
    file_destinations: Option<DestinationFileGuard>,
    resolver: Option<ResolverProjection>,
}
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
fn adopt_writable_rootfs(bundle: &OciBundle, materialized: MaterializedRootfs) -> Result<()> {
    let source = materialized.preserve();
    let destination = bundle.rootfs()?.to_path_buf();
    fs::remove_dir(&destination).context("failed to remove empty OCI bundle rootfs")?;
    fs::rename(&source, &destination)
        .context("failed to move writable materialized rootfs into OCI bundle")?;
    Ok(())
}
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

/// The host resources the native backend holds for the lifetime of one Run: a
/// cancellation registration, a durable recovery attempt, and the Run network
/// namespace with its binding.
///
/// Collecting them lets `execution` orchestrate a Run without naming any of
/// them. `execution` keeps a non-Linux twin of this type whose operations do
/// nothing, so the orchestration reads the same on every host.
#[derive(Default)]
pub(super) struct RunScope {
    cancellation: Option<TerminationFlag>,
    attempt: Option<NativeAttempt>,
    network: Option<RunNetwork>,
    binding: Option<NativeNetworkBinding>,
}

impl RunScope {
    pub(super) fn open(
        runner: &Runner<'_>,
        run_id: RunId,
        backend: &BackendFacts,
        prepared: &mut PreparedBackend,
    ) -> Result<Self> {
        let cancellation = match prepared {
            PreparedBackend::Native(_) => Some(TerminationFlag::register()?),
            PreparedBackend::Docker(_) => None,
        };
        let attempt = runner.prepare_native_attempt(run_id, backend, prepared)?;
        Ok(Self {
            cancellation,
            attempt,
            network: None,
            binding: None,
        })
    }

    /// Discard a recovery attempt whose Run was never accepted.
    ///
    /// The attempt is published before acceptance so a crash between the two is
    /// observable, which means an ordinary error return would leave a durable
    /// entry behind and block `state gc` until someone reconciles it. Removing
    /// it here is the eager half of that contract; reconciliation stays the
    /// backstop for the failures no error path can reach, such as SIGKILL.
    ///
    /// The reason the caller already has is what gets reported. A cleanup
    /// failure is appended to it rather than replacing it, because the entry
    /// that survives is the reconcilable one.
    pub(super) fn abort_pre_acceptance(self, error: anyhow::Error) -> anyhow::Error {
        let Some(attempt) = self.attempt else {
            return error;
        };
        match attempt.remove() {
            Ok(()) => error,
            Err(cleanup) => anyhow::anyhow!(
                "{error:#}; pre-acceptance recovery cleanup also failed: {cleanup:#}"
            ),
        }
    }

    /// Where the Run working directory belongs. A native Run keeps it inside
    /// the recovery attempt so an interrupted Run can still find its files.
    pub(super) fn workspace(&self) -> Option<PathBuf> {
        self.attempt.as_ref().map(NativeAttempt::workspace)
    }

    /// Record acceptance in the recovery journal. Returns whether the Run may
    /// proceed; a journal failure is reported on the participant instead.
    pub(super) fn mark_accepted(&mut self, state: &mut RunState) -> bool {
        let Some(attempt) = self.attempt.as_mut() else {
            return true;
        };
        match attempt.advance_phase(NativeRecoveryPhase::Accepted) {
            Ok(()) => true,
            Err(error) => {
                state
                    .primary
                    .fail_before_start("recovery_checkpoint", &error);
                false
            }
        }
    }

    pub(super) fn start_network(
        &mut self,
        runner: &Runner<'_>,
        prepared: &PreparedBackend,
        network: NetworkControl,
        ready: bool,
        state: &mut RunState,
    ) -> Result<bool> {
        let (run_network, binding, execution_ready) = runner.prepare_single_run_network(
            prepared,
            network,
            ready,
            self.attempt.as_mut(),
            state,
        )?;
        self.network = run_network;
        self.binding = binding;
        Ok(execution_ready)
    }

    /// A Run that left native resources behind must not be terminalized
    /// silently; reconciliation has to observe and release them first.
    pub(super) fn requires_reconciliation(&self, state: &RunState) -> bool {
        self.attempt.is_some()
            && state
                .primary
                .operation_errors
                .iter()
                .any(|error| error.phase == "native_recovery")
    }

    /// Release the Run network and record the terminal checkpoint. Returns a
    /// cleanup message when the network could not be released.
    pub(super) fn checkpoint_terminal(
        &mut self,
        terminal_at: DateTime<Utc>,
        state: &mut RunState,
    ) -> Result<Option<String>> {
        let network = self.network.take();
        let Some(attempt) = self.attempt.as_mut() else {
            return Ok(None);
        };
        let mut network_cleanup_error = None;
        if attempt.journal().shared_network().is_some()
            && let Err(error) = finish_run_network(attempt, network)
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
        Ok(network_cleanup_error)
    }

    /// Remove the recovery attempt now that the Run Record is durable. The
    /// cancellation registration stays until the caller drops the scope.
    pub(super) fn close(&mut self, network_cleanup_error: Option<String>) -> RunCleanup {
        let Some(attempt) = self.attempt.take() else {
            return RunCleanup::complete();
        };
        if let Some(error) = network_cleanup_error {
            return RunCleanup::pending(error);
        }
        match attempt.remove_after_terminal() {
            Ok(()) => RunCleanup::complete(),
            Err(error) => RunCleanup::pending(format!(
                "terminal Run recovery cleanup is pending: {error:#}"
            )),
        }
    }
}

impl<'a> Runner<'a> {
    /// Preflight the native backend for one Run and carry the realization
    /// forward. The parent only chooses the backend; the shape of a prepared
    /// native Run stays inside this module.
    pub(super) fn prepare_native_backend(
        &self,
        backend: &NativeBackend,
        runtime: &RuntimeConfig,
        controls: &RunControls,
        initial: &ImageView,
    ) -> Result<(BackendFacts, PreparedBackend)> {
        let rootless = !rustix::process::geteuid().is_root();
        if !rootless && controls.network == NetworkControl::Egress {
            runtime.validate_native_resolver_destination()?;
            verify_resolver_target(self.images, initial)?;
        }
        let state_root = self
            .database
            .path()
            .parent()
            .context("Run database path has no state root")?;
        let preflight = backend.preflight(runtime, controls, state_root)?;
        if preflight.mode.is_rootless() {
            verify_rootless_image(self.images, initial, state_root, preflight.mode.ownership())?;
        }
        Ok((
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
        ))
    }

    /// Run the primary participant of a single-participant native Run.
    pub(super) fn execute_native_primary(
        &self,
        prepared: &PreparedNativeBackend,
        execution: &PreparedExecution<'_>,
        scope: &mut RunScope,
        state: &mut RunState,
    ) {
        let RunScope {
            cancellation,
            attempt,
            binding,
            ..
        } = scope;
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
                cancelled: cancellation
                    .as_ref()
                    .expect("native cancellation is registered")
                    .flag(),
                lifecycle_stop: &lifecycle_stop,
                participant: NativeParticipant::Primary,
                network: binding.as_ref(),
                read_only_files: &prepared.read_only_files,
                resolver_source: prepared.resolver_source.as_ref(),
            },
            attempt.as_mut().expect("native recovery attempt is owned"),
            state,
        );
    }

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

    pub(super) fn prepare_native_attempt(
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

    pub(super) fn prepare_single_run_network(
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

    pub(super) fn execute_native(
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
                let writable = match self.images.materialize_rootfs_at(
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
        let materialized = match self.images.materialize_rootfs_at(
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{Architecture, BackendDetails, Platform};

    fn pre_acceptance_backend() -> BackendFacts {
        BackendFacts {
            name: "native_linux".to_owned(),
            version: "0.2.0-dev.0".to_owned(),
            platform: Platform::linux(Architecture::Arm64),
            network: NetworkControl::None,
            run_network: None,
            details: BackendDetails::NativeLinux {
                runtime_name: "runc".to_owned(),
                runtime_version: "1.5.1".to_owned(),
                runtime_commit: "fixture".to_owned(),
                runtime_spec: "1.3.0".to_owned(),
                runtime_digest: crate::integrity::digest_bytes(b"runc fixture"),
                runtime_size: 12,
                kernel_release: "fixture".to_owned(),
                runtime_invocation: crate::core::NativeRuntimeInvocation::Direct,
                runtime_config: crate::core::NativeRuntimeConfigRealization::Accepted,
                filesystem: crate::core::NativeFilesystemRealization::OverlayFs {
                    profile: "index=on".to_owned(),
                },
            },
        }
    }

    #[test]
    fn a_run_that_is_never_accepted_leaves_no_recovery_attempt() {
        let state = tempfile::tempdir().expect("state");
        let store = NativeRecoveryStore::open(state.path()).expect("store");
        let run_id = RunId::new();
        let attempt = store
            .prepare(run_id, pre_acceptance_backend())
            .expect("attempt");
        assert_eq!(
            store.list_attempt_ids(None, 4).expect("published").ids,
            vec![run_id],
            "the attempt must be durable before acceptance"
        );

        let scope = RunScope {
            cancellation: None,
            attempt: Some(attempt),
            network: None,
            binding: None,
        };
        let reported = scope.abort_pre_acceptance(anyhow::anyhow!("acceptance failed"));

        assert_eq!(format!("{reported:#}"), "acceptance failed");
        assert!(
            store
                .list_attempt_ids(None, 4)
                .expect("after abort")
                .ids
                .is_empty(),
            "a Run that was never accepted must not keep blocking state GC"
        );
    }

    #[test]
    fn aborting_without_a_recovery_attempt_reports_the_original_error() {
        let scope = RunScope {
            cancellation: None,
            attempt: None,
            network: None,
            binding: None,
        };
        let reported = scope.abort_pre_acceptance(anyhow::anyhow!("acceptance failed"));
        assert_eq!(format!("{reported:#}"), "acceptance failed");
    }

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
}
