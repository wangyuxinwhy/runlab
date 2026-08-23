use std::fs;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::Utc;

use crate::core::{
    BackendDetails, ImageSlot, ManagedServiceFacts, ManagedServiceReadiness,
    NativeFilesystemRealization, OperationError, OperationErrorScope, ProcessFacts, ProcessOutcome,
    ProcessSlot, RunId, RunRecord, StoredBytes, TERMINAL_RUN_RECORD_SCHEMA_VERSION,
    TerminalLifecycle, TerminalRunRecord,
};
use crate::filesystem::{FilesystemOwnership, TreeCapture};
use crate::image::ImageService;
use crate::integrity::digest_bytes;
use crate::native_backend::RuncRunner;
use crate::native_fs::OverlayRootfs;
use crate::native_network::{EgressNetworkTools, NetworkHolderHandle, RunNetworkMode};
use crate::native_recovery::{
    ManagedTerminalCheckpoint, NativeAttempt, NativeParticipant, NativeRecoveryEntry,
    NativeRecoveryPhase, NativeRecoveryStore, NativeResolverProjectionJournal,
    NativeSharedNetworkPhase, RecoveryCaptureCheckpoint, TerminalCheckpoint,
};
use crate::native_resolver::recover_cleanup;
use crate::reconciliation::{
    RunReconcileBatchItem, RunReconcileBatchOutcome, RunReconcileBatchResult, RunReconcileResult,
};
use crate::storage::RunDatabase;

pub(crate) fn reconcile_native_run(
    state_root: &Path,
    database: &RunDatabase,
    images: Option<&ImageService>,
    run_id: RunId,
    dry_run: bool,
) -> Result<RunReconcileResult> {
    let record = database.find(run_id)?;
    let entry = open_entry(state_root, run_id)?;
    match (record, entry) {
        (Some(RunRecord::Terminal(_)), None) => Ok(already_terminal(run_id)),
        (Some(RunRecord::Accepted(_)), None) => bail!(
            "accepted Run has no native recovery attempt; its backend ownership is unknown: {run_id}"
        ),
        (None, None) => bail!("Run is unknown: {run_id}"),
        (None, Some(NativeRecoveryEntry::Staging(staging))) => {
            reconcile_staging(run_id, staging, dry_run)
        }
        (Some(_), Some(NativeRecoveryEntry::Staging(_))) => {
            bail!("stored Run has only a native recovery staging attempt: {run_id}")
        }
        (None, Some(NativeRecoveryEntry::Published(attempt))) => {
            reconcile_pre_acceptance(run_id, *attempt, dry_run)
        }
        (Some(RunRecord::Terminal(_)), Some(NativeRecoveryEntry::Published(attempt))) => {
            reconcile_terminal_attempt(run_id, *attempt, dry_run)
        }
        (Some(RunRecord::Accepted(accepted)), Some(NativeRecoveryEntry::Published(attempt))) => {
            reconcile_accepted(database, images, &accepted, *attempt, dry_run)
        }
    }
}

pub(crate) fn reconcile_native_runs(
    state_root: &Path,
    database: &RunDatabase,
    images: Option<&ImageService>,
    after: Option<RunId>,
    limit: usize,
    dry_run: bool,
) -> Result<RunReconcileBatchResult> {
    let fetch_limit = limit
        .checked_add(1)
        .context("native Run reconciliation limit overflow")?;
    let accepted = database.list(Some("accepted"), after, fetch_limit)?;
    let attempts = NativeRecoveryStore::open_existing(state_root)?
        .map(|store| store.list_attempt_ids(after, fetch_limit))
        .transpose()?;
    let attempts_have_more = attempts.as_ref().is_some_and(|page| page.has_more);
    let mut candidates = accepted
        .records
        .iter()
        .map(record_run_id)
        .chain(attempts.into_iter().flat_map(|page| page.ids.into_iter()))
        .map(|run_id| (run_id.to_string(), run_id))
        .collect::<std::collections::BTreeMap<_, _>>()
        .into_iter()
        .rev()
        .map(|(_, run_id)| run_id)
        .collect::<Vec<_>>();
    let has_more = accepted.has_more || attempts_have_more || candidates.len() > limit;
    candidates.truncate(limit);
    let next_after = has_more.then(|| candidates.last().copied()).flatten();
    let mut failed = 0_usize;
    let items = candidates
        .into_iter()
        .map(|run_id| {
            let outcome = match reconcile_native_run(state_root, database, images, run_id, dry_run)
            {
                Ok(result) => RunReconcileBatchOutcome::Completed { result },
                Err(error) => {
                    failed += 1;
                    RunReconcileBatchOutcome::Failed {
                        error: format!("{error:#}"),
                    }
                }
            };
            RunReconcileBatchItem { run_id, outcome }
        })
        .collect();
    Ok(RunReconcileBatchResult {
        schema_version: 1,
        dry_run,
        items,
        failed,
        next_after,
    })
}

fn record_run_id(record: &RunRecord) -> RunId {
    match record {
        RunRecord::Accepted(record) => record.run_id,
        RunRecord::Terminal(record) => record.run_id,
    }
}

fn open_entry(state_root: &Path, run_id: RunId) -> Result<Option<NativeRecoveryEntry>> {
    NativeRecoveryStore::open_existing(state_root)?
        .map_or(Ok(None), |store| store.open_entry(run_id))
}

fn reconcile_staging(
    run_id: RunId,
    staging: crate::native_recovery::NativeStagingAttempt,
    dry_run: bool,
) -> Result<RunReconcileResult> {
    if staging.run_id() != run_id {
        bail!("native recovery staging identity does not match the requested Run");
    }
    if dry_run {
        return Ok(planned(run_id, vec!["staging_attempt_remove"]));
    }
    staging.remove()?;
    Ok(completed(
        run_id,
        "discarded_prepublication",
        false,
        vec!["staging_attempt_removed"],
    ))
}

fn reconcile_pre_acceptance(
    run_id: RunId,
    mut attempt: NativeAttempt,
    dry_run: bool,
) -> Result<RunReconcileResult> {
    if attempt.journal().phase() != NativeRecoveryPhase::PreAcceptance {
        bail!("native recovery attempt has no accepted Run Record: {run_id}");
    }
    if dry_run {
        return Ok(planned(run_id, vec!["attempt_remove"]));
    }
    cleanup_resources(&mut attempt, &mut Vec::new())?;
    attempt.remove()?;
    Ok(completed(
        run_id,
        "discarded_pre_acceptance",
        false,
        vec!["attempt_removed"],
    ))
}

fn reconcile_terminal_attempt(
    run_id: RunId,
    mut attempt: NativeAttempt,
    dry_run: bool,
) -> Result<RunReconcileResult> {
    if dry_run {
        return Ok(planned(run_id, vec!["resource_cleanup"]));
    }
    let mut actions = Vec::new();
    cleanup_resources(&mut attempt, &mut actions)?;
    attempt.remove()?;
    actions.push("attempt_removed");
    Ok(completed(
        run_id,
        "cleaned_terminal_attempt",
        false,
        actions,
    ))
}

fn reconcile_accepted(
    database: &RunDatabase,
    images: Option<&ImageService>,
    accepted: &crate::core::AcceptedRunRecord,
    attempt: NativeAttempt,
    dry_run: bool,
) -> Result<RunReconcileResult> {
    match (
        accepted.managed_service.as_ref(),
        attempt.journal().managed_service(),
    ) {
        (None, None) => reconcile_accepted_primary(database, images, accepted, attempt, dry_run),
        (Some(_), Some(_)) => {
            reconcile_accepted_managed(database, images, accepted, attempt, dry_run)
        }
        _ => bail!("accepted Run and native recovery attempt disagree on Managed Service"),
    }
}

fn reconcile_accepted_primary(
    database: &RunDatabase,
    images: Option<&ImageService>,
    accepted: &crate::core::AcceptedRunRecord,
    mut attempt: NativeAttempt,
    dry_run: bool,
) -> Result<RunReconcileResult> {
    let run_id = accepted.run_id;
    if dry_run {
        return Ok(primary_reconcile_plan(
            run_id,
            attempt.journal().resolver().is_some(),
        ));
    }
    let images = images.context("Image storage is required for native reconciliation")?;

    let terminal_prepared = attempt.journal().phase() == NativeRecoveryPhase::TerminalPrepared;
    let mut actions = Vec::new();
    cleanup_runtime(&attempt, NativeParticipant::Primary, &mut actions)?;
    cleanup_resolver(&mut attempt, NativeParticipant::Primary, &mut actions)?;
    let mut operation_errors = attempt.journal().operation_errors().to_vec();
    if !terminal_prepared {
        operation_errors.push(OperationError {
            scope: OperationErrorScope::Run,
            phase: "recovery".to_owned(),
            message: "supervisor_lost: native coordinator stopped before terminalization"
                .to_owned(),
        });
    }

    let mut recovered = recover_participant(
        images,
        &accepted.initial_image.digest,
        run_id,
        &mut attempt,
        NativeParticipant::Primary,
        operation_errors,
        &mut actions,
    )?;

    let rootfs = attempt.bundle_directory().join("rootfs");
    if reconcile_participant_filesystem(&attempt, NativeParticipant::Primary, &rootfs)? {
        actions.push("overlay_unmounted");
    }
    let terminal_at = attempt.journal().terminal_at().unwrap_or_else(Utc::now);
    if !terminal_prepared {
        attempt.prepare_terminal(TerminalCheckpoint {
            terminal_at,
            process: recovered.process.clone(),
            stdout: recovered.stdout.clone(),
            stderr: recovered.stderr.clone(),
            stdout_bytes: recovered.stdout_bytes.as_deref(),
            stderr_bytes: recovered.stderr_bytes.as_deref(),
            final_image: recovered.final_image.clone(),
            operation_errors: recovered.operation_errors.clone(),
        })?;
    }
    let network_cleanup_complete =
        cleanup_network_for_terminal(&mut attempt, &mut recovered.operation_errors, &mut actions);
    let terminal = TerminalRunRecord {
        schema_version: TERMINAL_RUN_RECORD_SCHEMA_VERSION,
        run_id,
        lifecycle: TerminalLifecycle::Terminal,
        accepted_at: accepted.accepted_at,
        terminal_at,
        requested_image_reference: accepted.requested_image_reference.clone(),
        initial_image: accepted.initial_image.clone(),
        runtime_config: accepted.runtime_config.clone(),
        controls: accepted.controls.clone(),
        backend: Some(attempt.journal().backend().clone()),
        process: recovered.process,
        stdout: recovered.stdout,
        stderr: recovered.stderr,
        final_image: recovered.final_image,
        operation_errors: recovered.operation_errors,
        managed_service: None,
    };
    database.terminal(
        &terminal,
        recovered.stdout_bytes.as_deref(),
        recovered.stderr_bytes.as_deref(),
    )?;
    actions.push("run_terminalized");
    if network_cleanup_complete {
        match attempt.remove_after_terminal() {
            Ok(()) => {
                actions.push("attempt_removed");
                Ok(completed(run_id, "reconciled", true, actions))
            }
            Err(error) => Ok(cleanup_pending(run_id, actions, &error)),
        }
    } else {
        Ok(completed_with_resources(
            run_id,
            "terminalized_cleanup_pending",
            true,
            actions,
            false,
        ))
    }
}

fn primary_reconcile_plan(run_id: RunId, has_resolver: bool) -> RunReconcileResult {
    let mut actions = vec!["runtime_cleanup"];
    if has_resolver {
        actions.push("resolver_projection_cleanup");
    }
    actions.extend(["overlay_unmount", "run_terminalize"]);
    planned(run_id, actions)
}

fn reconcile_accepted_managed(
    database: &RunDatabase,
    images: Option<&ImageService>,
    accepted: &crate::core::AcceptedRunRecord,
    mut attempt: NativeAttempt,
    dry_run: bool,
) -> Result<RunReconcileResult> {
    let run_id = accepted.run_id;
    if dry_run {
        return Ok(managed_reconcile_plan(
            run_id,
            attempt.journal().resolver().is_some(),
        ));
    }
    let images = images.context("Image storage is required for native reconciliation")?;
    let condition = accepted
        .managed_service
        .as_ref()
        .context("accepted Managed Service condition is missing")?;
    let primary_prepared = attempt.journal().phase() == NativeRecoveryPhase::TerminalPrepared;
    let service_prepared = attempt
        .journal()
        .managed_service()
        .context("native recovery Managed Service journal is missing")?
        .phase()
        == NativeRecoveryPhase::TerminalPrepared;
    let network_prepared = attempt
        .journal()
        .shared_network()
        .is_some_and(|network| network.phase() == NativeSharedNetworkPhase::CleanupComplete);

    let mut actions = Vec::new();
    cleanup_runtime(&attempt, NativeParticipant::ManagedService, &mut actions)?;
    cleanup_runtime(&attempt, NativeParticipant::Primary, &mut actions)?;
    cleanup_resolver(
        &mut attempt,
        NativeParticipant::ManagedService,
        &mut actions,
    )?;
    cleanup_resolver(&mut attempt, NativeParticipant::Primary, &mut actions)?;

    let (primary_errors, service_errors) = recovery_errors(
        attempt.journal().operation_errors(),
        attempt
            .journal()
            .managed_service()
            .context("native recovery Managed Service journal is missing")?
            .operation_errors(),
        primary_prepared,
        service_prepared,
        network_prepared,
    );

    let mut primary = recover_participant(
        images,
        &accepted.initial_image.digest,
        run_id,
        &mut attempt,
        NativeParticipant::Primary,
        primary_errors,
        &mut actions,
    )?;
    let service = recover_participant(
        images,
        &condition.initial_image.digest,
        run_id,
        &mut attempt,
        NativeParticipant::ManagedService,
        service_errors,
        &mut actions,
    )?;

    reconcile_overlay(&attempt, NativeParticipant::ManagedService, &mut actions)?;
    reconcile_overlay(&attempt, NativeParticipant::Primary, &mut actions)?;

    let readiness = attempt
        .journal()
        .managed_service()
        .and_then(|journal| journal.readiness().cloned())
        .unwrap_or_else(|| ManagedServiceReadiness::ProbeError {
            observed_at: Utc::now(),
            attempts: 0,
            error: "readiness facts were not durably observed before recovery".to_owned(),
        });
    let terminal_at = prepare_recovered_managed_checkpoints(
        &mut attempt,
        &primary,
        &service,
        &readiness,
        primary_prepared,
        service_prepared,
    )?;
    let network_cleanup_complete =
        cleanup_network_for_terminal(&mut attempt, &mut primary.operation_errors, &mut actions);
    terminalize_recovered_managed(
        database,
        accepted,
        attempt,
        ManagedRecovery {
            primary,
            service,
            readiness,
            terminal_at,
            network_cleanup_complete,
        },
        actions,
    )
}

fn prepare_recovered_managed_checkpoints(
    attempt: &mut NativeAttempt,
    primary: &RecoveredParticipant,
    service: &RecoveredParticipant,
    readiness: &ManagedServiceReadiness,
    primary_prepared: bool,
    service_prepared: bool,
) -> Result<chrono::DateTime<Utc>> {
    if !service_prepared {
        attempt.prepare_managed_terminal(ManagedTerminalCheckpoint {
            readiness: readiness.clone(),
            process: service.process.clone(),
            stdout: service.stdout.clone(),
            stderr: service.stderr.clone(),
            stdout_bytes: service.stdout_bytes.as_deref(),
            stderr_bytes: service.stderr_bytes.as_deref(),
            final_image: service.final_image.clone(),
            operation_errors: service.operation_errors.clone(),
        })?;
    }
    let terminal_at = attempt.journal().terminal_at().unwrap_or_else(Utc::now);
    if !primary_prepared {
        attempt.prepare_terminal(TerminalCheckpoint {
            terminal_at,
            process: primary.process.clone(),
            stdout: primary.stdout.clone(),
            stderr: primary.stderr.clone(),
            stdout_bytes: primary.stdout_bytes.as_deref(),
            stderr_bytes: primary.stderr_bytes.as_deref(),
            final_image: primary.final_image.clone(),
            operation_errors: primary.operation_errors.clone(),
        })?;
    }
    Ok(terminal_at)
}

fn managed_reconcile_plan(run_id: RunId, has_resolver: bool) -> RunReconcileResult {
    let mut actions = vec!["managed_runtime_cleanup", "primary_runtime_cleanup"];
    if has_resolver {
        actions.extend([
            "managed_resolver_projection_cleanup",
            "resolver_projection_cleanup",
        ]);
    }
    actions.extend([
        "managed_overlay_unmount",
        "primary_overlay_unmount",
        "shared_network_cleanup",
        "run_terminalize",
    ]);
    planned(run_id, actions)
}

fn cleanup_network_for_terminal(
    attempt: &mut NativeAttempt,
    operation_errors: &mut Vec<OperationError>,
    actions: &mut Vec<&'static str>,
) -> bool {
    match cleanup_shared_network(attempt, actions) {
        Ok(()) => true,
        Err(error) => {
            operation_errors.push(run_network_cleanup_error(&error));
            actions.push("run_network_cleanup_deferred");
            false
        }
    }
}

struct ManagedRecovery {
    primary: RecoveredParticipant,
    service: RecoveredParticipant,
    readiness: ManagedServiceReadiness,
    terminal_at: chrono::DateTime<Utc>,
    network_cleanup_complete: bool,
}

fn terminalize_recovered_managed(
    database: &RunDatabase,
    accepted: &crate::core::AcceptedRunRecord,
    attempt: NativeAttempt,
    recovered: ManagedRecovery,
    mut actions: Vec<&'static str>,
) -> Result<RunReconcileResult> {
    let condition = accepted
        .managed_service
        .as_ref()
        .context("accepted Managed Service condition is missing")?;
    let managed_service = ManagedServiceFacts {
        name: condition.name.clone(),
        requested_image_reference: condition.requested_image_reference.clone(),
        initial_image: condition.initial_image.clone(),
        runtime_config: condition.runtime_config.clone(),
        readiness_condition: condition.readiness.clone(),
        readiness: recovered.readiness,
        process: recovered.service.process,
        stdout: recovered.service.stdout,
        stderr: recovered.service.stderr,
        final_image: recovered.service.final_image,
        operation_errors: recovered.service.operation_errors,
    };
    let terminal = TerminalRunRecord {
        schema_version: TERMINAL_RUN_RECORD_SCHEMA_VERSION,
        run_id: accepted.run_id,
        lifecycle: TerminalLifecycle::Terminal,
        accepted_at: accepted.accepted_at,
        terminal_at: recovered.terminal_at,
        requested_image_reference: accepted.requested_image_reference.clone(),
        initial_image: accepted.initial_image.clone(),
        runtime_config: accepted.runtime_config.clone(),
        controls: accepted.controls.clone(),
        backend: Some(attempt.journal().backend().clone()),
        process: recovered.primary.process,
        stdout: recovered.primary.stdout,
        stderr: recovered.primary.stderr,
        final_image: recovered.primary.final_image,
        operation_errors: recovered.primary.operation_errors,
        managed_service: Some(managed_service),
    };
    database.terminal_with_managed_service(
        &terminal,
        recovered.primary.stdout_bytes.as_deref(),
        recovered.primary.stderr_bytes.as_deref(),
        recovered.service.stdout_bytes.as_deref(),
        recovered.service.stderr_bytes.as_deref(),
    )?;
    actions.push("run_terminalized");
    if recovered.network_cleanup_complete {
        match attempt.remove_after_terminal() {
            Ok(()) => {
                actions.push("attempt_removed");
                Ok(completed(accepted.run_id, "reconciled", true, actions))
            }
            Err(error) => Ok(cleanup_pending(accepted.run_id, actions, &error)),
        }
    } else {
        Ok(completed_with_resources(
            accepted.run_id,
            "terminalized_cleanup_pending",
            true,
            actions,
            false,
        ))
    }
}

fn supervisor_lost_error(scope: OperationErrorScope) -> OperationError {
    OperationError {
        scope,
        phase: "recovery".to_owned(),
        message: "supervisor_lost: native coordinator stopped before terminalization".to_owned(),
    }
}

fn recovery_errors(
    primary: &[OperationError],
    service: &[OperationError],
    primary_prepared: bool,
    service_prepared: bool,
    network_prepared: bool,
) -> (Vec<OperationError>, Vec<OperationError>) {
    let mut primary = primary.to_vec();
    let mut service = service.to_vec();
    if !primary_prepared || !service_prepared || !network_prepared {
        push_unique_error(
            &mut primary,
            supervisor_lost_error(OperationErrorScope::Run),
        );
    }
    if !service_prepared {
        push_unique_error(
            &mut service,
            supervisor_lost_error(OperationErrorScope::ManagedService),
        );
    }
    (primary, service)
}

fn push_unique_error(errors: &mut Vec<OperationError>, error: OperationError) {
    if !errors.contains(&error) {
        errors.push(error);
    }
}

fn already_terminal(run_id: RunId) -> RunReconcileResult {
    completed(run_id, "already_terminal", false, Vec::new())
}

fn planned(run_id: RunId, actions: Vec<&'static str>) -> RunReconcileResult {
    RunReconcileResult {
        schema_version: 2,
        run_id,
        status: "planned",
        terminalized: false,
        actions,
        resources_absent: false,
        cleanup_errors: Vec::new(),
    }
}

fn completed(
    run_id: RunId,
    status: &'static str,
    terminalized: bool,
    actions: Vec<&'static str>,
) -> RunReconcileResult {
    completed_with_resources(run_id, status, terminalized, actions, true)
}

fn completed_with_resources(
    run_id: RunId,
    status: &'static str,
    terminalized: bool,
    actions: Vec<&'static str>,
    resources_absent: bool,
) -> RunReconcileResult {
    RunReconcileResult {
        schema_version: 2,
        run_id,
        status,
        terminalized,
        actions,
        resources_absent,
        cleanup_errors: Vec::new(),
    }
}

fn cleanup_pending(
    run_id: RunId,
    actions: Vec<&'static str>,
    error: &anyhow::Error,
) -> RunReconcileResult {
    RunReconcileResult {
        schema_version: 2,
        run_id,
        status: "terminalized_cleanup_pending",
        terminalized: true,
        actions,
        resources_absent: false,
        cleanup_errors: vec![format!("native recovery attempt removal failed: {error:#}")],
    }
}

fn run_network_cleanup_error(error: &anyhow::Error) -> OperationError {
    OperationError {
        scope: OperationErrorScope::Run,
        phase: "resource_cleanup".to_owned(),
        message: format!("run_network_cleanup: {error:#}"),
    }
}

fn cleanup_shared_network(
    attempt: &mut NativeAttempt,
    actions: &mut Vec<&'static str>,
) -> Result<()> {
    let Some(network) = attempt.journal().shared_network() else {
        return Ok(());
    };
    let phase = network.phase();
    if phase == NativeSharedNetworkPhase::CleanupComplete {
        return Ok(());
    }
    let plan = network.plan().cloned();
    let holder = NetworkHolderHandle::open(&attempt.workspace(), attempt.journal().run_id())
        .context("failed to open the durable Run network holder")?;
    attempt.begin_shared_network_cleanup()?;
    if let Some(plan) = plan.as_ref()
        && plan.mode() == RunNetworkMode::EgressIpv4
    {
        EgressNetworkTools::discover()
            .context("native egress cleanup tools are unavailable")?
            .cleanup_plan(plan, Duration::from_secs(5))
            .context("failed to clean Run egress resources")?;
        actions.push("run_network_egress_removed");
    }
    if let Some(holder) = holder {
        holder
            .request_stop(Duration::from_secs(5))
            .context("failed to stop the durable Run network holder")?;
        actions.push("run_network_holder_stopped");
    } else {
        actions.push("run_network_holder_absent");
    }
    attempt.record_shared_network_cleanup(Utc::now())?;
    actions.push("run_network_cleanup_complete");
    Ok(())
}

fn cleanup_resources(attempt: &mut NativeAttempt, actions: &mut Vec<&'static str>) -> Result<()> {
    if attempt.journal().managed_service().is_some() {
        cleanup_runtime(attempt, NativeParticipant::ManagedService, actions)?;
    }
    cleanup_runtime(attempt, NativeParticipant::Primary, actions)?;
    if attempt.journal().managed_service().is_some() {
        cleanup_resolver(attempt, NativeParticipant::ManagedService, actions)?;
    }
    cleanup_resolver(attempt, NativeParticipant::Primary, actions)?;
    if attempt.journal().managed_service().is_some() {
        reconcile_overlay(attempt, NativeParticipant::ManagedService, actions)?;
    }
    reconcile_overlay(attempt, NativeParticipant::Primary, actions)?;
    cleanup_shared_network(attempt, actions)?;
    Ok(())
}

fn cleanup_resolver(
    attempt: &mut NativeAttempt,
    participant: NativeParticipant,
    actions: &mut Vec<&'static str>,
) -> Result<()> {
    let Some(resolver) = attempt.journal().resolver() else {
        return Ok(());
    };
    let projection = resolver.projection(participant)?.clone();
    let (pending, mounted) = match projection {
        NativeResolverProjectionJournal::NotStarted
        | NativeResolverProjectionJournal::CleanupComplete => return Ok(()),
        NativeResolverProjectionJournal::MountPending { pending } => (pending, None),
        NativeResolverProjectionJournal::Mounted { pending, mounted } => (pending, Some(mounted)),
        NativeResolverProjectionJournal::CleanupPending { pending, mounted } => (pending, mounted),
    };
    let source = attempt
        .open_resolver_source()?
        .context("native resolver recovery source is missing")?;
    let target = attempt
        .participant_bundle_directory(participant)?
        .join("rootfs/etc/resolv.conf");
    attempt.begin_resolver_cleanup(participant)?;
    recover_cleanup(source.path(), &target, &pending, mounted)?;
    attempt.record_resolver_cleanup(participant)?;
    actions.push(participant_action(
        participant,
        "resolver_projection_removed",
        "managed_resolver_projection_removed",
    ));
    Ok(())
}

fn cleanup_runtime(
    attempt: &NativeAttempt,
    participant: NativeParticipant,
    actions: &mut Vec<&'static str>,
) -> Result<()> {
    let rootless = rootless_ownership(attempt)?.is_some();
    let root = attempt.participant_runtime_root(participant)?;
    let cgroup_checkpoint = attempt.participant_cgroup_checkpoint(participant)?;
    let runtime_id = attempt.participant_runtime_id(participant)?;
    let phase = attempt.participant_phase(participant)?;
    let metadata = match fs::symlink_metadata(&root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let cgroup_reconciled = if rootless {
                false
            } else {
                reconcile_cgroup(&cgroup_checkpoint, runtime_id, participant, actions)?
            };
            if phase == NativeRecoveryPhase::RuntimeStartPending && !cgroup_reconciled {
                bail!(
                    "native runtime start was pending but an absent runtime root cannot prove resource absence"
                );
            }
            return Ok(());
        }
        Err(error) => return Err(error).context("failed to inspect native runtime root"),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("native runtime recovery root is not a real directory");
    }
    if fs::read_dir(&root)
        .context("failed to inspect native runtime state")?
        .next()
        .is_none()
    {
        let cgroup_reconciled = if rootless {
            false
        } else {
            reconcile_cgroup(&cgroup_checkpoint, runtime_id, participant, actions)?
        };
        if phase == NativeRecoveryPhase::RuntimeStartPending && !cgroup_reconciled {
            bail!(
                "native runtime start was pending but its empty runtime root cannot prove resource absence"
            );
        }
        fs::remove_dir(&root).context("failed to remove empty native runtime root")?;
        actions.push(participant_action(
            participant,
            "runtime_root_removed",
            "managed_runtime_root_removed",
        ));
        return Ok(());
    }
    let runner = RuncRunner::discover(Duration::from_secs(5))?;
    verify_runtime_identity(&runner, attempt)?;
    let BackendDetails::NativeLinux {
        runtime_invocation, ..
    } = &attempt.journal().backend().details
    else {
        bail!("native recovery journal has a non-native backend");
    };
    let runner = runner.configured_for_recovery(runtime_invocation)?;
    if runner.reconcile(&root, runtime_id)? {
        actions.push(participant_action(
            participant,
            "runtime_deleted",
            "managed_runtime_deleted",
        ));
    }
    if !rootless {
        let _ = reconcile_cgroup(&cgroup_checkpoint, runtime_id, participant, actions)?;
    }
    Ok(())
}

fn reconcile_cgroup(
    checkpoint: &Path,
    runtime_id: &str,
    participant: NativeParticipant,
    actions: &mut Vec<&'static str>,
) -> Result<bool> {
    let reconciled = crate::native_cgroup::reconcile_checkpoint(checkpoint, runtime_id)?;
    if reconciled {
        actions.push(participant_action(
            participant,
            "cgroup_removed",
            "managed_cgroup_removed",
        ));
    }
    Ok(reconciled)
}

fn reconcile_overlay(
    attempt: &NativeAttempt,
    participant: NativeParticipant,
    actions: &mut Vec<&'static str>,
) -> Result<()> {
    let rootfs = attempt
        .participant_bundle_directory(participant)?
        .join("rootfs");
    if reconcile_participant_filesystem(attempt, participant, &rootfs)? {
        actions.push(participant_action(
            participant,
            "overlay_unmounted",
            "managed_overlay_unmounted",
        ));
    }
    Ok(())
}

fn reconcile_participant_filesystem(
    attempt: &NativeAttempt,
    _participant: NativeParticipant,
    rootfs: &Path,
) -> Result<bool> {
    if rootless_ownership(attempt)?.is_some() {
        crate::native_fs::ensure_no_mounts_at_or_below(rootfs)?;
        Ok(false)
    } else {
        OverlayRootfs::reconcile(rootfs)
    }
}

fn rootless_ownership(attempt: &NativeAttempt) -> Result<Option<FilesystemOwnership>> {
    let BackendDetails::NativeLinux { filesystem, .. } = &attempt.journal().backend().details
    else {
        bail!("native recovery journal has a non-native backend");
    };
    match filesystem {
        NativeFilesystemRealization::OverlayFs { .. } => Ok(None),
        NativeFilesystemRealization::WritableMaterialized {
            container_uid: 0,
            host_uid,
            container_gid: 0,
            host_gid,
        } => Ok(Some(FilesystemOwnership::SingleId {
            host_uid: *host_uid,
            host_gid: *host_gid,
        })),
        NativeFilesystemRealization::WritableMaterialized { .. } => {
            bail!("native recovery journal has an unsupported writable rootfs ID mapping")
        }
    }
}

const fn participant_action(
    participant: NativeParticipant,
    primary: &'static str,
    service: &'static str,
) -> &'static str {
    match participant {
        NativeParticipant::Primary => primary,
        NativeParticipant::ManagedService => service,
    }
}

fn verify_runtime_identity(runner: &RuncRunner, attempt: &NativeAttempt) -> Result<()> {
    let BackendDetails::NativeLinux {
        runtime_name,
        runtime_version,
        runtime_commit,
        runtime_spec,
        runtime_digest,
        runtime_size,
        ..
    } = &attempt.journal().backend().details
    else {
        bail!("native recovery journal has a non-native backend");
    };
    let identity = runner.identity();
    if runtime_name != "runc"
        || runtime_version != &identity.version
        || runtime_commit != &identity.commit
        || runtime_spec != &identity.runtime_spec
        || runtime_digest != &identity.digest
        || runtime_size != &identity.size
    {
        bail!("installed runc identity differs from the interrupted native attempt");
    }
    Ok(())
}

struct RecoveredProcess {
    process: ProcessSlot,
    stdout: StoredBytes,
    stderr: StoredBytes,
    stdout_bytes: Option<Vec<u8>>,
    stderr_bytes: Option<Vec<u8>>,
}

struct RecoveredParticipant {
    process: ProcessSlot,
    stdout: StoredBytes,
    stderr: StoredBytes,
    stdout_bytes: Option<Vec<u8>>,
    stderr_bytes: Option<Vec<u8>>,
    final_image: ImageSlot,
    operation_errors: Vec<OperationError>,
}

fn recover_participant(
    images: &ImageService,
    initial_manifest: &crate::core::Digest,
    run_id: RunId,
    attempt: &mut NativeAttempt,
    participant: NativeParticipant,
    mut operation_errors: Vec<OperationError>,
    actions: &mut Vec<&'static str>,
) -> Result<RecoveredParticipant> {
    let recovered = recovered_process(attempt, participant)?;
    let (final_image, captured_at) = participant_capture_facts(attempt, participant)?;
    let rootfs = attempt
        .participant_bundle_directory(participant)?
        .join("rootfs");
    if final_image.is_none()
        && captured_at.is_none()
        && participant_capture_ready(attempt, &rootfs)?
    {
        attempt.begin_participant_recovery_capture(
            participant,
            RecoveryCaptureCheckpoint {
                captured_at: Utc::now(),
                process: recovered.process.clone(),
                stdout: recovered.stdout.clone(),
                stderr: recovered.stderr.clone(),
                stdout_bytes: recovered.stdout_bytes.as_deref(),
                stderr_bytes: recovered.stderr_bytes.as_deref(),
                operation_errors: operation_errors.clone(),
            },
        )?;
        actions.push(participant_action(
            participant,
            "recovery_capture_started",
            "managed_recovery_capture_started",
        ));
    }
    let final_image = recover_final_image(
        images,
        initial_manifest,
        run_id,
        attempt,
        participant,
        &mut operation_errors,
        actions,
    );
    Ok(RecoveredParticipant {
        process: recovered.process,
        stdout: recovered.stdout,
        stderr: recovered.stderr,
        stdout_bytes: recovered.stdout_bytes,
        stderr_bytes: recovered.stderr_bytes,
        final_image,
        operation_errors,
    })
}

fn participant_capture_facts(
    attempt: &NativeAttempt,
    participant: NativeParticipant,
) -> Result<(Option<&ImageSlot>, Option<chrono::DateTime<Utc>>)> {
    match participant {
        NativeParticipant::Primary => Ok((
            attempt.journal().final_image(),
            attempt.journal().captured_at(),
        )),
        NativeParticipant::ManagedService => {
            let service = attempt
                .journal()
                .managed_service()
                .context("native attempt has no Managed Service")?;
            Ok((service.final_image(), service.captured_at()))
        }
    }
}

fn recovered_process(
    attempt: &NativeAttempt,
    participant: NativeParticipant,
) -> Result<RecoveredProcess> {
    let (process, stdout, stderr) = match participant {
        NativeParticipant::Primary => (
            attempt.journal().process(),
            attempt.journal().stdout(),
            attempt.journal().stderr(),
        ),
        NativeParticipant::ManagedService => {
            let service = attempt
                .journal()
                .managed_service()
                .context("native attempt has no Managed Service")?;
            (service.process(), service.stdout(), service.stderr())
        }
    };
    if let (Some(process), Some(stdout), Some(stderr)) = (process, stdout, stderr) {
        let stdout_bytes = recover_stream(stdout, &attempt.participant_stdout_path(participant))?;
        let stderr_bytes = recover_stream(stderr, &attempt.participant_stderr_path(participant))?;
        return Ok(RecoveredProcess {
            process: process.clone(),
            stdout: stdout.clone(),
            stderr: stderr.clone(),
            stdout_bytes,
            stderr_bytes,
        });
    }

    if attempt.participant_phase(participant)? < NativeRecoveryPhase::RuntimeStartPending {
        return Ok(RecoveredProcess {
            process: ProcessSlot::available(ProcessFacts {
                terminal_outcome: ProcessOutcome::NotStarted,
                exit_code: None,
                started_at: None,
                ended_at: Some(Utc::now()),
                oom_killed: None,
                backend_error: Some(
                    "supervisor stopped before the native process start became pending".to_owned(),
                ),
            }),
            stdout: StoredBytes::NotApplicable,
            stderr: StoredBytes::NotApplicable,
            stdout_bytes: None,
            stderr_bytes: None,
        });
    }

    let process = ProcessSlot::Unavailable {
        error: "process facts were not durably observed before recovery".to_owned(),
    };
    Ok(RecoveredProcess {
        process,
        stdout: StoredBytes::Unavailable {
            error: "stdout was not durably observed before recovery".to_owned(),
        },
        stderr: StoredBytes::Unavailable {
            error: "stderr was not durably observed before recovery".to_owned(),
        },
        stdout_bytes: None,
        stderr_bytes: None,
    })
}

fn recover_stream(slot: &StoredBytes, path: &Path) -> Result<Option<Vec<u8>>> {
    let expected = match slot {
        StoredBytes::Available { digest, size } | StoredBytes::Partial { digest, size, .. } => {
            Some((digest, *size))
        }
        StoredBytes::Unavailable { .. } | StoredBytes::NotApplicable => None,
    };
    let Some((digest, size)) = expected else {
        return Ok(None);
    };
    let metadata = fs::symlink_metadata(path).context("failed to inspect recovered stream")?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() != size {
        bail!("recovered stream does not match its durable facts");
    }
    let bytes = fs::read(path).context("failed to read recovered stream")?;
    if bytes.len() as u64 != size || &digest_bytes(&bytes) != digest {
        bail!("recovered stream does not match its durable facts");
    }
    Ok(Some(bytes))
}

fn recover_final_image(
    images: &ImageService,
    initial_manifest: &crate::core::Digest,
    run_id: RunId,
    attempt: &mut NativeAttempt,
    participant: NativeParticipant,
    operation_errors: &mut Vec<OperationError>,
    actions: &mut Vec<&'static str>,
) -> ImageSlot {
    let (final_image, captured_at) = match participant {
        NativeParticipant::Primary => (
            attempt.journal().final_image(),
            attempt.journal().captured_at(),
        ),
        NativeParticipant::ManagedService => {
            let Some(service) = attempt.journal().managed_service() else {
                return ImageSlot::Unavailable {
                    error: "Managed Service recovery journal is missing".to_owned(),
                };
            };
            (service.final_image(), service.captured_at())
        }
    };
    if let Some(final_image) = final_image {
        return final_image.clone();
    }
    let Some(captured_at) = captured_at else {
        return ImageSlot::Unavailable {
            error: "Final Image was not durably captured before recovery".to_owned(),
        };
    };
    let workspace = match attempt.participant_workspace(participant) {
        Ok(workspace) => workspace,
        Err(error) => {
            operation_errors.push(participant_error(
                participant,
                "final_image_capture",
                format!("{error:#}"),
            ));
            return ImageSlot::Unavailable {
                error: "Final Image recovery failed".to_owned(),
            };
        }
    };
    let ownership = match rootless_ownership(attempt) {
        Ok(Some(ownership)) => ownership,
        Ok(None) => FilesystemOwnership::Native,
        Err(error) => {
            operation_errors.push(participant_error(
                participant,
                "final_image_capture",
                format!("{error:#}"),
            ));
            return ImageSlot::Unavailable {
                error: "Final Image recovery failed".to_owned(),
            };
        }
    };
    let captured = TreeCapture::with_ownership(ownership)
        .capture_inventory(&workspace.join("lower/rootfs"))
        .and_then(|before| {
            TreeCapture::with_ownership(ownership)
                .capture_in(&workspace.join("bundle/rootfs"), &workspace)
                .and_then(|after| {
                    images.capture_filesystem(
                        initial_manifest,
                        &before,
                        &after,
                        &run_id,
                        captured_at,
                        &workspace,
                    )
                })
        });
    match captured {
        Ok(capture) => {
            let final_image = ImageSlot::Available {
                manifest: capture.image.manifest,
            };
            if let Some(message) = capture.cleanup_error {
                operation_errors.push(participant_error(participant, "capture_cleanup", message));
            }
            if let Err(error) = attempt.record_participant_final(participant, final_image.clone()) {
                operation_errors.push(participant_error(
                    participant,
                    "recovery_checkpoint",
                    format!("{error:#}"),
                ));
            } else {
                actions.push(participant_action(
                    participant,
                    "final_image_published",
                    "managed_final_image_published",
                ));
            }
            final_image
        }
        Err(error) => {
            operation_errors.push(participant_error(
                participant,
                "final_image_capture",
                format!("{error:#}"),
            ));
            ImageSlot::Unavailable {
                error: "Final Image recovery failed".to_owned(),
            }
        }
    }
}

fn participant_capture_ready(attempt: &NativeAttempt, rootfs: &Path) -> Result<bool> {
    if rootless_ownership(attempt)?.is_some() {
        crate::native_fs::ensure_no_mounts_at_or_below(rootfs)?;
        let metadata = match fs::symlink_metadata(rootfs) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error).context("failed to inspect writable rootfs"),
        };
        Ok(!metadata.file_type().is_symlink() && metadata.is_dir())
    } else {
        OverlayRootfs::recovery_capture_ready(rootfs)
    }
}

fn participant_error(
    participant: NativeParticipant,
    phase: &str,
    message: String,
) -> OperationError {
    OperationError {
        scope: match participant {
            NativeParticipant::Primary => OperationErrorScope::Primary,
            NativeParticipant::ManagedService => OperationErrorScope::ManagedService,
        },
        phase: phase.to_owned(),
        message,
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;
    use crate::core::{
        ACCEPTED_RUN_RECORD_SCHEMA_VERSION, AcceptedLifecycle, AcceptedRunRecord, Architecture,
        BackendFacts, ManagedServiceCondition, NetworkControl, OciDescriptor, Platform,
        ProcessFacts, ProcessOutcome, RunControls, RunNetworkFacts, RunNetworkRealization,
        ServiceName, TcpReadinessCondition,
    };
    use crate::native_recovery::SharedNetworkCheckpoint;
    use crate::oci::OciLayout;

    #[test]
    fn managed_pre_checkpoint_crash_terminalizes_once() {
        let state = tempfile::tempdir().expect("state");
        let database = RunDatabase::open(state.path().join("runs.sqlite3")).expect("database");
        let images = ImageService::new(OciLayout::open(state.path().join("oci")).expect("OCI"));
        let run_id = RunId::new();
        let runtime = b"primary runtime";
        let service_runtime = b"service runtime";
        let stdin = b"task";
        let accepted = accepted(run_id, runtime, service_runtime, stdin);
        database
            .accept_with_managed_service(&accepted, runtime, stdin, Some(service_runtime))
            .expect("accept");
        let store = NativeRecoveryStore::open(state.path()).expect("recovery store");
        let attempt = store.prepare_managed(run_id, backend()).expect("attempt");
        drop(attempt);

        let result = reconcile_native_run(state.path(), &database, Some(&images), run_id, false)
            .expect("reconcile");
        assert!(result.terminalized);
        let RunRecord::Terminal(terminal) = database.find(run_id).expect("find").expect("Run")
        else {
            panic!("Run must be terminal");
        };
        assert_eq!(
            terminal.process.facts().map(|facts| facts.terminal_outcome),
            Some(ProcessOutcome::NotStarted)
        );
        let service = terminal.managed_service.expect("service facts");
        assert_eq!(
            service.process.facts().map(|facts| facts.terminal_outcome),
            Some(ProcessOutcome::NotStarted)
        );
        assert!(matches!(
            service.readiness,
            ManagedServiceReadiness::ProbeError { attempts: 0, .. }
        ));
        assert!(
            service
                .operation_errors
                .iter()
                .all(|error| error.scope == OperationErrorScope::ManagedService)
        );
        let repeated = reconcile_native_run(state.path(), &database, Some(&images), run_id, false)
            .expect("repeat reconcile");
        assert_eq!(repeated.status, "already_terminal");
        assert!(!repeated.terminalized);
    }

    #[test]
    fn managed_observed_process_and_streams_survive_recovery() {
        let state = tempfile::tempdir().expect("state");
        let store = NativeRecoveryStore::open(state.path()).expect("recovery store");
        let run_id = RunId::new();
        let mut attempt = store.prepare_managed(run_id, backend()).expect("attempt");
        attempt
            .advance_phase(NativeRecoveryPhase::Accepted)
            .expect("primary accepted");
        attempt
            .advance_participant_phase(
                NativeParticipant::ManagedService,
                NativeRecoveryPhase::Accepted,
            )
            .expect("service accepted");
        attempt
            .record_network_plan(crate::native_network::RunNetworkPlan::loopback(run_id))
            .expect("network plan");
        attempt
            .record_shared_network(SharedNetworkCheckpoint::for_test(
                RunNetworkFacts {
                    namespace_device: 7,
                    namespace_inode: 8,
                    realization: RunNetworkRealization::LoopbackOnly,
                },
                i32::MAX as u32,
                123,
            ))
            .expect("network checkpoint");
        let started_at = Utc::now();
        let process = ProcessSlot::available(ProcessFacts {
            terminal_outcome: ProcessOutcome::ProcessExited,
            exit_code: Some(0),
            started_at: Some(started_at),
            ended_at: Some(started_at),
            oom_killed: Some(false),
            backend_error: None,
        });
        let stdout = b"service stdout";
        let stderr = b"service stderr";
        attempt
            .record_participant_process(
                NativeParticipant::ManagedService,
                process.clone(),
                available(stdout),
                available(stderr),
                Some(stdout),
                Some(stderr),
                Vec::new(),
            )
            .expect("process checkpoint");
        drop(attempt);
        let reopened = store.open_attempt(run_id).expect("open").expect("attempt");
        let recovered = recovered_process(&reopened, NativeParticipant::ManagedService)
            .expect("recover process");
        assert_eq!(recovered.process, process);
        assert_eq!(recovered.stdout_bytes.as_deref(), Some(stdout.as_slice()));
        assert_eq!(recovered.stderr_bytes.as_deref(), Some(stderr.as_slice()));
    }

    #[test]
    fn state_wide_dry_run_merges_attempts_and_accepted_rows_with_pagination() {
        let state = tempfile::tempdir().expect("state");
        let database = RunDatabase::open(state.path().join("runs.sqlite3")).expect("database");
        let store = NativeRecoveryStore::open(state.path()).expect("recovery store");
        let matched_id = RunId::new();
        let matched = accepted_primary(matched_id, b"runtime", b"stdin");
        database
            .accept(&matched, b"runtime", b"stdin")
            .expect("matched accepted Run");
        let mut matched_attempt = store
            .prepare(matched_id, backend())
            .expect("matched attempt");
        matched_attempt
            .advance_phase(NativeRecoveryPhase::Accepted)
            .expect("matched accepted phase");
        drop(matched_attempt);

        let pre_acceptance_id = RunId::new();
        drop(
            store
                .prepare(pre_acceptance_id, backend())
                .expect("pre-acceptance attempt"),
        );

        let missing_attempt_id = RunId::new();
        let missing_attempt = accepted_primary(missing_attempt_id, b"runtime", b"stdin");
        database
            .accept(&missing_attempt, b"runtime", b"stdin")
            .expect("accepted Run without attempt");

        let first = reconcile_native_runs(state.path(), &database, None, None, 2, true)
            .expect("first page");
        assert_eq!(first.items.len(), 2);
        assert!(first.next_after.is_some());
        let second =
            reconcile_native_runs(state.path(), &database, None, first.next_after, 2, true)
                .expect("second page");
        assert_eq!(second.items.len(), 1);
        assert_eq!(second.next_after, None);

        let failed = first.failed + second.failed;
        let items = first
            .items
            .into_iter()
            .chain(second.items)
            .collect::<Vec<_>>();
        assert_eq!(items.len(), 3);
        assert_eq!(failed, 1);
        assert!(items.iter().any(|item| {
            item.run_id == missing_attempt_id
                && matches!(item.outcome, RunReconcileBatchOutcome::Failed { .. })
        }));
        assert!(items.iter().any(|item| {
            item.run_id == matched_id
                && matches!(item.outcome, RunReconcileBatchOutcome::Completed { .. })
        }));
        assert!(items.iter().any(|item| {
            item.run_id == pre_acceptance_id
                && matches!(item.outcome, RunReconcileBatchOutcome::Completed { .. })
        }));
        assert!(
            store
                .open_attempt(matched_id)
                .expect("matched lookup")
                .is_some()
        );
        assert!(
            store
                .open_attempt(pre_acceptance_id)
                .expect("pre-acceptance lookup")
                .is_some()
        );
    }

    #[test]
    fn state_wide_reconciliation_discards_prepublication_staging_once() {
        let state = tempfile::tempdir().expect("state");
        let database = RunDatabase::open(state.path().join("runs.sqlite3")).expect("database");
        NativeRecoveryStore::open(state.path()).expect("recovery store");
        let run_id = RunId::new();
        let staging = state
            .path()
            .join("recovery/native")
            .join(format!(".prepare-{run_id}"));
        fs::create_dir(&staging).expect("staging");
        fs::set_permissions(&staging, fs::Permissions::from_mode(0o700))
            .expect("staging permissions");

        let dry_run = reconcile_native_runs(state.path(), &database, None, None, 20, true)
            .expect("dry-run staging reconciliation");
        assert_eq!(dry_run.failed, 0);
        assert_eq!(dry_run.items.len(), 1);
        let RunReconcileBatchOutcome::Completed { result } = &dry_run.items[0].outcome else {
            panic!("staging dry-run failed");
        };
        assert_eq!(result.status, "planned");
        assert_eq!(result.actions, vec!["staging_attempt_remove"]);
        assert!(staging.exists(), "dry-run removed staging");

        let applied = reconcile_native_runs(state.path(), &database, None, None, 20, false)
            .expect("apply staging reconciliation");
        assert_eq!(applied.failed, 0);
        let RunReconcileBatchOutcome::Completed { result } = &applied.items[0].outcome else {
            panic!("staging reconciliation failed");
        };
        assert_eq!(result.status, "discarded_prepublication");
        assert_eq!(result.actions, vec!["staging_attempt_removed"]);
        assert!(!staging.exists());

        let repeated = reconcile_native_runs(state.path(), &database, None, None, 20, false)
            .expect("repeat staging reconciliation");
        assert!(repeated.items.is_empty());
        assert_eq!(repeated.failed, 0);
    }

    #[test]
    fn pending_runtime_with_no_root_fails_closed() {
        let state = tempfile::tempdir().expect("state");
        let store = NativeRecoveryStore::open(state.path()).expect("recovery store");
        let mut attempt = store.prepare(RunId::new(), backend()).expect("attempt");
        attempt
            .advance_phase(NativeRecoveryPhase::RuntimeStartPending)
            .expect("runtime pending");
        let error = cleanup_runtime(&attempt, NativeParticipant::Primary, &mut Vec::new())
            .expect_err("absence cannot prove the pending spawn did not occur");
        assert!(error.to_string().contains("cannot prove resource absence"));
    }

    #[test]
    #[ignore = "requires writable cgroup v2"]
    fn pending_empty_runtime_with_owned_cgroup_checkpoint_reconciles() {
        let state = tempfile::tempdir().expect("state");
        let store = NativeRecoveryStore::open(state.path()).expect("recovery store");
        let mut attempt = store.prepare(RunId::new(), backend()).expect("attempt");
        attempt
            .advance_phase(NativeRecoveryPhase::RuntimeStartPending)
            .expect("runtime pending");
        let checkpoint = attempt
            .participant_cgroup_checkpoint(NativeParticipant::Primary)
            .expect("checkpoint");
        let runtime_id = attempt
            .participant_runtime_id(NativeParticipant::Primary)
            .expect("runtime id");
        fs::create_dir(
            attempt
                .participant_runtime_root(NativeParticipant::Primary)
                .expect("runtime root"),
        )
        .expect("empty runtime root");
        let prepared = crate::native_cgroup::PreparedNativeCgroup::prepare(runtime_id, &checkpoint)
            .expect("cgroup");
        drop(prepared);
        let mut actions = Vec::new();

        cleanup_runtime(&attempt, NativeParticipant::Primary, &mut actions)
            .expect("owned cgroup proves cleanup");

        assert_eq!(actions, ["cgroup_removed", "runtime_root_removed"]);
        let checkpoint_value: serde_json::Value =
            serde_json::from_slice(&fs::read(&checkpoint).expect("cgroup tombstone"))
                .expect("checkpoint JSON");
        assert_eq!(checkpoint_value["resources_absent"], true);
        let mut repeated_actions = Vec::new();
        cleanup_runtime(&attempt, NativeParticipant::Primary, &mut repeated_actions)
            .expect("cgroup tombstone is idempotent");
        assert_eq!(repeated_actions, ["cgroup_removed"]);
    }

    #[test]
    fn cleanup_pending_with_no_runtime_root_is_already_clean() {
        let state = tempfile::tempdir().expect("state");
        let store = NativeRecoveryStore::open(state.path()).expect("recovery store");
        let mut attempt = store.prepare(RunId::new(), backend()).expect("attempt");
        attempt
            .record_process(
                ProcessSlot::available(ProcessFacts::not_started()),
                StoredBytes::NotApplicable,
                StoredBytes::NotApplicable,
                None,
                None,
                Vec::new(),
            )
            .expect("process observed");
        attempt
            .begin_participant_capture(NativeParticipant::Primary, Utc::now())
            .expect("capture pending");
        attempt
            .record_participant_final(
                NativeParticipant::Primary,
                ImageSlot::Unavailable {
                    error: "fixture capture unavailable".to_owned(),
                },
            )
            .expect("final recorded");
        attempt
            .advance_phase(NativeRecoveryPhase::CleanupPending)
            .expect("cleanup pending");
        cleanup_runtime(&attempt, NativeParticipant::Primary, &mut Vec::new())
            .expect("deleted runtime is already clean");
    }

    #[test]
    fn repeated_recovery_does_not_duplicate_supervisor_loss() {
        let existing = supervisor_lost_error(OperationErrorScope::Run);
        let (primary, service) =
            recovery_errors(std::slice::from_ref(&existing), &[], true, true, false);
        assert_eq!(primary, vec![existing]);
        assert!(service.is_empty());
    }

    fn accepted(
        run_id: RunId,
        runtime: &[u8],
        service_runtime: &[u8],
        stdin: &[u8],
    ) -> AcceptedRunRecord {
        AcceptedRunRecord {
            schema_version: ACCEPTED_RUN_RECORD_SCHEMA_VERSION,
            run_id,
            lifecycle: AcceptedLifecycle::Accepted,
            accepted_at: Utc::now(),
            requested_image_reference: Some("runlab/primary:latest".to_owned()),
            initial_image: descriptor(b"primary image"),
            runtime_config: available(runtime),
            controls: RunControls {
                stdin: available(stdin),
                timeout_seconds: 30,
                stdout_limit_bytes: 1024,
                stderr_limit_bytes: 1024,
                network: NetworkControl::None,
            },
            managed_service: Some(ManagedServiceCondition {
                name: ServiceName::parse("database").expect("service name"),
                requested_image_reference: Some("runlab/database:latest".to_owned()),
                initial_image: descriptor(b"service image"),
                runtime_config: available(service_runtime),
                readiness: TcpReadinessCondition {
                    port: 5432,
                    timeout_seconds: 5,
                },
            }),
        }
    }

    fn accepted_primary(run_id: RunId, runtime: &[u8], stdin: &[u8]) -> AcceptedRunRecord {
        AcceptedRunRecord {
            schema_version: ACCEPTED_RUN_RECORD_SCHEMA_VERSION,
            run_id,
            lifecycle: AcceptedLifecycle::Accepted,
            accepted_at: Utc::now(),
            requested_image_reference: Some("runlab/primary:latest".to_owned()),
            initial_image: descriptor(b"primary image"),
            runtime_config: available(runtime),
            controls: RunControls {
                stdin: available(stdin),
                timeout_seconds: 30,
                stdout_limit_bytes: 1024,
                stderr_limit_bytes: 1024,
                network: NetworkControl::None,
            },
            managed_service: None,
        }
    }

    fn backend() -> BackendFacts {
        BackendFacts {
            name: "native_linux".to_owned(),
            version: "1".to_owned(),
            platform: Platform::linux(Architecture::Arm64),
            network: NetworkControl::None,
            run_network: None,
            details: BackendDetails::NativeLinux {
                runtime_name: "runc".to_owned(),
                runtime_version: "1.3.6".to_owned(),
                runtime_commit: "fixture".to_owned(),
                runtime_spec: "1.2.1".to_owned(),
                runtime_digest: digest_bytes(b"runc fixture"),
                runtime_size: 12,
                kernel_release: "fixture".to_owned(),
                runtime_invocation: crate::core::NativeRuntimeInvocation::Direct,
                runtime_config: crate::core::NativeRuntimeConfigRealization::Accepted,
                filesystem: crate::core::NativeFilesystemRealization::OverlayFs {
                    profile: "fixture".to_owned(),
                },
            },
        }
    }

    fn descriptor(bytes: &[u8]) -> OciDescriptor {
        OciDescriptor {
            digest: digest_bytes(bytes),
            size: u64::try_from(bytes.len()).expect("size"),
            media_type: "application/vnd.oci.image.manifest.v1+json".to_owned(),
        }
    }

    fn available(bytes: &[u8]) -> StoredBytes {
        StoredBytes::Available {
            digest: digest_bytes(bytes),
            size: u64::try_from(bytes.len()).expect("size"),
        }
    }
}
