use super::{
    AcceptedRunRecord, DockerImageCheckoutCreateResult, DockerImageMaterializeResult,
    ImageCatalogListResult, ImageCatalogRemoveResult, ImageCatalogSetResult,
    ImageCatalogShowResult, ImageDiffResult, ImageExportResult, ImageFileGetResult,
    ImageImportResult, ImageInspectResult, ImageOperationResult, ImagePullResult,
    ManagedServiceCheckResult, Result, RunDiffResult, RunListResult, RunReconcileBatchResult,
    RunReconcileResult, RunStartResult, RunStreamGetResult, RunVerifyResult,
    RuntimeConfigCheckResult, RuntimeConfigCreateResult, SchemaCommand, SchemaListResult,
    SchemaName, StateGcApplyResult, StateGcPlan, StateGcPlanResult, StateVerifyResult,
    TerminalRunRecord, emit,
};

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
