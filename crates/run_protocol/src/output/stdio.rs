use std::sync::Arc;

use super::{OperationError, OperationReport};
use crate::OutputError;

/// Maximum retained bytes for each Program output stream.
pub const MAX_CAPTURED_STREAM_BYTES: usize = 100 * 1024 * 1024;
impl OperationReport<StdinWriteFacts> {
    /// Records a failed stdin write while retaining the observed byte count.
    #[must_use]
    pub fn failed_with_facts(
        facts: StdinWriteFacts,
        first_error: OperationError,
        additional_errors: impl IntoIterator<Item = OperationError>,
    ) -> Self {
        Self::failed(first_error, additional_errors).with_facts(facts)
    }

    /// Records an indeterminate stdin write while retaining an observed byte count.
    ///
    /// # Errors
    ///
    /// Returns [`OutputError`] when `reason` is empty or whitespace.
    pub fn unknown_with_facts(
        facts: StdinWriteFacts,
        reason: impl Into<String>,
        errors: impl IntoIterator<Item = OperationError>,
    ) -> Result<Self, OutputError> {
        Ok(Self::unknown(reason, errors)?.with_facts(facts))
    }
}

impl OperationReport<StreamFacts> {
    /// Records a failed stream read while retaining bytes observed before failure.
    #[must_use]
    pub fn failed_with_facts(
        facts: StreamFacts,
        first_error: OperationError,
        additional_errors: impl IntoIterator<Item = OperationError>,
    ) -> Self {
        Self::failed(first_error, additional_errors).with_facts(facts)
    }

    /// Records an indeterminate stream read while retaining already observed facts.
    ///
    /// # Errors
    ///
    /// Returns [`OutputError`] when `reason` is empty or whitespace.
    pub fn unknown_with_facts(
        facts: StreamFacts,
        reason: impl Into<String>,
        errors: impl IntoIterator<Item = OperationError>,
    ) -> Result<Self, OutputError> {
        Ok(Self::unknown(reason, errors)?.with_facts(facts))
    }
}
/// Number of input bytes accepted by the Program's stdin pipe.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StdinWriteFacts {
    bytes_written: u64,
}

impl StdinWriteFacts {
    /// Records the exact count accepted by the standard-input pipe.
    #[must_use]
    pub fn new(bytes_written: u64) -> Self {
        Self { bytes_written }
    }

    #[must_use]
    /// Returns the number of accepted input bytes.
    pub fn bytes_written(&self) -> u64 {
        self.bytes_written
    }
}

/// Separate reports for writing stdin and closing the write end.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StdinOutput {
    write: OperationReport<StdinWriteFacts>,
    close: OperationReport<()>,
}

impl StdinOutput {
    /// Combines independent write and close operation reports.
    #[must_use]
    pub fn new(write: OperationReport<StdinWriteFacts>, close: OperationReport<()>) -> Self {
        Self { write, close }
    }

    #[must_use]
    /// Returns the standard-input write report.
    pub fn write(&self) -> &OperationReport<StdinWriteFacts> {
        &self.write
    }

    #[must_use]
    /// Returns the report for closing the write end.
    pub fn close(&self) -> &OperationReport<()> {
        &self.close
    }
}

/// Retained prefix and drain facts for one output stream.
#[derive(Clone, Eq, PartialEq)]
pub struct StreamFacts {
    bytes: Arc<[u8]>,
    omitted_after_limit: bool,
    eof: bool,
}

impl StreamFacts {
    /// Creates retained stream facts without allocating omitted bytes.
    ///
    /// # Errors
    ///
    /// Returns [`OutputError`] when retained bytes exceed the fixed limit or
    /// omission is claimed before that limit is reached.
    pub fn new(
        bytes: impl Into<Vec<u8>>,
        omitted_after_limit: bool,
        eof: bool,
    ) -> Result<Self, OutputError> {
        let bytes = bytes.into();
        validate_stream_shape(bytes.len(), omitted_after_limit)?;
        Ok(Self {
            bytes: Arc::from(bytes),
            omitted_after_limit,
            eof,
        })
    }

    #[must_use]
    /// Returns the retained prefix of raw stream bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    #[must_use]
    /// Returns whether bytes after the fixed limit were observed and omitted.
    pub fn omitted_after_limit(&self) -> bool {
        self.omitted_after_limit
    }

    #[must_use]
    /// Returns whether the Engine observed end-of-file.
    pub fn eof(&self) -> bool {
        self.eof
    }
}

pub(super) fn validate_stream_shape(
    retained_bytes: usize,
    omitted_after_limit: bool,
) -> Result<(), OutputError> {
    if retained_bytes > MAX_CAPTURED_STREAM_BYTES {
        return Err(OutputError::new(
            "stream.bytes",
            format!("retained {retained_bytes} bytes; the maximum is {MAX_CAPTURED_STREAM_BYTES}"),
        ));
    }
    if omitted_after_limit && retained_bytes != MAX_CAPTURED_STREAM_BYTES {
        return Err(OutputError::new(
            "stream.omitted_after_limit",
            "omitted output requires retaining exactly the fixed stream limit",
        ));
    }
    Ok(())
}

impl std::fmt::Debug for StreamFacts {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StreamFacts")
            .field("byte_len", &self.bytes.len())
            .field("omitted_after_limit", &self.omitted_after_limit)
            .field("eof", &self.eof)
            .finish()
    }
}
