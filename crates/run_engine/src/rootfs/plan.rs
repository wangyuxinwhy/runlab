use super::{FsPath, Metadata};

#[derive(Debug)]
pub(super) enum LayerKind {
    Regular {
        content: tempfile::TempPath,
        size: u64,
    },
    Directory,
    Symlink(Vec<u8>),
    Hardlink(FsPath),
    Fifo,
    Character {
        major: u32,
        minor: u32,
    },
    Block {
        major: u32,
        minor: u32,
    },
}

#[derive(Debug)]
pub(super) struct LayerEntry {
    pub(super) path: FsPath,
    pub(super) metadata: Metadata,
    pub(super) kind: LayerKind,
}

#[derive(Debug, Default)]
pub(super) struct LayerPlan {
    pub(super) whiteouts: Vec<FsPath>,
    pub(super) opaques: Vec<FsPath>,
    pub(super) entries: Vec<LayerEntry>,
}
