//! The explicit Docker compatibility adapter.
//!
//! Docker is reached only through its CLI, behind this boundary: no module
//! outside it knows that a `docker` process exists. It is opt-in — `--backend
//! docker` — and the native backend is the reference path.
//!
//! The adapter can only realize Runtime configs Docker can represent faithfully.
//! Where it cannot, it refuses rather than approximating.

mod image;

pub(crate) use image::DockerImageAdapter;

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;

use crate::core::{
    Architecture, BackendDetails, BackendFacts, NetworkControl, Platform, RunControls, RunId,
};
use crate::runtime::RuntimeConfig;
use crate::signal::TerminationFlag;
use crate::subprocess::bounded_output;

const MAX_COMMAND_OUTPUT_BYTES: u64 = 1024 * 1024;
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(25);
/// How long a Docker control-plane call may take: inspect, create, stop, remove.
const CONTROL_TIMEOUT: Duration = Duration::from_mins(2);
/// How long a Docker call that moves an Image's bytes may take: save, load, commit.
///
/// A Run's own bound comes from its `RunControls`; this bounds the client calls
/// around it, so a wedged Docker daemon cannot hold a `RunLab` process forever.
const TRANSFER_TIMEOUT: Duration = Duration::from_mins(30);
const REQUIRED_NAMESPACES: [&str; 6] = ["pid", "network", "ipc", "uts", "mount", "cgroup"];

#[derive(Debug)]
pub struct DockerPreflight {
    pub facts: BackendFacts,
    pub runtime: DockerRuntime,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DockerRuntime {
    #[serde(rename = "ociVersion")]
    _oci_version: String,
    pub root: DockerRuntimeRoot,
    pub process: DockerRuntimeProcess,
    pub hostname: String,
    pub linux: DockerRuntimeLinux,
    #[serde(default)]
    pub annotations: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DockerRuntimeRoot {
    #[serde(rename = "path")]
    _path: String,
    pub readonly: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DockerRuntimeProcess {
    pub terminal: bool,
    pub user: DockerRuntimeUser,
    pub args: Vec<String>,
    pub env: Vec<String>,
    pub cwd: String,
    pub no_new_privileges: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DockerRuntimeUser {
    pub uid: u32,
    pub gid: u32,
    #[serde(default)]
    pub additional_gids: Vec<u32>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DockerRuntimeLinux {
    pub namespaces: Vec<DockerRuntimeNamespace>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DockerRuntimeNamespace {
    #[serde(rename = "type")]
    pub kind: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    Timeout,
    StdoutLimit,
    StderrLimit,
    Cancelled,
}

#[derive(Debug)]
pub struct AttachedResult {
    pub client_status: Option<ExitStatus>,
    pub stop_reason: Option<StopReason>,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    pub operation_errors: Vec<String>,
}

#[derive(Debug)]
pub struct ContainerState {
    pub started: bool,
    pub exit_code: i32,
    pub oom_killed: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct DockerBackend {
    executable: PathBuf,
}

impl DockerBackend {
    pub fn discover() -> Result<Self> {
        let executable = find_executable("docker").context("Docker executable is not available")?;
        Ok(Self { executable })
    }

    pub fn preflight_run(
        &self,
        runtime: &RuntimeConfig,
        image: &Value,
        network: NetworkControl,
    ) -> Result<DockerPreflight> {
        let runtime = docker_profile(runtime)?;
        validate_image_defaults(image, &runtime)?;
        let facts = self.preflight(network)?;
        Ok(DockerPreflight { facts, runtime })
    }

    pub fn preflight(&self, network: NetworkControl) -> Result<BackendFacts> {
        if network == NetworkControl::Egress {
            bail!(
                "the Docker adapter cannot faithfully provision the outbound-only network=egress control"
            );
        }
        let context = self
            .run_text(["context", "show"], CONTROL_TIMEOUT)?
            .trim()
            .to_owned();
        let endpoint = self
            .run_text(
                [
                    "context",
                    "inspect",
                    "--format",
                    "{{(index .Endpoints \"docker\").Host}}",
                    &context,
                ],
                CONTROL_TIMEOUT,
            )?
            .trim()
            .to_owned();
        let endpoint_kind = endpoint_kind(&endpoint)?;
        let platform = self.native_platform()?;
        let version = self
            .run_text(
                ["version", "--format", "{{.Server.Version}}"],
                CONTROL_TIMEOUT,
            )?
            .trim()
            .to_owned();
        let engine_id = self
            .run_text(["info", "--format", "{{.ID}}"], CONTROL_TIMEOUT)?
            .trim()
            .to_owned();
        if engine_id.is_empty() {
            bail!("Docker backend returned an empty engine identity");
        }
        Ok(BackendFacts {
            name: "docker".to_owned(),
            version,
            platform,
            network,
            run_network: None,
            details: BackendDetails::Docker {
                context,
                endpoint_kind: endpoint_kind.to_owned(),
                engine_id,
            },
        })
    }

    pub fn native_platform(&self) -> Result<Platform> {
        let raw = self
            .run_text(
                ["version", "--format", "{{.Server.Os}}/{{.Server.Arch}}"],
                CONTROL_TIMEOUT,
            )?
            .trim()
            .to_owned();
        let Some((operating_system, architecture)) = raw.split_once('/') else {
            bail!("Docker backend returned an invalid platform: {raw}");
        };
        if operating_system != "linux" {
            bail!("Docker backend is not a Linux backend: {raw}");
        }
        Ok(Platform::linux(architecture.parse::<Architecture>()?))
    }

    pub fn save_image(&self, image: &str, destination: &Path) -> Result<()> {
        self.run(
            ["image", "save", "--output", path_text(destination)?, image],
            TRANSFER_TIMEOUT,
        )?;
        Ok(())
    }

    pub fn load_image(&self, archive: &Path) -> Result<()> {
        self.run(
            ["image", "load", "--input", path_text(archive)?],
            TRANSFER_TIMEOUT,
        )?;
        Ok(())
    }

    pub fn image_exists(&self, reference: &str) -> Result<bool> {
        let output = self.invoke(
            ["image", "inspect", "--format", "{{.Id}}", reference],
            CONTROL_TIMEOUT,
        )?;
        if output.status.success() {
            return Ok(true);
        }
        if output.stderr.contains("No such image") || output.stderr.contains("No such object") {
            return Ok(false);
        }
        Err(command_failure(&output))
    }

    pub fn remove_image_tag(&self, tag: &str) -> Result<()> {
        let output = self.invoke(["image", "rm", tag], CONTROL_TIMEOUT)?;
        if output.status.success() || output.stderr.contains("No such image") {
            return Ok(());
        }
        Err(command_failure(&output))
    }

    pub fn image_diff_ids(&self, image: &str) -> Result<Vec<String>> {
        let raw = self.run_text(
            [
                "image",
                "inspect",
                "--format",
                "{{json .RootFS.Layers}}",
                image,
            ],
            CONTROL_TIMEOUT,
        )?;
        serde_json::from_str(&raw).context("Docker returned an invalid image Layer list")
    }

    pub fn create_checkout(&self, image: &str, parent_manifest: &str) -> Result<String> {
        let suffix = RunId::new().to_string();
        let name = format!("runlab-checkout-{}", &suffix[4..]);
        let label = format!("runlab.parent-manifest={parent_manifest}");
        let output = self.run(
            [
                "container",
                "create",
                "--name",
                &name,
                "--hostname",
                "runlab-checkout",
                "--label",
                &label,
                "--entrypoint",
                "/bin/sh",
                image,
                "-c",
                "while :; do sleep 3600; done",
            ],
            CONTROL_TIMEOUT,
        )?;
        let container = output.stdout.trim().to_owned();
        self.run(["container", "start", &container], CONTROL_TIMEOUT)?;
        Ok(container)
    }

    pub fn checkout_parent(&self, container: &str) -> Result<String> {
        let value = self
            .run_text(
                [
                    "container",
                    "inspect",
                    "--format",
                    "{{index .Config.Labels \"runlab.parent-manifest\"}}",
                    container,
                ],
                CONTROL_TIMEOUT,
            )?
            .trim()
            .to_owned();
        if !value.starts_with("sha256:") {
            bail!("container is not a RunLab checkout: {container}");
        }
        Ok(value)
    }

    pub fn commit(&self, container: &str, tag: &str) -> Result<()> {
        self.run(["container", "commit", container, tag], TRANSFER_TIMEOUT)?;
        Ok(())
    }

    pub fn create_run_container(
        &self,
        image: &str,
        run_id: RunId,
        runtime: &DockerRuntime,
        network: NetworkControl,
    ) -> Result<String> {
        let run_id = run_id.to_string();
        let user = format!("{}:{}", runtime.process.user.uid, runtime.process.user.gid);
        let label = format!("runlab.run-id={run_id}");
        let network = match network {
            NetworkControl::None => "none",
            NetworkControl::Egress => {
                bail!(
                    "the Docker adapter cannot faithfully provision the outbound-only network=egress control"
                )
            }
        };
        let mut arguments = vec![
            "container".to_owned(),
            "create".to_owned(),
            "--name".to_owned(),
            run_id,
            "--hostname".to_owned(),
            runtime.hostname.clone(),
            "--label".to_owned(),
            label,
            "--network".to_owned(),
            network.to_owned(),
            "--cgroupns".to_owned(),
            "private".to_owned(),
            "--ipc".to_owned(),
            "private".to_owned(),
            "--workdir".to_owned(),
            runtime.process.cwd.clone(),
            "--user".to_owned(),
            user,
            "--entrypoint".to_owned(),
            runtime.process.args[0].clone(),
            "--interactive".to_owned(),
            "--cap-drop".to_owned(),
            "ALL".to_owned(),
            "--security-opt".to_owned(),
            "seccomp=unconfined".to_owned(),
        ];
        if runtime.root.readonly {
            arguments.push("--read-only".to_owned());
        }
        if runtime.process.no_new_privileges {
            arguments.extend(["--security-opt".to_owned(), "no-new-privileges".to_owned()]);
        }
        for group in &runtime.process.user.additional_gids {
            arguments.extend(["--group-add".to_owned(), group.to_string()]);
        }
        for environment in &runtime.process.env {
            arguments.extend(["--env".to_owned(), environment.clone()]);
        }
        if let Some(stop_signal) = runtime
            .annotations
            .get("org.opencontainers.image.stopSignal")
        {
            arguments.extend(["--stop-signal".to_owned(), stop_signal.clone()]);
        }
        arguments.push(image.to_owned());
        arguments.extend(runtime.process.args.iter().skip(1).cloned());
        let output = self.run_owned(&arguments, CONTROL_TIMEOUT)?;
        Ok(output.stdout.trim().to_owned())
    }

    pub fn start_attached(
        &self,
        container: &str,
        stdin_bytes: Vec<u8>,
        stdout_path: &Path,
        stderr_path: &Path,
        controls: &RunControls,
    ) -> Result<AttachedResult> {
        let stdout = private_output(stdout_path)?;
        let stderr = private_output(stderr_path)?;
        let interrupted = TerminationFlag::register()?;
        let started_at = Utc::now();
        let mut child = Command::new(&self.executable)
            .args(["container", "start", "--attach", "--interactive", container])
            .stdin(Stdio::piped())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .spawn()
            .context("could not start attached Docker client")?;
        let mut operation_errors = Vec::new();
        let writer = if let Some(stdin) = child.stdin.take() {
            Some(thread::spawn(move || write_stdin(stdin, &stdin_bytes)))
        } else {
            operation_errors.push("Docker client stdin is unavailable".to_owned());
            None
        };
        let (client_status, stop_reason) = match self.monitor_attached(
            &mut child,
            container,
            stdout_path,
            stderr_path,
            controls,
            interrupted.flag(),
        ) {
            Ok((status, reason)) => (Some(status), reason),
            Err(error) => {
                operation_errors.push(format!("{error:#}"));
                if let Err(stop_error) = self.stop_container(container) {
                    operation_errors.push(format!(
                        "failed to stop container after attach error: {stop_error:#}"
                    ));
                }
                match child.wait() {
                    Ok(status) => (Some(status), None),
                    Err(wait_error) => {
                        operation_errors.push(format!(
                            "failed to wait for Docker client after attach error: {wait_error}"
                        ));
                        (None, None)
                    }
                }
            }
        };
        if let Some(writer) = writer {
            match writer.join() {
                Ok(Ok(())) => {}
                Ok(Err(error)) => operation_errors.push(format!("{error:#}")),
                Err(_) => operation_errors.push("Docker stdin writer panicked".to_owned()),
            }
        }
        Ok(AttachedResult {
            client_status,
            stop_reason,
            started_at,
            ended_at: Utc::now(),
            operation_errors,
        })
    }

    fn monitor_attached(
        &self,
        child: &mut std::process::Child,
        container: &str,
        stdout_path: &Path,
        stderr_path: &Path,
        controls: &RunControls,
        interrupted: &AtomicBool,
    ) -> Result<(ExitStatus, Option<StopReason>)> {
        let timeout_started = Instant::now();
        let timeout = Duration::from_secs(controls.timeout_seconds);
        loop {
            if let Some(status) = child.try_wait().context("failed to poll Docker client")? {
                let reason = completed_stream_limit(stdout_path, stderr_path, controls)?;
                return Ok((status, reason));
            }
            let reason = if interrupted.load(Ordering::Relaxed) {
                Some(StopReason::Cancelled)
            } else if timeout_started.elapsed() >= timeout {
                Some(StopReason::Timeout)
            } else {
                running_stream_limit(stdout_path, stderr_path, controls)?
            };
            if let Some(reason) = reason {
                self.stop_container(container)?;
                let status = child
                    .wait()
                    .context("failed to wait for stopped Docker client")?;
                return Ok((status, Some(reason)));
            }
            thread::sleep(PROCESS_POLL_INTERVAL);
        }
    }

    pub fn inspect_container_state(&self, container: &str) -> Result<ContainerState> {
        #[derive(Deserialize)]
        #[serde(rename_all = "PascalCase")]
        struct State {
            started_at: String,
            exit_code: i32,
            #[serde(rename = "OOMKilled")]
            oom_killed: bool,
            error: String,
        }
        let raw = self.run_text(
            [
                "container",
                "inspect",
                "--format",
                "{{json .State}}",
                container,
            ],
            CONTROL_TIMEOUT,
        )?;
        let state: State =
            serde_json::from_str(&raw).context("Docker returned an invalid container state")?;
        Ok(ContainerState {
            started: !state.started_at.starts_with("0001-"),
            exit_code: state.exit_code,
            oom_killed: state.oom_killed,
            error: (!state.error.is_empty()).then_some(state.error),
        })
    }

    pub fn remove_container(&self, container: &str) -> Result<()> {
        let output = self.invoke(["container", "rm", "--force", container], CONTROL_TIMEOUT)?;
        if output.status.success() || output.stderr.contains("No such container") {
            return Ok(());
        }
        Err(command_failure(&output))
    }

    fn stop_container(&self, container: &str) -> Result<()> {
        self.run(
            ["container", "stop", "--time", "2", container],
            CONTROL_TIMEOUT,
        )?;
        Ok(())
    }

    fn run<const N: usize>(
        &self,
        arguments: [&str; N],
        timeout: Duration,
    ) -> Result<CommandOutput> {
        let output = self.invoke(arguments, timeout)?;
        if output.status.success() {
            Ok(output)
        } else {
            Err(command_failure(&output))
        }
    }

    fn run_owned(&self, arguments: &[String], timeout: Duration) -> Result<CommandOutput> {
        let output = self.invoke(arguments.iter().map(String::as_str), timeout)?;
        if output.status.success() {
            Ok(output)
        } else {
            Err(command_failure(&output))
        }
    }

    fn run_text<const N: usize>(&self, arguments: [&str; N], timeout: Duration) -> Result<String> {
        Ok(self.run(arguments, timeout)?.stdout)
    }

    fn invoke<'a>(
        &self,
        arguments: impl IntoIterator<Item = &'a str>,
        timeout: Duration,
    ) -> Result<CommandOutput> {
        let mut command = Command::new(&self.executable);
        command.args(arguments);
        let output = bounded_output(
            &mut command,
            None,
            timeout,
            usize::try_from(MAX_COMMAND_OUTPUT_BYTES)
                .context("Docker output limit is too large")?,
            "Docker",
        )?;
        Ok(CommandOutput {
            status: output.status,
            stdout: command_text(output.stdout, "stdout")?,
            stderr: command_text(output.stderr, "stderr")?,
        })
    }
}

fn docker_profile(config: &RuntimeConfig) -> Result<DockerRuntime> {
    if config
        .value()
        .get("mounts")
        .is_some_and(|mounts| !mounts.is_null())
    {
        bail!("the Docker adapter does not support OCI Runtime mounts");
    }
    let runtime: DockerRuntime =
        serde_json::from_value(config.value().clone()).map_err(|error| {
            anyhow::anyhow!(
                "the Docker adapter cannot faithfully realize this OCI Runtime config: {error}"
            )
        })?;
    if runtime.process.terminal {
        bail!("the Docker adapter does not support OCI process.terminal=true");
    }
    let namespaces = runtime
        .linux
        .namespaces
        .iter()
        .map(|namespace| namespace.kind.as_str())
        .collect::<BTreeSet<_>>();
    let expected = BTreeSet::from(REQUIRED_NAMESPACES);
    if namespaces != expected {
        bail!(
            "the Docker adapter requires exactly private pid, network, ipc, uts, mount, and cgroup OCI namespaces"
        );
    }
    Ok(runtime)
}

fn validate_image_defaults(image: &Value, runtime: &DockerRuntime) -> Result<()> {
    let image_config = match image.get("config") {
        None | Some(Value::Null) => None,
        Some(Value::Object(config)) => Some(config),
        Some(_) => bail!("OCI Image config.config must be an object"),
    };
    let Some(image_config) = image_config else {
        return Ok(());
    };
    if image_config
        .get("Volumes")
        .and_then(Value::as_object)
        .is_some_and(|volumes| !volumes.is_empty())
    {
        bail!("the Docker adapter cannot faithfully commit OCI Images declaring Config.Volumes");
    }
    let runtime_names = runtime
        .process
        .env
        .iter()
        .filter_map(|item| item.split_once('=').map(|(name, _)| name))
        .collect::<BTreeSet<_>>();
    let mut missing = image_config
        .get("Env")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter_map(|item| item.split_once('=').map(|(name, _)| name))
        .filter(|name| !runtime_names.contains(name))
        .collect::<Vec<_>>();
    missing.sort_unstable();
    missing.dedup();
    if !missing.is_empty() {
        bail!(
            "the Docker adapter cannot remove inherited OCI Image environment variables; runtime config must define: {}",
            missing.join(", ")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docker_profile_rejects_unsupported_standard_fields() {
        let runtime = RuntimeConfig::load(
            br#"{
                "ociVersion":"1.2.0",
                "root":{"path":"rootfs"},
                "process":{"args":["/bin/true"],"cwd":"/","user":{"uid":0,"gid":0}},
                "linux":{"namespaces":[]},
                "hooks":{}
            }"#,
        )
        .expect("valid OCI Runtime config");
        let error = docker_profile(&runtime).expect_err("unsupported Docker field");
        assert!(error.to_string().contains("cannot faithfully realize"));
    }

    #[test]
    fn docker_profile_explicitly_rejects_read_only_file_mounts() {
        let runtime = RuntimeConfig::load(
            br#"{
                "ociVersion":"1.2.0",
                "root":{"path":"rootfs"},
                "process":{"args":["/bin/true"],"cwd":"/","user":{"uid":0,"gid":0}},
                "mounts":[{
                    "destination":"/run/credential",
                    "type":"bind",
                    "source":"/var/runlab-input/credential",
                    "options":["bind","ro","nosuid","nodev","noexec"]
                }],
                "linux":{"namespaces":[]}
            }"#,
        )
        .expect("valid OCI Runtime config");
        let error = docker_profile(&runtime).expect_err("Docker mount rejection");
        assert_eq!(
            error.to_string(),
            "the Docker adapter does not support OCI Runtime mounts"
        );
    }
}

#[derive(Debug)]
struct CommandOutput {
    status: ExitStatus,
    stdout: String,
    stderr: String,
}

fn command_failure(output: &CommandOutput) -> anyhow::Error {
    let detail = if output.stderr.trim().is_empty() {
        output.stdout.trim()
    } else {
        output.stderr.trim()
    };
    anyhow::anyhow!(
        "Docker operation failed (status {}): {}",
        output.status,
        if detail.is_empty() {
            "unknown error"
        } else {
            detail
        }
    )
}

fn command_text(bytes: Vec<u8>, stream: &str) -> Result<String> {
    String::from_utf8(bytes).with_context(|| format!("Docker {stream} is not valid UTF-8"))
}

fn endpoint_kind(endpoint: &str) -> Result<&'static str> {
    if endpoint.starts_with("unix://") {
        Ok("unix_socket")
    } else if endpoint.starts_with("npipe://") {
        Ok("named_pipe")
    } else {
        bail!("remote Docker endpoint is not supported by single-machine RunLab: {endpoint}");
    }
}

fn find_executable(name: &str) -> Result<PathBuf> {
    let paths = env::var_os("PATH").context("PATH is not set")?;
    for directory in env::split_paths(&paths) {
        let candidate = directory.join(name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    bail!("executable is not on PATH: {name}")
}

fn path_text(path: &Path) -> Result<&str> {
    path.to_str()
        .with_context(|| format!("path is not valid UTF-8: {}", path.display()))
}

fn private_output(path: &Path) -> Result<File> {
    let mut options = fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options
        .open(path)
        .with_context(|| format!("failed to create stream capture {}", path.display()))
}

fn write_stdin(mut stdin: impl Write, bytes: &[u8]) -> Result<()> {
    if let Err(error) = stdin.write_all(bytes) {
        if error.kind() == std::io::ErrorKind::BrokenPipe {
            return Ok(());
        }
        return Err(error).context("failed to write Docker stdin");
    }
    match stdin.flush() {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        Err(error) => Err(error).context("failed to flush Docker stdin"),
    }
}

fn completed_stream_limit(
    stdout_path: &Path,
    stderr_path: &Path,
    controls: &RunControls,
) -> Result<Option<StopReason>> {
    if file_size(stdout_path)? > controls.stdout_limit_bytes {
        Ok(Some(StopReason::StdoutLimit))
    } else if file_size(stderr_path)? > controls.stderr_limit_bytes {
        Ok(Some(StopReason::StderrLimit))
    } else {
        Ok(None)
    }
}

fn running_stream_limit(
    stdout_path: &Path,
    stderr_path: &Path,
    controls: &RunControls,
) -> Result<Option<StopReason>> {
    if file_size(stdout_path)? >= controls.stdout_limit_bytes {
        Ok(Some(StopReason::StdoutLimit))
    } else if file_size(stderr_path)? >= controls.stderr_limit_bytes {
        Ok(Some(StopReason::StderrLimit))
    } else {
        Ok(None)
    }
}

fn file_size(path: &Path) -> Result<u64> {
    fs::metadata(path)
        .with_context(|| format!("failed to inspect stream capture {}", path.display()))
        .map(|metadata| metadata.len())
}
