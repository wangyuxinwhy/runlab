//! `runlab schema`: the public JSON schema of every document this CLI prints.
//!
//! Each name maps to the exact type its command emits, so a schema cannot drift
//! from the output it describes without failing to compile.

use anyhow::Result;
use clap::{Subcommand, ValueEnum};
use schemars::JsonSchema;
use serde::Serialize;

use crate::core::{AcceptedRunRecord, TerminalRunRecord};
use crate::execution::RunStartResult;
use crate::maintenance::{
    RunVerifyResult, StateGcApplyResult, StateGcPlan, StateGcPlanResult, StateVerifyResult,
};
use crate::reconciliation::{RunReconcileBatchResult, RunReconcileResult};

use super::emit;
use super::image::{
    DockerImageCheckoutCreateResult, DockerImageMaterializeResult, ImageCatalogListResult,
    ImageCatalogRemoveResult, ImageCatalogSetResult, ImageCatalogShowResult, ImageDiffResult,
    ImageExportResult, ImageFileGetResult, ImageImportResult, ImageInspectResult,
    ImageOperationResult, ImagePullResult,
};
use super::inputs::{
    ManagedServiceCheckResult, RuntimeConfigCheckResult, RuntimeConfigCreateResult,
};
use super::run::{RunDiffResult, RunListResult, RunStreamGetResult};

pub(super) fn run_schema(command: SchemaCommand) -> Result<u8> {
    match command {
        SchemaCommand::List => emit(&SchemaListResult {
            schema_version: 1,
            schemas: SchemaName::ALL.to_vec(),
        })?,
        SchemaCommand::Show { name } => match name {
            SchemaName::AcceptedRunRecord => emit(&schemars::schema_for!(AcceptedRunRecord))?,
            SchemaName::TerminalRunRecord => emit(&schemars::schema_for!(TerminalRunRecord))?,
            SchemaName::RunStartResult => emit(&schemars::schema_for!(RunStartResult))?,
            SchemaName::RunListResult => emit(&schemars::schema_for!(RunListResult))?,
            SchemaName::RunDiffResult => emit(&schemars::schema_for!(RunDiffResult))?,
            SchemaName::RunStreamGetResult => emit(&schemars::schema_for!(RunStreamGetResult))?,
            SchemaName::RunReconcileResult => emit(&schemars::schema_for!(RunReconcileResult))?,
            SchemaName::RunReconcileBatchResult => {
                emit(&schemars::schema_for!(RunReconcileBatchResult))?;
            }
            SchemaName::RunVerifyResult => emit(&schemars::schema_for!(RunVerifyResult))?,
            SchemaName::ImageOperationResult => emit(&schemars::schema_for!(ImageOperationResult))?,
            SchemaName::ImageInspectResult => emit(&schemars::schema_for!(ImageInspectResult))?,
            SchemaName::ImageImportResult => emit(&schemars::schema_for!(ImageImportResult))?,
            SchemaName::ImagePullResult => emit(&schemars::schema_for!(ImagePullResult))?,
            SchemaName::ImageCatalogListResult => {
                emit(&schemars::schema_for!(ImageCatalogListResult))?;
            }
            SchemaName::ImageCatalogShowResult => {
                emit(&schemars::schema_for!(ImageCatalogShowResult))?;
            }
            SchemaName::ImageCatalogSetResult => {
                emit(&schemars::schema_for!(ImageCatalogSetResult))?;
            }
            SchemaName::ImageCatalogRemoveResult => {
                emit(&schemars::schema_for!(ImageCatalogRemoveResult))?;
            }
            SchemaName::ImageDiffResult => emit(&schemars::schema_for!(ImageDiffResult))?,
            SchemaName::ImageExportResult => emit(&schemars::schema_for!(ImageExportResult))?,
            SchemaName::ImageFileGetResult => emit(&schemars::schema_for!(ImageFileGetResult))?,
            SchemaName::DockerImageMaterializeResult => {
                emit(&schemars::schema_for!(DockerImageMaterializeResult))?;
            }
            SchemaName::DockerImageCheckoutCreateResult => {
                emit(&schemars::schema_for!(DockerImageCheckoutCreateResult))?;
            }
            SchemaName::RuntimeConfigCreateResult => {
                emit(&schemars::schema_for!(RuntimeConfigCreateResult))?;
            }
            SchemaName::RuntimeConfigCheckResult => {
                emit(&schemars::schema_for!(RuntimeConfigCheckResult))?;
            }
            SchemaName::ManagedServiceCheckResult => {
                emit(&schemars::schema_for!(ManagedServiceCheckResult))?;
            }
            SchemaName::StateVerifyResult => emit(&schemars::schema_for!(StateVerifyResult))?,
            SchemaName::StateGcPlan => emit(&schemars::schema_for!(StateGcPlan))?,
            SchemaName::StateGcPlanResult => {
                emit(&schemars::schema_for!(StateGcPlanResult))?;
            }
            SchemaName::StateGcApplyResult => {
                emit(&schemars::schema_for!(StateGcApplyResult))?;
            }
            SchemaName::VmStatus => emit(&schemars::schema_for!(crate::managed_vm::VmStatus))?,
            SchemaName::VmInstallResult => {
                emit(&schemars::schema_for!(crate::managed_vm::VmInstallResult))?;
            }
            SchemaName::VmOperationResult => {
                emit(&schemars::schema_for!(crate::managed_vm::VmOperationResult))?;
            }
            SchemaName::VmOperationStatus => {
                emit(&schemars::schema_for!(crate::managed_vm::VmOperationStatus))?;
            }
            SchemaName::VmCancelResult => {
                emit(&schemars::schema_for!(crate::managed_vm::VmCancelResult))?;
            }
            SchemaName::VmDiscardResult => {
                emit(&schemars::schema_for!(crate::managed_vm::VmDiscardResult))?;
            }
            SchemaName::SchemaListResult => emit(&schemars::schema_for!(SchemaListResult))?,
        },
    }
    Ok(0)
}

#[derive(Debug, Clone, Copy, Subcommand)]
pub(super) enum SchemaCommand {
    /// List the versioned public JSON schema names.
    List,
    /// Print one public JSON Schema.
    Show {
        #[arg(value_enum, default_value_t = SchemaName::TerminalRunRecord)]
        name: SchemaName,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum, Serialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub(super) enum SchemaName {
    AcceptedRunRecord,
    TerminalRunRecord,
    RunStartResult,
    RunListResult,
    RunDiffResult,
    RunStreamGetResult,
    RunReconcileResult,
    RunReconcileBatchResult,
    RunVerifyResult,
    ImageOperationResult,
    ImageInspectResult,
    ImageImportResult,
    ImagePullResult,
    ImageCatalogListResult,
    ImageCatalogShowResult,
    ImageCatalogSetResult,
    ImageCatalogRemoveResult,
    ImageDiffResult,
    ImageExportResult,
    ImageFileGetResult,
    DockerImageMaterializeResult,
    DockerImageCheckoutCreateResult,
    RuntimeConfigCreateResult,
    RuntimeConfigCheckResult,
    ManagedServiceCheckResult,
    StateVerifyResult,
    StateGcPlan,
    StateGcPlanResult,
    StateGcApplyResult,
    VmStatus,
    VmInstallResult,
    VmOperationResult,
    VmOperationStatus,
    VmCancelResult,
    VmDiscardResult,
    SchemaListResult,
}

impl SchemaName {
    const ALL: [Self; 36] = [
        Self::AcceptedRunRecord,
        Self::TerminalRunRecord,
        Self::RunStartResult,
        Self::RunListResult,
        Self::RunDiffResult,
        Self::RunStreamGetResult,
        Self::RunReconcileResult,
        Self::RunReconcileBatchResult,
        Self::RunVerifyResult,
        Self::ImageOperationResult,
        Self::ImageInspectResult,
        Self::ImageImportResult,
        Self::ImagePullResult,
        Self::ImageCatalogListResult,
        Self::ImageCatalogShowResult,
        Self::ImageCatalogSetResult,
        Self::ImageCatalogRemoveResult,
        Self::ImageDiffResult,
        Self::ImageExportResult,
        Self::ImageFileGetResult,
        Self::DockerImageMaterializeResult,
        Self::DockerImageCheckoutCreateResult,
        Self::RuntimeConfigCreateResult,
        Self::RuntimeConfigCheckResult,
        Self::ManagedServiceCheckResult,
        Self::StateVerifyResult,
        Self::StateGcPlan,
        Self::StateGcPlanResult,
        Self::StateGcApplyResult,
        Self::VmStatus,
        Self::VmInstallResult,
        Self::VmOperationResult,
        Self::VmOperationStatus,
        Self::VmCancelResult,
        Self::VmDiscardResult,
        Self::SchemaListResult,
    ];
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct SchemaListResult {
    schema_version: u32,
    schemas: Vec<SchemaName>,
}
