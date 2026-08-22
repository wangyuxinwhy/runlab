#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

use crate::core::{
    BackendDetails, BackendFacts, ImageSlot, ManagedServiceReadiness, NativeFilesystemRealization,
    NativeRuntimeConfigRealization, NativeRuntimeInvocation, NetworkControl, OperationError,
    OperationErrorScope, ProcessSlot, RunId, RunNetworkFacts, RunNetworkRealization, StoredBytes,
};
use crate::integrity::{canonical_json, digest_bytes, ensure_private_directory};
#[cfg(target_os = "linux")]
use crate::native_network::{EgressNetworkTools, HostNetworkLock, acquire_host_network_lock};
use crate::native_network::{RunNetworkMode, RunNetworkPlan};
#[cfg(target_os = "linux")]
use crate::native_resolver::{
    ResolverConfig, ResolverProjectionMounted, ResolverProjectionPending, ResolverSourceCheckpoint,
    ResolverSourceFile,
};

const JOURNAL_SCHEMA_VERSION: u32 = 5;
const MAX_JOURNAL_BYTES: u64 = 1024 * 1024;
const MAX_RECOVERY_DIRECTORY_ENTRIES: usize = 100_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NativeRecoveryPhase {
    PreAcceptance,
    Accepted,
    ExecutionPrepared,
    OverlayMountPending,
    OverlayMounted,
    RuntimeStartPending,
    RuntimeActive,
    ProcessObserved,
    CapturePending,
    FinalPublished,
    CleanupPending,
    CleanupComplete,
    TerminalPrepared,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NativeParticipant {
    Primary,
    ManagedService,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NativeSharedNetworkPhase {
    PlanPending,
    CreatePending,
    Active,
    CleanupPending,
    CleanupComplete,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeSharedNetworkJournal {
    phase: NativeSharedNetworkPhase,
    plan: Option<RunNetworkPlan>,
    facts: Option<RunNetworkFacts>,
    holder_pid: Option<u32>,
    holder_start_time_ticks: Option<u64>,
    holder_exit_observed_at: Option<DateTime<Utc>>,
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub(crate) enum NativeResolverProjectionJournal {
    NotStarted,
    MountPending {
        pending: ResolverProjectionPending,
    },
    Mounted {
        pending: ResolverProjectionPending,
        mounted: ResolverProjectionMounted,
    },
    CleanupPending {
        pending: ResolverProjectionPending,
        mounted: Option<ResolverProjectionMounted>,
    },
    CleanupComplete,
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeResolverJournal {
    facts: crate::core::RunResolverFacts,
    source: ResolverSourceCheckpoint,
    primary: NativeResolverProjectionJournal,
    managed_service: Option<NativeResolverProjectionJournal>,
}

#[cfg(target_os = "linux")]
impl NativeResolverJournal {
    pub(crate) fn facts(&self) -> &crate::core::RunResolverFacts {
        &self.facts
    }

    pub(crate) fn source(&self) -> &ResolverSourceCheckpoint {
        &self.source
    }

    pub(crate) fn projection(
        &self,
        participant: NativeParticipant,
    ) -> Result<&NativeResolverProjectionJournal> {
        match participant {
            NativeParticipant::Primary => Ok(&self.primary),
            NativeParticipant::ManagedService => self
                .managed_service
                .as_ref()
                .context("native resolver has no Managed Service projection"),
        }
    }
}

impl NativeSharedNetworkJournal {
    #[must_use]
    pub(crate) const fn phase(&self) -> NativeSharedNetworkPhase {
        self.phase
    }

    #[must_use]
    pub(crate) const fn plan(&self) -> Option<&RunNetworkPlan> {
        self.plan.as_ref()
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) const fn facts(&self) -> Option<&RunNetworkFacts> {
        self.facts.as_ref()
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) const fn holder_pid(&self) -> Option<u32> {
        self.holder_pid
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) const fn holder_start_time_ticks(&self) -> Option<u64> {
        self.holder_start_time_ticks
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) const fn holder_exit_observed_at(&self) -> Option<DateTime<Utc>> {
        self.holder_exit_observed_at
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeManagedServiceJournal {
    phase: NativeRecoveryPhase,
    runtime_id: String,
    readiness: Option<ManagedServiceReadiness>,
    process: Option<ProcessSlot>,
    stdout: Option<StoredBytes>,
    stderr: Option<StoredBytes>,
    captured_at: Option<DateTime<Utc>>,
    final_image: Option<ImageSlot>,
    operation_errors: Vec<OperationError>,
}

impl NativeManagedServiceJournal {
    #[must_use]
    pub(crate) const fn phase(&self) -> NativeRecoveryPhase {
        self.phase
    }

    #[must_use]
    pub(crate) fn readiness(&self) -> Option<&ManagedServiceReadiness> {
        self.readiness.as_ref()
    }

    #[must_use]
    pub(crate) fn process(&self) -> Option<&ProcessSlot> {
        self.process.as_ref()
    }

    #[must_use]
    pub(crate) fn stdout(&self) -> Option<&StoredBytes> {
        self.stdout.as_ref()
    }

    #[must_use]
    pub(crate) fn stderr(&self) -> Option<&StoredBytes> {
        self.stderr.as_ref()
    }

    #[must_use]
    pub(crate) const fn captured_at(&self) -> Option<DateTime<Utc>> {
        self.captured_at
    }

    #[must_use]
    pub(crate) fn final_image(&self) -> Option<&ImageSlot> {
        self.final_image.as_ref()
    }

    #[must_use]
    pub(crate) fn operation_errors(&self) -> &[OperationError] {
        &self.operation_errors
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeRecoveryJournal {
    schema_version: u32,
    generation: u64,
    run_id: RunId,
    phase: NativeRecoveryPhase,
    backend: BackendFacts,
    runtime_id: String,
    process: Option<ProcessSlot>,
    stdout: Option<StoredBytes>,
    stderr: Option<StoredBytes>,
    captured_at: Option<DateTime<Utc>>,
    final_image: Option<ImageSlot>,
    terminal_at: Option<DateTime<Utc>>,
    operation_errors: Vec<OperationError>,
    managed_service: Option<NativeManagedServiceJournal>,
    shared_network: Option<NativeSharedNetworkJournal>,
    #[cfg(target_os = "linux")]
    resolver: Option<NativeResolverJournal>,
}

impl NativeRecoveryJournal {
    #[must_use]
    pub(crate) const fn phase(&self) -> NativeRecoveryPhase {
        self.phase
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) const fn generation(&self) -> u64 {
        self.generation
    }

    #[must_use]
    pub(crate) const fn run_id(&self) -> RunId {
        self.run_id
    }

    #[must_use]
    pub(crate) fn backend(&self) -> &BackendFacts {
        &self.backend
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) fn runtime_id(&self) -> &str {
        &self.runtime_id
    }

    #[must_use]
    pub(crate) fn process(&self) -> Option<&ProcessSlot> {
        self.process.as_ref()
    }

    #[must_use]
    pub(crate) fn stdout(&self) -> Option<&StoredBytes> {
        self.stdout.as_ref()
    }

    #[must_use]
    pub(crate) fn stderr(&self) -> Option<&StoredBytes> {
        self.stderr.as_ref()
    }

    #[must_use]
    pub(crate) const fn captured_at(&self) -> Option<DateTime<Utc>> {
        self.captured_at
    }

    #[must_use]
    pub(crate) fn final_image(&self) -> Option<&ImageSlot> {
        self.final_image.as_ref()
    }

    #[must_use]
    pub(crate) const fn terminal_at(&self) -> Option<DateTime<Utc>> {
        self.terminal_at
    }

    #[must_use]
    pub(crate) fn operation_errors(&self) -> &[OperationError] {
        &self.operation_errors
    }

    #[must_use]
    pub(crate) const fn managed_service(&self) -> Option<&NativeManagedServiceJournal> {
        self.managed_service.as_ref()
    }

    #[must_use]
    pub(crate) const fn shared_network(&self) -> Option<&NativeSharedNetworkJournal> {
        self.shared_network.as_ref()
    }

    #[cfg(target_os = "linux")]
    pub(crate) const fn resolver(&self) -> Option<&NativeResolverJournal> {
        self.resolver.as_ref()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct NativeRecoveryStore {
    root: PathBuf,
}

#[derive(Debug)]
pub(crate) struct NativeAttemptIdPage {
    pub ids: Vec<RunId>,
    pub has_more: bool,
}

#[derive(Debug)]
pub(crate) enum NativeRecoveryEntry {
    Published(Box<NativeAttempt>),
    Staging(NativeStagingAttempt),
}

#[derive(Debug)]
pub(crate) struct NativeStagingAttempt {
    directory: PathBuf,
    run_id: RunId,
    _root_lock: File,
    _attempt_lock: Option<File>,
}

impl NativeRecoveryStore {
    pub(crate) fn open(state_root: &Path) -> Result<Self> {
        ensure_real_private_directory(state_root)?;
        let recovery = state_root.join("recovery");
        ensure_real_private_directory(&recovery)?;
        let root = recovery.join("native");
        ensure_real_private_directory(&root)?;
        Ok(Self { root })
    }

    pub(crate) fn open_existing(state_root: &Path) -> Result<Option<Self>> {
        validate_directory(state_root)?;
        let recovery = state_root.join("recovery");
        match fs::symlink_metadata(&recovery) {
            Ok(metadata) => {
                validate_directory_metadata(&recovery, &metadata)?;
                #[cfg(unix)]
                verify_mode(&recovery, 0o700)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("failed to inspect recovery directory"),
        }
        let root = recovery.join("native");
        match fs::symlink_metadata(&root) {
            Ok(metadata) => {
                validate_directory_metadata(&root, &metadata)?;
                #[cfg(unix)]
                verify_mode(&root, 0o700)?;
                Ok(Some(Self { root }))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).context("failed to inspect native recovery root"),
        }
    }

    pub(crate) fn list_attempt_ids(
        &self,
        after: Option<RunId>,
        limit: usize,
    ) -> Result<NativeAttemptIdPage> {
        if limit == 0 {
            bail!("native recovery attempt list limit must be positive");
        }
        let retained_limit = limit
            .checked_add(1)
            .context("native recovery attempt list limit overflow")?;
        let _root_lock = self.lock_root()?;
        let after = after.map(|run_id| run_id.to_string());
        let mut retained = BTreeMap::new();
        let mut discarded = false;
        let mut observed = 0_usize;
        for entry in fs::read_dir(&self.root).context("failed to list native recovery attempts")? {
            observed = observed
                .checked_add(1)
                .context("native recovery directory entry count overflow")?;
            if observed > MAX_RECOVERY_DIRECTORY_ENTRIES {
                bail!("native recovery directory exceeds {MAX_RECOVERY_DIRECTORY_ENTRIES} entries");
            }
            let entry = entry.context("failed to read native recovery directory entry")?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| anyhow::anyhow!("native recovery entry name is not UTF-8"))?;
            let (run_id, staging) = parse_recovery_entry_name(&name)
                .with_context(|| format!("unexpected native recovery entry: {name}"))?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)
                .with_context(|| format!("failed to inspect native recovery entry {name}"))?;
            validate_directory_metadata(&path, &metadata)?;
            #[cfg(unix)]
            verify_mode(&path, 0o700)?;
            let identity = run_id.to_string();
            let counterpart = if staging {
                self.root.join(&identity)
            } else {
                self.staging_path(run_id)
            };
            if path_present(&counterpart).with_context(|| {
                format!("failed to inspect native recovery counterpart for {run_id}")
            })? {
                bail!("native recovery has both staging and published attempts: {run_id}");
            }
            if after.as_ref().is_some_and(|after| identity >= *after) {
                continue;
            }
            retained.insert(identity, run_id);
            if retained.len() > retained_limit {
                retained.pop_first();
                discarded = true;
            }
        }
        let mut ids = retained
            .into_iter()
            .rev()
            .map(|(_, run_id)| run_id)
            .collect::<Vec<_>>();
        let has_more = discarded || ids.len() > limit;
        ids.truncate(limit);
        Ok(NativeAttemptIdPage { ids, has_more })
    }

    pub(crate) fn prepare(&self, run_id: RunId, backend: BackendFacts) -> Result<NativeAttempt> {
        self.prepare_inner(run_id, backend, false)
    }

    pub(crate) fn prepare_managed(
        &self,
        run_id: RunId,
        backend: BackendFacts,
    ) -> Result<NativeAttempt> {
        self.prepare_inner(run_id, backend, true)
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn reserve_network_plan(
        &self,
        attempt: &mut NativeAttempt,
        control: NetworkControl,
        egress_tools: Option<&EgressNetworkTools>,
        timeout: std::time::Duration,
    ) -> Result<RunNetworkPlanReservation> {
        if attempt.directory.parent() != Some(self.root.as_path()) {
            bail!("native recovery attempt belongs to a different store");
        }
        match control {
            NetworkControl::None => {
                let plan = RunNetworkPlan::loopback(attempt.journal.run_id);
                attempt.record_network_plan(plan.clone())?;
                Ok(RunNetworkPlanReservation {
                    plan,
                    host_lock: None,
                })
            }
            NetworkControl::Egress => {
                let tools = egress_tools.context("native egress tools are unavailable")?;
                let deadline = std::time::Instant::now()
                    .checked_add(timeout)
                    .context("native IPv4 egress allocation timeout is too large")?;
                let host_lock = acquire_host_network_lock(network_allocation_remaining(deadline)?)?;
                let routes = tools
                    .route_snapshot(network_allocation_remaining(deadline)?)
                    .context("failed to snapshot host routes for egress allocation")?;
                let count = RunNetworkPlan::egress_subnet_count();
                let start = initial_subnet_slot(attempt.journal.run_id, count)?;
                let mut selected = None;
                for offset in 0..count {
                    let slot = (start + offset) % count;
                    let plan = RunNetworkPlan::egress_ipv4(attempt.journal.run_id, slot)?;
                    if tools.subnet_is_available(
                        &plan,
                        &routes,
                        network_allocation_remaining(deadline)?,
                    )? {
                        selected = Some(plan);
                        break;
                    }
                }
                let plan = selected.context("native IPv4 egress subnet pool is exhausted")?;
                attempt.record_network_plan(plan.clone())?;
                Ok(RunNetworkPlanReservation {
                    plan,
                    host_lock: Some(host_lock),
                })
            }
        }
    }

    fn prepare_inner(
        &self,
        run_id: RunId,
        backend: BackendFacts,
        managed: bool,
    ) -> Result<NativeAttempt> {
        self.prepare_inner_after_precheck(run_id, backend, managed, |_| Ok(()))
    }

    fn prepare_inner_after_precheck(
        &self,
        run_id: RunId,
        backend: BackendFacts,
        managed: bool,
        after_precheck: impl FnOnce(&Path) -> Result<()>,
    ) -> Result<NativeAttempt> {
        validate_backend(&backend)?;
        if backend.run_network.is_some() {
            bail!("native recovery must checkpoint shared network facts after creation");
        }
        let root_lock = self.lock_root()?;
        let target = self.root.join(run_id.to_string());
        let staging = self.staging_path(run_id);
        match fs::symlink_metadata(&target) {
            Ok(_) => bail!("native recovery attempt already exists: {run_id}"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to inspect native recovery attempt {run_id}")
                });
            }
        }
        match fs::symlink_metadata(&staging) {
            Ok(_) => bail!("native recovery staging attempt already exists: {run_id}"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to inspect native recovery staging attempt {run_id}")
                });
            }
        }
        after_precheck(&staging)?;

        let journal = initial_journal(run_id, backend, managed);
        let mut staging_created = false;
        let prepared = (|| -> Result<File> {
            create_private_directory_entry(&staging)
                .context("failed to create native recovery staging directory")?;
            staging_created = true;
            set_private_directory(&staging)?;
            let lock = create_private_file(&staging.join("lock"))?;
            try_lock(&lock, run_id)?;
            ensure_real_private_directory(&staging.join("workspace"))?;
            create_private_file(&staging.join("stdout"))?.sync_all()?;
            create_private_file(&staging.join("stderr"))?.sync_all()?;
            if managed {
                ensure_real_private_directory(&staging.join("workspace/managed-service"))?;
                create_private_file(&staging.join("managed-service-stdout"))?.sync_all()?;
                create_private_file(&staging.join("managed-service-stderr"))?.sync_all()?;
            }

            write_initial_journal(&staging, &journal)?;
            sync_directory(&staging)?;
            publish_staging(&root_lock, run_id)?;
            Ok(lock)
        })();
        let lock = match prepared {
            Ok(lock) => lock,
            Err(error) if !staging_created => return Err(error),
            Err(error) => {
                return match cleanup_staging_directory(&staging) {
                    Ok(()) => Err(error),
                    Err(cleanup) => Err(anyhow::anyhow!(
                        "{error:#}; native recovery staging cleanup also failed: {cleanup:#}"
                    )),
                };
            }
        };
        sync_directory(&self.root)?;

        Ok(NativeAttempt {
            directory: target,
            _lock: lock,
            journal,
        })
    }

    pub(crate) fn open_entry(&self, run_id: RunId) -> Result<Option<NativeRecoveryEntry>> {
        let root_lock = self.lock_root()?;
        let published = self.root.join(run_id.to_string());
        let staging = self.staging_path(run_id);
        let published_exists = path_present(&published)
            .context("failed to inspect published native recovery attempt")?;
        let staging_exists =
            path_present(&staging).context("failed to inspect native recovery staging attempt")?;
        if published_exists && staging_exists {
            bail!("native recovery has both staging and published attempts: {run_id}");
        }
        if published_exists {
            let attempt = self
                .open_attempt(run_id)?
                .expect("published native recovery attempt was just observed");
            drop(root_lock);
            return Ok(Some(NativeRecoveryEntry::Published(Box::new(attempt))));
        }
        if !staging_exists {
            return Ok(None);
        }
        validate_staging_directory(&staging)?;
        let lock_path = staging.join("lock");
        let attempt_lock = match fs::symlink_metadata(&lock_path) {
            Ok(_) => {
                let lock = open_private_file(&lock_path)?;
                try_lock(&lock, run_id)?;
                Some(lock)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(error).context("failed to inspect native recovery staging lock");
            }
        };
        Ok(Some(NativeRecoveryEntry::Staging(NativeStagingAttempt {
            directory: staging,
            run_id,
            _root_lock: root_lock,
            _attempt_lock: attempt_lock,
        })))
    }

    pub(crate) fn open_attempt(&self, run_id: RunId) -> Result<Option<NativeAttempt>> {
        let directory = self.root.join(run_id.to_string());
        match fs::symlink_metadata(&directory) {
            Ok(metadata) => {
                validate_directory_metadata(&directory, &metadata)?;
                #[cfg(unix)]
                verify_mode(&directory, 0o700)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to inspect native recovery attempt {run_id}")
                });
            }
        }
        validate_recovery_workspace(&directory.join("workspace"))?;
        validate_regular_file(&directory.join("stdout"))?;
        validate_regular_file(&directory.join("stderr"))?;
        let lock = open_private_file(&directory.join("lock"))?;
        try_lock(&lock, run_id)?;
        let journal = read_journal(&directory, run_id)?;
        #[cfg(target_os = "linux")]
        if let Some(resolver) = journal.resolver.as_ref() {
            ResolverSourceFile::open_from_attempt(&directory, resolver.facts(), resolver.source())?;
        }
        if journal.managed_service.is_some() {
            validate_managed_workspace(&directory.join("workspace/managed-service"))?;
            validate_regular_file(&directory.join("managed-service-stdout"))?;
            validate_regular_file(&directory.join("managed-service-stderr"))?;
        }
        Ok(Some(NativeAttempt {
            directory,
            _lock: lock,
            journal,
        }))
    }

    fn lock_root(&self) -> Result<File> {
        validate_directory(&self.root)?;
        let root = File::open(&self.root).context("failed to open native recovery root")?;
        match root.try_lock() {
            Ok(()) => Ok(root),
            Err(TryLockError::WouldBlock) => bail!("native recovery root is active"),
            Err(TryLockError::Error(error)) => {
                Err(error).context("failed to lock native recovery root")
            }
        }
    }

    fn staging_path(&self, run_id: RunId) -> PathBuf {
        self.root.join(format!(".prepare-{run_id}"))
    }
}

fn initial_journal(run_id: RunId, backend: BackendFacts, managed: bool) -> NativeRecoveryJournal {
    let needs_run_network = managed || backend.network == NetworkControl::Egress;
    NativeRecoveryJournal {
        schema_version: JOURNAL_SCHEMA_VERSION,
        generation: 1,
        run_id,
        phase: NativeRecoveryPhase::PreAcceptance,
        runtime_id: runtime_id(run_id),
        backend,
        process: None,
        stdout: None,
        stderr: None,
        captured_at: None,
        final_image: None,
        terminal_at: None,
        operation_errors: Vec::new(),
        managed_service: managed.then(|| NativeManagedServiceJournal {
            phase: NativeRecoveryPhase::PreAcceptance,
            runtime_id: managed_runtime_id(run_id),
            readiness: None,
            process: None,
            stdout: None,
            stderr: None,
            captured_at: None,
            final_image: None,
            operation_errors: Vec::new(),
        }),
        shared_network: needs_run_network.then_some(NativeSharedNetworkJournal {
            phase: NativeSharedNetworkPhase::PlanPending,
            plan: None,
            facts: None,
            holder_pid: None,
            holder_start_time_ticks: None,
            holder_exit_observed_at: None,
        }),
        #[cfg(target_os = "linux")]
        resolver: None,
    }
}

impl NativeStagingAttempt {
    #[must_use]
    pub(crate) const fn run_id(&self) -> RunId {
        self.run_id
    }

    pub(crate) fn remove(self) -> Result<()> {
        cleanup_staging_directory(&self.directory)
    }
}

#[cfg(target_os = "linux")]
pub(crate) struct RunNetworkPlanReservation {
    plan: RunNetworkPlan,
    host_lock: Option<HostNetworkLock>,
}

#[cfg(target_os = "linux")]
impl RunNetworkPlanReservation {
    #[must_use]
    pub(crate) fn plan(&self) -> &RunNetworkPlan {
        &self.plan
    }

    #[must_use]
    pub(crate) fn host_lock(&self) -> Option<&HostNetworkLock> {
        self.host_lock.as_ref()
    }
}

#[cfg(target_os = "linux")]
fn network_allocation_remaining(deadline: std::time::Instant) -> Result<std::time::Duration> {
    deadline
        .checked_duration_since(std::time::Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .context("native IPv4 egress allocation timed out")
}

fn initial_subnet_slot(run_id: RunId, count: u16) -> Result<u16> {
    if count == 0 {
        bail!("native IPv4 egress subnet pool is empty");
    }
    let identity = run_id.to_string();
    let suffix = identity
        .rsplit('-')
        .next()
        .context("Run identity has no subnet allocation suffix")?;
    let tail = suffix
        .get(suffix.len().saturating_sub(4)..)
        .context("Run identity subnet allocation suffix is invalid")?;
    Ok(
        u16::from_str_radix(tail, 16)
            .context("Run identity subnet allocation suffix is invalid")?
            % count,
    )
}

#[derive(Debug)]
pub(crate) struct NativeAttempt {
    directory: PathBuf,
    _lock: File,
    journal: NativeRecoveryJournal,
}

pub(crate) struct TerminalCheckpoint<'a> {
    pub terminal_at: DateTime<Utc>,
    pub process: ProcessSlot,
    pub stdout: StoredBytes,
    pub stderr: StoredBytes,
    pub stdout_bytes: Option<&'a [u8]>,
    pub stderr_bytes: Option<&'a [u8]>,
    pub final_image: ImageSlot,
    pub operation_errors: Vec<OperationError>,
}

pub(crate) struct RecoveryCaptureCheckpoint<'a> {
    pub captured_at: DateTime<Utc>,
    pub process: ProcessSlot,
    pub stdout: StoredBytes,
    pub stderr: StoredBytes,
    pub stdout_bytes: Option<&'a [u8]>,
    pub stderr_bytes: Option<&'a [u8]>,
    pub operation_errors: Vec<OperationError>,
}

pub(crate) struct ManagedTerminalCheckpoint<'a> {
    pub readiness: ManagedServiceReadiness,
    pub process: ProcessSlot,
    pub stdout: StoredBytes,
    pub stderr: StoredBytes,
    pub stdout_bytes: Option<&'a [u8]>,
    pub stderr_bytes: Option<&'a [u8]>,
    pub final_image: ImageSlot,
    pub operation_errors: Vec<OperationError>,
}

#[derive(Debug, Clone)]
pub(crate) struct SharedNetworkCheckpoint {
    facts: RunNetworkFacts,
    holder_pid: u32,
    holder_start_time_ticks: u64,
}

impl SharedNetworkCheckpoint {
    pub(crate) fn from_resources(
        resources: &crate::native_network::RunNetworkResources,
        resolver: Option<crate::core::RunResolverFacts>,
    ) -> Result<Self> {
        Ok(Self {
            facts: resources
                .facts(resolver)
                .context("Run network facts are invalid")?,
            holder_pid: resources.holder_pid,
            holder_start_time_ticks: resources.holder_start_time_ticks,
        })
    }

    #[cfg(test)]
    pub(crate) const fn for_test(
        facts: RunNetworkFacts,
        holder_pid: u32,
        holder_start_time_ticks: u64,
    ) -> Self {
        Self {
            facts,
            holder_pid,
            holder_start_time_ticks,
        }
    }
}

impl NativeAttempt {
    #[cfg(test)]
    #[must_use]
    pub(crate) fn path(&self) -> &Path {
        &self.directory
    }

    #[must_use]
    pub(crate) fn workspace(&self) -> PathBuf {
        self.directory.join("workspace")
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn prepare_resolver(
        &mut self,
        config: &ResolverConfig,
    ) -> Result<ResolverSourceFile> {
        if self.journal.phase != NativeRecoveryPhase::PreAcceptance
            || self.journal.backend.network != NetworkControl::Egress
            || self.journal.resolver.is_some()
        {
            bail!("native resolver source cannot be prepared in the current attempt state");
        }
        let source = config.write_attempt_source(&self.directory)?;
        let facts = config.facts();
        let checkpoint = source.checkpoint().clone();
        let managed = self.journal.managed_service.is_some();
        self.update_journal(|journal| {
            journal.resolver = Some(NativeResolverJournal {
                facts,
                source: checkpoint,
                primary: NativeResolverProjectionJournal::NotStarted,
                managed_service: managed.then_some(NativeResolverProjectionJournal::NotStarted),
            });
        })?;
        Ok(source)
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn open_resolver_source(&self) -> Result<Option<ResolverSourceFile>> {
        let Some(resolver) = self.journal.resolver.as_ref() else {
            return Ok(None);
        };
        ResolverSourceFile::open_from_attempt(&self.directory, resolver.facts(), resolver.source())
            .map(Some)
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn begin_resolver_mount(
        &mut self,
        participant: NativeParticipant,
        pending: ResolverProjectionPending,
    ) -> Result<()> {
        if self.participant_phase(participant)? != NativeRecoveryPhase::OverlayMounted {
            bail!("native resolver projection requires a mounted participant OverlayFS");
        }
        if !matches!(
            self.resolver_projection(participant)?,
            NativeResolverProjectionJournal::NotStarted
        ) {
            bail!("native resolver projection mount already started");
        }
        self.update_resolver_projection(participant, |_| {
            NativeResolverProjectionJournal::MountPending { pending }
        })
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn record_resolver_mounted(
        &mut self,
        participant: NativeParticipant,
        mounted: ResolverProjectionMounted,
    ) -> Result<()> {
        let pending = match self.resolver_projection(participant)? {
            NativeResolverProjectionJournal::MountPending { pending } => pending.clone(),
            _ => bail!("native resolver projection mount is not pending"),
        };
        self.update_resolver_projection(participant, |_| NativeResolverProjectionJournal::Mounted {
            pending,
            mounted,
        })
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn begin_resolver_cleanup(&mut self, participant: NativeParticipant) -> Result<()> {
        let next = match self.resolver_projection(participant)? {
            NativeResolverProjectionJournal::NotStarted
            | NativeResolverProjectionJournal::CleanupPending { .. }
            | NativeResolverProjectionJournal::CleanupComplete => return Ok(()),
            NativeResolverProjectionJournal::MountPending { pending } => {
                NativeResolverProjectionJournal::CleanupPending {
                    pending: pending.clone(),
                    mounted: None,
                }
            }
            NativeResolverProjectionJournal::Mounted { pending, mounted } => {
                NativeResolverProjectionJournal::CleanupPending {
                    pending: pending.clone(),
                    mounted: Some(*mounted),
                }
            }
        };
        self.update_resolver_projection(participant, |_| next)
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn record_resolver_cleanup(&mut self, participant: NativeParticipant) -> Result<()> {
        if !matches!(
            self.resolver_projection(participant)?,
            NativeResolverProjectionJournal::CleanupPending { .. }
        ) {
            bail!("native resolver projection cleanup is not pending");
        }
        self.update_resolver_projection(participant, |_| {
            NativeResolverProjectionJournal::CleanupComplete
        })
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn resolver_projection(
        &self,
        participant: NativeParticipant,
    ) -> Result<&NativeResolverProjectionJournal> {
        self.journal
            .resolver
            .as_ref()
            .context("native attempt has no resolver")?
            .projection(participant)
    }

    #[cfg(target_os = "linux")]
    fn update_resolver_projection(
        &mut self,
        participant: NativeParticipant,
        update: impl FnOnce(&NativeResolverProjectionJournal) -> NativeResolverProjectionJournal,
    ) -> Result<()> {
        self.update_journal(|journal| {
            let resolver = journal
                .resolver
                .as_mut()
                .expect("native resolver journal was validated");
            match participant {
                NativeParticipant::Primary => resolver.primary = update(&resolver.primary),
                NativeParticipant::ManagedService => {
                    let state = resolver
                        .managed_service
                        .as_mut()
                        .expect("Managed Service resolver journal was validated");
                    *state = update(state);
                }
            }
        })
    }

    #[must_use]
    pub(crate) fn bundle_directory(&self) -> PathBuf {
        self.workspace().join("bundle")
    }

    #[must_use]
    pub(crate) fn stdout_path(&self) -> PathBuf {
        self.participant_stdout_path(NativeParticipant::Primary)
    }

    #[must_use]
    pub(crate) fn stderr_path(&self) -> PathBuf {
        self.participant_stderr_path(NativeParticipant::Primary)
    }

    pub(crate) fn participant_workspace(&self, participant: NativeParticipant) -> Result<PathBuf> {
        match participant {
            NativeParticipant::Primary => Ok(self.workspace()),
            NativeParticipant::ManagedService => {
                self.journal
                    .managed_service
                    .as_ref()
                    .context("native attempt has no Managed Service")?;
                Ok(self.workspace().join("managed-service"))
            }
        }
    }

    pub(crate) fn participant_lower_workspace(
        &self,
        participant: NativeParticipant,
    ) -> Result<PathBuf> {
        Ok(self.participant_workspace(participant)?.join("lower"))
    }

    pub(crate) fn participant_bundle_directory(
        &self,
        participant: NativeParticipant,
    ) -> Result<PathBuf> {
        Ok(self.participant_workspace(participant)?.join("bundle"))
    }

    pub(crate) fn participant_overlay_workspace(
        &self,
        participant: NativeParticipant,
    ) -> Result<PathBuf> {
        Ok(self.participant_workspace(participant)?.join("overlay"))
    }

    pub(crate) fn participant_runtime_root(
        &self,
        participant: NativeParticipant,
    ) -> Result<PathBuf> {
        Ok(self.participant_workspace(participant)?.join("runtime"))
    }

    pub(crate) fn participant_cgroup_checkpoint(
        &self,
        participant: NativeParticipant,
    ) -> Result<PathBuf> {
        Ok(self.participant_workspace(participant)?.join("cgroup.json"))
    }

    #[must_use]
    pub(crate) fn participant_stdout_path(&self, participant: NativeParticipant) -> PathBuf {
        match participant {
            NativeParticipant::Primary => self.directory.join("stdout"),
            NativeParticipant::ManagedService => self.directory.join("managed-service-stdout"),
        }
    }

    #[must_use]
    pub(crate) fn participant_stderr_path(&self, participant: NativeParticipant) -> PathBuf {
        match participant {
            NativeParticipant::Primary => self.directory.join("stderr"),
            NativeParticipant::ManagedService => self.directory.join("managed-service-stderr"),
        }
    }

    pub(crate) fn participant_runtime_id(&self, participant: NativeParticipant) -> Result<&str> {
        match participant {
            NativeParticipant::Primary => Ok(&self.journal.runtime_id),
            NativeParticipant::ManagedService => Ok(&self
                .journal
                .managed_service
                .as_ref()
                .context("native attempt has no Managed Service")?
                .runtime_id),
        }
    }

    #[must_use]
    pub(crate) const fn journal(&self) -> &NativeRecoveryJournal {
        &self.journal
    }

    pub(crate) fn record_network_plan(&mut self, plan: RunNetworkPlan) -> Result<()> {
        if self.journal.phase != NativeRecoveryPhase::Accepted
            || self
                .journal
                .managed_service
                .as_ref()
                .is_some_and(|service| service.phase != NativeRecoveryPhase::Accepted)
        {
            bail!("Run network planning requires every participant to be accepted");
        }
        plan.validate().context("Run network plan is invalid")?;
        if plan.run_id() != self.journal.run_id {
            bail!("Run network plan identity differs from its recovery attempt");
        }
        validate_network_plan_mode(
            plan.mode(),
            self.journal.backend.network,
            self.journal.managed_service.is_some(),
        )?;
        let network = self
            .journal
            .shared_network
            .as_ref()
            .context("native attempt has no Run network")?;
        if network.phase != NativeSharedNetworkPhase::PlanPending {
            bail!("Run network planning is not pending");
        }
        self.update_journal(|journal| {
            let network = journal
                .shared_network
                .as_mut()
                .expect("Run network was validated");
            network.phase = NativeSharedNetworkPhase::CreatePending;
            network.plan = Some(plan);
        })
    }

    pub(crate) fn advance_phase(&mut self, next: NativeRecoveryPhase) -> Result<()> {
        self.advance_participant_phase(NativeParticipant::Primary, next)
    }

    pub(crate) fn advance_participant_phase(
        &mut self,
        participant: NativeParticipant,
        next: NativeRecoveryPhase,
    ) -> Result<()> {
        let current = self.participant_phase(participant)?;
        if next < current {
            bail!("native recovery phase cannot move backward from {current:?} to {next:?}");
        }
        if next == current {
            return Ok(());
        }
        #[cfg(target_os = "linux")]
        if next == NativeRecoveryPhase::RuntimeStartPending
            && let Some(resolver) = self.journal.resolver.as_ref()
            && !matches!(
                resolver.projection(participant)?,
                NativeResolverProjectionJournal::Mounted { .. }
            )
        {
            let name = match participant {
                NativeParticipant::Primary => "primary",
                NativeParticipant::ManagedService => "Managed Service",
            };
            bail!("{name} runtime requires an active resolver projection");
        }
        self.update_journal(|journal| match participant {
            NativeParticipant::Primary => journal.phase = next,
            NativeParticipant::ManagedService => {
                journal
                    .managed_service
                    .as_mut()
                    .expect("Managed Service participant was validated")
                    .phase = next;
            }
        })
    }

    pub(crate) fn participant_phase(
        &self,
        participant: NativeParticipant,
    ) -> Result<NativeRecoveryPhase> {
        match participant {
            NativeParticipant::Primary => Ok(self.journal.phase),
            NativeParticipant::ManagedService => Ok(self
                .journal
                .managed_service
                .as_ref()
                .context("native attempt has no Managed Service")?
                .phase),
        }
    }

    #[cfg(test)]
    pub(crate) fn record_process(
        &mut self,
        process: ProcessSlot,
        stdout: StoredBytes,
        stderr: StoredBytes,
        stdout_bytes: Option<&[u8]>,
        stderr_bytes: Option<&[u8]>,
        operation_errors: Vec<OperationError>,
    ) -> Result<()> {
        self.record_participant_process(
            NativeParticipant::Primary,
            process,
            stdout,
            stderr,
            stdout_bytes,
            stderr_bytes,
            operation_errors,
        )
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the participant checkpoint keeps process, stream slots, exact bytes, and errors atomic"
    )]
    pub(crate) fn record_participant_process(
        &mut self,
        participant: NativeParticipant,
        process: ProcessSlot,
        stdout: StoredBytes,
        stderr: StoredBytes,
        stdout_bytes: Option<&[u8]>,
        stderr_bytes: Option<&[u8]>,
        operation_errors: Vec<OperationError>,
    ) -> Result<()> {
        self.participant_phase(participant)?;
        if !matches!(process, ProcessSlot::Available { .. }) {
            bail!("native execution cannot record unavailable process facts as observed");
        }
        validate_participant_errors(participant, &operation_errors)?;
        validate_sidecar_input(&stdout, stdout_bytes, "stdout")?;
        validate_sidecar_input(&stderr, stderr_bytes, "stderr")?;
        match self.participant_observation(participant)? {
            (None, None, None) => {}
            (Some(existing_process), Some(existing_stdout), Some(existing_stderr))
                if existing_process == &process
                    && existing_stdout == &stdout
                    && existing_stderr == &stderr
                    && self.participant_errors(participant)? == operation_errors =>
            {
                verify_sidecar(&self.participant_stdout_path(participant), stdout_bytes)?;
                verify_sidecar(&self.participant_stderr_path(participant), stderr_bytes)?;
                return Ok(());
            }
            (Some(_), Some(_), Some(_)) => {
                bail!("native recovery cannot replace observed participant facts")
            }
            _ => bail!("native recovery journal has an incomplete process observation"),
        }
        write_sidecar(&self.participant_stdout_path(participant), stdout_bytes)?;
        write_sidecar(&self.participant_stderr_path(participant), stderr_bytes)?;
        self.checkpoint_participant(
            participant,
            NativeRecoveryPhase::ProcessObserved,
            |journal| match participant {
                NativeParticipant::Primary => {
                    journal.process = Some(process);
                    journal.stdout = Some(stdout);
                    journal.stderr = Some(stderr);
                    journal.operation_errors = operation_errors;
                }
                NativeParticipant::ManagedService => {
                    let service = journal
                        .managed_service
                        .as_mut()
                        .expect("Managed Service participant was validated");
                    service.process = Some(process);
                    service.stdout = Some(stdout);
                    service.stderr = Some(stderr);
                    service.operation_errors = operation_errors;
                }
            },
        )
    }

    pub(crate) fn begin_participant_capture(
        &mut self,
        participant: NativeParticipant,
        captured_at: DateTime<Utc>,
    ) -> Result<()> {
        #[cfg(target_os = "linux")]
        self.ensure_resolver_ready_for_capture(participant)?;
        let existing = match participant {
            NativeParticipant::Primary => self.journal.captured_at,
            NativeParticipant::ManagedService => {
                self.journal
                    .managed_service
                    .as_ref()
                    .context("native attempt has no Managed Service")?
                    .captured_at
            }
        };
        if let Some(existing) = existing {
            if existing == captured_at {
                return Ok(());
            }
            bail!("native recovery cannot replace a capture timestamp");
        }
        self.checkpoint_participant(
            participant,
            NativeRecoveryPhase::CapturePending,
            |journal| match participant {
                NativeParticipant::Primary => journal.captured_at = Some(captured_at),
                NativeParticipant::ManagedService => {
                    journal
                        .managed_service
                        .as_mut()
                        .expect("Managed Service participant was validated")
                        .captured_at = Some(captured_at);
                }
            },
        )
    }

    pub(crate) fn begin_participant_recovery_capture(
        &mut self,
        participant: NativeParticipant,
        checkpoint: RecoveryCaptureCheckpoint<'_>,
    ) -> Result<()> {
        #[cfg(target_os = "linux")]
        self.ensure_resolver_ready_for_capture(participant)?;
        validate_participant_errors(participant, &checkpoint.operation_errors)?;
        validate_sidecar_input(&checkpoint.stdout, checkpoint.stdout_bytes, "stdout")?;
        validate_sidecar_input(&checkpoint.stderr, checkpoint.stderr_bytes, "stderr")?;
        let observed = self.participant_observation(participant)?;
        let captured_at = match participant {
            NativeParticipant::Primary => self.journal.captured_at,
            NativeParticipant::ManagedService => {
                self.journal
                    .managed_service
                    .as_ref()
                    .context("native attempt has no Managed Service")?
                    .captured_at
            }
        };
        if let Some(captured_at) = captured_at {
            let unchanged = observed.0 == Some(&checkpoint.process)
                && observed.1 == Some(&checkpoint.stdout)
                && observed.2 == Some(&checkpoint.stderr)
                && captured_at == checkpoint.captured_at
                && self.participant_errors(participant)? == checkpoint.operation_errors;
            if !unchanged {
                bail!("native recovery cannot replace a recovery capture checkpoint");
            }
            verify_sidecar(
                &self.participant_stdout_path(participant),
                checkpoint.stdout_bytes,
            )?;
            verify_sidecar(
                &self.participant_stderr_path(participant),
                checkpoint.stderr_bytes,
            )?;
            return Ok(());
        }
        match observed {
            (None, None, None) => {
                write_sidecar(
                    &self.participant_stdout_path(participant),
                    checkpoint.stdout_bytes,
                )?;
                write_sidecar(
                    &self.participant_stderr_path(participant),
                    checkpoint.stderr_bytes,
                )?;
            }
            (Some(process), Some(stdout), Some(stderr))
                if process == &checkpoint.process
                    && stdout == &checkpoint.stdout
                    && stderr == &checkpoint.stderr =>
            {
                verify_sidecar(
                    &self.participant_stdout_path(participant),
                    checkpoint.stdout_bytes,
                )?;
                verify_sidecar(
                    &self.participant_stderr_path(participant),
                    checkpoint.stderr_bytes,
                )?;
            }
            (Some(_), Some(_), Some(_)) => {
                bail!("native recovery capture cannot replace observed process or stream facts")
            }
            _ => bail!("native recovery journal has an incomplete process observation"),
        }
        self.checkpoint_participant(
            participant,
            NativeRecoveryPhase::CapturePending,
            |journal| match participant {
                NativeParticipant::Primary => {
                    journal.process = Some(checkpoint.process);
                    journal.stdout = Some(checkpoint.stdout);
                    journal.stderr = Some(checkpoint.stderr);
                    journal.captured_at = Some(checkpoint.captured_at);
                    journal.operation_errors = checkpoint.operation_errors;
                }
                NativeParticipant::ManagedService => {
                    let service = journal
                        .managed_service
                        .as_mut()
                        .expect("Managed Service participant was validated");
                    service.process = Some(checkpoint.process);
                    service.stdout = Some(checkpoint.stdout);
                    service.stderr = Some(checkpoint.stderr);
                    service.captured_at = Some(checkpoint.captured_at);
                    service.operation_errors = checkpoint.operation_errors;
                }
            },
        )
    }

    pub(crate) fn record_participant_final(
        &mut self,
        participant: NativeParticipant,
        final_image: ImageSlot,
    ) -> Result<()> {
        let existing = match participant {
            NativeParticipant::Primary => self.journal.final_image.as_ref(),
            NativeParticipant::ManagedService => self
                .journal
                .managed_service
                .as_ref()
                .context("native attempt has no Managed Service")?
                .final_image
                .as_ref(),
        };
        if let Some(existing) = existing {
            if existing == &final_image {
                return Ok(());
            }
            bail!("native recovery cannot replace Final Image facts");
        }
        self.checkpoint_participant(
            participant,
            NativeRecoveryPhase::FinalPublished,
            |journal| match participant {
                NativeParticipant::Primary => journal.final_image = Some(final_image),
                NativeParticipant::ManagedService => {
                    journal
                        .managed_service
                        .as_mut()
                        .expect("Managed Service participant was validated")
                        .final_image = Some(final_image);
                }
            },
        )
    }

    pub(crate) fn prepare_terminal(&mut self, checkpoint: TerminalCheckpoint<'_>) -> Result<()> {
        validate_participant_errors(NativeParticipant::Primary, &checkpoint.operation_errors)?;
        validate_sidecar_input(&checkpoint.stdout, checkpoint.stdout_bytes, "stdout")?;
        validate_sidecar_input(&checkpoint.stderr, checkpoint.stderr_bytes, "stderr")?;
        if self.journal.phase == NativeRecoveryPhase::TerminalPrepared {
            let unchanged = self.journal.terminal_at == Some(checkpoint.terminal_at)
                && self.journal.process.as_ref() == Some(&checkpoint.process)
                && self.journal.stdout.as_ref() == Some(&checkpoint.stdout)
                && self.journal.stderr.as_ref() == Some(&checkpoint.stderr)
                && self.journal.final_image.as_ref() == Some(&checkpoint.final_image)
                && self.journal.operation_errors == checkpoint.operation_errors;
            if !unchanged {
                bail!("native recovery cannot replace a terminal checkpoint");
            }
            verify_sidecar(&self.stdout_path(), checkpoint.stdout_bytes)?;
            verify_sidecar(&self.stderr_path(), checkpoint.stderr_bytes)?;
            return Ok(());
        }
        let observed = (
            self.journal.process.as_ref(),
            self.journal.stdout.as_ref(),
            self.journal.stderr.as_ref(),
        );
        match observed {
            (None, None, None) => {
                write_sidecar(&self.stdout_path(), checkpoint.stdout_bytes)?;
                write_sidecar(&self.stderr_path(), checkpoint.stderr_bytes)?;
            }
            (Some(process), Some(stdout), Some(stderr))
                if process == &checkpoint.process
                    && stdout == &checkpoint.stdout
                    && stderr == &checkpoint.stderr =>
            {
                verify_sidecar(&self.stdout_path(), checkpoint.stdout_bytes)?;
                verify_sidecar(&self.stderr_path(), checkpoint.stderr_bytes)?;
            }
            (Some(_), Some(_), Some(_)) => {
                bail!("native terminal checkpoint cannot replace observed process or stream facts")
            }
            _ => bail!("native recovery journal has an incomplete process observation"),
        }
        self.checkpoint(NativeRecoveryPhase::TerminalPrepared, |journal| {
            journal.terminal_at = Some(checkpoint.terminal_at);
            journal.process = Some(checkpoint.process);
            journal.stdout = Some(checkpoint.stdout);
            journal.stderr = Some(checkpoint.stderr);
            journal.final_image = Some(checkpoint.final_image);
            journal.operation_errors = checkpoint.operation_errors;
        })
    }

    pub(crate) fn record_managed_readiness(
        &mut self,
        readiness: ManagedServiceReadiness,
    ) -> Result<()> {
        readiness.validate()?;
        let service = self
            .journal
            .managed_service
            .as_ref()
            .context("native attempt has no Managed Service")?;
        if let Some(existing) = &service.readiness {
            if existing == &readiness {
                return Ok(());
            }
            bail!("native recovery cannot replace Managed Service readiness facts");
        }
        self.update_journal(|journal| {
            journal
                .managed_service
                .as_mut()
                .expect("Managed Service participant was validated")
                .readiness = Some(readiness);
        })
    }

    pub(crate) fn prepare_managed_terminal(
        &mut self,
        checkpoint: ManagedTerminalCheckpoint<'_>,
    ) -> Result<()> {
        checkpoint.readiness.validate()?;
        validate_participant_errors(
            NativeParticipant::ManagedService,
            &checkpoint.operation_errors,
        )?;
        validate_sidecar_input(&checkpoint.stdout, checkpoint.stdout_bytes, "stdout")?;
        validate_sidecar_input(&checkpoint.stderr, checkpoint.stderr_bytes, "stderr")?;
        let service = self
            .journal
            .managed_service
            .as_ref()
            .context("native attempt has no Managed Service")?;
        if service.phase == NativeRecoveryPhase::TerminalPrepared {
            let unchanged = service.readiness.as_ref() == Some(&checkpoint.readiness)
                && service.process.as_ref() == Some(&checkpoint.process)
                && service.stdout.as_ref() == Some(&checkpoint.stdout)
                && service.stderr.as_ref() == Some(&checkpoint.stderr)
                && service.final_image.as_ref() == Some(&checkpoint.final_image)
                && service.operation_errors == checkpoint.operation_errors;
            if !unchanged {
                bail!("native recovery cannot replace a Managed Service terminal checkpoint");
            }
            verify_sidecar(
                &self.participant_stdout_path(NativeParticipant::ManagedService),
                checkpoint.stdout_bytes,
            )?;
            verify_sidecar(
                &self.participant_stderr_path(NativeParticipant::ManagedService),
                checkpoint.stderr_bytes,
            )?;
            return Ok(());
        }
        if let Some(readiness) = &service.readiness
            && readiness != &checkpoint.readiness
        {
            bail!("native terminal checkpoint cannot replace Managed Service readiness facts");
        }
        let observed = self.participant_observation(NativeParticipant::ManagedService)?;
        match observed {
            (None, None, None) => {
                write_sidecar(
                    &self.participant_stdout_path(NativeParticipant::ManagedService),
                    checkpoint.stdout_bytes,
                )?;
                write_sidecar(
                    &self.participant_stderr_path(NativeParticipant::ManagedService),
                    checkpoint.stderr_bytes,
                )?;
            }
            (Some(process), Some(stdout), Some(stderr))
                if process == &checkpoint.process
                    && stdout == &checkpoint.stdout
                    && stderr == &checkpoint.stderr =>
            {
                verify_sidecar(
                    &self.participant_stdout_path(NativeParticipant::ManagedService),
                    checkpoint.stdout_bytes,
                )?;
                verify_sidecar(
                    &self.participant_stderr_path(NativeParticipant::ManagedService),
                    checkpoint.stderr_bytes,
                )?;
            }
            (Some(_), Some(_), Some(_)) => {
                bail!("native terminal checkpoint cannot replace Managed Service process facts")
            }
            _ => bail!("native recovery journal has an incomplete Managed Service observation"),
        }
        self.checkpoint_participant(
            NativeParticipant::ManagedService,
            NativeRecoveryPhase::TerminalPrepared,
            |journal| {
                let service = journal
                    .managed_service
                    .as_mut()
                    .expect("Managed Service participant was validated");
                service.readiness = Some(checkpoint.readiness);
                service.process = Some(checkpoint.process);
                service.stdout = Some(checkpoint.stdout);
                service.stderr = Some(checkpoint.stderr);
                service.final_image = Some(checkpoint.final_image);
                service.operation_errors = checkpoint.operation_errors;
            },
        )
    }

    pub(crate) fn record_shared_network(
        &mut self,
        checkpoint: SharedNetworkCheckpoint,
    ) -> Result<()> {
        checkpoint.facts.validate(self.journal.backend.network)?;
        if checkpoint.holder_pid == 0 || checkpoint.holder_start_time_ticks == 0 {
            bail!("shared network holder identity must be positive");
        }
        let network = self
            .journal
            .shared_network
            .as_ref()
            .context("native attempt has no shared network")?;
        if network.phase != NativeSharedNetworkPhase::CreatePending {
            bail!("shared network creation is not pending");
        }
        let plan = network
            .plan
            .as_ref()
            .context("shared network creation has no durable plan")?;
        validate_network_plan_mode(
            plan.mode(),
            self.journal.backend.network,
            self.journal.managed_service.is_some(),
        )?;
        validate_network_facts_for_plan(plan, &checkpoint.facts)?;
        self.update_journal(|journal| {
            journal.backend.run_network = Some(checkpoint.facts.clone());
            let network = journal
                .shared_network
                .as_mut()
                .expect("shared network was validated");
            network.phase = NativeSharedNetworkPhase::Active;
            network.facts = Some(checkpoint.facts);
            network.holder_pid = Some(checkpoint.holder_pid);
            network.holder_start_time_ticks = Some(checkpoint.holder_start_time_ticks);
        })
    }

    pub(crate) fn begin_shared_network_cleanup(&mut self) -> Result<()> {
        let network = self
            .journal
            .shared_network
            .as_ref()
            .context("native attempt has no shared network")?;
        if network.phase == NativeSharedNetworkPhase::CleanupPending {
            return Ok(());
        }
        if !matches!(
            network.phase,
            NativeSharedNetworkPhase::PlanPending
                | NativeSharedNetworkPhase::CreatePending
                | NativeSharedNetworkPhase::Active
        ) {
            bail!("shared network cleanup cannot begin from its current phase");
        }
        self.update_journal(|journal| {
            journal
                .shared_network
                .as_mut()
                .expect("shared network was validated")
                .phase = NativeSharedNetworkPhase::CleanupPending;
        })
    }

    pub(crate) fn record_shared_network_cleanup(
        &mut self,
        observed_at: DateTime<Utc>,
    ) -> Result<()> {
        let network = self
            .journal
            .shared_network
            .as_ref()
            .context("native attempt has no shared network")?;
        if network.phase != NativeSharedNetworkPhase::CleanupPending {
            bail!("shared network cleanup is not pending");
        }
        self.update_journal(|journal| {
            let network = journal
                .shared_network
                .as_mut()
                .expect("shared network was validated");
            network.phase = NativeSharedNetworkPhase::CleanupComplete;
            network.holder_exit_observed_at = Some(observed_at);
        })
    }

    pub(crate) fn remove_after_terminal(self) -> Result<()> {
        if self.journal.phase != NativeRecoveryPhase::TerminalPrepared {
            bail!("native recovery attempt is not prepared for terminal removal");
        }
        if let Some(service) = &self.journal.managed_service
            && service.phase != NativeRecoveryPhase::TerminalPrepared
        {
            bail!("Managed Service recovery attempt is not prepared for terminal removal");
        }
        if let Some(network) = &self.journal.shared_network
            && network.phase != NativeSharedNetworkPhase::CleanupComplete
        {
            bail!("shared network recovery is not complete");
        }
        #[cfg(target_os = "linux")]
        if let Some(resolver) = &self.journal.resolver {
            ensure_resolver_projection_removable(&resolver.primary, "primary")?;
            if let Some(service) = &resolver.managed_service {
                ensure_resolver_projection_removable(service, "Managed Service")?;
            }
        }
        self.remove()
    }

    #[cfg(target_os = "linux")]
    fn ensure_resolver_ready_for_capture(&self, participant: NativeParticipant) -> Result<()> {
        let Some(resolver) = &self.journal.resolver else {
            return Ok(());
        };
        ensure_resolver_projection_removable(
            resolver.projection(participant)?,
            match participant {
                NativeParticipant::Primary => "primary",
                NativeParticipant::ManagedService => "Managed Service",
            },
        )
    }

    pub(crate) fn remove(self) -> Result<()> {
        #[cfg(target_os = "linux")]
        crate::native_fs::ensure_no_mounts_at_or_below(&self.directory)?;
        let parent = self
            .directory
            .parent()
            .context("native recovery attempt has no parent")?
            .to_path_buf();
        let directory = self.directory.clone();
        drop(self);
        fs::remove_dir_all(&directory).with_context(|| {
            format!(
                "failed to remove terminal native recovery attempt {}",
                directory.display()
            )
        })?;
        sync_directory(&parent)
    }

    fn checkpoint(
        &mut self,
        next: NativeRecoveryPhase,
        update: impl FnOnce(&mut NativeRecoveryJournal),
    ) -> Result<()> {
        self.checkpoint_participant(NativeParticipant::Primary, next, update)
    }

    fn checkpoint_participant(
        &mut self,
        participant: NativeParticipant,
        next: NativeRecoveryPhase,
        update: impl FnOnce(&mut NativeRecoveryJournal),
    ) -> Result<()> {
        let current = self.participant_phase(participant)?;
        if next < current {
            bail!("native recovery phase cannot move backward from {current:?} to {next:?}");
        }
        self.update_journal(|journal| {
            update(journal);
            match participant {
                NativeParticipant::Primary => journal.phase = next,
                NativeParticipant::ManagedService => {
                    journal
                        .managed_service
                        .as_mut()
                        .expect("Managed Service participant was validated")
                        .phase = next;
                }
            }
        })
    }

    fn update_journal(&mut self, update: impl FnOnce(&mut NativeRecoveryJournal)) -> Result<()> {
        let mut journal = self.journal.clone();
        update(&mut journal);
        journal.generation = journal
            .generation
            .checked_add(1)
            .context("native recovery journal generation overflow")?;
        write_journal(&self.directory, &journal)?;
        self.journal = journal;
        Ok(())
    }

    fn participant_observation(
        &self,
        participant: NativeParticipant,
    ) -> Result<(
        Option<&ProcessSlot>,
        Option<&StoredBytes>,
        Option<&StoredBytes>,
    )> {
        match participant {
            NativeParticipant::Primary => Ok((
                self.journal.process.as_ref(),
                self.journal.stdout.as_ref(),
                self.journal.stderr.as_ref(),
            )),
            NativeParticipant::ManagedService => {
                let service = self
                    .journal
                    .managed_service
                    .as_ref()
                    .context("native attempt has no Managed Service")?;
                Ok((
                    service.process.as_ref(),
                    service.stdout.as_ref(),
                    service.stderr.as_ref(),
                ))
            }
        }
    }

    fn participant_errors(&self, participant: NativeParticipant) -> Result<&[OperationError]> {
        match participant {
            NativeParticipant::Primary => Ok(&self.journal.operation_errors),
            NativeParticipant::ManagedService => Ok(&self
                .journal
                .managed_service
                .as_ref()
                .context("native attempt has no Managed Service")?
                .operation_errors),
        }
    }
}

#[cfg(target_os = "linux")]
fn ensure_resolver_projection_removable(
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

fn validate_sidecar_input(slot: &StoredBytes, bytes: Option<&[u8]>, name: &str) -> Result<()> {
    match (slot, bytes) {
        (
            StoredBytes::Available { digest, size } | StoredBytes::Partial { digest, size, .. },
            Some(bytes),
        ) if *digest == digest_bytes(bytes) && *size == bytes.len() as u64 => Ok(()),
        (StoredBytes::Unavailable { .. } | StoredBytes::NotApplicable, None) => Ok(()),
        _ => bail!("native recovery {name} bytes do not match their durable slot"),
    }
}

fn verify_sidecar(path: &Path, expected: Option<&[u8]>) -> Result<()> {
    validate_regular_file(path)?;
    let actual = fs::read(path)
        .with_context(|| format!("failed to read native sidecar {}", path.display()))?;
    if actual.as_slice() != expected.unwrap_or_default() {
        bail!("native recovery sidecar differs from its durable observation");
    }
    Ok(())
}

fn write_sidecar(path: &Path, bytes: Option<&[u8]>) -> Result<()> {
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

fn validate_backend(backend: &BackendFacts) -> Result<()> {
    if backend.name != "native_linux" || backend.version.is_empty() {
        bail!("native recovery backend identity is incomplete");
    }
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
    if let Some(network) = &backend.run_network {
        network.validate(backend.network)?;
    }
    Ok(())
}

fn runtime_id(run_id: RunId) -> String {
    let value = run_id.to_string();
    format!(
        "runlab-{}",
        value
            .strip_prefix("run-")
            .expect("Run identity always has the run- prefix")
    )
}

fn managed_runtime_id(run_id: RunId) -> String {
    format!("{}-service", runtime_id(run_id))
}

fn parse_recovery_entry_name(name: &str) -> Result<(RunId, bool)> {
    match name.strip_prefix(".prepare-") {
        Some(identity) => Ok((RunId::parse(identity)?, true)),
        None => Ok((RunId::parse(name)?, false)),
    }
}

fn publish_staging(root: &File, run_id: RunId) -> Result<()> {
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

fn write_initial_journal(directory: &Path, journal: &NativeRecoveryJournal) -> Result<()> {
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

fn write_journal(directory: &Path, journal: &NativeRecoveryJournal) -> Result<()> {
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

fn read_journal(directory: &Path, run_id: RunId) -> Result<NativeRecoveryJournal> {
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

fn validate_journal(journal: &NativeRecoveryJournal, expected_run_id: RunId) -> Result<()> {
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
    #[cfg(target_os = "linux")]
    validate_resolver_journal(journal)?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn validate_resolver_journal(journal: &NativeRecoveryJournal) -> Result<()> {
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
            let bytes = resolver.facts.canonical_bytes()?;
            if digest_bytes(&bytes) != resolver.facts.content_digest {
                bail!("native resolver facts have an invalid canonical digest");
            }
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

#[cfg(target_os = "linux")]
fn validate_resolver_projection(
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

fn validate_participant_state(
    name: &str,
    phase: NativeRecoveryPhase,
    process: Option<&ProcessSlot>,
    stdout: Option<&StoredBytes>,
    stderr: Option<&StoredBytes>,
    captured_at: Option<DateTime<Utc>>,
    final_image: Option<&ImageSlot>,
) -> Result<()> {
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

fn validate_shared_network(
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

fn validate_cleanup_network_fields(
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

fn validate_network_plan(network: &NativeSharedNetworkJournal) -> Result<()> {
    network
        .plan
        .as_ref()
        .context("shared network has no durable plan")?
        .validate()
        .context("Run network plan is invalid")
}

fn validate_network_plan_mode(
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

fn validate_active_network_fields(
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

fn validate_network_facts_for_plan(plan: &RunNetworkPlan, facts: &RunNetworkFacts) -> Result<()> {
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
            let resolver_bytes = resolver.canonical_bytes()?;
            if digest_bytes(&resolver_bytes) != resolver.content_digest {
                bail!("Run resolver digest differs from its canonical bytes");
            }
            Ok(())
        }
        _ => bail!("Run network realization differs from the durable network plan"),
    }
}

fn validate_participant_errors(
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

fn try_lock(file: &File, run_id: RunId) -> Result<()> {
    match file.try_lock() {
        Ok(()) => Ok(()),
        Err(TryLockError::WouldBlock) => bail!("native recovery attempt is active: {run_id}"),
        Err(TryLockError::Error(error)) => {
            Err(error).context("failed to lock native recovery attempt")
        }
    }
}

fn ensure_real_private_directory(path: &Path) -> Result<()> {
    let mut created = false;
    match fs::symlink_metadata(path) {
        Ok(metadata) => validate_directory_metadata(path, &metadata)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(path)
                .with_context(|| format!("failed to create directory {}", path.display()))?;
            created = true;
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to inspect directory {}", path.display()));
        }
    }
    set_private_directory(path)?;
    if created {
        sync_directory(path)?;
        if let Some(parent) = path.parent() {
            sync_directory(parent)?;
        }
    }
    Ok(())
}

#[cfg(test)]
fn create_private_directory(path: &Path) -> Result<()> {
    create_private_directory_entry(path)?;
    set_private_directory(path)
}

fn create_private_directory_entry(path: &Path) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;

        builder.mode(0o700);
    }
    builder
        .create(path)
        .with_context(|| format!("failed to create private directory {}", path.display()))
}

fn path_present(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("failed to inspect {}", path.display())),
    }
}

fn validate_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect directory {}", path.display()))?;
    validate_directory_metadata(path, &metadata)?;
    #[cfg(unix)]
    verify_mode(path, 0o700)?;
    Ok(())
}

fn validate_recovery_workspace(workspace: &Path) -> Result<()> {
    validate_workspace_layout(workspace)
}

fn validate_managed_workspace(workspace: &Path) -> Result<()> {
    validate_workspace_layout(workspace)
}

fn validate_workspace_layout(workspace: &Path) -> Result<()> {
    validate_directory(workspace)?;
    for path in [
        workspace.join("lower"),
        workspace.join("lower/rootfs"),
        workspace.join("bundle"),
        workspace.join("bundle/rootfs"),
        workspace.join("overlay"),
        workspace.join("overlay/upper"),
        workspace.join("overlay/work"),
        workspace.join("runtime"),
    ] {
        validate_optional_directory(&path)?;
    }
    Ok(())
}

fn validate_optional_directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => validate_directory_metadata(path, &metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("failed to inspect directory {}", path.display()))
        }
    }
}

fn validate_directory_metadata(path: &Path, metadata: &fs::Metadata) -> Result<()> {
    if metadata.file_type().is_symlink() {
        bail!(
            "native recovery path must not be a symbolic link: {}",
            path.display()
        );
    }
    if !metadata.is_dir() {
        bail!(
            "native recovery path must be a directory: {}",
            path.display()
        );
    }
    Ok(())
}

fn validate_staging_directory(directory: &Path) -> Result<()> {
    validate_directory(directory)?;
    validate_same_mount(
        directory
            .parent()
            .context("native recovery staging directory has no parent")?,
        directory,
    )?;
    let name = directory
        .file_name()
        .and_then(|name| name.to_str())
        .context("native recovery staging directory name is invalid")?;
    let (run_id, staging) = parse_recovery_entry_name(name)?;
    if !staging {
        bail!("native recovery staging directory has a published name");
    }
    for entry in fs::read_dir(directory).context("failed to inspect native recovery staging")? {
        let entry = entry.context("failed to read native recovery staging entry")?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("native recovery staging entry name is not UTF-8"))?;
        let path = entry.path();
        match name.as_str() {
            "workspace" => validate_staging_workspace(&path)?,
            "lock" | "stdout" | "stderr" | "managed-service-stdout" | "managed-service-stderr" => {
                validate_same_mount(directory, &path)?;
                validate_empty_staging_file(&path)?;
            }
            "journal.json" => {
                validate_same_mount(directory, &path)?;
                validate_staging_journal(&path, run_id)?;
            }
            _ => bail!("unexpected native recovery staging entry: {name}"),
        }
    }
    Ok(())
}

fn validate_staging_journal(path: &Path, run_id: RunId) -> Result<()> {
    validate_bounded_staging_file(path, MAX_JOURNAL_BYTES)?;
    let bytes = fs::read(path).context("failed to read native recovery staging journal")?;
    if serde_json::from_slice::<serde_json::Value>(&bytes).is_err() {
        return Ok(());
    }
    let journal: NativeRecoveryJournal = serde_json::from_slice(&bytes)
        .context("complete native recovery staging journal is invalid")?;
    validate_journal(&journal, run_id)?;
    validate_pristine_prepublication_journal(&journal)
}

fn validate_pristine_prepublication_journal(journal: &NativeRecoveryJournal) -> Result<()> {
    let primary_has_facts = journal.process.is_some()
        || journal.stdout.is_some()
        || journal.stderr.is_some()
        || journal.captured_at.is_some()
        || journal.final_image.is_some()
        || journal.terminal_at.is_some()
        || !journal.operation_errors.is_empty();
    let service_has_facts = journal.managed_service.as_ref().is_some_and(|service| {
        service.phase != NativeRecoveryPhase::PreAcceptance
            || service.readiness.is_some()
            || service.process.is_some()
            || service.stdout.is_some()
            || service.stderr.is_some()
            || service.captured_at.is_some()
            || service.final_image.is_some()
            || !service.operation_errors.is_empty()
    });
    let network_has_facts = journal.shared_network.as_ref().is_some_and(|network| {
        network.phase != NativeSharedNetworkPhase::PlanPending
            || network.plan.is_some()
            || network.facts.is_some()
            || network.holder_pid.is_some()
            || network.holder_start_time_ticks.is_some()
            || network.holder_exit_observed_at.is_some()
    });
    if journal.generation != 1
        || journal.phase != NativeRecoveryPhase::PreAcceptance
        || primary_has_facts
        || service_has_facts
        || network_has_facts
        || journal.backend.run_network.is_some()
    {
        bail!("native recovery staging journal contains published resource state");
    }
    Ok(())
}

fn validate_staging_workspace(workspace: &Path) -> Result<()> {
    validate_directory(workspace)?;
    validate_same_mount(
        workspace
            .parent()
            .context("native recovery staging workspace has no parent")?,
        workspace,
    )?;
    for entry in
        fs::read_dir(workspace).context("failed to inspect native recovery staging workspace")?
    {
        let entry = entry.context("failed to read native recovery staging workspace entry")?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("native recovery staging entry name is not UTF-8"))?;
        if name != "managed-service" {
            bail!("unexpected native recovery staging workspace entry: {name}");
        }
        let managed = entry.path();
        validate_directory(&managed)?;
        validate_same_mount(workspace, &managed)?;
        if fs::read_dir(&managed)
            .context("failed to inspect native recovery staging Managed Service workspace")?
            .next()
            .transpose()
            .context("failed to read native recovery staging Managed Service workspace")?
            .is_some()
        {
            bail!("native recovery staging Managed Service workspace is not empty");
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn validate_same_mount(parent: &Path, child: &Path) -> Result<()> {
    if mount_id(parent)? != mount_id(child)? {
        bail!(
            "native recovery staging crosses a mount boundary: {}",
            child.display()
        );
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn validate_same_mount(parent: &Path, child: &Path) -> Result<()> {
    fs::symlink_metadata(parent).context("failed to inspect staging parent")?;
    fs::symlink_metadata(child).context("failed to inspect staging child")?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn mount_id(path: &Path) -> Result<u64> {
    use rustix::fs::{AtFlags, CWD, StatxFlags, statx};

    let status = statx(CWD, path, AtFlags::SYMLINK_NOFOLLOW, StatxFlags::MNT_ID)
        .with_context(|| format!("failed to inspect mount identity for {}", path.display()))?;
    if status.stx_mask & StatxFlags::MNT_ID.bits() == 0 {
        bail!(
            "mount identity is unavailable for native recovery staging: {}",
            path.display()
        );
    }
    Ok(status.stx_mnt_id)
}

fn validate_empty_staging_file(path: &Path) -> Result<()> {
    validate_bounded_staging_file(path, 0)
}

fn validate_bounded_staging_file(path: &Path, maximum: u64) -> Result<()> {
    validate_regular_file(path)?;
    let metadata = fs::symlink_metadata(path).with_context(|| {
        format!(
            "failed to inspect native recovery staging file {}",
            path.display()
        )
    })?;
    if metadata.len() > maximum {
        bail!(
            "native recovery staging file exceeds {maximum} bytes: {}",
            path.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;

        if metadata.nlink() != 1 {
            bail!(
                "native recovery staging file has unexpected link count: {}",
                path.display()
            );
        }
    }
    Ok(())
}

fn cleanup_staging_directory(directory: &Path) -> Result<()> {
    match fs::symlink_metadata(directory) {
        Ok(_) => validate_staging_directory(directory)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).context("failed to inspect native recovery staging directory");
        }
    }
    for name in [
        "lock",
        "stdout",
        "stderr",
        "managed-service-stdout",
        "managed-service-stderr",
        "journal.json",
    ] {
        remove_optional_file(&directory.join(name))?;
    }
    remove_optional_empty_directory(&directory.join("workspace/managed-service"))?;
    remove_optional_empty_directory(&directory.join("workspace"))?;
    fs::remove_dir(directory).with_context(|| {
        format!(
            "failed to remove native recovery staging directory {}",
            directory.display()
        )
    })?;
    sync_directory(
        directory
            .parent()
            .context("native recovery staging directory has no parent")?,
    )
}

fn remove_optional_file(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("failed to remove staging file {}", path.display()))
        }
    }
}

fn remove_optional_empty_directory(path: &Path) -> Result<()> {
    match fs::remove_dir(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("failed to remove staging directory {}", path.display())),
    }
}

fn set_private_directory(path: &Path) -> Result<()> {
    ensure_private_directory(path)?;
    #[cfg(unix)]
    verify_mode(path, 0o700)?;
    Ok(())
}

fn create_private_file(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("failed to create private file {}", path.display()))?;
    set_private_file(&file)?;
    Ok(file)
}

fn open_private_file(path: &Path) -> Result<File> {
    validate_regular_file(path)?;
    #[cfg(unix)]
    let file = {
        use rustix::fs::{Mode, OFlags, open};

        let descriptor = open(
            path,
            OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .with_context(|| format!("failed to open private file {}", path.display()))?;
        File::from(descriptor)
    };
    #[cfg(not(unix))]
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("failed to open private file {}", path.display()))?;
    Ok(file)
}

fn validate_regular_file(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect private file {}", path.display()))?;
    if metadata.file_type().is_symlink() {
        bail!(
            "native recovery path must not be a symbolic link: {}",
            path.display()
        );
    }
    if !metadata.is_file() {
        bail!(
            "native recovery path must be a regular file: {}",
            path.display()
        );
    }
    #[cfg(unix)]
    verify_mode(path, 0o600)?;
    Ok(())
}

fn set_private_file(file: &File) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        file.set_permissions(fs::Permissions::from_mode(0o600))
            .context("failed to secure native recovery file")?;
    }
    Ok(())
}

#[cfg(unix)]
fn verify_mode(path: &Path, expected: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    let actual = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect permissions for {}", path.display()))?
        .permissions()
        .mode()
        & 0o777;
    if actual != expected {
        bail!(
            "native recovery path has mode {actual:o}, expected {expected:o}: {}",
            path.display()
        );
    }
    Ok(())
}

fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)
        .with_context(|| format!("failed to open directory {}", path.display()))?
        .sync_all()
        .with_context(|| format!("failed to fsync directory {}", path.display()))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    use super::*;
    use crate::core::{
        Architecture, Digest, NetworkControl, OciDescriptor, Platform, ProcessFacts,
        ProcessOutcome, RunResolverFacts, RunResolverSource,
    };
    use crate::integrity::digest_bytes;

    fn backend() -> BackendFacts {
        BackendFacts {
            name: "native_linux".to_owned(),
            version: "0.2.0-dev.0".to_owned(),
            platform: Platform::linux(Architecture::Arm64),
            network: NetworkControl::None,
            run_network: None,
            details: BackendDetails::NativeLinux {
                runtime_name: "runc".to_owned(),
                runtime_version: "1.3.6".to_owned(),
                runtime_commit: "fixture".to_owned(),
                runtime_spec: "1.2.1".to_owned(),
                kernel_release: "fixture".to_owned(),
                runtime_invocation: crate::core::NativeRuntimeInvocation::Direct,
                runtime_config: crate::core::NativeRuntimeConfigRealization::Accepted,
                filesystem: crate::core::NativeFilesystemRealization::OverlayFs {
                    profile: "index=on,metacopy=off,nfs_export=off,redirect_dir=nofollow"
                        .to_owned(),
                },
            },
        }
    }

    fn egress_backend() -> BackendFacts {
        BackendFacts {
            network: NetworkControl::Egress,
            ..backend()
        }
    }

    fn resolver_facts() -> RunResolverFacts {
        let bytes = b"nameserver 192.0.2.53\n";
        RunResolverFacts {
            source: RunResolverSource::EtcResolvConf,
            nameservers: vec!["192.0.2.53".to_owned()],
            content_digest: digest_bytes(bytes),
            content_size: bytes.len() as u64,
        }
    }

    #[cfg(target_os = "linux")]
    fn resolver_source_checkpoint(facts: &RunResolverFacts) -> ResolverSourceCheckpoint {
        serde_json::from_value(serde_json::json!({
            "identity": resolver_file_identity(11, 0o100_444, 0, facts.content_size),
            "content_digest": facts.content_digest,
            "content_size": facts.content_size,
        }))
        .expect("resolver source checkpoint")
    }

    #[cfg(target_os = "linux")]
    fn resolver_projection_pending(source: &ResolverSourceCheckpoint) -> ResolverProjectionPending {
        serde_json::from_value(serde_json::json!({
            "source": source,
            "target": resolver_file_identity(12, 0o100_644, 1000, 0),
            "overlay_mount_id": 41,
        }))
        .expect("resolver projection pending")
    }

    #[cfg(target_os = "linux")]
    fn resolver_projection_mounted() -> ResolverProjectionMounted {
        serde_json::from_value(serde_json::json!({
            "projection_mount_id": 42,
        }))
        .expect("resolver projection mounted")
    }

    #[cfg(target_os = "linux")]
    fn resolver_file_identity(inode: u64, mode: u32, uid: u32, size: u64) -> serde_json::Value {
        serde_json::json!({
            "device": 1,
            "inode": inode,
            "mode": mode,
            "uid": uid,
            "gid": 0,
            "links": 1,
            "size": size,
            "modified_seconds": 1,
            "modified_nanoseconds": 0,
            "changed_seconds": 1,
            "changed_nanoseconds": 0,
        })
    }

    #[cfg(target_os = "linux")]
    fn seed_resolver(attempt: &mut NativeAttempt) -> RunResolverFacts {
        let facts = resolver_facts();
        let source = resolver_source_checkpoint(&facts);
        attempt
            .update_journal(|journal| {
                journal.resolver = Some(NativeResolverJournal {
                    facts: facts.clone(),
                    source,
                    primary: NativeResolverProjectionJournal::NotStarted,
                    managed_service: None,
                });
            })
            .expect("seed resolver journal");
        facts
    }

    #[cfg(target_os = "linux")]
    fn activate_egress_network(attempt: &mut NativeAttempt, resolver: RunResolverFacts) {
        attempt
            .advance_phase(NativeRecoveryPhase::Accepted)
            .expect("accept attempt");
        let plan = RunNetworkPlan::egress_ipv4(attempt.journal().run_id(), 42).expect("plan");
        let egress = plan.egress().expect("egress plan");
        let facts = RunNetworkFacts {
            namespace_device: 7,
            namespace_inode: 8,
            realization: RunNetworkRealization::Ipv4NatEgress {
                guest_address: egress.guest_address().to_string(),
                gateway: egress.host_address().to_string(),
                prefix_length: egress.prefix_length(),
                resolver,
            },
        };
        attempt.record_network_plan(plan).expect("network plan");
        attempt
            .record_shared_network(SharedNetworkCheckpoint::for_test(facts, 123, 456))
            .expect("active network");
    }

    #[cfg(target_os = "linux")]
    fn active_egress_attempt(store: &NativeRecoveryStore) -> NativeAttempt {
        let mut attempt = store
            .prepare(RunId::new(), egress_backend())
            .expect("egress attempt");
        let resolver = seed_resolver(&mut attempt);
        activate_egress_network(&mut attempt, resolver);
        attempt
    }

    #[cfg(target_os = "linux")]
    fn rewrite_journal(
        attempt: NativeAttempt,
        update: impl FnOnce(&mut serde_json::Value),
    ) -> RunId {
        let run_id = attempt.journal().run_id();
        let path = attempt.path().join("journal.json");
        let mut journal: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).expect("journal bytes")).expect("journal JSON");
        update(&mut journal);
        drop(attempt);
        fs::write(
            &path,
            serde_json::to_vec(&journal).expect("journal JSON bytes"),
        )
        .expect("rewrite journal");
        run_id
    }

    #[test]
    fn attempt_discovery_is_bounded_sorted_and_paginated() {
        let state = tempfile::tempdir().expect("state");
        let store = NativeRecoveryStore::open(state.path()).expect("store");
        let mut expected = Vec::new();
        for _ in 0..4 {
            let run_id = RunId::new();
            drop(store.prepare(run_id, backend()).expect("attempt"));
            expected.push(run_id);
        }
        expected.sort_by_key(|run_id| std::cmp::Reverse(run_id.to_string()));

        let first = store.list_attempt_ids(None, 2).expect("first page");
        assert_eq!(first.ids, expected[..2]);
        assert!(first.has_more);
        let second = store
            .list_attempt_ids(Some(expected[1]), 2)
            .expect("second page");
        assert_eq!(second.ids, expected[2..]);
        assert!(!second.has_more);
    }

    #[test]
    fn attempt_discovery_rejects_unowned_directory_entries() {
        let state = tempfile::tempdir().expect("state");
        let store = NativeRecoveryStore::open(state.path()).expect("store");
        fs::create_dir(store.root.join("foreign")).expect("foreign entry");

        let error = store
            .list_attempt_ids(None, 20)
            .expect_err("foreign entry must fail closed");

        assert!(
            error
                .to_string()
                .contains("unexpected native recovery entry")
        );
    }

    #[test]
    fn deterministic_staging_is_discovered_and_every_partial_shape_is_recoverable() {
        let state = tempfile::tempdir().expect("state");
        let store = NativeRecoveryStore::open(state.path()).expect("store");

        for completed_steps in 0..=8 {
            let run_id = RunId::new();
            let staging = store.staging_path(run_id);
            fs::create_dir(&staging).expect("staging");
            set_private_directory(&staging).expect("private staging");
            if completed_steps >= 1 {
                drop(create_private_file(&staging.join("lock")).expect("lock"));
            }
            if completed_steps >= 2 {
                ensure_real_private_directory(&staging.join("workspace")).expect("workspace");
            }
            if completed_steps >= 3 {
                drop(create_private_file(&staging.join("stdout")).expect("stdout"));
            }
            if completed_steps >= 4 {
                drop(create_private_file(&staging.join("stderr")).expect("stderr"));
            }
            if completed_steps >= 5 {
                ensure_real_private_directory(&staging.join("workspace/managed-service"))
                    .expect("Managed Service workspace");
            }
            if completed_steps >= 6 {
                drop(
                    create_private_file(&staging.join("managed-service-stdout"))
                        .expect("Managed Service stdout"),
                );
            }
            if completed_steps >= 7 {
                drop(
                    create_private_file(&staging.join("managed-service-stderr"))
                        .expect("Managed Service stderr"),
                );
            }
            if completed_steps >= 8 {
                let mut journal =
                    create_private_file(&staging.join("journal.json")).expect("partial journal");
                journal.write_all(b"{").expect("partial journal bytes");
                journal.sync_all().expect("partial journal fsync");
            }

            let page = store.list_attempt_ids(None, 1).expect("staging page");
            assert_eq!(page.ids, vec![run_id]);
            let Some(NativeRecoveryEntry::Staging(staging_attempt)) =
                store.open_entry(run_id).expect("staging lookup")
            else {
                panic!("expected staging attempt");
            };
            staging_attempt.remove().expect("discard staging");
            assert!(!staging.exists());
        }
    }

    #[test]
    fn staging_conflicts_and_unknown_contents_fail_closed() {
        let state = tempfile::tempdir().expect("state");
        let store = NativeRecoveryStore::open(state.path()).expect("store");
        let conflict_id = RunId::new();
        drop(store.prepare(conflict_id, backend()).expect("attempt"));
        let conflicting_staging = store.staging_path(conflict_id);
        fs::create_dir(&conflicting_staging).expect("conflicting staging");
        set_private_directory(&conflicting_staging).expect("private conflicting staging");
        assert!(
            store
                .open_entry(conflict_id)
                .expect_err("staging and target must conflict")
                .to_string()
                .contains("both staging and published")
        );

        fs::remove_dir(&conflicting_staging).expect("remove conflicting staging");
        let unknown_id = RunId::new();
        let unknown_staging = store.staging_path(unknown_id);
        fs::create_dir(&unknown_staging).expect("unknown staging");
        set_private_directory(&unknown_staging).expect("private unknown staging");
        drop(create_private_file(&unknown_staging.join("foreign")).expect("foreign entry"));
        assert!(
            store
                .open_entry(unknown_id)
                .expect_err("unknown staging contents must fail")
                .to_string()
                .contains("unexpected native recovery staging entry")
        );
        assert!(unknown_staging.exists());
    }

    #[test]
    fn staging_publish_is_no_clobber_and_discovery_respects_the_root_lock() {
        let state = tempfile::tempdir().expect("state");
        let store = NativeRecoveryStore::open(state.path()).expect("store");
        let run_id = RunId::new();
        let staging = store.staging_path(run_id);
        let target = store.root.join(run_id.to_string());
        create_private_directory(&staging).expect("staging");
        create_private_directory(&target).expect("target");

        let root_lock = store.lock_root().expect("root lock");
        let publish = publish_staging(&root_lock, run_id).expect_err("publish must not replace");
        assert!(publish.to_string().contains("failed to publish"));
        assert!(staging.is_dir());
        assert!(target.is_dir());
        assert!(
            store
                .list_attempt_ids(None, 20)
                .expect_err("discovery must not race preparation")
                .to_string()
                .contains("native recovery root is active")
        );
        drop(root_lock);
    }

    #[test]
    fn failed_atomic_staging_create_never_cleans_an_unowned_collision() {
        let state = tempfile::tempdir().expect("state");
        let store = NativeRecoveryStore::open(state.path()).expect("store");
        let run_id = RunId::new();
        let expected = b"partial journal owned elsewhere";

        let error = store
            .prepare_inner_after_precheck(run_id, backend(), false, |staging| {
                create_private_directory(staging)?;
                let mut journal = create_private_file(&staging.join("journal.json"))?;
                journal.write_all(expected)?;
                journal.sync_all()?;
                Ok(())
            })
            .expect_err("atomic create collision must fail");

        assert!(
            format!("{error:#}").contains("failed to create private directory"),
            "unexpected error: {error:#}"
        );
        let staging = store.staging_path(run_id);
        assert_eq!(
            fs::read(staging.join("journal.json")).expect("collision bytes"),
            expected
        );
        assert_eq!(mode(&staging), 0o700);
        assert_eq!(mode(&staging.join("journal.json")), 0o600);
    }

    #[test]
    fn staging_with_a_complete_post_acceptance_journal_fails_closed() {
        let state = tempfile::tempdir().expect("state");
        let store = NativeRecoveryStore::open(state.path()).expect("store");
        let source_id = RunId::new();
        let source = store.prepare(source_id, backend()).expect("source attempt");
        let mut journal: serde_json::Value = serde_json::from_slice(
            &fs::read(source.path().join("journal.json")).expect("source journal"),
        )
        .expect("source journal JSON");
        source.remove().expect("remove source attempt");

        let run_id = RunId::new();
        journal["run_id"] = serde_json::Value::String(run_id.to_string());
        journal["runtime_id"] = serde_json::Value::String(runtime_id(run_id));
        journal["phase"] = serde_json::Value::String("runtime_active".to_owned());
        let staging = store.staging_path(run_id);
        create_private_directory(&staging).expect("staging");
        let mut file = create_private_file(&staging.join("journal.json")).expect("journal");
        file.write_all(&serde_json::to_vec(&journal).expect("journal bytes"))
            .expect("journal write");
        file.sync_all().expect("journal fsync");

        let error = store
            .open_entry(run_id)
            .expect_err("post-acceptance staging must fail closed");
        assert!(error.to_string().contains("published resource state"));
        assert!(staging.exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires rootful Linux mount privileges"]
    fn staging_cleanup_rejects_every_bind_mount_boundary() {
        let state = tempfile::tempdir().expect("state");
        let store = NativeRecoveryStore::open(state.path()).expect("store");

        let directory_source = state.path().join("directory-source");
        create_private_directory(&directory_source).expect("directory source");
        fs::write(directory_source.join("sentinel"), b"directory").expect("directory sentinel");
        let directory_id = RunId::new();
        let directory_staging = store.staging_path(directory_id);
        create_private_directory(&directory_staging).expect("directory staging");
        {
            let _mount = TestBindMount::new(&directory_source, &directory_staging);
            assert_mount_boundary_rejected(&store, directory_id);
            assert_eq!(
                fs::read(directory_source.join("sentinel")).expect("directory sentinel retained"),
                b"directory"
            );
        }

        let workspace_source = state.path().join("workspace-source");
        create_private_directory(&workspace_source).expect("workspace source");
        fs::write(workspace_source.join("sentinel"), b"workspace").expect("workspace sentinel");
        let workspace_id = RunId::new();
        let workspace_staging = store.staging_path(workspace_id);
        create_private_directory(&workspace_staging).expect("workspace staging");
        create_private_directory(&workspace_staging.join("workspace")).expect("workspace target");
        {
            let _mount =
                TestBindMount::new(&workspace_source, &workspace_staging.join("workspace"));
            assert_mount_boundary_rejected(&store, workspace_id);
            assert_eq!(
                fs::read(workspace_source.join("sentinel")).expect("workspace sentinel retained"),
                b"workspace"
            );
        }

        let managed_source = state.path().join("managed-source");
        create_private_directory(&managed_source).expect("Managed Service source");
        fs::write(managed_source.join("sentinel"), b"managed").expect("managed sentinel");
        let managed_id = RunId::new();
        let managed_staging = store.staging_path(managed_id);
        create_private_directory(&managed_staging).expect("Managed Service staging");
        create_private_directory(&managed_staging.join("workspace")).expect("workspace");
        create_private_directory(&managed_staging.join("workspace/managed-service"))
            .expect("Managed Service target");
        {
            let _mount = TestBindMount::new(
                &managed_source,
                &managed_staging.join("workspace/managed-service"),
            );
            assert_mount_boundary_rejected(&store, managed_id);
            assert_eq!(
                fs::read(managed_source.join("sentinel")).expect("managed sentinel retained"),
                b"managed"
            );
        }

        let file_source = state.path().join("file-source");
        let file = create_private_file(&file_source).expect("file source");
        file.sync_all().expect("file source fsync");
        let file_id = RunId::new();
        let file_staging = store.staging_path(file_id);
        create_private_directory(&file_staging).expect("file staging");
        drop(create_private_file(&file_staging.join("stdout")).expect("file target"));
        {
            let _mount = TestBindMount::new(&file_source, &file_staging.join("stdout"));
            assert_mount_boundary_rejected(&store, file_id);
            assert!(file_source.is_file());
        }
    }

    #[test]
    fn pristine_prepublication_journal_rejects_every_fact_family() {
        let state = tempfile::tempdir().expect("state");
        let store = NativeRecoveryStore::open(state.path()).expect("store");
        let source = store
            .prepare_managed(RunId::new(), backend())
            .expect("source attempt");
        let pristine = source.journal().clone();
        validate_pristine_prepublication_journal(&pristine).expect("pristine journal");
        source.remove().expect("remove source attempt");

        let mut cases = Vec::new();

        let mut journal = pristine.clone();
        journal.process = Some(exited_process());
        cases.push(journal);

        let mut journal = pristine.clone();
        journal.stdout = Some(StoredBytes::NotApplicable);
        cases.push(journal);

        let mut journal = pristine.clone();
        journal.stderr = Some(StoredBytes::NotApplicable);
        cases.push(journal);

        let mut journal = pristine.clone();
        journal.captured_at = Some(Utc::now());
        cases.push(journal);

        let mut journal = pristine.clone();
        journal.final_image = Some(ImageSlot::NotApplicable);
        cases.push(journal);

        let mut journal = pristine.clone();
        journal.terminal_at = Some(Utc::now());
        cases.push(journal);

        let mut journal = pristine.clone();
        journal
            .operation_errors
            .push(fixture_operation_error(OperationErrorScope::Run));
        cases.push(journal);

        let mut journal = pristine.clone();
        journal
            .managed_service
            .as_mut()
            .expect("Managed Service")
            .readiness = Some(ManagedServiceReadiness::Ready {
            observed_at: Utc::now(),
            attempts: 1,
        });
        cases.push(journal);

        let mut journal = pristine.clone();
        journal
            .managed_service
            .as_mut()
            .expect("Managed Service")
            .process = Some(exited_process());
        cases.push(journal);

        let mut journal = pristine.clone();
        journal
            .managed_service
            .as_mut()
            .expect("Managed Service")
            .stdout = Some(StoredBytes::NotApplicable);
        cases.push(journal);

        let mut journal = pristine.clone();
        journal
            .managed_service
            .as_mut()
            .expect("Managed Service")
            .stderr = Some(StoredBytes::NotApplicable);
        cases.push(journal);

        let mut journal = pristine.clone();
        journal
            .managed_service
            .as_mut()
            .expect("Managed Service")
            .captured_at = Some(Utc::now());
        cases.push(journal);

        let mut journal = pristine.clone();
        journal
            .managed_service
            .as_mut()
            .expect("Managed Service")
            .final_image = Some(ImageSlot::NotApplicable);
        cases.push(journal);

        let mut journal = pristine.clone();
        journal
            .managed_service
            .as_mut()
            .expect("Managed Service")
            .operation_errors
            .push(fixture_operation_error(OperationErrorScope::ManagedService));
        cases.push(journal);

        let mut journal = pristine.clone();
        let run_id = journal.run_id;
        let network = journal.shared_network.as_mut().expect("shared network");
        network.plan = Some(RunNetworkPlan::loopback(run_id));
        cases.push(journal);

        for journal in cases {
            assert!(
                validate_pristine_prepublication_journal(&journal).is_err(),
                "prepublication journal accepted mutable facts: {journal:?}"
            );
        }
    }

    fn fixture_operation_error(scope: OperationErrorScope) -> OperationError {
        OperationError {
            scope,
            phase: "fixture".to_owned(),
            message: "fixture".to_owned(),
        }
    }

    #[cfg(target_os = "linux")]
    fn assert_mount_boundary_rejected(store: &NativeRecoveryStore, run_id: RunId) {
        let error = store
            .open_entry(run_id)
            .expect_err("bind-mounted staging must fail closed");
        assert!(error.to_string().contains("crosses a mount boundary"));
    }

    #[cfg(target_os = "linux")]
    struct TestBindMount {
        target: PathBuf,
    }

    #[cfg(target_os = "linux")]
    impl TestBindMount {
        fn new(source: &Path, target: &Path) -> Self {
            rustix::mount::mount(
                source,
                target,
                "",
                rustix::mount::MountFlags::BIND,
                None::<&std::ffi::CStr>,
            )
            .expect("bind mount");
            Self {
                target: target.to_path_buf(),
            }
        }
    }

    #[cfg(target_os = "linux")]
    impl Drop for TestBindMount {
        fn drop(&mut self) {
            rustix::mount::unmount(&self.target, rustix::mount::UnmountFlags::DETACH)
                .expect("unmount bind fixture");
        }
    }

    #[test]
    fn atomically_round_trips_a_private_attempt() {
        let state = tempfile::tempdir().expect("state");
        let store = NativeRecoveryStore::open(state.path()).expect("store");
        let run_id = RunId::new();
        let mut attempt = store.prepare(run_id, backend()).expect("attempt");
        assert_eq!(attempt.journal().generation(), 1);
        attempt
            .advance_phase(NativeRecoveryPhase::Accepted)
            .expect("advance");
        let path = attempt.path().to_path_buf();
        drop(attempt);

        let reopened = store
            .open_attempt(run_id)
            .expect("open")
            .expect("attempt exists");
        assert_eq!(reopened.journal().run_id(), run_id);
        assert_eq!(reopened.journal().phase(), NativeRecoveryPhase::Accepted);
        assert_eq!(reopened.journal().generation(), 2);
        assert_eq!(reopened.journal().backend(), &backend());
        assert!(reopened.journal().runtime_id().starts_with("runlab-"));
        assert!(reopened.workspace().is_dir());
        assert!(reopened.stdout_path().is_file());
        assert!(reopened.stderr_path().is_file());
        assert_eq!(
            fs::read_dir(path)
                .expect("attempt entries")
                .filter_map(std::result::Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().starts_with(".tmp"))
                .count(),
            0
        );
    }

    #[test]
    fn network_plan_is_durable_only_after_acceptance_and_cleans_before_creation() {
        let state = tempfile::tempdir().expect("state");
        let store = NativeRecoveryStore::open(state.path()).expect("store");
        let run_id = RunId::new();
        let mut attempt = store.prepare_managed(run_id, backend()).expect("attempt");
        let plan = RunNetworkPlan::loopback(run_id);
        assert!(attempt.record_network_plan(plan.clone()).is_err());
        attempt
            .advance_phase(NativeRecoveryPhase::Accepted)
            .expect("accepted");
        attempt
            .advance_participant_phase(
                NativeParticipant::ManagedService,
                NativeRecoveryPhase::Accepted,
            )
            .expect("Managed Service accepted");
        attempt
            .record_network_plan(plan.clone())
            .expect("durable plan");
        assert_eq!(
            attempt
                .journal()
                .shared_network()
                .expect("Run network")
                .plan(),
            Some(&plan)
        );
        attempt.begin_shared_network_cleanup().expect("cleanup");
        attempt
            .record_shared_network_cleanup(Utc::now())
            .expect("cleanup complete");
        let path = attempt.path().to_path_buf();
        drop(attempt);
        let journal: serde_json::Value =
            serde_json::from_slice(&fs::read(path.join("journal.json")).expect("journal"))
                .expect("journal JSON");
        assert_eq!(journal["shared_network"]["phase"], "cleanup_complete");
        assert_eq!(journal["shared_network"]["plan"]["mode"], "loopback_only");
        assert!(journal["shared_network"]["plan"]["egress"].is_null());
        assert!(journal["backend"]["run_network"].is_null());
    }

    #[test]
    fn subnet_slot_start_is_bounded_and_identity_derived() {
        let run_id = RunId::parse("run-018f47e2-7c31-7b18-a780-bf56f69303d9").expect("Run ID");
        let count = RunNetworkPlan::egress_subnet_count();
        let slot = initial_subnet_slot(run_id, count).expect("slot");
        assert!(slot < count);
        assert_eq!(slot, initial_subnet_slot(run_id, count).expect("same slot"));
        assert!(initial_subnet_slot(run_id, 0).is_err());
    }

    #[test]
    fn network_facts_must_match_the_durable_plan() {
        let run_id = RunId::new();
        let plan = RunNetworkPlan::egress_ipv4(run_id, 42).expect("plan");
        let egress = plan.egress().expect("egress plan");
        let mut facts = RunNetworkFacts {
            namespace_device: 7,
            namespace_inode: 8,
            realization: RunNetworkRealization::Ipv4NatEgress {
                guest_address: egress.guest_address().to_string(),
                gateway: egress.host_address().to_string(),
                prefix_length: egress.prefix_length(),
                resolver: resolver_facts(),
            },
        };
        validate_network_facts_for_plan(&plan, &facts).expect("matching facts");
        facts.realization = RunNetworkRealization::Ipv4NatEgress {
            guest_address: "10.240.255.254".to_owned(),
            gateway: egress.host_address().to_string(),
            prefix_length: egress.prefix_length(),
            resolver: resolver_facts(),
        };
        assert!(validate_network_facts_for_plan(&plan, &facts).is_err());
        assert!(
            validate_network_facts_for_plan(
                &RunNetworkPlan::loopback(run_id),
                &RunNetworkFacts {
                    namespace_device: 7,
                    namespace_inode: 8,
                    realization: facts.realization,
                },
            )
            .is_err()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn resolver_fact_mismatches_fail_closed_before_source_reopen() {
        let state = tempfile::tempdir().expect("state");
        let store = NativeRecoveryStore::open(state.path()).expect("store");

        let attempt = active_egress_attempt(&store);
        let run_id = rewrite_journal(attempt, |journal| {
            journal["resolver"]["source"]["content_digest"] =
                serde_json::to_value(digest_bytes(b"different source")).expect("digest JSON");
        });
        let error = store
            .open_attempt(run_id)
            .expect_err("source/facts mismatch must fail");
        assert!(
            format!("{error:#}")
                .contains("native resolver source checkpoint differs from Run resolver facts"),
            "unexpected error: {error:#}"
        );

        let attempt = active_egress_attempt(&store);
        let run_id = rewrite_journal(attempt, |journal| {
            journal["resolver"]["facts"]["content_digest"] =
                serde_json::to_value(digest_bytes(b"different top-level facts"))
                    .expect("digest JSON");
        });
        let error = store
            .open_attempt(run_id)
            .expect_err("non-canonical top-level facts must fail");
        assert!(
            format!("{error:#}").contains("native resolver facts have an invalid canonical digest"),
            "unexpected error: {error:#}"
        );

        let attempt = active_egress_attempt(&store);
        let alternate_bytes = b"nameserver 192.0.2.54\n";
        let alternate = RunResolverFacts {
            source: RunResolverSource::EtcResolvConf,
            nameservers: vec!["192.0.2.54".to_owned()],
            content_digest: digest_bytes(alternate_bytes),
            content_size: alternate_bytes.len() as u64,
        };
        let run_id = rewrite_journal(attempt, |journal| {
            let alternate = serde_json::to_value(alternate).expect("resolver facts JSON");
            journal["backend"]["run_network"]["realization"]["resolver"] = alternate.clone();
            journal["shared_network"]["facts"]["realization"]["resolver"] = alternate;
        });
        let error = store
            .open_attempt(run_id)
            .expect_err("network/top-level facts mismatch must fail");
        assert!(
            format!("{error:#}")
                .contains("Run network resolver facts differ from native resolver facts"),
            "unexpected error: {error:#}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn runtime_start_pending_requires_a_mounted_resolver() {
        let state = tempfile::tempdir().expect("state");
        let store = NativeRecoveryStore::open(state.path()).expect("store");
        let mut attempt = active_egress_attempt(&store);
        let generation = attempt.journal().generation();

        let error = attempt
            .advance_phase(NativeRecoveryPhase::RuntimeStartPending)
            .expect_err("runtime start without resolver projection must fail");

        assert!(
            format!("{error:#}").contains("runtime requires an active resolver projection"),
            "unexpected error: {error:#}"
        );
        assert_eq!(attempt.journal().phase(), NativeRecoveryPhase::Accepted);
        assert_eq!(attempt.journal().generation(), generation);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn active_resolver_blocks_capture_without_mutating_observations() {
        let state = tempfile::tempdir().expect("state");
        let store = NativeRecoveryStore::open(state.path()).expect("store");
        let mut attempt = active_egress_attempt(&store);
        attempt
            .advance_phase(NativeRecoveryPhase::OverlayMounted)
            .expect("overlay mounted");
        let source = attempt
            .journal()
            .resolver()
            .expect("resolver")
            .source()
            .clone();
        attempt
            .begin_resolver_mount(
                NativeParticipant::Primary,
                resolver_projection_pending(&source),
            )
            .expect("resolver mount pending");
        attempt
            .record_resolver_mounted(NativeParticipant::Primary, resolver_projection_mounted())
            .expect("resolver mounted");
        attempt
            .advance_phase(NativeRecoveryPhase::RuntimeStartPending)
            .expect("runtime start pending");
        attempt
            .advance_phase(NativeRecoveryPhase::RuntimeActive)
            .expect("runtime active");
        let stdout = b"durable stdout";
        let stderr = b"durable stderr";
        attempt
            .record_process(
                exited_process(),
                stored(stdout),
                stored(stderr),
                Some(stdout),
                Some(stderr),
                Vec::new(),
            )
            .expect("process observation");
        let generation = attempt.journal().generation();
        let journal = fs::read(attempt.path().join("journal.json")).expect("journal bytes");

        let error = attempt
            .begin_participant_capture(NativeParticipant::Primary, Utc::now())
            .expect_err("active resolver must block capture");

        assert!(
            format!("{error:#}").contains("primary resolver projection cleanup is incomplete"),
            "unexpected error: {error:#}"
        );
        assert_eq!(attempt.journal().generation(), generation);
        assert_eq!(
            attempt.journal().phase(),
            NativeRecoveryPhase::ProcessObserved
        );
        assert_eq!(
            fs::read(attempt.path().join("journal.json")).expect("journal bytes"),
            journal
        );
        assert_eq!(fs::read(attempt.stdout_path()).expect("stdout"), stdout);
        assert_eq!(fs::read(attempt.stderr_path()).expect("stderr"), stderr);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn not_started_resolver_cleanup_is_idempotent() {
        let state = tempfile::tempdir().expect("state");
        let store = NativeRecoveryStore::open(state.path()).expect("store");
        let mut attempt = store
            .prepare(RunId::new(), egress_backend())
            .expect("egress attempt");
        seed_resolver(&mut attempt);
        let generation = attempt.journal().generation();
        let journal = fs::read(attempt.path().join("journal.json")).expect("journal bytes");

        attempt
            .begin_resolver_cleanup(NativeParticipant::Primary)
            .expect("first cleanup");
        attempt
            .begin_resolver_cleanup(NativeParticipant::Primary)
            .expect("repeated cleanup");

        assert!(matches!(
            attempt
                .resolver_projection(NativeParticipant::Primary)
                .expect("resolver projection"),
            NativeResolverProjectionJournal::NotStarted
        ));
        assert_eq!(attempt.journal().generation(), generation);
        assert_eq!(
            fs::read(attempt.path().join("journal.json")).expect("journal bytes"),
            journal
        );
    }

    #[test]
    fn durably_records_an_unavailable_process_slot() {
        let state = tempfile::tempdir().expect("state");
        let store = NativeRecoveryStore::open(state.path()).expect("store");
        let run_id = RunId::new();
        let mut attempt = store.prepare(run_id, backend()).expect("attempt");
        let process = ProcessSlot::Unavailable {
            error: "process evidence was lost".to_owned(),
        };
        let stdout = StoredBytes::Unavailable {
            error: "stdout evidence was lost".to_owned(),
        };
        let stderr = StoredBytes::Unavailable {
            error: "stderr evidence was lost".to_owned(),
        };
        attempt
            .begin_participant_recovery_capture(
                NativeParticipant::Primary,
                RecoveryCaptureCheckpoint {
                    captured_at: Utc::now(),
                    process: process.clone(),
                    stdout: stdout.clone(),
                    stderr: stderr.clone(),
                    stdout_bytes: None,
                    stderr_bytes: None,
                    operation_errors: Vec::new(),
                },
            )
            .expect("recovery checkpoint");
        let path = attempt.path().join("journal.json");
        let encoded: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).expect("journal")).expect("JSON");
        assert_eq!(encoded["schema_version"], JOURNAL_SCHEMA_VERSION);
        assert_eq!(encoded["process"]["availability"], "unavailable");
        drop(attempt);

        let reopened = store
            .open_attempt(run_id)
            .expect("open")
            .expect("attempt exists");
        assert_eq!(reopened.journal().process(), Some(&process));
        assert_eq!(reopened.journal().stdout(), Some(&stdout));
        assert_eq!(reopened.journal().stderr(), Some(&stderr));
    }

    #[test]
    fn managed_participants_checkpoint_independent_facts() {
        let state = tempfile::tempdir().expect("state");
        let store = NativeRecoveryStore::open(state.path()).expect("store");
        let run_id = RunId::new();
        let mut attempt = store
            .prepare_managed(run_id, backend())
            .expect("managed attempt");
        assert_managed_identity_and_paths(&attempt);
        attempt
            .advance_phase(NativeRecoveryPhase::Accepted)
            .expect("primary accepted");
        attempt
            .advance_participant_phase(
                NativeParticipant::ManagedService,
                NativeRecoveryPhase::Accepted,
            )
            .expect("service accepted");
        let facts = checkpoint_managed(&mut attempt);
        assert_eq!(attempt.journal().phase(), NativeRecoveryPhase::Accepted);
        let path = attempt.path().to_path_buf();
        drop(attempt);

        let reopened = store
            .open_attempt(run_id)
            .expect("open")
            .expect("attempt exists");
        let service = reopened
            .journal()
            .managed_service()
            .expect("managed service");
        assert_eq!(service.phase(), NativeRecoveryPhase::TerminalPrepared);
        assert_eq!(service.readiness(), Some(&facts.readiness));
        assert_eq!(service.process(), Some(&facts.process));
        assert_eq!(service.stdout(), Some(&facts.stdout));
        assert_eq!(service.stderr(), Some(&facts.stderr));
        assert_eq!(service.captured_at(), Some(facts.captured_at));
        assert_eq!(service.final_image(), Some(&facts.final_image));
        assert_eq!(service.operation_errors(), facts.errors);
        let network = reopened.journal().shared_network().expect("shared network");
        assert_eq!(network.phase(), NativeSharedNetworkPhase::CleanupComplete);
        assert_eq!(network.facts(), Some(&facts.network));
        assert_eq!(network.holder_pid(), Some(123));
        assert_eq!(network.holder_start_time_ticks(), Some(456));
        assert_eq!(
            network.holder_exit_observed_at(),
            Some(facts.holder_exit_observed_at)
        );
        assert_eq!(
            fs::read(path.join("managed-service-stdout")).expect("service stdout"),
            facts.stdout_bytes
        );
        assert_eq!(
            fs::read(path.join("managed-service-stderr")).expect("service stderr"),
            facts.stderr_bytes
        );
    }

    #[test]
    fn rejects_cross_participant_operation_error_scopes() {
        let state = tempfile::tempdir().expect("state");
        let store = NativeRecoveryStore::open(state.path()).expect("store");
        let mut attempt = store
            .prepare_managed(RunId::new(), backend())
            .expect("managed attempt");
        let process = exited_process();
        let stream = StoredBytes::Unavailable {
            error: "fixture stream unavailable".to_owned(),
        };
        let primary_error = OperationError {
            scope: OperationErrorScope::ManagedService,
            phase: "execute".to_owned(),
            message: "wrong participant".to_owned(),
        };
        assert!(
            attempt
                .record_process(
                    process.clone(),
                    stream.clone(),
                    stream.clone(),
                    None,
                    None,
                    vec![primary_error],
                )
                .is_err()
        );
        let service_error = OperationError {
            scope: OperationErrorScope::Run,
            phase: "execute".to_owned(),
            message: "wrong participant".to_owned(),
        };
        assert!(
            attempt
                .record_participant_process(
                    NativeParticipant::ManagedService,
                    process,
                    stream.clone(),
                    stream,
                    None,
                    None,
                    vec![service_error],
                )
                .is_err()
        );
    }

    #[test]
    fn observed_process_facts_and_sidecars_are_immutable() {
        let state = tempfile::tempdir().expect("state");
        let store = NativeRecoveryStore::open(state.path()).expect("store");
        let mut attempt = store.prepare(RunId::new(), backend()).expect("attempt");
        let stdout = b"first stdout";
        let stderr = b"first stderr";
        attempt
            .record_process(
                exited_process(),
                stored(stdout),
                stored(stderr),
                Some(stdout),
                Some(stderr),
                Vec::new(),
            )
            .expect("first observation");
        let replacement = b"replacement";
        assert!(
            attempt
                .record_process(
                    ProcessSlot::available(ProcessFacts::not_started()),
                    stored(replacement),
                    stored(stderr),
                    Some(replacement),
                    Some(stderr),
                    Vec::new(),
                )
                .is_err()
        );
        assert_eq!(fs::read(attempt.stdout_path()).expect("stdout"), stdout);
        assert_eq!(fs::read(attempt.stderr_path()).expect("stderr"), stderr);
    }

    struct ManagedCheckpointFacts {
        network: RunNetworkFacts,
        readiness: ManagedServiceReadiness,
        process: ProcessSlot,
        stdout: StoredBytes,
        stderr: StoredBytes,
        captured_at: DateTime<Utc>,
        final_image: ImageSlot,
        errors: Vec<OperationError>,
        holder_exit_observed_at: DateTime<Utc>,
        stdout_bytes: &'static [u8],
        stderr_bytes: &'static [u8],
    }

    fn checkpoint_managed(attempt: &mut NativeAttempt) -> ManagedCheckpointFacts {
        attempt
            .record_network_plan(RunNetworkPlan::loopback(attempt.journal().run_id))
            .expect("network plan");
        let network = RunNetworkFacts {
            namespace_device: 4,
            namespace_inode: 42,
            realization: crate::core::RunNetworkRealization::LoopbackOnly,
        };
        attempt
            .record_shared_network(SharedNetworkCheckpoint {
                facts: network.clone(),
                holder_pid: 123,
                holder_start_time_ticks: 456,
            })
            .expect("network active");
        let readiness = ManagedServiceReadiness::Ready {
            observed_at: Utc::now(),
            attempts: 2,
        };
        attempt
            .record_managed_readiness(readiness.clone())
            .expect("readiness");
        let process = exited_process();
        let stdout_bytes = b"service stdout";
        let stderr_bytes = b"service stderr";
        let stdout = stored(stdout_bytes);
        let stderr = stored(stderr_bytes);
        let errors = vec![OperationError {
            scope: OperationErrorScope::ManagedService,
            phase: "cleanup".to_owned(),
            message: "fixture warning".to_owned(),
        }];
        attempt
            .record_participant_process(
                NativeParticipant::ManagedService,
                process.clone(),
                stdout.clone(),
                stderr.clone(),
                Some(stdout_bytes),
                Some(stderr_bytes),
                errors.clone(),
            )
            .expect("service process");
        let captured_at = Utc::now();
        attempt
            .begin_participant_capture(NativeParticipant::ManagedService, captured_at)
            .expect("service capture");
        let final_image = ImageSlot::Available {
            manifest: descriptor('3'),
        };
        attempt
            .record_participant_final(NativeParticipant::ManagedService, final_image.clone())
            .expect("service Final Image");
        attempt
            .begin_shared_network_cleanup()
            .expect("network cleanup pending");
        let holder_exit_observed_at = Utc::now();
        attempt
            .record_shared_network_cleanup(holder_exit_observed_at)
            .expect("network cleanup complete");
        attempt
            .prepare_managed_terminal(ManagedTerminalCheckpoint {
                readiness: readiness.clone(),
                process: process.clone(),
                stdout: stdout.clone(),
                stderr: stderr.clone(),
                stdout_bytes: Some(stdout_bytes),
                stderr_bytes: Some(stderr_bytes),
                final_image: final_image.clone(),
                operation_errors: errors.clone(),
            })
            .expect("service terminal");
        ManagedCheckpointFacts {
            network,
            readiness,
            process,
            stdout,
            stderr,
            captured_at,
            final_image,
            errors,
            holder_exit_observed_at,
            stdout_bytes,
            stderr_bytes,
        }
    }

    fn assert_managed_identity_and_paths(attempt: &NativeAttempt) {
        let primary = attempt
            .participant_runtime_id(NativeParticipant::Primary)
            .expect("primary runtime");
        let service = attempt
            .participant_runtime_id(NativeParticipant::ManagedService)
            .expect("service runtime");
        assert_ne!(primary, service);
        assert!(service.ends_with("-service"));
        let service_root = attempt.workspace().join("managed-service");
        assert_eq!(
            attempt
                .participant_lower_workspace(NativeParticipant::ManagedService)
                .expect("lower path"),
            service_root.join("lower")
        );
        assert_eq!(
            attempt
                .participant_bundle_directory(NativeParticipant::ManagedService)
                .expect("bundle path"),
            service_root.join("bundle")
        );
        assert_eq!(
            attempt
                .participant_overlay_workspace(NativeParticipant::ManagedService)
                .expect("overlay path"),
            service_root.join("overlay")
        );
        assert_eq!(
            attempt
                .participant_runtime_root(NativeParticipant::ManagedService)
                .expect("runtime path"),
            service_root.join("runtime")
        );
        assert!(
            attempt
                .participant_stdout_path(NativeParticipant::ManagedService)
                .is_file()
        );
        assert!(
            attempt
                .participant_stderr_path(NativeParticipant::ManagedService)
                .is_file()
        );
    }

    #[test]
    fn inspecting_an_absent_store_is_read_only() {
        let state = tempfile::tempdir().expect("state");
        fs::set_permissions(state.path(), fs::Permissions::from_mode(0o700)).expect("state mode");
        assert!(
            NativeRecoveryStore::open_existing(state.path())
                .expect("inspect store")
                .is_none()
        );
        assert!(!state.path().join("recovery").exists());
    }

    #[test]
    fn rejects_backward_phase_transition() {
        let state = tempfile::tempdir().expect("state");
        let store = NativeRecoveryStore::open(state.path()).expect("store");
        let mut attempt = store.prepare(RunId::new(), backend()).expect("attempt");
        attempt
            .advance_phase(NativeRecoveryPhase::OverlayMounted)
            .expect("advance");
        let error = attempt
            .advance_phase(NativeRecoveryPhase::Accepted)
            .expect_err("backward phase must fail");
        assert!(error.to_string().contains("cannot move backward"));
        assert_eq!(attempt.journal().generation(), 2);
    }

    #[test]
    fn rejects_lock_competition() {
        let state = tempfile::tempdir().expect("state");
        let store = NativeRecoveryStore::open(state.path()).expect("store");
        let run_id = RunId::new();
        let attempt = store.prepare(run_id, backend()).expect("attempt");
        let error = store
            .open_attempt(run_id)
            .expect_err("second owner must fail");
        assert!(error.to_string().contains("is active"));
        drop(attempt);
        assert!(store.open_attempt(run_id).expect("reopen").is_some());
    }

    #[test]
    fn rejects_truncated_unknown_and_oversized_journals() {
        let state = tempfile::tempdir().expect("state");
        let store = NativeRecoveryStore::open(state.path()).expect("store");

        let truncated_id = RunId::new();
        let truncated = store.prepare(truncated_id, backend()).expect("attempt");
        let truncated_path = truncated.path().join("journal.json");
        drop(truncated);
        fs::write(&truncated_path, b"{").expect("truncate journal");
        assert!(store.open_attempt(truncated_id).is_err());

        let unknown_id = RunId::new();
        let unknown = store.prepare(unknown_id, backend()).expect("attempt");
        let unknown_path = unknown.path().join("journal.json");
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&unknown_path).expect("journal")).expect("JSON");
        value["unexpected"] = serde_json::json!(true);
        drop(unknown);
        fs::write(&unknown_path, serde_json::to_vec(&value).expect("JSON")).expect("write");
        assert!(store.open_attempt(unknown_id).is_err());

        let outdated_id = RunId::new();
        let outdated = store.prepare(outdated_id, backend()).expect("attempt");
        let outdated_path = outdated.path().join("journal.json");
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&outdated_path).expect("journal")).expect("JSON");
        value["schema_version"] = serde_json::json!(2);
        drop(outdated);
        fs::write(&outdated_path, serde_json::to_vec(&value).expect("JSON")).expect("write");
        let error = store
            .open_attempt(outdated_id)
            .expect_err("outdated journal must fail");
        assert!(
            error
                .to_string()
                .contains("unsupported native recovery journal version")
        );

        let inconsistent_id = RunId::new();
        let inconsistent = store.prepare(inconsistent_id, backend()).expect("attempt");
        let inconsistent_path = inconsistent.path().join("journal.json");
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&inconsistent_path).expect("journal")).expect("JSON");
        value["phase"] = serde_json::json!("capture_pending");
        drop(inconsistent);
        fs::write(
            &inconsistent_path,
            serde_json::to_vec(&value).expect("JSON"),
        )
        .expect("write");
        let error = store
            .open_attempt(inconsistent_id)
            .expect_err("inconsistent journal must fail");
        assert!(
            error
                .to_string()
                .contains("requires process and stream facts")
        );

        let oversized_id = RunId::new();
        let oversized = store.prepare(oversized_id, backend()).expect("attempt");
        let oversized_path = oversized.path().join("journal.json");
        drop(oversized);
        fs::write(
            &oversized_path,
            vec![b'x'; usize::try_from(MAX_JOURNAL_BYTES + 1).expect("journal bound fits usize")],
        )
        .expect("oversized journal");
        let error = store
            .open_attempt(oversized_id)
            .expect_err("oversized journal must fail");
        assert!(error.to_string().contains("exceeds"));
    }

    #[test]
    fn creates_exact_private_permissions() {
        let state = tempfile::tempdir().expect("state");
        let store = NativeRecoveryStore::open(state.path()).expect("store");
        let attempt = store.prepare(RunId::new(), backend()).expect("attempt");
        assert_eq!(mode(attempt.path()), 0o700);
        assert_eq!(mode(&attempt.workspace()), 0o700);
        assert_eq!(mode(&attempt.path().join("lock")), 0o600);
        assert_eq!(mode(&attempt.path().join("journal.json")), 0o600);
        assert_eq!(mode(&attempt.stdout_path()), 0o600);
        assert_eq!(mode(&attempt.stderr_path()), 0o600);
        let managed = store
            .prepare_managed(RunId::new(), backend())
            .expect("managed attempt");
        assert_eq!(
            mode(
                &managed
                    .participant_workspace(NativeParticipant::ManagedService)
                    .expect("service workspace")
            ),
            0o700
        );
        assert_eq!(
            mode(&managed.participant_stdout_path(NativeParticipant::ManagedService)),
            0o600
        );
        assert_eq!(
            mode(&managed.participant_stderr_path(NativeParticipant::ManagedService)),
            0o600
        );
    }

    #[test]
    fn rejects_a_replaced_journal_symlink() {
        let state = tempfile::tempdir().expect("state");
        let store = NativeRecoveryStore::open(state.path()).expect("store");
        let run_id = RunId::new();
        let attempt = store.prepare(run_id, backend()).expect("attempt");
        let journal = attempt.path().join("journal.json");
        let outside = state.path().join("outside");
        fs::write(&outside, b"{}").expect("outside");
        drop(attempt);
        fs::remove_file(&journal).expect("remove journal");
        symlink(&outside, &journal).expect("symlink journal");
        let error = store.open_attempt(run_id).expect_err("symlink must fail");
        assert!(error.to_string().contains("must not be a symbolic link"));
    }

    #[test]
    fn rejects_managed_workspace_and_sidecar_symlinks() {
        let state = tempfile::tempdir().expect("state");
        let store = NativeRecoveryStore::open(state.path()).expect("store");
        let outside = state.path().join("outside");
        fs::create_dir(&outside).expect("outside directory");

        let workspace_id = RunId::new();
        let workspace_attempt = store
            .prepare_managed(workspace_id, backend())
            .expect("managed attempt");
        let workspace = workspace_attempt.workspace().join("managed-service");
        drop(workspace_attempt);
        fs::remove_dir(&workspace).expect("remove service workspace");
        symlink(&outside, &workspace).expect("symlink service workspace");
        let error = store
            .open_attempt(workspace_id)
            .expect_err("service workspace symlink must fail");
        assert!(error.to_string().contains("must not be a symbolic link"));

        let sidecar_id = RunId::new();
        let sidecar_attempt = store
            .prepare_managed(sidecar_id, backend())
            .expect("managed attempt");
        let sidecar = sidecar_attempt.participant_stdout_path(NativeParticipant::ManagedService);
        let outside_file = state.path().join("outside-file");
        fs::write(&outside_file, b"unchanged").expect("outside file");
        drop(sidecar_attempt);
        fs::remove_file(&sidecar).expect("remove service sidecar");
        symlink(&outside_file, &sidecar).expect("symlink service sidecar");
        let error = store
            .open_attempt(sidecar_id)
            .expect_err("service sidecar symlink must fail");
        assert!(error.to_string().contains("must not be a symbolic link"));
        assert_eq!(fs::read(outside_file).expect("outside bytes"), b"unchanged");
    }

    fn descriptor(digit: char) -> OciDescriptor {
        OciDescriptor {
            digest: Digest::parse(format!("sha256:{}", digit.to_string().repeat(64)))
                .expect("digest"),
            size: 123,
            media_type: "application/vnd.oci.image.manifest.v1+json".to_owned(),
        }
    }

    fn stored(bytes: &[u8]) -> StoredBytes {
        StoredBytes::Available {
            digest: digest_bytes(bytes),
            size: u64::try_from(bytes.len()).expect("size"),
        }
    }

    fn exited_process() -> ProcessSlot {
        ProcessSlot::available(ProcessFacts {
            terminal_outcome: ProcessOutcome::ProcessExited,
            exit_code: Some(0),
            started_at: None,
            ended_at: None,
            oom_killed: None,
            backend_error: None,
        })
    }

    fn mode(path: &Path) -> u32 {
        fs::symlink_metadata(path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777
    }
}
