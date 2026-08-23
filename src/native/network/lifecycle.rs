use std::ffi::OsStr;
use std::fs;
use std::io::{self, Read as _};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::core::RunId;

use super::{
    EgressNetworkTools, HostNetworkLock, NETWORK_HOLDER_DIRECTORY, NativeNetworkIdentity,
    NativeNetworkTools, NetworkHolderHandle, NetworkHolderIdentity, POLL_INTERVAL, RunNetworkMode,
    RunNetworkPlan, RunNetworkResources, contextual, deadline, force_reap, helper_failure,
    invalid_data, invalid_input, join_capture, namespace_identity, other, read_bounded, remaining,
    run_bounded, sleep_until, validate_executable,
};

#[derive(Debug)]
pub(crate) struct EgressNetworkAttachment {
    tools: EgressNetworkTools,
    plan: RunNetworkPlan,
    timeout: Duration,
    finished: bool,
}

impl EgressNetworkAttachment {
    pub(crate) fn finish(mut self) -> io::Result<()> {
        self.shutdown()
    }

    fn shutdown(&mut self) -> io::Result<()> {
        if self.finished {
            return Ok(());
        }
        self.tools.cleanup_plan(&self.plan, self.timeout)?;
        self.finished = true;
        Ok(())
    }
}

impl Drop for EgressNetworkAttachment {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

#[derive(Debug)]
pub(crate) struct RunNetwork {
    resources: RunNetworkResources,
    egress: Option<EgressNetworkAttachment>,
    shared: Option<SharedLoopbackNetwork>,
}

impl RunNetwork {
    pub(crate) fn start_from_persisted_plan_durable(
        plan: &RunNetworkPlan,
        holder: NetworkHolderHandle,
        native_tools: NativeNetworkTools,
        egress_tools: Option<EgressNetworkTools>,
        host_lock: Option<&HostNetworkLock>,
        helper_timeout: Duration,
    ) -> io::Result<Self> {
        plan.validate()?;
        if plan.run_id() != holder.run_id {
            return Err(invalid_input(
                "Run network plan and holder handle belong to different Runs",
            ));
        }
        match (plan.mode(), egress_tools.as_ref(), host_lock) {
            (RunNetworkMode::LoopbackOnly, None, None) => {}
            (RunNetworkMode::EgressIpv4, Some(tools), Some(_)) => {
                plan.egress()?;
                tools.preflight(helper_timeout)?;
            }
            (RunNetworkMode::LoopbackOnly, Some(_), _) => {
                return Err(invalid_input(
                    "loopback-only Run network plan must not receive egress tools",
                ));
            }
            (RunNetworkMode::LoopbackOnly, None, Some(_)) => {
                return Err(invalid_input(
                    "loopback-only Run network plan must not receive a host network lock",
                ));
            }
            (RunNetworkMode::EgressIpv4, None, _) => {
                return Err(invalid_input(
                    "IPv4 egress Run network plan requires egress tools",
                ));
            }
            (RunNetworkMode::EgressIpv4, Some(_), None) => {
                return Err(invalid_input(
                    "IPv4 egress Run network plan requires the allocation lock",
                ));
            }
        }
        let mut shared =
            SharedLoopbackNetwork::start_durable(native_tools, holder, helper_timeout)?;
        let resources = shared.resources(plan.clone())?;
        let egress = match egress_tools {
            Some(tools) => Some(shared.attach_egress(
                tools,
                plan.clone(),
                host_lock.expect("egress host lock was validated"),
                helper_timeout,
            )?),
            None => None,
        };
        Ok(Self {
            resources,
            egress,
            shared: Some(shared),
        })
    }

    #[must_use]
    pub(crate) fn resources(&self) -> &RunNetworkResources {
        &self.resources
    }

    pub(crate) fn binding(&mut self) -> io::Result<NativeNetworkBinding> {
        self.shared
            .as_mut()
            .ok_or_else(|| other("Run network is already finished"))?
            .binding()
    }

    pub(crate) fn finish(mut self) -> io::Result<()> {
        self.shutdown()
    }

    fn shutdown(&mut self) -> io::Result<()> {
        let egress = self
            .egress
            .take()
            .map_or(Ok(()), EgressNetworkAttachment::finish);
        let shared = self
            .shared
            .take()
            .map_or(Ok(()), SharedLoopbackNetwork::finish);
        match (egress, shared) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
            (Err(egress), Err(shared)) => Err(other(format!(
                "Run egress cleanup failed: {egress}; network namespace cleanup also failed: {shared}"
            ))),
        }
    }
}

impl Drop for RunNetwork {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LinkOwnership {
    Absent,
    Owned,
    CreatePending,
    Foreign,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TableOwnership {
    Absent,
    Owned,
    Foreign,
}

#[derive(Debug)]
pub(crate) struct NativeNetworkHelperOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

#[derive(Debug, Clone)]
pub(crate) struct NativeNetworkBinding {
    nsenter: PathBuf,
    namespace_path: PathBuf,
    identity: NativeNetworkIdentity,
}

impl NativeNetworkBinding {
    pub(crate) fn entered_command(&self, executable: impl AsRef<Path>) -> io::Result<Command> {
        // The coordinator retains the holder until every command built from this binding exits.
        self.verify_identity()?;
        let executable = validate_executable(executable.as_ref())?;
        let mut command = Command::new(&self.nsenter);
        command
            .arg(format!("--net={}", self.namespace_path.display()))
            .arg("--")
            .arg(executable);
        Ok(command)
    }

    pub(crate) fn invoke<I, S>(
        &self,
        executable: impl AsRef<Path>,
        arguments: I,
        timeout: Duration,
    ) -> io::Result<NativeNetworkHelperOutput>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut command = self.entered_command(executable)?;
        command.args(arguments);
        run_bounded(command, timeout)
    }

    fn verify_identity(&self) -> io::Result<()> {
        let actual = namespace_identity(&self.namespace_path)
            .map_err(|error| contextual(&error, "failed to inspect private network namespace"))?;
        if actual.device != self.identity.namespace_device
            || actual.inode != self.identity.namespace_inode
        {
            return Err(other("private network namespace identity changed"));
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct SharedLoopbackNetwork {
    tools: NativeNetworkTools,
    helper_timeout: Duration,
    holder: Child,
    holder_stdin: Option<ChildStdin>,
    holder_stderr: Option<JoinHandle<io::Result<Vec<u8>>>>,
    identity: NativeNetworkIdentity,
    holder_start_time_ticks: u64,
    namespace_path: PathBuf,
    durable_holder: Option<NetworkHolderHandle>,
    finished: bool,
}

struct StartingNetworkHolder {
    child: Child,
    stdin: ChildStdin,
    stderr: JoinHandle<io::Result<Vec<u8>>>,
    start_time_ticks: u64,
    namespace_path: PathBuf,
}

impl SharedLoopbackNetwork {
    #[cfg(test)]
    pub(crate) fn start(tools: NativeNetworkTools, helper_timeout: Duration) -> io::Result<Self> {
        Self::start_inner(tools, None, helper_timeout)
    }

    pub(crate) fn start_durable(
        tools: NativeNetworkTools,
        holder: NetworkHolderHandle,
        helper_timeout: Duration,
    ) -> io::Result<Self> {
        holder.validate_directory()?;
        if holder.stop_requested()? {
            return Err(other("network holder already has a durable stop request"));
        }
        if holder.read_identity()?.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "network holder identity already exists",
            ));
        }
        Self::start_inner(tools, Some(holder), helper_timeout)
    }

    fn start_inner(
        tools: NativeNetworkTools,
        durable_holder: Option<NetworkHolderHandle>,
        helper_timeout: Duration,
    ) -> io::Result<Self> {
        let deadline = deadline(helper_timeout, "network helper timeout")?;
        let host_identity = namespace_identity(Path::new("/proc/self/ns/net"))?;
        let starting = start_network_holder(&tools, durable_holder.as_ref())?;
        let mut holder = starting.child;
        let holder_stdin = starting.stdin;
        let holder_stderr = starting.stderr;
        let holder_start_time_ticks = starting.start_time_ticks;
        let namespace_path = starting.namespace_path;

        let identity = loop {
            let status = match holder.try_wait() {
                Ok(status) => status,
                Err(error) => {
                    let _ = force_reap(&mut holder)?;
                    let _ = join_capture(holder_stderr);
                    return Err(contextual(
                        &error,
                        "failed to poll private network holder during startup",
                    ));
                }
            };
            if let Some(status) = status {
                let stderr = join_capture(holder_stderr)?;
                return Err(other(format!(
                    "private network holder exited during startup with {status}; stderr: {}",
                    String::from_utf8_lossy(&stderr)
                )));
            }
            match namespace_identity(&namespace_path) {
                Ok(identity) if identity != host_identity => {
                    let identity = NativeNetworkIdentity {
                        namespace_device: identity.device,
                        namespace_inode: identity.inode,
                    };
                    if let Some(handle) = durable_holder.as_ref() {
                        match handle.read_identity()? {
                            Some(recorded)
                                if recorded.pid == holder.id()
                                    && recorded.start_time_ticks == holder_start_time_ticks
                                    && recorded.namespace == identity => {}
                            Some(_) => {
                                let _ = force_reap(&mut holder)?;
                                let _ = join_capture(holder_stderr);
                                return Err(invalid_data(
                                    "network holder sidecar does not match its live process",
                                ));
                            }
                            None => {
                                if Instant::now() >= deadline {
                                    let _ = force_reap(&mut holder)?;
                                    let _ = join_capture(holder_stderr);
                                    return Err(other(
                                        "network holder did not publish its durable identity before the helper timeout",
                                    ));
                                }
                                sleep_until(deadline);
                                continue;
                            }
                        }
                    }
                    break identity;
                }
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    let _ = force_reap(&mut holder)?;
                    let _ = join_capture(holder_stderr);
                    return Err(contextual(
                        &error,
                        "failed to inspect private network namespace during startup",
                    ));
                }
            }
            if Instant::now() >= deadline {
                let _ = force_reap(&mut holder)?;
                let stderr = join_capture(holder_stderr)?;
                return Err(other(format!(
                    "private network holder did not create a distinct namespace before the helper timeout; stderr: {}",
                    String::from_utf8_lossy(&stderr)
                )));
            }
            sleep_until(deadline);
        };

        let mut network = Self {
            tools,
            helper_timeout,
            holder,
            holder_stdin: Some(holder_stdin),
            holder_stderr: Some(holder_stderr),
            identity,
            holder_start_time_ticks,
            namespace_path,
            durable_holder,
            finished: false,
        };
        network.enable_loopback(deadline)?;
        Ok(network)
    }

    fn enable_loopback(&mut self, deadline: Instant) -> io::Result<()> {
        let ip = self.tools.ip.clone();
        let binding = self.binding()?;
        let output = binding.invoke(
            ip,
            ["link", "set", "dev", "lo", "up"],
            remaining(deadline, "network helper timeout")?,
        )?;
        if !output.status.success() {
            return Err(helper_failure("failed to enable private loopback", &output));
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn identity(&self) -> &NativeNetworkIdentity {
        &self.identity
    }

    pub(crate) fn holder_identity(&self) -> (u32, u64) {
        (self.holder.id(), self.holder_start_time_ticks)
    }

    pub(crate) fn binding(&mut self) -> io::Result<NativeNetworkBinding> {
        self.verify_holder()?;
        Ok(NativeNetworkBinding {
            nsenter: self.tools.nsenter.clone(),
            namespace_path: self.namespace_path.clone(),
            identity: self.identity.clone(),
        })
    }

    pub(crate) fn resources(&mut self, plan: RunNetworkPlan) -> io::Result<RunNetworkResources> {
        self.verify_holder()?;
        let (holder_pid, holder_start_time_ticks) = self.holder_identity();
        Ok(RunNetworkResources {
            plan,
            namespace: self.identity.clone(),
            holder_pid,
            holder_start_time_ticks,
        })
    }

    pub(crate) fn attach_egress(
        &mut self,
        tools: EgressNetworkTools,
        plan: RunNetworkPlan,
        host_lock: &HostNetworkLock,
        timeout: Duration,
    ) -> io::Result<EgressNetworkAttachment> {
        plan.egress()?;
        tools.preflight(timeout)?;
        if let Err(error) = tools.apply(plan.egress()?, self, timeout) {
            return match tools.cleanup_plan_locked(&plan, timeout, host_lock) {
                Ok(()) => Err(error),
                Err(cleanup) => Err(other(format!(
                    "{error}; Run egress rollback also failed: {cleanup}"
                ))),
            };
        }
        Ok(EgressNetworkAttachment {
            tools,
            plan,
            timeout,
            finished: false,
        })
    }

    pub(crate) fn finish(mut self) -> io::Result<()> {
        self.shutdown()
    }

    fn verify_holder(&mut self) -> io::Result<()> {
        if self.finished {
            return Err(other("private network holder is already finished"));
        }
        // A live unreaped holder owns the PID while bindings use its procfs namespace path.
        if let Some(status) = self
            .holder
            .try_wait()
            .map_err(|error| contextual(&error, "failed to poll private network holder"))?
        {
            return Err(other(format!(
                "private network holder exited unexpectedly with {status}"
            )));
        }
        let actual = namespace_identity(&self.namespace_path)
            .map_err(|error| contextual(&error, "failed to inspect private network namespace"))?;
        if actual.device != self.identity.namespace_device
            || actual.inode != self.identity.namespace_inode
        {
            return Err(other("private network namespace identity changed"));
        }
        Ok(())
    }

    fn shutdown(&mut self) -> io::Result<()> {
        if self.finished {
            return Ok(());
        }
        let deadline = deadline(self.helper_timeout, "network holder shutdown timeout")?;
        if let Some(holder) = self.durable_holder.as_ref() {
            holder.publish_stop()?;
        }
        drop(self.holder_stdin.take());
        let status = loop {
            match self.holder.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {}
                Err(error) => {
                    if let Err(cleanup_error) = force_reap(&mut self.holder) {
                        return Err(other(format!(
                            "failed to poll private network holder during shutdown: {error}; cleanup also failed: {cleanup_error}"
                        )));
                    }
                    self.finished = true;
                    let stderr = match self.holder_stderr.take() {
                        Some(capture) => join_capture(capture)?,
                        None => Vec::new(),
                    };
                    return Err(other(format!(
                        "failed to poll private network holder during shutdown: {error}; stderr: {}",
                        String::from_utf8_lossy(&stderr)
                    )));
                }
            }
            if Instant::now() >= deadline {
                break force_reap(&mut self.holder)?;
            }
            sleep_until(deadline);
        };
        self.finished = true;
        let stderr = match self.holder_stderr.take() {
            Some(capture) => join_capture(capture)?,
            None => Vec::new(),
        };
        if !status.success() {
            return Err(other(format!(
                "private network holder exited with {status}; stderr: {}",
                String::from_utf8_lossy(&stderr)
            )));
        }
        Ok(())
    }
}

impl Drop for SharedLoopbackNetwork {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

fn start_network_holder(
    tools: &NativeNetworkTools,
    durable: Option<&NetworkHolderHandle>,
) -> io::Result<StartingNetworkHolder> {
    let mut command = Command::new(&tools.unshare);
    command.arg("--net").arg("--");
    match durable {
        Some(handle) => {
            command
                .arg(&tools.holder_executable)
                .arg("__internal-network-holder")
                .arg("--directory")
                .arg(&handle.directory)
                .arg("--run-id")
                .arg(handle.run_id.to_string());
        }
        None => {
            command.arg(&tools.cat);
        }
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| contextual(&error, "failed to start private network holder"))?;
    let pid = child.id();
    let start_time_ticks = match process_start_time_ticks(pid) {
        Ok(value) => value,
        Err(error) => {
            let _ = force_reap(&mut child)?;
            return Err(error);
        }
    };
    let Some(stdin) = child.stdin.take() else {
        let _ = force_reap(&mut child)?;
        return Err(invalid_data(
            "private network holder did not expose its stdin pipe",
        ));
    };
    let Some(stderr) = child.stderr.take() else {
        drop(stdin);
        let _ = force_reap(&mut child)?;
        return Err(invalid_data(
            "private network holder did not expose its stderr pipe",
        ));
    };
    Ok(StartingNetworkHolder {
        child,
        stdin,
        stderr: thread::spawn(move || read_bounded(stderr)),
        start_time_ticks,
        namespace_path: PathBuf::from(format!("/proc/{pid}/ns/net")),
    })
}

pub(crate) fn hold_network_namespace(directory: &Path, run_id: RunId) -> io::Result<()> {
    use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};

    let workspace = directory
        .parent()
        .ok_or_else(|| invalid_input("network holder directory has no attempt workspace"))?;
    if directory.file_name() != Some(OsStr::new(NETWORK_HOLDER_DIRECTORY)) {
        return Err(invalid_input(
            "network holder directory has an invalid name",
        ));
    }
    let handle = NetworkHolderHandle::open(workspace, run_id)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "network holder directory is missing",
        )
    })?;
    if handle.directory != directory {
        return Err(invalid_input("network holder directory is not canonical"));
    }
    disable_ipv6_in_current_namespace()?;
    let identity = NetworkHolderIdentity::current(run_id)?;
    handle.publish_identity(&identity)?;

    let stdin = io::stdin();
    let mut stdin = stdin.lock();
    let flags = fcntl_getfl(&stdin)?;
    fcntl_setfl(&stdin, flags | OFlags::NONBLOCK)?;
    let mut byte = [0_u8; 1];
    loop {
        if handle.stop_requested()? {
            return Ok(());
        }
        match stdin.read(&mut byte) {
            Ok(0) => return Ok(()),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => {
                return Err(contextual(
                    &error,
                    "failed to monitor network holder supervisor",
                ));
            }
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn disable_ipv6_in_current_namespace() -> io::Result<()> {
    for path in [
        "/proc/sys/net/ipv6/conf/default/disable_ipv6",
        "/proc/sys/net/ipv6/conf/all/disable_ipv6",
    ] {
        disable_ipv6_at(Path::new(path))?;
    }
    Ok(())
}

pub(super) fn disable_ipv6_for_interface(interface: &str) -> io::Result<()> {
    disable_ipv6_at(
        &Path::new("/proc/sys/net/ipv6/conf")
            .join(interface)
            .join("disable_ipv6"),
    )
}

fn disable_ipv6_at(path: &Path) -> io::Result<()> {
    match fs::write(path, b"1\n") {
        Ok(()) => {}
        Err(error)
            if error.kind() == io::ErrorKind::NotFound
                && !Path::new("/proc/sys/net/ipv6").exists() =>
        {
            return Ok(());
        }
        Err(error) => {
            return Err(contextual(
                &error,
                format!("failed to disable IPv6 at {}", path.display()),
            ));
        }
    }
    let value = fs::read_to_string(path).map_err(|error| {
        contextual(
            &error,
            format!("failed to verify IPv6 state at {}", path.display()),
        )
    })?;
    if value.trim() != "1" {
        return Err(other(format!(
            "IPv6 remained enabled at {}",
            path.display()
        )));
    }
    Ok(())
}

pub(crate) fn connect_loopback_tcp(port: u16, timeout: Duration) -> io::Result<()> {
    if port == 0 {
        return Err(invalid_input(
            "TCP readiness port must be greater than zero",
        ));
    }
    if timeout.is_zero() {
        return Err(invalid_input(
            "TCP readiness timeout must be greater than zero",
        ));
    }
    let address = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port));
    TcpStream::connect_timeout(&address, timeout).map(|_| ())
}

pub(super) fn process_start_time_ticks(pid: u32) -> io::Result<u64> {
    let path = PathBuf::from(format!("/proc/{pid}/stat"));
    let value = fs::read_to_string(&path)
        .map_err(|error| contextual(&error, "failed to read network holder process identity"))?;
    let closing = value
        .rfind(')')
        .ok_or_else(|| invalid_data("network holder process identity is malformed"))?;
    let start_time = value[closing + 1..]
        .split_whitespace()
        .nth(19)
        .ok_or_else(|| invalid_data("network holder process identity has no start time"))?
        .parse::<u64>()
        .map_err(|_| invalid_data("network holder process start time is invalid"))?;
    if start_time == 0 {
        return Err(invalid_data(
            "network holder process start time must be positive",
        ));
    }
    Ok(start_time)
}
