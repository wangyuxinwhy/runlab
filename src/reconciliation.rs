//! The result vocabulary of native Run reconciliation.
//!
//! These types live outside `native` because `cli` names them on every host,
//! while reconciliation itself only runs on Linux.
//!
//! `status` and `action` are enums rather than strings so `runlab schema show`
//! lists the values a caller can branch on. The wire format is unchanged: each
//! variant serializes to the `snake_case` name it always had.

use schemars::JsonSchema;
use serde::Serialize;

use crate::core::RunId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "reconciliation produces these on Linux only")
)]
pub enum RunReconcileStatus {
    /// The Run was already terminal; nothing was left to reconcile.
    AlreadyTerminal,
    /// A terminal Run still owned a recovery attempt, which was released.
    CleanedTerminalAttempt,
    /// The attempt never reached acceptance, so no Run Record exists.
    DiscardedPreAcceptance,
    /// The attempt was discarded before any Final Image was published.
    DiscardedPrepublication,
    /// The Run was terminalized and its resources released.
    Reconciled,
    /// The Run was terminalized, but releasing its resources did not complete.
    TerminalizedCleanupPending,
    /// A dry run: these are the actions reconciliation would take.
    Planned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "reconciliation produces these on Linux only")
)]
pub enum RunReconcileAction {
    // Planned work, reported by a dry run.
    AttemptRemove,
    StagingAttemptRemove,
    ResourceCleanup,
    RuntimeCleanup,
    PrimaryRuntimeCleanup,
    ManagedRuntimeCleanup,
    ManagedResolverProjectionCleanup,
    OverlayUnmount,
    PrimaryOverlayUnmount,
    ManagedOverlayUnmount,
    SharedNetworkCleanup,
    RunTerminalize,

    // Work that was performed.
    AttemptRemoved,
    StagingAttemptRemoved,
    RunTerminalized,
    CgroupRemoved,
    FinalImagePublished,
    OverlayUnmounted,
    RecoveryCaptureStarted,
    ResolverProjectionCleanup,
    ResolverProjectionRemoved,
    RuntimeDeleted,
    RuntimeRootRemoved,
    ManagedCgroupRemoved,
    ManagedFinalImagePublished,
    ManagedOverlayUnmounted,
    ManagedRecoveryCaptureStarted,
    ManagedResolverProjectionRemoved,
    ManagedRuntimeDeleted,
    ManagedRuntimeRootRemoved,
    RunNetworkCleanupComplete,
    RunNetworkCleanupDeferred,
    RunNetworkEgressRemoved,
    RunNetworkHolderAbsent,
    RunNetworkHolderStopped,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct RunReconcileResult {
    pub schema_version: u32,
    pub run_id: RunId,
    pub status: RunReconcileStatus,
    pub terminalized: bool,
    pub actions: Vec<RunReconcileAction>,
    pub resources_absent: bool,
    pub cleanup_errors: Vec<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct RunReconcileBatchResult {
    pub schema_version: u32,
    pub dry_run: bool,
    pub items: Vec<RunReconcileBatchItem>,
    pub failed: usize,
    pub next_after: Option<RunId>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct RunReconcileBatchItem {
    pub run_id: RunId,
    pub outcome: RunReconcileBatchOutcome,
}

#[derive(Debug, Serialize, JsonSchema)]
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "state-wide reconciliation executes on Linux only")
)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RunReconcileBatchOutcome {
    Completed { result: RunReconcileResult },
    Failed { error: String },
}
