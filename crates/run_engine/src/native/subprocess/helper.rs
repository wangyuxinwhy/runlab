use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::process::CommandExt as _;
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result as AnyResult, bail};

use super::supervisor::{
    InvocationSupervisor, SUPERVISOR_REAP_LIMIT, SupervisorLifecycle, SupervisorToken,
};
use crate::CancellationToken;

pub(in crate::native) const HELPER_OUTPUT_LIMIT: usize = 1024 * 1024;
const MIN_HELPER_START_REMAINING: Duration = Duration::from_millis(20);
const POLL_INTERVAL: Duration = Duration::from_millis(10);

#[derive(Debug)]
pub(in crate::native) struct HelperOutput {
    pub(in crate::native) status: ExitStatus,
    pub(in crate::native) stdout: Vec<u8>,
    pub(in crate::native) stderr: Vec<u8>,
}

#[derive(Debug)]
pub(in crate::native) struct HelperRunError {
    message: String,
    pub(in crate::native) supervisor_reaped: bool,
}

impl std::fmt::Display for HelperRunError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for HelperRunError {}

fn helper_run_error(error: impl std::fmt::Display, supervisor_reaped: bool) -> HelperRunError {
    HelperRunError {
        message: error.to_string(),
        supervisor_reaped,
    }
}

pub(in crate::native) fn run_helper(
    supervisor: &InvocationSupervisor,
    command: &mut Command,
    timeout: Duration,
) -> Result<HelperOutput, HelperRunError> {
    let deadline = checked_deadline(Instant::now(), timeout, "helper deadline")
        .map_err(|error| helper_run_error(format!("{error:#}"), true))?;
    run_helper_until(supervisor, command, deadline, None)
}

pub(in crate::native) fn run_helper_until(
    supervisor: &InvocationSupervisor,
    command: &mut Command,
    deadline: Instant,
    cancellation: Option<&CancellationToken>,
) -> Result<HelperOutput, HelperRunError> {
    let now = Instant::now();
    if now >= deadline {
        return Err(helper_run_error(
            "helper deadline exceeded before spawn",
            supervisor.lifecycle() == SupervisorLifecycle::Reaped,
        ));
    }
    let remaining = deadline.saturating_duration_since(now);
    if remaining < MIN_HELPER_START_REMAINING {
        return Err(helper_run_error(
            "helper deadline has insufficient time to reserve one work poll and one reap poll before spawn",
            supervisor.lifecycle() == SupervisorLifecycle::Reaped,
        ));
    }
    let reap_reserve = SUPERVISOR_REAP_LIMIT.min(remaining / 2);
    let work_deadline = deadline.checked_sub(reap_reserve).unwrap_or(now);
    if now >= work_deadline {
        return Err(helper_run_error(
            "helper deadline has no remaining bounded work interval before its reap reserve",
            supervisor.lifecycle() == SupervisorLifecycle::Reaped,
        ));
    }
    let mut helper = match RunningHelper::spawn(supervisor, command) {
        Ok(helper) => helper,
        Err(error) => {
            return Err(helper_run_error(
                format!("{error:#}"),
                supervisor.lifecycle() == SupervisorLifecycle::Reaped,
            ));
        }
    };
    loop {
        match helper.try_finish_until(deadline) {
            Ok(Some(output)) => return Ok(output),
            Ok(None) => {}
            Err(error) => {
                return match helper.terminate_until(deadline) {
                    Ok(()) => Err(helper_run_error(format!("{error:#}"), true)),
                    Err(cleanup) => Err(helper_run_error(
                        format!(
                            "helper supervision failed: {error:#}; helper termination failed: {cleanup:#}"
                        ),
                        false,
                    )),
                };
            }
        }
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            return match helper.terminate_until(deadline) {
                Ok(()) => Err(helper_run_error("helper interrupted by cancellation", true)),
                Err(error) => Err(helper_run_error(
                    format!("failed to terminate cancelled helper: {error:#}"),
                    false,
                )),
            };
        }
        if Instant::now() >= work_deadline {
            return match helper.terminate_until(deadline) {
                Ok(()) => Err(helper_run_error("helper deadline exceeded", true)),
                Err(error) => Err(helper_run_error(
                    format!("failed to terminate timed-out helper: {error:#}"),
                    false,
                )),
            };
        }
        thread::sleep(POLL_INTERVAL.min(work_deadline.saturating_duration_since(Instant::now())));
    }
}

pub(in crate::native) struct RunningHelper {
    supervisor: InvocationSupervisor,
    token: SupervisorToken,
    stdout: File,
    stderr: File,
    reaped: bool,
}

impl RunningHelper {
    pub(in crate::native) fn spawn(
        supervisor: &InvocationSupervisor,
        command: &mut Command,
    ) -> AnyResult<Self> {
        let stdout = tempfile::tempfile()?;
        let stderr = tempfile::tempfile()?;
        command
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout.try_clone()?))
            .stderr(Stdio::from(stderr.try_clone()?))
            .process_group(0);
        Ok(Self {
            token: supervisor.spawn(command).map_err(anyhow::Error::from)?,
            supervisor: supervisor.clone(),
            stdout,
            stderr,
            reaped: false,
        })
    }

    pub(in crate::native) fn try_finish(&mut self) -> AnyResult<Option<HelperOutput>> {
        let deadline = checked_deadline(
            Instant::now(),
            SUPERVISOR_REAP_LIMIT,
            "helper supervision deadline",
        )?;
        self.try_finish_until(deadline)
    }

    fn try_finish_until(&mut self, deadline: Instant) -> AnyResult<Option<HelperOutput>> {
        if Instant::now() >= deadline {
            bail!("helper supervision deadline exceeded");
        }
        let stdout_size = match self.stdout.metadata() {
            Ok(metadata) => metadata.len(),
            Err(error) => {
                self.terminate_until(deadline)
                    .context("failed to terminate helper after stdout metadata error")?;
                return Err(error.into());
            }
        };
        let stderr_size = match self.stderr.metadata() {
            Ok(metadata) => metadata.len(),
            Err(error) => {
                self.terminate_until(deadline)
                    .context("failed to terminate helper after stderr metadata error")?;
                return Err(error.into());
            }
        };
        let oversized = if stdout_size > HELPER_OUTPUT_LIMIT as u64 {
            Some("stdout")
        } else if stderr_size > HELPER_OUTPUT_LIMIT as u64 {
            Some("stderr")
        } else {
            None
        };
        if let Some(stream) = oversized {
            self.terminate_until(deadline)
                .context("failed to terminate oversized-output helper")?;
            bail!("helper {stream} output exceeds {HELPER_OUTPUT_LIMIT} bytes");
        }
        let status = match self.supervisor.try_wait(self.token) {
            Ok(Some(status)) => {
                self.reaped = true;
                self.supervisor.release_reaped(self.token)?;
                status
            }
            Ok(None) => return Ok(None),
            Err(error) => {
                self.terminate_until(deadline)
                    .context("failed to terminate helper after wait error")?;
                return Err(error.into());
            }
        };
        Ok(Some(HelperOutput {
            status,
            stdout: read_helper_output(&mut self.stdout, "stdout")?,
            stderr: read_helper_output(&mut self.stderr, "stderr")?,
        }))
    }

    pub(in crate::native) fn terminate(&mut self) -> AnyResult<()> {
        let deadline = checked_deadline(
            Instant::now(),
            SUPERVISOR_REAP_LIMIT,
            "helper reap deadline",
        )?;
        self.terminate_until(deadline)
    }

    fn terminate_until(&mut self, deadline: Instant) -> AnyResult<()> {
        loop {
            if Instant::now() >= deadline {
                bail!("helper reap deadline exceeded");
            }
            if self.supervisor.try_wait(self.token)?.is_some() {
                self.reaped = true;
                self.supervisor.release_reaped(self.token)?;
                return Ok(());
            }
            self.supervisor.progress_kill(self.token)?;
            if Instant::now() >= deadline {
                bail!(
                    "helper process group was killed but its leader was not reaped before the confirmation deadline"
                );
            }
            thread::sleep(POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())));
        }
    }

    pub(in crate::native) fn request_terminate(&mut self) -> AnyResult<()> {
        if self.supervisor.try_wait(self.token)?.is_some() {
            self.reaped = true;
            self.supervisor.release_reaped(self.token)?;
            return Ok(());
        }
        self.supervisor.progress_kill(self.token).map(|_| ())
    }

    pub(in crate::native) fn poll_reaped(&mut self) -> AnyResult<()> {
        if !self.reaped && self.supervisor.try_wait(self.token)?.is_some() {
            self.reaped = true;
            self.supervisor.release_reaped(self.token)?;
        } else if !self.reaped {
            self.supervisor.progress_kill(self.token)?;
        }
        Ok(())
    }

    pub(in crate::native) const fn is_reaped(&self) -> bool {
        self.reaped
    }
}

impl Drop for RunningHelper {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.request_terminate();
        }
    }
}

pub(in crate::native) fn terminate_child(
    supervisor: &InvocationSupervisor,
    child: &mut Option<SupervisorToken>,
    timeout: Duration,
) -> AnyResult<()> {
    let Some(token) = *child else {
        return Ok(());
    };
    if let Ok(Some(_)) = supervisor.try_wait(token) {
        supervisor.release_reaped(token)?;
        *child = None;
        return Ok(());
    }
    let deadline = checked_deadline(Instant::now(), timeout, "child reap deadline")?;
    loop {
        if supervisor.try_wait(token)?.is_some() {
            supervisor.release_reaped(token)?;
            *child = None;
            return Ok(());
        }
        supervisor.progress_kill(token)?;
        if Instant::now() >= deadline {
            bail!(
                "child process group was killed but its leader was not reaped before the confirmation deadline"
            );
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn read_helper_output(file: &mut File, stream: &str) -> AnyResult<Vec<u8>> {
    let size = file.metadata()?.len();
    if size > HELPER_OUTPUT_LIMIT as u64 {
        bail!("helper {stream} output exceeds {HELPER_OUTPUT_LIMIT} bytes");
    }
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::with_capacity(usize::try_from(size).unwrap_or(HELPER_OUTPUT_LIMIT));
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn checked_deadline(start: Instant, duration: Duration, operation: &str) -> AnyResult<Instant> {
    start
        .checked_add(duration)
        .with_context(|| format!("{operation} exceeds the monotonic clock range"))
}
