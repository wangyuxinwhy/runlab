use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
#[cfg(test)]
use tempfile::TempDir;
use tempfile::tempfile;
#[cfg(test)]
use uuid::Uuid;

use crate::bundle::OciBundle;
use crate::core::{Digest, MAX_CAPTURED_STREAM_BYTES, NativeRuntimeInvocation};
use crate::integrity::{digest_reader, ensure_private_directory};
use crate::native::cgroup::PreparedNativeCgroup;
use crate::native::network::NativeNetworkBinding;
use crate::native::read_only_file::{VerifiedSourceFile, verify_all_sources};
use crate::runtime::{RootlessMapping, RuntimeConfig};

const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(10);
const CANCELLATION_GRACE: Duration = Duration::from_secs(2);
const MAX_HELPER_OUTPUT_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuncStopReason {
    Cancelled,
    LifecycleStop,
    DeadlineExceeded,
    StdoutLimitExceeded,
    StderrLimitExceeded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RuncCaptureLimits {
    stdout_bytes: u64,
    stderr_bytes: u64,
}

impl RuncCaptureLimits {
    pub(crate) fn new(stdout_bytes: u64, stderr_bytes: u64) -> Result<Self> {
        if stdout_bytes > MAX_CAPTURED_STREAM_BYTES || stderr_bytes > MAX_CAPTURED_STREAM_BYTES {
            bail!("runc capture limits cannot exceed {MAX_CAPTURED_STREAM_BYTES} bytes per stream");
        }
        Ok(Self {
            stdout_bytes,
            stderr_bytes,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RuncStateObservation {
    pub status: String,
    pub pid: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RuncStreamCapture {
    pub bytes: Vec<u8>,
    pub observed_bytes: u64,
    pub partial: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuncOperationErrorKind {
    StateObservation,
    OomObservation,
    StdoutCapture,
    StderrCapture,
    Cleanup,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RuncOperationError {
    pub kind: RuncOperationErrorKind,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RuncRecoveryHandle {
    pub runtime_root: Option<PathBuf>,
    pub cgroup_checkpoint: Option<PathBuf>,
    pub id: String,
}

#[derive(Debug)]
pub(crate) struct RuncRunResult {
    pub init_pid: Option<u32>,
    pub foreground_status: ExitStatus,
    pub stdout: RuncStreamCapture,
    pub stderr: RuncStreamCapture,
    pub stop_reason: Option<RuncStopReason>,
    pub state_before_delete: Option<RuncStateObservation>,
    pub oom_killed: Option<bool>,
    pub operation_errors: Vec<RuncOperationError>,
    pub recovery: Option<RuncRecoveryHandle>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct RuncExecution<'a> {
    pub stdin: &'a [u8],
    pub timeout: Duration,
    pub capture_limits: RuncCaptureLimits,
    pub cancelled: &'a AtomicBool,
    pub lifecycle_stop: &'a AtomicBool,
    pub process_terminal_observed: &'a AtomicBool,
    pub read_only_files: &'a [VerifiedSourceFile],
}

#[derive(Debug)]
pub(crate) struct PreparedRuncRun<'a> {
    runner: &'a RuncRunner,
    lifecycle: Option<RuncLifecycle<'a>>,
    cgroup: Option<PreparedNativeCgroup>,
}

#[derive(Debug)]
pub(crate) struct RuncRunFailure {
    pub init_pid: Option<u32>,
    error: anyhow::Error,
}

impl std::fmt::Display for RuncRunFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{:#}", self.error)
    }
}

impl std::error::Error for RuncRunFailure {}

impl RuncRunFailure {
    fn before_start(error: anyhow::Error) -> Self {
        Self {
            init_pid: None,
            error,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct RuncRunner {
    executable: PathBuf,
    helper_timeout: Duration,
    identity: RuncIdentity,
    invocation: ConfiguredInvocation,
}

#[derive(Debug, Clone)]
enum ConfiguredInvocation {
    Direct,
    Apparmor {
        executable: PathBuf,
        profile: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RuncIdentity {
    pub version: String,
    pub commit: String,
    pub runtime_spec: String,
    pub digest: Digest,
    pub size: u64,
}

impl RuncRunner {
    pub(crate) fn discover(helper_timeout: Duration) -> Result<Self> {
        let executable = find_executable("runc").context("runc executable is not available")?;
        Self::probe(executable, helper_timeout)
    }

    pub(crate) fn probe(executable: impl AsRef<Path>, helper_timeout: Duration) -> Result<Self> {
        if helper_timeout.is_zero() {
            bail!("runc helper timeout must be greater than zero");
        }
        let executable = executable.as_ref().to_path_buf();
        if !executable.is_absolute() {
            bail!(
                "runc executable path must be absolute: {}",
                executable.display()
            );
        }
        let executable = fs::canonicalize(&executable).with_context(|| {
            format!("failed to resolve runc executable {}", executable.display())
        })?;
        let file = File::open(&executable)
            .with_context(|| format!("failed to open runc executable {}", executable.display()))?;
        if !file
            .metadata()
            .with_context(|| format!("failed to inspect runc executable {}", executable.display()))?
            .is_file()
        {
            bail!(
                "runc executable is not a regular file: {}",
                executable.display()
            );
        }
        let (digest, size) = digest_reader(file).with_context(|| {
            format!("failed to digest runc executable {}", executable.display())
        })?;
        let mut runner = Self {
            executable,
            helper_timeout,
            identity: RuncIdentity {
                version: String::new(),
                commit: String::new(),
                runtime_spec: String::new(),
                digest: digest.clone(),
                size,
            },
            invocation: ConfiguredInvocation::Direct,
        };
        let version = runner.invoke(None, &[OsString::from("--version")], helper_timeout)?;
        if !version.status.success() {
            return Err(helper_failure("runc --version", &version));
        }
        runner.identity = decode_identity(&version.stdout, digest, size)?;
        Ok(runner)
    }

    pub(crate) fn identity(&self) -> &RuncIdentity {
        &self.identity
    }

    fn command(&self) -> Command {
        match &self.invocation {
            ConfiguredInvocation::Direct => Command::new(&self.executable),
            ConfiguredInvocation::Apparmor {
                executable,
                profile,
            } => {
                let mut command = Command::new(executable);
                command
                    .arg("-p")
                    .arg(profile)
                    .arg("--")
                    .arg(&self.executable);
                command
            }
        }
    }

    pub(crate) fn invocation_fact(&self) -> NativeRuntimeInvocation {
        match &self.invocation {
            ConfiguredInvocation::Direct => NativeRuntimeInvocation::Direct,
            ConfiguredInvocation::Apparmor { profile, .. } => {
                NativeRuntimeInvocation::ApparmorProfile {
                    profile: profile.clone(),
                }
            }
        }
    }

    pub(crate) fn configured_for_recovery(
        &self,
        invocation: &NativeRuntimeInvocation,
    ) -> Result<Self> {
        let invocation = match invocation {
            NativeRuntimeInvocation::Direct => ConfiguredInvocation::Direct,
            NativeRuntimeInvocation::ApparmorProfile { profile } if profile == "runc" => {
                ConfiguredInvocation::Apparmor {
                    executable: find_executable("aa-exec")
                        .context("recorded AppArmor runc invocation is unavailable")?,
                    profile: profile.clone(),
                }
            }
            NativeRuntimeInvocation::ApparmorProfile { profile } => {
                bail!("unsupported recorded AppArmor profile for runc: {profile}")
            }
        };
        Ok(Self {
            invocation,
            ..self.clone()
        })
    }

    pub(crate) fn probe_rootless_invocation(
        &self,
        state_root: &Path,
        mapping: RootlessMapping,
    ) -> Result<Self> {
        let direct = self.clone();
        match direct.probe_rootless_candidate(state_root, mapping) {
            Ok(()) => Ok(direct),
            Err(direct_error) => {
                let aa_exec = find_executable("aa-exec").with_context(|| {
                    format!(
                        "direct rootless runc probe failed: {direct_error:#}; AppArmor aa-exec is unavailable"
                    )
                })?;
                let wrapped = Self {
                    invocation: ConfiguredInvocation::Apparmor {
                        executable: aa_exec,
                        profile: "runc".to_owned(),
                    },
                    ..self.clone()
                };
                wrapped
                    .probe_rootless_candidate(state_root, mapping)
                    .with_context(|| {
                        format!(
                            "direct rootless runc probe failed: {direct_error:#}; AppArmor-profiled probe also failed"
                        )
                    })?;
                Ok(wrapped)
            }
        }
    }

    fn probe_rootless_candidate(&self, state_root: &Path, mapping: RootlessMapping) -> Result<()> {
        let probe = tempfile::Builder::new()
            .prefix("runlab-rootless-runc-probe-")
            .tempdir_in(state_root)
            .context("failed to create rootless runc probe workspace")?;
        let runtime = RuntimeConfig::load(&serde_json::to_vec(&serde_json::json!({
            "ociVersion": "1.2.0",
            "root": {"path": "rootfs", "readonly": false},
            "process": {
                "terminal": false,
                "user": {"uid": 0, "gid": 0},
                "args": ["/runlab-rootless-probe-missing"],
                "env": [],
                "cwd": "/",
                "noNewPrivileges": true
            },
            "mounts": [{
                "destination": "/proc",
                "type": "proc",
                "source": "proc",
                "options": ["nosuid", "noexec", "nodev"]
            }],
            "linux": {
                "namespaces": [
                    {"type": "user"},
                    {"type": "pid"},
                    {"type": "network"},
                    {"type": "ipc"},
                    {"type": "uts"},
                    {"type": "mount"},
                    {"type": "cgroup"}
                ],
                "uidMappings": [{
                    "containerID": 0,
                    "hostID": mapping.host_uid,
                    "size": 1
                }],
                "gidMappings": [{
                    "containerID": 0,
                    "hostID": mapping.host_gid,
                    "size": 1
                }]
            }
        }))?)?;
        let bundle = OciBundle::create_at(&probe.path().join("bundle"), &runtime)?;
        fs::create_dir(bundle.rootfs()?.join("proc"))?;
        let runtime_root = probe.path().join("runtime");
        ensure_private_directory(&runtime_root)?;
        let output = self.invoke(
            Some(&runtime_root),
            &[
                OsString::from("run"),
                OsString::from("--bundle"),
                bundle.path().as_os_str().to_owned(),
                OsString::from("runlab-rootless-probe"),
            ],
            self.helper_timeout,
        )?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        let reached_init = [
            "error during container init",
            "/runlab-rootless-probe-missing",
            "no such file or directory",
        ]
        .iter()
        .all(|marker| stderr.contains(marker));
        if output.status.success() || !reached_init {
            bail!(
                "disposable rootless runc probe did not reach the expected container init boundary: status={}, stderr={}",
                output.status,
                stderr.trim()
            );
        }
        if fs::read_dir(&runtime_root)
            .context("failed to inspect rootless runc probe state")?
            .next()
            .is_some()
        {
            bail!("disposable rootless runc probe retained runtime state");
        }
        Ok(())
    }

    pub(crate) fn reconcile(&self, runtime_root: &Path, runtime_id: &str) -> Result<bool> {
        validate_runtime_id(runtime_id)?;
        let metadata = match fs::symlink_metadata(runtime_root) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error).context("failed to inspect native runtime root"),
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            bail!("native runtime root is not a real directory");
        }
        let entries = self.list_runtime_entries(runtime_root, "runc list during reconciliation")?;
        match entries.as_slice() {
            [] => {}
            [entry] if entry.id == runtime_id => {
                let deleted = self.invoke(
                    Some(runtime_root),
                    &[
                        OsString::from("delete"),
                        OsString::from("--force"),
                        OsString::from(runtime_id),
                    ],
                    self.helper_timeout,
                )?;
                if !deleted.status.success() {
                    return Err(helper_failure(
                        "runc delete --force during reconciliation",
                        &deleted,
                    ));
                }
                if !self
                    .list_runtime_entries(runtime_root, "runc list after reconciliation delete")?
                    .is_empty()
                {
                    bail!("native runtime state remains after reconciliation");
                }
            }
            [entry] => bail!(
                "private runc runtime root contains unexpected container {}; expected {runtime_id}",
                entry.id
            ),
            entries => {
                let ids = entries
                    .iter()
                    .map(|entry| entry.id.as_str())
                    .collect::<Vec<_>>();
                bail!("private runc runtime root contains multiple containers: {ids:?}");
            }
        }
        fs::remove_dir_all(runtime_root).context("failed to remove native runtime root")?;
        Ok(true)
    }

    fn list_runtime_entries(
        &self,
        runtime_root: &Path,
        operation: &str,
    ) -> Result<Vec<RuncListEntry>> {
        let listed = self.invoke(
            Some(runtime_root),
            &[
                OsString::from("list"),
                OsString::from("--format"),
                OsString::from("json"),
            ],
            self.helper_timeout,
        )?;
        if !listed.status.success() {
            return Err(helper_failure(operation, &listed));
        }
        let entries: Option<Vec<RuncListEntry>> = serde_json::from_slice(&listed.stdout)
            .with_context(|| format!("{operation} returned invalid JSON"))?;
        Ok(entries.unwrap_or_default())
    }

    #[cfg(test)]
    pub(crate) fn run(
        &self,
        bundle: &OciBundle,
        stdin: &[u8],
        timeout: Duration,
        capture_limits: RuncCaptureLimits,
        cancelled: &AtomicBool,
    ) -> std::result::Result<RuncRunResult, RuncRunFailure> {
        if timeout.is_zero() {
            return Err(RuncRunFailure::before_start(anyhow::anyhow!(
                "runc foreground timeout must be greater than zero"
            )));
        }
        if cancelled.load(Ordering::Acquire) {
            return Err(RuncRunFailure::before_start(anyhow::anyhow!(
                "runc run was cancelled before foreground start"
            )));
        }
        bundle.config_path().map_err(RuncRunFailure::before_start)?;
        bundle.rootfs().map_err(RuncRunFailure::before_start)?;

        let lifecycle = RuncLifecycle::create(self).map_err(RuncRunFailure::before_start)?;
        let lifecycle_stop = AtomicBool::new(false);
        let process_terminal_observed = AtomicBool::new(false);
        let execution = RuncExecution {
            stdin,
            timeout,
            capture_limits,
            cancelled,
            lifecycle_stop: &lifecycle_stop,
            process_terminal_observed: &process_terminal_observed,
            read_only_files: &[],
        };
        self.run_lifecycle(lifecycle, bundle, execution, None, None)
    }

    pub(crate) fn prepare_at<'a>(
        &'a self,
        runtime_root: &Path,
        runtime_id: &str,
        rootless: bool,
    ) -> Result<PreparedRuncRun<'a>> {
        let lifecycle = RuncLifecycle::create_at(self, runtime_root, runtime_id)?;
        let checkpoint = runtime_root
            .parent()
            .context("native runtime root has no participant workspace")?
            .join("cgroup.json");
        let mut prepared = PreparedRuncRun {
            runner: self,
            lifecycle: Some(lifecycle),
            cgroup: None,
        };
        if rootless {
            return Ok(prepared);
        }
        match PreparedNativeCgroup::prepare(runtime_id, &checkpoint) {
            Ok(cgroup) => {
                prepared.cgroup = Some(cgroup);
                Ok(prepared)
            }
            Err(error) => {
                let cleanup = prepared.cleanup_inner();
                Err(anyhow::anyhow!(
                    "failed to prepare native cgroup: {error:#}; runtime cleanup: {}",
                    cleanup_result(cleanup)
                ))
            }
        }
    }

    fn run_lifecycle(
        &self,
        mut lifecycle: RuncLifecycle<'_>,
        bundle: &OciBundle,
        execution: RuncExecution<'_>,
        network: Option<&NativeNetworkBinding>,
        mut cgroup: Option<PreparedNativeCgroup>,
    ) -> std::result::Result<RuncRunResult, RuncRunFailure> {
        let result = self.run_kept(&mut lifecycle, bundle, execution, network, &mut cgroup);
        let init_pid_on_failure = result
            .is_err()
            .then(|| observe_init_pid(lifecycle.pid_file()).ok().flatten())
            .flatten();
        match result {
            Ok(result) => Ok(result),
            Err(error) => {
                let runtime_cleanup = lifecycle.delete(false);
                let runtime_cleanup_failed = runtime_cleanup.is_err();
                let cgroup_cleanup = match cgroup {
                    Some(cgroup) => cgroup.finish_after_runc_delete(),
                    None => Ok(()),
                };
                match (runtime_cleanup, cgroup_cleanup) {
                    (Ok(()), Ok(())) => Err(RuncRunFailure {
                        init_pid: init_pid_on_failure,
                        error,
                    }),
                    (runtime_cleanup, cgroup_cleanup) => {
                        if runtime_cleanup_failed {
                            let _ = lifecycle.preserve_if_open();
                        }
                        Err(RuncRunFailure {
                            init_pid: init_pid_on_failure,
                            error: anyhow::anyhow!(
                                "runc execution failed: {error:#}; runtime cleanup: {}; cgroup cleanup: {}; native runtime resources require explicit reconciliation",
                                cleanup_result(runtime_cleanup),
                                cleanup_result(cgroup_cleanup)
                            ),
                        })
                    }
                }
            }
        }
    }

    fn run_kept(
        &self,
        lifecycle: &mut RuncLifecycle<'_>,
        bundle: &OciBundle,
        execution: RuncExecution<'_>,
        network: Option<&NativeNetworkBinding>,
        cgroup: &mut Option<PreparedNativeCgroup>,
    ) -> Result<RuncRunResult> {
        let mut foreground = self.start_foreground(
            lifecycle,
            bundle,
            execution.stdin,
            execution.capture_limits,
            network,
        )?;
        let monitored = self.monitor(
            &mut foreground.child,
            lifecycle,
            MonitorControls {
                stdout: &foreground.stdout_progress,
                stderr: &foreground.stderr_progress,
                timeout: execution.timeout,
                cancelled: execution.cancelled,
                lifecycle_stop: execution.lifecycle_stop,
                cgroup: cgroup.as_ref(),
            },
        );
        let foreground_status = match monitored {
            Ok(monitored) => monitored,
            Err(error) => {
                let recovery = self.recover_failed_foreground(&mut foreground.child, lifecycle);
                return match recovery {
                    Ok(()) => Err(error.context("runc foreground monitor failed")),
                    Err(recovery) => Err(anyhow::anyhow!(
                        "runc foreground monitor failed: {error:#}; recovery also failed: {recovery:#}"
                    )),
                };
            }
        };
        execution
            .process_terminal_observed
            .store(true, Ordering::Release);
        Ok(self.finish_kept_run(lifecycle, execution, cgroup, foreground, &foreground_status))
    }

    fn finish_kept_run(
        &self,
        lifecycle: &mut RuncLifecycle<'_>,
        execution: RuncExecution<'_>,
        cgroup: &mut Option<PreparedNativeCgroup>,
        foreground: ForegroundProcess,
        foreground_status: &MonitoredStatus,
    ) -> RuncRunResult {
        let mut operation_errors = Vec::new();
        let state_before_delete = match self.observe_stopped(lifecycle) {
            Ok(state) => Some(state),
            Err(error) => {
                operation_errors.push(RuncOperationError {
                    kind: RuncOperationErrorKind::StateObservation,
                    message: format!("{error:#}"),
                });
                None
            }
        };
        let oom_killed = observe_oom(
            cgroup.as_mut(),
            foreground_status.cgroup_verified,
            self.helper_timeout,
            &mut operation_errors,
        );
        let runtime_cleanup = lifecycle.delete(true);
        let cgroup_checkpoint = cgroup
            .as_ref()
            .map(|cgroup| cgroup.checkpoint_path().to_path_buf());
        let cgroup_cleanup = match cgroup.take() {
            Some(cgroup) => cgroup.finish_after_runc_delete(),
            None => Ok(()),
        };
        let cleanup = combine_cleanup(runtime_cleanup, cgroup_cleanup);
        let (stdout, stderr) = foreground.finish(cleanup.is_err(), self.helper_timeout);
        record_capture_error(
            &mut operation_errors,
            &stdout,
            RuncOperationErrorKind::StdoutCapture,
        );
        record_capture_error(
            &mut operation_errors,
            &stderr,
            RuncOperationErrorKind::StderrCapture,
        );
        let completed_limit = {
            if stdout.capture.observed_bytes > execution.capture_limits.stdout_bytes {
                Some(RuncStopReason::StdoutLimitExceeded)
            } else if stderr.capture.observed_bytes > execution.capture_limits.stderr_bytes {
                Some(RuncStopReason::StderrLimitExceeded)
            } else {
                None
            }
        };
        let mut result = RuncRunResult {
            init_pid: foreground_status.init_pid,
            foreground_status: foreground_status.status,
            stdout: stdout.capture,
            stderr: stderr.capture,
            stop_reason: foreground_status.stop_reason.or(completed_limit),
            state_before_delete,
            oom_killed,
            operation_errors,
            recovery: None,
        };
        if let Err(error) = cleanup {
            result.operation_errors.push(RuncOperationError {
                kind: RuncOperationErrorKind::Cleanup,
                message: format!("{error:#}"),
            });
            result.recovery = Some(RuncRecoveryHandle {
                runtime_root: lifecycle.preserve_if_open(),
                cgroup_checkpoint,
                id: lifecycle.id.clone(),
            });
        }
        result
    }

    fn start_foreground(
        &self,
        lifecycle: &RuncLifecycle<'_>,
        bundle: &OciBundle,
        stdin_bytes: &[u8],
        capture_limits: RuncCaptureLimits,
        network: Option<&NativeNetworkBinding>,
    ) -> Result<ForegroundProcess> {
        let mut stdin = tempfile().context("failed to create runc stdin capture")?;
        stdin
            .write_all(stdin_bytes)
            .context("failed to stage runc stdin bytes")?;
        stdin
            .seek(SeekFrom::Start(0))
            .context("failed to rewind staged runc stdin")?;
        let mut command = match network {
            Some(network) => network
                .entered_command(&self.executable)
                .context("failed to enter the Run network namespace for runc")?,
            None => self.command(),
        };
        let mut child = command
            .arg("--root")
            .arg(lifecycle.root())
            .args(["run", "--keep", "--pid-file"])
            .arg(lifecycle.pid_file())
            .arg("--bundle")
            .arg(bundle.path())
            .arg(lifecycle.id())
            .stdin(Stdio::from(stdin))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| {
                format!(
                    "failed to start configured runc at {}",
                    self.executable.display()
                )
            })?;
        let Some(stdout) = child.stdout.take() else {
            return Err(spawned_child_error(
                &mut child,
                anyhow::anyhow!("runc stdout pipe is unavailable"),
            ));
        };
        let Some(stderr) = child.stderr.take() else {
            return Err(spawned_child_error(
                &mut child,
                anyhow::anyhow!("runc stderr pipe is unavailable"),
            ));
        };
        if let Err(error) = set_nonblocking(&stdout) {
            return Err(spawned_child_error(
                &mut child,
                anyhow::Error::new(error).context("failed to configure runc stdout pipe"),
            ));
        }
        if let Err(error) = set_nonblocking(&stderr) {
            return Err(spawned_child_error(
                &mut child,
                anyhow::Error::new(error).context("failed to configure runc stderr pipe"),
            ));
        }
        let stdout_progress = Arc::new(StreamProgress::default());
        let stderr_progress = Arc::new(StreamProgress::default());
        let stdout_drain = spawn_stream_drain(
            stdout,
            capture_limits.stdout_bytes,
            Arc::clone(&stdout_progress),
        );
        let stderr_drain = spawn_stream_drain(
            stderr,
            capture_limits.stderr_bytes,
            Arc::clone(&stderr_progress),
        );
        Ok(ForegroundProcess {
            child,
            stdout_progress,
            stderr_progress,
            stdout_drain,
            stderr_drain,
        })
    }

    #[expect(
        clippy::too_many_lines,
        reason = "the monitor keeps one ordered process-control state machine"
    )]
    fn monitor(
        &self,
        child: &mut Child,
        lifecycle: &RuncLifecycle<'_>,
        controls: MonitorControls<'_>,
    ) -> Result<MonitoredStatus> {
        let deadline = checked_deadline(controls.timeout, "runc foreground timeout")?;
        let mut stop_reason = None;
        let mut init_pid = None;
        let mut cgroup_verified = controls.cgroup.is_none();
        let mut cancellation_deadline = None;
        let mut forced_exit_deadline = None;
        loop {
            observe_init_identity(
                lifecycle.pid_file(),
                controls.cgroup,
                &mut init_pid,
                &mut cgroup_verified,
            )?;
            if let Some(status) = child.try_wait().context("failed to poll runc foreground")? {
                return completed_monitored_status(
                    lifecycle,
                    controls,
                    status,
                    stop_reason,
                    init_pid,
                    cgroup_verified,
                );
            }

            if stop_reason.is_none() && controls.cancelled.load(Ordering::Acquire) {
                let reason = RuncStopReason::Cancelled;
                if let Some(status) = self.signal_for_stop(
                    child,
                    lifecycle,
                    "TERM",
                    reason,
                    init_pid,
                    cgroup_verified,
                )? {
                    return Ok(status);
                }
                stop_reason = Some(reason);
                cancellation_deadline = Some(checked_deadline(
                    CANCELLATION_GRACE,
                    "runc cancellation grace",
                )?);
            } else if stop_reason.is_none() && controls.lifecycle_stop.load(Ordering::Acquire) {
                let reason = RuncStopReason::LifecycleStop;
                if let Some(status) = self.signal_for_stop(
                    child,
                    lifecycle,
                    "TERM",
                    reason,
                    init_pid,
                    cgroup_verified,
                )? {
                    return Ok(status);
                }
                stop_reason = Some(reason);
                cancellation_deadline = Some(checked_deadline(
                    CANCELLATION_GRACE,
                    "runc lifecycle stop grace",
                )?);
            } else if stop_reason.is_none() && Instant::now() >= deadline {
                let reason = RuncStopReason::DeadlineExceeded;
                if let Some(status) = self.signal_for_stop(
                    child,
                    lifecycle,
                    "KILL",
                    reason,
                    init_pid,
                    cgroup_verified,
                )? {
                    return Ok(status);
                }
                stop_reason = Some(reason);
                forced_exit_deadline = Some(checked_deadline(
                    self.helper_timeout,
                    "runc foreground KILL reap timeout",
                )?);
            } else if stop_reason.is_none() && controls.stdout.exceeded() {
                let reason = RuncStopReason::StdoutLimitExceeded;
                if let Some(status) = self.signal_for_stop(
                    child,
                    lifecycle,
                    "KILL",
                    reason,
                    init_pid,
                    cgroup_verified,
                )? {
                    return Ok(status);
                }
                stop_reason = Some(reason);
                forced_exit_deadline = Some(checked_deadline(
                    self.helper_timeout,
                    "runc stdout-limit KILL reap timeout",
                )?);
            } else if stop_reason.is_none() && controls.stderr.exceeded() {
                let reason = RuncStopReason::StderrLimitExceeded;
                if let Some(status) = self.signal_for_stop(
                    child,
                    lifecycle,
                    "KILL",
                    reason,
                    init_pid,
                    cgroup_verified,
                )? {
                    return Ok(status);
                }
                stop_reason = Some(reason);
                forced_exit_deadline = Some(checked_deadline(
                    self.helper_timeout,
                    "runc stderr-limit KILL reap timeout",
                )?);
            } else if matches!(
                stop_reason,
                Some(RuncStopReason::Cancelled | RuncStopReason::LifecycleStop)
            ) && cancellation_deadline.is_some_and(|deadline| Instant::now() >= deadline)
            {
                if let SignalOutcome::ProcessExited(status) =
                    self.signal_when_running(child, lifecycle, "KILL")?
                {
                    return Ok(MonitoredStatus {
                        status,
                        stop_reason,
                        init_pid,
                        cgroup_verified,
                    });
                }
                cancellation_deadline = None;
                forced_exit_deadline = Some(checked_deadline(
                    self.helper_timeout,
                    "runc cancelled foreground KILL reap timeout",
                )?);
            } else if forced_exit_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                bail!("runc foreground did not exit after KILL before helper timeout");
            }
            thread::sleep(if cgroup_verified {
                PROCESS_POLL_INTERVAL
            } else {
                Duration::from_millis(1)
            });
        }
    }

    fn signal_for_stop(
        &self,
        child: &mut Child,
        lifecycle: &RuncLifecycle<'_>,
        signal: &str,
        reason: RuncStopReason,
        init_pid: Option<u32>,
        cgroup_verified: bool,
    ) -> Result<Option<MonitoredStatus>> {
        match self.signal_when_running(child, lifecycle, signal)? {
            SignalOutcome::ProcessExited(status) => Ok(Some(MonitoredStatus {
                status,
                stop_reason: stop_reason_before_signal(reason),
                init_pid,
                cgroup_verified,
            })),
            SignalOutcome::SignalSent => Ok(None),
        }
    }

    fn signal_when_running(
        &self,
        child: &mut Child,
        lifecycle: &RuncLifecycle<'_>,
        signal: &str,
    ) -> Result<SignalOutcome> {
        let deadline = checked_deadline(self.helper_timeout, "runc signal helper timeout")?;
        let mut last_state_error = Vec::new();
        loop {
            if let Some(status) = child.try_wait().context("failed to poll runc foreground")? {
                return Ok(SignalOutcome::ProcessExited(status));
            }
            let state = self.invoke(
                Some(lifecycle.root()),
                &[OsString::from("state"), OsString::from(lifecycle.id())],
                remaining(deadline)?,
            )?;
            if state.status.success() {
                let state = decode_state(&state.stdout, lifecycle.id())?;
                if state.status == "running" {
                    let killed = self.invoke(
                        Some(lifecycle.root()),
                        &[
                            OsString::from("kill"),
                            OsString::from(lifecycle.id()),
                            OsString::from(signal),
                        ],
                        remaining(deadline)?,
                    )?;
                    if killed.status.success() {
                        return Ok(SignalOutcome::SignalSent);
                    }
                    if let Some(status) =
                        child.try_wait().context("failed to poll runc foreground")?
                    {
                        return Ok(SignalOutcome::ProcessExited(status));
                    }
                    return Err(helper_failure(&format!("runc kill {signal}"), &killed));
                }
            } else {
                last_state_error = state.stderr;
            }
            if Instant::now() >= deadline {
                bail!(
                    "runc container {} did not become signalable before helper timeout; last state stderr: {}",
                    lifecycle.id(),
                    String::from_utf8_lossy(&last_state_error)
                );
            }
            thread::sleep(PROCESS_POLL_INTERVAL);
        }
    }

    fn observe_stopped(&self, lifecycle: &RuncLifecycle<'_>) -> Result<RuncStateObservation> {
        let deadline = checked_deadline(self.helper_timeout, "runc state helper timeout")?;
        loop {
            let output = self.invoke(
                Some(lifecycle.root()),
                &[OsString::from("state"), OsString::from(lifecycle.id())],
                remaining(deadline)?,
            )?;
            if !output.status.success() {
                return Err(helper_failure("runc state after foreground exit", &output));
            }
            let state = decode_state(&output.stdout, lifecycle.id())?;
            if state.status == "stopped" {
                return Ok(RuncStateObservation {
                    status: state.status,
                    pid: state.pid,
                });
            }
            if Instant::now() >= deadline {
                bail!(
                    "runc container {} remained in state {:?} after foreground exit",
                    lifecycle.id(),
                    state.status
                );
            }
            thread::sleep(PROCESS_POLL_INTERVAL);
        }
    }

    fn recover_failed_foreground(
        &self,
        child: &mut Child,
        lifecycle: &RuncLifecycle<'_>,
    ) -> Result<()> {
        if child
            .try_wait()
            .context("failed to poll failed runc foreground")?
            .is_none()
        {
            let _ = self.invoke(
                Some(lifecycle.root()),
                &[
                    OsString::from("kill"),
                    OsString::from(lifecycle.id()),
                    OsString::from("KILL"),
                ],
                self.helper_timeout,
            );
            if child
                .try_wait()
                .context("failed to poll failed runc foreground")?
                .is_none()
            {
                child
                    .kill()
                    .context("failed to kill failed runc foreground")?;
                child
                    .wait()
                    .context("failed to reap failed runc foreground")?;
            }
        }
        Ok(())
    }

    fn invoke(
        &self,
        runtime_root: Option<&Path>,
        arguments: &[OsString],
        timeout: Duration,
    ) -> Result<HelperOutput> {
        if timeout.is_zero() {
            bail!("runc helper deadline elapsed before invocation");
        }
        let mut stdout = tempfile().context("failed to create runc helper stdout capture")?;
        let mut stderr = tempfile().context("failed to create runc helper stderr capture")?;
        let mut command = self.command();
        if let Some(root) = runtime_root {
            command.arg("--root").arg(root);
        }
        let mut child = command
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::from(
                stdout
                    .try_clone()
                    .context("failed to clone runc helper stdout capture")?,
            ))
            .stderr(Stdio::from(
                stderr
                    .try_clone()
                    .context("failed to clone runc helper stderr capture")?,
            ))
            .spawn()
            .with_context(|| {
                format!(
                    "failed to execute configured runc at {}",
                    self.executable.display()
                )
            })?;
        let deadline = checked_deadline(timeout, "runc helper timeout")?;
        let status = loop {
            let observed = match child.try_wait() {
                Ok(observed) => observed,
                Err(error) => {
                    return Err(spawned_child_error(
                        &mut child,
                        anyhow::Error::new(error).context("failed to poll runc helper"),
                    ));
                }
            };
            if let Some(status) = observed {
                break status;
            }
            if Instant::now() >= deadline {
                child
                    .kill()
                    .context("failed to kill timed out runc helper")?;
                child
                    .wait()
                    .context("failed to reap timed out runc helper")?;
                let stdout = read_capture_bounded(
                    &mut stdout,
                    MAX_HELPER_OUTPUT_BYTES,
                    "timed out runc helper stdout",
                )?;
                let stderr = read_capture_bounded(
                    &mut stderr,
                    MAX_HELPER_OUTPUT_BYTES,
                    "timed out runc helper stderr",
                )?;
                bail!(
                    "runc helper exceeded {timeout:?}; stdout: {}; stderr: {}",
                    String::from_utf8_lossy(&stdout),
                    String::from_utf8_lossy(&stderr)
                );
            }
            thread::sleep(PROCESS_POLL_INTERVAL);
        };
        Ok(HelperOutput {
            status,
            stdout: read_capture_bounded(
                &mut stdout,
                MAX_HELPER_OUTPUT_BYTES,
                "runc helper stdout",
            )?,
            stderr: read_capture_bounded(
                &mut stderr,
                MAX_HELPER_OUTPUT_BYTES,
                "runc helper stderr",
            )?,
        })
    }
}

impl PreparedRuncRun<'_> {
    pub(crate) fn execute(
        mut self,
        bundle: &OciBundle,
        execution: RuncExecution<'_>,
        network: Option<&NativeNetworkBinding>,
    ) -> std::result::Result<RuncRunResult, RuncRunFailure> {
        let validation = (|| {
            if execution.timeout.is_zero() {
                bail!("runc foreground timeout must be greater than zero");
            }
            if execution.cancelled.load(Ordering::Acquire) {
                bail!("runc run was cancelled before foreground start");
            }
            if execution.lifecycle_stop.load(Ordering::Acquire) {
                bail!("runc lifecycle stop was requested before foreground start");
            }
            verify_all_sources(execution.read_only_files)?;
            bundle.config_path()?;
            bundle.rootfs()?;
            Ok(())
        })();
        if let Err(error) = validation {
            let cleanup = self.cleanup_inner();
            return Err(RuncRunFailure::before_start(anyhow::anyhow!(
                "runc input validation failed: {error:#}; prepared resource cleanup: {}",
                cleanup_result(cleanup)
            )));
        }
        let lifecycle = self
            .lifecycle
            .take()
            .expect("prepared runc lifecycle is owned");
        let cgroup = self.cgroup.take();
        self.runner
            .run_lifecycle(lifecycle, bundle, execution, network, cgroup)
    }

    pub(crate) fn cleanup(mut self) -> Result<()> {
        self.cleanup_inner()
    }

    fn cleanup_inner(&mut self) -> Result<()> {
        let runtime_cleanup = match self.lifecycle.as_mut() {
            Some(lifecycle) => lifecycle.delete(false),
            None => Ok(()),
        };
        if runtime_cleanup.is_err()
            && let Some(lifecycle) = self.lifecycle.as_mut()
        {
            let _ = lifecycle.preserve_if_open();
        }
        self.lifecycle.take();
        let cgroup_cleanup = match self.cgroup.take() {
            Some(cgroup) => cgroup.finish_after_runc_delete(),
            None => Ok(()),
        };
        combine_cleanup(runtime_cleanup, cgroup_cleanup)
    }
}

impl Drop for PreparedRuncRun<'_> {
    fn drop(&mut self) {
        let _ = self.cleanup_inner();
    }
}

fn find_executable(name: &str) -> Result<PathBuf> {
    let path = std::env::var_os("PATH").context("PATH is not set")?;
    for directory in std::env::split_paths(&path) {
        let candidate = directory.join(name);
        if candidate.is_file() {
            return candidate
                .canonicalize()
                .with_context(|| format!("failed to resolve {}", candidate.display()));
        }
    }
    bail!("executable is not available in PATH: {name}")
}

#[derive(Debug)]
struct RuncLifecycle<'a> {
    runner: &'a RuncRunner,
    root: Option<RuntimeRoot>,
    id: String,
    pid_file: PathBuf,
    deleted: bool,
}

#[derive(Debug)]
enum RuntimeRoot {
    #[cfg(test)]
    Temporary(TempDir),
    External(PathBuf),
}

impl RuntimeRoot {
    fn path(&self) -> &Path {
        match self {
            #[cfg(test)]
            Self::Temporary(directory) => directory.path(),
            Self::External(path) => path,
        }
    }

    fn preserve(self) -> PathBuf {
        match self {
            #[cfg(test)]
            Self::Temporary(directory) => directory.keep(),
            Self::External(path) => path,
        }
    }
}

impl<'a> RuncLifecycle<'a> {
    #[cfg(test)]
    fn create(runner: &'a RuncRunner) -> Result<Self> {
        let root = tempfile::Builder::new()
            .prefix("runlab-runc-root-")
            .tempdir()
            .context("failed to create private runc runtime root")?;
        ensure_private_directory(root.path())?;
        let pid_file = root.path().join("init.pid");
        Ok(Self {
            runner,
            root: Some(RuntimeRoot::Temporary(root)),
            id: format!("runlab-{}", Uuid::now_v7()),
            pid_file,
            deleted: false,
        })
    }

    fn create_at(runner: &'a RuncRunner, root: &Path, id: &str) -> Result<Self> {
        validate_runtime_id(id)?;
        fs::create_dir(root).with_context(|| {
            format!(
                "failed to create private runc runtime root {}",
                root.display()
            )
        })?;
        ensure_private_directory(root)?;
        Ok(Self {
            runner,
            root: Some(RuntimeRoot::External(root.to_path_buf())),
            id: id.to_owned(),
            pid_file: root.join("init.pid"),
            deleted: false,
        })
    }

    fn root(&self) -> &Path {
        self.root
            .as_ref()
            .expect("runc lifecycle root is open")
            .path()
    }

    fn id(&self) -> &str {
        &self.id
    }

    fn pid_file(&self) -> &Path {
        &self.pid_file
    }

    fn delete(&mut self, require_retained_state: bool) -> Result<()> {
        if self.deleted {
            return Ok(());
        }
        let deleted = self.runner.invoke(
            Some(self.root()),
            &[
                OsString::from("delete"),
                OsString::from("--force"),
                OsString::from(self.id()),
            ],
            self.runner.helper_timeout,
        )?;
        let listed = self.runner.invoke(
            Some(self.root()),
            &[
                OsString::from("list"),
                OsString::from("--format"),
                OsString::from("json"),
            ],
            self.runner.helper_timeout,
        )?;
        if !listed.status.success() {
            return Err(helper_failure("runc list after delete", &listed));
        }
        let entries: Option<Vec<RuncListEntry>> = serde_json::from_slice(&listed.stdout)
            .context("runc list returned invalid JSON after delete")?;
        let entries = entries.unwrap_or_default();
        if !entries.is_empty() {
            let ids = entries
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>();
            bail!("private runc runtime root retained containers: {ids:?}");
        }
        if require_retained_state && !deleted.status.success() {
            return Err(helper_failure("runc delete --force", &deleted));
        }
        fs::remove_dir_all(self.root()).context("failed to remove private runc runtime root")?;
        self.deleted = true;
        drop(self.root.take().expect("runc lifecycle root is open"));
        Ok(())
    }

    fn preserve_if_open(&mut self) -> Option<PathBuf> {
        let runtime_root = self.root.take().map(RuntimeRoot::preserve);
        self.deleted = true;
        runtime_root
    }
}

fn validate_runtime_id(id: &str) -> Result<()> {
    if !id.starts_with("runlab-")
        || id
            .bytes()
            .any(|byte| !byte.is_ascii_alphanumeric() && byte != b'-')
    {
        bail!("native runtime identity is invalid");
    }
    Ok(())
}

impl Drop for RuncLifecycle<'_> {
    fn drop(&mut self) {
        if self.deleted || self.root.is_none() {
            return;
        }
        let deleted = self.runner.invoke(
            Some(self.root()),
            &[
                OsString::from("delete"),
                OsString::from("--force"),
                OsString::from(self.id()),
            ],
            self.runner.helper_timeout,
        );
        if (deleted.is_err() || deleted.is_ok_and(|output| !output.status.success()))
            && let Some(root) = self.root.take()
        {
            let _ = root.preserve();
        }
    }
}

#[derive(Debug)]
struct ForegroundProcess {
    child: Child,
    stdout_progress: Arc<StreamProgress>,
    stderr_progress: Arc<StreamProgress>,
    stdout_drain: thread::JoinHandle<StreamDrainResult>,
    stderr_drain: thread::JoinHandle<StreamDrainResult>,
}

impl ForegroundProcess {
    fn finish(
        self,
        cleanup_failed: bool,
        timeout: Duration,
    ) -> (StreamDrainResult, StreamDrainResult) {
        if cleanup_failed {
            self.stdout_progress.request_stop();
            self.stderr_progress.request_stop();
        }
        let deadline = Instant::now().checked_add(timeout);
        let stdout = finish_stream_drain(
            self.stdout_drain,
            &self.stdout_progress,
            "runc stdout",
            deadline,
            cleanup_failed,
        );
        let stderr = finish_stream_drain(
            self.stderr_drain,
            &self.stderr_progress,
            "runc stderr",
            deadline,
            cleanup_failed,
        );
        (stdout, stderr)
    }
}

#[derive(Debug)]
struct MonitoredStatus {
    status: ExitStatus,
    stop_reason: Option<RuncStopReason>,
    init_pid: Option<u32>,
    cgroup_verified: bool,
}

#[derive(Debug)]
enum SignalOutcome {
    ProcessExited(ExitStatus),
    SignalSent,
}

#[derive(Debug, Clone, Copy)]
struct MonitorControls<'a> {
    stdout: &'a StreamProgress,
    stderr: &'a StreamProgress,
    timeout: Duration,
    cancelled: &'a AtomicBool,
    lifecycle_stop: &'a AtomicBool,
    cgroup: Option<&'a PreparedNativeCgroup>,
}

#[derive(Debug, Default)]
struct StreamProgress {
    observed_bytes: AtomicU64,
    exceeded: AtomicBool,
    stop_requested: AtomicBool,
    retained: Mutex<Vec<u8>>,
}

impl StreamProgress {
    fn observed_bytes(&self) -> u64 {
        self.observed_bytes.load(Ordering::Acquire)
    }

    fn exceeded(&self) -> bool {
        self.exceeded.load(Ordering::Acquire)
    }

    fn request_stop(&self) {
        self.stop_requested.store(true, Ordering::Release);
    }

    fn stop_requested(&self) -> bool {
        self.stop_requested.load(Ordering::Acquire)
    }

    fn retain(&self, bytes: &[u8], limit: u64) {
        let mut retained = self
            .retained
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let remaining = usize::try_from(limit.saturating_sub(retained.len() as u64)).unwrap_or(0);
        retained.extend_from_slice(&bytes[..bytes.len().min(remaining)]);
    }

    fn snapshot(&self, error: Option<String>) -> StreamDrainResult {
        let bytes = self
            .retained
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let observed_bytes = self.observed_bytes();
        StreamDrainResult {
            capture: RuncStreamCapture {
                partial: error.is_some() || observed_bytes > bytes.len() as u64,
                bytes,
                observed_bytes,
            },
            error,
        }
    }
}

#[derive(Debug)]
struct StreamDrainResult {
    capture: RuncStreamCapture,
    error: Option<String>,
}

#[derive(Debug)]
struct HelperOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

#[derive(Debug, Deserialize)]
struct RuncState {
    id: String,
    status: String,
    pid: u32,
}

#[derive(Debug, Deserialize)]
struct RuncListEntry {
    id: String,
}

fn decode_state(bytes: &[u8], expected_id: &str) -> Result<RuncState> {
    let state: RuncState =
        serde_json::from_slice(bytes).context("runc state returned invalid JSON")?;
    if state.id != expected_id {
        bail!(
            "runc state identity mismatch: expected {expected_id}, received {}",
            state.id
        );
    }
    Ok(state)
}

fn decode_identity(bytes: &[u8], digest: Digest, size: u64) -> Result<RuncIdentity> {
    let text = std::str::from_utf8(bytes).context("runc --version returned non-UTF-8 stdout")?;
    let mut lines = text.lines();
    let version = lines
        .next()
        .and_then(|line| line.strip_prefix("runc version "))
        .filter(|value| !value.is_empty())
        .context("runc --version omitted its version")?;
    let commit = lines
        .next()
        .and_then(|line| line.strip_prefix("commit: "))
        .filter(|value| !value.is_empty())
        .context("runc --version omitted its commit")?;
    let runtime_spec = lines
        .next()
        .and_then(|line| line.strip_prefix("spec: "))
        .filter(|value| !value.is_empty())
        .context("runc --version omitted its runtime-spec version")?;
    Ok(RuncIdentity {
        version: version.to_owned(),
        commit: commit.to_owned(),
        runtime_spec: runtime_spec.to_owned(),
        digest,
        size,
    })
}

fn helper_failure(operation: &str, output: &HelperOutput) -> anyhow::Error {
    anyhow::anyhow!(
        "{operation} failed with {}; stdout: {}; stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn checked_deadline(duration: Duration, name: &str) -> Result<Instant> {
    Instant::now()
        .checked_add(duration)
        .with_context(|| format!("{name} is too large"))
}

fn remaining(deadline: Instant) -> Result<Duration> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        bail!("runc helper deadline elapsed");
    }
    Ok(remaining)
}

fn completed_capture_limit(controls: MonitorControls<'_>) -> Option<RuncStopReason> {
    if controls.stdout.exceeded() {
        return Some(RuncStopReason::StdoutLimitExceeded);
    }
    if controls.stderr.exceeded() {
        return Some(RuncStopReason::StderrLimitExceeded);
    }
    None
}

fn stop_reason_before_signal(reason: RuncStopReason) -> Option<RuncStopReason> {
    match reason {
        RuncStopReason::LifecycleStop => None,
        reason => Some(reason),
    }
}

fn completed_monitored_status(
    lifecycle: &RuncLifecycle<'_>,
    controls: MonitorControls<'_>,
    status: ExitStatus,
    stop_reason: Option<RuncStopReason>,
    init_pid: Option<u32>,
    cgroup_verified: bool,
) -> Result<MonitoredStatus> {
    let init_pid = match init_pid {
        Some(init_pid) => Some(init_pid),
        None => observe_init_pid(lifecycle.pid_file())?,
    };
    Ok(MonitoredStatus {
        status,
        stop_reason: stop_reason.or_else(|| completed_capture_limit(controls)),
        init_pid,
        cgroup_verified,
    })
}

fn record_capture_error(
    errors: &mut Vec<RuncOperationError>,
    capture: &StreamDrainResult,
    kind: RuncOperationErrorKind,
) {
    if let Some(message) = &capture.error {
        errors.push(RuncOperationError {
            kind,
            message: message.clone(),
        });
    }
}

fn spawn_stream_drain(
    reader: impl Read + Send + 'static,
    limit: u64,
    progress: Arc<StreamProgress>,
) -> thread::JoinHandle<StreamDrainResult> {
    thread::spawn(move || drain_pipe(reader, limit, &progress))
}

fn set_nonblocking(fd: impl std::os::fd::AsFd) -> std::io::Result<()> {
    use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};

    let flags = fcntl_getfl(&fd)?;
    Ok(fcntl_setfl(fd, flags | OFlags::NONBLOCK)?)
}

fn spawned_child_error(child: &mut Child, error: anyhow::Error) -> anyhow::Error {
    let kill = child.kill();
    match child.wait() {
        Ok(_) => error,
        Err(wait_error) => match kill {
            Ok(()) => error.context(format!("failed to reap spawned child: {wait_error}")),
            Err(kill_error) => error.context(format!(
                "failed to kill spawned child: {kill_error}; failed to reap it: {wait_error}"
            )),
        },
    }
}

fn drain_pipe(mut reader: impl Read, limit: u64, progress: &StreamProgress) -> StreamDrainResult {
    let mut buffer = [0_u8; 16 * 1024];
    let mut error = None;
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                if let Err(observation_error) =
                    record_stream_bytes(progress, &buffer[..read], limit)
                {
                    error = Some(observation_error);
                    break;
                }
                if progress.stop_requested() {
                    error = Some(
                        "stream capture stopped while inherited writers remained after runtime cleanup"
                            .to_owned(),
                    );
                    break;
                }
            }
            Err(read_error) if read_error.kind() == std::io::ErrorKind::WouldBlock => {
                if progress.stop_requested() {
                    error = Some(
                        "stream capture stopped while inherited writers remained after runtime cleanup"
                            .to_owned(),
                    );
                    break;
                }
                thread::sleep(PROCESS_POLL_INTERVAL);
            }
            Err(read_error) => {
                error = Some(format!("stream read failed: {read_error}"));
                break;
            }
        }
    }
    progress.snapshot(error)
}

#[cfg(test)]
fn drain_stream(mut reader: impl Read, limit: u64, progress: &StreamProgress) -> StreamDrainResult {
    let mut buffer = [0_u8; 16 * 1024];
    let mut error = None;
    loop {
        let read = match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(read_error) => {
                error = Some(format!("stream read failed: {read_error}"));
                break;
            }
        };
        if let Err(observation_error) = record_stream_bytes(progress, &buffer[..read], limit) {
            error = Some(observation_error);
            break;
        }
    }
    progress.snapshot(error)
}

fn record_stream_bytes(
    progress: &StreamProgress,
    bytes: &[u8],
    limit: u64,
) -> std::result::Result<(), String> {
    let read = u64::try_from(bytes.len()).expect("read size fits u64");
    let Some(total) = progress.observed_bytes().checked_add(read) else {
        return Err("stream observed byte count overflowed u64".to_owned());
    };
    progress.observed_bytes.store(total, Ordering::Release);
    if total > limit {
        progress.exceeded.store(true, Ordering::Release);
    }
    progress.retain(bytes, limit);
    Ok(())
}

fn finish_stream_drain(
    drain: thread::JoinHandle<StreamDrainResult>,
    progress: &StreamProgress,
    stream: &str,
    deadline: Option<Instant>,
    cleanup_failed: bool,
) -> StreamDrainResult {
    while !drain.is_finished()
        && deadline.is_some_and(|deadline| Instant::now() < deadline)
        && !cleanup_failed
    {
        thread::sleep(PROCESS_POLL_INTERVAL);
    }
    if !drain.is_finished() {
        progress.request_stop();
        let stop_deadline = Instant::now() + Duration::from_millis(100);
        while !drain.is_finished() && Instant::now() < stop_deadline {
            thread::sleep(PROCESS_POLL_INTERVAL);
        }
    }
    if !drain.is_finished() {
        return progress.snapshot(Some(format!(
            "{stream} drain did not stop after its capture deadline"
        )));
    }
    drain.join().unwrap_or_else(|_| {
        progress.snapshot(Some(format!(
            "{stream} drain thread panicked at a Rust boundary"
        )))
    })
}

fn read_capture_bounded(file: &mut File, limit: u64, stream: &str) -> Result<Vec<u8>> {
    file.seek(SeekFrom::Start(0))
        .with_context(|| format!("failed to rewind {stream}"))?;
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("failed to read {stream}"))?;
    if bytes.len() as u64 > limit {
        bail!("{stream} exceeded {limit} bytes");
    }
    Ok(bytes)
}

fn observe_init_pid(pid_file: &Path) -> Result<Option<u32>> {
    match fs::read_to_string(pid_file) {
        Ok(value) => value
            .trim()
            .parse::<u32>()
            .map(Some)
            .context("runc pid file is invalid"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).context("failed to read runc pid file"),
    }
}

fn observe_init_identity(
    pid_file: &Path,
    cgroup: Option<&PreparedNativeCgroup>,
    init_pid: &mut Option<u32>,
    cgroup_verified: &mut bool,
) -> Result<()> {
    if init_pid.is_none() {
        *init_pid = observe_init_pid(pid_file)?;
    }
    if let (Some(cgroup), Some(init_pid)) = (cgroup, *init_pid)
        && cgroup.verify_init_pid(init_pid)?
    {
        *cgroup_verified = true;
    }
    if !*cgroup_verified
        && let Some(cgroup) = cgroup
        && cgroup.has_observed_member()?
    {
        *cgroup_verified = true;
    }
    Ok(())
}

fn combine_cleanup(runtime: Result<()>, cgroup: Result<()>) -> Result<()> {
    match (runtime, cgroup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(runtime), Ok(())) => Err(runtime).context("runc runtime cleanup failed"),
        (Ok(()), Err(cgroup)) => Err(cgroup).context("native cgroup cleanup failed"),
        (Err(runtime), Err(cgroup)) => Err(anyhow::anyhow!(
            "runc runtime cleanup failed: {runtime:#}; native cgroup cleanup failed: {cgroup:#}"
        )),
    }
}

fn observe_oom(
    cgroup: Option<&mut PreparedNativeCgroup>,
    cgroup_verified: bool,
    timeout: Duration,
    operation_errors: &mut Vec<RuncOperationError>,
) -> Option<bool> {
    let cgroup = cgroup?;
    match cgroup.observe_terminal(timeout) {
        Ok(observation) if cgroup_verified => Some(observation.oom_killed),
        Ok(_) => {
            operation_errors.push(RuncOperationError {
                kind: RuncOperationErrorKind::OomObservation,
                message: "runc init cgroup membership was not observed".to_owned(),
            });
            None
        }
        Err(error) => {
            operation_errors.push(RuncOperationError {
                kind: RuncOperationErrorKind::OomObservation,
                message: format!("{error:#}"),
            });
            None
        }
    }
}

fn cleanup_result(result: Result<()>) -> String {
    match result {
        Ok(()) => "complete".to_owned(),
        Err(error) => format!("failed: {error:#}"),
    }
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::fs;
    use std::io::{Cursor, Error as IoError};
    use std::os::unix::fs::PermissionsExt as _;
    use std::sync::Arc;

    use serde_json::{Value, json};

    use super::*;
    use crate::runtime::RuntimeConfig;

    const TEST_CAPTURE_LIMIT: u64 = 1024 * 1024;

    #[test]
    fn lifecycle_stop_requires_a_signalable_process() {
        assert_eq!(
            stop_reason_before_signal(RuncStopReason::LifecycleStop),
            None
        );
        assert_eq!(
            stop_reason_before_signal(RuncStopReason::Cancelled),
            Some(RuncStopReason::Cancelled)
        );
    }

    #[test]
    fn decodes_runtime_identity_without_selecting_a_supported_version() {
        let identity = decode_identity(
            b"runc version 1.3.6\ncommit: v1.3.6-0-g491b69ba\nspec: 1.2.1\ngo: go1.25.5\n",
            crate::integrity::digest_bytes(b"runc fixture"),
            12,
        )
        .expect("identity");
        assert_eq!(
            identity,
            RuncIdentity {
                version: "1.3.6".to_owned(),
                commit: "v1.3.6-0-g491b69ba".to_owned(),
                runtime_spec: "1.2.1".to_owned(),
                digest: crate::integrity::digest_bytes(b"runc fixture"),
                size: 12,
            }
        );
    }

    #[test]
    fn helper_probe_has_a_hard_deadline() {
        let directory = tempfile::tempdir().expect("fixture");
        let executable = directory.path().join("runc");
        fs::write(&executable, b"#!/bin/sh\nexec sleep 30\n").expect("fake runc");
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700))
            .expect("fake runc mode");
        let started = Instant::now();
        let error = RuncRunner::probe(&executable, Duration::from_millis(50))
            .expect_err("probe must time out");
        assert!(error.to_string().contains("runc helper exceeded"));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn reconciliation_deletes_only_the_expected_runtime() {
        let state = tempfile::tempdir().expect("runtime state marker directory");
        let marker = state.path().join("present");
        fs::write(&marker, b"").expect("runtime state marker");
        let fixture = FakeRunc::new(&format!(
            r#"
if command == "list":
    if os.path.exists({marker}):
        print(json.dumps([{{"id": "runlab-recovery"}}]))
    else:
        print("[]")
    raise SystemExit(0)
if command == "delete":
    if args[-1] != "runlab-recovery":
        raise SystemExit(91)
    os.unlink({marker})
    raise SystemExit(0)
"#,
            marker = serde_json::to_string(&marker.to_string_lossy()).expect("marker JSON")
        ));
        let runtime_root = tempfile::tempdir().expect("runtime root");

        assert!(
            fixture
                .runner()
                .reconcile(runtime_root.path(), "runlab-recovery")
                .expect("reconciliation")
        );
        assert!(!runtime_root.path().exists());
    }

    #[test]
    fn reconciliation_rejects_an_unexpected_runtime_identity() {
        let fixture = FakeRunc::new(
            r#"
if command == "list":
    print(json.dumps([{"id": "someone-else"}]))
    raise SystemExit(0)
if command == "delete":
    raise SystemExit(91)
"#,
        );
        let runtime_root = tempfile::tempdir().expect("runtime root");

        let error = fixture
            .runner()
            .reconcile(runtime_root.path(), "runlab-recovery")
            .expect_err("unexpected identity must be preserved");
        assert!(
            error
                .to_string()
                .contains("unexpected container someone-else")
        );
        assert!(runtime_root.path().is_dir());
    }

    #[test]
    fn reconciliation_reports_delete_failure() {
        let fixture = FakeRunc::new(
            r#"
if command == "list":
    print(json.dumps([{"id": "runlab-recovery"}]))
    raise SystemExit(0)
if command == "delete":
    os.write(2, b"fixture delete failure")
    raise SystemExit(4)
"#,
        );
        let runtime_root = tempfile::tempdir().expect("runtime root");

        let error = fixture
            .runner()
            .reconcile(runtime_root.path(), "runlab-recovery")
            .expect_err("delete failure must not be inferred away");
        assert!(
            error
                .to_string()
                .contains("runc delete --force during reconciliation failed")
        );
        assert!(runtime_root.path().is_dir());
    }

    #[test]
    fn stream_capture_keeps_observed_size_and_prefix_after_read_error() {
        struct FailingReader {
            delivered: bool,
        }

        impl Read for FailingReader {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                if self.delivered {
                    return Err(IoError::other("fixture read failure"));
                }
                self.delivered = true;
                buffer[..3].copy_from_slice(b"abc");
                Ok(3)
            }
        }

        let progress = StreamProgress::default();
        let result = drain_stream(FailingReader { delivered: false }, 16, &progress);
        assert_eq!(result.capture.bytes, b"abc");
        assert_eq!(result.capture.observed_bytes, 3);
        assert!(result.capture.partial);
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("fixture read failure"))
        );
    }

    #[test]
    fn stream_capture_tracks_each_limit_independently() {
        let stdout_progress = StreamProgress::default();
        let stderr_progress = StreamProgress::default();
        let stdout = drain_stream(Cursor::new(b"stdout-over-limit"), 4, &stdout_progress);
        let stderr = drain_stream(Cursor::new(b"stderr-also-over-limit"), 6, &stderr_progress);

        assert_eq!(stdout.capture.bytes, b"stdo");
        assert_eq!(stdout.capture.observed_bytes, 17);
        assert!(stdout.capture.partial);
        assert!(stdout_progress.exceeded());
        assert_eq!(stderr.capture.bytes, b"stderr");
        assert_eq!(stderr.capture.observed_bytes, 22);
        assert!(stderr.capture.partial);
        assert!(stderr_progress.exceeded());
    }

    #[test]
    fn foreground_uses_pipes_and_preserves_completed_facts() {
        let fixture = FakeRunc::new(
            r#"
if command == "run":
    try:
        os.lseek(1, 0, os.SEEK_SET)
    except OSError:
        os.write(1, b"pipe-out")
        os.write(2, b"pipe-err")
        raise SystemExit(7)
    raise SystemExit(91)
if command == "state":
    print(json.dumps({"id": args[-1], "status": "stopped", "pid": 0}))
    raise SystemExit(0)
if command == "delete":
    raise SystemExit(0)
if command == "list":
    print("[]")
    raise SystemExit(0)
"#,
        );
        let runner = fixture.runner();
        let bundle = OciBundle::create(&test_runtime()).expect("bundle");
        let result = runner
            .run(
                &bundle,
                &[],
                Duration::from_secs(2),
                RuncCaptureLimits::new(64, 64).expect("limits"),
                &AtomicBool::new(false),
            )
            .expect("run result");

        assert_eq!(result.foreground_status.code(), Some(7));
        assert_eq!(result.stdout.bytes, b"pipe-out");
        assert_eq!(result.stderr.bytes, b"pipe-err");
        assert_complete_capture(&result.stdout);
        assert_complete_capture(&result.stderr);
        assert_stopped(&result);
    }

    #[test]
    fn observation_and_cleanup_errors_do_not_erase_completed_facts() {
        let fixture = FakeRunc::new(
            r#"
if command == "run":
    os.write(1, b"completed-out")
    os.write(2, b"completed-err")
    raise SystemExit(9)
if command == "state":
    os.write(2, b"state unavailable")
    raise SystemExit(3)
if command == "delete":
    os.write(2, b"delete failed")
    raise SystemExit(4)
if command == "list":
    print(json.dumps([{"id": "retained"}]))
    raise SystemExit(0)
"#,
        );
        let runner = fixture.runner();
        let bundle = OciBundle::create(&test_runtime()).expect("bundle");
        let result = runner
            .run(
                &bundle,
                &[],
                Duration::from_secs(2),
                RuncCaptureLimits::new(64, 64).expect("limits"),
                &AtomicBool::new(false),
            )
            .expect("completed facts remain available");

        assert_eq!(result.foreground_status.code(), Some(9));
        assert_eq!(result.stdout.bytes, b"completed-out");
        assert_eq!(result.stderr.bytes, b"completed-err");
        assert_eq!(result.state_before_delete, None);
        assert_eq!(
            result
                .operation_errors
                .iter()
                .map(|error| error.kind)
                .collect::<Vec<_>>(),
            [
                RuncOperationErrorKind::StateObservation,
                RuncOperationErrorKind::Cleanup
            ]
        );
        let recovery = result.recovery.expect("recovery handle");
        assert!(
            recovery
                .runtime_root
                .as_ref()
                .is_some_and(|root| root.is_dir())
        );
        assert_eq!(recovery.cgroup_checkpoint, None);
        assert!(recovery.id.starts_with("runlab-"));
        fs::remove_dir_all(recovery.runtime_root.expect("runtime root"))
            .expect("remove fixture recovery root");
    }

    #[test]
    fn cleanup_failure_with_inherited_pipes_returns_bounded_partial_facts() {
        let fixture = FakeRunc::new(
            r#"
if command == "run":
    if os.fork() == 0:
        time.sleep(1)
        os._exit(0)
    os.write(1, b"completed-out")
    os.write(2, b"completed-err")
    raise SystemExit(9)
if command == "state":
    print(json.dumps({"id": args[-1], "status": "stopped", "pid": 0}))
    raise SystemExit(0)
if command == "delete":
    os.write(2, b"delete failed")
    raise SystemExit(4)
if command == "list":
    print(json.dumps([{"id": "retained"}]))
    raise SystemExit(0)
"#,
        );
        let runner = fixture.runner();
        let bundle = OciBundle::create(&test_runtime()).expect("bundle");
        let started = Instant::now();
        let result = runner
            .run(
                &bundle,
                &[],
                Duration::from_secs(2),
                RuncCaptureLimits::new(64, 64).expect("limits"),
                &AtomicBool::new(false),
            )
            .expect("completed facts remain available");

        assert!(started.elapsed() < Duration::from_millis(500));
        assert_eq!(result.foreground_status.code(), Some(9));
        assert_eq!(result.stdout.bytes, b"completed-out");
        assert_eq!(result.stderr.bytes, b"completed-err");
        assert!(result.stdout.partial);
        assert!(result.stderr.partial);
        assert!(result.operation_errors.iter().any(|error| {
            error.kind == RuncOperationErrorKind::StdoutCapture
                && error.message.contains("inherited writers remained")
        }));
        assert!(result.operation_errors.iter().any(|error| {
            error.kind == RuncOperationErrorKind::StderrCapture
                && error.message.contains("inherited writers remained")
        }));
        let recovery = result.recovery.expect("recovery handle");
        fs::remove_dir_all(recovery.runtime_root.expect("runtime root"))
            .expect("remove fixture recovery root");
    }

    #[test]
    fn preserving_after_successful_delete_is_empty_and_idempotent() {
        let fixture = FakeRunc::new(
            r#"
if command == "delete":
    raise SystemExit(0)
if command == "list":
    print("[]")
    raise SystemExit(0)
raise SystemExit(2)
"#,
        );
        let runner = fixture.runner();
        let mut lifecycle = RuncLifecycle::create(&runner).expect("lifecycle");
        lifecycle.delete(false).expect("delete");

        assert_eq!(lifecycle.preserve_if_open(), None);
        assert_eq!(lifecycle.preserve_if_open(), None);
    }

    #[test]
    #[ignore = "requires writable cgroup v2"]
    fn unverified_cgroup_membership_never_produces_negative_oom_fact() {
        let checkpoint_directory = tempfile::tempdir().expect("checkpoint directory");
        let runtime_id = format!("runlab-{}", Uuid::now_v7());
        let mut cgroup = PreparedNativeCgroup::prepare(
            &runtime_id,
            &checkpoint_directory.path().join("cgroup.json"),
        )
        .expect("cgroup");
        let mut errors = Vec::new();

        let oom_killed = observe_oom(
            Some(&mut cgroup),
            false,
            Duration::from_secs(1),
            &mut errors,
        );

        assert_eq!(oom_killed, None);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].kind, RuncOperationErrorKind::OomObservation);
        assert!(errors[0].message.contains("membership was not observed"));
        cgroup.cleanup_owned_empty().expect("cleanup cgroup");
    }

    #[test]
    fn confirmed_cancellation_survives_foreground_exit_race() {
        let marker_directory = tempfile::tempdir().expect("marker directory");
        let marker = marker_directory.path().join("running");
        let fixture = FakeRunc::new(&format!(
            r#"
if command == "run":
    open({}, "wb").write(b"ready")
    time.sleep(0.08)
    raise SystemExit(0)
if command == "state":
    time.sleep(0.15)
    print(json.dumps({{"id": args[-1], "status": "stopped", "pid": 0}}))
    raise SystemExit(0)
if command == "delete":
    raise SystemExit(0)
if command == "list":
    print("[]")
    raise SystemExit(0)
"#,
            serde_json::to_string(&marker.to_string_lossy()).expect("marker JSON")
        ));
        let runner = fixture.runner();
        let bundle = OciBundle::create(&test_runtime()).expect("bundle");
        let cancelled = Arc::new(AtomicBool::new(false));
        let result = thread::scope(|scope| {
            let flag = Arc::clone(&cancelled);
            let marker = marker.clone();
            let watcher = scope.spawn(move || {
                wait_for_path(&marker, Duration::from_secs(1));
                flag.store(true, Ordering::Release);
            });
            let result = runner.run(
                &bundle,
                &[],
                Duration::from_secs(2),
                RuncCaptureLimits::new(64, 64).expect("limits"),
                &cancelled,
            );
            watcher.join().expect("cancel watcher");
            result
        })
        .expect("cancelled result");

        assert_eq!(result.foreground_status.code(), Some(0));
        assert_eq!(result.stop_reason, Some(RuncStopReason::Cancelled));
        assert_stopped(&result);
    }

    #[test]
    #[ignore = "requires RUNLAB_TEST_RUNC, RUNLAB_TEST_PYTHON, rootful cgroup-v2 Linux and mount privileges"]
    #[expect(
        clippy::too_many_lines,
        reason = "the sequential real-runtime corpus keeps one setup and one frozen cleanup denominator"
    )]
    fn real_ordinary_linux_runc_1_5_1_production_lifecycle() {
        let executable = required_absolute_path("RUNLAB_TEST_RUNC");
        let python = required_absolute_path("RUNLAB_TEST_PYTHON");
        let runner = RuncRunner::probe(&executable, Duration::from_secs(5)).expect("runc probe");
        let (digest, size) =
            digest_reader(File::open(&executable).expect("open runc")).expect("digest runc");
        assert_eq!(runner.identity().version, "1.5.1");
        assert_eq!(runner.identity().commit, "v1.5.1-0-g8f2685a47");
        assert_eq!(runner.identity().runtime_spec, "1.3.0");
        assert_eq!(runner.identity().digest, digest);
        assert_eq!(runner.identity().size, size);
        let limits =
            RuncCaptureLimits::new(TEST_CAPTURE_LIMIT, TEST_CAPTURE_LIMIT).expect("capture limits");
        let idle_cancel = AtomicBool::new(false);

        let input = [0, b'A', 0xff, b'\n'];
        let written_directory = tempfile::tempdir().expect("rootfs write directory");
        let written_path = written_directory.path().join("changed.bin");
        let exact_script = format!(
            "import sys\nb=sys.stdin.buffer.read()\nopen({},'wb').write(b'\\0changed\\xff')\nsys.stdout.buffer.write(b'\\0out:'+b)\nsys.stderr.buffer.write(b'\\0err:'+b+b'\\xff')",
            serde_json::to_string(&written_path.to_string_lossy()).expect("write path JSON")
        );
        let exact = MountedBundle::create(&executable, &python_arguments(&python, &exact_script));
        let exact_result = runner
            .run(
                exact.bundle(),
                &input,
                Duration::from_secs(10),
                limits,
                &idle_cancel,
            )
            .expect("exact run");
        assert_eq!(exact_result.foreground_status.code(), Some(0));
        assert_eq!(
            exact_result.stdout.bytes,
            [b"\0out:".as_slice(), &input].concat()
        );
        assert_eq!(
            exact_result.stderr.bytes,
            [b"\0err:".as_slice(), &input, &[0xff]].concat()
        );
        assert_complete_capture(&exact_result.stdout);
        assert_complete_capture(&exact_result.stderr);
        assert_eq!(exact_result.stop_reason, None);
        assert_stopped(&exact_result);
        assert_eq!(
            fs::read(&written_path).expect("rootfs write"),
            b"\0changed\xff"
        );
        exact.finish();

        let exit_seven = MountedBundle::create(
            &executable,
            &python_arguments(
                &python,
                "import os\nos.write(1,b'seven-out')\nos.write(2,b'seven-err')\nraise SystemExit(7)",
            ),
        );
        let exit_seven_result = runner
            .run(
                exit_seven.bundle(),
                &[],
                Duration::from_secs(10),
                limits,
                &idle_cancel,
            )
            .expect("exit-seven run");
        assert_eq!(exit_seven_result.foreground_status.code(), Some(7));
        assert_eq!(exit_seven_result.stdout.bytes, b"seven-out");
        assert_eq!(exit_seven_result.stderr.bytes, b"seven-err");
        assert_eq!(exit_seven_result.stop_reason, None);
        assert_stopped(&exit_seven_result);
        exit_seven.finish();

        let fast_exit = MountedBundle::create(&executable, &["/bin/true".to_owned()]);
        let fast_exit_result = runner
            .run(
                fast_exit.bundle(),
                &[],
                Duration::from_secs(10),
                limits,
                &idle_cancel,
            )
            .expect("fast-exit run");
        assert_eq!(fast_exit_result.foreground_status.code(), Some(0));
        assert_stopped(&fast_exit_result);
        fast_exit.finish();

        let self_signal = MountedBundle::create(
            &executable,
            &python_arguments(&python, "import ctypes\nctypes.CDLL(None).abort()"),
        );
        let self_signal_result = runner
            .run(
                self_signal.bundle(),
                &[],
                Duration::from_secs(10),
                limits,
                &idle_cancel,
            )
            .expect("self-signal run");
        assert_eq!(self_signal_result.foreground_status.code(), Some(133));
        assert_eq!(self_signal_result.stop_reason, None);
        assert_stopped(&self_signal_result);
        self_signal.finish();

        let marker_directory = tempfile::tempdir().expect("cancel marker directory");
        let marker = marker_directory.path().join("ready");
        let cancel_script = format!(
            "import os,signal,subprocess,sys,time\ndef stop(_signal,_frame):\n os.write(2,b'cancelled')\n raise SystemExit(42)\nsignal.signal(signal.SIGTERM,stop)\nsubprocess.Popen([sys.executable,'-c','import time\\nwhile True: time.sleep(1)'],stdin=subprocess.DEVNULL,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)\nopen({},'wb').write(b'ready')\nos.write(1,b'ready\\n')\nwhile True: time.sleep(1)",
            serde_json::to_string(&marker.to_string_lossy()).expect("marker JSON")
        );
        let cancelled_bundle =
            MountedBundle::create(&executable, &python_arguments(&python, &cancel_script));
        let cancellation = Arc::new(AtomicBool::new(false));
        let cancelled_result = thread::scope(|scope| {
            let cancellation_flag = Arc::clone(&cancellation);
            let marker = marker.clone();
            let cancellation_task = scope.spawn(move || {
                wait_for_path(&marker, Duration::from_secs(5));
                cancellation_flag.store(true, Ordering::Release);
            });
            let result = runner.run(
                cancelled_bundle.bundle(),
                &[],
                Duration::from_secs(10),
                limits,
                &cancellation,
            );
            cancellation_task.join().expect("cancel watcher");
            result
        })
        .expect("cancelled run");
        assert_eq!(cancelled_result.foreground_status.code(), Some(42));
        assert_eq!(cancelled_result.stdout.bytes, b"ready\n");
        assert_eq!(cancelled_result.stderr.bytes, b"cancelled");
        assert_eq!(
            cancelled_result.stop_reason,
            Some(RuncStopReason::Cancelled)
        );
        assert_stopped(&cancelled_result);
        cancelled_bundle.finish();

        let deadline_bundle = MountedBundle::create(
            &executable,
            &python_arguments(
                &python,
                "import os,subprocess,sys,time\nsubprocess.Popen([sys.executable,'-c','import time\\nwhile True: time.sleep(1)'],stdin=subprocess.DEVNULL,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)\nos.write(1,b'ready\\n')\nwhile True: time.sleep(1)",
            ),
        );
        let deadline_result = runner
            .run(
                deadline_bundle.bundle(),
                &[],
                Duration::from_secs(1),
                limits,
                &idle_cancel,
            )
            .expect("deadline run");
        assert_eq!(deadline_result.foreground_status.code(), Some(137));
        assert_eq!(deadline_result.stdout.bytes, b"ready\n");
        assert_eq!(
            deadline_result.stop_reason,
            Some(RuncStopReason::DeadlineExceeded)
        );
        assert_stopped(&deadline_result);
        deadline_bundle.finish();

        let stdout_limit_bundle = MountedBundle::create(
            &executable,
            &python_arguments(
                &python,
                "import os,time\nos.write(1,b'A'*(1024*1024))\nwhile True: time.sleep(1)",
            ),
        );
        let four_bytes = RuncCaptureLimits::new(4, TEST_CAPTURE_LIMIT).expect("small limit");
        let stdout_limit_result = runner
            .run(
                stdout_limit_bundle.bundle(),
                &[],
                Duration::from_secs(10),
                four_bytes,
                &idle_cancel,
            )
            .expect("stdout-limit run");
        assert_eq!(stdout_limit_result.foreground_status.code(), Some(137));
        assert_eq!(stdout_limit_result.stdout.bytes, b"AAAA");
        assert!(stdout_limit_result.stdout.observed_bytes > 4);
        assert!(stdout_limit_result.stdout.observed_bytes <= 1024 * 1024);
        assert!(stdout_limit_result.stdout.partial);
        assert_eq!(
            stdout_limit_result.stop_reason,
            Some(RuncStopReason::StdoutLimitExceeded)
        );
        assert_stopped(&stdout_limit_result);
        stdout_limit_bundle.finish();

        let fast_stdout_limit_bundle = MountedBundle::create(
            &executable,
            &python_arguments(&python, "import os\nos.write(1,b'B'*4096)"),
        );
        let fast_stdout_limit_result = runner
            .run(
                fast_stdout_limit_bundle.bundle(),
                &[],
                Duration::from_secs(10),
                four_bytes,
                &idle_cancel,
            )
            .expect("fast stdout-limit run");
        assert_eq!(fast_stdout_limit_result.foreground_status.code(), Some(0));
        assert_eq!(fast_stdout_limit_result.stdout.bytes, b"BBBB");
        assert_eq!(fast_stdout_limit_result.stdout.observed_bytes, 4096);
        assert!(fast_stdout_limit_result.stdout.partial);
        assert_eq!(
            fast_stdout_limit_result.stop_reason,
            Some(RuncStopReason::StdoutLimitExceeded)
        );
        assert_stopped(&fast_stdout_limit_result);
        fast_stdout_limit_bundle.finish();
    }

    fn required_absolute_path(name: &str) -> PathBuf {
        let path = PathBuf::from(env::var_os(name).unwrap_or_else(|| panic!("{name} is required")));
        assert!(path.is_absolute(), "{name} must be absolute");
        path
    }

    fn python_arguments(python: &Path, script: &str) -> Vec<String> {
        vec![
            python.to_string_lossy().into_owned(),
            "-c".to_owned(),
            script.to_owned(),
        ]
    }

    fn assert_stopped(result: &RuncRunResult) {
        let state = result
            .state_before_delete
            .as_ref()
            .expect("stopped state observation");
        assert_eq!(state.status, "stopped");
        assert_eq!(state.pid, 0);
        assert!(result.operation_errors.is_empty());
        assert_eq!(result.recovery, None);
    }

    fn assert_complete_capture(capture: &RuncStreamCapture) {
        assert_eq!(capture.observed_bytes, capture.bytes.len() as u64);
        assert!(!capture.partial);
    }

    fn wait_for_path(path: &Path, timeout: Duration) {
        let deadline = checked_deadline(timeout, "fixture marker timeout").expect("deadline");
        while !path.exists() {
            assert!(Instant::now() < deadline, "fixture marker did not appear");
            thread::sleep(PROCESS_POLL_INTERVAL);
        }
    }

    struct FakeRunc {
        _directory: TempDir,
        executable: PathBuf,
    }

    impl FakeRunc {
        fn new(body: &str) -> Self {
            let directory = tempfile::tempdir().expect("fake runc directory");
            let executable = directory.path().join("runc");
            let script = format!(
                r#"#!/usr/bin/env python3
import json
import os
import sys
import time

args = sys.argv[1:]
if args == ["--version"]:
    print("runc version 1.3.6")
    print("commit: fixture")
    print("spec: 1.2.1")
    raise SystemExit(0)
if args[:1] == ["--root"]:
    args = args[2:]
command = args[0]
{body}
raise SystemExit(97)
"#
            );
            let mut file = File::create(&executable).expect("fake runc executable");
            file.write_all(script.as_bytes())
                .expect("write fake runc executable");
            file.set_permissions(fs::Permissions::from_mode(0o700))
                .expect("fake runc mode");
            file.sync_all().expect("sync fake runc executable");
            drop(file);
            Self {
                _directory: directory,
                executable,
            }
        }

        /// Probe the fake runc, waiting out `ETXTBSY`.
        ///
        /// A subprocess forked by another test thread inherits the descriptor
        /// this fixture used to write the executable, and Linux refuses to
        /// execute a file any process still holds open for writing until that
        /// child reaches `exec` and closes it.
        fn runner(&self) -> RuncRunner {
            let mut waited = Duration::ZERO;
            loop {
                match RuncRunner::probe(&self.executable, Duration::from_secs(1)) {
                    Ok(runner) => return runner,
                    Err(error)
                        if is_text_file_busy(&error) && waited < Duration::from_millis(200) =>
                    {
                        thread::sleep(Duration::from_millis(10));
                        waited += Duration::from_millis(10);
                    }
                    Err(error) => panic!("fake runc probe: {error:#}"),
                }
            }
        }
    }

    const ETXTBSY: i32 = 26;

    fn is_text_file_busy(error: &anyhow::Error) -> bool {
        error.chain().any(|cause| {
            cause
                .downcast_ref::<std::io::Error>()
                .and_then(std::io::Error::raw_os_error)
                == Some(ETXTBSY)
        })
    }

    fn test_runtime() -> RuntimeConfig {
        RuntimeConfig::load(
            br#"{
                "ociVersion":"1.2.0",
                "root":{"path":"rootfs","readonly":false},
                "process":{
                    "terminal":false,
                    "user":{"uid":0,"gid":0},
                    "args":["/bin/true"],
                    "env":[],
                    "cwd":"/",
                    "noNewPrivileges":true
                },
                "hostname":"runlab",
                "linux":{"namespaces":[]}
            }"#,
        )
        .expect("runtime config")
    }

    struct MountedBundle {
        bundle: Option<OciBundle>,
        rootfs: PathBuf,
        cgroup: PathBuf,
        mounted: bool,
    }

    impl MountedBundle {
        fn create(executable: &Path, arguments: &[String]) -> Self {
            let cgroups_path = unique_cgroups_path();
            let cgroup = Path::new("/sys/fs/cgroup").join(cgroups_path.trim_start_matches('/'));
            assert!(!cgroup.exists(), "fixture cgroup already exists");
            let runtime = authored_runtime(executable, arguments, &cgroups_path);
            let bundle = OciBundle::create(&runtime).expect("bundle");
            let rootfs = bundle.rootfs().expect("rootfs").to_path_buf();
            require_success(
                &Command::new("mount")
                    .args(["--bind", "/"])
                    .arg(&rootfs)
                    .output()
                    .expect("mount rootfs"),
                "mount rootfs",
            );
            Self {
                bundle: Some(bundle),
                rootfs,
                cgroup,
                mounted: true,
            }
        }

        fn finish(mut self) {
            assert_eq!(residual_mounts(&self.rootfs), vec![self.rootfs.clone()]);
            assert!(!self.cgroup.exists(), "runc retained its cgroup");
            require_success(
                &Command::new("umount")
                    .arg(&self.rootfs)
                    .output()
                    .expect("unmount rootfs"),
                "unmount rootfs",
            );
            self.mounted = false;
            assert!(residual_mounts(&self.rootfs).is_empty());
        }

        fn bundle(&self) -> &OciBundle {
            self.bundle.as_ref().expect("mounted bundle is open")
        }

        fn preserve_bundle(&mut self) {
            if let Some(bundle) = self.bundle.take() {
                let _ = bundle.preserve();
            }
        }
    }

    impl Drop for MountedBundle {
        fn drop(&mut self) {
            if self.mounted {
                let unmounted = Command::new("umount")
                    .args(["--lazy"])
                    .arg(&self.rootfs)
                    .output()
                    .is_ok_and(|output| output.status.success());
                if !unmounted {
                    self.preserve_bundle();
                }
            }
        }
    }

    fn authored_runtime(
        executable: &Path,
        arguments: &[String],
        cgroups_path: &str,
    ) -> RuntimeConfig {
        let directory = tempfile::tempdir().expect("spec directory");
        require_success(
            &Command::new(executable)
                .arg("spec")
                .current_dir(directory.path())
                .output()
                .expect("runc spec"),
            "runc spec",
        );
        let mut value: Value = serde_json::from_slice(
            &fs::read(directory.path().join("config.json")).expect("generated config"),
        )
        .expect("generated config JSON");
        value["ociVersion"] = json!("1.2.0");
        value["process"]["terminal"] = json!(false);
        value["process"]["args"] = json!(arguments);
        value["process"]["env"] =
            json!(["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"]);
        value["process"]["cwd"] = json!("/");
        value["process"]["noNewPrivileges"] = json!(true);
        value["root"]["path"] = json!("rootfs");
        value["root"]["readonly"] = json!(false);
        value["hostname"] = json!("runlab-runc-test");
        value["linux"]["cgroupsPath"] = json!(cgroups_path);
        RuntimeConfig::load(&serde_json::to_vec(&value).expect("runtime JSON"))
            .expect("validated runtime config")
    }

    fn unique_cgroups_path() -> String {
        let unified = fs::read_to_string("/proc/self/cgroup").expect("self cgroup");
        let outer = unified
            .lines()
            .find_map(|line| line.strip_prefix("0::"))
            .expect("unified cgroup v2 path")
            .trim_end_matches('/');
        format!("{outer}/runlab-runc-production-{}", Uuid::now_v7())
    }

    fn residual_mounts(rootfs: &Path) -> Vec<PathBuf> {
        let prefix = format!("{}/", rootfs.display());
        fs::read_to_string("/proc/self/mountinfo")
            .expect("mountinfo")
            .lines()
            .filter_map(|line| line.split_whitespace().nth(4))
            .filter(|mountpoint| {
                *mountpoint == rootfs.to_string_lossy() || mountpoint.starts_with(&prefix)
            })
            .map(PathBuf::from)
            .collect()
    }

    fn require_success(output: &std::process::Output, operation: &str) {
        assert!(
            output.status.success(),
            "{operation} failed with {}; stdout: {}; stderr: {}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
