use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr};
use std::os::unix::ffi::OsStringExt as _;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use rustix::fs::{AtFlags, CWD, Mode, OFlags, StatxFlags, open, statx};
use rustix::mount::{MountFlags, UnmountFlags, mount_bind, mount_remount, unmount};
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

use crate::core::{Digest, RunResolverFacts, RunResolverSource};
use crate::integrity::{digest_bytes, sync_directory};

const ETC_RESOLV_CONF: &str = "/etc/resolv.conf";
const SYSTEMD_RESOLVED_UPLINK: &str = "/run/systemd/resolve/resolv.conf";
const RESOLVER_DIRECTORY: &str = "resolver";
const RESOLVER_FILE: &str = "resolv.conf";
const MAX_RESOLVER_BYTES: u64 = 64 * 1024;
const MAX_NAMESERVERS: usize = 3;

#[derive(Debug, Clone)]
pub(crate) struct ResolverConfig {
    facts: RunResolverFacts,
    bytes: Vec<u8>,
}

impl ResolverConfig {
    pub(crate) fn preflight() -> Result<Self> {
        Self::read_from_paths(
            Path::new(ETC_RESOLV_CONF),
            Path::new(SYSTEMD_RESOLVED_UPLINK),
        )
    }

    pub(crate) fn facts(&self) -> RunResolverFacts {
        self.facts.clone()
    }

    pub(crate) fn write_attempt_source(&self, attempt: &Path) -> Result<ResolverSourceFile> {
        ensure_root_private_directory(attempt)?;
        let directory = attempt.join(RESOLVER_DIRECTORY);
        fs::create_dir(&directory).with_context(|| {
            format!(
                "failed to create native resolver directory {}",
                directory.display()
            )
        })?;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).with_context(|| {
            format!(
                "failed to secure native resolver directory {}",
                directory.display()
            )
        })?;
        sync_directory(attempt)?;

        let path = ResolverSourceFile::path_in_attempt(attempt);
        let mut temporary = NamedTempFile::new_in(&directory).with_context(|| {
            format!(
                "failed to create native resolver staging file in {}",
                directory.display()
            )
        })?;
        temporary
            .write_all(&self.bytes)
            .context("failed to write canonical native resolver bytes")?;
        temporary
            .as_file_mut()
            .sync_all()
            .context("failed to fsync canonical native resolver bytes")?;
        temporary
            .persist_noclobber(&path)
            .map_err(|error| error.error)
            .context("failed to publish canonical native resolver file")?;

        fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).with_context(|| {
            format!(
                "failed to make native resolver source read-only {}",
                path.display()
            )
        })?;
        File::open(&path)
            .with_context(|| format!("failed to reopen native resolver source {}", path.display()))?
            .sync_all()
            .with_context(|| {
                format!("failed to fsync native resolver source {}", path.display())
            })?;
        sync_directory(&directory)?;

        ResolverSourceFile::open(&path, &self.facts)
    }

    fn read_from_paths(primary: &Path, fallback: &Path) -> Result<Self> {
        let primary_bytes = read_verified_resolver_file(primary, "host resolver configuration")?;
        let primary_candidates = parse_nameservers(&primary_bytes, primary)?;
        if !primary_candidates.usable.is_empty() {
            return Self::from_addresses(
                RunResolverSource::EtcResolvConf,
                primary_candidates.usable,
            );
        }
        if !primary_candidates.saw_loopback_stub {
            bail!(
                "host resolver configuration contains no usable IPv4 nameserver and no loopback stub: {}",
                primary.display()
            );
        }

        let fallback_bytes =
            read_verified_resolver_file(fallback, "systemd-resolved uplink configuration")?;
        let fallback_candidates = parse_nameservers(&fallback_bytes, fallback)?;
        if fallback_candidates.usable.is_empty() {
            bail!(
                "systemd-resolved uplink configuration contains no usable IPv4 nameserver: {}",
                fallback.display()
            );
        }
        Self::from_addresses(
            RunResolverSource::SystemdResolvedUplink,
            fallback_candidates.usable,
        )
    }

    fn from_addresses(source: RunResolverSource, addresses: Vec<Ipv4Addr>) -> Result<Self> {
        let nameservers = addresses
            .into_iter()
            .map(|address| address.to_string())
            .collect::<Vec<_>>();
        let mut bytes = Vec::new();
        for nameserver in &nameservers {
            bytes.extend_from_slice(b"nameserver ");
            bytes.extend_from_slice(nameserver.as_bytes());
            bytes.push(b'\n');
        }
        let facts = RunResolverFacts {
            source,
            nameservers,
            content_digest: digest_bytes(&bytes),
            content_size: bytes.len() as u64,
        };
        if facts.canonical_bytes()? != bytes {
            bail!("native resolver facts do not reconstruct their canonical bytes");
        }
        Ok(Self { facts, bytes })
    }

    #[cfg(test)]
    fn from_paths(primary: &Path, fallback: &Path) -> Result<Self> {
        Self::read_from_paths(primary, fallback)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ResolverSourceFile {
    path: PathBuf,
    checkpoint: ResolverSourceCheckpoint,
    pinned: Arc<File>,
}

impl ResolverSourceFile {
    fn open(path: &Path, facts: &RunResolverFacts) -> Result<Self> {
        let (file, identity) = open_resolver_source(path)?;
        let bytes = read_opened_bounded(&file, MAX_RESOLVER_BYTES, &identity)
            .with_context(|| format!("failed to read native resolver source {}", path.display()))?;
        if bytes.len() as u64 != facts.content_size
            || digest_bytes(&bytes) != facts.content_digest
            || bytes != facts.canonical_bytes()?
        {
            bail!(
                "native resolver source differs from accepted canonical resolver bytes: {}",
                path.display()
            );
        }
        let checkpoint = ResolverSourceCheckpoint {
            identity,
            content_digest: facts.content_digest.clone(),
            content_size: facts.content_size,
        };
        Ok(Self {
            path: path.to_path_buf(),
            checkpoint,
            pinned: Arc::new(file),
        })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn open_from_attempt(
        attempt: &Path,
        facts: &RunResolverFacts,
        expected: &ResolverSourceCheckpoint,
    ) -> Result<Self> {
        ensure_root_private_directory(attempt)?;
        expected.validate_against_facts(facts)?;
        let source = Self::open(&Self::path_in_attempt(attempt), facts)?;
        if &source.checkpoint != expected {
            bail!("native resolver source differs from its recovery checkpoint");
        }
        Ok(source)
    }

    pub(crate) fn path_in_attempt(attempt: &Path) -> PathBuf {
        attempt.join(RESOLVER_DIRECTORY).join(RESOLVER_FILE)
    }

    pub(crate) fn checkpoint(&self) -> &ResolverSourceCheckpoint {
        &self.checkpoint
    }

    fn verify(&self) -> Result<()> {
        verify_source_checkpoint(&self.path, &self.checkpoint)?;
        let pinned = ResolverFileIdentity::from_metadata(
            &self
                .pinned
                .metadata()
                .context("failed to inspect pinned native resolver source")?,
        );
        if pinned != self.checkpoint.identity {
            bail!("pinned native resolver source identity changed");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResolverSourceCheckpoint {
    identity: ResolverFileIdentity,
    content_digest: Digest,
    content_size: u64,
}

impl ResolverSourceCheckpoint {
    pub(crate) fn validate_against_facts(&self, facts: &RunResolverFacts) -> Result<()> {
        self.identity.validate_source()?;
        let canonical = facts.canonical_bytes()?;
        if digest_bytes(&canonical) != facts.content_digest {
            bail!("Run resolver content digest differs from its canonical bytes");
        }
        if self.content_digest != facts.content_digest || self.content_size != facts.content_size {
            bail!("native resolver source checkpoint differs from Run resolver facts");
        }
        if self.identity.size != self.content_size {
            bail!("native resolver source identity size differs from its content checkpoint");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResolverProjectionPending {
    source: ResolverSourceCheckpoint,
    target: ResolverFileIdentity,
    overlay_mount_id: u64,
}

impl ResolverProjectionPending {
    pub(crate) const fn overlay_mount_id(&self) -> u64 {
        self.overlay_mount_id
    }

    pub(crate) fn validate_against_source(&self, source: &ResolverSourceCheckpoint) -> Result<()> {
        self.validate()?;
        if &self.source != source {
            bail!("native resolver projection source differs from its attempt checkpoint");
        }
        Ok(())
    }

    fn validate(&self) -> Result<()> {
        self.source.identity.validate_source()?;
        self.target.validate_regular("native resolver target")?;
        if self.overlay_mount_id == 0 {
            bail!("native resolver overlay mount identity is invalid");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResolverProjectionMounted {
    projection_mount_id: u64,
}

impl ResolverProjectionMounted {
    pub(crate) const fn projection_mount_id(self) -> u64 {
        self.projection_mount_id
    }
}

#[derive(Debug)]
pub(crate) struct ResolverProjectionPlan {
    source: ResolverSourceFile,
    target: PathBuf,
    target_pin: File,
    pending: ResolverProjectionPending,
}

impl ResolverProjectionPlan {
    pub(crate) fn prepare(source: ResolverSourceFile, rootfs: &Path) -> Result<Self> {
        source.verify()?;
        let rootfs = canonical_real_path(rootfs, "native resolver rootfs")?;
        let target = rootfs.join("etc/resolv.conf");
        verify_no_symlink_components(&target)?;
        if !mounts_exactly_at(&target)?.is_empty() {
            bail!(
                "native resolver target is already a mountpoint: {}",
                target.display()
            );
        }
        let (target_pin, target_identity) = open_regular_path(&target).with_context(|| {
            format!(
                "native resolver target must be an existing regular file: {}",
                target.display()
            )
        })?;
        let overlay_mount_id = mount_id(&target)?;
        let pending = ResolverProjectionPending {
            source: source.checkpoint.clone(),
            target: target_identity,
            overlay_mount_id,
        };
        Ok(Self {
            source,
            target,
            target_pin,
            pending,
        })
    }

    pub(crate) fn pending(&self) -> &ResolverProjectionPending {
        &self.pending
    }

    pub(crate) fn install(self) -> Result<ResolverProjection> {
        self.verify_unmounted_state()?;
        mount_bind(self.source.path(), &self.target).with_context(|| {
            format!(
                "failed to bind native resolver source over {}",
                self.target.display()
            )
        })?;

        let install_result = (|| {
            mount_remount(
                &self.target,
                MountFlags::BIND
                    | MountFlags::RDONLY
                    | MountFlags::NOSUID
                    | MountFlags::NODEV
                    | MountFlags::NOEXEC,
                "",
            )
            .with_context(|| {
                format!(
                    "failed to make native resolver projection read-only {}",
                    self.target.display()
                )
            })?;
            let projection_mount_id = mount_id(&self.target)?;
            if projection_mount_id == self.pending.overlay_mount_id {
                bail!("native resolver bind mount did not create a distinct mount identity");
            }
            verify_projection_mount(
                self.source.path(),
                &self.target,
                &self.pending,
                projection_mount_id,
                true,
            )?;
            Ok(ResolverProjectionMounted {
                projection_mount_id,
            })
        })();

        match install_result {
            Ok(mounted) => Ok(ResolverProjection {
                plan: self,
                mounted,
                active: true,
            }),
            Err(error) => {
                match recover_cleanup(self.source.path(), &self.target, &self.pending, None) {
                    Ok(_) => Err(error),
                    Err(cleanup) => Err(anyhow::anyhow!(
                        "{error:#}; native resolver cleanup also failed: {cleanup:#}"
                    )),
                }
            }
        }
    }

    fn verify_unmounted_state(&self) -> Result<()> {
        self.source.verify()?;
        verify_no_symlink_components(&self.target)?;
        if mount_id(&self.target)? != self.pending.overlay_mount_id
            || !mounts_exactly_at(&self.target)?.is_empty()
        {
            bail!("native resolver target mount identity changed before installation");
        }
        verify_identity_at(&self.target, &self.pending.target, "native resolver target")?;
        let pinned = ResolverFileIdentity::from_metadata(
            &self
                .target_pin
                .metadata()
                .context("failed to inspect pinned native resolver target")?,
        );
        if pinned != self.pending.target {
            bail!("pinned native resolver target identity changed before installation");
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct ResolverProjection {
    plan: ResolverProjectionPlan,
    mounted: ResolverProjectionMounted,
    active: bool,
}

impl ResolverProjection {
    pub(crate) const fn mounted(&self) -> ResolverProjectionMounted {
        self.mounted
    }

    pub(crate) fn unmount(&mut self) -> Result<()> {
        if !self.active {
            return Ok(());
        }
        let outcome = recover_cleanup(
            self.plan.source.path(),
            &self.plan.target,
            &self.plan.pending,
            Some(self.mounted),
        )?;
        if outcome != ResolverRecoveryOutcome::Unmounted {
            bail!("active native resolver projection was already absent");
        }
        self.plan.verify_unmounted_state()?;
        self.active = false;
        Ok(())
    }

    pub(crate) fn preserve(mut self) {
        self.active = false;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResolverRecoveryOutcome {
    AlreadyRestored,
    Unmounted,
}

pub(crate) fn recover_cleanup(
    source: &Path,
    target: &Path,
    pending: &ResolverProjectionPending,
    mounted: Option<ResolverProjectionMounted>,
) -> Result<ResolverRecoveryOutcome> {
    pending.validate()?;
    if mounted.is_some_and(|mounted| {
        mounted.projection_mount_id == 0 || mounted.projection_mount_id == pending.overlay_mount_id
    }) {
        bail!("native resolver projection mount checkpoint is invalid");
    }
    verify_source_checkpoint(source, &pending.source)?;
    verify_no_symlink_components(target)?;
    let current_mount_id = mount_id(target)?;
    if current_mount_id == pending.overlay_mount_id {
        if mounted.is_some() && !mounts_exactly_at(target)?.is_empty() {
            bail!("native resolver mount checkpoint is absent but target remains a mountpoint");
        }
        verify_identity_at(target, &pending.target, "restored native resolver target")?;
        return Ok(ResolverRecoveryOutcome::AlreadyRestored);
    }

    if let Some(mounted) = mounted
        && current_mount_id != mounted.projection_mount_id
    {
        bail!(
            "native resolver projection mount identity changed: expected {}, observed {current_mount_id}",
            mounted.projection_mount_id
        );
    }
    verify_projection_mount(source, target, pending, current_mount_id, mounted.is_some())?;
    unmount(target, UnmountFlags::empty()).with_context(|| {
        format!(
            "failed to unmount native resolver projection {}",
            target.display()
        )
    })?;
    if mount_id(target)? != pending.overlay_mount_id || !mounts_exactly_at(target)?.is_empty() {
        bail!("native resolver target did not return to its overlay mount identity");
    }
    verify_identity_at(target, &pending.target, "restored native resolver target")?;
    verify_source_checkpoint(source, &pending.source)?;
    Ok(ResolverRecoveryOutcome::Unmounted)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResolverFileIdentity {
    device: u64,
    inode: u64,
    mode: u32,
    uid: u32,
    gid: u32,
    links: u64,
    size: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

impl ResolverFileIdentity {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            mode: metadata.mode(),
            uid: metadata.uid(),
            gid: metadata.gid(),
            links: metadata.nlink(),
            size: metadata.size(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
            changed_seconds: metadata.ctime(),
            changed_nanoseconds: metadata.ctime_nsec(),
        }
    }

    fn validate_source(&self) -> Result<()> {
        self.validate_regular("native resolver source")?;
        if self.uid != 0
            || self.links != 1
            || self.mode & 0o7777 != 0o444
            || self.size > MAX_RESOLVER_BYTES
        {
            bail!("native resolver source checkpoint identity is invalid");
        }
        Ok(())
    }

    fn validate_regular(&self, description: &str) -> Result<()> {
        if self.inode == 0 || self.links == 0 || self.mode & 0o170_000 != 0o100_000 {
            bail!("{description} checkpoint identity is invalid");
        }
        Ok(())
    }
}

#[derive(Debug)]
struct ResolverCandidates {
    usable: Vec<Ipv4Addr>,
    saw_loopback_stub: bool,
}

fn parse_nameservers(bytes: &[u8], path: &Path) -> Result<ResolverCandidates> {
    let contents = std::str::from_utf8(bytes)
        .with_context(|| format!("resolver configuration is not UTF-8: {}", path.display()))?;
    let mut seen = BTreeSet::new();
    let mut usable = Vec::new();
    let mut saw_loopback_stub = false;
    for line in contents.lines() {
        let line = line
            .split(['#', ';'])
            .next()
            .expect("split always returns one field");
        let mut fields = line.split_ascii_whitespace();
        if fields.next() != Some("nameserver") {
            continue;
        }
        let Some(value) = fields.next() else {
            continue;
        };
        let Ok(address) = value.parse::<IpAddr>() else {
            continue;
        };
        let IpAddr::V4(address) = address else {
            continue;
        };
        if address.is_loopback() {
            saw_loopback_stub = true;
            continue;
        }
        if !is_usable_nameserver(address) || !seen.insert(address) {
            continue;
        }
        if usable.len() < MAX_NAMESERVERS {
            usable.push(address);
        }
    }
    Ok(ResolverCandidates {
        usable,
        saw_loopback_stub,
    })
}

fn is_usable_nameserver(address: Ipv4Addr) -> bool {
    let octets = address.octets();
    !(address.is_unspecified()
        || address.is_loopback()
        || address.is_link_local()
        || address.is_multicast()
        || address == Ipv4Addr::BROADCAST
        || (octets[0] == 10 && octets[1] == 240))
}

/// Read a host resolver file and bind the bytes to the inode and size observed
/// on the same descriptor, so a swap between `stat` and `read` is rejected.
fn read_verified_resolver_file(path: &Path, description: &str) -> Result<Vec<u8>> {
    let file = File::open(path)
        .with_context(|| format!("failed to open {description} {}", path.display()))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("failed to inspect {description} {}", path.display()))?;
    if !metadata.is_file() {
        bail!("{description} is not a regular file: {}", path.display());
    }
    let identity = ResolverFileIdentity::from_metadata(&metadata);
    read_opened_bounded(&file, MAX_RESOLVER_BYTES, &identity)
        .with_context(|| format!("failed to read {description} {}", path.display()))
}

fn read_opened_bounded(
    file: &File,
    maximum: u64,
    expected: &ResolverFileIdentity,
) -> Result<Vec<u8>> {
    if expected.size > maximum {
        bail!("file exceeds its {maximum}-byte read limit");
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut file.try_clone()?)
        .take(maximum + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > maximum {
        bail!("file exceeds its {maximum}-byte read limit");
    }
    let observed = ResolverFileIdentity::from_metadata(
        &file
            .metadata()
            .context("failed to re-inspect file after bounded read")?,
    );
    if &observed != expected || bytes.len() as u64 != expected.size {
        bail!("file identity changed during bounded read");
    }
    Ok(bytes)
}

fn ensure_root_private_directory(path: &Path) -> Result<()> {
    verify_no_symlink_components(path)?;
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect native attempt {}", path.display()))?;
    if !metadata.is_dir() || metadata.uid() != 0 || metadata.mode() & 0o7777 != 0o700 {
        bail!(
            "native attempt must be a root-owned mode 0700 directory: {}",
            path.display()
        );
    }
    Ok(())
}

fn open_resolver_source(path: &Path) -> Result<(File, ResolverFileIdentity)> {
    verify_no_symlink_components(path)?;
    let (file, identity) = open_regular_path(path)?;
    if identity.uid != 0
        || identity.links != 1
        || identity.mode & 0o7777 != 0o444
        || identity.size > MAX_RESOLVER_BYTES
    {
        bail!(
            "native resolver source must be a root-owned single-link mode 0444 bounded regular file: {}",
            path.display()
        );
    }
    Ok((file, identity))
}

fn verify_source_checkpoint(path: &Path, expected: &ResolverSourceCheckpoint) -> Result<()> {
    let (file, identity) = open_resolver_source(path)?;
    if identity != expected.identity {
        bail!(
            "native resolver source identity changed: {}",
            path.display()
        );
    }
    let bytes = read_opened_bounded(&file, MAX_RESOLVER_BYTES, &identity)?;
    if bytes.len() as u64 != expected.content_size
        || digest_bytes(&bytes) != expected.content_digest
    {
        bail!("native resolver source content changed: {}", path.display());
    }
    Ok(())
}

fn open_regular_path(path: &Path) -> Result<(File, ResolverFileIdentity)> {
    let file = File::from(
        open(
            path,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .with_context(|| format!("failed to open regular file {}", path.display()))?,
    );
    let metadata = file
        .metadata()
        .with_context(|| format!("failed to inspect opened file {}", path.display()))?;
    if !metadata.is_file() {
        bail!("path is not a regular file: {}", path.display());
    }
    let path_metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect regular file {}", path.display()))?;
    let identity = ResolverFileIdentity::from_metadata(&metadata);
    if !path_metadata.is_file() || ResolverFileIdentity::from_metadata(&path_metadata) != identity {
        bail!(
            "regular file changed while it was opened: {}",
            path.display()
        );
    }
    Ok((file, identity))
}

fn verify_identity_at(
    path: &Path,
    expected: &ResolverFileIdentity,
    description: &str,
) -> Result<()> {
    let (_, observed) = open_regular_path(path)
        .with_context(|| format!("failed to verify {description} {}", path.display()))?;
    if &observed != expected {
        bail!("{description} identity changed: {}", path.display());
    }
    Ok(())
}

fn verify_projection_mount(
    source: &Path,
    target: &Path,
    pending: &ResolverProjectionPending,
    expected_mount_id: u64,
    require_final_flags: bool,
) -> Result<()> {
    verify_source_checkpoint(source, &pending.source)?;
    if mount_id(target)? != expected_mount_id {
        bail!("native resolver projection mount identity changed while it was inspected");
    }
    verify_identity_at(
        target,
        &pending.source.identity,
        "native resolver projection source",
    )?;
    let mounts = mounts_exactly_at(target)?;
    if mounts.len() != 1 || mounts[0].mount_id != expected_mount_id {
        bail!("native resolver target has an unknown or stacked mount");
    }
    if require_final_flags {
        for required in ["ro", "nosuid", "nodev", "noexec"] {
            if !mounts[0].options.iter().any(|option| option == required) {
                bail!("native resolver projection lacks required mount option {required}");
            }
        }
    }
    Ok(())
}

fn mount_id(path: &Path) -> Result<u64> {
    let status = statx(CWD, path, AtFlags::SYMLINK_NOFOLLOW, StatxFlags::MNT_ID)
        .with_context(|| format!("failed to inspect mount identity for {}", path.display()))?;
    if status.stx_mask & StatxFlags::MNT_ID.bits() == 0 || status.stx_mnt_id == 0 {
        bail!("mount identity is unavailable for {}", path.display());
    }
    Ok(status.stx_mnt_id)
}

#[derive(Debug)]
struct MountInfo {
    mount_id: u64,
    mountpoint: PathBuf,
    options: Vec<String>,
}

fn mounts_exactly_at(target: &Path) -> Result<Vec<MountInfo>> {
    let target = canonical_path_without_following_final(target)?;
    read_mountinfo().map(|mounts| {
        mounts
            .into_iter()
            .filter(|mount| mount.mountpoint == target)
            .collect()
    })
}

fn read_mountinfo() -> Result<Vec<MountInfo>> {
    fs::read("/proc/self/mountinfo")
        .context("failed to read /proc/self/mountinfo")?
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(parse_mountinfo)
        .collect()
}

fn parse_mountinfo(line: &[u8]) -> Result<MountInfo> {
    let fields = line
        .split(u8::is_ascii_whitespace)
        .filter(|field| !field.is_empty())
        .collect::<Vec<_>>();
    let separator = fields
        .iter()
        .position(|field| *field == b"-")
        .context("mountinfo entry lacks field separator")?;
    if separator < 6 {
        bail!("mountinfo entry is incomplete");
    }
    Ok(MountInfo {
        mount_id: std::str::from_utf8(fields[0])
            .context("mountinfo mount identity is not ASCII")?
            .parse::<u64>()
            .context("mountinfo mount identity is invalid")?,
        mountpoint: PathBuf::from(unescape_mountinfo(fields[4])?),
        options: std::str::from_utf8(fields[5])
            .context("mountinfo options are not ASCII")?
            .split(',')
            .map(ToOwned::to_owned)
            .collect(),
    })
}

fn unescape_mountinfo(value: &[u8]) -> Result<OsString> {
    let mut output = Vec::with_capacity(value.len());
    let mut index = 0;
    while index < value.len() {
        if value[index] != b'\\' {
            output.push(value[index]);
            index += 1;
            continue;
        }
        let digits = value
            .get(index + 1..index + 4)
            .context("mountinfo path has a truncated escape")?;
        if !digits.iter().all(|digit| (b'0'..=b'7').contains(digit)) {
            bail!("mountinfo path has an invalid escape");
        }
        output.push((digits[0] - b'0') * 64 + (digits[1] - b'0') * 8 + digits[2] - b'0');
        index += 4;
    }
    Ok(OsString::from_vec(output))
}

fn canonical_real_path(path: &Path, description: &str) -> Result<PathBuf> {
    verify_no_symlink_components(path)?;
    let canonical = path
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {description} {}", path.display()))?;
    if canonical != path {
        bail!(
            "{description} must be an absolute canonical path: {}",
            path.display()
        );
    }
    Ok(canonical)
}

fn canonical_path_without_following_final(path: &Path) -> Result<PathBuf> {
    let parent = path
        .parent()
        .context("native resolver target has no parent")?
        .canonicalize()
        .context("failed to canonicalize native resolver target parent")?;
    let name = path
        .file_name()
        .context("native resolver target has no file name")?;
    Ok(parent.join(name))
}

fn verify_no_symlink_components(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        bail!("path must be absolute: {}", path.display());
    }
    let mut current = PathBuf::new();
    for component in path.components() {
        match component {
            Component::RootDir => current.push(component.as_os_str()),
            Component::Normal(part) => {
                current.push(part);
                let metadata = fs::symlink_metadata(&current).with_context(|| {
                    format!("failed to inspect path component {}", current.display())
                })?;
                if metadata.file_type().is_symlink() {
                    bail!(
                        "path must not contain symbolic links: {}",
                        current.display()
                    );
                }
            }
            Component::CurDir | Component::ParentDir | Component::Prefix(_) => {
                bail!("path must be absolute and normalized: {}", path.display());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    fn resolver_files(primary: &[u8], fallback: &[u8]) -> (tempfile::TempDir, PathBuf, PathBuf) {
        let directory = tempfile::tempdir().expect("resolver directory");
        let primary_path = directory.path().join("resolv.conf");
        let fallback_path = directory.path().join("uplink.conf");
        fs::write(&primary_path, primary).expect("primary resolver");
        fs::write(&fallback_path, fallback).expect("fallback resolver");
        (directory, primary_path, fallback_path)
    }

    #[test]
    fn selects_stable_unique_primary_ipv4_nameservers() {
        let (_directory, primary, fallback) = resolver_files(
            b"search ignored.example\nnameserver 192.0.2.53\nnameserver 192.0.2.53\noptions rotate\nnameserver 198.51.100.7\nnameserver 203.0.113.9\nnameserver 8.8.8.8\n",
            b"nameserver 9.9.9.9\n",
        );
        let resolver = ResolverConfig::from_paths(&primary, &fallback).expect("resolver");

        assert_eq!(resolver.facts.source, RunResolverSource::EtcResolvConf);
        assert_eq!(
            resolver.facts.nameservers,
            ["192.0.2.53", "198.51.100.7", "203.0.113.9"]
        );
        assert_eq!(
            resolver.bytes,
            b"nameserver 192.0.2.53\nnameserver 198.51.100.7\nnameserver 203.0.113.9\n"
        );
        assert_eq!(resolver.facts.content_digest, digest_bytes(&resolver.bytes));
        assert_eq!(resolver.facts.content_size, resolver.bytes.len() as u64);
    }

    #[test]
    fn falls_back_only_for_a_loopback_stub_without_usable_primary_nameservers() {
        let (_directory, primary, fallback) = resolver_files(
            b"nameserver 127.0.0.53\nnameserver ::1\n",
            b"nameserver 9.9.9.9\nnameserver 149.112.112.112\n",
        );
        let resolver = ResolverConfig::from_paths(&primary, &fallback).expect("fallback resolver");
        assert_eq!(
            resolver.facts.source,
            RunResolverSource::SystemdResolvedUplink
        );
        assert_eq!(resolver.facts.nameservers, ["9.9.9.9", "149.112.112.112"]);

        fs::write(&primary, b"nameserver 192.0.2.53\nnameserver 127.0.0.53\n")
            .expect("primary resolver");
        fs::remove_file(&fallback).expect("remove fallback");
        let resolver = ResolverConfig::from_paths(&primary, &fallback).expect("primary resolver");
        assert_eq!(resolver.facts.source, RunResolverSource::EtcResolvConf);
        assert_eq!(resolver.facts.nameservers, ["192.0.2.53"]);

        fs::write(&primary, b"options rotate\n").expect("primary without stub");
        assert!(ResolverConfig::from_paths(&primary, &fallback).is_err());
    }

    #[test]
    fn excludes_addresses_unreachable_by_the_egress_profile() {
        let (_directory, primary, fallback) = resolver_files(
            b"nameserver 0.0.0.0\nnameserver 127.0.0.1\nnameserver 169.254.1.1\nnameserver 224.0.0.1\nnameserver 255.255.255.255\nnameserver 10.240.2.3\n",
            b"nameserver 192.0.2.1\n",
        );
        let resolver = ResolverConfig::from_paths(&primary, &fallback).expect("stub fallback");
        assert_eq!(resolver.facts.nameservers, ["192.0.2.1"]);

        fs::write(
            &primary,
            b"nameserver 0.0.0.0\nnameserver 169.254.1.1\nnameserver 224.0.0.1\nnameserver 255.255.255.255\nnameserver 10.240.2.3\n",
        )
        .expect("unusable primary");
        assert!(ResolverConfig::from_paths(&primary, &fallback).is_err());
    }

    #[test]
    fn rejects_oversized_and_non_utf8_resolver_configuration() {
        let (_directory, primary, fallback) = resolver_files(b"nameserver 192.0.2.1\n", b"");
        let oversized = usize::try_from(MAX_RESOLVER_BYTES).expect("resolver limit fits usize") + 1;
        fs::write(&primary, vec![b'x'; oversized]).expect("oversized resolver");
        assert!(ResolverConfig::from_paths(&primary, &fallback).is_err());

        fs::write(&primary, b"nameserver \xff\n").expect("non-UTF8 resolver");
        assert!(ResolverConfig::from_paths(&primary, &fallback).is_err());
    }

    #[test]
    fn rejects_a_symlink_component_in_the_projection_target() {
        let directory = tempfile::tempdir().expect("directory");
        let real = directory.path().join("real");
        fs::create_dir(&real).expect("real directory");
        fs::write(real.join("resolv.conf"), b"initial\n").expect("target");
        let link = directory.path().join("etc");
        symlink(&real, &link).expect("symlink");
        assert!(verify_no_symlink_components(&link.join("resolv.conf")).is_err());
    }

    #[test]
    fn parses_mountinfo_identity_options_and_escapes() {
        let mount = parse_mountinfo(
            b"91 80 0:42 /source /tmp/runlab\\040root/etc/resolv.conf ro,nosuid,nodev,noexec - ext4 /dev/vda rw",
        )
        .expect("mountinfo");
        assert_eq!(mount.mount_id, 91);
        assert_eq!(
            mount.mountpoint,
            Path::new("/tmp/runlab root/etc/resolv.conf")
        );
        assert_eq!(mount.options, ["ro", "nosuid", "nodev", "noexec"]);
    }

    #[test]
    fn preserves_unrelated_non_utf8_mountpoints() {
        use std::os::unix::ffi::OsStrExt as _;

        let mount = parse_mountinfo(
            b"91 80 0:42 /source /tmp/unrelated-\xff ro,nosuid,nodev,noexec - ext4 /dev/vda rw",
        )
        .expect("mountinfo");
        assert_eq!(
            mount.mountpoint.as_os_str().as_bytes(),
            b"/tmp/unrelated-\xff"
        );
    }

    #[test]
    #[ignore = "requires root ownership and CAP_SYS_ADMIN"]
    fn writes_and_projects_a_recoverable_read_only_resolver() {
        assert_eq!(rustix::process::geteuid().as_raw(), 0);
        let attempt = tempfile::tempdir().expect("attempt");
        fs::set_permissions(attempt.path(), fs::Permissions::from_mode(0o700))
            .expect("attempt permissions");
        let (_directory, primary, fallback) =
            resolver_files(b"nameserver 192.0.2.53\n", b"nameserver 198.51.100.7\n");
        let resolver = ResolverConfig::from_paths(&primary, &fallback).expect("resolver");
        let source = resolver
            .write_attempt_source(attempt.path())
            .expect("source");
        let source_metadata = fs::symlink_metadata(source.path()).expect("source metadata");
        assert_eq!(source_metadata.uid(), 0);
        assert_eq!(source_metadata.nlink(), 1);
        assert_eq!(source_metadata.mode() & 0o7777, 0o444);

        let rootfs = attempt.path().join("rootfs");
        fs::create_dir_all(rootfs.join("etc")).expect("rootfs");
        fs::write(rootfs.join("etc/resolv.conf"), b"initial\n").expect("target");
        let plan = ResolverProjectionPlan::prepare(source, &rootfs).expect("plan");
        let pending = plan.pending().clone();
        let mut projection = plan.install().expect("projection");
        assert_ne!(
            pending.overlay_mount_id(),
            projection.mounted().projection_mount_id()
        );
        assert_eq!(
            fs::read(rootfs.join("etc/resolv.conf")).expect("projected bytes"),
            resolver.bytes
        );
        projection.unmount().expect("unmount");
        assert_eq!(
            fs::read(rootfs.join("etc/resolv.conf")).expect("restored bytes"),
            b"initial\n"
        );
    }

    #[test]
    #[ignore = "requires root ownership and CAP_SYS_ADMIN"]
    fn pending_recovery_unmounts_a_bind_without_a_mounted_checkpoint() {
        assert_eq!(rustix::process::geteuid().as_raw(), 0);
        let attempt = tempfile::tempdir().expect("attempt");
        fs::set_permissions(attempt.path(), fs::Permissions::from_mode(0o700))
            .expect("attempt permissions");
        let (_directory, primary, fallback) = resolver_files(b"nameserver 192.0.2.53\n", b"");
        let resolver = ResolverConfig::from_paths(&primary, &fallback).expect("resolver");
        let source = resolver
            .write_attempt_source(attempt.path())
            .expect("source");
        let source_path = source.path().to_path_buf();
        let rootfs = attempt.path().join("rootfs");
        fs::create_dir_all(rootfs.join("etc")).expect("rootfs");
        let target = rootfs.join("etc/resolv.conf");
        fs::write(&target, b"initial\n").expect("target");
        let plan = ResolverProjectionPlan::prepare(source, &rootfs).expect("plan");
        let pending = plan.pending().clone();

        mount_bind(&source_path, &target).expect("uncheckpointed bind");
        assert_eq!(
            recover_cleanup(&source_path, &target, &pending, None).expect("recovery"),
            ResolverRecoveryOutcome::Unmounted
        );
        assert_eq!(fs::read(&target).expect("restored target"), b"initial\n");
    }

    #[test]
    #[ignore = "requires root ownership and CAP_SYS_ADMIN"]
    fn recovery_leaves_a_foreign_mount_in_place() {
        assert_eq!(rustix::process::geteuid().as_raw(), 0);
        let attempt = tempfile::tempdir().expect("attempt");
        fs::set_permissions(attempt.path(), fs::Permissions::from_mode(0o700))
            .expect("attempt permissions");
        let (_directory, primary, fallback) = resolver_files(b"nameserver 192.0.2.53\n", b"");
        let resolver = ResolverConfig::from_paths(&primary, &fallback).expect("resolver");
        let source = resolver
            .write_attempt_source(attempt.path())
            .expect("source");
        let source_path = source.path().to_path_buf();
        let rootfs = attempt.path().join("rootfs");
        fs::create_dir_all(rootfs.join("etc")).expect("rootfs");
        let target = rootfs.join("etc/resolv.conf");
        fs::write(&target, b"initial\n").expect("target");
        let plan = ResolverProjectionPlan::prepare(source, &rootfs).expect("plan");
        let pending = plan.pending().clone();
        let foreign = attempt.path().join("foreign");
        fs::write(&foreign, b"foreign\n").expect("foreign source");

        mount_bind(&foreign, &target).expect("foreign bind");
        let foreign_mount_id = mount_id(&target).expect("foreign mount identity");
        assert!(recover_cleanup(&source_path, &target, &pending, None).is_err());
        assert_eq!(
            mount_id(&target).expect("retained foreign mount"),
            foreign_mount_id
        );
        unmount(&target, UnmountFlags::empty()).expect("test cleanup");
    }
}
