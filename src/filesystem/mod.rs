#[cfg(any(test, target_os = "linux"))]
mod capture;
mod inventory;
#[cfg(any(test, target_os = "linux"))]
mod ownership;
mod path;

#[cfg(target_os = "linux")]
pub(crate) use capture::{CapturedTree, TreeCapture};

pub(crate) use inventory::{EntryKind, FsEntry, Inventory, Metadata, Timestamp, Xattrs};
#[cfg(any(test, target_os = "linux"))]
pub(crate) use ownership::FilesystemOwnership;
pub(crate) use path::{FsPath, FsPathError};
