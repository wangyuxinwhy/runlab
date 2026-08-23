//! The Run Protocol vocabulary: identities, content slots, process and backend
//! facts, and the two Run Record shapes.
//!
//! This is the crate's leaf. Everything depends on it and it depends on nothing,
//! which is what lets a record state its own invariants — `validate` on these
//! types is the single definition of what a coherent Run Record is, checked by
//! producers and by the persistence layer alike.
//!
//! Rules here must hold for any conforming implementation. A rule that is true
//! only of one backend's realization belongs with that backend.

use std::fmt;
use std::fmt::Write as _;
use std::str::FromStr;

use anyhow::{Context as _, Result, bail};
use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest as _, Sha256};
use uuid::Uuid;

pub const OCI_IMAGE_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
pub const OCI_IMAGE_CONFIG: &str = "application/vnd.oci.image.config.v1+json";
pub const OCI_IMAGE_INDEX: &str = "application/vnd.oci.image.index.v1+json";
pub const OCI_LAYER_TAR: &str = "application/vnd.oci.image.layer.v1.tar";
pub const OCI_LAYER_GZIP: &str = "application/vnd.oci.image.layer.v1.tar+gzip";
pub const OCI_LAYER_ZSTD: &str = "application/vnd.oci.image.layer.v1.tar+zstd";
pub(crate) const MAX_CAPTURED_STREAM_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, JsonSchema)]
#[schemars(with = "String")]
pub struct RunId(Uuid);

impl RunId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    pub fn parse(value: &str) -> Result<Self> {
        let Some(uuid) = value.strip_prefix("run-") else {
            bail!("invalid Run identity: {value}");
        };
        Ok(Self(Uuid::parse_str(uuid).map_err(|_| {
            anyhow::anyhow!("invalid Run identity: {value}")
        })?))
    }
}

impl Default for RunId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for RunId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "run-{}", self.0)
    }
}

impl FromStr for RunId {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        Self::parse(value)
    }
}

impl Serialize for RunId {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for RunId {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, JsonSchema)]
#[serde(transparent)]
pub struct ServiceName(String);

impl ServiceName {
    pub fn parse(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        let bytes = value.as_bytes();
        let valid_endpoint = |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
        if bytes.is_empty()
            || bytes.len() > 63
            || !valid_endpoint(bytes[0])
            || !valid_endpoint(bytes[bytes.len() - 1])
            || !bytes
                .iter()
                .all(|byte| valid_endpoint(*byte) || *byte == b'-')
        {
            bail!("invalid Managed Service name: {value}");
        }
        Ok(Self(value))
    }
}

impl fmt::Display for ServiceName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for ServiceName {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        Self::parse(value)
    }
}

impl<'de> Deserialize<'de> for ServiceName {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
#[serde(transparent)]
pub struct Digest(String);

impl Digest {
    /// The SHA-256 identity of `bytes`. Constructing an identity from content
    /// is the identity type's own job, which lets protocol types state
    /// invariants about their own digests without depending on `integrity`.
    /// `integrity` still owns streaming digests, canonical JSON, and every
    /// exact-byte read and write.
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        let mut hexadecimal = String::with_capacity(64);
        for byte in hasher.finalize() {
            write!(&mut hexadecimal, "{byte:02x}").expect("writing to a String cannot fail");
        }
        Self(format!("sha256:{hexadecimal}"))
    }

    pub fn parse(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        let Some(hex) = value.strip_prefix("sha256:") else {
            bail!("unsupported OCI digest: {value}");
        };
        if hex.len() != 64
            || !hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            bail!("unsupported OCI digest: {value}");
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn hex(&self) -> &str {
        &self.0[7..]
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for Digest {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        Self::parse(value)
    }
}

impl<'de> Deserialize<'de> for Digest {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OciDescriptor {
    pub digest: Digest,
    pub size: u64,
    pub media_type: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Architecture {
    Amd64,
    Arm64,
}

impl fmt::Display for Architecture {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Amd64 => formatter.write_str("amd64"),
            Self::Arm64 => formatter.write_str("arm64"),
        }
    }
}

impl FromStr for Architecture {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "amd64" => Ok(Self::Amd64),
            "arm64" => Ok(Self::Arm64),
            _ => bail!("unsupported architecture: {value}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Platform {
    pub os: OperatingSystem,
    pub architecture: Architecture,
}

impl Platform {
    #[must_use]
    pub const fn linux(architecture: Architecture) -> Self {
        Self {
            os: OperatingSystem::Linux,
            architecture,
        }
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}", self.os, self.architecture)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum OperatingSystem {
    Linux,
}

impl fmt::Display for OperatingSystem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("linux")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ImageView {
    pub manifest: OciDescriptor,
    pub config: OciDescriptor,
    pub platform: Platform,
    pub layers: Vec<OciDescriptor>,
    pub diff_ids: Vec<Digest>,
    pub parent_manifest: Option<Digest>,
    pub added_layers: Vec<Digest>,
}

impl ImageView {
    pub fn validate(&self) -> Result<()> {
        if self.layers.len() != self.diff_ids.len() {
            bail!("OCI Image layers and rootfs.diff_ids must have equal length");
        }
        if self
            .added_layers
            .iter()
            .any(|added| !self.layers.iter().any(|layer| layer.digest == *added))
        {
            bail!("added_layers must refer to OCI Image layers");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "availability", rename_all = "snake_case")]
pub enum StoredBytes {
    Available {
        digest: Digest,
        size: u64,
    },
    Partial {
        digest: Digest,
        size: u64,
        limit_bytes: u64,
        reason: String,
    },
    Unavailable {
        error: String,
    },
    NotApplicable,
}

impl StoredBytes {
    #[must_use]
    pub fn digest(&self) -> Option<&Digest> {
        match self {
            Self::Available { digest, .. } | Self::Partial { digest, .. } => Some(digest),
            Self::Unavailable { .. } | Self::NotApplicable => None,
        }
    }

    #[must_use]
    pub const fn size(&self) -> Option<u64> {
        match self {
            Self::Available { size, .. } | Self::Partial { size, .. } => Some(*size),
            Self::Unavailable { .. } | Self::NotApplicable => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "availability", rename_all = "snake_case")]
pub enum ImageSlot {
    Available { manifest: OciDescriptor },
    Unavailable { error: String },
    NotApplicable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProcessOutcome {
    ProcessExited,
    TimedOut,
    CaptureLimitExceeded,
    Cancelled,
    NotStarted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProcessFacts {
    pub terminal_outcome: ProcessOutcome,
    pub exit_code: Option<i32>,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
    pub oom_killed: Option<bool>,
    pub backend_error: Option<String>,
}

impl ProcessFacts {
    #[must_use]
    pub const fn not_started() -> Self {
        Self {
            terminal_outcome: ProcessOutcome::NotStarted,
            exit_code: None,
            started_at: None,
            ended_at: None,
            oom_killed: None,
            backend_error: None,
        }
    }

    pub fn validate(&self) -> Result<()> {
        match self.terminal_outcome {
            ProcessOutcome::NotStarted => {
                if self.exit_code.is_some()
                    || self.started_at.is_some()
                    || self.oom_killed.is_some()
                {
                    bail!("not_started Process facts contain process execution evidence");
                }
            }
            ProcessOutcome::ProcessExited => {
                if self.exit_code.is_none() {
                    bail!("process_exited Process facts require an exit code");
                }
                self.validate_execution_times()?;
            }
            ProcessOutcome::TimedOut | ProcessOutcome::CaptureLimitExceeded => {
                self.validate_execution_times()?;
            }
            ProcessOutcome::Cancelled => match (self.started_at, self.ended_at) {
                (Some(_), Some(_)) => self.validate_execution_times()?,
                (None, Some(_)) if self.exit_code.is_none() && self.oom_killed.is_none() => {}
                _ => bail!("cancelled Process facts contain incomplete execution evidence"),
            },
        }
        Ok(())
    }

    fn validate_execution_times(&self) -> Result<()> {
        let started_at = self
            .started_at
            .context("started Process facts require started_at")?;
        let ended_at = self
            .ended_at
            .context("started Process facts require ended_at")?;
        if ended_at < started_at {
            bail!("Process ended_at precedes started_at");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "availability", rename_all = "snake_case")]
pub enum ProcessSlot {
    Available { facts: ProcessFacts },
    Unavailable { error: String },
}

impl ProcessSlot {
    #[must_use]
    pub const fn available(facts: ProcessFacts) -> Self {
        Self::Available { facts }
    }

    #[must_use]
    pub const fn facts(&self) -> Option<&ProcessFacts> {
        match self {
            Self::Available { facts } => Some(facts),
            Self::Unavailable { .. } => None,
        }
    }

    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Available { facts } => facts.validate(),
            Self::Unavailable { error } if error.is_empty() => {
                bail!("unavailable Process slot requires an error")
            }
            Self::Unavailable { .. } => Ok(()),
        }
    }

    /// Whether the slot positively records that no process ever started. An
    /// unavailable slot says nothing either way and answers `false`.
    #[must_use]
    pub fn was_not_started(&self) -> bool {
        self.facts().is_some_and(|facts| {
            facts.terminal_outcome == ProcessOutcome::NotStarted && facts.started_at.is_none()
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NetworkControl {
    None,
    Egress,
}

impl fmt::Display for NetworkControl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => formatter.write_str("none"),
            Self::Egress => formatter.write_str("egress"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RunControls {
    pub stdin: StoredBytes,
    pub timeout_seconds: u64,
    pub stdout_limit_bytes: u64,
    pub stderr_limit_bytes: u64,
    pub network: NetworkControl,
}

impl RunControls {
    /// Reject limits a Run cannot honour. Separate from `new` so a caller can
    /// refuse a request before reading any input or creating any state.
    pub fn validate_limits(
        timeout_seconds: u64,
        stdout_limit_bytes: u64,
        stderr_limit_bytes: u64,
    ) -> Result<()> {
        if timeout_seconds == 0 {
            bail!("Run timeout must be greater than zero seconds");
        }
        if stdout_limit_bytes == 0 || stderr_limit_bytes == 0 {
            bail!("stream limits must be greater than zero");
        }
        if stdout_limit_bytes > MAX_CAPTURED_STREAM_BYTES
            || stderr_limit_bytes > MAX_CAPTURED_STREAM_BYTES
        {
            bail!("stream limits must not exceed {MAX_CAPTURED_STREAM_BYTES} bytes");
        }
        Ok(())
    }

    pub fn new(
        stdin: StoredBytes,
        timeout_seconds: u64,
        stdout_limit_bytes: u64,
        stderr_limit_bytes: u64,
        network: NetworkControl,
    ) -> Result<Self> {
        Self::validate_limits(timeout_seconds, stdout_limit_bytes, stderr_limit_bytes)?;
        Ok(Self {
            stdin,
            timeout_seconds,
            stdout_limit_bytes,
            stderr_limit_bytes,
            network,
        })
    }

    pub fn validate(&self) -> Result<()> {
        Self::validate_limits(
            self.timeout_seconds,
            self.stdout_limit_bytes,
            self.stderr_limit_bytes,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BackendFacts {
    pub name: String,
    pub version: String,
    pub platform: Platform,
    pub network: NetworkControl,
    pub run_network: Option<RunNetworkFacts>,
    pub details: BackendDetails,
}

impl BackendDetails {
    /// The backend name these facts belong to.
    ///
    /// `name` is the identity a consumer reads and `details` are the facts
    /// behind it, so one determines the other. Deriving it here is what lets
    /// every producer and every validator agree on the correspondence instead
    /// of each restating it.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Docker { .. } => "docker",
            Self::NativeLinux { .. } => "native_linux",
        }
    }
}

impl BackendFacts {
    /// The invariants every consumer of a public Run Record may rely on.
    ///
    /// These are protocol invariants, not backend policy: a record whose name
    /// contradicts its own facts describes no backend that exists, and a
    /// record with no version identifies nothing. Constraints that only one
    /// backend can state -- which runtime realizations pair, which profiles
    /// are supported -- stay with that backend.
    pub fn validate(&self) -> Result<()> {
        if self.name != self.details.name() {
            bail!(
                "backend name is {} but its facts describe {}",
                self.name,
                self.details.name()
            );
        }
        if self.version.is_empty() {
            bail!("backend version must not be empty");
        }
        if let Some(network) = &self.run_network {
            network.validate(self.network)?;
        }
        if let BackendDetails::NativeLinux { runtime_size, .. } = &self.details
            && *runtime_size == 0
        {
            bail!("native backend runtime artifact size must be positive");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RunNetworkFacts {
    pub namespace_device: u64,
    pub namespace_inode: u64,
    pub realization: RunNetworkRealization,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunResolverSource {
    EtcResolvConf,
    SystemdResolvedUplink,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RunResolverFacts {
    pub source: RunResolverSource,
    pub nameservers: Vec<String>,
    pub content_digest: Digest,
    pub content_size: u64,
}

impl RunResolverFacts {
    /// The exact `/etc/resolv.conf` bytes this record describes, checked
    /// against the digest and size the record claims for them. A record whose
    /// digest does not cover its own nameservers is not a record of anything,
    /// so this is verified wherever the record is validated rather than only
    /// on the path that produced it.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        let bytes = self.render()?;
        if self.content_size != bytes.len() as u64 {
            bail!("Run resolver content size differs from its canonical bytes");
        }
        if self.content_digest != Digest::of(&bytes) {
            bail!("Run resolver content digest differs from its canonical bytes");
        }
        Ok(bytes)
    }

    /// Render the nameservers this record carries. Rules here hold for any
    /// conforming implementation; which addresses a particular host allocator
    /// reserves is that backend's concern, not the record's.
    fn render(&self) -> Result<Vec<u8>> {
        if self.nameservers.is_empty() || self.nameservers.len() > 3 {
            bail!("Run resolver must contain between one and three nameservers");
        }
        let mut seen = std::collections::BTreeSet::new();
        let mut bytes = Vec::new();
        for nameserver in &self.nameservers {
            let address = nameserver
                .parse::<std::net::Ipv4Addr>()
                .map_err(|_| anyhow::anyhow!("Run resolver nameserver is not an IPv4 address"))?;
            if address.is_unspecified()
                || address.is_loopback()
                || address.is_link_local()
                || address.is_multicast()
                || address == std::net::Ipv4Addr::BROADCAST
            {
                bail!("Run resolver nameserver cannot answer queries");
            }
            if !seen.insert(address) || nameserver != &address.to_string() {
                bail!("Run resolver nameservers must be unique canonical IPv4 addresses");
            }
            bytes.extend_from_slice(b"nameserver ");
            bytes.extend_from_slice(nameserver.as_bytes());
            bytes.push(b'\n');
        }
        Ok(bytes)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RunNetworkRealization {
    LoopbackOnly,
    Ipv4NatEgress {
        guest_address: String,
        gateway: String,
        prefix_length: u8,
        resolver: RunResolverFacts,
    },
}

impl RunNetworkFacts {
    pub fn validate(&self, requested: NetworkControl) -> Result<()> {
        if self.namespace_device == 0 || self.namespace_inode == 0 {
            bail!("Run network namespace identity must be positive");
        }
        match (&self.realization, requested) {
            (RunNetworkRealization::LoopbackOnly, NetworkControl::None) => Ok(()),
            (
                RunNetworkRealization::Ipv4NatEgress {
                    guest_address,
                    gateway,
                    prefix_length,
                    resolver,
                },
                NetworkControl::Egress,
            ) => {
                if *prefix_length != 30 {
                    bail!("Run IPv4 egress network must use a /30 subnet");
                }
                let guest = guest_address
                    .parse::<std::net::Ipv4Addr>()
                    .map_err(|_| anyhow::anyhow!("Run network guest IPv4 address is invalid"))?;
                let gateway = gateway
                    .parse::<std::net::Ipv4Addr>()
                    .map_err(|_| anyhow::anyhow!("Run network gateway IPv4 address is invalid"))?;
                let network = u32::from(guest) & !3;
                if u32::from(gateway) != network + 1 || u32::from(guest) != network + 2 {
                    bail!("Run IPv4 egress addresses do not form the expected /30 subnet");
                }
                resolver.canonical_bytes()?;
                Ok(())
            }
            _ => bail!("Run network realization does not match the requested network control"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BackendDetails {
    Docker {
        context: String,
        endpoint_kind: String,
        engine_id: String,
    },
    NativeLinux {
        runtime_name: String,
        runtime_version: String,
        runtime_commit: String,
        runtime_spec: String,
        runtime_digest: Digest,
        runtime_size: u64,
        kernel_release: String,
        runtime_invocation: NativeRuntimeInvocation,
        runtime_config: NativeRuntimeConfigRealization,
        filesystem: NativeFilesystemRealization,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NativeRuntimeInvocation {
    Direct,
    ApparmorProfile { profile: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NativeRuntimeConfigRealization {
    Accepted,
    RootlessSingleId { digest: Digest, size: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NativeFilesystemRealization {
    OverlayFs {
        profile: String,
    },
    WritableMaterialized {
        container_uid: u32,
        host_uid: u32,
        container_gid: u32,
        host_gid: u32,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct OperationError {
    pub scope: OperationErrorScope,
    pub phase: String,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OperationErrorScope {
    Run,
    Primary,
    ManagedService,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TcpReadinessCondition {
    pub port: u16,
    pub timeout_seconds: u64,
}

impl TcpReadinessCondition {
    pub fn validate(&self) -> Result<()> {
        if self.port == 0 {
            bail!("Managed Service readiness port must be nonzero");
        }
        if self.timeout_seconds == 0 {
            bail!("Managed Service readiness timeout must be nonzero");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ManagedServiceCondition {
    pub name: ServiceName,
    pub requested_image_reference: Option<String>,
    pub initial_image: OciDescriptor,
    pub runtime_config: StoredBytes,
    pub readiness: TcpReadinessCondition,
}

impl ManagedServiceCondition {
    pub fn validate(&self) -> Result<()> {
        if !matches!(self.runtime_config, StoredBytes::Available { .. }) {
            bail!("Managed Service Runtime Config must be available");
        }
        self.readiness.validate()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ManagedServiceReadiness {
    Ready {
        observed_at: DateTime<Utc>,
        attempts: u32,
    },
    TimedOut {
        observed_at: DateTime<Utc>,
        attempts: u32,
    },
    ServiceExited {
        observed_at: DateTime<Utc>,
        attempts: u32,
    },
    Cancelled {
        observed_at: DateTime<Utc>,
        attempts: u32,
    },
    ProbeError {
        observed_at: DateTime<Utc>,
        attempts: u32,
        error: String,
    },
}

impl ManagedServiceReadiness {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Ready { attempts, .. } | Self::TimedOut { attempts, .. } => {
                if *attempts == 0 {
                    bail!("Managed Service readiness observation must include an attempt");
                }
            }
            Self::ServiceExited { .. } | Self::Cancelled { .. } => {}
            Self::ProbeError { error, .. } => {
                if error.is_empty() {
                    bail!("Managed Service readiness probe error must not be empty");
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ManagedServiceFacts {
    pub name: ServiceName,
    pub requested_image_reference: Option<String>,
    pub initial_image: OciDescriptor,
    pub runtime_config: StoredBytes,
    pub readiness_condition: TcpReadinessCondition,
    pub readiness: ManagedServiceReadiness,
    pub process: ProcessSlot,
    pub stdout: StoredBytes,
    pub stderr: StoredBytes,
    pub final_image: ImageSlot,
    pub operation_errors: Vec<OperationError>,
}

impl ManagedServiceFacts {
    pub fn validate(&self) -> Result<()> {
        if !matches!(self.runtime_config, StoredBytes::Available { .. }) {
            bail!("Managed Service Runtime Config must be available");
        }
        self.readiness_condition.validate()?;
        self.readiness.validate()?;
        self.process.validate()?;
        if self
            .operation_errors
            .iter()
            .any(|error| error.scope != OperationErrorScope::ManagedService)
        {
            bail!("Managed Service operation error has a non-service scope");
        }
        Ok(())
    }
}

pub(crate) const ACCEPTED_RUN_RECORD_SCHEMA_VERSION: u32 = 3;
pub(crate) const TERMINAL_RUN_RECORD_SCHEMA_VERSION: u32 = 7;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AcceptedRunRecord {
    /// Published as a JSON Schema `const` so consumers discover the expected
    /// version instead of hardcoding it.
    #[schemars(extend("const" = ACCEPTED_RUN_RECORD_SCHEMA_VERSION))]
    pub schema_version: u32,
    pub run_id: RunId,
    pub lifecycle: AcceptedLifecycle,
    pub accepted_at: DateTime<Utc>,
    pub requested_image_reference: Option<String>,
    pub initial_image: OciDescriptor,
    pub runtime_config: StoredBytes,
    pub controls: RunControls,
    pub managed_service: Option<ManagedServiceCondition>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AcceptedLifecycle {
    Accepted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TerminalRunRecord {
    /// Published as a JSON Schema `const` so consumers discover the expected
    /// version instead of hardcoding it.
    #[schemars(extend("const" = TERMINAL_RUN_RECORD_SCHEMA_VERSION))]
    pub schema_version: u32,
    pub run_id: RunId,
    pub lifecycle: TerminalLifecycle,
    pub accepted_at: DateTime<Utc>,
    pub terminal_at: DateTime<Utc>,
    pub requested_image_reference: Option<String>,
    pub initial_image: OciDescriptor,
    pub runtime_config: StoredBytes,
    pub controls: RunControls,
    pub backend: Option<BackendFacts>,
    pub process: ProcessSlot,
    pub stdout: StoredBytes,
    pub stderr: StoredBytes,
    pub final_image: ImageSlot,
    pub operation_errors: Vec<OperationError>,
    pub managed_service: Option<ManagedServiceFacts>,
}

impl TerminalRunRecord {
    /// Every rule a terminal Run Record must satisfy to be a coherent account
    /// of one Run. Producers and the persistence layer both check it, so an
    /// unpersisted record is held to the same contract as a stored one.
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != TERMINAL_RUN_RECORD_SCHEMA_VERSION {
            bail!(
                "unsupported terminal Run Record schema version: expected {TERMINAL_RUN_RECORD_SCHEMA_VERSION}, received {}",
                self.schema_version
            );
        }
        if let Some(service) = &self.managed_service {
            service.validate()?;
        }
        self.controls.validate()?;
        self.process.validate()?;
        if self
            .operation_errors
            .iter()
            .any(|error| error.scope == OperationErrorScope::ManagedService)
        {
            bail!("top-level operation error has Managed Service scope");
        }
        if let Some(backend) = &self.backend {
            backend.validate()?;
        }
        self.validate_network_participation()
    }

    /// A Run network is shared by every participant, so a Managed Service Run
    /// that started anything must record one. A single-participant Run only has
    /// network facts when egress was requested.
    fn validate_network_participation(&self) -> Result<()> {
        match (&self.managed_service, &self.backend) {
            (Some(_), Some(backend)) if backend.run_network.is_some() => Ok(()),
            (Some(service), Some(_))
                if self.process.was_not_started() && service.process.was_not_started() =>
            {
                Ok(())
            }
            (Some(_), _) => bail!(
                "Managed Service terminal facts require Run network facts unless both processes were not started"
            ),
            (None, Some(backend))
                if backend.run_network.is_some() && backend.network != NetworkControl::Egress =>
            {
                bail!("single-participant Run network facts require network=egress")
            }
            _ => Ok(()),
        }
    }
}

impl AcceptedRunRecord {
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != ACCEPTED_RUN_RECORD_SCHEMA_VERSION {
            bail!(
                "unsupported accepted Run Record schema version: expected {ACCEPTED_RUN_RECORD_SCHEMA_VERSION}, received {}",
                self.schema_version
            );
        }
        self.controls.validate()?;
        if let Some(service) = &self.managed_service {
            service.validate()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TerminalLifecycle {
    Terminal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum RunRecord {
    Accepted(Box<AcceptedRunRecord>),
    Terminal(Box<TerminalRunRecord>),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn docker_details() -> BackendDetails {
        BackendDetails::Docker {
            context: "default".to_owned(),
            endpoint_kind: "unix_socket".to_owned(),
            engine_id: "engine".to_owned(),
        }
    }

    fn native_details(runtime_size: u64) -> BackendDetails {
        BackendDetails::NativeLinux {
            runtime_name: "runc".to_owned(),
            runtime_version: "1.5.1".to_owned(),
            runtime_commit: "fixture".to_owned(),
            runtime_spec: "1.3.0".to_owned(),
            runtime_digest: Digest::of(b"runc"),
            runtime_size,
            kernel_release: "fixture".to_owned(),
            runtime_invocation: NativeRuntimeInvocation::Direct,
            runtime_config: NativeRuntimeConfigRealization::Accepted,
            filesystem: NativeFilesystemRealization::OverlayFs {
                profile: "index=on".to_owned(),
            },
        }
    }

    fn facts(name: &str, version: &str, details: BackendDetails) -> BackendFacts {
        BackendFacts {
            name: name.to_owned(),
            version: version.to_owned(),
            platform: Platform::linux(Architecture::Arm64),
            network: NetworkControl::None,
            run_network: None,
            details,
        }
    }

    /// A public record whose name contradicts its own facts describes no
    /// backend that exists, whichever way the two were swapped.
    #[test]
    fn backend_name_and_facts_cannot_disagree() {
        facts("docker", "29.7.2", docker_details())
            .validate()
            .expect("docker facts");
        facts("native_linux", "0.2.0", native_details(12))
            .validate()
            .expect("native facts");

        for (name, details) in [
            ("docker", native_details(12)),
            ("native_linux", docker_details()),
            ("", docker_details()),
            ("runc", native_details(12)),
        ] {
            let error = facts(name, "1.0", details)
                .validate()
                .expect_err("mismatched backend identity must fail closed");
            assert!(
                error.to_string().contains("but its facts describe"),
                "unexpected error: {error:#}"
            );
        }
    }

    #[test]
    fn backend_facts_require_a_version_and_a_real_runtime_artifact() {
        let error = facts("docker", "", docker_details())
            .validate()
            .expect_err("an unversioned backend identifies nothing");
        assert!(
            error.to_string().contains("version must not be empty"),
            "unexpected error: {error:#}"
        );

        let error = facts("native_linux", "0.2.0", native_details(0))
            .validate()
            .expect_err("a zero-size runtime artifact is not a runtime artifact");
        assert!(
            error.to_string().contains("runtime artifact size"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn service_name_is_a_bounded_lowercase_dns_label() {
        for valid in ["postgres", "db-2", "0"] {
            assert_eq!(
                ServiceName::parse(valid).expect("valid name").to_string(),
                valid
            );
        }
        for invalid in ["", "Postgres", "-db", "db-", "db.local", &"a".repeat(64)] {
            assert!(ServiceName::parse(invalid).is_err());
        }
        assert!(serde_json::from_str::<ServiceName>(r#""DB""#).is_err());
    }

    #[test]
    fn readiness_validation_preserves_zero_attempt_pre_probe_failures() {
        let observed_at = Utc::now();
        assert!(
            ManagedServiceReadiness::Ready {
                observed_at,
                attempts: 0,
            }
            .validate()
            .is_err()
        );
        ManagedServiceReadiness::ServiceExited {
            observed_at,
            attempts: 0,
        }
        .validate()
        .expect("service may exit before the first probe");
        ManagedServiceReadiness::Cancelled {
            observed_at,
            attempts: 0,
        }
        .validate()
        .expect("cancellation may arrive before the first probe");
        ManagedServiceReadiness::ProbeError {
            observed_at,
            attempts: 0,
            error: "probe helper failed to start".to_owned(),
        }
        .validate()
        .expect("probe setup may fail before the first attempt");
    }

    #[test]
    fn process_facts_reject_contradictory_execution_evidence() {
        let observed_at = Utc::now();
        ProcessFacts {
            terminal_outcome: ProcessOutcome::ProcessExited,
            exit_code: Some(0),
            started_at: Some(observed_at),
            ended_at: Some(observed_at),
            oom_killed: Some(false),
            backend_error: None,
        }
        .validate()
        .expect("complete process facts");

        let missing_exit = ProcessFacts {
            terminal_outcome: ProcessOutcome::ProcessExited,
            exit_code: None,
            started_at: Some(observed_at),
            ended_at: Some(observed_at),
            oom_killed: None,
            backend_error: Some("observation failed".to_owned()),
        };
        assert!(missing_exit.validate().is_err());

        let false_start = ProcessFacts {
            terminal_outcome: ProcessOutcome::NotStarted,
            exit_code: None,
            started_at: Some(observed_at),
            ended_at: Some(observed_at),
            oom_killed: None,
            backend_error: None,
        };
        assert!(false_start.validate().is_err());
    }

    #[test]
    fn resolver_facts_reconstruct_only_canonical_ipv4_nameserver_lines() {
        let expected = b"nameserver 192.0.2.53\nnameserver 198.51.100.7\n";
        let facts = RunResolverFacts {
            source: RunResolverSource::SystemdResolvedUplink,
            nameservers: vec!["192.0.2.53".to_owned(), "198.51.100.7".to_owned()],
            content_digest: Digest::of(expected),
            content_size: expected.len() as u64,
        };
        assert_eq!(
            facts.canonical_bytes().expect("canonical resolver"),
            expected
        );

        for address in [
            "0.0.0.0",
            "127.0.0.53",
            "169.254.1.1",
            "224.0.0.1",
            "255.255.255.255",
        ] {
            let line = format!("nameserver {address}\n");
            let mut invalid = facts.clone();
            invalid.nameservers = vec![address.to_owned()];
            invalid.content_size = line.len() as u64;
            invalid.content_digest = Digest::of(line.as_bytes());
            assert!(invalid.canonical_bytes().is_err(), "accepted {address}");
        }
    }

    #[test]
    fn resolver_facts_reject_a_digest_that_does_not_cover_the_nameservers() {
        let facts = RunResolverFacts {
            source: RunResolverSource::EtcResolvConf,
            nameservers: vec!["192.0.2.53".to_owned()],
            content_digest: Digest::of(b"nameserver 198.51.100.7\n"),
            content_size: b"nameserver 192.0.2.53\n".len() as u64,
        };
        let error = facts.canonical_bytes().expect_err("mismatched digest");
        assert!(
            format!("{error:#}").contains("content digest differs"),
            "unexpected error: {error:#}"
        );
    }
}
