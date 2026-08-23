//! Native Linux execution of a Run.
//!
//! `execution` owns the platform-neutral shape of a Run: acceptance, the
//! terminal Run Record, and the exit-status contract. Everything that is
//! specific to running an OCI bundle through runc on this host lives here,
//! behind a single `#[cfg(target_os = "linux")]` on the `mod native;`
//! declaration. Nothing inside needs a per-item platform gate.
//!
//! The work is split by what each part owns rather than by mechanism:
//!
//! - `scope` owns what outlives a single step of a Run: the recovery attempt
//!   and the Run network.
//! - `participant` owns one OCI bundle end to end: prepare, run, capture.
//! - `managed` owns the two-participant topology: readiness, concurrent
//!   observation, and the terminal transaction that relates both.
//!
//! Nothing else belongs here. This module wires the three together, re-exports
//! what `execution` needs, and declares the one constructor that says a
//! `Runner` is native.
//!
//! The Linux-only halves of `Runner` are inherent methods declared in these
//! modules rather than in the parent, so the parent file describes only the
//! path that exists on every host.

mod managed;
mod participant;
mod scope;

pub use managed::{ManagedPrimaryInput, ManagedServiceInput};
pub(in crate::execution) use participant::PreparedNativeBackend;
pub(in crate::execution) use scope::RunScope;

use crate::image::ImageService;
use crate::native::backend::NativeBackend;
use crate::storage::RunDatabase;

use super::{Runner, RunnerBackend};

impl<'a> Runner<'a> {
    #[must_use]
    pub(crate) const fn native(
        database: &'a RunDatabase,
        images: &'a ImageService,
        backend: &'a NativeBackend,
    ) -> Self {
        Self {
            database,
            images,
            backend: RunnerBackend::Native(backend),
        }
    }
}
