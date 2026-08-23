use std::collections::BTreeSet;
use std::fs::File;
use std::io::Seek;
use std::path::Path;

use anyhow::{Context, Result, bail};
use flate2::{Compression, GzBuilder};
use tempfile::NamedTempFile;

use crate::core::{Digest, OCI_LAYER_GZIP, OciDescriptor};
#[cfg(test)]
use crate::filesystem::pax_timestamp;
use crate::filesystem::{ContentStore, FilesystemTarWriter, FsPath};
use crate::integrity::digest_reader;
#[cfg(test)]
use crate::oci::OciLayout;

use super::ChangeSet;

const MAX_PATH_BYTES: u64 = 16 * 1024;

#[derive(Debug)]
#[cfg(test)]
pub(crate) struct EncodedLayer {
    pub(crate) descriptor: OciDescriptor,
    pub(crate) diff_id: Digest,
}

#[derive(Debug)]
pub(crate) struct StagedLayer {
    pub(crate) descriptor: OciDescriptor,
    pub(crate) diff_id: Digest,
    compressed: NamedTempFile,
}

impl StagedLayer {
    pub(crate) fn reader(&mut self) -> &mut File {
        self.compressed.as_file_mut()
    }
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
    #[cfg(test)]
    pub(crate) fn encode(
        &self,
        layout: &OciLayout,
        changes: &ChangeSet,
        contents: &ContentStore,
    ) -> Result<EncodedLayer> {
        let mut staged = self.stage_with(changes, contents, None)?;
        let expected = staged.descriptor.clone();
        let descriptor = layout.put_reader(staged.reader(), OCI_LAYER_GZIP, Some(&expected))?;
        Ok(EncodedLayer {
            descriptor,
            diff_id: staged.diff_id,
        })
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn stage_in(
        &self,
        changes: &ChangeSet,
        contents: &ContentStore,
        staging_parent: &Path,
    ) -> Result<StagedLayer> {
        self.stage_with(changes, contents, Some(staging_parent))
    }

    pub(crate) fn stage(
        &self,
        changes: &ChangeSet,
        contents: &ContentStore,
    ) -> Result<StagedLayer> {
        self.stage_with(changes, contents, None)
    }

    fn stage_with(
        &self,
        changes: &ChangeSet,
        contents: &ContentStore,
        staging_parent: Option<&Path>,
    ) -> Result<StagedLayer> {
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
        let (digest, size) = digest_reader(compressed.as_file_mut())?;
        compressed.rewind()?;
        Ok(StagedLayer {
            descriptor: OciDescriptor {
                digest,
                size,
                media_type: OCI_LAYER_GZIP.to_owned(),
            },
            diff_id,
            compressed,
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

        let mut writer = FilesystemTarWriter::new(destination);
        if let Some(metadata) = changes.root() {
            writer.append_root(metadata)?;
        }
        for path in whiteouts {
            writer.append_empty_regular(&path)?;
        }
        for (path, entry) in changes.entries() {
            writer.append_entry(path, entry, contents)?;
        }
        writer.finish()
    }
}

fn temporary_in(parent: Option<&Path>) -> std::io::Result<NamedTempFile> {
    parent.map_or_else(NamedTempFile::new, NamedTempFile::new_in)
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::Read as _;

    use flate2::read::MultiGzDecoder;
    use tar::{Archive, EntryType};

    use super::*;
    use crate::filesystem::pax::DEFAULT_MAX_PAX_BYTES;
    use crate::filesystem::{EntryKind, FsEntry, Inventory, Metadata, Timestamp};

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
        let index = crate::filesystem::pax::scan_tar(
            uncompressed.as_slice(),
            crate::filesystem::pax::TarPaxLimits {
                entries: 1,
                total_bytes: u64::try_from(uncompressed.len()).expect("tar length"),
                pax_bytes: DEFAULT_MAX_PAX_BYTES,
                index_bytes: DEFAULT_MAX_PAX_BYTES,
            },
        )
        .expect("PAX scan");
        let records = index.get(0).expect("file records").expect("PAX records");
        assert_eq!(records.get(b"mtime"), Some(b"-0.5".as_slice()));
        assert_eq!(
            crate::filesystem::pax::decode_xattrs(records).expect("xattrs"),
            xattrs
        );

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
