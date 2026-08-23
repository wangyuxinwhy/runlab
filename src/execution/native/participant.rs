//! Preparing, running, and capturing one participant of a native Run.
//!
//! Everything here is about a single OCI bundle: the rootfs it is given, the
//! runc process it becomes, the facts that process leaves behind, and the Final
//! Image captured from what it wrote. It knows nothing about how many
//! participants a Run has.
//!
//! `NativeEnvironment` is why this is one module rather than a set of free
//! functions: the mounts, projections, and holders a participant needs must be
//! released in the reverse of the order they were taken, so one type owns all
//! of them and releases them on drop.

use std::fs;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::Utc;

use crate::bundle::OciBundle;
use crate::core::{
    BackendFacts, Digest, ImageView, NetworkControl, OperationError, ProcessFacts, ProcessOutcome,
    ProcessSlot, RunControls, RunId, StoredBytes,
};
use crate::filesystem::{Inventory, TreeCapture};
use crate::integrity::digest_bytes;
use crate::materialize::MaterializedRootfs;
use crate::native::backend::{
    NativeBackend, NativeExecutionMode, PreparedRuncRun, RuncCaptureLimits, RuncExecution,
    RuncOperationErrorKind, RuncRunFailure, RuncRunResult, RuncRunner, RuncStopReason,
    verify_resolver_target, verify_rootless_image,
};
use crate::native::fs::OverlayRootfs;
use crate::native::network::{EgressNetworkTools, NativeNetworkBinding, NativeNetworkTools};
use crate::native::read_only_file::{DestinationFileGuard, VerifiedSourceFile};
use crate::native::recovery::{NativeAttempt, NativeParticipant, NativeRecoveryPhase};
use crate::native::resolver::{
    ResolverConfig, ResolverProjection, ResolverProjectionPlan, ResolverSourceFile,
};
use crate::runtime::RuntimeConfig;

use super::scope::RunScope;
use crate::execution::{
    ParticipantState, PreparedBackend, PreparedExecution, RunState, Runner, record_final_capture,
};

pub(in crate::execution) struct PreparedNativeBackend {
    runner: RuncRunner,
    mode: NativeExecutionMode,
    realized_runtime: Option<RuntimeConfig>,
    capture_limits: RuncCaptureLimits,
    read_only_files: Vec<VerifiedSourceFile>,
    pub(super) native_network_tools: Option<NativeNetworkTools>,
    pub(super) egress_network_tools: Option<EgressNetworkTools>,
    pub(super) resolver: Option<ResolverConfig>,
    pub(super) resolver_source: Option<ResolverSourceFile>,
}

/// Everything one participant needs to run, resolved before it starts.
///
/// Its fields are open to the rest of `native` because the Managed Service
/// topology builds one of these per participant; the alternative is a
/// constructor with the same arity and no added meaning.
pub(super) struct NativeExecution<'a> {
    pub(super) runner: &'a RuncRunner,
    pub(super) mode: NativeExecutionMode,
    pub(super) initial_manifest: &'a Digest,
    pub(super) bundle_runtime: &'a RuntimeConfig,
    pub(super) controls: &'a RunControls,
    pub(super) stdin: &'a [u8],
    pub(super) run_id: RunId,
    pub(super) capture_limits: RuncCaptureLimits,
    pub(super) cancelled: &'a AtomicBool,
    pub(super) lifecycle_stop: &'a AtomicBool,
    pub(super) participant: NativeParticipant,
    pub(super) network: Option<&'a NativeNetworkBinding>,
    pub(super) read_only_files: &'a [VerifiedSourceFile],
    pub(super) resolver_source: Option<&'a ResolverSourceFile>,
}

pub(super) struct NativeProcessObservation {
    started_at: chrono::DateTime<Utc>,
    ended_at: chrono::DateTime<Utc>,
    pub(super) result: std::result::Result<RuncRunResult, RuncRunFailure>,
}

pub(super) struct NativeEnvironment {
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

    pub(super) fn bundle(&self) -> &OciBundle {
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

    pub(super) fn cleanup_all_or_preserve(
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

    pub(super) fn preserve(mut self, state: &mut ParticipantState, context: &str) {
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

impl Runner<'_> {
    /// Preflight the native backend for one Run and carry the realization
    /// forward. The parent only chooses the backend; the shape of a prepared
    /// native Run stays inside this module.
    pub(in crate::execution) fn prepare_native_backend(
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
    pub(in crate::execution) fn execute_native_primary(
        &self,
        prepared: &PreparedNativeBackend,
        execution: &PreparedExecution<'_>,
        scope: &mut RunScope,
        state: &mut RunState,
    ) {
        let scoped = scope.scoped_execution();
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
                cancelled: scoped.cancelled,
                lifecycle_stop: &lifecycle_stop,
                participant: NativeParticipant::Primary,
                network: scoped.binding,
                read_only_files: &prepared.read_only_files,
                resolver_source: prepared.resolver_source.as_ref(),
            },
            scoped.attempt,
            state,
        );
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
    pub(super) fn complete_native_participant(
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

    pub(super) fn prepare_native_environment(
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

pub(super) fn run_native_process_observation(
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

pub(super) fn prepare_native_process_start<'runc>(
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

pub(super) fn verify_platform(
    participant: &str,
    image: &ImageView,
    backend: &BackendFacts,
) -> Result<()> {
    if image.platform != backend.platform {
        bail!(
            "{participant} OCI Image platform does not match the execution backend: image {}, backend {}",
            image.platform,
            backend.platform
        );
    }
    Ok(())
}

pub(super) fn observe_native_process(
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

fn adopt_writable_rootfs(bundle: &OciBundle, materialized: MaterializedRootfs) -> Result<()> {
    let source = materialized.preserve();
    let destination = bundle.rootfs()?.to_path_buf();
    fs::remove_dir(&destination).context("failed to remove empty OCI bundle rootfs")?;
    fs::rename(&source, &destination)
        .context("failed to move writable materialized rootfs into OCI bundle")?;
    Ok(())
}
