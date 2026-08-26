use run_protocol::{EngineError, OperationError, OperationStage};

use super::time::wall_clock_now;

pub(super) fn operation_error(
    stage: OperationStage,
    message: impl Into<String>,
    code: Option<i64>,
) -> OperationError {
    OperationError::new(wall_clock_now(), stage, message, code)
        .expect("operation messages are non-empty")
}

pub(super) fn output_internal(error: impl std::fmt::Display) -> EngineError {
    EngineError::internal(format!(
        "failed to construct trustworthy RunOutput: {error}"
    ))
}
