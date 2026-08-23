use std::io::{ErrorKind, Read, Write};
use std::process::{ChildStdin, Command, ExitStatus, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

const POLL_INTERVAL: Duration = Duration::from_millis(25);
const TERMINATION_GRACE: Duration = Duration::from_secs(2);

#[allow(
    clippy::too_many_lines,
    reason = "subprocess spawn, supervision, bounded I/O, and reap form one ordered lifecycle"
)]
pub(crate) fn bounded_output(
    command: &mut Command,
    input: Option<&[u8]>,
    timeout: Duration,
    output_limit: usize,
    operation: &str,
) -> Result<Output> {
    if timeout.is_zero() || output_limit == 0 {
        bail!("bounded subprocess requires positive timeout and output limit");
    }
    let deadline = Instant::now()
        .checked_add(timeout)
        .context("bounded subprocess timeout is too large")?;
    command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("failed to invoke {operation}"))?;
    let stdin = child.stdin.take();
    let Some(stdout) = child.stdout.take() else {
        terminate(&mut child, operation)?;
        bail!("{operation} stdout is unavailable");
    };
    let Some(stderr) = child.stderr.take() else {
        terminate(&mut child, operation)?;
        bail!("{operation} stderr is unavailable");
    };
    if let Some(stdin) = stdin.as_ref()
        && let Err(error) = set_nonblocking(stdin)
    {
        return Err(spawned_child_error(
            &mut child,
            anyhow::Error::new(error).context("failed to configure subprocess stdin pipe"),
        ));
    }
    if let Err(error) = set_nonblocking(&stdout) {
        return Err(spawned_child_error(
            &mut child,
            anyhow::Error::new(error).context("failed to configure subprocess stdout pipe"),
        ));
    }
    if let Err(error) = set_nonblocking(&stderr) {
        return Err(spawned_child_error(
            &mut child,
            anyhow::Error::new(error).context("failed to configure subprocess stderr pipe"),
        ));
    }
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
    let mut status = None;
    loop {
        if status.is_none() {
            match child.try_wait() {
                Ok(observed) => status = observed,
                Err(error) => {
                    stop_io.store(true, Ordering::Release);
                    let _ = terminate(&mut child, operation);
                    let _ = join_writer(writer, operation);
                    let _ = join_capture(stdout_reader, operation, "stdout");
                    let _ = join_capture(stderr_reader, operation, "stderr");
                    return Err(error).with_context(|| format!("failed to poll {operation}"));
                }
            }
        }
        if status.is_some()
            && writer.is_finished()
            && stdout_reader.is_finished()
            && stderr_reader.is_finished()
        {
            break;
        }
        if stdout_exceeded.load(Ordering::Acquire) || stderr_exceeded.load(Ordering::Acquire) {
            stop_io.store(true, Ordering::Release);
            terminate(&mut child, operation)?;
            let _ = join_writer(writer, operation);
            let _ = join_capture(stdout_reader, operation, "stdout");
            let _ = join_capture(stderr_reader, operation, "stderr");
            bail!("{operation} output exceeds the {output_limit}-byte per-stream limit");
        }
        if Instant::now() >= deadline {
            stop_io.store(true, Ordering::Release);
            terminate(&mut child, operation)?;
            let _ = join_writer(writer, operation);
            let _ = join_capture(stdout_reader, operation, "stdout");
            let _ = join_capture(stderr_reader, operation, "stderr");
            bail!(
                "{operation} exceeded its {} second deadline",
                timeout.as_secs()
            );
        }
        thread::sleep(POLL_INTERVAL);
    }
    join_writer(writer, operation)?;
    let stdout = join_capture(stdout_reader, operation, "stdout")?;
    let stderr = join_capture(stderr_reader, operation, "stderr")?;
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
    if timeout.is_zero() || stderr_limit == 0 {
        bail!("bounded subprocess requires positive timeout and stderr limit");
    }
    let deadline = Instant::now()
        .checked_add(timeout)
        .context("bounded subprocess timeout is too large")?;
    command
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("failed to invoke {operation}"))?;
    let Some(stderr) = child.stderr.take() else {
        terminate(&mut child, operation)?;
        bail!("{operation} stderr is unavailable");
    };
    if let Err(error) = set_nonblocking(&stderr) {
        return Err(spawned_child_error(
            &mut child,
            anyhow::Error::new(error).context("failed to configure subprocess stderr pipe"),
        ));
    }
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
    let mut status = None;
    loop {
        if status.is_none() {
            match child.try_wait() {
                Ok(observed) => status = observed,
                Err(error) => {
                    stop_io.store(true, Ordering::Release);
                    let _ = terminate(&mut child, operation);
                    let _ = join_capture(stderr_reader, operation, "stderr");
                    return Err(error).with_context(|| format!("failed to poll {operation}"));
                }
            }
        }
        if status.is_some() && stderr_reader.is_finished() {
            break;
        }
        if stderr_exceeded.load(Ordering::Acquire) {
            stop_io.store(true, Ordering::Release);
            terminate(&mut child, operation)?;
            let _ = join_capture(stderr_reader, operation, "stderr");
            bail!("{operation} stderr exceeds the {stderr_limit}-byte limit");
        }
        if Instant::now() >= deadline {
            stop_io.store(true, Ordering::Release);
            terminate(&mut child, operation)?;
            let _ = join_capture(stderr_reader, operation, "stderr");
            bail!(
                "{operation} exceeded its {} second deadline",
                timeout.as_secs()
            );
        }
        thread::sleep(POLL_INTERVAL);
    }
    let stderr = join_capture(stderr_reader, operation, "stderr")?;
    if stderr_exceeded.load(Ordering::Acquire) {
        bail!("{operation} stderr exceeds the {stderr_limit}-byte limit");
    }
    Ok(Output {
        status: status.context("subprocess completed without an exit status")?,
        stdout: Vec::new(),
        stderr,
    })
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

#[cfg(unix)]
fn set_nonblocking(fd: impl std::os::fd::AsFd) -> std::io::Result<()> {
    use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};

    let flags = fcntl_getfl(&fd)?;
    Ok(fcntl_setfl(fd, flags | OFlags::NONBLOCK)?)
}

#[cfg(not(unix))]
fn set_nonblocking(_fd: impl std::os::fd::AsFd) -> std::io::Result<()> {
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

fn spawned_child_error(child: &mut std::process::Child, error: anyhow::Error) -> anyhow::Error {
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
