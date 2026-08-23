//! Filesystem primitives that treat paths and metadata as bytes.
//!
//! Paths are raw bytes, not `str`: an Image can legitimately contain names that
//! are not UTF-8, and losing them silently would change what a Run sees. On top
//! of that sit a semantic inventory, a content spool, a length-aware PAX codec,
//! and a deterministic tar writer.
//!
//! Determinism is the point. The same tree always produces the same archive
//! bytes, which is what makes a captured Layer content-addressable.

mod archive;
#[cfg(any(test, target_os = "linux"))]
mod capture;
mod content;
mod inventory;
#[cfg(any(test, target_os = "linux"))]
mod ownership;
mod path;
pub(crate) mod pax;

#[cfg(target_os = "linux")]
pub(crate) use capture::{CapturedTree, TreeCapture};

pub(crate) use archive::FilesystemTarWriter;
#[cfg(test)]
pub(crate) use archive::pax_timestamp;
pub(crate) use content::ContentStore;
pub(crate) use inventory::{EntryKind, FsEntry, Inventory, Metadata, Timestamp, Xattrs};
#[cfg(any(test, target_os = "linux"))]
pub(crate) use ownership::FilesystemOwnership;
pub(crate) use path::{FsPath, FsPathError};
