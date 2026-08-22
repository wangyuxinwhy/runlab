use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs::File;
use std::io::{Read, Seek, Write};
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use flate2::{Compression, GzBuilder};
use sha2::{Digest as _, Sha256};
use tar::{Builder, EntryType, Header, HeaderMode};
use tempfile::{NamedTempFile, TempDir};

use crate::core::{Digest, OCI_LAYER_GZIP, OciDescriptor};
use crate::filesystem::{EntryKind, FsEntry, FsPath};
use crate::oci::{OciLayout, digest_reader};
use crate::pax::{self, DEFAULT_MAX_PAX_BYTES, PaxRecords};

use super::ChangeSet;

const MAX_PATH_BYTES: u64 = 16 * 1024;

#[derive(Debug)]
pub(crate) struct ContentStore {
    #[cfg_attr(
        not(any(test, target_os = "linux")),
        allow(dead_code, reason = "production filesystem capture is Linux-only")
    )]
    directory: Option<TempDir>,
    paths: BTreeMap<Digest, PathBuf>,
}

impl ContentStore {
    pub(crate) fn new() -> Result<Self> {
        Self::create(tempfile::Builder::new().prefix("runlab-content-").tempdir())
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn new_in(parent: &Path) -> Result<Self> {
        Self::create(
            tempfile::Builder::new()
                .prefix("content-")
                .tempdir_in(parent),
        )
    }

    fn create(directory: std::io::Result<TempDir>) -> Result<Self> {
        Ok(Self {
            directory: Some(directory.context("failed to create changeset content store")?),
            paths: BTreeMap::new(),
        })
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn digest_only() -> Self {
        Self {
            directory: None,
            paths: BTreeMap::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn put_bytes(&mut self, bytes: &[u8]) -> Result<Digest> {
        self.put_reader(bytes).map(|(digest, _)| digest)
    }

    #[cfg_attr(
        not(any(test, target_os = "linux")),
        allow(dead_code, reason = "production filesystem capture is Linux-only")
    )]
    pub(crate) fn put_reader(&mut self, mut reader: impl Read) -> Result<(Digest, u64)> {
        let mut temporary = self
            .directory
            .as_ref()
            .map(|directory| NamedTempFile::new_in(directory.path()))
            .transpose()
            .context("failed to stage changeset content")?;
        let mut hasher = Sha256::new();
        let mut size = 0_u64;
        let mut buffer = vec![0_u8; 1024 * 1024];
        loop {
            let read = reader
                .read(&mut buffer)
                .context("failed to read changeset content")?;
            if read == 0 {
                break;
            }
            if let Some(temporary) = &mut temporary {
                temporary
                    .write_all(&buffer[..read])
                    .context("failed to write changeset content")?;
            }
            hasher.update(&buffer[..read]);
            size = size
                .checked_add(u64::try_from(read).context("changeset content size overflow")?)
                .context("changeset content size overflow")?;
        }
        let digest = crate::integrity::finish_sha256(hasher);
        if self.paths.contains_key(&digest) {
            return Ok((digest, size));
        }
        let Some(mut temporary) = temporary else {
            return Ok((digest, size));
        };
        temporary
            .as_file_mut()
            .sync_all()
            .context("failed to fsync changeset content")?;
        let path = self
            .directory
            .as_ref()
            .expect("content-backed store has a directory")
            .path()
            .join(digest.hex());
        temporary
            .persist_noclobber(&path)
            .map_err(|error| error.error)
            .context("failed to publish changeset content")?;
        self.paths.insert(digest.clone(), path);
        Ok((digest, size))
    }

    pub(crate) fn open(&self, digest: &Digest, expected_size: u64) -> Result<File> {
        let path = self
            .paths
            .get(digest)
            .with_context(|| format!("changeset content is unavailable: {digest}"))?;
        let mut file = File::open(path)
            .with_context(|| format!("failed to open changeset content: {digest}"))?;
        let (actual, size) = digest_reader(&mut file)?;
        if &actual != digest || size != expected_size {
            bail!(
                "changeset content failed verification for {digest}: size {size}, expected {expected_size}"
            );
        }
        file.rewind()?;
        Ok(file)
    }
}

#[derive(Debug)]
pub(crate) struct EncodedLayer {
    pub(crate) descriptor: OciDescriptor,
    pub(crate) diff_id: Digest,
}

#[derive(Debug)]
pub(crate) struct LayerEncoder {
    compression_level: u32,
}

impl Default for LayerEncoder {
    fn default() -> Self {
        Self {
            compression_level: 6,
        }
    }
}

impl LayerEncoder {
    pub(crate) fn encode(
        &self,
        layout: &OciLayout,
        changes: &ChangeSet,
        contents: &ContentStore,
    ) -> Result<EncodedLayer> {
        self.encode_with(layout, changes, contents, None)
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn encode_in(
        &self,
        layout: &OciLayout,
        changes: &ChangeSet,
        contents: &ContentStore,
        staging_parent: &Path,
    ) -> Result<EncodedLayer> {
        self.encode_with(layout, changes, contents, Some(staging_parent))
    }

    fn encode_with(
        &self,
        layout: &OciLayout,
        changes: &ChangeSet,
        contents: &ContentStore,
        staging_parent: Option<&Path>,
    ) -> Result<EncodedLayer> {
        let mut uncompressed = temporary_in(staging_parent)
            .context("failed to create uncompressed changeset Layer")?;
        Self::write_tar(uncompressed.as_file_mut(), changes, contents)?;
        uncompressed
            .as_file_mut()
            .sync_all()
            .context("failed to fsync uncompressed changeset Layer")?;
        let diff_id = digest_reader(uncompressed.reopen()?)?.0;
        uncompressed.rewind()?;

        let mut compressed =
            temporary_in(staging_parent).context("failed to create compressed changeset Layer")?;
        {
            let mut encoder = GzBuilder::new().mtime(0).operating_system(255).write(
                compressed.as_file_mut(),
                Compression::new(self.compression_level),
            );
            std::io::copy(uncompressed.as_file_mut(), &mut encoder)
                .context("failed to compress changeset Layer")?;
            encoder
                .finish()
                .context("failed to finish changeset compression")?;
        }
        compressed
            .as_file_mut()
            .sync_all()
            .context("failed to fsync compressed changeset Layer")?;
        compressed.rewind()?;
        let descriptor = layout.put_reader(compressed.as_file_mut(), OCI_LAYER_GZIP, None)?;
        Ok(EncodedLayer {
            descriptor,
            diff_id,
        })
    }

    pub(crate) fn write_tar(
        destination: &mut File,
        changes: &ChangeSet,
        contents: &ContentStore,
    ) -> Result<()> {
        let mut whiteouts = BTreeSet::new();
        for removal in changes.removals() {
            let whiteout = whiteout_path(removal)?;
            if !whiteouts.insert(whiteout.clone()) {
                bail!("duplicate changeset archive path: {}", whiteout.display());
            }
        }
        for (path, _) in changes.entries() {
            reject_reserved_name(path)?;
            if whiteouts.contains(path) {
                bail!(
                    "changeset archive path collides with a whiteout: {}",
                    path.display()
                );
            }
        }

        {
            let mut builder = Builder::new(destination);
            builder.mode(HeaderMode::Deterministic);
            if let Some(metadata) = changes.root() {
                append_metadata_extensions(&mut builder, metadata)?;
                append_directory(&mut builder, Path::new("."), "/", metadata)?;
            }
            for path in whiteouts {
                let mut header = header(0, 0, 0, 0, 0, EntryType::Regular)?;
                builder
                    .append_data(&mut header, path_buf(&path), std::io::empty())
                    .with_context(|| format!("failed to write whiteout {}", path.display()))?;
            }
            for (path, entry) in changes.entries() {
                append_entry(&mut builder, path, entry, contents)?;
            }
            builder
                .finish()
                .context("failed to finish changeset Layer")?;
        }
        Ok(())
    }
}

fn temporary_in(parent: Option<&Path>) -> std::io::Result<NamedTempFile> {
    parent.map_or_else(NamedTempFile::new, NamedTempFile::new_in)
}

fn append_entry(
    builder: &mut Builder<&mut File>,
    path: &FsPath,
    entry: &FsEntry,
    contents: &ContentStore,
) -> Result<()> {
    append_metadata_extensions(builder, &entry.metadata)?;
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
            builder
                .append_data(&mut header, path_buf(path), &mut file)
                .with_context(|| format!("failed to write regular file {}", path.display()))?;
        }
        EntryKind::Directory => {
            append_directory(builder, &path_buf(path), &path.display(), &entry.metadata)?;
        }
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
            append_link(builder, &mut header, path, target.as_bytes())
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
            append_link(builder, &mut header, path, target)
                .with_context(|| format!("failed to write symlink {}", path.display()))?;
        }
        EntryKind::Fifo => {
            append_special(builder, path, entry, EntryType::Fifo, None)?;
        }
        EntryKind::Character { major, minor } => {
            append_special(
                builder,
                path,
                entry,
                EntryType::Char,
                Some((*major, *minor)),
            )?;
        }
        EntryKind::Block { major, minor } => {
            append_special(
                builder,
                path,
                entry,
                EntryType::Block,
                Some((*major, *minor)),
            )?;
        }
    }
    Ok(())
}

fn append_directory(
    builder: &mut Builder<&mut File>,
    archive_path: &Path,
    display: &str,
    metadata: &crate::filesystem::Metadata,
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

fn append_metadata_extensions(
    builder: &mut Builder<&mut File>,
    metadata: &crate::filesystem::Metadata,
) -> Result<()> {
    let mut records = PaxRecords::default();
    if metadata.mtime.seconds < 0 || metadata.mtime.nanos != 0 {
        let mtime = pax_timestamp(metadata.mtime);
        records.insert(b"mtime", mtime.as_bytes())?;
    }
    pax::insert_xattrs(&mut records, &metadata.xattrs)?;
    pax::append_header(builder, &records, DEFAULT_MAX_PAX_BYTES)
        .context("failed to write PAX metadata")
}

fn base_mtime(timestamp: crate::filesystem::Timestamp) -> u64 {
    u64::try_from(timestamp.seconds).unwrap_or(0)
}

fn pax_timestamp(timestamp: crate::filesystem::Timestamp) -> String {
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

fn whiteout_path(path: &FsPath) -> Result<FsPath> {
    let basename = path.basename();
    if basename.is_empty() || basename.starts_with(b".wh.") {
        bail!(
            "filesystem path cannot be represented as an OCI whiteout: {}",
            path.display()
        );
    }
    let mut whiteout = b".wh.".to_vec();
    whiteout.extend_from_slice(basename);
    path.parent()
        .join_component(&whiteout, MAX_PATH_BYTES)
        .map_err(Into::into)
}

fn reject_reserved_name(path: &FsPath) -> Result<()> {
    if path.basename().starts_with(b".wh.") {
        bail!(
            "filesystem path uses the reserved OCI whiteout prefix: {}",
            path.display()
        );
    }
    Ok(())
}

fn path_buf(path: &FsPath) -> PathBuf {
    Path::new(OsStr::from_bytes(path.as_bytes())).to_path_buf()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::Read as _;

    use flate2::read::MultiGzDecoder;
    use tar::Archive;

    use super::*;
    use crate::filesystem::{Inventory, Metadata, Timestamp};

    #[test]
    fn layer_encoding_is_deterministic_and_raw_path_sorted() {
        let state = tempfile::tempdir().expect("state");
        let layout = OciLayout::open(state.path()).expect("layout");
        let mut contents = ContentStore::new().expect("content store");
        let raw_digest = contents.put_bytes(b"raw").expect("raw");
        let utf8_digest = contents.put_bytes(b"utf8").expect("utf8");
        let mut after = Inventory::default();
        after
            .insert(path(b"b-\xff"), regular(raw_digest, 3))
            .expect("raw entry");
        after
            .insert(path("b-�".as_bytes()), regular(utf8_digest, 4))
            .expect("utf8 entry");
        let changes = crate::changeset::compare(&Inventory::default(), &after).expect("diff");
        let first = LayerEncoder::default()
            .encode(&layout, &changes, &contents)
            .expect("first");
        let second = LayerEncoder::default()
            .encode(&layout, &changes, &contents)
            .expect("second");
        assert_eq!(first.descriptor, second.descriptor);
        assert_eq!(first.diff_id, second.diff_id);

        let bytes = layout
            .get_descriptor_bytes(&first.descriptor)
            .expect("compressed Layer");
        let mut archive = Archive::new(MultiGzDecoder::new(bytes.as_slice()));
        let paths = archive
            .entries()
            .expect("entries")
            .map(|entry| entry.expect("entry").path_bytes().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(paths, vec!["b-�".as_bytes().to_vec(), b"b-\xff".to_vec()]);
    }

    #[test]
    fn layer_binary_xattrs_round_trip_with_precise_negative_mtime() {
        let state = tempfile::tempdir().expect("state");
        let layout = OciLayout::open(state.path()).expect("layout");
        let mut contents = ContentStore::new().expect("content store");
        let digest = contents.put_bytes(b"value").expect("content");
        let mut file = regular(digest, 5);
        file.metadata.mtime = Timestamp {
            seconds: -1,
            nanos: 500_000_000,
        };
        let xattrs = BTreeMap::from([(
            b"user.percent%=\xff".to_vec().into_boxed_slice(),
            b"line\nzero\0tail".to_vec().into_boxed_slice(),
        )]);
        file.metadata.xattrs.clone_from(&xattrs);
        let mut after = Inventory::default();
        after.insert(path(b"value"), file).expect("file");
        let changes = crate::changeset::compare(&Inventory::default(), &after).expect("diff");
        let first = LayerEncoder::default()
            .encode(&layout, &changes, &contents)
            .expect("first");
        let second = LayerEncoder::default()
            .encode(&layout, &changes, &contents)
            .expect("second");
        assert_eq!(first.descriptor, second.descriptor);
        assert_eq!(first.diff_id, second.diff_id);

        let compressed = layout
            .get_descriptor_bytes(&first.descriptor)
            .expect("Layer");
        let mut uncompressed = Vec::new();
        MultiGzDecoder::new(compressed.as_slice())
            .read_to_end(&mut uncompressed)
            .expect("gzip");
        let index = crate::pax::scan_tar(
            uncompressed.as_slice(),
            crate::pax::TarPaxLimits {
                entries: 1,
                total_bytes: u64::try_from(uncompressed.len()).expect("tar length"),
                pax_bytes: DEFAULT_MAX_PAX_BYTES,
                index_bytes: DEFAULT_MAX_PAX_BYTES,
            },
        )
        .expect("PAX scan");
        let records = index.get(0).expect("file records").expect("PAX records");
        assert_eq!(records.get(b"mtime"), Some(b"-0.5".as_slice()));
        assert_eq!(crate::pax::decode_xattrs(records).expect("xattrs"), xattrs);

        let mut archive = Archive::new(uncompressed.as_slice());
        let entry = archive
            .entries()
            .expect("entries")
            .next()
            .expect("file")
            .expect("entry");
        assert_eq!(entry.path_bytes(), b"value".as_slice());
    }

    #[test]
    fn modified_file_is_a_direct_oci_layer_entry() {
        let old = crate::integrity::digest_bytes(b"old");
        let mut contents = ContentStore::new().expect("content store");
        let new = contents.put_bytes(b"new").expect("new");
        let mut before = Inventory::default();
        before
            .insert(path(b"value"), regular(old, 3))
            .expect("before");
        let mut after = Inventory::default();
        after
            .insert(path(b"value"), regular(new, 3))
            .expect("after");
        let changes = crate::changeset::compare(&before, &after).expect("diff");
        assert!(changes.removals().next().is_none());
        assert_eq!(
            changes
                .entries()
                .map(|(path, _)| path.as_bytes())
                .collect::<Vec<_>>(),
            vec![b"value".as_slice()]
        );
    }

    #[test]
    fn layer_encodes_links_special_files_and_precise_mtime() {
        let state = tempfile::tempdir().expect("state");
        let layout = OciLayout::open(state.path()).expect("layout");
        let mut contents = ContentStore::new().expect("content store");
        let digest = contents.put_bytes(b"shared").expect("shared");
        let timestamp = Timestamp {
            seconds: -1,
            nanos: 500_000_000,
        };
        let (after, target) = metadata_inventory(digest, timestamp, true);

        let changes = crate::changeset::compare(&Inventory::default(), &after).expect("diff");
        let encoded = LayerEncoder::default()
            .encode(&layout, &changes, &contents)
            .expect("encode");
        let bytes = layout
            .get_descriptor_bytes(&encoded.descriptor)
            .expect("Layer");
        let mut archive = Archive::new(MultiGzDecoder::new(bytes.as_slice()));
        let mut observed = BTreeMap::new();
        for entry in archive.entries().expect("entries") {
            let mut entry = entry.expect("entry");
            let pax = entry
                .pax_extensions()
                .expect("PAX")
                .expect("mtime extension")
                .map(|extension| {
                    let extension = extension.expect("PAX record");
                    (
                        extension.key_bytes().to_vec(),
                        extension.value_bytes().to_vec(),
                    )
                })
                .collect::<Vec<_>>();
            observed.insert(
                entry.path_bytes().into_owned(),
                (
                    entry.header().entry_type(),
                    entry.link_name_bytes().map(std::borrow::Cow::into_owned),
                    entry.header().device_major().ok().flatten(),
                    entry.header().device_minor().ok().flatten(),
                    pax,
                ),
            );
        }
        assert_metadata_entries(&observed, &target);
    }

    fn metadata_inventory(
        digest: Digest,
        timestamp: Timestamp,
        include_devices: bool,
    ) -> (Inventory, Vec<u8>) {
        let mut after = Inventory::default();
        after
            .insert(
                path(b"anchor"),
                FsEntry {
                    metadata: metadata(timestamp),
                    kind: EntryKind::Regular {
                        digest: digest.clone(),
                        size: 6,
                        hardlink: None,
                    },
                },
            )
            .expect("anchor");
        after
            .insert(
                path(b"hard"),
                FsEntry {
                    metadata: metadata(timestamp),
                    kind: EntryKind::Regular {
                        digest,
                        size: 6,
                        hardlink: Some(path(b"anchor")),
                    },
                },
            )
            .expect("hardlink");
        let mut target = b"../exact//".to_vec();
        target.extend(std::iter::repeat_n(b'x', 100));
        target.push(0xff);
        after
            .insert(
                path(b"symlink"),
                FsEntry {
                    metadata: metadata(timestamp),
                    kind: EntryKind::Symlink {
                        target: target.clone().into_boxed_slice(),
                    },
                },
            )
            .expect("symlink");
        let mut special = vec![(b"fifo".as_slice(), EntryKind::Fifo)];
        if include_devices {
            special.extend([
                (
                    b"char".as_slice(),
                    EntryKind::Character {
                        major: 12,
                        minor: 34,
                    },
                ),
                (
                    b"block".as_slice(),
                    EntryKind::Block {
                        major: 56,
                        minor: 78,
                    },
                ),
            ]);
        }
        for (name, kind) in special {
            after
                .insert(
                    path(name),
                    FsEntry {
                        metadata: metadata(timestamp),
                        kind,
                    },
                )
                .expect("special entry");
        }
        (after, target)
    }

    type ObservedMetadata = (
        EntryType,
        Option<Vec<u8>>,
        Option<u32>,
        Option<u32>,
        Vec<(Vec<u8>, Vec<u8>)>,
    );

    fn assert_metadata_entries(observed: &BTreeMap<Vec<u8>, ObservedMetadata>, target: &[u8]) {
        assert_eq!(observed.len(), 6);
        assert_eq!(observed[b"hard".as_slice()].0, EntryType::Link);
        assert_eq!(
            observed[b"hard".as_slice()].1.as_deref(),
            Some(b"anchor".as_slice())
        );
        assert_eq!(observed[b"symlink".as_slice()].0, EntryType::Symlink);
        assert_eq!(observed[b"symlink".as_slice()].1.as_deref(), Some(target));
        assert_eq!(observed[b"fifo".as_slice()].0, EntryType::Fifo);
        assert_eq!(observed[b"char".as_slice()].0, EntryType::Char);
        assert_eq!(observed[b"char".as_slice()].2, Some(12));
        assert_eq!(observed[b"char".as_slice()].3, Some(34));
        assert_eq!(observed[b"block".as_slice()].0, EntryType::Block);
        assert_eq!(observed[b"block".as_slice()].2, Some(56));
        assert_eq!(observed[b"block".as_slice()].3, Some(78));
        assert!(
            observed
                .values()
                .all(|value| value.4 == vec![(b"mtime".to_vec(), b"-0.5".to_vec())])
        );
    }

    #[test]
    fn pax_timestamp_uses_exact_decimal_value() {
        assert_eq!(
            pax_timestamp(Timestamp {
                seconds: 1,
                nanos: 230_000_000,
            }),
            "1.23"
        );
        assert_eq!(
            pax_timestamp(Timestamp {
                seconds: -1,
                nanos: 500_000_000,
            }),
            "-0.5"
        );
        assert_eq!(
            pax_timestamp(Timestamp {
                seconds: -1,
                nanos: 0,
            }),
            "-1"
        );
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "requires RUNLAB_TEST_BSDTAR pointing to a libarchive bsdtar executable"]
    fn libarchive_applies_links_fifo_and_subsecond_mtime() {
        use std::os::unix::ffi::OsStrExt as _;
        use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};
        use std::process::Command;

        use rustix::fs::getxattr;

        let executable = std::env::var_os("RUNLAB_TEST_BSDTAR").expect("RUNLAB_TEST_BSDTAR");
        let state = tempfile::tempdir().expect("state");
        let layout = OciLayout::open(state.path()).expect("layout");
        let mut contents = ContentStore::new().expect("content store");
        let digest = contents.put_bytes(b"shared").expect("shared");
        let timestamp = Timestamp {
            seconds: 1,
            nanos: 500_000_000,
        };
        let (mut after, target) = metadata_inventory(digest, timestamp, false);
        let precise_digest = contents.put_bytes(b"time").expect("precise content");
        let xattr_value = b"line\nzero\0tail";
        let mut precise_metadata = metadata(timestamp);
        precise_metadata.xattrs.insert(
            b"user.runlab".to_vec().into_boxed_slice(),
            xattr_value.to_vec().into_boxed_slice(),
        );
        after
            .insert(
                path(b"precise"),
                FsEntry {
                    metadata: precise_metadata,
                    kind: EntryKind::Regular {
                        digest: precise_digest,
                        size: 4,
                        hardlink: None,
                    },
                },
            )
            .expect("precise file");
        let changes = crate::changeset::compare(&Inventory::default(), &after).expect("diff");
        let encoded = LayerEncoder::default()
            .encode(&layout, &changes, &contents)
            .expect("encode");
        let compressed = layout
            .get_descriptor_bytes(&encoded.descriptor)
            .expect("Layer");
        let extraction = tempfile::tempdir().expect("extraction");
        let archive = extraction.path().join("layer.tar");
        let mut decoder = MultiGzDecoder::new(compressed.as_slice());
        let mut file = File::create(&archive).expect("archive");
        std::io::copy(&mut decoder, &mut file).expect("decompress");
        let root = extraction.path().join("rootfs");
        std::fs::create_dir(&root).expect("rootfs");
        let output = Command::new(executable)
            .args(["--xattrs", "-xf"])
            .arg(&archive)
            .args(["-C"])
            .arg(&root)
            .output()
            .expect("bsdtar");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );

        assert_eq!(
            std::fs::read(root.join("anchor")).expect("anchor"),
            b"shared"
        );
        let anchor = std::fs::metadata(root.join("anchor")).expect("anchor metadata");
        let hard = std::fs::metadata(root.join("hard")).expect("hardlink metadata");
        assert_eq!(anchor.ino(), hard.ino());
        let precise = std::fs::metadata(root.join("precise")).expect("precise metadata");
        assert_eq!((precise.mtime(), precise.mtime_nsec()), (1, 500_000_000));
        let precise_path = root.join("precise");
        let mut empty = [0_u8; 0];
        let required =
            getxattr(&precise_path, b"user.runlab".as_slice(), &mut empty).expect("xattr size");
        let mut value = vec![0_u8; required];
        let read =
            getxattr(&precise_path, b"user.runlab".as_slice(), &mut value).expect("xattr value");
        value.truncate(read);
        assert_eq!(value, xattr_value);
        assert_eq!((anchor.mtime(), anchor.mtime_nsec()), (1, 500_000_000));
        assert_eq!(
            std::fs::read_link(root.join("symlink"))
                .expect("symlink")
                .as_os_str()
                .as_bytes(),
            target
        );
        assert!(
            std::fs::symlink_metadata(root.join("fifo"))
                .expect("fifo")
                .file_type()
                .is_fifo()
        );
    }

    #[test]
    fn empty_changeset_has_stable_nonempty_tar_diff_id() {
        let state = tempfile::tempdir().expect("state");
        let layout = OciLayout::open(state.path()).expect("layout");
        let contents = ContentStore::new().expect("content store");
        let changes = ChangeSet::default();
        assert!(changes.is_empty());
        let encoded = LayerEncoder::default()
            .encode(&layout, &changes, &contents)
            .expect("empty");
        let bytes = layout
            .get_descriptor_bytes(&encoded.descriptor)
            .expect("Layer");
        let mut uncompressed = Vec::new();
        MultiGzDecoder::new(bytes.as_slice())
            .read_to_end(&mut uncompressed)
            .expect("gzip");
        assert_eq!(uncompressed.len(), 1024);
        assert_eq!(
            encoded.diff_id,
            crate::integrity::digest_bytes(&uncompressed)
        );
    }

    fn path(bytes: &[u8]) -> FsPath {
        FsPath::from_relative(bytes, MAX_PATH_BYTES).expect("path")
    }

    fn regular(digest: Digest, size: u64) -> FsEntry {
        FsEntry {
            metadata: metadata(Timestamp {
                seconds: 0,
                nanos: 0,
            }),
            kind: EntryKind::Regular {
                digest,
                size,
                hardlink: None,
            },
        }
    }

    fn metadata(mtime: Timestamp) -> Metadata {
        Metadata {
            mode: 0o644,
            uid: 0,
            gid: 0,
            mtime,
            xattrs: BTreeMap::new(),
        }
    }
}
