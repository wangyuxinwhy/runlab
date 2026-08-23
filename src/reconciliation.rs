use schemars::JsonSchema;
use serde::Serialize;

use crate::core::RunId;

#[derive(Debug, Serialize, JsonSchema)]
pub struct RunReconcileResult {
    pub schema_version: u32,
    pub run_id: RunId,
    pub status: &'static str,
    pub terminalized: bool,
    pub actions: Vec<&'static str>,
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
