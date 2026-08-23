use std::fmt;
use std::str::FromStr;

use anyhow::{Context as _, Result, bail};
use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BackendFacts {
    pub name: String,
    pub version: String,
    pub platform: Platform,
    pub network: NetworkControl,
    pub run_network: Option<RunNetworkFacts>,
    pub details: BackendDetails,
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
    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        if self.nameservers.is_empty() || self.nameservers.len() > 3 {
            bail!("Run resolver must contain between one and three nameservers");
        }
        let mut seen = std::collections::BTreeSet::new();
        let mut bytes = Vec::new();
        for nameserver in &self.nameservers {
            let address = nameserver
                .parse::<std::net::Ipv4Addr>()
                .map_err(|_| anyhow::anyhow!("Run resolver nameserver is not an IPv4 address"))?;
            let octets = address.octets();
            if address.is_unspecified()
                || address.is_loopback()
                || address.is_link_local()
                || address.is_multicast()
                || address == std::net::Ipv4Addr::BROADCAST
                || (octets[0] == 10 && octets[1] == 240)
            {
                bail!("Run resolver nameserver is not reachable by the IPv4 egress profile");
            }
            if !seen.insert(address) || nameserver != &address.to_string() {
                bail!("Run resolver nameservers must be unique canonical IPv4 addresses");
            }
            bytes.extend_from_slice(b"nameserver ");
            bytes.extend_from_slice(nameserver.as_bytes());
            bytes.push(b'\n');
        }
        if self.content_size != bytes.len() as u64 {
            bail!("Run resolver content size differs from its canonical bytes");
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
        let facts = RunResolverFacts {
            source: RunResolverSource::SystemdResolvedUplink,
            nameservers: vec!["192.0.2.53".to_owned(), "198.51.100.7".to_owned()],
            content_digest: Digest::parse(format!("sha256:{}", "0".repeat(64))).expect("digest"),
            content_size: 46,
        };
        assert_eq!(
            facts.canonical_bytes().expect("canonical resolver"),
            b"nameserver 192.0.2.53\nnameserver 198.51.100.7\n"
        );

        for address in [
            "0.0.0.0",
            "127.0.0.53",
            "169.254.1.1",
            "224.0.0.1",
            "255.255.255.255",
            "10.240.0.1",
        ] {
            let mut invalid = facts.clone();
            invalid.nameservers = vec![address.to_owned()];
            invalid.content_size = format!("nameserver {address}\n").len() as u64;
            assert!(invalid.canonical_bytes().is_err(), "accepted {address}");
        }
    }
}
