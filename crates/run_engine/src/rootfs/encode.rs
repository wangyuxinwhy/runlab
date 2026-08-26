use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::File;
use std::io::Seek as _;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use oci_spec::image::Digest;
use tar::{Builder, EntryType, Header, HeaderMode};

use super::diff::ChangeSet;
use super::digest::copy_and_digest;
use super::xattr::{encode_base64, validate_pax_xattr_name};
use super::{
    EntryKind, FsEntry, FsPath, Metadata, RootfsLimits, Timestamp, path_buf, usize_to_u64,
};

pub(super) fn encode_layer(
    changes: &ChangeSet,
    contents: &BTreeMap<String, tempfile::TempPath>,
    workspace: &Path,
    limits: RootfsLimits,
) -> Result<(tempfile::TempPath, u64, Digest)> {
    let mut output = tempfile::NamedTempFile::new_in(workspace)?;
    {
        let bounded = BoundedWriter::new(output.as_file_mut(), limits.tar_bytes);
        let mut builder = Builder::new(bounded);
        builder.mode(HeaderMode::Deterministic);
        if let Some(metadata) = &changes.root {
            append_entry(
                &mut builder,
                &FsPath(Box::default()),
                &FsEntry {
                    metadata: metadata.clone(),
                    kind: EntryKind::Directory,
                },
                contents,
                true,
            )?;
        }
        for directory in &changes.opaques {
            let path = directory.join(b".wh..wh..opq", limits.path_bytes)?;
            append_whiteout(&mut builder, &path)?;
        }
        for removal in &changes.removals {
            if removal.basename().starts_with(b".wh.") {
                bail!(
                    "filesystem removal cannot be encoded as OCI whiteout: {}",
                    removal.display()
                );
            }
            let mut name = b".wh.".to_vec();
            name.extend_from_slice(removal.basename());
            let path = removal.parent().join(&name, limits.path_bytes)?;
            append_whiteout(&mut builder, &path)?;
        }
        for (path, entry) in &changes.entries {
            if path.basename().starts_with(b".wh.") {
                bail!(
                    "filesystem path uses reserved OCI whiteout name: {}",
                    path.display()
                );
            }
            append_entry(&mut builder, path, entry, contents, false)?;
        }
        builder.finish()?;
        let bounded = builder.into_inner()?;
        if bounded.written > limits.tar_bytes {
            unreachable!("bounded writer prevents oversized output");
        }
    }
    output.as_file_mut().sync_all()?;
    output.as_file_mut().rewind()?;
    let (digest, size) = copy_and_digest(output.as_file_mut(), std::io::sink(), None)?;
    output.as_file_mut().rewind()?;
    Ok((output.into_temp_path(), size, digest))
}

pub(super) fn append_whiteout<W: std::io::Write>(
    builder: &mut Builder<W>,
    path: &FsPath,
) -> Result<()> {
    let mut header = tar_header(0, 0, 0, 0, 0, EntryType::Regular)?;
    builder.append_data(&mut header, path_buf(path), std::io::empty())?;
    Ok(())
}

pub(super) fn append_entry<W: std::io::Write>(
    builder: &mut Builder<W>,
    path: &FsPath,
    entry: &FsEntry,
    contents: &BTreeMap<String, tempfile::TempPath>,
    root: bool,
) -> Result<()> {
    append_pax_metadata(builder, &entry.metadata)?;
    let archive_path = if root {
        PathBuf::from(".")
    } else {
        path_buf(path)
    };
    let base_mtime = u64::try_from(entry.metadata.mtime.seconds).unwrap_or(0);
    match &entry.kind {
        EntryKind::Regular {
            digest,
            size,
            hardlink: None,
        } => {
            let content = contents
                .get(&digest.to_string())
                .with_context(|| format!("captured content is unavailable: {digest}"))?;
            let mut source = File::open(content)?;
            let mut header = tar_header(
                *size,
                entry.metadata.mode,
                entry.metadata.uid,
                entry.metadata.gid,
                base_mtime,
                EntryType::Regular,
            )?;
            builder.append_data(&mut header, archive_path, &mut source)?;
        }
        EntryKind::Regular {
            hardlink: Some(target),
            ..
        } => {
            let mut header = tar_header(
                0,
                entry.metadata.mode,
                entry.metadata.uid,
                entry.metadata.gid,
                base_mtime,
                EntryType::Link,
            )?;
            builder.append_link(&mut header, archive_path, path_buf(target))?;
        }
        EntryKind::Directory => {
            let mut header = tar_header(
                0,
                entry.metadata.mode,
                entry.metadata.uid,
                entry.metadata.gid,
                base_mtime,
                EntryType::Directory,
            )?;
            builder.append_data(&mut header, archive_path, std::io::empty())?;
        }
        EntryKind::Symlink(target) => {
            if target.contains(&0) {
                bail!("captured symlink target contains NUL: {}", path.display());
            }
            let mut header = tar_header(
                0,
                entry.metadata.mode,
                entry.metadata.uid,
                entry.metadata.gid,
                base_mtime,
                EntryType::Symlink,
            )?;
            builder.append_link(
                &mut header,
                archive_path,
                Path::new(OsStr::from_bytes(target)),
            )?;
        }
        EntryKind::Fifo => append_special(builder, archive_path, entry, EntryType::Fifo, None)?,
        EntryKind::Character { major, minor } => append_special(
            builder,
            archive_path,
            entry,
            EntryType::Char,
            Some((*major, *minor)),
        )?,
        EntryKind::Block { major, minor } => append_special(
            builder,
            archive_path,
            entry,
            EntryType::Block,
            Some((*major, *minor)),
        )?,
    }
    Ok(())
}

pub(super) fn append_special<W: std::io::Write>(
    builder: &mut Builder<W>,
    path: PathBuf,
    entry: &FsEntry,
    entry_type: EntryType,
    device: Option<(u32, u32)>,
) -> Result<()> {
    let mut header = tar_header(
        0,
        entry.metadata.mode,
        entry.metadata.uid,
        entry.metadata.gid,
        u64::try_from(entry.metadata.mtime.seconds).unwrap_or(0),
        entry_type,
    )?;
    if let Some((major, minor)) = device {
        header.set_device_major(major)?;
        header.set_device_minor(minor)?;
        header.set_cksum();
    }
    builder.append_data(&mut header, path, std::io::empty())?;
    Ok(())
}

pub(super) fn append_pax_metadata<W: std::io::Write>(
    builder: &mut Builder<W>,
    metadata: &Metadata,
) -> Result<()> {
    let mut records = Vec::<(Vec<u8>, Vec<u8>)>::new();
    if metadata.mtime.seconds < 0 || metadata.mtime.nanos != 0 {
        records.push((
            b"mtime".to_vec(),
            pax_timestamp(metadata.mtime).into_bytes(),
        ));
    }
    for (name, value) in &metadata.xattrs {
        validate_pax_xattr_name(name)?;
        let mut schily = b"SCHILY.xattr.".to_vec();
        schily.extend_from_slice(name);
        records.push((schily, value.to_vec()));
        let mut libarchive = b"LIBARCHIVE.xattr.".to_vec();
        libarchive.extend_from_slice(name);
        records.push((libarchive, encode_base64(value).into_bytes()));
    }
    if records.is_empty() {
        return Ok(());
    }
    let mut data = Vec::new();
    for (key, value) in records {
        let remainder = key.len() + value.len() + 3;
        let mut digits = 1;
        loop {
            let length = remainder + digits;
            let actual_digits = length.to_string().len();
            if actual_digits == digits {
                data.extend_from_slice(length.to_string().as_bytes());
                data.push(b' ');
                data.extend_from_slice(&key);
                data.push(b'=');
                data.extend_from_slice(&value);
                data.push(b'\n');
                break;
            }
            digits = actual_digits;
        }
    }
    let mut header = tar_header(usize_to_u64(data.len()), 0o644, 0, 0, 0, EntryType::XHeader)?;
    header.set_path("PaxHeaders/runlab")?;
    header.set_cksum();
    builder.append(&header, data.as_slice())?;
    Ok(())
}

pub(super) fn tar_header(
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

pub(super) fn pax_timestamp(timestamp: Timestamp) -> String {
    let total = i128::from(timestamp.seconds) * 1_000_000_000 + i128::from(timestamp.nanos);
    let negative = total < 0;
    let absolute = total.unsigned_abs();
    let seconds = absolute / 1_000_000_000;
    let fraction = absolute % 1_000_000_000;
    if fraction == 0 {
        return format!("{}{seconds}", if negative { "-" } else { "" });
    }
    let fraction = format!("{fraction:09}").trim_end_matches('0').to_owned();
    format!("{}{seconds}.{fraction}", if negative { "-" } else { "" })
}

pub(super) struct BoundedWriter<W> {
    inner: W,
    limit: u64,
    written: u64,
}

impl<W> BoundedWriter<W> {
    fn new(inner: W, limit: u64) -> Self {
        Self {
            inner,
            limit,
            written: 0,
        }
    }
}

impl<W: std::io::Write> std::io::Write for BoundedWriter<W> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let remaining = self.limit.saturating_sub(self.written);
        if usize_to_u64(buffer.len()) > remaining {
            return Err(std::io::Error::other(
                "deterministic Layer exceeds tar byte limit",
            ));
        }
        let written = self.inner.write(buffer)?;
        self.written = self.written.saturating_add(usize_to_u64(written));
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}
