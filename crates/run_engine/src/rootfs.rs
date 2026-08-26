//! Linux-private OCI root filesystem materialization and stopped-tree capture.
//!
//! A caller must keep the workspace private, remove all runtime mounts before
//! capture, and remove the workspace only after proving that no mount remains.
//! In particular, [`Rootfs`] does not recursively clean anything from `Drop` or
//! from a failed materialization attempt.

use std::fs::File;

use anyhow::Result;
use oci_spec::image::{Descriptor, Digest, MediaType};

/// Exact verified input for one ordered OCI Image Layer.
#[derive(Clone, Copy, Debug)]
pub(crate) struct VerifiedLayer<'a> {
    pub(crate) descriptor: &'a Descriptor,
    pub(crate) expected_diff_id: &'a Digest,
}

/// Explicit resource limits shared by materialization and capture.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RootfsLimits {
    pub(crate) layers: u64,
    pub(crate) entries: u64,
    pub(crate) total_compressed_bytes: u64,
    pub(crate) total_uncompressed_bytes: u64,
    pub(crate) entry_bytes: u64,
    pub(crate) path_bytes: u64,
    pub(crate) total_path_bytes: u64,
    pub(crate) link_target_bytes: u64,
    pub(crate) xattr_names_bytes: usize,
    pub(crate) xattr_value_bytes: usize,
    pub(crate) extension_bytes: u64,
    pub(crate) total_xattr_bytes: u64,
    pub(crate) total_content_bytes: u64,
    pub(crate) tar_bytes: u64,
    pub(crate) pending_hardlinks: u64,
    pub(crate) depth: u64,
    pub(crate) cleanup_entries: u64,
}

impl Default for RootfsLimits {
    fn default() -> Self {
        Self {
            layers: 1_024,
            entries: 1_000_000,
            total_compressed_bytes: 64 * 1024 * 1024 * 1024,
            total_uncompressed_bytes: 64 * 1024 * 1024 * 1024,
            entry_bytes: 64 * 1024 * 1024 * 1024,
            path_bytes: 16 * 1024,
            total_path_bytes: 1024 * 1024 * 1024,
            link_target_bytes: 16 * 1024,
            xattr_names_bytes: 1024 * 1024,
            xattr_value_bytes: 16 * 1024 * 1024,
            extension_bytes: 1024 * 1024,
            total_xattr_bytes: 1024 * 1024 * 1024,
            total_content_bytes: 64 * 1024 * 1024 * 1024,
            tar_bytes: 64 * 1024 * 1024 * 1024,
            pending_hardlinks: 1_000_000,
            depth: 1_024,
            cleanup_entries: 1_000_000,
        }
    }
}

/// Deterministic, uncompressed OCI Layer produced from a stopped rootfs.
#[derive(Debug)]
pub(crate) struct CapturedLayer {
    pub(crate) media_type: MediaType,
    pub(crate) path: tempfile::TempPath,
    pub(crate) size: u64,
    pub(crate) diff_id: Digest,
}

impl CapturedLayer {
    pub(crate) fn open(&self) -> Result<File> {
        File::open(&self.path).map_err(Into::into)
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use std::collections::{BTreeMap, BTreeSet};
    use std::ffi::{CStr, OsStr};
    use std::fs::File;
    use std::io::{Read, Seek as _};
    use std::os::fd::{AsFd, AsRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt as _;
    use std::path::{Path, PathBuf};
    use std::str::FromStr as _;

    use anyhow::{Context, Result, bail};
    use flate2::read::MultiGzDecoder;
    use oci_spec::image::{Descriptor, Digest, DigestAlgorithm, MediaType};
    use rustix::fs::{
        AtFlags, Dir, FileType, Mode, OFlags, Stat, Timestamps, UTIME_OMIT, XattrFlags, chmodat,
        chownat, fchmod, fchown, fgetxattr, flistxattr, fremovexattr, fsetxattr, fstat, futimens,
        lgetxattr, linkat, llistxattr, lremovexattr, lsetxattr, major, makedev, minor, mkdirat,
        mkfifoat, mknodat, open, openat, readlinkat, statat, symlinkat, unlinkat, utimensat,
    };
    use rustix::io::Errno;
    use rustix::process::{Gid, Uid, geteuid};
    use rustix::time::Timespec;
    use sha2::{Digest as _, Sha256};
    use tar::{Archive, Builder, EntryType, Header, HeaderMode};

    use super::{CapturedLayer, RootfsLimits, VerifiedLayer};

    type Xattrs = BTreeMap<Box<[u8]>, Box<[u8]>>;

    #[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
    struct FsPath(Box<[u8]>);

    impl FsPath {
        fn from_relative(raw: &[u8], limit: u64) -> Result<Self> {
            let observed = normalized_relative_len(raw)?;
            enforce("path bytes", limit, observed)?;
            let mut normalized = Vec::with_capacity(usize::try_from(observed)?);
            for component in raw.split(|byte| *byte == b'/') {
                if component.is_empty() || component == b"." {
                    continue;
                }
                if !normalized.is_empty() {
                    normalized.push(b'/');
                }
                normalized.extend_from_slice(component);
            }
            Ok(Self(normalized.into_boxed_slice()))
        }

        fn as_bytes(&self) -> &[u8] {
            &self.0
        }

        fn is_root(&self) -> bool {
            self.0.is_empty()
        }

        fn components(&self) -> impl Iterator<Item = &[u8]> {
            self.0
                .split(|byte| *byte == b'/')
                .filter(|part| !part.is_empty())
        }

        fn parent(&self) -> Self {
            self.0.iter().rposition(|byte| *byte == b'/').map_or_else(
                || Self(Box::default()),
                |split| Self(self.0[..split].into()),
            )
        }

        fn basename(&self) -> &[u8] {
            self.0
                .iter()
                .rposition(|byte| *byte == b'/')
                .map_or(&self.0, |split| &self.0[split + 1..])
        }

        fn join(&self, component: &[u8], limit: u64) -> Result<Self> {
            if component.is_empty()
                || component == b"."
                || component == b".."
                || component.contains(&b'/')
                || component.contains(&0)
            {
                bail!("unsafe filesystem component: {}", display_bytes(component));
            }
            let mut bytes = self.0.to_vec();
            if !bytes.is_empty() {
                bytes.push(b'/');
            }
            bytes.extend_from_slice(component);
            Self::from_relative(&bytes, limit)
        }

        fn is_descendant_of(&self, ancestor: &Self) -> bool {
            if ancestor.is_root() {
                return !self.is_root();
            }
            self.0.len() > ancestor.0.len()
                && self.0.starts_with(&ancestor.0)
                && self.0[ancestor.0.len()] == b'/'
        }

        fn display(&self) -> String {
            format!("/{}", display_bytes(&self.0))
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct Timestamp {
        seconds: i64,
        nanos: u32,
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct Metadata {
        mode: u32,
        uid: u32,
        gid: u32,
        mtime: Timestamp,
        xattrs: Xattrs,
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    enum EntryKind {
        Regular {
            digest: Digest,
            size: u64,
            hardlink: Option<FsPath>,
        },
        Directory,
        Symlink(Box<[u8]>),
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

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct FsEntry {
        metadata: Metadata,
        kind: EntryKind,
    }

    #[derive(Clone, Debug, Default, Eq, PartialEq)]
    struct Inventory {
        root: Option<Metadata>,
        entries: BTreeMap<FsPath, FsEntry>,
    }

    #[derive(Debug)]
    enum LayerKind {
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
    struct LayerEntry {
        path: FsPath,
        metadata: Metadata,
        kind: LayerKind,
    }

    #[derive(Debug, Default)]
    struct LayerPlan {
        whiteouts: Vec<FsPath>,
        opaques: Vec<FsPath>,
        entries: Vec<LayerEntry>,
    }

    struct MaterializationBudget {
        limits: RootfsLimits,
        compressed_bytes: u64,
        uncompressed_bytes: u64,
        entries: u64,
        raw_path_bytes: u64,
        xattr_bytes: u64,
        extension_bytes: u64,
    }

    impl MaterializationBudget {
        fn new(limits: RootfsLimits) -> Self {
            Self {
                limits,
                compressed_bytes: 0,
                uncompressed_bytes: 0,
                entries: 0,
                raw_path_bytes: 0,
                xattr_bytes: 0,
                extension_bytes: 0,
            }
        }

        fn compressed(&mut self, bytes: u64) -> Result<()> {
            self.compressed_bytes = checked_total(
                self.compressed_bytes,
                bytes,
                self.limits.total_compressed_bytes,
                "compressed Layer bytes",
            )?;
            Ok(())
        }

        fn remaining_uncompressed(&self) -> u64 {
            self.limits
                .total_uncompressed_bytes
                .saturating_sub(self.uncompressed_bytes)
        }

        fn uncompressed(&mut self, bytes: u64) -> Result<()> {
            self.uncompressed_bytes = checked_total(
                self.uncompressed_bytes,
                bytes,
                self.limits.total_uncompressed_bytes,
                "uncompressed Layer bytes",
            )?;
            Ok(())
        }

        fn entry(&mut self) -> Result<()> {
            self.entries = checked_total(self.entries, 1, self.limits.entries, "Layer entries")?;
            Ok(())
        }

        fn raw_path_bytes(&mut self, bytes: u64) -> Result<()> {
            self.raw_path_bytes = checked_total(
                self.raw_path_bytes,
                bytes,
                self.limits.total_path_bytes,
                "Layer raw path bytes",
            )?;
            Ok(())
        }

        fn xattr(&mut self, name_bytes: u64, value_bytes: u64) -> Result<()> {
            let added = name_bytes
                .checked_add(value_bytes)
                .context("Layer xattr byte count overflow")?;
            self.xattr_bytes = checked_total(
                self.xattr_bytes,
                added,
                self.limits.total_xattr_bytes,
                "Layer xattr bytes",
            )?;
            Ok(())
        }

        fn extension(&mut self, bytes: u64) -> Result<()> {
            self.extension_bytes = checked_total(
                self.extension_bytes,
                bytes,
                self.limits.extension_bytes,
                "tar extension bytes",
            )?;
            Ok(())
        }
    }

    /// A materialized rootfs and the immutable logical snapshot it came from.
    ///
    /// This object never recursively removes `workspace` or `rootfs`.  The
    /// execution engine owns the ordered unmount-and-cleanup boundary.
    #[derive(Debug)]
    pub(crate) struct Rootfs {
        workspace: PathBuf,
        root_path: PathBuf,
        root: OwnedFd,
        initial: Inventory,
        limits: RootfsLimits,
    }

    impl Rootfs {
        pub(crate) fn materialize_in<F, R>(
            workspace: &Path,
            layers: &[VerifiedLayer<'_>],
            limits: RootfsLimits,
            mut open_layer: F,
        ) -> Result<Self>
        where
            F: FnMut(&Descriptor) -> Result<R>,
            R: Read,
        {
            enforce("layer count", limits.layers, usize_to_u64(layers.len()))?;
            let workspace_fd = validate_workspace(workspace)?;
            let root_path = workspace.join("rootfs");
            mkdirat(&workspace_fd, c"rootfs", Mode::RWXU).with_context(|| {
                format!("failed to create private rootfs {}", root_path.display())
            })?;
            let root = openat(
                &workspace_fd,
                c"rootfs",
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )?;
            let mut directories = BTreeMap::from([(FsPath(Box::default()), default_directory())]);
            let mut materialization = MaterializationBudget::new(limits);
            let mut cleanup = CleanupBudget::new(limits);
            for layer in layers {
                materialization.compressed(layer.descriptor.size())?;
                let mut decoded = verify_and_stage(
                    workspace,
                    layer,
                    open_layer(layer.descriptor)?,
                    materialization.remaining_uncompressed(),
                )?;
                materialization.uncompressed(decoded.as_file().metadata()?.len())?;
                let plan = scan_layer(
                    decoded.as_file_mut(),
                    workspace,
                    limits,
                    &mut materialization,
                )?;
                update_directory_metadata(&mut directories, &plan)?;
                apply_plan(&root, &plan, limits, &mut cleanup)?;
            }
            apply_directory_metadata(&root, directories, limits)?;
            let initial = capture_stable(&root, workspace, limits, false)?.inventory;
            Ok(Self {
                workspace: workspace.to_path_buf(),
                root_path,
                root,
                initial,
                limits,
            })
        }

        pub(crate) fn path(&self) -> &Path {
            &self.root_path
        }

        /// Captures the complete stopped tree after proving all mounts are gone.
        pub(crate) fn capture(&self) -> Result<CapturedLayer> {
            self.ensure_no_mounts()?;
            let after = capture_stable(&self.root, &self.workspace, self.limits, true)?;
            self.ensure_no_mounts()?;
            let changes = compare(&self.initial, &after.inventory)?;
            let (path, size, diff_id) =
                encode_layer(&changes, &after.contents, &self.workspace, self.limits)?;
            Ok(CapturedLayer {
                media_type: MediaType::ImageLayer,
                path,
                size,
                diff_id,
            })
        }

        /// Fails unless `/proc/self/mountinfo` proves no mount remains below rootfs.
        pub(crate) fn ensure_no_mounts(&self) -> Result<()> {
            ensure_no_mounts(&self.root)
        }
    }

    fn validate_workspace(workspace: &Path) -> Result<OwnedFd> {
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

    fn verify_and_stage(
        workspace: &Path,
        layer: &VerifiedLayer<'_>,
        mut source: impl Read,
        max_uncompressed: u64,
    ) -> Result<tempfile::NamedTempFile> {
        if layer.descriptor.digest().algorithm() != &DigestAlgorithm::Sha256 {
            bail!(
                "unsupported OCI Layer digest algorithm: {}",
                layer.descriptor.digest()
            );
        }
        let mut compressed = tempfile::NamedTempFile::new_in(workspace)?;
        let (actual_descriptor_digest, compressed_size) = copy_and_digest(
            &mut source,
            compressed.as_file_mut(),
            Some(layer.descriptor.size()),
        )?;
        compressed.as_file_mut().sync_all()?;
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
        compressed.as_file_mut().rewind()?;
        let reader: Box<dyn Read + '_> = match layer.descriptor.media_type() {
            MediaType::ImageLayer | MediaType::ImageLayerNonDistributable => {
                Box::new(compressed.as_file_mut())
            }
            MediaType::ImageLayerGzip | MediaType::ImageLayerNonDistributableGzip => {
                Box::new(MultiGzDecoder::new(compressed.as_file_mut()))
            }
            MediaType::ImageLayerZstd | MediaType::ImageLayerNonDistributableZstd => Box::new(
                zstd::stream::read::Decoder::new(compressed.as_file_mut())
                    .context("failed to initialize zstd OCI Layer decoder")?,
            ),
            other => bail!("unsupported OCI Layer mediaType: {other}"),
        };
        let mut decoded = tempfile::NamedTempFile::new_in(workspace)?;
        let mut bounded = reader.take(max_uncompressed.saturating_add(1));
        let (actual_diff_id, decoded_size) =
            copy_and_digest(&mut bounded, decoded.as_file_mut(), None)?;
        enforce("uncompressed Layer", max_uncompressed, decoded_size)?;
        if &actual_diff_id != layer.expected_diff_id {
            bail!(
                "OCI Layer DiffID mismatch for {}: expected {}, received {}",
                layer.descriptor.digest(),
                layer.expected_diff_id,
                actual_diff_id
            );
        }
        decoded.as_file_mut().sync_all()?;
        decoded.as_file_mut().rewind()?;
        Ok(decoded)
    }

    fn scan_layer(
        decoded: &mut File,
        workspace: &Path,
        limits: RootfsLimits,
        budget: &mut MaterializationBudget,
    ) -> Result<LayerPlan> {
        preflight_decoded_tar(decoded, budget)?;
        decoded.rewind()?;
        let mut archive = Archive::new(decoded);
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
                EntryType::Regular | EntryType::Continuous => {
                    let mut content = tempfile::NamedTempFile::new_in(workspace)?;
                    let (_, copied) =
                        copy_and_digest(&mut entry, content.as_file_mut(), Some(size))?;
                    if copied != size {
                        bail!("OCI Layer regular size changed at {}", path.display());
                    }
                    content.as_file_mut().sync_all()?;
                    LayerKind::Regular {
                        content: content.into_temp_path(),
                        size,
                    }
                }
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
                other => bail!(
                    "unsupported OCI Layer entry type {other:?} at {}",
                    path.display()
                ),
            };
            plan.entries.push(LayerEntry {
                path,
                metadata,
                kind,
            });
        }
        Ok(plan)
    }

    /// Performs a fixed-memory physical tar pass before `tar::Archive` can
    /// allocate GNU long-name or PAX extension payloads. The counters belong
    /// to the whole image, so a later Layer can only consume what remains.
    fn preflight_decoded_tar(decoded: &mut File, budget: &mut MaterializationBudget) -> Result<()> {
        decoded.rewind()?;
        loop {
            let mut header = Header::new_old();
            read_tar_block(decoded, header.as_mut_bytes())?;
            if header.as_bytes().iter().all(|byte| *byte == 0) {
                return verify_zero_tar_tail(decoded);
            }
            validate_tar_checksum(&header)?;
            account_raw_header_paths(&header, budget)?;
            let size = header
                .entry_size()
                .context("invalid OCI Layer tar entry size")?;
            let entry_type = header.entry_type();
            if entry_type.is_gnu_sparse() {
                bail!("GNU sparse OCI Layer entries are unsupported");
            }
            if entry_type.is_pax_global_extensions() {
                bail!("OCI Layer PAX global extensions are unsupported");
            }
            if entry_type.is_pax_local_extensions() {
                budget.extension(size)?;
                preflight_pax_payload(decoded, size, budget)?;
            } else if entry_type.is_gnu_longname() || entry_type.is_gnu_longlink() {
                budget.extension(size)?;
                budget.raw_path_bytes(size)?;
                discard_exact(decoded, size)?;
            } else {
                budget.entry()?;
                enforce("entry bytes", budget.limits.entry_bytes, size)?;
                discard_exact(decoded, size)?;
            }
            discard_tar_padding(decoded, size)?;
        }
    }

    fn verify_zero_tar_tail(reader: &mut File) -> Result<()> {
        let mut buffer = [0_u8; 8 * 1024];
        loop {
            let read = reader.read(&mut buffer)?;
            if read == 0 {
                return Ok(());
            }
            if buffer[..read].iter().any(|byte| *byte != 0) {
                bail!("OCI Layer tar contains non-zero data after its end marker");
            }
        }
    }

    fn read_tar_block(reader: &mut File, block: &mut [u8; 512]) -> Result<()> {
        reader
            .read_exact(block)
            .context("truncated OCI Layer tar header")
    }

    fn validate_tar_checksum(header: &Header) -> Result<()> {
        let bytes = header.as_bytes();
        let actual = bytes[..148]
            .iter()
            .chain(&bytes[156..])
            .fold(8_u64 * u64::from(b' '), |sum, byte| {
                sum.saturating_add(u64::from(*byte))
            });
        let expected = u64::from(
            header
                .cksum()
                .context("invalid OCI Layer tar checksum field")?,
        );
        if actual != expected {
            bail!("OCI Layer tar header checksum mismatch");
        }
        Ok(())
    }

    fn account_raw_header_paths(header: &Header, budget: &mut MaterializationBudget) -> Result<()> {
        let bytes = header.as_bytes();
        budget.raw_path_bytes(usize_to_u64(nul_terminated_len(&bytes[..100])))?;
        budget.raw_path_bytes(usize_to_u64(nul_terminated_len(&bytes[157..257])))?;
        if header.as_ustar().is_some() {
            budget.raw_path_bytes(usize_to_u64(nul_terminated_len(&bytes[345..500])))?;
        }
        Ok(())
    }

    fn nul_terminated_len(bytes: &[u8]) -> usize {
        bytes
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(bytes.len())
    }

    fn preflight_pax_payload(
        reader: &mut File,
        payload_size: u64,
        budget: &mut MaterializationBudget,
    ) -> Result<()> {
        let mut remaining = payload_size;
        while remaining != 0 {
            let mut record_length = 0_u64;
            let mut prefix_bytes = 0_u64;
            loop {
                let byte = read_tar_byte(reader)?;
                remaining = remaining
                    .checked_sub(1)
                    .context("PAX record length exceeds extension payload")?;
                prefix_bytes += 1;
                if byte == b' ' {
                    break;
                }
                if !byte.is_ascii_digit() || prefix_bytes > 20 {
                    bail!("invalid OCI Layer PAX record length");
                }
                record_length = record_length
                    .checked_mul(10)
                    .and_then(|length| length.checked_add(u64::from(byte - b'0')))
                    .context("OCI Layer PAX record length overflow")?;
            }
            if record_length <= prefix_bytes || record_length - prefix_bytes > remaining {
                bail!("invalid OCI Layer PAX record boundary");
            }
            let body_bytes = record_length - prefix_bytes;
            let mut key = [0_u8; 300];
            let mut key_bytes = 0_u64;
            let mut key_stored = 0_usize;
            loop {
                if key_bytes + 2 > body_bytes {
                    bail!("invalid OCI Layer PAX record");
                }
                let byte = read_tar_byte(reader)?;
                remaining -= 1;
                if byte == b'=' {
                    break;
                }
                if key_stored < key.len() {
                    key[key_stored] = byte;
                    key_stored += 1;
                }
                key_bytes += 1;
            }
            let value_bytes = body_bytes - key_bytes - 2;
            let complete_key = key_bytes == usize_to_u64(key_stored);
            if complete_key && (key[..key_stored] == *b"path" || key[..key_stored] == *b"linkpath")
            {
                budget.raw_path_bytes(value_bytes)?;
            } else if complete_key {
                let key = &key[..key_stored];
                if key == b"size" {
                    // tar::Archive uses this value to override the following
                    // header's physical payload size. Rejecting it keeps this
                    // pass and the high-level parser on identical offsets.
                    bail!("OCI Layer PAX size overrides are unsupported");
                }
                if key.starts_with(b"GNU.sparse.") {
                    bail!("GNU sparse OCI Layer PAX metadata is unsupported");
                }
                if let Some(name) = key
                    .strip_prefix(b"SCHILY.xattr.")
                    .or_else(|| key.strip_prefix(b"LIBARCHIVE.xattr."))
                {
                    validate_pax_xattr_name(name)?;
                    budget.xattr(usize_to_u64(name.len()), value_bytes)?;
                }
            }
            discard_exact(reader, value_bytes)?;
            remaining -= value_bytes;
            if read_tar_byte(reader)? != b'\n' {
                bail!("invalid OCI Layer PAX record terminator");
            }
            remaining -= 1;
        }
        Ok(())
    }

    fn read_tar_byte(reader: &mut File) -> Result<u8> {
        let mut byte = [0_u8; 1];
        reader
            .read_exact(&mut byte)
            .context("truncated OCI Layer tar extension")?;
        Ok(byte[0])
    }

    fn discard_exact(reader: &mut File, mut bytes: u64) -> Result<()> {
        let mut buffer = [0_u8; 8 * 1024];
        while bytes != 0 {
            let requested = usize::try_from(bytes.min(usize_to_u64(buffer.len())))?;
            reader
                .read_exact(&mut buffer[..requested])
                .context("truncated OCI Layer tar entry")?;
            bytes -= usize_to_u64(requested);
        }
        Ok(())
    }

    fn discard_tar_padding(reader: &mut File, size: u64) -> Result<()> {
        let padding = (512 - size % 512) % 512;
        discard_exact(reader, padding)
    }

    fn collect_whiteout(
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

    fn layer_metadata<R: Read>(
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
                        .context("xattr base64 size limit overflow")?;
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

    fn validate_xattr(name: &[u8], value: &[u8], limits: RootfsLimits) -> Result<()> {
        if name.is_empty() || name.len() > 255 || name.contains(&0) {
            bail!("invalid OCI Layer xattr name");
        }
        enforce(
            "xattr value bytes",
            usize_to_u64(limits.xattr_value_bytes),
            usize_to_u64(value.len()),
        )
    }

    fn validate_pax_xattr_name(name: &[u8]) -> Result<()> {
        if name
            .iter()
            .any(|byte| !(33..=126).contains(byte) || *byte == b'=')
        {
            bail!(
                "Linux xattr name cannot be represented literally in a PAX key: {}",
                display_bytes(name)
            );
        }
        Ok(())
    }

    fn parse_pax_timestamp(raw: &[u8]) -> Result<Timestamp> {
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

    fn update_directory_metadata(
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

    fn ensure_metadata_ancestors(
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

    fn remove_metadata_subtree(
        directories: &mut BTreeMap<FsPath, Metadata>,
        path: &FsPath,
        include_root: bool,
    ) {
        directories.retain(|candidate, _| {
            !(candidate.is_descendant_of(path) || (include_root && candidate == path))
        });
    }

    struct CleanupBudget {
        entries: u64,
        limits: RootfsLimits,
    }

    impl CleanupBudget {
        fn new(limits: RootfsLimits) -> Self {
            Self { entries: 0, limits }
        }

        fn visit(&mut self, depth: u64) -> Result<()> {
            enforce("cleanup depth", self.limits.depth, depth)?;
            self.entries = checked_total(
                self.entries,
                1,
                self.limits.cleanup_entries,
                "cleanup entries",
            )?;
            Ok(())
        }
    }

    struct PendingHardlink<'a> {
        entry: &'a LayerEntry,
        target: &'a FsPath,
    }

    fn apply_plan(
        root: &OwnedFd,
        plan: &LayerPlan,
        limits: RootfsLimits,
        cleanup: &mut CleanupBudget,
    ) -> Result<()> {
        for path in &plan.whiteouts {
            remove_if_present(root, path, cleanup)?;
        }
        for path in &plan.opaques {
            remove_children(root, path, cleanup)?;
        }
        let mut pending = BTreeMap::new();
        for entry in &plan.entries {
            match &entry.kind {
                LayerKind::Directory => ensure_directory(root, &entry.path, cleanup)?,
                LayerKind::Regular { content, size } => {
                    apply_regular(root, entry, content, *size, cleanup, limits)?;
                }
                LayerKind::Symlink(target) => {
                    apply_symlink(root, entry, target, cleanup, limits)?;
                }
                LayerKind::Hardlink(target) => {
                    if !try_hardlink(root, entry, target, cleanup)? {
                        enforce(
                            "pending hardlinks",
                            limits.pending_hardlinks,
                            usize_to_u64(pending.len() + 1),
                        )?;
                        pending.insert(entry.path.clone(), PendingHardlink { entry, target });
                    }
                }
                LayerKind::Fifo => apply_fifo(root, entry, cleanup, limits)?,
                LayerKind::Character { major, minor } => apply_device(
                    root,
                    entry,
                    FileType::CharacterDevice,
                    *major,
                    *minor,
                    cleanup,
                    limits,
                )?,
                LayerKind::Block { major, minor } => {
                    apply_device(
                        root,
                        entry,
                        FileType::BlockDevice,
                        *major,
                        *minor,
                        cleanup,
                        limits,
                    )?;
                }
            }
        }
        resolve_hardlinks(root, pending, cleanup)
    }

    fn resolve_hardlinks(
        root: &OwnedFd,
        mut pending: BTreeMap<FsPath, PendingHardlink<'_>>,
        cleanup: &mut CleanupBudget,
    ) -> Result<()> {
        while let Some(origin) = pending.keys().next().cloned() {
            let mut current = origin;
            let mut chain = Vec::new();
            let mut visiting = BTreeSet::new();
            loop {
                let target = pending
                    .get(&current)
                    .context("hardlink plan lost an entry")?
                    .target;
                if !visiting.insert(current.clone()) {
                    bail!(
                        "unresolved OCI hardlink cycle: {} -> {}",
                        current.display(),
                        target.display()
                    );
                }
                chain.push(current);
                if pending.contains_key(target) {
                    current = target.clone();
                } else {
                    break;
                }
            }
            for path in chain.into_iter().rev() {
                let item = pending
                    .remove(&path)
                    .context("hardlink plan lost an entry")?;
                if !try_hardlink(root, item.entry, item.target, cleanup)? {
                    bail!(
                        "unresolved OCI hardlink: {} -> {}",
                        item.entry.path.display(),
                        item.target.display()
                    );
                }
            }
        }
        Ok(())
    }

    fn apply_regular(
        root: &OwnedFd,
        entry: &LayerEntry,
        content: &Path,
        size: u64,
        cleanup: &mut CleanupBudget,
        limits: RootfsLimits,
    ) -> Result<()> {
        let parent = open_parent(root, &entry.path, true)?;
        let name = os(entry.path.basename());
        remove_at_if_present(&parent, name, cleanup)?;
        let fd = openat(
            &parent,
            name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )?;
        let mut destination = File::from(fd);
        let mut source = File::open(content)?;
        let copied = std::io::copy(&mut source, &mut destination)?;
        if copied != size {
            bail!(
                "staged regular content size changed for {}",
                entry.path.display()
            );
        }
        destination.sync_all()?;
        apply_metadata_fd(&destination, &entry.metadata, limits)
    }

    fn apply_symlink(
        root: &OwnedFd,
        entry: &LayerEntry,
        target: &[u8],
        cleanup: &mut CleanupBudget,
        limits: RootfsLimits,
    ) -> Result<()> {
        let parent = open_parent(root, &entry.path, true)?;
        let name = os(entry.path.basename());
        remove_at_if_present(&parent, name, cleanup)?;
        symlinkat(os(target), &parent, name)?;
        apply_metadata_path(&parent, name, &entry.metadata, limits)
    }

    fn apply_fifo(
        root: &OwnedFd,
        entry: &LayerEntry,
        cleanup: &mut CleanupBudget,
        limits: RootfsLimits,
    ) -> Result<()> {
        let parent = open_parent(root, &entry.path, true)?;
        let name = os(entry.path.basename());
        remove_at_if_present(&parent, name, cleanup)?;
        mkfifoat(&parent, name, Mode::RUSR | Mode::WUSR)?;
        apply_metadata_path(&parent, name, &entry.metadata, limits)
    }

    fn apply_device(
        root: &OwnedFd,
        entry: &LayerEntry,
        file_type: FileType,
        major: u32,
        minor: u32,
        cleanup: &mut CleanupBudget,
        limits: RootfsLimits,
    ) -> Result<()> {
        let parent = open_parent(root, &entry.path, true)?;
        let name = os(entry.path.basename());
        remove_at_if_present(&parent, name, cleanup)?;
        mknodat(
            &parent,
            name,
            file_type,
            Mode::RUSR | Mode::WUSR,
            makedev(major, minor),
        )?;
        apply_metadata_path(&parent, name, &entry.metadata, limits)
    }

    fn try_hardlink(
        root: &OwnedFd,
        entry: &LayerEntry,
        target: &FsPath,
        cleanup: &mut CleanupBudget,
    ) -> Result<bool> {
        let Some(target_parent) = open_parent_existing(root, target)? else {
            return Ok(false);
        };
        match statat(
            &target_parent,
            os(target.basename()),
            AtFlags::SYMLINK_NOFOLLOW,
        ) {
            Ok(stat) if FileType::from_raw_mode(stat.st_mode) != FileType::Directory => {}
            Ok(_) => bail!("OCI hardlink target is a directory: {}", target.display()),
            Err(Errno::NOENT) => return Ok(false),
            Err(error) => return Err(error.into()),
        }
        let parent = open_parent(root, &entry.path, true)?;
        remove_at_if_present(&parent, os(entry.path.basename()), cleanup)?;
        linkat(
            &target_parent,
            os(target.basename()),
            &parent,
            os(entry.path.basename()),
            AtFlags::empty(),
        )?;
        Ok(true)
    }

    fn ensure_directory(root: &OwnedFd, path: &FsPath, cleanup: &mut CleanupBudget) -> Result<()> {
        if path.is_root() {
            return Ok(());
        }
        let parent = open_parent(root, path, true)?;
        let name = os(path.basename());
        match openat(
            &parent,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(_) => Ok(()),
            Err(Errno::NOTDIR | Errno::LOOP) => {
                remove_at_if_present(&parent, name, cleanup)?;
                mkdirat(&parent, name, Mode::RUSR | Mode::WUSR | Mode::XUSR)?;
                Ok(())
            }
            Err(Errno::NOENT) => {
                mkdirat(&parent, name, Mode::RUSR | Mode::WUSR | Mode::XUSR)?;
                Ok(())
            }
            Err(error) => Err(error.into()),
        }
    }

    fn open_parent(root: &OwnedFd, path: &FsPath, create: bool) -> Result<OwnedFd> {
        let components = path.components().collect::<Vec<_>>();
        let mut directory = reopen_directory(root)?;
        for component in components.iter().take(components.len().saturating_sub(1)) {
            match openat(
                &directory,
                os(component),
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            ) {
                Ok(child) => directory = child,
                Err(Errno::NOENT) if create => {
                    mkdirat(
                        &directory,
                        os(component),
                        Mode::RUSR | Mode::WUSR | Mode::XUSR,
                    )?;
                    let child = openat(
                        &directory,
                        os(component),
                        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                        Mode::empty(),
                    )?;
                    apply_new_directory_metadata(&child, &default_directory())?;
                    directory = child;
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(directory)
    }

    fn open_parent_existing(root: &OwnedFd, path: &FsPath) -> Result<Option<OwnedFd>> {
        match open_parent(root, path, false) {
            Ok(parent) => Ok(Some(parent)),
            Err(error) if error.downcast_ref::<Errno>() == Some(&Errno::NOENT) => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn open_directory(root: &OwnedFd, path: &FsPath) -> Result<OwnedFd> {
        if path.is_root() {
            return reopen_directory(root);
        }
        let parent = open_parent(root, path, false)?;
        Ok(openat(
            &parent,
            os(path.basename()),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?)
    }

    fn remove_if_present(root: &OwnedFd, path: &FsPath, budget: &mut CleanupBudget) -> Result<()> {
        let Some(parent) = open_parent_existing(root, path)? else {
            return Ok(());
        };
        remove_at_if_present(&parent, os(path.basename()), budget)
    }

    fn remove_children(root: &OwnedFd, path: &FsPath, budget: &mut CleanupBudget) -> Result<()> {
        let directory = match open_directory(root, path) {
            Ok(directory) => directory,
            Err(error)
                if error.downcast_ref::<Errno>().is_some_and(|errno| {
                    [Errno::NOENT, Errno::NOTDIR, Errno::LOOP].contains(errno)
                }) =>
            {
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        remove_directory_entries(&directory, 1, budget)
    }

    fn remove_at_if_present(
        parent: &OwnedFd,
        name: &OsStr,
        budget: &mut CleanupBudget,
    ) -> Result<()> {
        match remove_at(parent, name, 1, budget) {
            Ok(()) => Ok(()),
            Err(error) if error.downcast_ref::<Errno>() == Some(&Errno::NOENT) => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn remove_at(
        parent: &OwnedFd,
        name: &OsStr,
        depth: u64,
        budget: &mut CleanupBudget,
    ) -> Result<()> {
        let stat = statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)?;
        budget.visit(depth)?;
        if FileType::from_raw_mode(stat.st_mode) == FileType::Directory {
            let directory = openat(
                parent,
                name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )?;
            remove_directory_entries(
                &directory,
                depth.checked_add(1).context("cleanup depth overflow")?,
                budget,
            )?;
            unlinkat(parent, name, AtFlags::REMOVEDIR)?;
        } else {
            unlinkat(parent, name, AtFlags::empty())?;
        }
        Ok(())
    }

    fn remove_directory_entries(
        directory: &OwnedFd,
        depth: u64,
        budget: &mut CleanupBudget,
    ) -> Result<()> {
        for result in Dir::read_from(directory)? {
            let entry = result?;
            let name = entry.file_name().to_bytes();
            if name == b"." || name == b".." {
                continue;
            }
            let name = name.to_vec();
            remove_at(directory, os(&name), depth, budget)?;
        }
        Ok(())
    }

    fn apply_directory_metadata(
        root: &OwnedFd,
        entries: BTreeMap<FsPath, Metadata>,
        limits: RootfsLimits,
    ) -> Result<()> {
        let mut entries = entries.into_iter().collect::<Vec<_>>();
        entries.sort_by_key(|(path, _)| std::cmp::Reverse(path.components().count()));
        for (path, metadata) in entries {
            let fd = open_directory(root, &path)?;
            apply_metadata_fd(&fd, &metadata, limits)?;
        }
        Ok(())
    }

    fn apply_metadata_fd(fd: impl AsFd, metadata: &Metadata, limits: RootfsLimits) -> Result<()> {
        fchown(
            &fd,
            Some(valid_uid(metadata.uid)?),
            Some(valid_gid(metadata.gid)?),
        )?;
        replace_fd_xattrs(&fd, &metadata.xattrs, limits)?;
        fchmod(&fd, Mode::from_raw_mode(metadata.mode))?;
        futimens(&fd, &timestamps(metadata.mtime))?;
        Ok(())
    }

    fn apply_new_directory_metadata(fd: impl AsFd, metadata: &Metadata) -> Result<()> {
        fchown(
            &fd,
            Some(valid_uid(metadata.uid)?),
            Some(valid_gid(metadata.gid)?),
        )?;
        fchmod(&fd, Mode::from_raw_mode(metadata.mode))?;
        futimens(&fd, &timestamps(metadata.mtime))?;
        Ok(())
    }

    fn apply_metadata_path(
        parent: &OwnedFd,
        name: &OsStr,
        metadata: &Metadata,
        limits: RootfsLimits,
    ) -> Result<()> {
        chownat(
            parent,
            name,
            Some(valid_uid(metadata.uid)?),
            Some(valid_gid(metadata.gid)?),
            AtFlags::SYMLINK_NOFOLLOW,
        )?;
        replace_path_xattrs(parent, name, &metadata.xattrs, limits)?;
        let stat = statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)?;
        if FileType::from_raw_mode(stat.st_mode) != FileType::Symlink {
            chmodat(
                parent,
                name,
                Mode::from_raw_mode(metadata.mode),
                AtFlags::empty(),
            )?;
        }
        utimensat(
            parent,
            name,
            &timestamps(metadata.mtime),
            AtFlags::SYMLINK_NOFOLLOW,
        )?;
        Ok(())
    }

    fn replace_fd_xattrs(fd: impl AsFd, xattrs: &Xattrs, limits: RootfsLimits) -> Result<()> {
        let names = list_fd_xattr_names(&fd, limits.xattr_names_bytes)?;
        for name in split_xattr_names(&names)? {
            fremovexattr(&fd, name)?;
        }
        for (name, value) in xattrs {
            fsetxattr(&fd, name.as_ref(), value, XattrFlags::empty())?;
        }
        Ok(())
    }

    fn replace_path_xattrs(
        parent: &OwnedFd,
        name: &OsStr,
        xattrs: &Xattrs,
        limits: RootfsLimits,
    ) -> Result<()> {
        let path = proc_path(parent, name.as_bytes());
        let names = list_path_xattr_names(&path, limits.xattr_names_bytes)?;
        for old in split_xattr_names(&names)? {
            lremovexattr(&path, old)?;
        }
        for (key, value) in xattrs {
            lsetxattr(&path, key.as_ref(), value, XattrFlags::empty())?;
        }
        Ok(())
    }

    fn timestamps(timestamp: Timestamp) -> Timestamps {
        Timestamps {
            last_access: Timespec {
                tv_sec: 0,
                tv_nsec: UTIME_OMIT,
            },
            last_modification: Timespec {
                tv_sec: timestamp.seconds,
                tv_nsec: i64::from(timestamp.nanos),
            },
        }
    }

    fn valid_uid(raw: u32) -> Result<Uid> {
        if raw == u32::MAX {
            bail!("OCI Layer uid is reserved");
        }
        Ok(Uid::from_raw(raw))
    }

    fn valid_gid(raw: u32) -> Result<Gid> {
        if raw == u32::MAX {
            bail!("OCI Layer gid is reserved");
        }
        Ok(Gid::from_raw(raw))
    }

    #[derive(Default)]
    struct CapturedTree {
        inventory: Inventory,
        contents: BTreeMap<String, tempfile::TempPath>,
    }

    struct CaptureBudget {
        limits: RootfsLimits,
        entries: u64,
        path_bytes: u64,
        xattr_bytes: u64,
        content_bytes: u64,
    }

    impl CaptureBudget {
        fn new(limits: RootfsLimits) -> Self {
            Self {
                limits,
                entries: 0,
                path_bytes: 0,
                xattr_bytes: 0,
                content_bytes: 0,
            }
        }

        fn entry(&mut self, parent: Option<&FsPath>, name: &[u8], depth: u64) -> Result<()> {
            enforce("capture depth", self.limits.depth, depth)?;
            self.entries = checked_total(self.entries, 1, self.limits.entries, "capture entries")?;
            let separator = u64::from(parent.is_some_and(|path| !path.is_root()));
            let path_bytes = parent
                .map_or(0, |path| usize_to_u64(path.as_bytes().len()))
                .checked_add(separator)
                .and_then(|bytes| bytes.checked_add(usize_to_u64(name.len())))
                .context("capture path byte count overflow")?;
            enforce("capture path bytes", self.limits.path_bytes, path_bytes)?;
            self.path_bytes = checked_total(
                self.path_bytes,
                path_bytes,
                self.limits.total_path_bytes,
                "capture path bytes",
            )?;
            Ok(())
        }

        fn xattrs(&mut self, bytes: usize) -> Result<()> {
            self.xattr_bytes = checked_total(
                self.xattr_bytes,
                usize_to_u64(bytes),
                self.limits.total_xattr_bytes,
                "capture xattr bytes",
            )?;
            Ok(())
        }

        fn content(&mut self, bytes: u64) -> Result<()> {
            self.content_bytes = checked_total(
                self.content_bytes,
                bytes,
                self.limits.total_content_bytes,
                "capture content bytes",
            )?;
            Ok(())
        }
    }

    struct CaptureState {
        tree: CapturedTree,
        hardlinks: BTreeMap<(u128, u128), Vec<FsPath>>,
        budget: CaptureBudget,
        keep_contents: bool,
        content_parent: PathBuf,
    }

    fn capture_stable(
        root: &OwnedFd,
        content_parent: &Path,
        limits: RootfsLimits,
        keep_contents: bool,
    ) -> Result<CapturedTree> {
        let first = capture_pass(root, content_parent, limits, false)?;
        let second = capture_pass(root, content_parent, limits, keep_contents)?;
        if first.inventory != second.inventory {
            bail!("filesystem changed between complete capture passes");
        }
        Ok(second)
    }

    fn capture_pass(
        root: &OwnedFd,
        content_parent: &Path,
        limits: RootfsLimits,
        keep_contents: bool,
    ) -> Result<CapturedTree> {
        let mut state = CaptureState {
            tree: CapturedTree::default(),
            hardlinks: BTreeMap::new(),
            budget: CaptureBudget::new(limits),
            keep_contents,
            content_parent: content_parent.to_path_buf(),
        };
        let directory = reopen_directory(root)?;
        let initial = fstat(&directory)?;
        let initial_xattrs = read_fd_xattrs(&directory, limits, Some(&mut state.budget))?;
        walk_directory(&directory, None, 0, limits, &mut state)?;
        let final_stat = fstat(&directory)?;
        let final_xattrs = read_fd_xattrs(&directory, limits, None)?;
        ensure_stable(&initial, &final_stat, &initial_xattrs, &final_xattrs, "/")?;
        state.tree.inventory.root = Some(metadata(&initial, initial_xattrs)?);
        normalize_hardlinks(&mut state.tree.inventory, state.hardlinks)?;
        Ok(state.tree)
    }

    fn walk_directory(
        directory: &OwnedFd,
        parent: Option<&FsPath>,
        depth: u64,
        limits: RootfsLimits,
        state: &mut CaptureState,
    ) -> Result<()> {
        let children =
            capture_directory_entries(directory, parent, depth, limits, &mut state.budget)?;
        for (name, path, child_depth) in children {
            let c_name = c_name(&name)?;
            let initial = statat(directory, &c_name, AtFlags::SYMLINK_NOFOLLOW)
                .with_context(|| format!("failed to stat {}", path.display()))?;
            let entry = capture_entry(
                directory,
                &c_name,
                &name,
                &path,
                child_depth,
                &initial,
                limits,
                state,
            )?;
            if state
                .tree
                .inventory
                .entries
                .insert(path.clone(), entry)
                .is_some()
            {
                bail!("duplicate captured filesystem path: {}", path.display());
            }
        }
        Ok(())
    }

    fn capture_directory_entries(
        directory: &OwnedFd,
        parent: Option<&FsPath>,
        depth: u64,
        limits: RootfsLimits,
        budget: &mut CaptureBudget,
    ) -> Result<Vec<(Vec<u8>, FsPath, u64)>> {
        let child_depth = depth.checked_add(1).context("capture depth overflow")?;
        let mut children = Vec::new();
        for result in Dir::read_from(directory)? {
            let entry = result?;
            let name = entry.file_name().to_bytes();
            if name == b"." || name == b".." {
                continue;
            }
            budget.entry(parent, name, child_depth)?;
            let path = parent.map_or_else(
                || FsPath::from_relative(name, limits.path_bytes),
                |parent| parent.join(name, limits.path_bytes),
            )?;
            children.push((name.to_vec(), path, child_depth));
        }
        children.sort_by(|left, right| left.0.cmp(&right.0));
        Ok(children)
    }

    #[allow(clippy::too_many_arguments)]
    fn capture_entry(
        directory: &OwnedFd,
        name: &CStr,
        raw_name: &[u8],
        path: &FsPath,
        depth: u64,
        initial: &Stat,
        limits: RootfsLimits,
        state: &mut CaptureState,
    ) -> Result<FsEntry> {
        match FileType::from_raw_mode(initial.st_mode) {
            FileType::RegularFile => capture_regular(directory, name, path, initial, limits, state),
            FileType::Directory => {
                capture_directory(directory, name, path, depth, initial, limits, state)
            }
            FileType::Symlink => {
                capture_symlink(directory, name, raw_name, path, initial, limits, state)
            }
            FileType::Fifo => capture_special(
                directory,
                raw_name,
                path,
                initial,
                limits,
                state,
                EntryKind::Fifo,
            ),
            FileType::CharacterDevice => capture_special(
                directory,
                raw_name,
                path,
                initial,
                limits,
                state,
                EntryKind::Character {
                    major: major(initial.st_rdev),
                    minor: minor(initial.st_rdev),
                },
            ),
            FileType::BlockDevice => capture_special(
                directory,
                raw_name,
                path,
                initial,
                limits,
                state,
                EntryKind::Block {
                    major: major(initial.st_rdev),
                    minor: minor(initial.st_rdev),
                },
            ),
            FileType::Socket | FileType::Unknown => {
                bail!("unsupported filesystem object at {}", path.display())
            }
        }
    }

    fn capture_regular(
        directory: &OwnedFd,
        name: &CStr,
        path: &FsPath,
        initial: &Stat,
        limits: RootfsLimits,
        state: &mut CaptureState,
    ) -> Result<FsEntry> {
        let fd = openat(
            directory,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        let opened = fstat(&fd)?;
        ensure_same_object(initial, &opened, path)?;
        let initial_xattrs = read_fd_xattrs(&fd, limits, Some(&mut state.budget))?;
        let declared = u64::try_from(opened.st_size)
            .with_context(|| format!("negative regular size at {}", path.display()))?;
        state.budget.content(declared)?;
        let mut file = File::from(fd);
        let (digest, size) = if state.keep_contents {
            let mut staged = tempfile::NamedTempFile::new_in(&state.content_parent)?;
            let result = copy_and_digest(&mut file, staged.as_file_mut(), Some(declared))?;
            staged.as_file_mut().sync_all()?;
            state
                .tree
                .contents
                .entry(result.0.to_string())
                .or_insert_with(|| staged.into_temp_path());
            result
        } else {
            copy_and_digest(&mut file, std::io::sink(), Some(declared))?
        };
        let final_stat = fstat(&file)?;
        let final_xattrs = read_fd_xattrs(&file, limits, None)?;
        ensure_stable(
            &opened,
            &final_stat,
            &initial_xattrs,
            &final_xattrs,
            &path.display(),
        )?;
        if size != declared {
            bail!("regular file changed while capturing {}", path.display());
        }
        if opened.st_nlink > 1 {
            state
                .hardlinks
                .entry((u128::from(opened.st_dev), u128::from(opened.st_ino)))
                .or_default()
                .push(path.clone());
        }
        Ok(FsEntry {
            metadata: metadata(&opened, initial_xattrs)?,
            kind: EntryKind::Regular {
                digest,
                size,
                hardlink: None,
            },
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn capture_directory(
        directory: &OwnedFd,
        name: &CStr,
        path: &FsPath,
        depth: u64,
        initial: &Stat,
        limits: RootfsLimits,
        state: &mut CaptureState,
    ) -> Result<FsEntry> {
        let fd = openat(
            directory,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        let opened = fstat(&fd)?;
        ensure_same_object(initial, &opened, path)?;
        let initial_xattrs = read_fd_xattrs(&fd, limits, Some(&mut state.budget))?;
        walk_directory(&fd, Some(path), depth, limits, state)?;
        let final_stat = fstat(&fd)?;
        let final_xattrs = read_fd_xattrs(&fd, limits, None)?;
        ensure_stable(
            &opened,
            &final_stat,
            &initial_xattrs,
            &final_xattrs,
            &path.display(),
        )?;
        Ok(FsEntry {
            metadata: metadata(&opened, initial_xattrs)?,
            kind: EntryKind::Directory,
        })
    }

    fn capture_symlink(
        directory: &OwnedFd,
        name: &CStr,
        raw_name: &[u8],
        path: &FsPath,
        initial: &Stat,
        limits: RootfsLimits,
        state: &mut CaptureState,
    ) -> Result<FsEntry> {
        let initial_xattrs =
            read_path_xattrs(directory, raw_name, limits, Some(&mut state.budget))?;
        let target = readlinkat(directory, name, Vec::new())?.into_bytes();
        enforce(
            "captured link target",
            limits.link_target_bytes,
            usize_to_u64(target.len()),
        )?;
        let final_stat = statat(directory, name, AtFlags::SYMLINK_NOFOLLOW)?;
        let final_xattrs = read_path_xattrs(directory, raw_name, limits, None)?;
        ensure_stable(
            initial,
            &final_stat,
            &initial_xattrs,
            &final_xattrs,
            &path.display(),
        )?;
        if readlinkat(directory, name, Vec::new())?.into_bytes() != target {
            bail!("symlink changed while capturing {}", path.display());
        }
        Ok(FsEntry {
            metadata: metadata(initial, initial_xattrs)?,
            kind: EntryKind::Symlink(target.into_boxed_slice()),
        })
    }

    fn capture_special(
        directory: &OwnedFd,
        raw_name: &[u8],
        path: &FsPath,
        initial: &Stat,
        limits: RootfsLimits,
        state: &mut CaptureState,
        kind: EntryKind,
    ) -> Result<FsEntry> {
        let initial_xattrs =
            read_path_xattrs(directory, raw_name, limits, Some(&mut state.budget))?;
        let c_name = c_name(raw_name)?;
        let final_stat = statat(directory, &c_name, AtFlags::SYMLINK_NOFOLLOW)?;
        let final_xattrs = read_path_xattrs(directory, raw_name, limits, None)?;
        ensure_stable(
            initial,
            &final_stat,
            &initial_xattrs,
            &final_xattrs,
            &path.display(),
        )?;
        Ok(FsEntry {
            metadata: metadata(initial, initial_xattrs)?,
            kind,
        })
    }

    fn normalize_hardlinks(
        inventory: &mut Inventory,
        groups: BTreeMap<(u128, u128), Vec<FsPath>>,
    ) -> Result<()> {
        for mut paths in groups.into_values() {
            if paths.len() < 2 {
                continue;
            }
            paths.sort();
            let anchor = paths[0].clone();
            let anchor_entry = inventory
                .entries
                .get(&anchor)
                .with_context(|| format!("hardlink anchor disappeared: {}", anchor.display()))?
                .clone();
            for path in paths.into_iter().skip(1) {
                let entry = inventory
                    .entries
                    .get_mut(&path)
                    .with_context(|| format!("hardlink member disappeared: {}", path.display()))?;
                if entry.metadata != anchor_entry.metadata {
                    bail!(
                        "hardlink metadata disagrees: {} -> {}",
                        path.display(),
                        anchor.display()
                    );
                }
                let (
                    EntryKind::Regular {
                        digest,
                        size,
                        hardlink,
                    },
                    EntryKind::Regular {
                        digest: anchor_digest,
                        size: anchor_size,
                        ..
                    },
                ) = (&mut entry.kind, &anchor_entry.kind)
                else {
                    bail!("hardlink member is not regular: {}", path.display());
                };
                if digest != anchor_digest || size != anchor_size {
                    bail!(
                        "hardlink content disagrees: {} -> {}",
                        path.display(),
                        anchor.display()
                    );
                }
                *hardlink = Some(anchor.clone());
            }
        }
        Ok(())
    }

    fn metadata(stat: &Stat, xattrs: Xattrs) -> Result<Metadata> {
        Ok(Metadata {
            mode: stat.st_mode & 0o7777,
            uid: stat.st_uid,
            gid: stat.st_gid,
            mtime: Timestamp {
                seconds: stat.st_mtime,
                nanos: u32::try_from(stat.st_mtime_nsec).context("mtime nanoseconds overflow")?,
            },
            xattrs,
        })
    }

    fn ensure_same_object(initial: &Stat, opened: &Stat, path: &FsPath) -> Result<()> {
        if initial.st_dev != opened.st_dev
            || initial.st_ino != opened.st_ino
            || FileType::from_raw_mode(initial.st_mode) != FileType::from_raw_mode(opened.st_mode)
        {
            bail!("filesystem object changed while opening {}", path.display());
        }
        Ok(())
    }

    fn ensure_stable(
        initial: &Stat,
        final_stat: &Stat,
        initial_xattrs: &Xattrs,
        final_xattrs: &Xattrs,
        path: &str,
    ) -> Result<()> {
        if initial.st_dev != final_stat.st_dev
            || initial.st_ino != final_stat.st_ino
            || initial.st_mode != final_stat.st_mode
            || initial.st_nlink != final_stat.st_nlink
            || initial.st_uid != final_stat.st_uid
            || initial.st_gid != final_stat.st_gid
            || initial.st_rdev != final_stat.st_rdev
            || initial.st_size != final_stat.st_size
            || initial.st_mtime != final_stat.st_mtime
            || initial.st_mtime_nsec != final_stat.st_mtime_nsec
            || initial.st_ctime != final_stat.st_ctime
            || initial.st_ctime_nsec != final_stat.st_ctime_nsec
            || initial_xattrs != final_xattrs
        {
            bail!("filesystem object changed while capturing {path}");
        }
        Ok(())
    }

    fn read_fd_xattrs(
        fd: impl AsFd,
        limits: RootfsLimits,
        budget: Option<&mut CaptureBudget>,
    ) -> Result<Xattrs> {
        let names = list_fd_xattr_names(&fd, limits.xattr_names_bytes)?;
        read_xattr_values(&names, limits, budget, |name, buffer| {
            fgetxattr(&fd, name, buffer).map_err(Into::into)
        })
    }

    fn read_path_xattrs(
        parent: &OwnedFd,
        name: &[u8],
        limits: RootfsLimits,
        budget: Option<&mut CaptureBudget>,
    ) -> Result<Xattrs> {
        let path = proc_path(parent, name);
        let names = list_path_xattr_names(&path, limits.xattr_names_bytes)?;
        read_xattr_values(&names, limits, budget, |name, buffer| {
            lgetxattr(&path, name, buffer).map_err(Into::into)
        })
    }

    fn list_fd_xattr_names(fd: impl AsFd, limit: usize) -> Result<Vec<u8>> {
        let empty: &mut [u8] = &mut [];
        let required = flistxattr(&fd, empty)?;
        if required > limit {
            bail!("filesystem xattr name list exceeds limit");
        }
        let mut names = vec![0_u8; required];
        let read = flistxattr(&fd, &mut names)?;
        names.truncate(read);
        Ok(names)
    }

    fn list_path_xattr_names(path: &Path, limit: usize) -> Result<Vec<u8>> {
        let empty: &mut [u8] = &mut [];
        let required = llistxattr(path, empty)?;
        if required > limit {
            bail!("filesystem xattr name list exceeds limit");
        }
        let mut names = vec![0_u8; required];
        let read = llistxattr(path, &mut names)?;
        names.truncate(read);
        Ok(names)
    }

    fn split_xattr_names(names: &[u8]) -> Result<impl Iterator<Item = &[u8]>> {
        if !names.is_empty() && !names.ends_with(&[0]) {
            bail!("filesystem returned malformed xattr names");
        }
        Ok(names
            .split(|byte| *byte == 0)
            .filter(|name| !name.is_empty()))
    }

    fn read_xattr_values(
        names: &[u8],
        limits: RootfsLimits,
        mut budget: Option<&mut CaptureBudget>,
        mut get: impl FnMut(&[u8], &mut [u8]) -> Result<usize>,
    ) -> Result<Xattrs> {
        let mut result = Xattrs::new();
        if let Some(budget) = &mut budget {
            budget.xattrs(names.len())?;
        }
        for name in split_xattr_names(names)? {
            let required = get(name, &mut [])?;
            if required > limits.xattr_value_bytes {
                bail!("filesystem xattr value exceeds capture limit");
            }
            if let Some(budget) = &mut budget {
                budget.xattrs(required)?;
            }
            let mut value = vec![0_u8; required];
            let read = get(name, &mut value)?;
            value.truncate(read);
            if result
                .insert(name.into(), value.into_boxed_slice())
                .is_some()
            {
                bail!("filesystem returned duplicate xattr name");
            }
        }
        Ok(result)
    }

    #[derive(Default)]
    struct ChangeSet {
        root: Option<Metadata>,
        removals: BTreeSet<FsPath>,
        opaques: BTreeSet<FsPath>,
        entries: BTreeMap<FsPath, FsEntry>,
    }

    fn compare(before: &Inventory, after: &Inventory) -> Result<ChangeSet> {
        let root = match (&before.root, &after.root) {
            (Some(left), Some(right)) if left != right => Some(right.clone()),
            (Some(_), Some(_)) | (None, None) => None,
            _ => bail!("filesystem inventories disagree about root metadata"),
        };
        let all_paths = before
            .entries
            .keys()
            .chain(after.entries.keys())
            .cloned()
            .collect::<BTreeSet<_>>();
        let mut changes = ChangeSet {
            root,
            ..ChangeSet::default()
        };
        for path in all_paths {
            match (before.entries.get(&path), after.entries.get(&path)) {
                (Some(left), Some(right)) if left == right => {}
                (_, Some(right)) => {
                    changes.entries.insert(path, right.clone());
                }
                (Some(_), None) => insert_removal(&mut changes.removals, path),
                (None, None) => unreachable!(),
            }
        }
        select_opaque_removals(before, after, &mut changes);
        promote_changed_hardlink_anchors(after, &mut changes.entries)?;
        Ok(changes)
    }

    fn insert_removal(removals: &mut BTreeSet<FsPath>, path: FsPath) {
        if removals
            .iter()
            .any(|parent| path == *parent || path.is_descendant_of(parent))
        {
            return;
        }
        removals.retain(|child| !child.is_descendant_of(&path));
        removals.insert(path);
    }

    fn select_opaque_removals(before: &Inventory, after: &Inventory, changes: &mut ChangeSet) {
        for (directory, entry) in &after.entries {
            if !matches!(entry.kind, EntryKind::Directory)
                || !matches!(
                    before.entries.get(directory).map(|entry| &entry.kind),
                    Some(EntryKind::Directory)
                )
            {
                continue;
            }
            let had_children = before
                .entries
                .keys()
                .any(|path| path.is_descendant_of(directory));
            let retained_old_child = before
                .entries
                .keys()
                .any(|path| path.is_descendant_of(directory) && after.entries.contains_key(path));
            if had_children && !retained_old_child {
                changes.opaques.insert(directory.clone());
                changes
                    .removals
                    .retain(|path| !path.is_descendant_of(directory));
            }
        }
    }

    fn promote_changed_hardlink_anchors(
        after: &Inventory,
        entries: &mut BTreeMap<FsPath, FsEntry>,
    ) -> Result<()> {
        let anchors = entries
            .values()
            .filter_map(|entry| match &entry.kind {
                EntryKind::Regular {
                    hardlink: Some(anchor),
                    ..
                } => Some(anchor.clone()),
                _ => None,
            })
            .collect::<BTreeSet<_>>();
        for anchor in anchors {
            let entry = after
                .entries
                .get(&anchor)
                .with_context(|| format!("hardlink anchor is absent: {}", anchor.display()))?;
            entries.insert(anchor, entry.clone());
        }
        Ok(())
    }

    fn encode_layer(
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

    fn append_whiteout<W: std::io::Write>(builder: &mut Builder<W>, path: &FsPath) -> Result<()> {
        let mut header = tar_header(0, 0, 0, 0, 0, EntryType::Regular)?;
        builder.append_data(&mut header, path_buf(path), std::io::empty())?;
        Ok(())
    }

    fn append_entry<W: std::io::Write>(
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

    fn append_special<W: std::io::Write>(
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

    fn append_pax_metadata<W: std::io::Write>(
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

    fn tar_header(
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

    fn pax_timestamp(timestamp: Timestamp) -> String {
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

    struct BoundedWriter<W> {
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

    fn copy_and_digest(
        mut reader: impl Read,
        mut writer: impl std::io::Write,
        expected_size: Option<u64>,
    ) -> Result<(Digest, u64)> {
        let mut hasher = Sha256::new();
        let mut size = 0_u64;
        let mut buffer = vec![0_u8; 1024 * 1024];
        loop {
            let read = reader.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            size = size
                .checked_add(usize_to_u64(read))
                .context("content size overflow")?;
            if expected_size.is_some_and(|expected| size > expected) {
                bail!("content exceeds declared size");
            }
            writer.write_all(&buffer[..read])?;
            hasher.update(&buffer[..read]);
        }
        if let Some(expected) = expected_size
            && size != expected
        {
            bail!("content size mismatch: expected {expected}, received {size}");
        }
        Ok((finish_sha256(hasher), size))
    }

    #[cfg(test)]
    fn sha256_digest(bytes: &[u8]) -> Digest {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        finish_sha256(hasher)
    }

    fn finish_sha256(hasher: Sha256) -> Digest {
        let mut hexadecimal = String::with_capacity(64);
        for byte in hasher.finalize() {
            use std::fmt::Write as _;
            write!(&mut hexadecimal, "{byte:02x}").expect("writing to String cannot fail");
        }
        Digest::from_str(&format!("sha256:{hexadecimal}"))
            .expect("SHA-256 always forms a valid OCI digest")
    }

    fn encode_base64(bytes: &[u8]) -> String {
        const TABLE: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut encoded = Vec::with_capacity(bytes.len().div_ceil(3) * 4);
        for chunk in bytes.chunks(3) {
            let first = chunk[0];
            let second = chunk.get(1).copied().unwrap_or(0);
            let third = chunk.get(2).copied().unwrap_or(0);
            encoded.push(TABLE[usize::from(first >> 2)]);
            encoded.push(TABLE[usize::from((first & 0x03) << 4 | second >> 4)]);
            encoded.push(if chunk.len() > 1 {
                TABLE[usize::from((second & 0x0f) << 2 | third >> 6)]
            } else {
                b'='
            });
            encoded.push(if chunk.len() > 2 {
                TABLE[usize::from(third & 0x3f)]
            } else {
                b'='
            });
        }
        String::from_utf8(encoded).expect("base64 is ASCII")
    }

    fn decode_base64(bytes: &[u8]) -> Result<Vec<u8>> {
        if !bytes.len().is_multiple_of(4) {
            bail!("invalid base64 xattr value");
        }
        let mut decoded = Vec::with_capacity(bytes.len() / 4 * 3);
        for (index, chunk) in bytes.chunks_exact(4).enumerate() {
            let last = index + 1 == bytes.len() / 4;
            let a = base64_value(chunk[0])?;
            let b = base64_value(chunk[1])?;
            let c = if chunk[2] == b'=' {
                0
            } else {
                base64_value(chunk[2])?
            };
            let d = if chunk[3] == b'=' {
                0
            } else {
                base64_value(chunk[3])?
            };
            if (chunk[3] == b'=' || chunk[2] == b'=') && (chunk[3] != b'=' || !last) {
                bail!("invalid base64 xattr padding");
            }
            decoded.push((a << 2) | (b >> 4));
            if chunk[2] != b'=' {
                decoded.push((b << 4) | (c >> 2));
            }
            if chunk[3] != b'=' {
                decoded.push((c << 6) | d);
            }
        }
        Ok(decoded)
    }

    fn base64_value(byte: u8) -> Result<u8> {
        match byte {
            b'A'..=b'Z' => Ok(byte - b'A'),
            b'a'..=b'z' => Ok(byte - b'a' + 26),
            b'0'..=b'9' => Ok(byte - b'0' + 52),
            b'+' => Ok(62),
            b'/' => Ok(63),
            _ => bail!("invalid base64 xattr byte"),
        }
    }

    fn ensure_no_mounts(root: &OwnedFd) -> Result<()> {
        let root_path = std::fs::read_link(proc_fd_path(root))?;
        let root_bytes = root_path.as_os_str().as_bytes();
        let mountinfo = std::fs::read("/proc/self/mountinfo")?;
        ensure_mountinfo_clear(root_bytes, &mountinfo)
    }

    fn ensure_mountinfo_clear(root_bytes: &[u8], mountinfo: &[u8]) -> Result<()> {
        if let Some(mountpoint) = mount_below(root_bytes, mountinfo)? {
            bail!(
                "rootfs still contains a mount at {}; capture requires all runtime mounts to be removed",
                display_bytes(&mountpoint)
            );
        }
        Ok(())
    }

    fn mount_below(root_bytes: &[u8], mountinfo: &[u8]) -> Result<Option<Vec<u8>>> {
        for line in mountinfo
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
        {
            let fields = line.split(|byte| *byte == b' ').collect::<Vec<_>>();
            if fields.len() < 5 {
                bail!("malformed /proc/self/mountinfo record");
            }
            let mountpoint = decode_mountinfo_path(fields[4])?;
            if mountpoint == root_bytes
                || (mountpoint.len() > root_bytes.len()
                    && mountpoint.starts_with(root_bytes)
                    && mountpoint[root_bytes.len()] == b'/')
            {
                return Ok(Some(mountpoint));
            }
        }
        Ok(None)
    }

    fn decode_mountinfo_path(raw: &[u8]) -> Result<Vec<u8>> {
        let mut decoded = Vec::with_capacity(raw.len());
        let mut index = 0;
        while index < raw.len() {
            if raw[index] == b'\\' {
                let escape = raw
                    .get(index + 1..index + 4)
                    .context("truncated mountinfo escape")?;
                let value = match escape {
                    b"040" => b' ',
                    b"011" => b'\t',
                    b"012" => b'\n',
                    b"134" => b'\\',
                    _ => bail!("unknown mountinfo path escape"),
                };
                decoded.push(value);
                index += 4;
            } else {
                decoded.push(raw[index]);
                index += 1;
            }
        }
        Ok(decoded)
    }

    fn reopen_directory(directory: &OwnedFd) -> Result<OwnedFd> {
        Ok(openat(
            directory,
            c".",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?)
    }

    fn proc_fd_path(fd: &OwnedFd) -> PathBuf {
        PathBuf::from(format!("/proc/self/fd/{}", fd.as_raw_fd()))
    }

    fn proc_path(parent: &OwnedFd, name: &[u8]) -> PathBuf {
        let mut bytes = format!("/proc/self/fd/{}/", parent.as_raw_fd()).into_bytes();
        bytes.extend_from_slice(name);
        PathBuf::from(OsStr::from_bytes(&bytes))
    }

    fn c_name(bytes: &[u8]) -> Result<std::ffi::CString> {
        std::ffi::CString::new(bytes).context("filesystem name contains NUL")
    }

    fn os(bytes: &[u8]) -> &OsStr {
        OsStr::from_bytes(bytes)
    }

    fn path_buf(path: &FsPath) -> PathBuf {
        PathBuf::from(os(path.as_bytes()))
    }

    fn default_directory() -> Metadata {
        Metadata {
            mode: 0o755,
            uid: 0,
            gid: 0,
            mtime: Timestamp {
                seconds: 0,
                nanos: 0,
            },
            xattrs: Xattrs::new(),
        }
    }

    fn enforce(noun: &str, limit: u64, observed: u64) -> Result<()> {
        if observed > limit {
            bail!("{noun} limit exceeded: limit {limit}, observed {observed}");
        }
        Ok(())
    }

    fn checked_total(current: u64, added: u64, limit: u64, noun: &str) -> Result<u64> {
        let observed = current
            .checked_add(added)
            .with_context(|| format!("{noun} overflow"))?;
        enforce(noun, limit, observed)?;
        Ok(observed)
    }

    fn usize_to_u64(value: usize) -> u64 {
        u64::try_from(value).unwrap_or(u64::MAX)
    }

    fn normalized_relative_len(raw: &[u8]) -> Result<u64> {
        if raw.contains(&0) || raw.starts_with(b"/") {
            bail!("unsafe filesystem path: {}", display_bytes(raw));
        }
        let mut length = 0_u64;
        for component in raw.split(|byte| *byte == b'/') {
            if component.is_empty() || component == b"." {
                continue;
            }
            if component == b".." {
                bail!("unsafe filesystem path: {}", display_bytes(raw));
            }
            length = length
                .checked_add(u64::from(length != 0))
                .and_then(|bytes| bytes.checked_add(usize_to_u64(component.len())))
                .context("filesystem path length overflow")?;
        }
        Ok(length)
    }

    fn display_bytes(bytes: &[u8]) -> String {
        bytes
            .iter()
            .flat_map(|byte| std::ascii::escape_default(*byte))
            .map(char::from)
            .collect()
    }

    #[cfg(test)]
    mod tests {
        use std::io::{Cursor, Read as _};
        use std::os::unix::fs::MetadataExt as _;

        use rustix::process::geteuid;

        use super::*;

        struct TestLayer {
            descriptor: Descriptor,
            bytes: Vec<u8>,
            diff_id: Digest,
        }

        #[test]
        fn rejects_path_traversal_and_normalizes_safe_components() {
            assert!(FsPath::from_relative(b"../../escape", 1024).is_err());
            assert!(FsPath::from_relative(b"/absolute", 1024).is_err());
            assert_eq!(
                FsPath::from_relative(b"safe/./path", 1024)
                    .expect("path")
                    .as_bytes(),
                b"safe/path"
            );
        }

        #[test]
        fn ordered_layers_apply_whiteout_and_type_replacement() {
            if !geteuid().is_root() {
                return;
            }
            let lower = tar_layer(|builder| {
                append_test_directory(builder, b"dir")?;
                append_test_file(builder, b"dir/old", b"old")?;
                append_test_file(builder, b"value", b"lower")
            });
            let upper = tar_layer(|builder| {
                append_test_file(builder, b"dir/.wh..wh..opq", b"")?;
                append_test_file(builder, b"dir/new", b"new")?;
                append_test_file(builder, b".wh.value", b"")?;
                append_test_file(builder, b"value", b"upper")
            });
            let workspace = tempfile::tempdir().expect("workspace");
            let rootfs = materialize(workspace.path(), &[&lower, &upper]).expect("materialize");
            assert!(!rootfs.path().join("dir/old").exists());
            assert_eq!(
                std::fs::read(rootfs.path().join("dir/new")).unwrap(),
                b"new"
            );
            assert_eq!(
                std::fs::read(rootfs.path().join("value")).unwrap(),
                b"upper"
            );
        }

        #[test]
        fn forward_hardlink_is_materialized_as_one_inode() {
            if !geteuid().is_root() {
                return;
            }
            let layer = tar_layer(|builder| {
                append_test_hardlink(builder, b"alias", b"target")?;
                append_test_file(builder, b"target", b"shared")
            });
            let workspace = tempfile::tempdir().expect("workspace");
            let rootfs = materialize(workspace.path(), &[&layer]).expect("materialize");
            let target = std::fs::metadata(rootfs.path().join("target")).unwrap();
            let alias = std::fs::metadata(rootfs.path().join("alias")).unwrap();
            assert_eq!(target.ino(), alias.ino());
            assert_eq!(target.nlink(), 2);
        }

        #[test]
        fn stopped_capture_is_deterministic_and_preserves_raw_hardlinks() {
            if !geteuid().is_root() {
                return;
            }
            let layer = tar_layer(|builder| append_test_file(builder, b"base", b"base"));
            let workspace = tempfile::tempdir().expect("workspace");
            let rootfs = materialize(workspace.path(), &[&layer]).expect("materialize");
            let raw = rootfs.path().join(OsStr::from_bytes(b"raw-\xff"));
            std::fs::write(&raw, b"changed").unwrap();
            std::fs::hard_link(&raw, rootfs.path().join("hard")).unwrap();
            std::fs::remove_file(rootfs.path().join("base")).unwrap();

            let first = rootfs.capture().expect("first capture");
            let second = rootfs.capture().expect("second capture");
            assert_eq!(first.diff_id, second.diff_id);
            assert_eq!(first.size, second.size);
            let mut first_bytes = Vec::new();
            first.open().unwrap().read_to_end(&mut first_bytes).unwrap();
            let mut second_bytes = Vec::new();
            second
                .open()
                .unwrap()
                .read_to_end(&mut second_bytes)
                .unwrap();
            assert_eq!(first_bytes, second_bytes);

            let mut archive = Archive::new(Cursor::new(first_bytes));
            let mut observed = Vec::new();
            for entry in archive.entries().unwrap() {
                let entry = entry.unwrap();
                observed.push((
                    entry.path_bytes().into_owned(),
                    entry.header().entry_type(),
                    entry.link_name_bytes().map(std::borrow::Cow::into_owned),
                ));
            }
            assert!(observed.iter().any(|entry| entry.0 == b".wh.base"));
            assert!(observed.iter().any(|entry| entry.0 == b"raw-\xff"));
            assert!(observed.iter().any(|entry| entry.1 == EntryType::Link));
        }

        #[test]
        fn captured_binary_xattr_survives_layer_materialization() {
            if !geteuid().is_root() {
                return;
            }
            let base = tar_layer(|_builder| Ok(()));
            let first_workspace = tempfile::tempdir().expect("first workspace");
            let first = materialize(first_workspace.path(), &[&base]).expect("first rootfs");
            let path = first.path().join("value");
            std::fs::write(&path, b"content").expect("write value");
            let file = File::open(&path).expect("open value");
            fsetxattr(
                &file,
                b"user.percent%name".as_slice(),
                b"binary\0value",
                XattrFlags::empty(),
            )
            .expect("set binary xattr");

            let captured = first.capture().expect("capture xattr");
            let mut bytes = Vec::new();
            captured
                .open()
                .expect("open capture")
                .read_to_end(&mut bytes)
                .expect("read capture");
            let captured_layer = TestLayer {
                descriptor: Descriptor::new(
                    captured.media_type,
                    captured.size,
                    captured.diff_id.clone(),
                ),
                bytes,
                diff_id: captured.diff_id,
            };

            let second_workspace = tempfile::tempdir().expect("second workspace");
            let second = materialize(second_workspace.path(), &[&captured_layer])
                .expect("materialize captured xattr");
            let fd = open(
                second.path().join("value"),
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .expect("open materialized value");
            let xattrs = read_fd_xattrs(&fd, RootfsLimits::default(), None)
                .expect("read materialized xattrs");

            assert_eq!(
                xattrs
                    .get(b"user.percent%name".as_slice())
                    .map(AsRef::as_ref),
                Some(b"binary\0value".as_slice())
            );
        }

        #[test]
        fn capture_keeps_mount_path_and_outside_hardlink_in_one_group() {
            if !geteuid().is_root() {
                return;
            }
            let base = tar_layer(|builder| {
                append_test_directory(builder, b"mount")?;
                append_test_file(builder, b"mount/value", b"old")?;
                append_test_hardlink(builder, b"outside", b"mount/value")
            });
            let workspace = tempfile::tempdir().expect("workspace");
            let rootfs = materialize(workspace.path(), &[&base]).expect("materialize");
            let mut outside = std::fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(rootfs.path().join("outside"))
                .expect("open outside hardlink");
            std::io::Write::write_all(&mut outside, b"new").expect("write hardlink group");
            outside.sync_all().expect("sync hardlink group");

            let captured = rootfs.capture().expect("capture complete rootfs");
            let entries = captured_entries(&captured);
            let mount = entries
                .iter()
                .find(|entry| entry.0 == b"mount/value")
                .expect("mount path entry");
            let outside = entries
                .iter()
                .find(|entry| entry.0 == b"outside")
                .expect("outside path entry");
            assert!(
                [mount, outside]
                    .iter()
                    .any(|entry| entry.1 == EntryType::Regular && entry.3 == b"new")
            );
            assert!(
                [mount, outside]
                    .iter()
                    .any(|entry| entry.1 == EntryType::Link)
            );
        }

        #[test]
        fn opaque_replaces_lower_non_directory_before_children_are_applied() {
            if !geteuid().is_root() {
                return;
            }
            for lower_is_symlink in [false, true] {
                let lower = tar_layer(|builder| {
                    if lower_is_symlink {
                        append_test_symlink(builder, b"a", b"target")
                    } else {
                        append_test_file(builder, b"a", b"file")
                    }
                });
                let upper = tar_layer(|builder| {
                    append_test_file(builder, b"a/.wh..wh..opq", b"")?;
                    append_test_directory(builder, b"a")?;
                    append_test_file(builder, b"a/child", b"child")
                });
                let workspace = tempfile::tempdir().expect("workspace");
                let rootfs = materialize(workspace.path(), &[&lower, &upper])
                    .expect("opaque type replacement");
                assert_eq!(
                    std::fs::read(rootfs.path().join("a/child")).expect("child"),
                    b"child"
                );
            }
        }

        #[test]
        fn normalized_duplicate_tar_paths_are_rejected() {
            let layer = tar_layer(|builder| {
                append_test_file(builder, b"a/b", b"first")?;
                append_test_file(builder, b"a//b", b"second")
            });
            let error =
                scan_test_layer(&layer, RootfsLimits::default()).expect_err("normalized duplicate");
            assert!(error.to_string().contains("duplicate OCI Layer path"));
        }

        #[test]
        fn traversal_and_symlink_escape_fail_without_writing_outside() {
            if !geteuid().is_root() {
                return;
            }
            let mut traversal = tar_layer(|builder| append_test_file(builder, b"safe-name", b"x"));
            replace_first_tar_path(&mut traversal.bytes, b"../escape");
            refresh_test_layer(&mut traversal);
            let workspace = tempfile::tempdir().expect("workspace");
            assert!(materialize(workspace.path(), &[&traversal]).is_err());

            let outside = tempfile::tempdir().expect("outside");
            let target = outside.path().as_os_str().as_bytes().to_vec();
            let lower = tar_layer(|builder| append_test_symlink(builder, b"link", &target));
            let upper = tar_layer(|builder| append_test_file(builder, b"link/escaped", b"bad"));
            let workspace = tempfile::tempdir().expect("workspace");
            assert!(materialize(workspace.path(), &[&lower, &upper]).is_err());
            assert!(!outside.path().join("escaped").exists());
        }

        #[test]
        fn hardlink_cycle_and_explicit_budgets_fail_closed() {
            if !geteuid().is_root() {
                return;
            }
            let cycle = tar_layer(|builder| {
                append_test_hardlink(builder, b"first", b"second")?;
                append_test_hardlink(builder, b"second", b"first")
            });
            let workspace = tempfile::tempdir().expect("workspace");
            assert!(materialize(workspace.path(), &[&cycle]).is_err());

            let paths = tar_layer(|builder| {
                append_test_file(builder, b"aa", b"")?;
                append_test_file(builder, b"bb", b"")
            });
            let limits = RootfsLimits {
                total_path_bytes: 3,
                ..RootfsLimits::default()
            };
            let error = scan_test_layer(&paths, limits).expect_err("path byte budget");
            assert!(
                error
                    .to_string()
                    .contains("Layer raw path bytes limit exceeded")
            );

            let metadata = Metadata {
                mode: 0o644,
                uid: 0,
                gid: 0,
                mtime: Timestamp {
                    seconds: 0,
                    nanos: 0,
                },
                xattrs: BTreeMap::from([(
                    b"user.budget".to_vec().into_boxed_slice(),
                    b"value".to_vec().into_boxed_slice(),
                )]),
            };
            let xattrs = tar_layer(|builder| {
                append_pax_metadata(builder, &metadata)?;
                append_test_file(builder, b"value", b"")
            });
            let limits = RootfsLimits {
                total_xattr_bytes: 4,
                ..RootfsLimits::default()
            };
            let error = scan_test_layer(&xattrs, limits).expect_err("xattr byte budget");
            assert!(
                error
                    .to_string()
                    .contains("Layer xattr bytes limit exceeded")
            );

            let lower = tar_layer(|builder| {
                append_test_directory(builder, b"tree")?;
                append_test_file(builder, b"tree/a", b"")?;
                append_test_file(builder, b"tree/b", b"")
            });
            let upper = tar_layer(|builder| append_test_file(builder, b".wh.tree", b""));
            let limits = RootfsLimits {
                cleanup_entries: 1,
                ..RootfsLimits::default()
            };
            let workspace = tempfile::tempdir().expect("workspace");
            assert!(materialize_with_limits(workspace.path(), &[&lower, &upper], limits).is_err());
        }

        #[test]
        fn pax_xattr_suffix_is_literal_and_raw_unrepresentable_name_fails() {
            let percent = b"user.percent%name".to_vec().into_boxed_slice();
            let metadata = Metadata {
                mode: 0o644,
                uid: 0,
                gid: 0,
                mtime: Timestamp {
                    seconds: 0,
                    nanos: 0,
                },
                xattrs: BTreeMap::from([(percent.clone(), b"value".to_vec().into_boxed_slice())]),
            };
            let mut builder = Builder::new(Vec::new());
            append_pax_metadata(&mut builder, &metadata).expect("literal percent xattr");
            builder.finish().expect("finish PAX archive");
            let bytes = builder.into_inner().expect("PAX bytes");
            assert!(
                bytes
                    .windows(b"SCHILY.xattr.user.percent%name".len())
                    .any(|window| { window == b"SCHILY.xattr.user.percent%name" })
            );

            let raw = Metadata {
                xattrs: BTreeMap::from([(
                    b"user.raw-\xff".to_vec().into_boxed_slice(),
                    Box::<[u8]>::default(),
                )]),
                ..metadata
            };
            let mut builder = Builder::new(Vec::new());
            let error = append_pax_metadata(&mut builder, &raw)
                .expect_err("raw PAX xattr name is not representable");
            assert!(
                error
                    .to_string()
                    .contains("cannot be represented literally")
            );
        }

        #[test]
        fn mountinfo_descendant_is_a_positive_fail_closed_signal() {
            let mountinfo = b"36 29 0:32 / / rw,relatime - ext4 /dev/root rw\n\
                40 36 0:45 / /state/rootfs/runtime\\040mount rw - tmpfs tmpfs rw\n";
            assert_eq!(
                mount_below(b"/state/rootfs", mountinfo).expect("mountinfo"),
                Some(b"/state/rootfs/runtime mount".to_vec())
            );
            assert!(ensure_mountinfo_clear(b"/state/rootfs", mountinfo).is_err());
            assert_eq!(
                mount_below(b"/other/rootfs", mountinfo).expect("unrelated mountinfo"),
                None
            );
        }

        #[test]
        fn materialization_budgets_are_shared_across_layers() {
            if !geteuid().is_root() {
                return;
            }
            let first = tar_layer(|builder| append_test_file(builder, b"first", b"1"));
            let second = tar_layer(|builder| append_test_file(builder, b"second", b"2"));

            let workspace = tempfile::tempdir().expect("workspace");
            let limits = RootfsLimits {
                total_uncompressed_bytes: usize_to_u64(first.bytes.len() + second.bytes.len() - 1),
                ..RootfsLimits::default()
            };
            let error = materialize_with_limits(workspace.path(), &[&first, &second], limits)
                .expect_err("second Layer must exceed the remaining uncompressed byte");
            assert!(
                error
                    .to_string()
                    .contains("uncompressed Layer limit exceeded")
            );

            let workspace = tempfile::tempdir().expect("workspace");
            let limits = RootfsLimits {
                entries: 1,
                ..RootfsLimits::default()
            };
            let error = materialize_with_limits(workspace.path(), &[&first, &second], limits)
                .expect_err("second Layer entry must exceed the shared entry budget");
            assert!(error.to_string().contains("Layer entries limit exceeded"));

            let workspace = tempfile::tempdir().expect("workspace");
            let limits = RootfsLimits {
                total_path_bytes: usize_to_u64(b"first".len()),
                ..RootfsLimits::default()
            };
            let error = materialize_with_limits(workspace.path(), &[&first, &second], limits)
                .expect_err("second Layer path must exceed the shared raw path budget");
            assert!(
                error
                    .to_string()
                    .contains("Layer raw path bytes limit exceeded")
            );

            let xattr_metadata = |name: &[u8]| Metadata {
                mode: 0o644,
                uid: 0,
                gid: 0,
                mtime: Timestamp {
                    seconds: 0,
                    nanos: 0,
                },
                xattrs: BTreeMap::from([(
                    name.to_vec().into_boxed_slice(),
                    b"v".to_vec().into_boxed_slice(),
                )]),
            };
            let first_xattr = tar_layer(|builder| {
                append_pax_metadata(builder, &xattr_metadata(b"user.one"))?;
                append_test_file(builder, b"x-one", b"")
            });
            let second_xattr = tar_layer(|builder| {
                append_pax_metadata(builder, &xattr_metadata(b"user.two"))?;
                append_test_file(builder, b"x-two", b"")
            });
            // SCHILY carries one raw byte and LIBARCHIVE carries four base64
            // bytes, so the first Layer consumes 2*8 name bytes plus 5 values.
            let workspace = tempfile::tempdir().expect("workspace");
            let limits = RootfsLimits {
                total_xattr_bytes: 21,
                ..RootfsLimits::default()
            };
            let error =
                materialize_with_limits(workspace.path(), &[&first_xattr, &second_xattr], limits)
                    .expect_err("second Layer xattr must exceed the shared xattr budget");
            assert!(
                error
                    .to_string()
                    .contains("Layer xattr bytes limit exceeded")
            );
        }

        #[test]
        fn raw_dot_components_cannot_bypass_path_budget() {
            if !geteuid().is_root() {
                return;
            }
            let mut layer = tar_layer(|builder| append_test_file(builder, b"value", b"x"));
            let raw = format!("{}value", "./".repeat(30));
            replace_first_tar_path(&mut layer.bytes, raw.as_bytes());
            refresh_test_layer(&mut layer);
            let limits = RootfsLimits {
                total_path_bytes: usize_to_u64(b"value".len()),
                ..RootfsLimits::default()
            };
            let workspace = tempfile::tempdir().expect("workspace");
            let error = materialize_with_limits(workspace.path(), &[&layer], limits)
                .expect_err("raw path bytes must be counted before normalization");
            assert!(
                error
                    .to_string()
                    .contains("Layer raw path bytes limit exceeded")
            );
        }

        #[test]
        fn oversized_pax_and_gnu_longname_are_rejected_by_preflight() {
            if !geteuid().is_root() {
                return;
            }
            for entry_type in [EntryType::XHeader, EntryType::GNULongName] {
                let layer = tar_layer(|builder| {
                    let payload = vec![b'x'; 33];
                    let mut header =
                        tar_header(usize_to_u64(payload.len()), 0o644, 0, 0, 0, entry_type)?;
                    header.set_path("extension")?;
                    header.set_cksum();
                    builder.append(&header, payload.as_slice())?;
                    append_test_file(builder, b"value", b"")
                });
                let workspace = tempfile::tempdir().expect("workspace");
                let limits = RootfsLimits {
                    extension_bytes: 32,
                    ..RootfsLimits::default()
                };
                let error = materialize_with_limits(workspace.path(), &[&layer], limits)
                    .expect_err("advanced tar parser must not receive oversized extension");
                assert!(
                    error
                        .to_string()
                        .contains("tar extension bytes limit exceeded")
                );
            }
        }

        #[test]
        fn pax_size_polyglot_is_rejected_before_hidden_extension() {
            if !geteuid().is_root() {
                return;
            }
            let pax = pax_record(b"size", b"0");
            let mut bytes = Vec::new();
            bytes.extend_from_slice(&raw_tar_header(
                usize_to_u64(pax.len()),
                EntryType::XHeader,
                b"PaxHeaders/size",
            ));
            bytes.extend_from_slice(&pax);
            bytes.resize(bytes.len().next_multiple_of(512), 0);

            // The raw carrier claims that the following GNU longname header is
            // data. tar::Archive would instead apply PAX size=0, expose that
            // hidden header, and allocate its following 512-byte payload.
            bytes.extend_from_slice(&raw_tar_header(512, EntryType::Regular, b"carrier"));
            bytes.extend_from_slice(&raw_tar_header(512, EntryType::GNULongName, b"hidden"));
            bytes.extend_from_slice(&raw_tar_header(0, EntryType::Regular, b"decoy"));
            bytes.extend_from_slice(&raw_tar_header(0, EntryType::Regular, b"visible"));
            bytes.extend_from_slice(&[0_u8; 1024]);
            let layer = test_layer_from_bytes(bytes);
            let workspace = tempfile::tempdir().expect("workspace");
            let error = materialize_with_limits(
                workspace.path(),
                &[&layer],
                RootfsLimits {
                    extension_bytes: usize_to_u64(pax.len()),
                    ..RootfsLimits::default()
                },
            )
            .expect_err("PAX size must fail before the hidden GNU extension is parsed");
            assert!(
                error
                    .to_string()
                    .contains("PAX size overrides are unsupported")
            );
        }

        #[test]
        fn gnu_sparse_headers_and_pax_keys_are_rejected_by_preflight() {
            if !geteuid().is_root() {
                return;
            }
            let sparse_type = tar_layer(|builder| {
                let mut header = tar_header(0, 0o644, 0, 0, 0, EntryType::GNUSparse)?;
                header.set_path("sparse")?;
                header.set_cksum();
                builder.append(&header, std::io::empty())?;
                Ok(())
            });
            let workspace = tempfile::tempdir().expect("workspace");
            let error = materialize(workspace.path(), &[&sparse_type])
                .expect_err("GNU sparse type must fail in preflight");
            assert!(
                error
                    .to_string()
                    .contains("GNU sparse OCI Layer entries are unsupported")
            );

            let pax = pax_record(b"GNU.sparse.map", b"0,1");
            let sparse_pax = tar_layer(|builder| {
                let mut header =
                    tar_header(usize_to_u64(pax.len()), 0o644, 0, 0, 0, EntryType::XHeader)?;
                header.set_path("PaxHeaders/sparse")?;
                header.set_cksum();
                builder.append(&header, pax.as_slice())?;
                append_test_file(builder, b"value", b"")
            });
            let workspace = tempfile::tempdir().expect("workspace");
            let error = materialize(workspace.path(), &[&sparse_pax])
                .expect_err("GNU sparse PAX key must fail in preflight");
            assert!(
                error
                    .to_string()
                    .contains("GNU sparse OCI Layer PAX metadata is unsupported")
            );
        }

        #[test]
        fn nonzero_tail_after_tar_end_marker_is_rejected() {
            if !geteuid().is_root() {
                return;
            }
            let mut layer = tar_layer(|builder| append_test_file(builder, b"value", b""));
            layer.bytes.extend_from_slice(&[0_u8; 512]);
            layer.bytes.push(1);
            refresh_test_layer(&mut layer);
            let workspace = tempfile::tempdir().expect("workspace");
            let error = materialize(workspace.path(), &[&layer])
                .expect_err("non-zero unvisited tar tail must fail");
            assert!(
                error
                    .to_string()
                    .contains("non-zero data after its end marker")
            );
        }

        fn materialize(workspace: &Path, layers: &[&TestLayer]) -> Result<Rootfs> {
            materialize_with_limits(workspace, layers, RootfsLimits::default())
        }

        fn materialize_with_limits(
            workspace: &Path,
            layers: &[&TestLayer],
            limits: RootfsLimits,
        ) -> Result<Rootfs> {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(workspace, std::fs::Permissions::from_mode(0o700))?;
            let verified = layers
                .iter()
                .map(|layer| VerifiedLayer {
                    descriptor: &layer.descriptor,
                    expected_diff_id: &layer.diff_id,
                })
                .collect::<Vec<_>>();
            Rootfs::materialize_in(workspace, &verified, limits, |descriptor| {
                let layer = layers
                    .iter()
                    .find(|layer| &layer.descriptor == descriptor)
                    .context("test Layer is absent")?;
                Ok(Cursor::new(layer.bytes.clone()))
            })
        }

        fn scan_test_layer(layer: &TestLayer, limits: RootfsLimits) -> Result<LayerPlan> {
            use std::io::{Seek as _, Write as _};
            let workspace = tempfile::tempdir()?;
            let mut file = tempfile::NamedTempFile::new_in(workspace.path())?;
            file.write_all(&layer.bytes)?;
            file.rewind()?;
            let mut budget = MaterializationBudget::new(limits);
            scan_layer(file.as_file_mut(), workspace.path(), limits, &mut budget)
        }

        type CapturedEntry = (Vec<u8>, EntryType, Option<Vec<u8>>, Vec<u8>);

        fn captured_entries(layer: &CapturedLayer) -> Vec<CapturedEntry> {
            let mut archive = Archive::new(layer.open().expect("open captured Layer"));
            archive
                .entries()
                .expect("captured entries")
                .map(|entry| {
                    let mut entry = entry.expect("captured entry");
                    let mut content = Vec::new();
                    entry.read_to_end(&mut content).expect("captured content");
                    (
                        entry.path_bytes().into_owned(),
                        entry.header().entry_type(),
                        entry.link_name_bytes().map(std::borrow::Cow::into_owned),
                        content,
                    )
                })
                .collect()
        }

        fn tar_layer(mut write: impl FnMut(&mut Builder<Vec<u8>>) -> Result<()>) -> TestLayer {
            let mut builder = Builder::new(Vec::new());
            builder.mode(HeaderMode::Deterministic);
            write(&mut builder).expect("Layer entries");
            builder.finish().expect("finish Layer");
            let bytes = builder.into_inner().expect("Layer bytes");
            test_layer_from_bytes(bytes)
        }

        fn test_layer_from_bytes(bytes: Vec<u8>) -> TestLayer {
            let diff_id = sha256_digest(&bytes);
            let descriptor = Descriptor::new(
                MediaType::ImageLayer,
                usize_to_u64(bytes.len()),
                diff_id.clone(),
            );
            TestLayer {
                descriptor,
                bytes,
                diff_id,
            }
        }

        fn pax_record(key: &[u8], value: &[u8]) -> Vec<u8> {
            let remainder = key.len() + value.len() + 3;
            let mut digits = 1;
            loop {
                let length = remainder + digits;
                if length.to_string().len() == digits {
                    let mut record = length.to_string().into_bytes();
                    record.push(b' ');
                    record.extend_from_slice(key);
                    record.push(b'=');
                    record.extend_from_slice(value);
                    record.push(b'\n');
                    return record;
                }
                digits = length.to_string().len();
            }
        }

        fn raw_tar_header(size: u64, entry_type: EntryType, path: &[u8]) -> [u8; 512] {
            let mut header = tar_header(size, 0o644, 0, 0, 0, entry_type).expect("raw header");
            header
                .set_path(Path::new(OsStr::from_bytes(path)))
                .expect("raw header path");
            header.set_cksum();
            *header.as_bytes()
        }

        fn refresh_test_layer(layer: &mut TestLayer) {
            layer.diff_id = sha256_digest(&layer.bytes);
            layer.descriptor = Descriptor::new(
                MediaType::ImageLayer,
                usize_to_u64(layer.bytes.len()),
                layer.diff_id.clone(),
            );
        }

        fn replace_first_tar_path(bytes: &mut [u8], path: &[u8]) {
            assert!(path.len() <= 100);
            bytes[..100].fill(0);
            bytes[..path.len()].copy_from_slice(path);
            bytes[148..156].fill(b' ');
            let checksum = bytes[..512]
                .iter()
                .map(|byte| u64::from(*byte))
                .sum::<u64>();
            let encoded = format!("{checksum:06o}\0 ");
            bytes[148..156].copy_from_slice(encoded.as_bytes());
        }

        fn append_test_file(
            builder: &mut Builder<Vec<u8>>,
            path: &[u8],
            bytes: &[u8],
        ) -> Result<()> {
            let mut header = tar_header(
                usize_to_u64(bytes.len()),
                0o644,
                0,
                0,
                0,
                EntryType::Regular,
            )?;
            builder.append_data(&mut header, Path::new(OsStr::from_bytes(path)), bytes)?;
            Ok(())
        }

        fn append_test_directory(builder: &mut Builder<Vec<u8>>, path: &[u8]) -> Result<()> {
            let mut header = tar_header(0, 0o755, 0, 0, 0, EntryType::Directory)?;
            builder.append_data(
                &mut header,
                Path::new(OsStr::from_bytes(path)),
                std::io::empty(),
            )?;
            Ok(())
        }

        fn append_test_hardlink(
            builder: &mut Builder<Vec<u8>>,
            path: &[u8],
            target: &[u8],
        ) -> Result<()> {
            let mut header = tar_header(0, 0o644, 0, 0, 0, EntryType::Link)?;
            builder.append_link(
                &mut header,
                Path::new(OsStr::from_bytes(path)),
                Path::new(OsStr::from_bytes(target)),
            )?;
            Ok(())
        }

        fn append_test_symlink(
            builder: &mut Builder<Vec<u8>>,
            path: &[u8],
            target: &[u8],
        ) -> Result<()> {
            let mut header = tar_header(0, 0o777, 0, 0, 0, EntryType::Symlink)?;
            builder.append_link(
                &mut header,
                Path::new(OsStr::from_bytes(path)),
                Path::new(OsStr::from_bytes(target)),
            )?;
            Ok(())
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod platform {
    use anyhow::{Result, bail};
    use oci_spec::image::Descriptor;

    use super::{CapturedLayer, RootfsLimits, VerifiedLayer};
    use std::path::{Path, PathBuf};

    #[derive(Debug)]
    pub(crate) struct Rootfs {
        workspace: PathBuf,
        rootfs: PathBuf,
    }

    impl Rootfs {
        pub(crate) fn materialize_in<F, R>(
            workspace: &Path,
            _layers: &[VerifiedLayer<'_>],
            _limits: RootfsLimits,
            _open_layer: F,
        ) -> Result<Self>
        where
            F: FnMut(&Descriptor) -> Result<R>,
            R: std::io::Read,
        {
            let _ = workspace;
            bail!("private OCI rootfs materialization is supported only on Linux")
        }

        pub(crate) fn path(&self) -> &Path {
            &self.rootfs
        }

        pub(crate) fn workspace(&self) -> &Path {
            &self.workspace
        }

        pub(crate) fn capture(&self) -> Result<CapturedLayer> {
            let _ = &self.rootfs;
            bail!("stopped rootfs capture is supported only on Linux")
        }

        pub(crate) fn ensure_no_mounts(&self) -> Result<()> {
            let _ = &self.rootfs;
            bail!("rootfs mount inspection is supported only on Linux")
        }
    }
}

#[allow(
    unused_imports,
    reason = "the Gate 4 rootfs pipeline is compiled before its engine call site"
)]
pub(crate) use platform::Rootfs;
