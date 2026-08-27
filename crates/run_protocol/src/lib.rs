#![forbid(unsafe_code)]
#![warn(missing_docs)]

//! Data model and invariants for the Run Protocol.

mod error;
mod input;
mod json;
mod oci;
mod output;

pub use error::{EngineError, InputError, InputPath, OutputError};
pub use input::{
    MAX_STDIN_BYTES, Network, ProgramId, ProgramInput, RunInput, RuntimeConfig, SecretValue,
    Secrets,
};
pub use oci::{ImageDescriptor, ImageDescriptorError};
pub use output::{
    Availability, CreateFacts, ExecutionInterval, ExecutionOutput, Explanation,
    MAX_CAPTURED_STREAM_BYTES, OperationError, OperationReport, OperationStage, OperationStatus,
    ProcessResult, ProgramOutput, RunOutput, StartFacts, StdinOutput, StdinWriteFacts, StopAction,
    StopActionResult, StopSignal, StreamFacts,
};
