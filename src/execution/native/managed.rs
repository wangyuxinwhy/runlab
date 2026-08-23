//! The two-participant topology: a Primary Run bound to one required Managed
//! Service.
//!
//! This module owns everything that only exists because a Run has two
//! participants: readiness, concurrent observation of both processes, the
//! lifecycle stop one triggers in the other, and the terminal transaction that
//! relates both sets of facts. Single-participant behaviour is reached through
//! `participant`, and the recovery attempt and Run network through `scope`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chrono::Utc;

use crate::core::{
    ACCEPTED_RUN_RECORD_SCHEMA_VERSION, AcceptedLifecycle, AcceptedRunRecord, Digest, ImageView,
    ManagedServiceCondition, ManagedServiceFacts, ManagedServiceReadiness, NetworkControl,
    OciDescriptor, OperationError, OperationErrorScope, ProcessSlot, RunControls, RunId,
    ServiceName, StoredBytes, TERMINAL_RUN_RECORD_SCHEMA_VERSION, TcpReadinessCondition,
    TerminalLifecycle, TerminalRunRecord,
};
use crate::filesystem::Inventory;
use crate::native::backend::{
    NativeBackend, NativeExecutionMode, NativePreflight, PreparedRuncRun, RuncCaptureLimits,
    RuncRunner, RuncStopReason, verify_resolver_target,
};
use crate::native::network::{NativeNetworkBinding, RunNetwork};
use crate::native::read_only_file::VerifiedSourceFile;
use crate::native::recovery::{
    ManagedTerminalCheckpoint, NativeAttempt, NativeParticipant, NativeRecoveryStore,
    TerminalCheckpoint,
};
use crate::native::resolver::{ResolverConfig, ResolverSourceFile};
use crate::runtime::RuntimeConfig;
use crate::signal::TerminationFlag;

use super::participant::{
    NativeEnvironment, NativeExecution, NativeProcessObservation, observe_native_process,
    prepare_native_process_start, run_native_process_observation, verify_platform,
};
use super::scope::{
    abandon_managed_pre_acceptance, finish_run_network, mark_managed_accepted,
    prepare_resolver_source, run_network_cleanup_error, start_run_network,
};
use crate::execution::{
    ParticipantState, RunCleanup, RunResult, RunState, Runner, RunnerBackend, TerminalInput,
    available_bytes, run_cli_exit_code_with_errors,
};

impl Runner<'_> {
    pub fn run_with_managed_service(
        &self,
        input: ManagedPrimaryInput<'_>,
        service: &ManagedServiceInput<'_>,
    ) -> Result<RunResult> {
        let RunnerBackend::Native(backend) = self.backend else {
            bail!("Managed Service execution requires the native backend");
        };
        let prepared = self.prepare_managed_run(
            backend,
            input.initial_manifest,
            input.runtime,
            &input.controls,
            service,
        )?;
        let cancellation = TerminationFlag::register()?;
        let run_id = RunId::new();
        let recovery = NativeRecoveryStore::open(&prepared.state_root)?;
        let mut attempt = recovery.prepare_managed(run_id, prepared.preflight.facts.clone())?;
        let resolver_source =
            match prepare_resolver_source(&mut attempt, prepared.preflight.resolver.as_ref()) {
                Ok(source) => source,
                Err(error) => return abandon_managed_pre_acceptance(attempt, error),
            };
        let accepted = match self.accept_managed_run(run_id, &prepared, &input, service) {
            Ok(accepted) => accepted,
            Err(error) => return abandon_managed_pre_acceptance(attempt, error),
        };

        let mut state = RunState::new(attempt.journal().backend().clone());
        let mut managed = ManagedRunState::new(accepted.condition.clone());
        let mut network = None;
        if mark_managed_accepted(&mut attempt, &mut state.primary, &mut managed.participant) {
            match start_run_network(
                &recovery,
                &mut attempt,
                input.controls.network,
                prepared
                    .preflight
                    .native_network_tools
                    .clone()
                    .expect("Managed Service network tools were preflighted"),
                prepared.preflight.egress_network_tools.clone(),
                prepared
                    .preflight
                    .resolver
                    .as_ref()
                    .map(ResolverConfig::facts),
            ) {
                Ok((created, binding)) => {
                    state.backend = attempt.journal().backend().clone();
                    self.execute_managed_native(
                        &prepared.preflight.runner,
                        &binding,
                        &mut attempt,
                        &ManagedNativeInput {
                            run_id,
                            controls: &input.controls,
                            stdin: input.stdin,
                            primary_manifest: input.initial_manifest,
                            primary_runtime: input.runtime,
                            service_manifest: service.initial_manifest,
                            service_runtime: service.runtime,
                            capture_limits: prepared.preflight.capture_limits,
                            cancelled: cancellation.flag(),
                            service_timeout_seconds: prepared.service_timeout_seconds,
                            primary_files: &prepared.preflight.primary_files,
                            service_files: &prepared.preflight.managed_service_files,
                            resolver_source: resolver_source.as_ref(),
                        },
                        &mut state,
                        &mut managed,
                    );
                    network = Some(created);
                }
                Err(error) => {
                    state.primary.fail_before_start("network_setup", &error);
                    managed
                        .participant
                        .fail_before_start("network_setup", &error);
                    managed.readiness = Some(readiness_probe_error(
                        0,
                        "Run network setup failed before readiness probing",
                    ));
                }
            }
        } else {
            managed.readiness = Some(readiness_probe_error(
                0,
                "native accepted-state checkpoint failed before readiness probing",
            ));
        }
        self.terminalize_managed(
            TerminalInput {
                run_id,
                accepted_at: accepted.accepted_at,
                requested_image_reference: input.requested_image_reference.map(ToOwned::to_owned),
                initial_image: accepted.primary_image,
                runtime_config: accepted.primary_runtime,
                controls: input.controls,
            },
            state,
            managed,
            attempt,
            network,
        )
    }

    fn prepare_managed_run(
        &self,
        backend: &NativeBackend,
        initial_manifest: &Digest,
        runtime: &RuntimeConfig,
        controls: &RunControls,
        service: &ManagedServiceInput<'_>,
    ) -> Result<ManagedPreparation> {
        let primary_image = self.images.inspect(initial_manifest)?;
        let service_image = self.images.inspect(service.initial_manifest)?;
        let service_timeout_seconds = service
            .readiness
            .timeout_seconds
            .checked_add(controls.timeout_seconds)
            .and_then(|value| value.checked_add(5))
            .context("Managed Service lifecycle timeout overflow")?;
        let state_root = self
            .database
            .path()
            .parent()
            .context("Run database path has no state root")?
            .to_path_buf();
        if controls.network == NetworkControl::Egress {
            runtime.validate_native_resolver_destination()?;
            service.runtime.validate_native_resolver_destination()?;
            verify_resolver_target(self.images, &primary_image)?;
            verify_resolver_target(self.images, &service_image)?;
        }
        let preflight =
            backend.preflight_managed(runtime, service.runtime, controls, &state_root)?;
        verify_platform("Primary", &primary_image, &preflight.facts)?;
        verify_platform("Managed Service", &service_image, &preflight.facts)?;
        Ok(ManagedPreparation {
            primary_image,
            service_image,
            service_timeout_seconds,
            state_root,
            preflight,
        })
    }

    fn accept_managed_run(
        &self,
        run_id: RunId,
        prepared: &ManagedPreparation,
        input: &ManagedPrimaryInput<'_>,
        service: &ManagedServiceInput<'_>,
    ) -> Result<ManagedAcceptance> {
        let accepted_at = Utc::now();
        let primary_runtime = available_bytes(input.runtime_bytes)?;
        let condition = ManagedServiceCondition {
            name: service.name.clone(),
            requested_image_reference: service.requested_image_reference.map(ToOwned::to_owned),
            initial_image: prepared.service_image.manifest.clone(),
            runtime_config: available_bytes(service.runtime_bytes)?,
            readiness: service.readiness.clone(),
        };
        condition.validate()?;
        let record = AcceptedRunRecord {
            schema_version: ACCEPTED_RUN_RECORD_SCHEMA_VERSION,
            run_id,
            lifecycle: AcceptedLifecycle::Accepted,
            accepted_at,
            requested_image_reference: input.requested_image_reference.map(ToOwned::to_owned),
            initial_image: prepared.primary_image.manifest.clone(),
            runtime_config: primary_runtime.clone(),
            controls: input.controls.clone(),
            managed_service: Some(condition.clone()),
        };
        self.database.accept_with_managed_service(
            &record,
            input.runtime_bytes,
            input.stdin,
            Some(service.runtime_bytes),
        )?;
        Ok(ManagedAcceptance {
            accepted_at,
            primary_image: prepared.primary_image.manifest.clone(),
            primary_runtime,
            condition,
        })
    }

    fn execute_managed_native(
        &self,
        runc: &RuncRunner,
        network: &NativeNetworkBinding,
        attempt: &mut NativeAttempt,
        input: &ManagedNativeInput<'_>,
        state: &mut RunState,
        managed: &mut ManagedRunState,
    ) {
        let primary_stop = AtomicBool::new(false);
        let service_stop = AtomicBool::new(false);
        let service_controls = RunControls {
            timeout_seconds: input.service_timeout_seconds,
            ..input.controls.clone()
        };
        let primary_execution = NativeExecution {
            runner: runc,
            mode: NativeExecutionMode::Rootful,
            initial_manifest: input.primary_manifest,
            bundle_runtime: input.primary_runtime,
            controls: input.controls,
            stdin: input.stdin,
            run_id: input.run_id,
            capture_limits: input.capture_limits,
            cancelled: input.cancelled,
            lifecycle_stop: &primary_stop,
            participant: NativeParticipant::Primary,
            network: Some(network),
            read_only_files: input.primary_files,
            resolver_source: input.resolver_source,
        };
        let service_execution = NativeExecution {
            runner: runc,
            mode: NativeExecutionMode::Rootful,
            initial_manifest: input.service_manifest,
            bundle_runtime: input.service_runtime,
            controls: &service_controls,
            stdin: &[],
            run_id: input.run_id,
            capture_limits: input.capture_limits,
            cancelled: input.cancelled,
            lifecycle_stop: &service_stop,
            participant: NativeParticipant::ManagedService,
            network: Some(network),
            read_only_files: input.service_files,
            resolver_source: input.resolver_source,
        };
        let executions = ManagedExecutions {
            primary: &primary_execution,
            service: &service_execution,
        };
        let Some(mut prepared) =
            self.prepare_managed_environments(runc, executions, attempt, state, managed)
        else {
            return;
        };
        let observations = run_managed_observations(
            runc,
            network,
            input,
            executions,
            &mut prepared,
            ManagedExecutionStates {
                attempt,
                primary: &mut state.primary,
                managed,
            },
        );
        self.complete_managed_executions(
            executions,
            attempt,
            state,
            managed,
            prepared,
            observations,
        );
    }

    fn prepare_managed_environments<'runc>(
        &self,
        runc: &'runc RuncRunner,
        executions: ManagedExecutions<'_, '_>,
        attempt: &mut NativeAttempt,
        state: &mut RunState,
        managed: &mut ManagedRunState,
    ) -> Option<PreparedManagedEnvironments<'runc>> {
        let Some((mut service, service_before)) =
            self.prepare_native_environment(executions.service, attempt, &mut managed.participant)
        else {
            managed.readiness = Some(readiness_probe_error(
                0,
                "Managed Service environment preparation failed",
            ));
            state
                .primary
                .not_started("Managed Service environment preparation failed");
            return None;
        };
        let Some((mut primary, primary_before)) =
            self.prepare_native_environment(executions.primary, attempt, &mut state.primary)
        else {
            managed.readiness = Some(readiness_probe_error(
                0,
                "Primary environment preparation failed before service start",
            ));
            service.cleanup_all_or_preserve(
                attempt,
                NativeParticipant::ManagedService,
                &mut managed.participant,
                "Primary environment preparation failed",
            );
            return None;
        };
        let Some(service_runtime) = prepare_native_process_start(
            runc,
            NativeExecutionMode::Rootful,
            attempt,
            NativeParticipant::ManagedService,
            &mut managed.participant,
        ) else {
            managed.readiness = Some(readiness_probe_error(
                0,
                "Managed Service runtime checkpoint failed",
            ));
            service.cleanup_all_or_preserve(
                attempt,
                NativeParticipant::ManagedService,
                &mut managed.participant,
                "Managed Service runtime checkpoint failed",
            );
            primary.cleanup_all_or_preserve(
                attempt,
                NativeParticipant::Primary,
                &mut state.primary,
                "Managed Service runtime checkpoint failed",
            );
            state
                .primary
                .not_started("Managed Service runtime checkpoint failed");
            return None;
        };
        Some(PreparedManagedEnvironments {
            primary,
            primary_before,
            service,
            service_before,
            service_runtime: Some(service_runtime),
        })
    }

    fn complete_managed_executions(
        &self,
        executions: ManagedExecutions<'_, '_>,
        attempt: &mut NativeAttempt,
        state: &mut RunState,
        managed: &mut ManagedRunState,
        mut prepared: PreparedManagedEnvironments<'_>,
        observations: ManagedObservations,
    ) {
        match observations.service.and_then(|observation| {
            observe_native_process(
                observation,
                executions.service.cancelled,
                &mut managed.participant,
            )
        }) {
            Some((result, started_at, ended_at)) => self.complete_native_participant(
                executions.service,
                attempt,
                &mut managed.participant,
                prepared.service,
                &prepared.service_before,
                &result,
                started_at,
                ended_at,
            ),
            None => prepared.service.preserve(
                &mut managed.participant,
                "Managed Service process facts are incomplete",
            ),
        }
        match observations.primary.and_then(|observation| {
            observe_native_process(
                observation,
                executions.primary.cancelled,
                &mut state.primary,
            )
        }) {
            Some((result, started_at, ended_at)) => self.complete_native_participant(
                executions.primary,
                attempt,
                &mut state.primary,
                prepared.primary,
                &prepared.primary_before,
                &result,
                started_at,
                ended_at,
            ),
            None => prepared.primary.cleanup_all_or_preserve(
                attempt,
                NativeParticipant::Primary,
                &mut state.primary,
                "Primary process was not executed",
            ),
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "managed terminalization is one ordered capture, cleanup, and database transaction"
    )]
    fn terminalize_managed(
        &self,
        input: TerminalInput,
        state: RunState,
        managed: ManagedRunState,
        mut attempt: NativeAttempt,
        network: Option<RunNetwork>,
    ) -> Result<RunResult> {
        if state
            .primary
            .operation_errors
            .iter()
            .chain(&managed.participant.operation_errors)
            .any(|error| error.phase == "native_recovery")
        {
            bail!("native resources require explicit reconciliation before terminalization");
        }
        let readiness = managed.readiness.clone().unwrap_or_else(|| {
            readiness_probe_error(0, "Managed Service readiness was not observed")
        });
        let mut state = state;
        let mut network_cleanup_error = None;
        if let Err(error) = finish_run_network(&mut attempt, network) {
            let operation_error = run_network_cleanup_error(&error);
            network_cleanup_error = Some(operation_error.message.clone());
            state.primary.operation_errors.push(operation_error);
        }
        attempt.prepare_managed_terminal(ManagedTerminalCheckpoint {
            readiness: readiness.clone(),
            process: managed.participant.process.clone(),
            stdout: managed.participant.stdout.clone(),
            stderr: managed.participant.stderr.clone(),
            stdout_bytes: managed.participant.stdout_bytes.as_deref(),
            stderr_bytes: managed.participant.stderr_bytes.as_deref(),
            final_image: managed.participant.final_image.clone(),
            operation_errors: managed.participant.operation_errors.clone(),
        })?;
        let terminal_at = Utc::now();
        attempt.prepare_terminal(TerminalCheckpoint {
            terminal_at,
            process: state.primary.process.clone(),
            stdout: state.primary.stdout.clone(),
            stderr: state.primary.stderr.clone(),
            stdout_bytes: state.primary.stdout_bytes.as_deref(),
            stderr_bytes: state.primary.stderr_bytes.as_deref(),
            final_image: state.primary.final_image.clone(),
            operation_errors: state.primary.operation_errors.clone(),
        })?;

        let managed_facts = ManagedServiceFacts {
            name: managed.condition.name,
            requested_image_reference: managed.condition.requested_image_reference,
            initial_image: managed.condition.initial_image,
            runtime_config: managed.condition.runtime_config,
            readiness_condition: managed.condition.readiness,
            readiness,
            process: managed.participant.process,
            stdout: managed.participant.stdout,
            stderr: managed.participant.stderr,
            final_image: managed.participant.final_image,
            operation_errors: managed.participant.operation_errors,
        };
        managed_facts.validate()?;
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
            managed_service: Some(managed_facts),
        };
        self.database.terminal_with_managed_service(
            &record,
            state.primary.stdout_bytes.as_deref(),
            state.primary.stderr_bytes.as_deref(),
            managed.participant.stdout_bytes.as_deref(),
            managed.participant.stderr_bytes.as_deref(),
        )?;
        let cleanup = if let Some(error) = network_cleanup_error {
            RunCleanup::pending(error)
        } else {
            match attempt.remove_after_terminal() {
                Ok(()) => RunCleanup::complete(),
                Err(error) => RunCleanup::pending(format!(
                    "terminal Run recovery cleanup is pending: {error:#}"
                )),
            }
        };
        cleanup.validate()?;
        let cli_exit_code = managed_run_cli_exit_code(
            &record.process,
            &record.operation_errors,
            &record
                .managed_service
                .as_ref()
                .expect("Managed Service facts were constructed")
                .operation_errors,
        )
        .max(u8::from(!cleanup.resources_absent));
        Ok(RunResult {
            record,
            database: self.database.path().to_path_buf(),
            cli_exit_code,
            cleanup,
        })
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{ProcessFacts, ProcessOutcome};

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
