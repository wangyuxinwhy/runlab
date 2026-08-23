//! Running child processes under a bound, and the names under which this
//! binary re-invokes itself.
//!
//! A few operations need a separate process rather than a thread: holding a
//! network namespace open, or probing a port from inside one. Those hidden
//! subcommands are named here so the `clap` declaration and the code that
//! spawns them cannot drift apart — they compile against the same constant.

use std::io::{ErrorKind, Read, Write};
use std::process::{ChildStdin, Command, ExitStatus, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

/// Keeps a Run network namespace alive for the lifetime of the Run.
pub(crate) const NETWORK_HOLDER_COMMAND: &str = "__internal-network-holder";

/// Probes a Managed Service readiness port from inside the Run network
/// namespace.
pub(crate) const TCP_PROBE_COMMAND: &str = "__internal-tcp-probe";

const POLL_INTERVAL: Duration = Duration::from_millis(25);
const TERMINATION_GRACE: Duration = Duration::from_secs(2);

pub(crate) fn bounded_output(
    command: &mut Command,
    input: Option<&[u8]>,
    timeout: Duration,
    output_limit: usize,
    operation: &str,
) -> Result<Output> {
    let stdin = if input.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    };
    let (mut child, deadline) = spawn_bounded(
        command,
        stdin,
        Stdio::piped(),
        timeout,
        output_limit,
        operation,
    )?;
    let stdin = child.stdin.take();
    if let Some(stdin) = stdin.as_ref() {
        configure_pipe(stdin, &mut child, "stdin")?;
    }
    let stdout = child.stdout.take();
    let stdout = take_pipe(stdout, &mut child, operation, "stdout")?;
    let stderr = child.stderr.take();
    let stderr = take_pipe(stderr, &mut child, operation, "stderr")?;
    let stop_io = Arc::new(AtomicBool::new(false));
    let stdout_exceeded = Arc::new(AtomicBool::new(false));
    let stderr_exceeded = Arc::new(AtomicBool::new(false));
    let stdout_reader = capture(
        stdout,
        output_limit,
        Arc::clone(&stdout_exceeded),
        Arc::clone(&stop_io),
        operation,
        "stdout",
    );
    let stderr_reader = capture(
        stderr,
        output_limit,
        Arc::clone(&stderr_exceeded),
        Arc::clone(&stop_io),
        operation,
        "stderr",
    );
    let input = input.map(ToOwned::to_owned);
    let writer_stop = Arc::clone(&stop_io);
    let writer = thread::spawn(move || write_input(stdin, input.as_deref(), &writer_stop));
    let pumps = Pumps {
        stop: stop_io,
        writer: Some(writer),
        stdout: Some(stdout_reader),
        stderr: stderr_reader,
        operation: operation.to_owned(),
    };
    let mut status = None;
    loop {
        if status.is_none() {
            match child.try_wait() {
                Ok(observed) => status = observed,
                Err(error) => {
                    let reason =
                        anyhow::Error::new(error).context(format!("failed to poll {operation}"));
                    return Err(pumps.abandon(&mut child, reason));
                }
            }
        }
        if status.is_some() && pumps.are_drained() {
            break;
        }
        if stdout_exceeded.load(Ordering::Acquire) || stderr_exceeded.load(Ordering::Acquire) {
            return Err(pumps.abandon(
                &mut child,
                anyhow::anyhow!(
                    "{operation} output exceeds the {output_limit}-byte per-stream limit"
                ),
            ));
        }
        if Instant::now() >= deadline {
            return Err(pumps.abandon(
                &mut child,
                anyhow::anyhow!(
                    "{operation} exceeded its {} second deadline",
                    timeout.as_secs()
                ),
            ));
        }
        thread::sleep(POLL_INTERVAL);
    }
    let (stdout, stderr) = pumps.join()?;
    if stdout_exceeded.load(Ordering::Acquire) || stderr_exceeded.load(Ordering::Acquire) {
        bail!("{operation} output exceeds the {output_limit}-byte per-stream limit");
    }
    Ok(Output {
        status: status.context("subprocess completed without an exit status")?,
        stdout,
        stderr,
    })
}

pub(crate) fn bounded_status_with_stdout(
    command: &mut Command,
    stdout: Stdio,
    timeout: Duration,
    stderr_limit: usize,
    operation: &str,
) -> Result<Output> {
    let (mut child, deadline) = spawn_bounded(
        command,
        Stdio::null(),
        stdout,
        timeout,
        stderr_limit,
        operation,
    )?;
    let stderr = child.stderr.take();
    let stderr = take_pipe(stderr, &mut child, operation, "stderr")?;
    let stop_io = Arc::new(AtomicBool::new(false));
    let stderr_exceeded = Arc::new(AtomicBool::new(false));
    let stderr_reader = capture(
        stderr,
        stderr_limit,
        Arc::clone(&stderr_exceeded),
        Arc::clone(&stop_io),
        operation,
        "stderr",
    );
    let pumps = Pumps {
        stop: stop_io,
        writer: None,
        stdout: None,
        stderr: stderr_reader,
        operation: operation.to_owned(),
    };
    let mut status = None;
    loop {
        if status.is_none() {
            match child.try_wait() {
                Ok(observed) => status = observed,
                Err(error) => {
                    let reason =
                        anyhow::Error::new(error).context(format!("failed to poll {operation}"));
                    return Err(pumps.abandon(&mut child, reason));
                }
            }
        }
        if status.is_some() && pumps.are_drained() {
            break;
        }
        if stderr_exceeded.load(Ordering::Acquire) {
            return Err(pumps.abandon(
                &mut child,
                anyhow::anyhow!("{operation} stderr exceeds the {stderr_limit}-byte limit"),
            ));
        }
        if Instant::now() >= deadline {
            return Err(pumps.abandon(
                &mut child,
                anyhow::anyhow!(
                    "{operation} exceeded its {} second deadline",
                    timeout.as_secs()
                ),
            ));
        }
        thread::sleep(POLL_INTERVAL);
    }
    let (_, stderr) = pumps.join()?;
    if stderr_exceeded.load(Ordering::Acquire) {
        bail!("{operation} stderr exceeds the {stderr_limit}-byte limit");
    }
    Ok(Output {
        status: status.context("subprocess completed without an exit status")?,
        stdout: Vec::new(),
        stderr,
    })
}

/// Start `command` in its own process group, with its deadline.
///
/// The group is what lets termination reach a helper's own children instead of
/// only the process this crate spawned.
fn spawn_bounded(
    command: &mut Command,
    stdin: Stdio,
    stdout: Stdio,
    timeout: Duration,
    limit: usize,
    operation: &str,
) -> Result<(std::process::Child, Instant)> {
    if timeout.is_zero() || limit == 0 {
        bail!("bounded subprocess requires positive timeout and output limit");
    }
    let deadline = Instant::now()
        .checked_add(timeout)
        .context("bounded subprocess timeout is too large")?;
    command.stdin(stdin).stdout(stdout).stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    let child = command
        .spawn()
        .with_context(|| format!("failed to invoke {operation}"))?;
    Ok((child, deadline))
}

/// Take one of the child's pipes, in non-blocking mode.
///
/// The capture threads poll instead of blocking in `read`, so a child that
/// outlives its deadline cannot wedge them after it has been killed.
fn take_pipe<T: std::os::fd::AsFd>(
    pipe: Option<T>,
    child: &mut std::process::Child,
    operation: &str,
    stream: &str,
) -> Result<T> {
    let Some(pipe) = pipe else {
        terminate(child, operation)?;
        bail!("{operation} {stream} is unavailable");
    };
    configure_pipe(&pipe, child, stream)?;
    Ok(pipe)
}

fn configure_pipe(
    pipe: impl std::os::fd::AsFd,
    child: &mut std::process::Child,
    stream: &str,
) -> Result<()> {
    if let Err(error) = set_nonblocking(pipe) {
        return Err(spawned_child_error(
            child,
            anyhow::Error::new(error)
                .context(format!("failed to configure subprocess {stream} pipe")),
        ));
    }
    Ok(())
}

/// The threads pumping a child's pipes, and the flag that stops them.
///
/// Keeping them together is what lets every failure path tear down the same
/// way: stop the pumps, stop the child, drain the threads, and report the
/// reason the caller already has. A teardown problem must not be reported in
/// place of the deadline or limit that caused it.
struct Pumps {
    stop: Arc<AtomicBool>,
    writer: Option<JoinHandle<Result<()>>>,
    stdout: Option<JoinHandle<Result<Vec<u8>>>>,
    stderr: JoinHandle<Result<Vec<u8>>>,
    operation: String,
}

impl Pumps {
    fn are_drained(&self) -> bool {
        self.writer.as_ref().is_none_or(JoinHandle::is_finished)
            && self.stdout.as_ref().is_none_or(JoinHandle::is_finished)
            && self.stderr.is_finished()
    }

    fn abandon(self, child: &mut std::process::Child, reason: anyhow::Error) -> anyhow::Error {
        self.stop.store(true, Ordering::Release);
        let operation = self.operation.clone();
        let _ = terminate(child, &operation);
        let _ = self.join();
        reason
    }

    fn join(self) -> Result<(Vec<u8>, Vec<u8>)> {
        if let Some(writer) = self.writer {
            join_writer(writer, &self.operation)?;
        }
        let stdout = self
            .stdout
            .map(|handle| join_capture(handle, &self.operation, "stdout"))
            .transpose()?
            .unwrap_or_default();
        let stderr = join_capture(self.stderr, &self.operation, "stderr")?;
        Ok((stdout, stderr))
    }
}

fn capture(
    mut reader: impl Read + Send + 'static,
    limit: usize,
    exceeded: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    operation: &str,
    stream: &'static str,
) -> JoinHandle<Result<Vec<u8>>> {
    let operation = operation.to_owned();
    thread::spawn(move || {
        let mut bytes = Vec::with_capacity(limit.min(1024 * 1024));
        let mut buffer = [0_u8; 16 * 1024];
        loop {
            if stop.load(Ordering::Acquire) {
                return Ok(bytes);
            }
            let read = match reader.read(&mut buffer) {
                Ok(read) => read,
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    thread::sleep(POLL_INTERVAL);
                    continue;
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("failed to read {operation} {stream}"));
                }
            };
            if read == 0 {
                return Ok(bytes);
            }
            let remaining = limit.saturating_sub(bytes.len());
            bytes.extend_from_slice(&buffer[..read.min(remaining)]);
            if read > remaining {
                exceeded.store(true, Ordering::Release);
            }
        }
    })
}

fn write_input(
    mut stdin: Option<ChildStdin>,
    input: Option<&[u8]>,
    stop: &AtomicBool,
) -> Result<()> {
    let (Some(stdin), Some(input)) = (&mut stdin, input) else {
        return Ok(());
    };
    let mut written = 0;
    while written < input.len() {
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }
        match stdin.write(&input[written..]) {
            Ok(0) => bail!("subprocess stdin closed without accepting input"),
            Ok(size) => written += size,
            Err(error) if error.kind() == ErrorKind::WouldBlock => thread::sleep(POLL_INTERVAL),
            Err(error) if error.kind() == ErrorKind::BrokenPipe => return Ok(()),
            Err(error) => return Err(error).context("failed to write subprocess stdin"),
        }
    }
    Ok(())
}

/// Put a pipe in non-blocking mode.
///
/// Every capture thread in this crate polls rather than blocking in `read`, so
/// a child that outlives its deadline cannot wedge one after it is killed.
#[cfg(unix)]
pub(crate) fn set_nonblocking(fd: impl std::os::fd::AsFd) -> std::io::Result<()> {
    use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};

    let flags = fcntl_getfl(&fd)?;
    Ok(fcntl_setfl(fd, flags | OFlags::NONBLOCK)?)
}

#[cfg(not(unix))]
pub(crate) fn set_nonblocking(_fd: impl std::os::fd::AsFd) -> std::io::Result<()> {
    Ok(())
}

fn join_writer(handle: JoinHandle<Result<()>>, operation: &str) -> Result<()> {
    handle
        .join()
        .map_err(|_| anyhow::anyhow!("{operation} stdin writer panicked"))?
}

fn join_capture(
    handle: JoinHandle<Result<Vec<u8>>>,
    operation: &str,
    stream: &str,
) -> Result<Vec<u8>> {
    handle
        .join()
        .map_err(|_| anyhow::anyhow!("{operation} {stream} reader panicked"))?
}

/// Reap a child this crate just spawned, keeping `error` as the reported
/// failure unless the reap itself fails.
pub(crate) fn spawned_child_error(
    child: &mut std::process::Child,
    error: anyhow::Error,
) -> anyhow::Error {
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

fn terminate(child: &mut std::process::Child, operation: &str) -> Result<ExitStatus> {
    signal_group(child, false);
    let deadline = Instant::now()
        .checked_add(TERMINATION_GRACE)
        .context("subprocess termination grace is too large")?;
    loop {
        if let Some(status) = child
            .try_wait()
            .with_context(|| format!("failed to poll terminating {operation}"))?
        {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            signal_group(child, true);
            child
                .kill()
                .or_else(|error| {
                    child
                        .try_wait()
                        .and_then(|status| status.map_or(Err(error), |_| Ok(())))
                })
                .with_context(|| format!("failed to kill {operation}"))?;
            return child
                .wait()
                .with_context(|| format!("failed to reap {operation}"));
        }
        thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(unix)]
fn signal_group(child: &std::process::Child, force: bool) {
    let Ok(raw_pid) = i32::try_from(child.id()) else {
        return;
    };
    let Some(pid) = rustix::process::Pid::from_raw(raw_pid) else {
        return;
    };
    let signal = if force {
        rustix::process::Signal::KILL
    } else {
        rustix::process::Signal::TERM
    };
    let _ = rustix::process::kill_process_group(pid, signal);
}

#[cfg(not(unix))]
fn signal_group(_child: &std::process::Child, _force: bool) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn enforces_deadline_and_output_limit() {
        let timeout = bounded_output(
            Command::new("sh").args(["-c", "sleep 30"]),
            None,
            Duration::from_millis(50),
            1024,
            "timeout fixture",
        )
        .expect_err("timeout");
        assert!(timeout.to_string().contains("deadline"));

        let output = bounded_output(
            Command::new("sh").args(["-c", "printf 12345"]),
            None,
            Duration::from_secs(2),
            4,
            "output fixture",
        )
        .expect_err("output limit");
        assert!(output.to_string().contains("output exceeds"));
    }

    #[cfg(unix)]
    #[test]
    fn deadline_includes_pipes_inherited_by_descendants() {
        let started = Instant::now();
        let error = bounded_output(
            Command::new("sh").args(["-c", "sleep 30 &"]),
            None,
            Duration::from_millis(100),
            1024,
            "inherited pipe fixture",
        )
        .expect_err("inherited pipe must remain bounded");
        assert!(error.to_string().contains("deadline"));
        assert!(started.elapsed() < Duration::from_secs(3));
    }
}
