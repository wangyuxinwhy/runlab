use run_protocol::ProgramId;

/// A coarse execution boundary exposed by an Engine implementation while a Run is active.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EngineStage {
    /// At least one Program has entered the OCI execution interval.
    Executing,
    /// The Engine has entered a bounded stop flow for active Programs.
    Stopping,
    /// Program writers are stopped and final environments are being captured.
    Capturing,
}

/// One Program output pipe.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ProgramStream {
    /// The Program standard output pipe.
    Stdout,
    /// The Program standard error pipe.
    Stderr,
}

/// Best-effort, invocation-scoped observations from an Engine implementation.
///
/// Observations do not replace or extend [`run_protocol::RunOutput`]. Implementations
/// may discard observations; observers must return promptly and must not panic.
pub trait EngineObserver: Send + Sync {
    /// Reports that the Engine entered a coarse execution boundary.
    fn stage(&self, _stage: EngineStage) {}

    /// Reports bytes drained from one Program output pipe.
    fn program_output(
        &self,
        _program_id: &ProgramId,
        _stream: ProgramStream,
        _byte_offset: u64,
        _bytes: &[u8],
    ) {
    }

    /// Reports that one Program output pipe cannot produce more bytes.
    fn program_stream_closed(&self, _program_id: &ProgramId, _stream: ProgramStream) {}
}

#[cfg(target_os = "linux")]
pub(crate) struct IgnoreObserver;

#[cfg(target_os = "linux")]
impl EngineObserver for IgnoreObserver {}
