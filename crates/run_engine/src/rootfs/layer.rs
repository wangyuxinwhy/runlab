use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{Read, Seek};
use std::os::fd::OwnedFd;
use std::path::Path;

use anyhow::{Context, Result, bail};
use flate2::read::MultiGzDecoder;
use oci_spec::image::{Descriptor, DigestAlgorithm, MediaType};
use rustix::fs::{Mode, OFlags, fstat, mkdirat, open, openat};
use rustix::io::Errno;
use rustix::process::geteuid;
use tar::{Archive, EntryType};

use super::super::VerifiedLayer;
use super::apply::{CleanupBudget, apply_directory_metadata, apply_plan};
use super::capture::capture_stable;
use super::digest::copy_and_digest;
use super::plan::{LayerEntry, LayerKind, LayerPlan};
use super::preflight::{MaterializationBudget, preflight_decoded_tar};
use super::xattr::{decode_base64, validate_pax_xattr_name};
use super::{
    FsPath, MaterializationFault, Metadata, Rootfs, RootfsError, RootfsErrorKind, RootfsLimits,
    Timestamp, Xattrs, classify_io_error, classify_materialization_error, default_directory,
    enforce, internal_error, take_materialization_fault, unsupported_input, usize_to_u64,
};

struct ClassifiedReader<R> {
    inner: R,
    kind: RootfsErrorKind,
    fault: Option<MaterializationFault>,
}

impl<R> ClassifiedReader<R> {
    fn new(inner: R, kind: RootfsErrorKind) -> Self {
        Self {
            inner,
            kind,
            fault: None,
        }
    }

    fn engine_owned(inner: R, fault: MaterializationFault) -> Self {
        Self {
            inner,
            kind: RootfsErrorKind::Internal,
            fault: Some(fault),
        }
    }
}

impl<R: Read> Read for ClassifiedReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if self.fault.is_some_and(take_materialization_fault) {
            return Err(classify_io_error(
                std::io::Error::from_raw_os_error(Errno::IO.raw_os_error()),
                RootfsErrorKind::Internal,
            ));
        }
        self.inner
            .read(buffer)
            .map_err(|error| classify_io_error(error, self.kind))
    }
}

impl<R: Seek> Seek for ClassifiedReader<R> {
    fn seek(&mut self, position: std::io::SeekFrom) -> std::io::Result<u64> {
        self.inner
            .seek(position)
            .map_err(|error| classify_io_error(error, RootfsErrorKind::Internal))
    }
}

struct ClassifiedWriter<W> {
    inner: W,
}

impl<W: std::io::Write> std::io::Write for ClassifiedWriter<W> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.inner.write(buffer).map_err(|error| {
            std::io::Error::new(
                error.kind(),
                RootfsError::new(RootfsErrorKind::Internal, error.into()),
            )
        })
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush().map_err(|error| {
            std::io::Error::new(
                error.kind(),
                RootfsError::new(RootfsErrorKind::Internal, error.into()),
            )
        })
    }
}

impl Rootfs {
    pub(crate) fn materialize_in<F, R>(
        workspace: &Path,
        layers: &[VerifiedLayer<'_>],
        limits: RootfsLimits,
        mut open_layer: F,
    ) -> std::result::Result<Self, RootfsError>
    where
        F: FnMut(&Descriptor) -> Result<R>,
        R: Read,
    {
        enforce("layer count", limits.layers, usize_to_u64(layers.len())).map_err(|error| {
            classify_materialization_error(error, RootfsErrorKind::UnsupportedInput)
        })?;
        let workspace_fd = validate_workspace(workspace)
            .map_err(|error| classify_materialization_error(error, RootfsErrorKind::Internal))?;
        let root_path = workspace.join("rootfs");
        mkdirat(&workspace_fd, c"rootfs", Mode::RWXU)
            .with_context(|| format!("failed to create private rootfs {}", root_path.display()))
            .map_err(|error| classify_materialization_error(error, RootfsErrorKind::Internal))?;
        let root = openat(
            &workspace_fd,
            c"rootfs",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(anyhow::Error::from)
        .map_err(|error| classify_materialization_error(error, RootfsErrorKind::Internal))?;
        let mut directories = BTreeMap::from([(FsPath(Box::default()), default_directory())]);
        let mut materialization = MaterializationBudget::new(limits);
        let mut cleanup = CleanupBudget::new(limits);
        for layer in layers {
            materialization
                .compressed(layer.descriptor.size())
                .map_err(|error| {
                    classify_materialization_error(error, RootfsErrorKind::UnsupportedInput)
                })?;
            let source = open_layer(layer.descriptor)
                .map_err(|error| classify_materialization_error(error, RootfsErrorKind::Content))?;
            let mut decoded = verify_and_stage(
                workspace,
                layer,
                source,
                materialization.remaining_uncompressed(),
            )
            .map_err(|error| {
                classify_materialization_error(error, RootfsErrorKind::InvalidInput)
            })?;
            let decoded_size = decoded
                .as_file()
                .metadata()
                .map_err(anyhow::Error::from)
                .map_err(|error| classify_materialization_error(error, RootfsErrorKind::Internal))?
                .len();
            materialization
                .uncompressed(decoded_size)
                .map_err(|error| {
                    classify_materialization_error(error, RootfsErrorKind::UnsupportedInput)
                })?;
            let plan = scan_layer(
                decoded.as_file_mut(),
                workspace,
                limits,
                &mut materialization,
            )
            .map_err(|error| {
                classify_materialization_error(error, RootfsErrorKind::InvalidInput)
            })?;
            update_directory_metadata(&mut directories, &plan).map_err(|error| {
                classify_materialization_error(error, RootfsErrorKind::InvalidInput)
            })?;
            apply_plan(&root, &plan, limits, &mut cleanup).map_err(|error| {
                classify_materialization_error(error, RootfsErrorKind::Internal)
            })?;
        }
        apply_directory_metadata(&root, directories, limits)
            .map_err(|error| classify_materialization_error(error, RootfsErrorKind::Internal))?;
        let initial = capture_stable(&root, workspace, limits, false)
            .map_err(|error| classify_materialization_error(error, RootfsErrorKind::Internal))?
            .inventory;
        Ok(Self {
            workspace: workspace.to_path_buf(),
            root_path,
            root,
            initial,
            limits,
        })
    }
}

pub(super) fn validate_workspace(workspace: &Path) -> Result<OwnedFd> {
    let fd = open(
        workspace,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .with_context(|| format!("failed to inspect workspace {}", workspace.display()))?;
    let metadata = fstat(&fd)?;
    if metadata.st_uid != geteuid().as_raw() {
        bail!("rootfs workspace is not owned by the current effective uid");
    }
    if metadata.st_mode & 0o077 != 0 {
        bail!("rootfs workspace must not grant group or other permissions");
    }
    Ok(fd)
}

pub(super) fn verify_and_stage(
    workspace: &Path,
    layer: &VerifiedLayer<'_>,
    mut source: impl Read,
    max_uncompressed: u64,
) -> Result<tempfile::NamedTempFile> {
    if layer.descriptor.digest().algorithm() != &DigestAlgorithm::Sha256 {
        return Err(unsupported_input(format!(
            "unsupported OCI Layer digest algorithm: {}",
            layer.descriptor.digest()
        )));
    }
    let mut compressed = tempfile::NamedTempFile::new_in(workspace).map_err(internal_error)?;
    let (actual_descriptor_digest, compressed_size) = copy_and_digest(
        ClassifiedReader::new(&mut source, RootfsErrorKind::Content),
        ClassifiedWriter {
            inner: compressed.as_file_mut(),
        },
        Some(layer.descriptor.size()),
    )?;
    compressed
        .as_file_mut()
        .sync_all()
        .map_err(internal_error)?;
    if compressed_size != layer.descriptor.size() {
        bail!(
            "OCI Layer descriptor size mismatch for {}: expected {}, received {}",
            layer.descriptor.digest(),
            layer.descriptor.size(),
            compressed_size
        );
    }
    if &actual_descriptor_digest != layer.descriptor.digest() {
        bail!(
            "OCI Layer descriptor digest mismatch: expected {}, received {}",
            layer.descriptor.digest(),
            actual_descriptor_digest
        );
    }
    compressed.as_file_mut().rewind().map_err(internal_error)?;
    let mut compressed_reader = ClassifiedReader::engine_owned(
        compressed.as_file_mut(),
        MaterializationFault::CompressedRead,
    );
    let reader: Box<dyn Read + '_> = match layer.descriptor.media_type() {
        MediaType::ImageLayer | MediaType::ImageLayerNonDistributable => {
            Box::new(&mut compressed_reader)
        }
        MediaType::ImageLayerGzip | MediaType::ImageLayerNonDistributableGzip => {
            Box::new(MultiGzDecoder::new(&mut compressed_reader))
        }
        MediaType::ImageLayerZstd | MediaType::ImageLayerNonDistributableZstd => Box::new(
            zstd::stream::read::Decoder::new(&mut compressed_reader)
                .context("failed to initialize zstd OCI Layer decoder")?,
        ),
        other => {
            return Err(unsupported_input(format!(
                "unsupported OCI Layer mediaType: {other}"
            )));
        }
    };
    let mut decoded = tempfile::NamedTempFile::new_in(workspace).map_err(internal_error)?;
    let mut bounded = reader.take(max_uncompressed.saturating_add(1));
    let (actual_diff_id, decoded_size) = copy_and_digest(
        ClassifiedReader::new(&mut bounded, RootfsErrorKind::InvalidInput),
        ClassifiedWriter {
            inner: decoded.as_file_mut(),
        },
        None,
    )?;
    enforce("uncompressed Layer", max_uncompressed, decoded_size)?;
    if &actual_diff_id != layer.expected_diff_id {
        bail!(
            "OCI Layer DiffID mismatch for {}: expected {}, received {}",
            layer.descriptor.digest(),
            layer.expected_diff_id,
            actual_diff_id
        );
    }
    decoded.as_file_mut().sync_all().map_err(internal_error)?;
    decoded.as_file_mut().rewind().map_err(internal_error)?;
    Ok(decoded)
}

pub(super) fn scan_layer(
    decoded: &mut File,
    workspace: &Path,
    limits: RootfsLimits,
    budget: &mut MaterializationBudget,
) -> Result<LayerPlan> {
    let mut decoded = ClassifiedReader::engine_owned(decoded, MaterializationFault::DecodedRead);
    preflight_decoded_tar(&mut decoded, budget)?;
    decoded.rewind().map_err(internal_error)?;
    let mut archive = Archive::new(&mut decoded);
    let mut plan = LayerPlan::default();
    let mut seen = BTreeSet::new();
    for result in archive.entries().context("failed to read OCI Layer tar")? {
        let mut entry = result.context("failed to read OCI Layer entry")?;
        let raw_path = entry.path_bytes();
        let path = FsPath::from_relative(&raw_path, limits.path_bytes)?;
        if path.is_root() && entry.header().entry_type() != EntryType::Directory {
            bail!("unsafe root OCI Layer entry: {}", path.display());
        }
        if !seen.insert(path.clone()) {
            bail!("duplicate OCI Layer path: {}", path.display());
        }
        let entry_type = entry.header().entry_type();
        let size = entry.size();
        enforce("entry bytes", limits.entry_bytes, size)?;
        if collect_whiteout(&path, entry_type, size, &mut plan)? {
            continue;
        }
        let metadata = layer_metadata(&mut entry, limits)?;
        let kind = match entry_type {
            EntryType::Regular | EntryType::Continuous => LayerKind::Regular {
                content: stage_regular_content(&mut entry, workspace, size, &path)?,
                size,
            },
            EntryType::Directory => LayerKind::Directory,
            EntryType::Symlink => {
                let target = entry
                    .link_name_bytes()
                    .context("OCI Layer symlink lacks a target")?
                    .into_owned();
                enforce(
                    "link target bytes",
                    limits.link_target_bytes,
                    usize_to_u64(target.len()),
                )?;
                if target.contains(&0) {
                    bail!(
                        "OCI Layer symlink target contains NUL at {}",
                        path.display()
                    );
                }
                LayerKind::Symlink(target)
            }
            EntryType::Link => {
                let target = entry
                    .link_name_bytes()
                    .context("OCI Layer hardlink lacks a target")?;
                let target = FsPath::from_relative(&target, limits.path_bytes)?;
                LayerKind::Hardlink(target)
            }
            EntryType::Fifo => LayerKind::Fifo,
            EntryType::Char => LayerKind::Character {
                major: entry
                    .header()
                    .device_major()?
                    .context("character device lacks major")?,
                minor: entry
                    .header()
                    .device_minor()?
                    .context("character device lacks minor")?,
            },
            EntryType::Block => LayerKind::Block {
                major: entry
                    .header()
                    .device_major()?
                    .context("block device lacks major")?,
                minor: entry
                    .header()
                    .device_minor()?
                    .context("block device lacks minor")?,
            },
            other => {
                return Err(unsupported_input(format!(
                    "unsupported OCI Layer entry type {other:?} at {}",
                    path.display()
                )));
            }
        };
        plan.entries.push(LayerEntry {
            path,
            metadata,
            kind,
        });
    }
    Ok(plan)
}

fn stage_regular_content<R: Read>(
    entry: &mut tar::Entry<'_, R>,
    workspace: &Path,
    size: u64,
    path: &FsPath,
) -> Result<tempfile::TempPath> {
    let mut content = tempfile::NamedTempFile::new_in(workspace).map_err(internal_error)?;
    let (_, copied) = copy_and_digest(
        ClassifiedReader::new(entry, RootfsErrorKind::InvalidInput),
        ClassifiedWriter {
            inner: content.as_file_mut(),
        },
        Some(size),
    )?;
    if copied != size {
        bail!("OCI Layer regular size changed at {}", path.display());
    }
    content.as_file_mut().sync_all().map_err(internal_error)?;
    Ok(content.into_temp_path())
}

pub(super) fn collect_whiteout(
    path: &FsPath,
    entry_type: EntryType,
    size: u64,
    plan: &mut LayerPlan,
) -> Result<bool> {
    if !path.basename().starts_with(b".wh.") {
        return Ok(false);
    }
    if entry_type != EntryType::Regular || size != 0 {
        bail!("invalid OCI whiteout: {}", path.display());
    }
    if path.basename() == b".wh..wh..opq" {
        plan.opaques.push(path.parent());
        return Ok(true);
    }
    let target = &path.basename()[4..];
    if target.is_empty() || target.starts_with(b".wh.") {
        bail!("invalid OCI whiteout: {}", path.display());
    }
    plan.whiteouts.push(path.parent().join(target, u64::MAX)?);
    Ok(true)
}

pub(super) fn layer_metadata<R: Read>(
    entry: &mut tar::Entry<'_, R>,
    limits: RootfsLimits,
) -> Result<Metadata> {
    let mode = entry.header().mode().context("invalid OCI Layer mode")?;
    if mode > 0o7777 {
        bail!("OCI Layer mode exceeds permission and special bits: {mode:o}");
    }
    let uid = u32::try_from(entry.header().uid().context("invalid OCI Layer uid")?)
        .context("OCI Layer uid exceeds u32")?;
    let gid = u32::try_from(entry.header().gid().context("invalid OCI Layer gid")?)
        .context("OCI Layer gid exceeds u32")?;
    let header_mtime = entry.header().mtime().context("invalid OCI Layer mtime")?;
    let mut mtime = Timestamp {
        seconds: i64::try_from(header_mtime).context("OCI Layer mtime exceeds i64")?,
        nanos: 0,
    };
    let mut schily_xattrs = Xattrs::new();
    let mut libarchive_xattrs = Xattrs::new();
    if let Some(extensions) = entry
        .pax_extensions()
        .context("invalid OCI Layer PAX header")?
    {
        for extension in extensions {
            let extension = extension.context("invalid OCI Layer PAX record")?;
            let key = extension.key_bytes();
            let value = extension.value_bytes();
            if key == b"mtime" {
                mtime = parse_pax_timestamp(value)?;
            } else if let Some(name) = key.strip_prefix(b"SCHILY.xattr.") {
                validate_pax_xattr_name(name)?;
                validate_xattr(name, value, limits)?;
                if schily_xattrs.insert(name.into(), value.into()).is_some() {
                    bail!("duplicate OCI Layer xattr");
                }
            } else if let Some(name) = key.strip_prefix(b"LIBARCHIVE.xattr.") {
                validate_pax_xattr_name(name)?;
                let max_encoded = limits
                    .xattr_value_bytes
                    .checked_mul(4)
                    .and_then(|bytes| bytes.checked_div(3))
                    .and_then(|bytes| bytes.checked_add(4))
                    .ok_or_else(|| unsupported_input("xattr base64 size limit overflow"))?;
                if value.len() > max_encoded {
                    bail!("OCI Layer base64 xattr value exceeds limit");
                }
                let decoded = decode_base64(value)?;
                validate_xattr(name, &decoded, limits)?;
                if libarchive_xattrs
                    .insert(name.into(), decoded.into_boxed_slice())
                    .is_some()
                {
                    bail!("duplicate OCI Layer xattr");
                }
            }
        }
    }
    let names = schily_xattrs
        .keys()
        .chain(libarchive_xattrs.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut xattrs = Xattrs::new();
    for name in names {
        let value = match (
            schily_xattrs.get(name.as_ref()),
            libarchive_xattrs.get(name.as_ref()),
        ) {
            (Some(left), Some(right)) if left != right => {
                bail!("OCI Layer xattr representations disagree")
            }
            (Some(value), _) | (_, Some(value)) => value.clone(),
            (None, None) => unreachable!("xattr name came from one representation"),
        };
        xattrs.insert(name, value);
    }
    Ok(Metadata {
        mode,
        uid,
        gid,
        mtime,
        xattrs,
    })
}

pub(super) fn validate_xattr(name: &[u8], value: &[u8], limits: RootfsLimits) -> Result<()> {
    if name.is_empty() || name.len() > 255 || name.contains(&0) {
        bail!("invalid OCI Layer xattr name");
    }
    enforce(
        "xattr value bytes",
        usize_to_u64(limits.xattr_value_bytes),
        usize_to_u64(value.len()),
    )
}

pub(super) fn parse_pax_timestamp(raw: &[u8]) -> Result<Timestamp> {
    let text = std::str::from_utf8(raw).context("PAX mtime is not ASCII")?;
    let negative = text.starts_with('-');
    let unsigned = text.strip_prefix(['-', '+']).unwrap_or(text);
    let (seconds, fraction) = unsigned.split_once('.').unwrap_or((unsigned, ""));
    if seconds.is_empty()
        || fraction.len() > 9
        || !seconds.bytes().all(|b| b.is_ascii_digit())
        || !fraction.bytes().all(|b| b.is_ascii_digit())
    {
        bail!("invalid PAX mtime: {text}");
    }
    let whole: i128 = seconds.parse().context("PAX mtime seconds overflow")?;
    let mut nanos_text = fraction.to_owned();
    nanos_text.extend(std::iter::repeat_n('0', 9 - fraction.len()));
    let fraction_nanos: i128 = nanos_text.parse().unwrap_or(0);
    let total = (whole * 1_000_000_000 + fraction_nanos) * if negative { -1 } else { 1 };
    let floor_seconds = total.div_euclid(1_000_000_000);
    let nanos = total.rem_euclid(1_000_000_000);
    Ok(Timestamp {
        seconds: i64::try_from(floor_seconds).context("PAX mtime exceeds i64")?,
        nanos: u32::try_from(nanos).expect("nanosecond remainder fits u32"),
    })
}

pub(super) fn update_directory_metadata(
    directories: &mut BTreeMap<FsPath, Metadata>,
    plan: &LayerPlan,
) -> Result<()> {
    for removed in &plan.whiteouts {
        remove_metadata_subtree(directories, removed, true);
    }
    for opaque in &plan.opaques {
        remove_metadata_subtree(directories, opaque, false);
    }
    for entry in &plan.entries {
        ensure_metadata_ancestors(directories, &entry.path)?;
        if matches!(entry.kind, LayerKind::Directory) {
            directories.insert(entry.path.clone(), entry.metadata.clone());
        } else {
            remove_metadata_subtree(directories, &entry.path, true);
        }
    }
    Ok(())
}

pub(super) fn ensure_metadata_ancestors(
    directories: &mut BTreeMap<FsPath, Metadata>,
    path: &FsPath,
) -> Result<()> {
    let parts = path.components().map(<[u8]>::to_vec).collect::<Vec<_>>();
    let mut current = FsPath(Box::default());
    for part in parts.iter().take(parts.len().saturating_sub(1)) {
        current = current.join(part, u64::MAX)?;
        directories
            .entry(current.clone())
            .or_insert_with(default_directory);
    }
    Ok(())
}

pub(super) fn remove_metadata_subtree(
    directories: &mut BTreeMap<FsPath, Metadata>,
    path: &FsPath,
    include_root: bool,
) {
    directories.retain(|candidate, _| {
        !(candidate.is_descendant_of(path) || (include_root && candidate == path))
    });
}

#[cfg(test)]
mod classification_tests {
    use std::io::Read;

    use anyhow::anyhow;

    use super::super::{RootfsError, RootfsErrorKind, classify_materialization_error};
    use super::ClassifiedReader;

    struct NestedClassifiedFailure;

    impl Read for NestedClassifiedFailure {
        fn read(&mut self, _buffer: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other(RootfsError::new(
                RootfsErrorKind::InvalidInput,
                anyhow!("nested classified read failure"),
            )))
        }
    }

    #[test]
    fn classified_reader_preserves_an_existing_rootfs_error() {
        let mut reader = ClassifiedReader::new(NestedClassifiedFailure, RootfsErrorKind::Internal);
        let mut byte = [0_u8; 1];
        let error = anyhow::Error::from(reader.read(&mut byte).expect_err("injected read failure"));
        let classified = classify_materialization_error(error, RootfsErrorKind::Internal);
        assert_eq!(classified.kind(), RootfsErrorKind::InvalidInput);
    }
}
