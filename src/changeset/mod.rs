//! Turning a before/after filesystem pair into one OCI Layer.
//!
//! `diff` computes what changed, expressing removals as OCI whiteouts;
//! `layer` encodes that change set as a deterministic, compressed tar. The two
//! are separate because the comparison is a decision about semantics and the
//! encoding is a decision about bytes.

mod diff;
mod layer;

pub(crate) use diff::ChangeSet;
#[cfg(any(test, target_os = "linux"))]
pub(crate) use diff::compare;
pub(crate) use layer::{LayerEncoder, StagedLayer};
