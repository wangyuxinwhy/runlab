use std::env;
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::fs::File;
use std::io::Read as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use uuid::Uuid;

use super::{GuestHandshake, TRANSPORT_VERSION};

pub(super) const INSTANCE: &str = "runlab";
const LIMA_VERSION: &str = "2.2.0";
const START_TIMEOUT: &str = "5m";
pub(super) const GUEST_BINARY_ROOT: &str = "/usr/local/libexec/runlab";
const RUNC_PATH: &str = "/usr/local/bin/runc";
const RUNC_VERSION: &str = "1.5.1";
pub(super) const STATE_PATH: &str = "/var/lib/runlab";
const SYSCTL_PATH: &str = "/etc/sysctl.d/90-runlab.conf";
const SYSCTL_CONTENTS: &[u8] = b"net.ipv4.ip_forward = 1\n";

const ARM64_IMAGE_LOCATION: &str = "https://cloud-images.ubuntu.com/releases/noble/release-20260705/ubuntu-24.04-server-cloudimg-arm64.img";
const ARM64_IMAGE_DIGEST: &str =
    "sha256:7df0201546f75b8bcc1044594c806c35749421ad3c9bc1be2a3ab806cfae39cc";
const AMD64_IMAGE_LOCATION: &str = "https://cloud-images.ubuntu.com/releases/noble/release-20260705/ubuntu-24.04-server-cloudimg-amd64.img";
const AMD64_IMAGE_DIGEST: &str =
    "sha256:ffe6203da54deeb6db5d2a98a83f9ec8e55f149d3f7ba622e1abe5fa966ee3d6";

pub(crate) struct ManagedVm {
    pub(super) limactl: PathBuf,
}

#[derive(Debug, Serialize)]
pub(crate) struct VmStatus {
    schema_version: u32,
    instance: &'static str,
    status: String,
    compatible: bool,
    problems: Vec<String>,
    lima_version: String,
    architecture: Option<String>,
    vm_type: Option<String>,
    cpus: Option<u16>,
    memory_bytes: Option<u64>,
    disk_bytes: Option<u64>,
    host_mounts: Option<usize>,
    image: Option<VmImage>,
    ready: bool,
    readiness_problems: Vec<String>,
    guest: Option<GuestHandshake>,
    runtime: Option<RuntimeProfile>,
}

#[derive(Debug, Serialize)]
pub(crate) struct VmInstallResult {
    schema_version: u32,
    instance: &'static str,
    binary: InstalledFile,
    runc: InstalledFile,
    guest: GuestHandshake,
    runtime: RuntimeProfile,
}

#[derive(Debug, Serialize)]
struct InstalledFile {
    path: String,
    digest: String,
    size: u64,
}

#[derive(Clone, Debug, Serialize)]
struct RuntimeProfile {
    runc_version: String,
    executables: Vec<String>,
    cgroup_version: Option<u8>,
    overlayfs: bool,
    ipv4_forwarding: bool,
}

impl RuntimeProfile {
    fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if self.runc_version != RUNC_VERSION {
            problems.push(format!("runc must be {RUNC_VERSION}"));
        }
        for name in ["ip", "iptables", "ip6tables", "nsenter"] {
            if !self.executables.iter().any(|value| value == name) {
                problems.push(format!("{name} is unavailable"));
            }
        }
        if self.cgroup_version != Some(2) {
            problems.push("cgroup v2 is unavailable".to_owned());
        }
        if !self.overlayfs {
            problems.push("OverlayFS is unavailable".to_owned());
        }
        if !self.ipv4_forwarding {
            problems.push("IPv4 forwarding is disabled".to_owned());
        }
        problems
    }
}

#[derive(Debug)]
pub(super) struct FileIdentity {
    pub(super) digest: String,
    pub(super) size: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct VmImage {
    location: String,
    arch: String,
    digest: Option<String>,
    #[serde(default)]
    variant: String,
}

#[derive(Debug, Deserialize)]
struct LimaInstance {
    name: String,
    status: String,
    #[serde(rename = "vmType")]
    vm_type: String,
    arch: String,
    cpus: u16,
    memory: u64,
    disk: u64,
    #[serde(rename = "limaVersion")]
    lima_version: String,
    config: LimaConfig,
}

#[derive(Debug, Deserialize)]
struct LimaConfig {
    plain: bool,
    #[serde(default)]
    mounts: Vec<Value>,
    images: Vec<VmImage>,
}

impl ManagedVm {
    pub(crate) fn new() -> Self {
        Self {
            limactl: env::var_os("RUNLAB_LIMACTL")
                .map_or_else(|| PathBuf::from("limactl"), PathBuf::from),
        }
    }

    pub(crate) fn status(&self) -> Result<VmStatus> {
        let lima_version = self.lima_version()?;
        let instance = self.instance()?;
        let mut status = status_from(instance.as_ref(), lima_version);
        if status.status == "running" && status.compatible {
            match self.installed_guest() {
                Ok((guest, runtime)) => {
                    status.readiness_problems = runtime.problems();
                    status.ready = status.readiness_problems.is_empty();
                    status.guest = Some(guest);
                    status.runtime = Some(runtime);
                }
                Err(error) => status.readiness_problems.push(format!("{error:#}")),
            }
        } else if status.status != "running" {
            status
                .readiness_problems
                .push("managed VM is not running".to_owned());
        }
        Ok(status)
    }

    pub(crate) fn create(&self, cpus: u16, memory_gib: u16, disk_gib: u16) -> Result<VmStatus> {
        ensure!((1..=64).contains(&cpus), "--cpus must be between 1 and 64");
        ensure!(
            (1..=256).contains(&memory_gib),
            "--memory-gib must be between 1 and 256"
        );
        ensure!(
            (8..=1024).contains(&disk_gib),
            "--disk-gib must be between 8 and 1024"
        );
        let lima_version = self.lima_version()?;
        if let Some(instance) = self.instance()? {
            ensure_compatible(&instance, &lima_version)?;
            return self.status();
        }

        let architecture = host_architecture()?;
        let template = template(architecture)?;
        let mut child = Command::new(&self.limactl)
            .args([
                "--tty=false",
                "create",
                "--name",
                INSTANCE,
                "--plain",
                "--vm-type",
                "vz",
                "--arch",
                architecture,
                "--cpus",
                &cpus.to_string(),
                "--memory",
                &memory_gib.to_string(),
                "--disk",
                &disk_gib.to_string(),
                "-",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to start {}", self.limactl.display()))?;
        child
            .stdin
            .take()
            .context("limactl create stdin is unavailable")?
            .write_all(template.as_bytes())
            .context("failed to provide the managed VM template")?;
        let output = child
            .wait_with_output()
            .context("failed to wait for limactl create")?;
        ensure_success(&output, "limactl create")?;
        let status = self.status()?;
        ensure!(status.compatible, "created managed VM is incompatible");
        Ok(status)
    }

    pub(crate) fn start(&self) -> Result<VmStatus> {
        let lima_version = self.lima_version()?;
        let instance = self
            .instance()?
            .context("managed VM does not exist; run `runlab vm create`")?;
        ensure_compatible(&instance, &lima_version)?;
        if !instance.status.eq_ignore_ascii_case("running") {
            self.run(["--tty=false", "start", INSTANCE, "--timeout", START_TIMEOUT])?;
        }
        let status = self.status()?;
        ensure!(
            status.status == "running",
            "managed VM did not reach running state"
        );
        Ok(status)
    }

    pub(crate) fn stop(&self) -> Result<VmStatus> {
        let lima_version = self.lima_version()?;
        let instance = self
            .instance()?
            .context("managed VM does not exist; run `runlab vm create`")?;
        ensure_compatible(&instance, &lima_version)?;
        if !instance.status.eq_ignore_ascii_case("stopped") {
            self.run(["--tty=false", "stop", INSTANCE])?;
        }
        let status = self.status()?;
        ensure!(
            status.status == "stopped",
            "managed VM did not reach stopped state"
        );
        Ok(status)
    }

    pub(crate) fn install(&self) -> Result<VmInstallResult> {
        self.ensure_running()?;
        let architecture = host_architecture()?;
        let binary = bundled_artifact("RUNLAB_GUEST_BINARY", "runlab", architecture)?;
        let runc = bundled_artifact("RUNLAB_GUEST_RUNC", "runc", architecture)?;
        let binary_identity = file_identity(&binary)?;
        let runc_identity = file_identity(&runc)?;
        let operation = Uuid::new_v4();
        let staged_binary = format!("/var/tmp/runlab-install-{operation}");
        let staged_runc = format!("/var/tmp/runc-install-{operation}");
        let staged_sysctl = format!("/var/tmp/runlab-sysctl-{operation}");

        let result = (|| {
            self.copy_checked(&binary, &binary_identity, &staged_binary)?;
            self.copy_checked(&runc, &runc_identity, &staged_runc)?;
            self.guest_success(["chmod", "0700", &staged_binary])?;
            self.guest_success(["chmod", "0700", &staged_runc])?;
            let _ = self.handshake_at(&staged_binary)?;
            let staged_runtime = self.runtime_profile(&staged_runc)?;
            let mut prerequisites = staged_runtime.problems();
            prerequisites.retain(|problem| problem != "IPv4 forwarding is disabled");
            ensure!(
                prerequisites.is_empty(),
                "managed VM does not satisfy the reference profile: {}",
                prerequisites.join("; ")
            );

            let binary_directory = format!("{GUEST_BINARY_ROOT}/{}", env!("CARGO_PKG_VERSION"));
            let binary_target = format!("{binary_directory}/runlab");
            let binary_temporary = format!("{binary_directory}/.runlab-{operation}");
            let runc_temporary = format!("/usr/local/bin/.runc-{operation}");
            self.guest_success(["sudo", "install", "-d", "-m", "0755", &binary_directory])?;
            self.guest_success(["sudo", "install", "-d", "-m", "0700", STATE_PATH])?;
            self.guest_success([
                "sudo",
                "install",
                "-m",
                "0755",
                &staged_binary,
                &binary_temporary,
            ])?;
            self.guest_success(["sudo", "mv", "-f", &binary_temporary, &binary_target])?;
            self.guest_success([
                "sudo",
                "install",
                "-m",
                "0755",
                &staged_runc,
                &runc_temporary,
            ])?;
            self.guest_success(["sudo", "mv", "-f", &runc_temporary, RUNC_PATH])?;

            let mut policy = tempfile::NamedTempFile::new()
                .context("failed to stage the managed VM network policy")?;
            policy.write_all(SYSCTL_CONTENTS)?;
            policy.flush()?;
            let policy_identity = file_identity(policy.path())?;
            self.copy_checked(policy.path(), &policy_identity, &staged_sysctl)?;
            let sysctl_temporary = format!("/etc/sysctl.d/.90-runlab-{operation}");
            self.guest_success([
                "sudo",
                "install",
                "-m",
                "0644",
                &staged_sysctl,
                &sysctl_temporary,
            ])?;
            self.guest_success(["sudo", "mv", "-f", &sysctl_temporary, SYSCTL_PATH])?;
            self.guest_success(["sudo", "sysctl", "-p", SYSCTL_PATH])?;

            let guest = self.handshake_at(&binary_target)?;
            let runtime = self.runtime_profile(RUNC_PATH)?;
            let problems = runtime.problems();
            ensure!(
                problems.is_empty(),
                "installed managed VM is not ready: {}",
                problems.join("; ")
            );
            ensure_remote_identity(
                &self.remote_file_identity(&binary_target)?,
                &binary_identity,
            )?;
            ensure_remote_identity(&self.remote_file_identity(RUNC_PATH)?, &runc_identity)?;
            Ok(VmInstallResult {
                schema_version: 1,
                instance: INSTANCE,
                binary: InstalledFile {
                    path: binary_target,
                    digest: binary_identity.digest.clone(),
                    size: binary_identity.size,
                },
                runc: InstalledFile {
                    path: RUNC_PATH.to_owned(),
                    digest: runc_identity.digest.clone(),
                    size: runc_identity.size,
                },
                guest,
                runtime,
            })
        })();

        for path in [&staged_binary, &staged_runc, &staged_sysctl] {
            let _ = self.guest_success(["rm", "-f", path]);
        }
        result
    }

    fn ensure_running(&self) -> Result<()> {
        let status = self.status()?;
        ensure!(
            status.compatible,
            "managed VM is incompatible: {}",
            status.problems.join("; ")
        );
        ensure!(
            status.status == "running",
            "managed VM is not running; run `runlab vm start`"
        );
        Ok(())
    }

    pub(super) fn ensure_ready(&self) -> Result<()> {
        let status = self.status()?;
        ensure!(
            status.ready,
            "managed VM is not ready: {}; run `runlab vm start` and `runlab vm install`",
            status.readiness_problems.join("; ")
        );
        Ok(())
    }

    fn installed_guest(&self) -> Result<(GuestHandshake, RuntimeProfile)> {
        let binary = guest_binary_path();
        Ok((
            self.handshake_at(&binary)?,
            self.runtime_profile(RUNC_PATH)?,
        ))
    }

    fn handshake_at(&self, binary: &str) -> Result<GuestHandshake> {
        let output = self.guest_output([binary, "__managed-vm-handshake"])?;
        let handshake: GuestHandshake = serde_json::from_slice(&output.stdout)
            .context("guest RunLab returned an invalid handshake")?;
        ensure!(
            handshake.schema_version == 1,
            "unsupported guest handshake schema"
        );
        ensure!(
            handshake.transport_version == TRANSPORT_VERSION,
            "guest transport version does not match the macOS CLI"
        );
        ensure!(
            handshake.runlab_version == env!("CARGO_PKG_VERSION"),
            "guest RunLab version does not match the macOS CLI"
        );
        ensure!(handshake.os == "linux", "managed VM guest is not Linux");
        ensure!(
            handshake.architecture == host_architecture()?,
            "guest architecture does not match the macOS host"
        );
        ensure!(
            handshake.capabilities == ["native-engine", "state-cli"],
            "guest RunLab capabilities do not match the macOS CLI"
        );
        Ok(handshake)
    }

    fn runtime_profile(&self, runc: &str) -> Result<RuntimeProfile> {
        let version = self.guest_output([runc, "--version"])?;
        let first_line = std::str::from_utf8(&version.stdout)
            .context("runc version output is not UTF-8")?
            .lines()
            .next()
            .context("runc version output is empty")?;
        let runc_version = first_line
            .strip_prefix("runc version ")
            .context("runc returned an unrecognized version")?
            .to_owned();
        let forwarding = self.guest_output(["/usr/bin/cat", "/proc/sys/net/ipv4/ip_forward"])?;
        let executables = [
            ("ip", ["/usr/sbin/ip", "/usr/bin/ip"]),
            ("iptables", ["/usr/sbin/iptables", "/usr/bin/iptables"]),
            ("ip6tables", ["/usr/sbin/ip6tables", "/usr/bin/ip6tables"]),
            ("nsenter", ["/usr/bin/nsenter", "/usr/sbin/nsenter"]),
        ]
        .into_iter()
        .filter(|(_, candidates)| self.guest_executable(candidates))
        .map(|(name, _)| name.to_owned())
        .collect();
        Ok(RuntimeProfile {
            runc_version,
            executables,
            cgroup_version: self
                .guest_test(["-f", "/sys/fs/cgroup/cgroup.controllers"])
                .then_some(2),
            overlayfs: self.guest_command_success([
                "/usr/bin/grep",
                "-qw",
                "overlay",
                "/proc/filesystems",
            ]),
            ipv4_forwarding: forwarding.stdout == b"1\n",
        })
    }

    fn guest_executable(&self, candidates: &[&str]) -> bool {
        for candidate in candidates {
            if self.guest_test(["-x", candidate]) {
                return true;
            }
        }
        false
    }

    pub(super) fn guest_test<const N: usize>(&self, arguments: [&str; N]) -> bool {
        let mut command = vec!["/usr/bin/test"];
        command.extend(arguments);
        self.guest_command_success(command)
    }

    fn guest_command_success<I, S>(&self, arguments: I) -> bool
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut command = Command::new(&self.limactl);
        command.args(["shell", "--tty=false", INSTANCE, "--"]);
        command.args(arguments);
        command.output().is_ok_and(|output| output.status.success())
    }

    pub(super) fn copy_checked(
        &self,
        source: &Path,
        identity: &FileIdentity,
        destination: &str,
    ) -> Result<()> {
        let remote = format!("{INSTANCE}:{destination}");
        let output = Command::new(&self.limactl)
            .args([OsStr::new("copy"), OsStr::new("--backend=scp")])
            .arg(source)
            .arg(&remote)
            .output()
            .with_context(|| format!("failed to run {} copy", self.limactl.display()))?;
        ensure_success(&output, "limactl copy")?;
        ensure_remote_identity(&self.remote_file_identity(destination)?, identity)
    }

    pub(super) fn remote_file_identity(&self, path: &str) -> Result<FileIdentity> {
        let digest = self.guest_output(["/usr/bin/sha256sum", "--", path])?;
        let digest = std::str::from_utf8(&digest.stdout)
            .context("guest sha256sum output is not UTF-8")?
            .split_whitespace()
            .next()
            .context("guest sha256sum output is empty")?;
        ensure!(
            digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "guest sha256sum returned an invalid digest"
        );
        let size = self.guest_output(["/usr/bin/stat", "-c", "%s", "--", path])?;
        let size = std::str::from_utf8(&size.stdout)
            .context("guest stat output is not UTF-8")?
            .trim()
            .parse()
            .context("guest stat returned an invalid size")?;
        Ok(FileIdentity {
            digest: format!("sha256:{digest}"),
            size,
        })
    }

    pub(super) fn guest_success<const N: usize>(&self, arguments: [&str; N]) -> Result<()> {
        self.guest_output(arguments).map(|_| ())
    }

    pub(super) fn guest_output<I, S>(&self, arguments: I) -> Result<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut command = Command::new(&self.limactl);
        command.args(["shell", "--tty=false", INSTANCE, "--"]);
        command.args(arguments);
        let output = command
            .output()
            .with_context(|| format!("failed to run {} shell", self.limactl.display()))?;
        ensure_success(&output, "managed VM command")?;
        Ok(output)
    }

    fn lima_version(&self) -> Result<String> {
        let output = self.run(["--version"])?;
        let value = std::str::from_utf8(&output.stdout)
            .context("limactl version output is not UTF-8")?
            .trim()
            .strip_prefix("limactl version ")
            .context("limactl returned an unrecognized version")?
            .to_owned();
        ensure!(
            value == LIMA_VERSION,
            "managed VM requires limactl {LIMA_VERSION}; found {value}"
        );
        Ok(value)
    }

    fn instance(&self) -> Result<Option<LimaInstance>> {
        let output = self.run(["list", "--json"])?;
        let mut found = None;
        for line in output.stdout.split(|byte| *byte == b'\n') {
            if line.is_empty() {
                continue;
            }
            let instance: LimaInstance = serde_json::from_slice(line)
                .context("limactl returned invalid instance metadata")?;
            if instance.name == INSTANCE {
                ensure!(
                    found.replace(instance).is_none(),
                    "limactl returned duplicate instances"
                );
            }
        }
        Ok(found)
    }

    fn run<const N: usize>(&self, arguments: [&str; N]) -> Result<Output> {
        let output = Command::new(&self.limactl)
            .args(arguments)
            .output()
            .with_context(|| format!("failed to run {}", self.limactl.display()))?;
        ensure_success(&output, "limactl")?;
        Ok(output)
    }
}

fn status_from(instance: Option<&LimaInstance>, lima_version: String) -> VmStatus {
    let Some(instance) = instance else {
        return VmStatus {
            schema_version: 1,
            instance: INSTANCE,
            status: "absent".to_owned(),
            compatible: false,
            problems: vec!["managed VM does not exist".to_owned()],
            lima_version,
            architecture: None,
            vm_type: None,
            cpus: None,
            memory_bytes: None,
            disk_bytes: None,
            host_mounts: None,
            image: None,
            ready: false,
            readiness_problems: Vec::new(),
            guest: None,
            runtime: None,
        };
    };
    let problems = compatibility_problems(instance, &lima_version);
    VmStatus {
        schema_version: 1,
        instance: INSTANCE,
        status: instance.status.to_ascii_lowercase(),
        compatible: problems.is_empty(),
        problems,
        lima_version,
        architecture: Some(instance.arch.clone()),
        vm_type: Some(instance.vm_type.clone()),
        cpus: Some(instance.cpus),
        memory_bytes: Some(instance.memory),
        disk_bytes: Some(instance.disk),
        host_mounts: Some(instance.config.mounts.len()),
        image: instance.config.images.first().cloned(),
        ready: false,
        readiness_problems: Vec::new(),
        guest: None,
        runtime: None,
    }
}

fn ensure_compatible(instance: &LimaInstance, lima_version: &str) -> Result<()> {
    let problems = compatibility_problems(instance, lima_version);
    if problems.is_empty() {
        return Ok(());
    }
    bail!("managed VM is incompatible: {}", problems.join("; "))
}

fn compatibility_problems(instance: &LimaInstance, lima_version: &str) -> Vec<String> {
    let mut problems = Vec::new();
    if lima_version != LIMA_VERSION || instance.lima_version != LIMA_VERSION {
        problems.push(format!("Lima must be {LIMA_VERSION}"));
    }
    if instance.vm_type != "vz" {
        problems.push("VM type must be vz".to_owned());
    }
    match host_architecture() {
        Ok(host) if instance.arch != host => {
            problems.push(format!("guest architecture must match host {host}"));
        }
        Err(error) => problems.push(error.to_string()),
        _ => {}
    }
    if !instance.config.plain {
        problems.push("Lima plain mode must be enabled".to_owned());
    }
    if !instance.config.mounts.is_empty() {
        problems.push("host filesystem mounts are not allowed".to_owned());
    }
    match (
        expected_image(&instance.arch),
        instance.config.images.as_slice(),
    ) {
        (Ok((location, digest)), [image])
            if image.location == location
                && image.digest.as_deref() == Some(digest)
                && image.arch == instance.arch
                && image.variant == "server" => {}
        _ => problems.push("VM image does not match the pinned RunLab image".to_owned()),
    }
    problems
}

fn host_architecture() -> Result<&'static str> {
    match env::consts::ARCH {
        "aarch64" => Ok("aarch64"),
        "x86_64" => Ok("x86_64"),
        architecture => bail!("managed VM does not support host architecture {architecture}"),
    }
}

fn expected_image(architecture: &str) -> Result<(&'static str, &'static str)> {
    match architecture {
        "aarch64" => Ok((ARM64_IMAGE_LOCATION, ARM64_IMAGE_DIGEST)),
        "x86_64" => Ok((AMD64_IMAGE_LOCATION, AMD64_IMAGE_DIGEST)),
        value => bail!("managed VM does not support architecture {value}"),
    }
}

fn template(architecture: &str) -> Result<String> {
    let (location, digest) = expected_image(architecture)?;
    serde_json::to_string(&serde_json::json!({
        "minimumLimaVersion": LIMA_VERSION,
        "images": [{
            "location": location,
            "arch": architecture,
            "digest": digest,
            "variant": "server",
        }]
    }))
    .context("failed to encode the managed VM template")
}

pub(super) fn guest_binary_path() -> String {
    format!("{GUEST_BINARY_ROOT}/{}/runlab", env!("CARGO_PKG_VERSION"))
}

fn bundled_artifact(variable: &str, name: &str, architecture: &str) -> Result<PathBuf> {
    let path = if let Some(path) = env::var_os(variable) {
        PathBuf::from(path)
    } else {
        let executable = env::current_exe().context("cannot locate the macOS runlab binary")?;
        executable
            .parent()
            .context("macOS runlab binary has no parent directory")?
            .join(format!("{name}-linux-{architecture}"))
    };
    ensure!(
        path.is_file(),
        "managed VM {name} artifact is unavailable at {}; reinstall RunLab or set {variable} for development",
        path.display()
    );
    Ok(path)
}

pub(super) fn file_identity(path: &Path) -> Result<FileIdentity> {
    let mut file = File::open(path)
        .with_context(|| format!("cannot open managed VM artifact {}", path.display()))?;
    ensure!(
        file.metadata()?.is_file(),
        "managed VM artifact is not a regular file: {}",
        path.display()
    );
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 8 * 1024];
    let mut size = 0_u64;
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("cannot hash managed VM artifact {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        size = size
            .checked_add(u64::try_from(read).context("managed VM artifact is too large")?)
            .context("managed VM artifact size overflow")?;
    }
    let digest = hasher.finalize();
    let encoded = digest.iter().fold(String::new(), |mut value, byte| {
        write!(value, "{byte:02x}").expect("writing to a String cannot fail");
        value
    });
    Ok(FileIdentity {
        digest: format!("sha256:{encoded}"),
        size,
    })
}

pub(super) fn ensure_remote_identity(remote: &FileIdentity, local: &FileIdentity) -> Result<()> {
    ensure!(
        remote.digest == local.digest && remote.size == local.size,
        "managed VM transfer failed digest or size verification"
    );
    Ok(())
}

pub(super) fn ensure_success(output: &Output, operation: &str) -> Result<()> {
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    bail!(
        "{operation} failed with {}: {}",
        output.status,
        stderr.trim()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_template_has_no_implicit_lima_features() {
        let value: Value =
            serde_json::from_str(&template("aarch64").expect("template")).expect("template JSON");
        assert_eq!(value["minimumLimaVersion"], LIMA_VERSION);
        assert_eq!(value["images"][0]["digest"], ARM64_IMAGE_DIGEST);
        assert!(value.get("mounts").is_none());
        assert!(value.get("containerd").is_none());
    }

    #[test]
    fn status_reports_incompatible_host_mounts() {
        let instance: LimaInstance = serde_json::from_value(serde_json::json!({
            "name": "runlab",
            "status": "Running",
            "vmType": "vz",
            "arch": env::consts::ARCH,
            "cpus": 4,
            "memory": 4_294_967_296_u64,
            "disk": 21_474_836_480_u64,
            "limaVersion": LIMA_VERSION,
            "config": {
                "plain": true,
                "mounts": [{"location": "/Users/example"}],
                "images": [{
                    "location": expected_image(env::consts::ARCH).expect("image").0,
                    "arch": env::consts::ARCH,
                    "digest": expected_image(env::consts::ARCH).expect("image").1,
                    "variant": "server"
                }]
            }
        }))
        .expect("instance");

        let status = status_from(Some(&instance), LIMA_VERSION.to_owned());

        assert!(!status.compatible);
        assert_eq!(status.status, "running");
        assert!(status.problems.iter().any(|value| value.contains("mounts")));
    }
}
