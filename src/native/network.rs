//! The Run network: a private namespace per Run, with optional outbound-only
//! IPv4 egress.
//!
//! `network=none` gets loopback and nothing else. `network=egress` gets a /30
//! veth pair out of a fixed host pool, NAT to the host's uplink, and rules that
//! stop one Run from reaching another. The namespace is held open by a separate
//! holder process so it survives a supervisor crash and can be reclaimed by
//! reconciliation.
//!
//! Host-wide resources — the address pool, the NAT table — are taken under a
//! host lock, because concurrent Runs allocate from the same space.

use std::ffi::OsStr;
use std::fs::TryLockError;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::net::Ipv4Addr;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tempfile::NamedTempFile;

use crate::core::{RunId, RunNetworkFacts, RunNetworkRealization, RunResolverFacts};

const POLL_INTERVAL: Duration = Duration::from_millis(10);
const MAX_HELPER_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_HELPER_INPUT_BYTES: usize = 64 * 1024;
const EGRESS_PLAN_SCHEMA_VERSION: u32 = 1;
const EGRESS_POOL_PREFIX: &str = "10.240.0.0/16";
const EGRESS_POOL_FIRST_OCTET: u8 = 10;
const EGRESS_POOL_SECOND_OCTET: u8 = 240;
const EGRESS_SUBNET_COUNT: u16 = 16_384;
const NETWORK_HOLDER_DIRECTORY: &str = "network-holder";
const NETWORK_HOLDER_IDENTITY: &str = "identity.json";
const NETWORK_HOLDER_STOP: &str = "stop";
const NETWORK_HOLDER_SCHEMA_VERSION: u32 = 1;
const MAX_NETWORK_HOLDER_IDENTITY_BYTES: u64 = 4 * 1024;
const HOST_NETWORK_LOCK_DIRECTORY: &str = "/run/runlab";
const HOST_NETWORK_LOCK_FILE: &str = "network-allocation.lock";

mod lifecycle;

use lifecycle::{
    LinkOwnership, NativeNetworkHelperOutput, SharedLoopbackNetwork, TableOwnership,
    disable_ipv6_for_interface, process_start_time_ticks,
};
pub(crate) use lifecycle::{
    NativeNetworkBinding, RunNetwork, connect_loopback_tcp, hold_network_namespace,
};
/// Whether `address` falls inside the pool this backend hands to Run networks.
/// A host resolver in that range would be a Run address, never a real server.
pub(crate) fn is_egress_pool_address(address: Ipv4Addr) -> bool {
    let octets = address.octets();
    octets[0] == EGRESS_POOL_FIRST_OCTET && octets[1] == EGRESS_POOL_SECOND_OCTET
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RunNetworkMode {
    LoopbackOnly,
    EgressIpv4,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RunNetworkPlan {
    schema_version: u32,
    run_id: RunId,
    mode: RunNetworkMode,
    egress: Option<EgressIpv4Plan>,
}

impl RunNetworkPlan {
    #[must_use]
    pub(crate) const fn loopback(run_id: RunId) -> Self {
        Self {
            schema_version: EGRESS_PLAN_SCHEMA_VERSION,
            run_id,
            mode: RunNetworkMode::LoopbackOnly,
            egress: None,
        }
    }

    pub(crate) fn egress_ipv4(run_id: RunId, subnet_slot: u16) -> io::Result<Self> {
        if subnet_slot >= EGRESS_SUBNET_COUNT {
            return Err(invalid_input(format!(
                "egress subnet slot must be less than {EGRESS_SUBNET_COUNT}"
            )));
        }
        Ok(Self {
            schema_version: EGRESS_PLAN_SCHEMA_VERSION,
            run_id,
            mode: RunNetworkMode::EgressIpv4,
            egress: Some(EgressIpv4Plan::new(run_id, subnet_slot)),
        })
    }

    #[must_use]
    pub(crate) const fn mode(&self) -> RunNetworkMode {
        self.mode
    }

    #[must_use]
    pub(crate) const fn run_id(&self) -> RunId {
        self.run_id
    }

    #[must_use]
    pub(crate) const fn egress_subnet_count() -> u16 {
        EGRESS_SUBNET_COUNT
    }

    pub(crate) fn validate(&self) -> io::Result<()> {
        if self.schema_version != EGRESS_PLAN_SCHEMA_VERSION {
            return Err(invalid_data(format!(
                "unsupported Run network plan schema version: {}",
                self.schema_version
            )));
        }
        match (self.mode, self.egress.as_ref()) {
            (RunNetworkMode::LoopbackOnly, None) => Ok(()),
            (RunNetworkMode::EgressIpv4, Some(plan)) => {
                if plan.subnet_slot >= EGRESS_SUBNET_COUNT {
                    return Err(invalid_data("Run network plan subnet slot is invalid"));
                }
                if plan != &EgressIpv4Plan::new(self.run_id, plan.subnet_slot) {
                    return Err(invalid_data(
                        "Run network plan resources do not match its Run identity and subnet slot",
                    ));
                }
                Ok(())
            }
            _ => Err(invalid_data("Run network plan mode and resources disagree")),
        }
    }

    pub(crate) fn egress(&self) -> io::Result<&EgressIpv4Plan> {
        self.validate()?;
        match (self.mode, self.egress.as_ref()) {
            (RunNetworkMode::EgressIpv4, Some(plan)) => Ok(plan),
            (RunNetworkMode::LoopbackOnly, None) => Err(invalid_input(
                "loopback-only Run network plan has no egress resources",
            )),
            _ => unreachable!("Run network plan shape was validated"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EgressIpv4Plan {
    subnet_slot: u16,
    host_interface: String,
    peer_interface: String,
    guest_interface: String,
    host_address: Ipv4Addr,
    guest_address: Ipv4Addr,
    prefix_length: u8,
    host_mac: String,
    guest_mac: String,
    nft_table: String,
    owner: String,
}

impl EgressIpv4Plan {
    fn new(run_id: RunId, subnet_slot: u16) -> Self {
        let digest = Sha256::digest(run_id.to_string().as_bytes());
        let suffix = hexadecimal(&digest[..6]);
        let offset = u32::from(subnet_slot) * 4;
        let network = u32::from(Ipv4Addr::new(10, 240, 0, 0)) + offset;
        let host_address = Ipv4Addr::from(network + 1);
        let guest_address = Ipv4Addr::from(network + 2);
        Self {
            subnet_slot,
            host_interface: format!("rlh{}", &suffix[..10]),
            peer_interface: format!("rlp{}", &suffix[..10]),
            guest_interface: "eth0".to_owned(),
            host_address,
            guest_address,
            prefix_length: 30,
            host_mac: mac_address(&digest, 0),
            guest_mac: mac_address(&digest, 1),
            nft_table: format!("runlab_{}", &suffix[..12]),
            owner: format!("runlab:{run_id}"),
        }
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) const fn subnet_slot(&self) -> u16 {
        self.subnet_slot
    }

    #[must_use]
    pub(crate) fn host_interface(&self) -> &str {
        &self.host_interface
    }

    #[must_use]
    pub(crate) fn peer_interface(&self) -> &str {
        &self.peer_interface
    }

    #[must_use]
    pub(crate) fn guest_interface(&self) -> &str {
        &self.guest_interface
    }

    #[must_use]
    pub(crate) const fn host_address(&self) -> Ipv4Addr {
        self.host_address
    }

    #[must_use]
    pub(crate) const fn guest_address(&self) -> Ipv4Addr {
        self.guest_address
    }

    #[must_use]
    pub(crate) const fn prefix_length(&self) -> u8 {
        self.prefix_length
    }

    #[must_use]
    pub(crate) fn host_mac(&self) -> &str {
        &self.host_mac
    }

    #[must_use]
    pub(crate) fn guest_mac(&self) -> &str {
        &self.guest_mac
    }

    #[must_use]
    pub(crate) fn nft_table(&self) -> &str {
        &self.nft_table
    }

    #[must_use]
    pub(crate) fn owner(&self) -> &str {
        &self.owner
    }

    #[must_use]
    pub(crate) fn host_cidr(&self) -> String {
        format!("{}/{}", self.host_address, self.prefix_length)
    }

    #[must_use]
    pub(crate) fn guest_cidr(&self) -> String {
        format!("{}/{}", self.guest_address, self.prefix_length)
    }

    #[must_use]
    pub(crate) fn subnet_cidr(&self) -> String {
        let mask = u32::MAX << (32 - self.prefix_length);
        let network = Ipv4Addr::from(u32::from(self.host_address) & mask);
        format!("{network}/{}", self.prefix_length)
    }

    #[must_use]
    pub(crate) fn nft_create_batch(&self) -> Vec<u8> {
        let table = &self.nft_table;
        let host = nft_string(&self.host_interface);
        let owner = nft_string(&self.owner);
        let guest = self.guest_address;
        format!(
            "add table ip {table} {{ comment {owner}; }}\n\
             add chain ip {table} input {{ type filter hook input priority 0; policy accept; }}\n\
             add chain ip {table} output {{ type filter hook output priority 0; policy accept; }}\n\
             add chain ip {table} forward {{ type filter hook forward priority 0; policy accept; }}\n\
             add chain ip {table} postrouting {{ type nat hook postrouting priority 100; policy accept; }}\n\
             add rule ip {table} input iifname {host} drop\n\
             add rule ip {table} output oifname {host} drop\n\
             add rule ip {table} forward iifname {host} ip saddr != {guest} drop\n\
             add rule ip {table} forward iifname {host} ip daddr {EGRESS_POOL_PREFIX} drop\n\
             add rule ip {table} forward iifname {host} ct state new,established,related accept\n\
             add rule ip {table} forward iifname {host} drop\n\
             add rule ip {table} forward oifname {host} ip daddr {guest} ct state established,related accept\n\
             add rule ip {table} forward oifname {host} drop\n\
             add rule ip {table} postrouting ip saddr {guest} ip daddr != {EGRESS_POOL_PREFIX} masquerade\n"
        )
        .into_bytes()
    }

    #[must_use]
    pub(crate) fn nft_delete_batch(&self) -> Vec<u8> {
        format!("delete table ip {}\n", self.nft_table).into_bytes()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RunNetworkResources {
    pub plan: RunNetworkPlan,
    pub namespace: NativeNetworkIdentity,
    pub holder_pid: u32,
    pub holder_start_time_ticks: u64,
}

impl RunNetworkResources {
    pub(crate) fn facts(&self, resolver: Option<RunResolverFacts>) -> io::Result<RunNetworkFacts> {
        let realization = match self.plan.mode() {
            RunNetworkMode::LoopbackOnly => {
                if resolver.is_some() {
                    return Err(invalid_input(
                        "loopback-only Run network cannot have resolver facts",
                    ));
                }
                RunNetworkRealization::LoopbackOnly
            }
            RunNetworkMode::EgressIpv4 => {
                let resolver = resolver
                    .ok_or_else(|| invalid_input("IPv4 egress network requires resolver facts"))?;
                let egress = self.plan.egress()?;
                RunNetworkRealization::Ipv4NatEgress {
                    guest_address: egress.guest_address().to_string(),
                    gateway: egress.host_address().to_string(),
                    prefix_length: egress.prefix_length(),
                    resolver,
                }
            }
        };
        Ok(RunNetworkFacts {
            namespace_device: self.namespace.namespace_device,
            namespace_inode: self.namespace.namespace_inode,
            realization,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct NativeNetworkIdentity {
    pub namespace_device: u64,
    pub namespace_inode: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NetworkHolderIdentity {
    schema_version: u32,
    run_id: RunId,
    pid: u32,
    start_time_ticks: u64,
    namespace: NativeNetworkIdentity,
}

impl NetworkHolderIdentity {
    fn current(run_id: RunId) -> io::Result<Self> {
        let pid = std::process::id();
        let namespace = namespace_identity(Path::new("/proc/self/ns/net"))?;
        Ok(Self {
            schema_version: NETWORK_HOLDER_SCHEMA_VERSION,
            run_id,
            pid,
            start_time_ticks: process_start_time_ticks(pid)?,
            namespace: NativeNetworkIdentity {
                namespace_device: namespace.device,
                namespace_inode: namespace.inode,
            },
        })
    }

    fn validate(&self, run_id: RunId) -> io::Result<()> {
        if self.schema_version != NETWORK_HOLDER_SCHEMA_VERSION {
            return Err(invalid_data(format!(
                "unsupported network holder identity schema version: {}",
                self.schema_version
            )));
        }
        if self.run_id != run_id {
            return Err(invalid_data(
                "network holder identity belongs to a different Run",
            ));
        }
        if self.pid == 0
            || self.start_time_ticks == 0
            || self.namespace.namespace_device == 0
            || self.namespace.namespace_inode == 0
        {
            return Err(invalid_data("network holder identity is incomplete"));
        }
        Ok(())
    }

    fn matches_live_process(&self) -> io::Result<bool> {
        match process_start_time_ticks(self.pid) {
            Ok(actual) if actual == self.start_time_ticks => {}
            Ok(_) => return Ok(false),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        }
        let path = PathBuf::from(format!("/proc/{}/ns/net", self.pid));
        match namespace_identity(&path) {
            Ok(actual) => Ok(actual.device == self.namespace.namespace_device
                && actual.inode == self.namespace.namespace_inode),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct NetworkHolderHandle {
    directory: PathBuf,
    run_id: RunId,
}

impl NetworkHolderHandle {
    pub(crate) fn prepare(attempt_workspace: &Path, run_id: RunId) -> io::Result<Self> {
        validate_private_directory(attempt_workspace)?;
        let directory = attempt_workspace.join(NETWORK_HOLDER_DIRECTORY);
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700);
        builder.create(&directory).map_err(|error| {
            contextual(
                &error,
                format!(
                    "failed to create network holder directory {}",
                    directory.display()
                ),
            )
        })?;
        sync_directory(attempt_workspace)?;
        let handle = Self { directory, run_id };
        handle.validate_directory()?;
        Ok(handle)
    }

    pub(crate) fn open(attempt_workspace: &Path, run_id: RunId) -> io::Result<Option<Self>> {
        validate_private_directory(attempt_workspace)?;
        let directory = attempt_workspace.join(NETWORK_HOLDER_DIRECTORY);
        match fs::symlink_metadata(&directory) {
            Ok(_) => {
                let handle = Self { directory, run_id };
                handle.validate_directory()?;
                Ok(Some(handle))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(contextual(
                &error,
                "failed to inspect network holder directory",
            )),
        }
    }

    pub(crate) fn request_stop(&self, timeout: Duration) -> io::Result<()> {
        // The caller owns the native-attempt lock, so publishing this tombstone orders before any possible future holder spawn for the same attempt.
        self.validate_directory()?;
        self.publish_stop()?;
        let deadline = deadline(timeout, "network holder shutdown timeout")?;
        loop {
            let Some(identity) = self.read_identity()? else {
                return Ok(());
            };
            if !identity.matches_live_process()? {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "network holder did not stop after its durable stop request",
                ));
            }
            sleep_until(deadline);
        }
    }

    fn publish_identity(&self, identity: &NetworkHolderIdentity) -> io::Result<()> {
        self.validate_directory()?;
        identity.validate(self.run_id)?;
        let bytes = serde_json::to_vec(identity).map_err(|error| {
            invalid_data(format!("failed to encode network holder identity: {error}"))
        })?;
        let path = self.identity_path();
        let mut temporary = NamedTempFile::new_in(&self.directory).map_err(|error| {
            contextual(
                &error,
                "failed to create network holder identity staging file",
            )
        })?;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
        temporary.write_all(&bytes)?;
        temporary.as_file_mut().sync_all()?;
        temporary.persist_noclobber(&path).map_err(|error| {
            contextual(&error.error, "failed to publish network holder identity")
        })?;
        sync_directory(&self.directory)
    }

    fn read_identity(&self) -> io::Result<Option<NetworkHolderIdentity>> {
        let path = self.identity_path();
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(contextual(
                    &error,
                    "failed to inspect network holder identity",
                ));
            }
        };
        validate_private_file(&path, &metadata)?;
        if metadata.len() > MAX_NETWORK_HOLDER_IDENTITY_BYTES {
            return Err(invalid_data(format!(
                "network holder identity exceeds {MAX_NETWORK_HOLDER_IDENTITY_BYTES} bytes"
            )));
        }
        let identity = serde_json::from_slice::<NetworkHolderIdentity>(&fs::read(&path)?).map_err(
            |error| invalid_data(format!("network holder identity is invalid: {error}")),
        )?;
        identity.validate(self.run_id)?;
        Ok(Some(identity))
    }

    fn stop_requested(&self) -> io::Result<bool> {
        let path = self.stop_path();
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                validate_private_file(&path, &metadata)?;
                Ok(true)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(contextual(
                &error,
                "failed to inspect network holder stop request",
            )),
        }
    }

    fn publish_stop(&self) -> io::Result<()> {
        let path = self.stop_path();
        let file = match OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let metadata = fs::symlink_metadata(&path)?;
                validate_private_file(&path, &metadata)?;
                return Ok(());
            }
            Err(error) => {
                return Err(contextual(
                    &error,
                    "failed to publish network holder stop request",
                ));
            }
        };
        file.sync_all()?;
        sync_directory(&self.directory)
    }

    fn validate_directory(&self) -> io::Result<()> {
        validate_private_directory(&self.directory)
    }

    fn identity_path(&self) -> PathBuf {
        self.directory.join(NETWORK_HOLDER_IDENTITY)
    }

    fn stop_path(&self) -> PathBuf {
        self.directory.join(NETWORK_HOLDER_STOP)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct NativeNetworkTools {
    unshare: PathBuf,
    nsenter: PathBuf,
    ip: PathBuf,
    cat: PathBuf,
    holder_executable: PathBuf,
}

impl NativeNetworkTools {
    pub(crate) fn discover() -> io::Result<Self> {
        Self::from_paths_with_holder(
            find_executable("unshare")?,
            find_executable("nsenter")?,
            find_executable("ip")?,
            find_executable("cat")?,
            std::env::current_exe()
                .map_err(|error| contextual(&error, "failed to locate the RunLab executable"))?,
        )
    }

    #[cfg(test)]
    pub(crate) fn from_paths(
        unshare: impl AsRef<Path>,
        nsenter: impl AsRef<Path>,
        ip: impl AsRef<Path>,
        cat: impl AsRef<Path>,
    ) -> io::Result<Self> {
        let holder_executable = std::env::current_exe()
            .map_err(|error| contextual(&error, "failed to locate the RunLab executable"))?;
        Self::from_paths_with_holder(unshare, nsenter, ip, cat, holder_executable)
    }

    fn from_paths_with_holder(
        unshare: impl AsRef<Path>,
        nsenter: impl AsRef<Path>,
        ip: impl AsRef<Path>,
        cat: impl AsRef<Path>,
        holder_executable: impl AsRef<Path>,
    ) -> io::Result<Self> {
        Ok(Self {
            unshare: validate_executable(unshare.as_ref())?,
            nsenter: validate_executable(nsenter.as_ref())?,
            ip: validate_executable(ip.as_ref())?,
            cat: validate_executable(cat.as_ref())?,
            holder_executable: validate_executable(holder_executable.as_ref())?,
        })
    }
}

#[derive(Debug, Clone)]
pub(crate) struct EgressNetworkTools {
    ip: PathBuf,
    nft: PathBuf,
    conntrack: PathBuf,
}

#[derive(Debug)]
pub(crate) struct HostNetworkLock {
    _file: File,
}

#[derive(Debug)]
pub(crate) struct EgressRouteSnapshot {
    prefixes: Vec<Ipv4Prefix>,
}

#[derive(Debug, Clone, Copy)]
struct Ipv4Prefix {
    network: u32,
    prefix_length: u8,
}

impl EgressNetworkTools {
    pub(crate) fn discover() -> io::Result<Self> {
        Self::from_paths(
            find_executable("ip")?,
            find_executable("nft")?,
            find_executable("conntrack")?,
        )
    }

    pub(crate) fn from_paths(
        ip: impl AsRef<Path>,
        nft: impl AsRef<Path>,
        conntrack: impl AsRef<Path>,
    ) -> io::Result<Self> {
        Ok(Self {
            ip: validate_executable(ip.as_ref())?,
            nft: validate_executable(nft.as_ref())?,
            conntrack: validate_executable(conntrack.as_ref())?,
        })
    }

    pub(crate) fn preflight(&self, timeout: Duration) -> io::Result<()> {
        deadline(timeout, "network helper timeout")?;
        let forwarding = fs::read_to_string("/proc/sys/net/ipv4/ip_forward")
            .map_err(|error| contextual(&error, "failed to inspect IPv4 forwarding"))?;
        if forwarding.trim() != "1" {
            return Err(other("rootful IPv4 egress requires net.ipv4.ip_forward=1"));
        }
        let mut ip = Command::new(&self.ip);
        ip.arg("-Version");
        require_success(&run_bounded(ip, timeout)?, "failed to preflight ip")?;
        let mut nft = Command::new(&self.nft);
        nft.arg("--version");
        require_success(&run_bounded(nft, timeout)?, "failed to preflight nft")?;
        let mut conntrack = Command::new(&self.conntrack);
        conntrack.arg("--version");
        require_success(
            &run_bounded(conntrack, timeout)?,
            "failed to preflight conntrack",
        )?;
        Ok(())
    }

    pub(crate) fn cleanup_plan(&self, plan: &RunNetworkPlan, timeout: Duration) -> io::Result<()> {
        let deadline = deadline(timeout, "network cleanup timeout")?;
        let host_lock = acquire_host_network_lock(remaining(deadline, "network cleanup timeout")?)?;
        self.cleanup_plan_locked(
            plan,
            remaining(deadline, "network cleanup timeout")?,
            &host_lock,
        )
    }

    pub(crate) fn cleanup_plan_locked(
        &self,
        plan: &RunNetworkPlan,
        timeout: Duration,
        _host_lock: &HostNetworkLock,
    ) -> io::Result<()> {
        let plan = plan.egress()?;
        let deadline = deadline(timeout, "network cleanup timeout")?;
        let link = self.inspect_link(plan, remaining(deadline, "network cleanup timeout")?)?;
        if link == LinkOwnership::Foreign {
            return Err(other(format!(
                "refusing to clean network resources while interface is not owned by this Run: {}",
                plan.host_interface()
            )));
        }
        self.cleanup_nft(plan, remaining(deadline, "network cleanup timeout")?)?;
        let remove_conntrack = match link {
            LinkOwnership::Owned => true,
            LinkOwnership::CreatePending => false,
            LinkOwnership::Absent => {
                let routes =
                    self.route_snapshot(remaining(deadline, "network cleanup timeout")?)?;
                !routes.overlaps(plan)?
            }
            LinkOwnership::Foreign => unreachable!(),
        };
        if remove_conntrack {
            self.cleanup_conntrack(plan, remaining(deadline, "network cleanup timeout")?)?;
        }
        self.cleanup_link(plan, remaining(deadline, "network cleanup timeout")?)
    }

    pub(crate) fn route_snapshot(&self, timeout: Duration) -> io::Result<EgressRouteSnapshot> {
        let mut command = Command::new(&self.ip);
        command.args(["-4", "-json", "route", "show", "table", "all"]);
        let output = run_bounded(command, timeout)?;
        require_success(&output, "failed to inspect host IPv4 routes")?;
        parse_route_snapshot(&output.stdout)
    }

    pub(crate) fn subnet_is_available(
        &self,
        plan: &RunNetworkPlan,
        routes: &EgressRouteSnapshot,
        timeout: Duration,
    ) -> io::Result<bool> {
        let plan = plan.egress()?;
        if routes.overlaps(plan)? {
            return Ok(false);
        }
        self.conntrack_is_empty(plan, timeout)
    }

    fn apply(
        &self,
        plan: &EgressIpv4Plan,
        network: &mut SharedLoopbackNetwork,
        timeout: Duration,
    ) -> io::Result<()> {
        let deadline = deadline(timeout, "network setup timeout")?;
        self.require_absent(plan, remaining(deadline, "network setup timeout")?)?;
        let binding = network.binding()?;
        let (holder_pid, _) = network.holder_identity();

        self.apply_host(plan, holder_pid, deadline)?;
        self.apply_guest(plan, &binding, deadline)?;
        self.nft_batch(
            &plan.nft_create_batch(),
            remaining(deadline, "network setup timeout")?,
            "failed to install Run egress firewall",
        )
    }

    fn apply_host(
        &self,
        plan: &EgressIpv4Plan,
        holder_pid: u32,
        deadline: Instant,
    ) -> io::Result<()> {
        self.ip(
            [
                "link",
                "add",
                "name",
                plan.host_interface(),
                "address",
                plan.host_mac(),
                "type",
                "veth",
                "peer",
                "name",
                plan.peer_interface(),
                "address",
                plan.guest_mac(),
            ],
            remaining(deadline, "network setup timeout")?,
            "failed to create Run egress veth",
        )?;
        disable_ipv6_for_interface(plan.host_interface())?;
        disable_ipv6_for_interface(plan.peer_interface())?;
        self.ip(
            [
                "link",
                "set",
                "dev",
                plan.host_interface(),
                "alias",
                plan.owner(),
            ],
            remaining(deadline, "network setup timeout")?,
            "failed to mark Run egress host interface ownership",
        )?;
        self.ip(
            [
                "link",
                "set",
                "dev",
                plan.peer_interface(),
                "alias",
                plan.owner(),
            ],
            remaining(deadline, "network setup timeout")?,
            "failed to mark Run egress peer interface ownership",
        )?;
        self.ip(
            [
                "address",
                "add",
                &plan.host_cidr(),
                "dev",
                plan.host_interface(),
            ],
            remaining(deadline, "network setup timeout")?,
            "failed to assign Run egress host address",
        )?;
        self.ip(
            ["link", "set", "dev", plan.host_interface(), "up"],
            remaining(deadline, "network setup timeout")?,
            "failed to enable Run egress host interface",
        )?;
        self.ip(
            [
                "link",
                "set",
                "dev",
                plan.peer_interface(),
                "netns",
                &holder_pid.to_string(),
            ],
            remaining(deadline, "network setup timeout")?,
            "failed to move Run egress peer into the private namespace",
        )
    }

    fn apply_guest(
        &self,
        plan: &EgressIpv4Plan,
        binding: &NativeNetworkBinding,
        deadline: Instant,
    ) -> io::Result<()> {
        self.ip_in_namespace(
            binding,
            [
                "link",
                "set",
                "dev",
                plan.peer_interface(),
                "name",
                plan.guest_interface(),
            ],
            remaining(deadline, "network setup timeout")?,
            "failed to name Run egress guest interface",
        )?;
        self.ip_in_namespace(
            binding,
            [
                "address",
                "add",
                &plan.guest_cidr(),
                "dev",
                plan.guest_interface(),
            ],
            remaining(deadline, "network setup timeout")?,
            "failed to assign Run egress guest address",
        )?;
        self.ip_in_namespace(
            binding,
            ["link", "set", "dev", plan.guest_interface(), "up"],
            remaining(deadline, "network setup timeout")?,
            "failed to enable Run egress guest interface",
        )?;
        self.ip_in_namespace(
            binding,
            [
                "route",
                "add",
                "default",
                "via",
                &plan.host_address().to_string(),
                "dev",
                plan.guest_interface(),
            ],
            remaining(deadline, "network setup timeout")?,
            "failed to install Run egress default route",
        )
    }

    fn require_absent(&self, plan: &EgressIpv4Plan, timeout: Duration) -> io::Result<()> {
        if self.inspect_link(plan, timeout)? != LinkOwnership::Absent {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "Run egress interface already exists: {}",
                    plan.host_interface()
                ),
            ));
        }
        if self.inspect_table(plan, timeout)? != TableOwnership::Absent {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("Run egress nft table already exists: {}", plan.nft_table()),
            ));
        }
        Ok(())
    }

    fn cleanup_nft(&self, plan: &EgressIpv4Plan, timeout: Duration) -> io::Result<()> {
        match self.inspect_table(plan, timeout)? {
            TableOwnership::Absent => Ok(()),
            TableOwnership::Owned => self.nft_batch(
                &plan.nft_delete_batch(),
                timeout,
                "failed to delete Run egress firewall",
            ),
            TableOwnership::Foreign => Err(other(format!(
                "refusing to delete nft table not owned by this Run: {}",
                plan.nft_table()
            ))),
        }
    }

    fn cleanup_link(&self, plan: &EgressIpv4Plan, timeout: Duration) -> io::Result<()> {
        match self.inspect_link(plan, timeout)? {
            LinkOwnership::Absent => Ok(()),
            LinkOwnership::Owned | LinkOwnership::CreatePending => self.ip(
                ["link", "delete", "dev", plan.host_interface()],
                timeout,
                "failed to delete Run egress veth",
            ),
            LinkOwnership::Foreign => Err(other(format!(
                "refusing to delete interface not owned by this Run: {}",
                plan.host_interface()
            ))),
        }
    }

    fn cleanup_conntrack(&self, plan: &EgressIpv4Plan, timeout: Duration) -> io::Result<()> {
        let deadline = deadline(timeout, "conntrack cleanup timeout")?;
        if self.conntrack_is_empty(plan, remaining(deadline, "conntrack cleanup timeout")?)? {
            return Ok(());
        }
        let mut command = Command::new(&self.conntrack);
        command
            .env("LC_ALL", "C")
            .args(["-D", "--orig-src", &plan.guest_address().to_string()]);
        let output = run_bounded(command, remaining(deadline, "conntrack cleanup timeout")?)?;
        if output.status.success()
            || self.conntrack_is_empty(plan, remaining(deadline, "conntrack cleanup timeout")?)?
        {
            return Ok(());
        }
        Err(helper_failure(
            "failed to delete Run egress conntrack entries",
            &output,
        ))
    }

    fn conntrack_is_empty(&self, plan: &EgressIpv4Plan, timeout: Duration) -> io::Result<bool> {
        let mut command = Command::new(&self.conntrack);
        command.env("LC_ALL", "C").args([
            "-L",
            "--orig-src",
            &plan.guest_address().to_string(),
            "--output",
            "xml",
        ]);
        let output = run_bounded(command, timeout)?;
        require_success(&output, "failed to inspect Run egress conntrack entries")?;
        Ok(output.stdout.iter().all(u8::is_ascii_whitespace))
    }

    fn inspect_link(&self, plan: &EgressIpv4Plan, timeout: Duration) -> io::Result<LinkOwnership> {
        let sysfs = Path::new("/sys/class/net").join(plan.host_interface());
        if !sysfs.exists() {
            return Ok(LinkOwnership::Absent);
        }
        let mut command = Command::new(&self.ip);
        command.args([
            "-details",
            "-json",
            "link",
            "show",
            "dev",
            plan.host_interface(),
        ]);
        let output = run_bounded(command, timeout)?;
        if !output.status.success() {
            if !sysfs.exists() {
                return Ok(LinkOwnership::Absent);
            }
            return Err(helper_failure(
                "failed to inspect Run egress interface",
                &output,
            ));
        }
        classify_link(plan, &output.stdout)
    }

    fn inspect_table(
        &self,
        plan: &EgressIpv4Plan,
        timeout: Duration,
    ) -> io::Result<TableOwnership> {
        let deadline = deadline(timeout, "network firewall inspection timeout")?;
        let mut command = Command::new(&self.nft);
        command.args(["--json", "list", "table", "ip", plan.nft_table()]);
        let output = run_bounded(
            command,
            remaining(deadline, "network firewall inspection timeout")?,
        )?;
        if !output.status.success() {
            if nft_table_is_absent(&output.stderr) {
                return Ok(TableOwnership::Absent);
            }
            return Err(helper_failure(
                "failed to inspect Run egress firewall",
                &output,
            ));
        }
        let mut text_command = Command::new(&self.nft);
        text_command.args(["list", "table", "ip", plan.nft_table()]);
        let text_output = run_bounded(
            text_command,
            remaining(deadline, "network firewall inspection timeout")?,
        )?;
        if !text_output.status.success() {
            if nft_table_is_absent(&text_output.stderr) {
                return Ok(TableOwnership::Absent);
            }
            return Err(helper_failure(
                "failed to inspect Run egress firewall ownership",
                &text_output,
            ));
        }
        classify_table(plan, &output.stdout, &text_output.stdout)
    }

    fn ip<I, S>(&self, arguments: I, timeout: Duration, operation: &str) -> io::Result<()>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut command = Command::new(&self.ip);
        command.args(arguments);
        require_success(&run_bounded(command, timeout)?, operation)
    }

    fn ip_in_namespace<I, S>(
        &self,
        binding: &NativeNetworkBinding,
        arguments: I,
        timeout: Duration,
        operation: &str,
    ) -> io::Result<()>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        require_success(&binding.invoke(&self.ip, arguments, timeout)?, operation)
    }

    fn nft_batch(&self, input: &[u8], timeout: Duration, operation: &str) -> io::Result<()> {
        let mut command = Command::new(&self.nft);
        command.args(["--check", "--file", "-"]);
        require_success(
            &run_bounded_with_input(command, input, timeout)?,
            &format!("{operation} validation"),
        )?;
        let mut command = Command::new(&self.nft);
        command.args(["--file", "-"]);
        require_success(&run_bounded_with_input(command, input, timeout)?, operation)
    }
}

pub(crate) fn acquire_host_network_lock(timeout: Duration) -> io::Result<HostNetworkLock> {
    {
        if !rustix::process::geteuid().is_root() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "host network lock requires root",
            ));
        }
        let directory = Path::new(HOST_NETWORK_LOCK_DIRECTORY);
        prepare_host_network_lock_directory(directory)?;
        acquire_host_network_lock_in(directory, (0, 0), timeout)
    }
}

fn prepare_host_network_lock_directory(directory: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.mode(0o700);
    match builder.create(directory) {
        Ok(()) => fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            return Err(contextual(
                &error,
                "failed to create host network lock directory",
            ));
        }
    }
    verify_host_network_lock_directory(directory, (0, 0))
}

fn acquire_host_network_lock_in(
    directory: &Path,
    expected_owner: (u32, u32),
    timeout: Duration,
) -> io::Result<HostNetworkLock> {
    use rustix::fs::{Mode, OFlags, open};

    verify_host_network_lock_directory(directory, expected_owner)?;
    let path = directory.join(HOST_NETWORK_LOCK_FILE);
    let file = File::from(
        open(
            &path,
            OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )
        .map_err(io::Error::from)?,
    );
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || (metadata.uid(), metadata.gid()) != expected_owner
        || metadata.permissions().mode() & 0o777 != 0o600
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "host network lock is not an owner-controlled 0600 regular file",
        ));
    }
    let current = fs::symlink_metadata(&path)?;
    if current.file_type().is_symlink()
        || !current.is_file()
        || current.dev() != metadata.dev()
        || current.ino() != metadata.ino()
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "host network lock identity changed while opening it",
        ));
    }
    let deadline = deadline(timeout, "host network allocation timeout")?;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(HostNetworkLock { _file: file }),
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                thread::sleep(POLL_INTERVAL);
            }
            Err(TryLockError::WouldBlock) => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "host network lock timed out",
                ));
            }
            Err(TryLockError::Error(error)) => {
                return Err(contextual(&error, "failed to acquire host network lock"));
            }
        }
    }
}

fn verify_host_network_lock_directory(
    directory: &Path,
    expected_owner: (u32, u32),
) -> io::Result<()> {
    let metadata = fs::symlink_metadata(directory)?;
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || (metadata.uid(), metadata.gid()) != expected_owner
        || metadata.permissions().mode() & 0o777 != 0o700
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "host network lock directory is not owner-controlled mode 0700",
        ));
    }
    Ok(())
}

fn run_bounded(command: Command, timeout: Duration) -> io::Result<NativeNetworkHelperOutput> {
    run_bounded_command(command, None, timeout)
}

fn run_bounded_with_input(
    command: Command,
    input: &[u8],
    timeout: Duration,
) -> io::Result<NativeNetworkHelperOutput> {
    if input.len() > MAX_HELPER_INPUT_BYTES {
        return Err(invalid_input(format!(
            "network helper input exceeds {MAX_HELPER_INPUT_BYTES} bytes"
        )));
    }
    run_bounded_command(command, Some(input), timeout)
}

fn run_bounded_command(
    mut command: Command,
    input: Option<&[u8]>,
    timeout: Duration,
) -> io::Result<NativeNetworkHelperOutput> {
    let deadline = deadline(timeout, "network helper timeout")?;
    command.env("LC_ALL", "C");
    let mut child = command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| contextual(&error, "failed to start network helper"))?;
    let input_writer = match input {
        Some(input) => {
            let Some(mut stdin) = child.stdin.take() else {
                let _ = force_reap(&mut child)?;
                return Err(invalid_data("network helper did not expose stdin"));
            };
            let input = input.to_vec();
            Some(thread::spawn(move || stdin.write_all(&input)))
        }
        None => None,
    };
    let Some(stdout) = child.stdout.take() else {
        let _ = force_reap(&mut child)?;
        return Err(invalid_data("network helper did not expose stdout"));
    };
    let Some(stderr) = child.stderr.take() else {
        let _ = force_reap(&mut child)?;
        return Err(invalid_data("network helper did not expose stderr"));
    };
    let stdout = thread::spawn(move || read_bounded(stdout));
    let stderr = thread::spawn(move || read_bounded(stderr));

    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => {
                if let Err(cleanup_error) = force_reap(&mut child) {
                    return Err(other(format!(
                        "failed to poll network helper: {error}; cleanup also failed: {cleanup_error}"
                    )));
                }
                let input_error = join_input(input_writer);
                let stdout = join_capture(stdout)?;
                let stderr = join_capture(stderr)?;
                return Err(other(format!(
                    "failed to poll network helper: {error}; input: {}; stdout: {}; stderr: {}",
                    input_result(&input_error),
                    String::from_utf8_lossy(&stdout),
                    String::from_utf8_lossy(&stderr)
                )));
            }
        }
        if Instant::now() >= deadline {
            if let Err(cleanup_error) = force_reap(&mut child) {
                return Err(other(format!(
                    "network helper exceeded {timeout:?}; cleanup also failed: {cleanup_error}"
                )));
            }
            let input_error = join_input(input_writer);
            let stdout = join_capture(stdout)?;
            let stderr = join_capture(stderr)?;
            return Err(other(format!(
                "network helper exceeded {timeout:?}; input: {}; stdout: {}; stderr: {}",
                input_result(&input_error),
                String::from_utf8_lossy(&stdout),
                String::from_utf8_lossy(&stderr)
            )));
        }
        sleep_until(deadline);
    };
    let input_error = join_input(input_writer);
    let output = NativeNetworkHelperOutput {
        status,
        stdout: join_capture(stdout)?,
        stderr: join_capture(stderr)?,
    };
    if output.status.success()
        && let Err(error) = input_error
    {
        return Err(contextual(&error, "failed to write network helper input"));
    }
    Ok(output)
}

fn join_input(handle: Option<JoinHandle<io::Result<()>>>) -> io::Result<()> {
    handle.map_or(Ok(()), |handle| {
        handle
            .join()
            .map_err(|_| other("network helper input writer panicked"))?
    })
}

fn input_result(result: &io::Result<()>) -> String {
    match result {
        Ok(()) => "complete".to_owned(),
        Err(error) => format!("failed: {error}"),
    }
}

fn read_bounded(mut reader: impl Read) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .by_ref()
        .take(u64::try_from(MAX_HELPER_OUTPUT_BYTES).unwrap_or(u64::MAX) + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_HELPER_OUTPUT_BYTES {
        return Err(other(format!(
            "network helper output exceeds {MAX_HELPER_OUTPUT_BYTES} bytes"
        )));
    }
    Ok(bytes)
}

fn join_capture(handle: JoinHandle<io::Result<Vec<u8>>>) -> io::Result<Vec<u8>> {
    handle
        .join()
        .map_err(|_| other("network helper output reader panicked"))?
}

fn force_reap(child: &mut Child) -> io::Result<ExitStatus> {
    if let Some(status) = child
        .try_wait()
        .map_err(|error| contextual(&error, "failed to poll child before forced cleanup"))?
    {
        return Ok(status);
    }
    if let Err(kill_error) = child.kill() {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| contextual(&error, "failed to repoll child during forced cleanup"))?
        {
            return Ok(status);
        }
        return Err(contextual(
            &kill_error,
            "failed to kill child during forced cleanup",
        ));
    }
    child
        .wait()
        .map_err(|error| contextual(&error, "failed to reap child during forced cleanup"))
}

fn namespace_identity(path: &Path) -> io::Result<NamespaceFileIdentity> {
    let metadata = fs::metadata(path)?;
    Ok(NamespaceFileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NamespaceFileIdentity {
    device: u64,
    inode: u64,
}

fn find_executable(name: &str) -> io::Result<PathBuf> {
    let path = std::env::var_os("PATH").ok_or_else(|| invalid_input("PATH is not set"))?;
    for directory in std::env::split_paths(&path) {
        let candidate = directory.join(name);
        if candidate.is_file() {
            return validate_executable(&candidate);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("executable is not available in PATH: {name}"),
    ))
}

fn validate_executable(path: &Path) -> io::Result<PathBuf> {
    if !path.is_absolute() {
        return Err(invalid_input(format!(
            "helper executable path must be absolute: {}",
            path.display()
        )));
    }
    let path = path.canonicalize().map_err(|error| {
        contextual(
            &error,
            format!("failed to resolve helper executable {}", path.display()),
        )
    })?;
    let metadata = path.metadata().map_err(|error| {
        contextual(
            &error,
            format!("failed to inspect helper executable {}", path.display()),
        )
    })?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        return Err(invalid_input(format!(
            "helper executable is not an executable regular file: {}",
            path.display()
        )));
    }
    Ok(path)
}

fn validate_private_directory(path: &Path) -> io::Result<()> {
    if !path.is_absolute() {
        return Err(invalid_input(format!(
            "network holder path must be absolute: {}",
            path.display()
        )));
    }
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        contextual(
            &error,
            format!("failed to inspect network holder path {}", path.display()),
        )
    })?;
    if !metadata.file_type().is_dir() || metadata.permissions().mode() & 0o777 != 0o700 {
        return Err(invalid_data(format!(
            "network holder path must be a real 0700 directory: {}",
            path.display()
        )));
    }
    Ok(())
}

fn validate_private_file(path: &Path, metadata: &fs::Metadata) -> io::Result<()> {
    if !metadata.file_type().is_file() || metadata.permissions().mode() & 0o777 != 0o600 {
        return Err(invalid_data(format!(
            "network holder file must be a real 0600 file: {}",
            path.display()
        )));
    }
    Ok(())
}

fn sync_directory(path: &Path) -> io::Result<()> {
    fs::File::open(path)?.sync_all()
}

fn deadline(timeout: Duration, name: &str) -> io::Result<Instant> {
    if timeout.is_zero() {
        return Err(invalid_input(format!("{name} must be greater than zero")));
    }
    Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| invalid_input(format!("{name} is too large")))
}

fn remaining(deadline: Instant, name: &str) -> io::Result<Duration> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("{name} elapsed"),
        ));
    }
    Ok(remaining)
}

fn sleep_until(deadline: Instant) {
    thread::sleep(POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())));
}

fn helper_failure(operation: &str, output: &NativeNetworkHelperOutput) -> io::Error {
    other(format!(
        "{operation}: {}; stdout: {}; stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    ))
}

fn require_success(output: &NativeNetworkHelperOutput, operation: &str) -> io::Result<()> {
    if output.status.success() {
        Ok(())
    } else {
        Err(helper_failure(operation, output))
    }
}

impl EgressRouteSnapshot {
    fn overlaps(&self, plan: &EgressIpv4Plan) -> io::Result<bool> {
        let candidate = parse_ipv4_prefix(&plan.subnet_cidr())?;
        Ok(self
            .prefixes
            .iter()
            .any(|prefix| prefix.overlaps(candidate)))
    }
}

impl Ipv4Prefix {
    fn overlaps(self, other: Self) -> bool {
        let common = self.prefix_length.min(other.prefix_length);
        let common_mask = prefix_mask(common);
        self.network & common_mask == other.network & common_mask
    }
}

fn parse_route_snapshot(bytes: &[u8]) -> io::Result<EgressRouteSnapshot> {
    let routes: Vec<serde_json::Value> = serde_json::from_slice(bytes)
        .map_err(|error| invalid_data(format!("invalid ip route JSON: {error}")))?;
    let mut prefixes = Vec::with_capacity(routes.len());
    for route in routes {
        let Some(destination) = route.get("dst") else {
            continue;
        };
        let destination = destination
            .as_str()
            .ok_or_else(|| invalid_data("ip route destination is not a string"))?;
        if destination == "default" {
            continue;
        }
        prefixes.push(parse_ipv4_prefix(destination)?);
    }
    Ok(EgressRouteSnapshot { prefixes })
}

fn parse_ipv4_prefix(value: &str) -> io::Result<Ipv4Prefix> {
    let (address, prefix_length) = value.split_once('/').map_or((value, "32"), |parts| parts);
    let address = address
        .parse::<Ipv4Addr>()
        .map_err(|_| invalid_data(format!("invalid IPv4 route prefix: {value}")))?;
    let prefix_length = prefix_length
        .parse::<u8>()
        .ok()
        .filter(|length| *length <= 32)
        .ok_or_else(|| invalid_data(format!("invalid IPv4 route prefix: {value}")))?;
    let mask = prefix_mask(prefix_length);
    Ok(Ipv4Prefix {
        network: u32::from(address) & mask,
        prefix_length,
    })
}

fn prefix_mask(prefix_length: u8) -> u32 {
    if prefix_length == 0 {
        0
    } else {
        u32::MAX << (32 - prefix_length)
    }
}

fn classify_link(plan: &EgressIpv4Plan, bytes: &[u8]) -> io::Result<LinkOwnership> {
    #[derive(Deserialize)]
    struct LinkInfo {
        info_kind: Option<String>,
    }

    #[derive(Deserialize)]
    struct Link {
        ifname: String,
        address: Option<String>,
        ifalias: Option<String>,
        linkinfo: Option<LinkInfo>,
    }

    let links: Vec<Link> = serde_json::from_slice(bytes)
        .map_err(|error| invalid_data(format!("invalid ip link JSON: {error}")))?;
    let [link] = links.as_slice() else {
        return Err(invalid_data(
            "ip link inspection did not return exactly one interface",
        ));
    };
    if link.ifname != plan.host_interface
        || !link
            .address
            .as_deref()
            .is_some_and(|address| address.eq_ignore_ascii_case(&plan.host_mac))
        || link
            .linkinfo
            .as_ref()
            .and_then(|info| info.info_kind.as_deref())
            != Some("veth")
    {
        return Ok(LinkOwnership::Foreign);
    }
    match link.ifalias.as_deref() {
        Some(alias) if alias == plan.owner => Ok(LinkOwnership::Owned),
        None | Some("") => Ok(LinkOwnership::CreatePending),
        Some(_) => Ok(LinkOwnership::Foreign),
    }
}

fn classify_table(
    plan: &EgressIpv4Plan,
    json_bytes: &[u8],
    text_bytes: &[u8],
) -> io::Result<TableOwnership> {
    let value: serde_json::Value = serde_json::from_slice(json_bytes)
        .map_err(|error| invalid_data(format!("invalid nft table JSON: {error}")))?;
    let entries = value
        .get("nftables")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| invalid_data("nft table JSON has no nftables array"))?;
    let mut tables = entries.iter().filter_map(|entry| entry.get("table"));
    let Some(table) = tables.next() else {
        return Err(invalid_data("nft table JSON has no table object"));
    };
    if tables.next().is_some()
        || table.get("family").and_then(serde_json::Value::as_str) != Some("ip")
        || table.get("name").and_then(serde_json::Value::as_str) != Some(plan.nft_table())
    {
        return Ok(TableOwnership::Foreign);
    }
    if table
        .get("comment")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|comment| comment != plan.owner())
    {
        return Ok(TableOwnership::Foreign);
    }
    let text = std::str::from_utf8(text_bytes)
        .map_err(|error| invalid_data(format!("invalid nft table text: {error}")))?;
    let expected = format!("comment {}", nft_string(plan.owner()));
    Ok(
        if text
            .lines()
            .map(str::trim)
            .filter(|line| *line == expected)
            .count()
            == 1
        {
            TableOwnership::Owned
        } else {
            TableOwnership::Foreign
        },
    )
}

fn nft_table_is_absent(stderr: &[u8]) -> bool {
    let stderr = String::from_utf8_lossy(stderr);
    stderr.contains("No such file or directory") || stderr.contains("does not exist")
}

fn hexadecimal(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut value = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        value.push(char::from(DIGITS[usize::from(byte >> 4)]));
        value.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    value
}

fn mac_address(digest: &[u8], discriminator: u8) -> String {
    format!(
        "02:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        digest[6],
        digest[7],
        digest[8],
        digest[9],
        digest[10] ^ discriminator
    )
}

fn nft_string(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('"');
    for character in value.chars() {
        if matches!(character, '"' | '\\') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped.push('"');
    escaped
}

fn contextual(error: &io::Error, context: impl AsRef<str>) -> io::Error {
    io::Error::new(error.kind(), format!("{}: {error}", context.as_ref()))
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn other(message: impl Into<String>) -> io::Error {
    io::Error::other(message.into())
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::io::Cursor;
    use std::net::TcpListener;

    use super::*;

    fn run_id(value: &str) -> RunId {
        RunId::parse(value).expect("Run identity")
    }

    fn egress_plan() -> RunNetworkPlan {
        RunNetworkPlan::egress_ipv4(run_id("run-018f47e2-7c31-7b18-a780-bf56f69303d9"), 513)
            .expect("egress plan")
    }

    #[test]
    fn egress_plan_is_deterministic_bounded_and_round_trips() {
        let plan = egress_plan();
        let egress = plan.egress().expect("egress resources");
        assert_eq!(plan.mode(), RunNetworkMode::EgressIpv4);
        assert_eq!(
            plan.run_id(),
            run_id("run-018f47e2-7c31-7b18-a780-bf56f69303d9")
        );
        assert_eq!(egress.subnet_slot(), 513);
        assert_eq!(egress.host_address(), Ipv4Addr::new(10, 240, 8, 5));
        assert_eq!(egress.guest_address(), Ipv4Addr::new(10, 240, 8, 6));
        assert_eq!(egress.host_cidr(), "10.240.8.5/30");
        assert_eq!(egress.guest_cidr(), "10.240.8.6/30");
        assert_eq!(egress.subnet_cidr(), "10.240.8.4/30");
        assert_eq!(egress.prefix_length(), 30);
        assert_eq!(RunNetworkPlan::egress_subnet_count(), 16_384);
        assert!(egress.host_interface().len() <= 15);
        assert!(egress.peer_interface().len() <= 15);
        assert!(egress.nft_table().len() <= 32);
        assert_eq!(egress.guest_interface(), "eth0");
        assert!(egress.host_mac().starts_with("02:"));
        assert!(egress.guest_mac().starts_with("02:"));
        assert_ne!(egress.host_mac(), egress.guest_mac());
        assert_eq!(
            serde_json::from_slice::<RunNetworkPlan>(
                &serde_json::to_vec(&plan).expect("serialize plan")
            )
            .expect("deserialize plan"),
            plan
        );
        assert_eq!(
            RunNetworkPlan::egress_ipv4(plan.run_id(), 513).expect("same plan"),
            plan
        );
    }

    #[test]
    fn egress_plan_names_and_macs_are_run_scoped() {
        let first = egress_plan();
        let second =
            RunNetworkPlan::egress_ipv4(run_id("run-018f47e2-7c31-7b18-a780-bf56f69303da"), 513)
                .expect("second plan");
        let first = first.egress().expect("first resources");
        let second = second.egress().expect("second resources");
        assert_ne!(first.host_interface(), second.host_interface());
        assert_ne!(first.peer_interface(), second.peer_interface());
        assert_ne!(first.nft_table(), second.nft_table());
        assert_ne!(first.host_mac(), second.host_mac());
        assert_ne!(first.owner(), second.owner());
    }

    #[test]
    fn network_plan_rejects_invalid_shape_and_subnet_slot() {
        assert_eq!(
            RunNetworkPlan::egress_ipv4(RunId::new(), EGRESS_SUBNET_COUNT)
                .expect_err("out-of-range slot")
                .kind(),
            io::ErrorKind::InvalidInput
        );
        let loopback = RunNetworkPlan::loopback(RunId::new());
        assert_eq!(loopback.mode(), RunNetworkMode::LoopbackOnly);
        assert_eq!(
            loopback.egress().expect_err("egress resources").kind(),
            io::ErrorKind::InvalidInput
        );

        let mut value = serde_json::to_value(egress_plan()).expect("plan JSON");
        value["mode"] = serde_json::json!("loopback_only");
        let inconsistent: RunNetworkPlan = serde_json::from_value(value).expect("structural JSON");
        assert_eq!(
            inconsistent.egress().expect_err("mode mismatch").kind(),
            io::ErrorKind::InvalidData
        );

        let mut value = serde_json::to_value(egress_plan()).expect("plan JSON");
        value["egress"]["host_interface"] = serde_json::json!("eth0");
        let redirected: RunNetworkPlan = serde_json::from_value(value).expect("structural JSON");
        assert_eq!(
            redirected
                .validate()
                .expect_err("derived resource mismatch")
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn nft_batch_is_outbound_only_and_atomic_by_construction() {
        let plan = egress_plan();
        let egress = plan.egress().expect("egress resources");
        let batch = String::from_utf8(egress.nft_create_batch()).expect("nft batch");
        assert!(batch.starts_with("add table ip "));
        assert!(batch.contains(&format!("comment \"{}\"", egress.owner())));
        assert!(batch.contains(&format!(
            "input iifname \"{}\" drop",
            egress.host_interface()
        )));
        assert!(batch.contains(&format!(
            "output oifname \"{}\" drop",
            egress.host_interface()
        )));
        assert!(batch.contains("ip daddr 10.240.0.0/16 drop"));
        assert!(batch.contains("ct state established,related accept"));
        assert!(batch.contains(&format!(
            "postrouting ip saddr {} ip daddr != 10.240.0.0/16 masquerade",
            egress.guest_address()
        )));
        assert!(!batch.contains("dnat"));
        assert!(!batch.contains("redirect"));
        assert!(!batch.contains(" dport "));
        assert_eq!(
            String::from_utf8(egress.nft_delete_batch()).expect("delete batch"),
            format!("delete table ip {}\n", egress.nft_table())
        );
    }

    #[test]
    fn ownership_requires_exact_run_markers_or_create_fingerprint() {
        let plan = egress_plan();
        let egress = plan.egress().expect("egress resources");
        let link = |alias: Option<&str>, address: &str, kind: &str| {
            serde_json::to_vec(&serde_json::json!([{
                "ifname": egress.host_interface(),
                "address": address,
                "ifalias": alias,
                "linkinfo": {"info_kind": kind}
            }]))
            .expect("link JSON")
        };
        assert_eq!(
            classify_link(
                egress,
                &link(Some(egress.owner()), egress.host_mac(), "veth")
            )
            .expect("owned link"),
            LinkOwnership::Owned
        );
        assert_eq!(
            classify_link(egress, &link(None, egress.host_mac(), "veth")).expect("pending link"),
            LinkOwnership::CreatePending
        );
        assert_eq!(
            classify_link(
                egress,
                &link(Some("runlab:other"), egress.host_mac(), "veth")
            )
            .expect("foreign alias"),
            LinkOwnership::Foreign
        );
        assert_eq!(
            classify_link(egress, &link(None, "02:00:00:00:00:00", "veth")).expect("foreign MAC"),
            LinkOwnership::Foreign
        );
        assert_eq!(
            classify_link(egress, &link(None, egress.host_mac(), "dummy")).expect("foreign kind"),
            LinkOwnership::Foreign
        );
    }

    #[test]
    fn nft_ownership_requires_exact_family_name_and_comment() {
        let plan = egress_plan();
        let egress = plan.egress().expect("egress resources");
        let table = |family: &str, name: &str, comment: Option<&str>| {
            serde_json::to_vec(&serde_json::json!({
                "nftables": [
                    {"metainfo": {"json_schema_version": 1}},
                    {"table": {"family": family, "name": name, "comment": comment}}
                ]
            }))
            .expect("table JSON")
        };
        let owned_text = format!(
            "table ip {} {{\n\tcomment {}\n}}\n",
            egress.nft_table(),
            nft_string(egress.owner())
        );
        assert_eq!(
            classify_table(
                egress,
                &table("ip", egress.nft_table(), None),
                owned_text.as_bytes()
            )
            .expect("owned table without JSON comment"),
            TableOwnership::Owned
        );
        assert_eq!(
            classify_table(
                egress,
                &table("ip", egress.nft_table(), Some("runlab:other")),
                owned_text.as_bytes()
            )
            .expect("foreign JSON comment"),
            TableOwnership::Foreign
        );
        assert_eq!(
            classify_table(
                egress,
                &table("inet", egress.nft_table(), Some(egress.owner())),
                owned_text.as_bytes()
            )
            .expect("wrong family"),
            TableOwnership::Foreign
        );
        assert_eq!(
            classify_table(
                egress,
                &table("ip", egress.nft_table(), Some(egress.owner())),
                b"table ip runlab {\n\tcomment \"runlab:other\"\n}\n"
            )
            .expect("foreign text comment"),
            TableOwnership::Foreign
        );
        let duplicated = format!("{owned_text}comment {}\n", nft_string(egress.owner()));
        assert_eq!(
            classify_table(
                egress,
                &table("ip", egress.nft_table(), Some(egress.owner())),
                duplicated.as_bytes()
            )
            .expect("duplicate ownership comment"),
            TableOwnership::Foreign
        );
    }

    #[test]
    fn network_helper_io_is_bounded_before_process_mutation() {
        let oversized = vec![0_u8; MAX_HELPER_INPUT_BYTES + 1];
        let error = run_bounded_with_input(
            Command::new("/path/that/must/not/be-executed"),
            &oversized,
            Duration::from_secs(1),
        )
        .expect_err("oversized helper input");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);

        let output = vec![0_u8; MAX_HELPER_OUTPUT_BYTES + 1];
        let error = read_bounded(Cursor::new(output)).expect_err("oversized helper output");
        assert_eq!(error.kind(), io::ErrorKind::Other);
    }

    #[test]
    fn route_snapshot_detects_parent_exact_and_child_collisions() {
        let plan = egress_plan();
        let egress = plan.egress().expect("egress plan");
        let snapshot = parse_route_snapshot(br#"[{"dst":"default"},{"dst":"192.0.2.0/24"}]"#)
            .expect("available routes");
        assert!(!snapshot.overlaps(egress).expect("available subnet"));
        for destination in ["10.240.0.0/16", "10.240.8.4/30", "10.240.8.6/32"] {
            let bytes =
                serde_json::to_vec(&serde_json::json!([{"dst": destination}])).expect("route JSON");
            let snapshot = parse_route_snapshot(&bytes).expect("occupied routes");
            assert!(snapshot.overlaps(egress).expect("occupied subnet"));
        }
        assert_eq!(
            parse_route_snapshot(br#"{"dst":"10.240.8.4/30"}"#)
                .expect_err("non-array route result")
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn durable_holder_handle_is_private_idempotent_and_run_scoped() {
        let workspace = tempfile::tempdir().expect("workspace");
        fs::set_permissions(workspace.path(), fs::Permissions::from_mode(0o700))
            .expect("secure workspace");
        let run_id = RunId::new();
        let handle = NetworkHolderHandle::prepare(workspace.path(), run_id).expect("handle");
        assert_eq!(
            fs::symlink_metadata(&handle.directory)
                .expect("holder metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        let opened = NetworkHolderHandle::open(workspace.path(), run_id)
            .expect("open handle")
            .expect("existing handle");
        assert_eq!(opened.directory, handle.directory);
        assert!(opened.read_identity().expect("identity").is_none());

        opened
            .request_stop(Duration::from_secs(1))
            .expect("first stop request");
        opened
            .request_stop(Duration::from_secs(1))
            .expect("idempotent stop request");
        assert!(opened.stop_requested().expect("stop state"));
        assert_eq!(
            fs::symlink_metadata(opened.stop_path())
                .expect("stop metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        assert_eq!(
            NetworkHolderHandle::prepare(workspace.path(), run_id)
                .expect_err("holder cannot be recreated")
                .kind(),
            io::ErrorKind::AlreadyExists
        );
    }

    #[test]
    fn durable_holder_identity_rejects_cross_run_and_stale_processes() {
        let workspace = tempfile::tempdir().expect("workspace");
        fs::set_permissions(workspace.path(), fs::Permissions::from_mode(0o700))
            .expect("secure workspace");
        let run_id = RunId::new();
        let handle = NetworkHolderHandle::prepare(workspace.path(), run_id).expect("handle");
        let identity = NetworkHolderIdentity {
            schema_version: NETWORK_HOLDER_SCHEMA_VERSION,
            run_id,
            pid: std::process::id(),
            start_time_ticks: u64::MAX,
            namespace: NativeNetworkIdentity {
                namespace_device: 1,
                namespace_inode: 1,
            },
        };
        handle
            .publish_identity(&identity)
            .expect("publish identity");
        assert_eq!(
            handle.read_identity().expect("read identity"),
            Some(identity)
        );
        handle
            .request_stop(Duration::from_secs(1))
            .expect("stale holder is already stopped");

        assert_eq!(
            NetworkHolderHandle::open(workspace.path(), RunId::new())
                .expect("open handle")
                .expect("existing handle")
                .read_identity()
                .expect_err("cross-Run identity")
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn loopback_probe_connects_to_the_requested_port() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("listen");
        let port = listener.local_addr().expect("local address").port();
        let accepting = thread::spawn(move || listener.accept().expect("accept"));

        connect_loopback_tcp(port, Duration::from_secs(1)).expect("connect");

        let _ = accepting.join().expect("accept thread");
    }

    #[test]
    fn loopback_probe_rejects_invalid_bounds() {
        assert_eq!(
            connect_loopback_tcp(0, Duration::from_secs(1))
                .expect_err("zero port")
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            connect_loopback_tcp(1, Duration::ZERO)
                .expect_err("zero timeout")
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn helper_paths_must_be_absolute_executable_files() {
        let error = NativeNetworkTools::from_paths("unshare", "nsenter", "ip", "cat")
            .expect_err("relative helpers");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);

        let executable = std::env::current_exe().expect("test executable");
        NativeNetworkTools::from_paths(&executable, &executable, &executable, &executable)
            .expect("absolute executable helpers");
        EgressNetworkTools::from_paths(&executable, &executable, &executable)
            .expect("absolute egress helpers");
    }

    #[test]
    fn cleanup_with_allocation_lock_does_not_reacquire_it() {
        let directory = tempfile::tempdir().expect("helper directory");
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
            .expect("secure helper directory");
        let helper = directory.path().join("network-helper");
        fs::write(
            &helper,
            b"#!/bin/sh\ncase \"$1\" in\n  --json) echo 'No such file or directory' >&2; exit 1;;\n  -4) echo '[{\"dst\":\"10.240.8.4/30\"}]';;\n  *) exit 0;;\nesac\n",
        )
        .expect("write helper");
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o700))
            .expect("make helper executable");
        let tools =
            EgressNetworkTools::from_paths(&helper, &helper, &helper).expect("egress helper paths");
        let host_lock = acquire_host_network_lock_in(
            directory.path(),
            (
                rustix::process::geteuid().as_raw(),
                rustix::process::getegid().as_raw(),
            ),
            Duration::from_secs(1),
        )
        .expect("host lock");

        tools
            .cleanup_plan_locked(&egress_plan(), Duration::from_secs(1), &host_lock)
            .expect("cleanup under existing lock");
    }

    #[test]
    fn filesystem_allocation_lock_rejects_unsafe_directory_and_serializes_holders() {
        let directory = tempfile::tempdir().expect("lock directory");
        let uid = rustix::process::geteuid().as_raw();
        let gid = rustix::process::getegid().as_raw();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o755))
            .expect("unsafe permissions");
        let error =
            acquire_host_network_lock_in(directory.path(), (uid, gid), Duration::from_millis(20))
                .expect_err("unsafe directory must fail");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);

        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
            .expect("secure permissions");
        let first =
            acquire_host_network_lock_in(directory.path(), (uid, gid), Duration::from_secs(1))
                .expect("first lock");
        let error =
            acquire_host_network_lock_in(directory.path(), (uid, gid), Duration::from_millis(20))
                .expect_err("concurrent holder must wait");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        drop(first);
        acquire_host_network_lock_in(directory.path(), (uid, gid), Duration::from_secs(1))
            .expect("released lock can be reacquired");
    }

    #[test]
    #[ignore = "requires rootful Linux with unshare, nsenter, ip, and network namespace support"]
    fn rootful_shared_loopback_namespace() {
        let tools = NativeNetworkTools::discover().expect("network tools");
        let ip = tools.ip.clone();
        let mut network =
            SharedLoopbackNetwork::start(tools, Duration::from_secs(5)).expect("network");
        let (holder_pid, holder_start_time_ticks) = network.holder_identity();
        assert!(holder_pid > 0);
        assert!(holder_start_time_ticks > 0);
        assert_ne!(
            namespace_identity(Path::new("/proc/self/ns/net")).expect("host namespace"),
            NamespaceFileIdentity {
                device: network.identity().namespace_device,
                inode: network.identity().namespace_inode,
            }
        );
        let binding = network.binding().expect("network binding");
        let first_binding = binding.clone();
        let second_binding = binding.clone();
        let first_ip = ip.clone();
        let second_ip = ip.clone();
        let (first, second) = thread::scope(|scope| {
            let first = scope.spawn(move || inspect_loopback(&first_binding, &first_ip));
            let second = scope.spawn(move || inspect_loopback(&second_binding, &second_ip));
            (
                first.join().expect("first helper thread"),
                second.join().expect("second helper thread"),
            )
        });
        let first = first.expect("first entered helper");
        let second = second.expect("second entered helper");
        assert!(first.status.success());
        assert!(second.status.success());
        assert!(String::from_utf8_lossy(&first.stdout).contains("LOOPBACK"));
        assert!(String::from_utf8_lossy(&second.stdout).contains("LOOPBACK"));
        network.finish().expect("finish network");
        assert!(binding.entered_command(ip).is_err());
    }

    fn inspect_loopback(
        binding: &NativeNetworkBinding,
        ip: &Path,
    ) -> io::Result<NativeNetworkHelperOutput> {
        binding.invoke(
            ip,
            [
                OsString::from("-o"),
                OsString::from("link"),
                OsString::from("show"),
                OsString::from("dev"),
                OsString::from("lo"),
            ],
            Duration::from_secs(5),
        )
    }
}
