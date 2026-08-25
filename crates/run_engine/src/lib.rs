#![forbid(unsafe_code)]
#![warn(missing_docs)]

//! Execution interfaces and implementations for the Run Protocol.

mod cancellation;
mod content;

pub use cancellation::CancellationToken;
pub use content::{ContentError, ContentErrorKind, OciContent, OciContentStore};

use run_protocol::{EngineError, RunInput, RunOutput};

/// Synchronous execution boundary for one complete Run Protocol invocation.
///
/// Implementations must validate the complete input and their capabilities
/// before starting any Program. The same instance may be called concurrently;
/// resources and cancellation state belong to each invocation.
pub trait RunEngine: Send + Sync {
    /// Executes every Program and returns the trustworthy facts collected for
    /// this invocation.
    ///
    /// Workload outcomes such as nonzero exit, signal termination, timeout,
    /// cancellation, or an OCI lifecycle failure belong in [`RunOutput`]. An
    /// [`EngineError`] means no structurally complete and trustworthy output
    /// can be returned.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError`] when input or required content is invalid or
    /// unavailable, the implementation cannot faithfully execute the input, or
    /// an internal failure prevents a trustworthy [`RunOutput`].
    fn run(
        &self,
        input: RunInput,
        cancellation: CancellationToken,
    ) -> Result<RunOutput, EngineError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_engine_contract<T: RunEngine>() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<T>();
    }

    #[test]
    fn run_engine_requires_concurrent_reuse() {
        struct TestEngine;

        impl RunEngine for TestEngine {
            fn run(
                &self,
                _input: RunInput,
                _cancellation: CancellationToken,
            ) -> Result<RunOutput, EngineError> {
                Err(EngineError::internal(
                    "test engine has no execution mechanism",
                ))
            }
        }

        assert_engine_contract::<TestEngine>();
    }
}
