use super::{
    ACCEPTED_RUN_RECORD_SCHEMA_VERSION, AcceptedLifecycle, AcceptedRunRecord, AtomicBool, Context,
    Digest, ManagedAcceptance, ManagedExecutionStates, ManagedExecutions, ManagedNativeInput,
    ManagedObservations, ManagedPreparation, ManagedPrimaryInput, ManagedRunState,
    ManagedServiceCondition, ManagedServiceFacts, ManagedServiceInput, ManagedTerminalCheckpoint,
    NativeAttempt, NativeBackend, NativeExecution, NativeExecutionMode, NativeNetworkBinding,
    NativeParticipant, NativeRecoveryStore, NetworkControl, PreparedManagedEnvironments,
    ResolverConfig, Result, RunCleanup, RunControls, RunId, RunNetwork, RunResult, RunState,
    RuncRunner, Runner, RunnerBackend, RuntimeConfig, TERMINAL_RUN_RECORD_SCHEMA_VERSION,
    TerminalCheckpoint, TerminalInput, TerminalLifecycle, TerminalRunRecord, TerminationFlag, Utc,
    abandon_managed_pre_acceptance, available_bytes, bail, finish_run_network,
    managed_run_cli_exit_code, mark_managed_accepted, observe_native_process,
    prepare_native_process_start, prepare_resolver_source, readiness_probe_error,
    run_managed_observations, run_network_cleanup_error, start_run_network, verify_platform,
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
            self.images.verify_native_resolver_target(&primary_image)?;
            self.images.verify_native_resolver_target(&service_image)?;
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
