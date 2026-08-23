use std::ffi::OsStr;
use std::fs::File;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use tar::{Builder, EntryType, Header, HeaderMode};

use super::pax::{self, DEFAULT_MAX_PAX_BYTES, PaxRecords};
use super::{ContentStore, EntryKind, FsEntry, FsPath, Metadata, Timestamp};

pub(crate) struct FilesystemTarWriter<'a> {
    builder: Builder<&'a mut File>,
}

impl<'a> FilesystemTarWriter<'a> {
    pub(crate) fn new(destination: &'a mut File) -> Self {
        let mut builder = Builder::new(destination);
        builder.mode(HeaderMode::Deterministic);
        Self { builder }
    }

    pub(crate) fn append_root(&mut self, metadata: &Metadata) -> Result<()> {
        append_metadata_extensions(&mut self.builder, metadata)?;
        append_directory(&mut self.builder, Path::new("."), "/", metadata)
    }

    pub(crate) fn append_empty_regular(&mut self, path: &FsPath) -> Result<()> {
        let mut header = header(0, 0, 0, 0, 0, EntryType::Regular)?;
        self.builder
            .append_data(&mut header, path_buf(path), std::io::empty())
            .with_context(|| format!("failed to write empty file {}", path.display()))
    }

    pub(crate) fn append_entry(
        &mut self,
        path: &FsPath,
        entry: &FsEntry,
        contents: &ContentStore,
    ) -> Result<()> {
        append_metadata_extensions(&mut self.builder, &entry.metadata)?;
        let mtime = base_mtime(entry.metadata.mtime);
        match &entry.kind {
            EntryKind::Regular {
                digest,
                size,
                hardlink: None,
            } => {
                let mut file = contents.open(digest, *size)?;
                let mut header = header(
                    *size,
                    entry.metadata.mode,
                    entry.metadata.uid,
                    entry.metadata.gid,
                    mtime,
                    EntryType::Regular,
                )?;
                self.builder
                    .append_data(&mut header, path_buf(path), &mut file)
                    .with_context(|| format!("failed to write regular file {}", path.display()))?;
            }
            EntryKind::Directory => append_directory(
                &mut self.builder,
                &path_buf(path),
                &path.display(),
                &entry.metadata,
            )?,
            EntryKind::Regular {
                hardlink: Some(target),
                ..
            } => {
                let mut header = header(
                    0,
                    entry.metadata.mode,
                    entry.metadata.uid,
                    entry.metadata.gid,
                    mtime,
                    EntryType::Link,
                )?;
                append_link(&mut self.builder, &mut header, path, target.as_bytes())
                    .with_context(|| format!("failed to write hardlink {}", path.display()))?;
            }
            EntryKind::Symlink { target } => {
                if target.contains(&0) {
                    bail!("symlink target contains NUL: {}", path.display());
                }
                let mut header = header(
                    0,
                    entry.metadata.mode,
                    entry.metadata.uid,
                    entry.metadata.gid,
                    mtime,
                    EntryType::Symlink,
                )?;
                append_link(&mut self.builder, &mut header, path, target)
                    .with_context(|| format!("failed to write symlink {}", path.display()))?;
            }
            EntryKind::Fifo => {
                append_special(&mut self.builder, path, entry, EntryType::Fifo, None)?;
            }
            EntryKind::Character { major, minor } => append_special(
                &mut self.builder,
                path,
                entry,
                EntryType::Char,
                Some((*major, *minor)),
            )?,
            EntryKind::Block { major, minor } => append_special(
                &mut self.builder,
                path,
                entry,
                EntryType::Block,
                Some((*major, *minor)),
            )?,
        }
        Ok(())
    }

    pub(crate) fn finish(&mut self) -> Result<()> {
        self.builder
            .finish()
            .context("failed to finish filesystem tar")
    }
}

fn append_directory(
    builder: &mut Builder<&mut File>,
    archive_path: &Path,
    display: &str,
    metadata: &Metadata,
) -> Result<()> {
    let mut header = header(
        0,
        metadata.mode,
        metadata.uid,
        metadata.gid,
        base_mtime(metadata.mtime),
        EntryType::Directory,
    )?;
    builder
        .append_data(&mut header, archive_path, std::io::empty())
        .with_context(|| format!("failed to write directory {display}"))
}

fn append_special(
    builder: &mut Builder<&mut File>,
    path: &FsPath,
    entry: &FsEntry,
    entry_type: EntryType,
    device: Option<(u32, u32)>,
) -> Result<()> {
    let mut header = header(
        0,
        entry.metadata.mode,
        entry.metadata.uid,
        entry.metadata.gid,
        base_mtime(entry.metadata.mtime),
        entry_type,
    )?;
    if let Some((major, minor)) = device {
        header.set_device_major(major)?;
        header.set_device_minor(minor)?;
        header.set_cksum();
    }
    builder
        .append_data(&mut header, path_buf(path), std::io::empty())
        .with_context(|| format!("failed to write special entry {}", path.display()))
}

fn append_link(
    builder: &mut Builder<&mut File>,
    header: &mut Header,
    path: &FsPath,
    target: &[u8],
) -> Result<()> {
    if header.set_link_name_literal(target).is_err() {
        append_gnu_long_link(builder, target)?;
    }
    builder
        .append_data(header, path_buf(path), std::io::empty())
        .map_err(Into::into)
}

fn append_gnu_long_link(builder: &mut Builder<&mut File>, target: &[u8]) -> Result<()> {
    let size = u64::try_from(target.len())?
        .checked_add(1)
        .context("GNU long-link target size overflow")?;
    let mut header = header(0, 0o644, 0, 0, 0, EntryType::GNULongLink)?;
    header.set_path("././@LongLink")?;
    header.set_size(size);
    header.set_cksum();
    let data = target.iter().copied().chain(std::iter::once(0));
    builder
        .append(&header, data.collect::<Vec<_>>().as_slice())
        .context("failed to write GNU long-link extension")
}

fn append_metadata_extensions(builder: &mut Builder<&mut File>, metadata: &Metadata) -> Result<()> {
    let mut records = PaxRecords::default();
    if metadata.mtime.seconds < 0 || metadata.mtime.nanos != 0 {
        let mtime = pax_timestamp(metadata.mtime);
        records.insert(b"mtime", mtime.as_bytes())?;
    }
    pax::insert_xattrs(&mut records, &metadata.xattrs)?;
    pax::append_header(builder, &records, DEFAULT_MAX_PAX_BYTES)
        .context("failed to write PAX metadata")
}

fn base_mtime(timestamp: Timestamp) -> u64 {
    u64::try_from(timestamp.seconds).unwrap_or(0)
}

pub(crate) fn pax_timestamp(timestamp: Timestamp) -> String {
    let nanos = i128::from(timestamp.seconds) * 1_000_000_000 + i128::from(timestamp.nanos);
    let negative = nanos < 0;
    let absolute = nanos.unsigned_abs();
    let seconds = absolute / 1_000_000_000;
    let fraction = absolute % 1_000_000_000;
    if fraction == 0 {
        return format!("{}{seconds}", if negative { "-" } else { "" });
    }
    let fraction = format!("{fraction:09}").trim_end_matches('0').to_owned();
    format!("{}{seconds}.{fraction}", if negative { "-" } else { "" })
}

fn header(
    size: u64,
    mode: u32,
    uid: u32,
    gid: u32,
    mtime: u64,
    entry_type: EntryType,
) -> Result<Header> {
    let mut header = Header::new_gnu();
    header.set_size(size);
    header.set_mode(mode);
    header.set_uid(u64::from(uid));
    header.set_gid(u64::from(gid));
    header.set_mtime(mtime);
    header.set_entry_type(entry_type);
    header.set_username("")?;
    header.set_groupname("")?;
    header.set_cksum();
    Ok(header)
}

fn path_buf(path: &FsPath) -> PathBuf {
    Path::new(OsStr::from_bytes(path.as_bytes())).to_path_buf()
}
