use std::env;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const INSTANCE: &str = "runlab";
const LIMA_VERSION: &str = "2.2.0";
const START_TIMEOUT: &str = "5m";

const ARM64_IMAGE_LOCATION: &str = "https://cloud-images.ubuntu.com/releases/noble/release-20260705/ubuntu-24.04-server-cloudimg-arm64.img";
const ARM64_IMAGE_DIGEST: &str =
    "sha256:7df0201546f75b8bcc1044594c806c35749421ad3c9bc1be2a3ab806cfae39cc";
const AMD64_IMAGE_LOCATION: &str = "https://cloud-images.ubuntu.com/releases/noble/release-20260705/ubuntu-24.04-server-cloudimg-amd64.img";
const AMD64_IMAGE_DIGEST: &str =
    "sha256:ffe6203da54deeb6db5d2a98a83f9ec8e55f149d3f7ba622e1abe5fa966ee3d6";

pub(crate) struct ManagedVm {
    limactl: PathBuf,
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
        Ok(status_from(instance.as_ref(), lima_version))
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
            return Ok(status_from(Some(&instance), lima_version));
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

fn ensure_success(output: &Output, operation: &str) -> Result<()> {
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
