mod aggregate;
mod operation;
mod process;
mod stdio;
mod stop;

pub use aggregate::{
    ExecutionInterval, ExecutionOutput, FinalEnvironment, ProgramOutput, RunOutput,
};
pub use operation::{
    Availability, Explanation, OperationError, OperationReport, OperationStage, OperationStatus,
};
pub use process::{CreateFacts, ProcessResult, StartFacts};
pub use stdio::{MAX_CAPTURED_STREAM_BYTES, StdinOutput, StdinWriteFacts, StreamFacts};
pub use stop::{StopAction, StopActionResult, StopSignal};

#[cfg(test)]
mod tests;
