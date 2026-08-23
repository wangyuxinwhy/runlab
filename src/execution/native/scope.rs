//! The durable lifecycle a native Run owns outside its own process.
//!
//! A native Run publishes a recovery attempt before it is accepted and holds a
//! private network until it is terminal. Both outlive any single step of the
//! Run and both have to be released even when the step that created them
//! failed, so they are owned here rather than by the code that uses them.
//!
//! `RunScope` is that ownership for a single-participant Run. The Managed
//! Service topology advances two participants through the same attempt, so the
//! primitives it drives directly are declared here as well.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};

use crate::core::{BackendFacts, NetworkControl, OperationError, OperationErrorScope, RunId};
use crate::native::network::{
    EgressNetworkTools, NativeNetworkBinding, NativeNetworkTools, NetworkHolderHandle, RunNetwork,
    RunNetworkMode,
};
use crate::native::recovery::{
    NativeAttempt, NativeParticipant, NativeRecoveryPhase, NativeRecoveryStore,
    SharedNetworkCheckpoint, TerminalCheckpoint,
};
use crate::native::resolver::{ResolverConfig, ResolverSourceFile};
use crate::signal::TerminationFlag;

use crate::execution::{
    ParticipantState, PreparedBackend, RunCleanup, RunResult, RunState, Runner,
};

/// The host resources the native backend holds for the lifetime of one Run: a
/// cancellation registration, a durable recovery attempt, and the Run network
/// namespace with its binding.
///
/// Collecting them lets `execution` orchestrate a Run without naming any of
/// them. `execution` keeps a non-Linux twin of this type whose operations do
/// nothing, so the orchestration reads the same on every host.
#[derive(Default)]
pub(in crate::execution) struct RunScope {
    cancellation: Option<TerminationFlag>,
    attempt: Option<NativeAttempt>,
    network: Option<RunNetwork>,
    binding: Option<NativeNetworkBinding>,
}

/// What one participant borrows from the scope while it runs.
///
/// The scope owns the cancellation flag, the recovery attempt, and the Run
/// network binding for the whole Run; a participant needs all three for the
/// length of one execution and none of them afterwards. Handing them over as
/// one borrow keeps the invariant -- a native Run always has a flag and an
/// attempt -- stated where the scope establishes it, instead of restating it
/// at each use.
pub(super) struct ScopedExecution<'a> {
    pub(super) cancelled: &'a AtomicBool,
    pub(super) binding: Option<&'a NativeNetworkBinding>,
    pub(super) attempt: &'a mut NativeAttempt,
}

impl RunScope {
    pub(super) fn scoped_execution(&mut self) -> ScopedExecution<'_> {
        let Self {
            cancellation,
            attempt,
            binding,
            ..
        } = self;
        ScopedExecution {
            cancelled: cancellation
                .as_ref()
                .expect("native cancellation is registered")
                .flag(),
            binding: binding.as_ref(),
            attempt: attempt.as_mut().expect("native recovery attempt is owned"),
        }
    }

    pub(in crate::execution) fn open(
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
    pub(in crate::execution) fn abort_pre_acceptance(self, error: anyhow::Error) -> anyhow::Error {
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
    pub(in crate::execution) fn workspace(&self) -> Option<PathBuf> {
        self.attempt.as_ref().map(NativeAttempt::workspace)
    }

    /// Record acceptance in the recovery journal. Returns whether the Run may
    /// proceed; a journal failure is reported on the participant instead.
    pub(in crate::execution) fn mark_accepted(&mut self, state: &mut RunState) -> bool {
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

    pub(in crate::execution) fn start_network(
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
    pub(in crate::execution) fn requires_reconciliation(&self, state: &RunState) -> bool {
        self.attempt.is_some()
            && state
                .primary
                .operation_errors
                .iter()
                .any(|error| error.phase == "native_recovery")
    }

    /// Release the Run network and record the terminal checkpoint. Returns a
    /// cleanup message when the network could not be released.
    pub(in crate::execution) fn checkpoint_terminal(
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
    pub(in crate::execution) fn close(
        &mut self,
        network_cleanup_error: Option<String>,
    ) -> RunCleanup {
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

impl Runner<'_> {
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
}

pub(super) fn start_run_network(
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

pub(super) fn prepare_resolver_source(
    attempt: &mut NativeAttempt,
    resolver: Option<&ResolverConfig>,
) -> Result<Option<ResolverSourceFile>> {
    resolver
        .map(|resolver| attempt.prepare_resolver(resolver))
        .transpose()
}

pub(super) fn abandon_managed_pre_acceptance(
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

pub(super) fn mark_managed_accepted(
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
}
