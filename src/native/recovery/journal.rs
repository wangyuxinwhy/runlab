use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use tempfile::NamedTempFile;

use crate::core::{
    BackendDetails, BackendFacts, ImageSlot, NativeFilesystemRealization,
    NativeRuntimeConfigRealization, NativeRuntimeInvocation, NetworkControl, OperationError,
    OperationErrorScope, ProcessSlot, RunId, RunNetworkFacts, RunNetworkRealization, StoredBytes,
};
use crate::integrity::{canonical_json, digest_bytes, set_private_file, sync_directory};
use crate::native::network::{RunNetworkMode, RunNetworkPlan};

use super::layout::{create_private_file, open_private_file, validate_regular_file};
use super::{
    JOURNAL_SCHEMA_VERSION, MAX_JOURNAL_BYTES, NativeParticipant, NativeRecoveryJournal,
    NativeRecoveryPhase, NativeSharedNetworkJournal, NativeSharedNetworkPhase,
};
use crate::native::resolver::ResolverSourceCheckpoint;

use super::NativeResolverProjectionJournal;

pub(super) fn ensure_resolver_projection_removable(
    projection: &NativeResolverProjectionJournal,
    participant: &str,
) -> Result<()> {
    if !matches!(
        projection,
        NativeResolverProjectionJournal::NotStarted
            | NativeResolverProjectionJournal::CleanupComplete
    ) {
        bail!("{participant} resolver projection cleanup is incomplete");
    }
    Ok(())
}

pub(super) fn validate_sidecar_input(
    slot: &StoredBytes,
    bytes: Option<&[u8]>,
    name: &str,
) -> Result<()> {
    match (slot, bytes) {
        (
            StoredBytes::Available { digest, size } | StoredBytes::Partial { digest, size, .. },
            Some(bytes),
        ) if *digest == digest_bytes(bytes) && *size == bytes.len() as u64 => Ok(()),
        (StoredBytes::Unavailable { .. } | StoredBytes::NotApplicable, None) => Ok(()),
        _ => bail!("native recovery {name} bytes do not match their durable slot"),
    }
}

pub(super) fn verify_sidecar(path: &Path, expected: Option<&[u8]>) -> Result<()> {
    validate_regular_file(path)?;
    let actual = fs::read(path)
        .with_context(|| format!("failed to read native sidecar {}", path.display()))?;
    if actual.as_slice() != expected.unwrap_or_default() {
        bail!("native recovery sidecar differs from its durable observation");
    }
    Ok(())
}

pub(super) fn write_sidecar(path: &Path, bytes: Option<&[u8]>) -> Result<()> {
    validate_regular_file(path)?;
    let parent = path
        .parent()
        .context("native recovery sidecar has no parent")?;
    let mut temporary =
        NamedTempFile::new_in(parent).context("failed to create native sidecar staging file")?;
    set_private_file(temporary.as_file())?;
    if let Some(bytes) = bytes {
        temporary
            .write_all(bytes)
            .with_context(|| format!("failed to write native sidecar {}", path.display()))?;
    }
    temporary
        .as_file_mut()
        .sync_all()
        .with_context(|| format!("failed to fsync native sidecar {}", path.display()))?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("failed to publish native sidecar {}", path.display()))?;
    sync_directory(parent)
}

/// The constraints only native recovery can state.
///
/// The invariants every Run Record shares -- name matching its facts, a
/// non-empty version, a positive runtime artifact, Run network facts matching
/// the requested control -- belong to `BackendFacts::validate` and are checked
/// there once. What remains here is what recovery alone knows: it can only
/// resume a native Linux Run, only under a runtime it supports, and only when
/// the runtime and filesystem realizations describe the same execution mode.
pub(super) fn validate_backend(backend: &BackendFacts) -> Result<()> {
    backend.validate()?;
    let BackendDetails::NativeLinux {
        runtime_invocation,
        runtime_config,
        filesystem,
        ..
    } = &backend.details
    else {
        bail!("native recovery requires native Linux backend facts");
    };
    if let NativeRuntimeInvocation::ApparmorProfile { profile } = runtime_invocation
        && profile != "runc"
    {
        bail!("native recovery backend has an unsupported AppArmor profile");
    }
    match (runtime_config, filesystem) {
        (
            NativeRuntimeConfigRealization::Accepted,
            NativeFilesystemRealization::OverlayFs { profile },
        ) if !profile.is_empty() => {}
        (
            NativeRuntimeConfigRealization::RootlessSingleId { size, .. },
            NativeFilesystemRealization::WritableMaterialized {
                container_uid: 0,
                host_uid,
                container_gid: 0,
                ..
            },
        ) if *size > 0 && *host_uid != 0 => {
            if backend.network != NetworkControl::None || backend.run_network.is_some() {
                bail!("rootless native recovery backend cannot own Run network facts");
            }
        }
        _ => bail!("native recovery backend has inconsistent runtime and filesystem realization"),
    }
    Ok(())
}

pub(super) fn runtime_id(run_id: RunId) -> String {
    let value = run_id.to_string();
    format!(
        "runlab-{}",
        value
            .strip_prefix("run-")
            .expect("Run identity always has the run- prefix")
    )
}

pub(super) fn managed_runtime_id(run_id: RunId) -> String {
    format!("{}-service", runtime_id(run_id))
}

pub(super) fn parse_recovery_entry_name(name: &str) -> Result<(RunId, bool)> {
    match name.strip_prefix(".prepare-") {
        Some(identity) => Ok((RunId::parse(identity)?, true)),
        None => Ok((RunId::parse(name)?, false)),
    }
}

pub(super) fn publish_staging(root: &File, run_id: RunId) -> Result<()> {
    let staging = format!(".prepare-{run_id}");
    let target = run_id.to_string();
    rustix::fs::renameat_with(
        root,
        staging.as_str(),
        root,
        target.as_str(),
        rustix::fs::RenameFlags::NOREPLACE,
    )
    .with_context(|| format!("failed to publish native recovery attempt {run_id}"))
}

pub(super) fn write_initial_journal(
    directory: &Path,
    journal: &NativeRecoveryJournal,
) -> Result<()> {
    validate_journal(journal, journal.run_id)?;
    let bytes = canonical_json(journal)?;
    if bytes.len() as u64 > MAX_JOURNAL_BYTES {
        bail!("native recovery journal exceeds {MAX_JOURNAL_BYTES} bytes");
    }
    let mut file = create_private_file(&directory.join("journal.json"))?;
    file.write_all(&bytes)
        .context("failed to write initial native recovery journal")?;
    file.sync_all()
        .context("failed to fsync initial native recovery journal")
}

pub(super) fn write_journal(directory: &Path, journal: &NativeRecoveryJournal) -> Result<()> {
    validate_journal(journal, journal.run_id)?;
    let bytes = canonical_json(journal)?;
    if bytes.len() as u64 > MAX_JOURNAL_BYTES {
        bail!("native recovery journal exceeds {MAX_JOURNAL_BYTES} bytes");
    }
    let path = directory.join("journal.json");
    let mut temporary = NamedTempFile::new_in(directory)
        .context("failed to create native recovery journal staging file")?;
    set_private_file(temporary.as_file())?;
    temporary
        .write_all(&bytes)
        .context("failed to write native recovery journal")?;
    temporary
        .as_file_mut()
        .sync_all()
        .context("failed to fsync native recovery journal")?;
    temporary
        .persist(&path)
        .map_err(|error| error.error)
        .context("failed to publish native recovery journal")?;
    sync_directory(directory)
}

pub(super) fn read_journal(directory: &Path, run_id: RunId) -> Result<NativeRecoveryJournal> {
    let path = directory.join("journal.json");
    let mut file = open_private_file(&path)?;
    let size = file
        .metadata()
        .context("failed to inspect native recovery journal")?
        .len();
    if size > MAX_JOURNAL_BYTES {
        bail!("native recovery journal exceeds {MAX_JOURNAL_BYTES} bytes");
    }
    let mut bytes = Vec::with_capacity(usize::try_from(size).context("journal is too large")?);
    Read::by_ref(&mut file)
        .take(MAX_JOURNAL_BYTES + 1)
        .read_to_end(&mut bytes)
        .context("failed to read native recovery journal")?;
    if bytes.len() as u64 > MAX_JOURNAL_BYTES {
        bail!("native recovery journal exceeds {MAX_JOURNAL_BYTES} bytes");
    }
    let journal: NativeRecoveryJournal =
        serde_json::from_slice(&bytes).context("native recovery journal is invalid")?;
    validate_journal(&journal, run_id)?;
    Ok(journal)
}

pub(super) fn validate_journal(
    journal: &NativeRecoveryJournal,
    expected_run_id: RunId,
) -> Result<()> {
    if journal.schema_version != JOURNAL_SCHEMA_VERSION {
        bail!(
            "unsupported native recovery journal version: expected {JOURNAL_SCHEMA_VERSION}, received {}",
            journal.schema_version
        );
    }
    if journal.generation == 0 {
        bail!("native recovery journal generation must be positive");
    }
    if journal.run_id != expected_run_id {
        bail!("native recovery journal Run identity does not match its directory");
    }
    if journal.runtime_id != runtime_id(journal.run_id) {
        bail!("native recovery journal runtime identity is invalid");
    }
    validate_backend(&journal.backend)?;
    validate_participant_state(
        "native",
        journal.phase,
        journal.process.as_ref(),
        journal.stdout.as_ref(),
        journal.stderr.as_ref(),
        journal.captured_at,
        journal.final_image.as_ref(),
    )?;
    if journal.phase == NativeRecoveryPhase::TerminalPrepared && journal.terminal_at.is_none() {
        bail!("native recovery terminal checkpoint requires a terminal timestamp");
    }
    if let Some(service) = &journal.managed_service {
        if service.runtime_id != managed_runtime_id(journal.run_id)
            || service.runtime_id == journal.runtime_id
        {
            bail!("Managed Service recovery runtime identity is invalid");
        }
        validate_participant_state(
            "Managed Service",
            service.phase,
            service.process.as_ref(),
            service.stdout.as_ref(),
            service.stderr.as_ref(),
            service.captured_at,
            service.final_image.as_ref(),
        )?;
        validate_participant_errors(NativeParticipant::ManagedService, &service.operation_errors)?;
        if let Some(readiness) = &service.readiness {
            readiness.validate()?;
        }
        if service.phase == NativeRecoveryPhase::TerminalPrepared && service.readiness.is_none() {
            bail!("Managed Service terminal checkpoint requires readiness facts");
        }
    }
    let needs_run_network = journal.managed_service.is_some()
        || journal.backend.network == crate::core::NetworkControl::Egress;
    match &journal.shared_network {
        Some(network) if needs_run_network => {
            validate_shared_network(network, journal.backend.run_network.as_ref())?;
            if let Some(plan) = network.plan.as_ref() {
                validate_network_plan_mode(
                    plan.mode(),
                    journal.backend.network,
                    journal.managed_service.is_some(),
                )?;
            }
            let execution_started = (journal.phase >= NativeRecoveryPhase::ExecutionPrepared
                && journal.phase < NativeRecoveryPhase::TerminalPrepared)
                || journal.managed_service.as_ref().is_some_and(|service| {
                    service.phase >= NativeRecoveryPhase::ExecutionPrepared
                        && service.phase < NativeRecoveryPhase::TerminalPrepared
                });
            if execution_started
                && matches!(
                    network.phase,
                    NativeSharedNetworkPhase::PlanPending | NativeSharedNetworkPhase::CreatePending
                )
            {
                bail!("native execution requires an active Run network checkpoint");
            }
        }
        None if !needs_run_network => {
            if journal.backend.run_network.is_some() {
                bail!("native backend has Run network facts without a recovery journal");
            }
        }
        Some(_) => bail!("network=none single-participant recovery has a Run network journal"),
        None => bail!("native topology requires a Run network recovery journal"),
    }
    validate_resolver_journal(journal)?;
    Ok(())
}

pub(super) fn validate_resolver_journal(journal: &NativeRecoveryJournal) -> Result<()> {
    match (&journal.resolver, journal.backend.network) {
        (None, NetworkControl::None) => return Ok(()),
        (None, NetworkControl::Egress) if journal.phase == NativeRecoveryPhase::PreAcceptance => {
            return Ok(());
        }
        (None, NetworkControl::Egress) => {
            bail!("accepted IPv4 egress recovery journal has no resolver")
        }
        (Some(_), NetworkControl::None) => {
            bail!("network=none recovery journal has resolver resources")
        }
        (Some(resolver), NetworkControl::Egress) => {
            resolver.facts.canonical_bytes()?;
            resolver.source.validate_against_facts(&resolver.facts)?;
            if let Some(network) = journal.backend.run_network.as_ref() {
                let RunNetworkRealization::Ipv4NatEgress {
                    resolver: network_resolver,
                    ..
                } = &network.realization
                else {
                    bail!("IPv4 egress recovery journal has non-egress network facts");
                };
                if network_resolver != &resolver.facts {
                    bail!("Run network resolver facts differ from native resolver facts");
                }
            }
            if resolver.managed_service.is_some() != journal.managed_service.is_some() {
                bail!("native resolver participant topology is inconsistent");
            }
            validate_resolver_projection(
                journal.phase,
                &resolver.primary,
                &resolver.source,
                "primary",
            )?;
            if let (Some(service), Some(projection)) =
                (&journal.managed_service, &resolver.managed_service)
            {
                validate_resolver_projection(
                    service.phase,
                    projection,
                    &resolver.source,
                    "Managed Service",
                )?;
            }
        }
    }
    Ok(())
}

pub(super) fn validate_resolver_projection(
    participant_phase: NativeRecoveryPhase,
    projection: &NativeResolverProjectionJournal,
    source: &ResolverSourceCheckpoint,
    participant: &str,
) -> Result<()> {
    let pending = match projection {
        NativeResolverProjectionJournal::NotStarted
        | NativeResolverProjectionJournal::CleanupComplete => None,
        NativeResolverProjectionJournal::MountPending { pending }
        | NativeResolverProjectionJournal::Mounted { pending, .. }
        | NativeResolverProjectionJournal::CleanupPending { pending, .. } => Some(pending),
    };
    if let Some(pending) = pending {
        pending.validate_against_source(source)?;
    }
    match projection {
        NativeResolverProjectionJournal::Mounted { pending, mounted }
        | NativeResolverProjectionJournal::CleanupPending {
            pending,
            mounted: Some(mounted),
        } if mounted.projection_mount_id() == 0
            || mounted.projection_mount_id() == pending.overlay_mount_id() =>
        {
            bail!("{participant} resolver projection mount identity is invalid");
        }
        _ => {}
    }
    if participant_phase < NativeRecoveryPhase::OverlayMounted
        && !matches!(projection, NativeResolverProjectionJournal::NotStarted)
    {
        bail!("{participant} resolver projection started before OverlayFS was mounted");
    }
    if (NativeRecoveryPhase::RuntimeStartPending..NativeRecoveryPhase::ProcessObserved)
        .contains(&participant_phase)
        && matches!(
            projection,
            NativeResolverProjectionJournal::NotStarted
                | NativeResolverProjectionJournal::MountPending { .. }
        )
    {
        bail!(
            "{participant} runtime requires an active resolver projection at phase {participant_phase:?}; found {projection:?}"
        );
    }
    if participant_phase >= NativeRecoveryPhase::CapturePending
        && !matches!(
            projection,
            NativeResolverProjectionJournal::NotStarted
                | NativeResolverProjectionJournal::CleanupComplete
        )
    {
        bail!("{participant} resolver projection remains active during Final capture");
    }
    Ok(())
}

pub(super) fn validate_participant_state(
    name: &str,
    phase: NativeRecoveryPhase,
    process: Option<&ProcessSlot>,
    stdout: Option<&StoredBytes>,
    stderr: Option<&StoredBytes>,
    captured_at: Option<DateTime<Utc>>,
    final_image: Option<&ImageSlot>,
) -> Result<()> {
    if let Some(process) = process {
        process
            .validate()
            .with_context(|| format!("{name} recovery journal has invalid Process facts"))?;
    }
    let observed_fields = [process.is_some(), stdout.is_some(), stderr.is_some()];
    if observed_fields.iter().any(|present| *present)
        && !observed_fields.iter().all(|present| *present)
    {
        bail!("{name} recovery journal has an incomplete process observation");
    }
    if phase >= NativeRecoveryPhase::ProcessObserved
        && !observed_fields.iter().all(|present| *present)
    {
        bail!("{name} recovery journal phase requires process and stream facts");
    }
    if phase == NativeRecoveryPhase::ProcessObserved
        && matches!(process, Some(ProcessSlot::Unavailable { .. }))
    {
        bail!("{name} observed process checkpoint requires available process facts");
    }
    if phase >= NativeRecoveryPhase::CapturePending
        && phase < NativeRecoveryPhase::TerminalPrepared
        && captured_at.is_none()
    {
        bail!("{name} recovery journal phase requires a capture timestamp");
    }
    if phase >= NativeRecoveryPhase::FinalPublished && final_image.is_none() {
        bail!("{name} recovery journal phase requires Final Image facts");
    }
    Ok(())
}

pub(super) fn validate_shared_network(
    network: &NativeSharedNetworkJournal,
    backend: Option<&RunNetworkFacts>,
) -> Result<()> {
    match network.phase {
        NativeSharedNetworkPhase::PlanPending => {
            if network.plan.is_some()
                || network.facts.is_some()
                || network.holder_pid.is_some()
                || network.holder_start_time_ticks.is_some()
                || network.holder_exit_observed_at.is_some()
                || backend.is_some()
            {
                bail!("pending Run network plan has premature resource facts");
            }
        }
        NativeSharedNetworkPhase::CreatePending => {
            let plan = network
                .plan
                .as_ref()
                .context("pending shared network creation has no durable plan")?;
            plan.validate().context("Run network plan is invalid")?;
            if network.facts.is_some()
                || network.holder_pid.is_some()
                || network.holder_start_time_ticks.is_some()
                || network.holder_exit_observed_at.is_some()
                || backend.is_some()
            {
                bail!("pending shared network has premature identity facts");
            }
        }
        NativeSharedNetworkPhase::Active => {
            validate_network_plan(network)?;
            validate_active_network_fields(network, backend)?;
            if network.holder_exit_observed_at.is_some() {
                bail!("active shared network has a holder exit observation");
            }
        }
        NativeSharedNetworkPhase::CleanupPending => {
            validate_cleanup_network_fields(network, backend)?;
            if network.holder_exit_observed_at.is_some() {
                bail!("pending shared network cleanup has a holder exit observation");
            }
        }
        NativeSharedNetworkPhase::CleanupComplete => {
            if network.holder_exit_observed_at.is_none() {
                bail!("completed shared network cleanup lacks a holder exit observation");
            }
            validate_cleanup_network_fields(network, backend)?;
        }
    }
    Ok(())
}

pub(super) fn validate_cleanup_network_fields(
    network: &NativeSharedNetworkJournal,
    backend: Option<&RunNetworkFacts>,
) -> Result<()> {
    match (&network.plan, &network.facts, backend) {
        (None, None, None) => {
            if network.holder_pid.is_some() || network.holder_start_time_ticks.is_some() {
                bail!("unplanned shared network cleanup has holder identity facts");
            }
        }
        (Some(_), None, None) => {
            validate_network_plan(network)?;
            if network.holder_pid.is_some() || network.holder_start_time_ticks.is_some() {
                bail!("uncreated shared network has partial holder identity facts");
            }
        }
        (Some(_), Some(_), Some(_)) => {
            validate_network_plan(network)?;
            validate_active_network_fields(network, backend)?;
        }
        _ => bail!("shared network cleanup has inconsistent resource facts"),
    }
    Ok(())
}

pub(super) fn validate_network_plan(network: &NativeSharedNetworkJournal) -> Result<()> {
    network
        .plan
        .as_ref()
        .context("shared network has no durable plan")?
        .validate()
        .context("Run network plan is invalid")
}

pub(super) fn validate_network_plan_mode(
    mode: RunNetworkMode,
    control: NetworkControl,
    managed: bool,
) -> Result<()> {
    match (mode, control, managed) {
        (RunNetworkMode::LoopbackOnly, NetworkControl::None, true)
        | (RunNetworkMode::EgressIpv4, NetworkControl::Egress, _) => Ok(()),
        (RunNetworkMode::LoopbackOnly, NetworkControl::None, false) => {
            bail!("single-participant network=none does not own a Run network")
        }
        (RunNetworkMode::LoopbackOnly, NetworkControl::Egress, _)
        | (RunNetworkMode::EgressIpv4, NetworkControl::None, _) => {
            bail!("Run network plan mode differs from the accepted network control")
        }
    }
}

pub(super) fn validate_active_network_fields(
    network: &NativeSharedNetworkJournal,
    backend: Option<&RunNetworkFacts>,
) -> Result<()> {
    let facts = network
        .facts
        .as_ref()
        .context("shared network identity is missing")?;
    if backend != Some(facts) {
        bail!("backend shared network identity differs from recovery facts");
    }
    validate_network_facts_for_plan(
        network
            .plan
            .as_ref()
            .context("active shared network has no durable plan")?,
        facts,
    )?;
    if network.holder_pid == Some(0)
        || network.holder_pid.is_none()
        || network.holder_start_time_ticks == Some(0)
        || network.holder_start_time_ticks.is_none()
    {
        bail!("shared network holder identity is incomplete");
    }
    Ok(())
}

pub(super) fn validate_network_facts_for_plan(
    plan: &RunNetworkPlan,
    facts: &RunNetworkFacts,
) -> Result<()> {
    match (plan.mode(), &facts.realization) {
        (RunNetworkMode::LoopbackOnly, RunNetworkRealization::LoopbackOnly) => Ok(()),
        (
            RunNetworkMode::EgressIpv4,
            RunNetworkRealization::Ipv4NatEgress {
                guest_address,
                gateway,
                prefix_length,
                resolver,
            },
        ) => {
            let egress = plan
                .egress()
                .context("Run network egress plan is invalid")?;
            if guest_address != &egress.guest_address().to_string()
                || gateway != &egress.host_address().to_string()
                || *prefix_length != egress.prefix_length()
            {
                bail!("Run network realization differs from the durable network plan");
            }
            resolver.canonical_bytes()?;
            Ok(())
        }
        _ => bail!("Run network realization differs from the durable network plan"),
    }
}

pub(super) fn validate_participant_errors(
    participant: NativeParticipant,
    errors: &[OperationError],
) -> Result<()> {
    match participant {
        NativeParticipant::Primary
            if errors
                .iter()
                .any(|error| error.scope == OperationErrorScope::ManagedService) =>
        {
            bail!("primary recovery error has a Managed Service scope");
        }
        NativeParticipant::ManagedService
            if errors
                .iter()
                .any(|error| error.scope != OperationErrorScope::ManagedService) =>
        {
            bail!("Managed Service recovery error has a non-service scope");
        }
        NativeParticipant::Primary | NativeParticipant::ManagedService => {}
    }
    Ok(())
}
