use std::fs::File;
use std::io::{IoSliceMut, Read};
use std::mem::MaybeUninit;
use std::num::NonZeroU32;
use std::os::fd::{AsRawFd as _, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context as _, Result as AnyResult, bail};
use rustix::net::netlink::{CONNECTOR as NETLINK_CONNECTOR, SocketAddrNetlink};
use rustix::net::sockopt::{
    set_socket_recv_buffer_size, set_socket_recv_buffer_size_force, socket_recv_buffer_size,
};
use rustix::net::{
    AddressFamily, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, ReturnFlags, SendFlags,
    SocketFlags, SocketType, bind, getsockname, recvfrom, recvmsg, sendto, socket_with,
};

pub(super) struct PidfdReceiver {
    listener: UnixListener,
    connection: Option<UnixStream>,
}

impl PidfdReceiver {
    pub(super) fn bind(path: &Path) -> std::io::Result<Self> {
        let listener = UnixListener::bind(path)?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener,
            connection: None,
        })
    }

    pub(super) fn try_receive(&mut self) -> std::io::Result<Option<OwnedFd>> {
        if self.connection.is_none() {
            match self.listener.accept() {
                Ok((connection, _)) => {
                    connection.set_nonblocking(true)?;
                    self.connection = Some(connection);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
                Err(error) => return Err(error),
            }
        }
        let connection = self.connection.as_ref().expect("accepted connection");
        let mut payload = [0_u8; 32];
        let mut iov = [IoSliceMut::new(&mut payload)];
        let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(2))];
        let mut ancillary = RecvAncillaryBuffer::new(&mut space);
        let message = match recvmsg(
            connection,
            &mut iov,
            &mut ancillary,
            RecvFlags::DONTWAIT | RecvFlags::CMSG_CLOEXEC,
        ) {
            Ok(message) => message,
            Err(error) if error == rustix::io::Errno::AGAIN => return Ok(None),
            Err(error) => return Err(std::io::Error::from_raw_os_error(error.raw_os_error())),
        };
        if message
            .flags
            .intersects(ReturnFlags::CTRUNC | ReturnFlags::TRUNC)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "truncated runc pidfd message",
            ));
        }
        if &payload[..message.bytes] != b"standard" {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "unexpected runc pidfd message payload",
            ));
        }
        let mut descriptors = Vec::new();
        for item in ancillary.drain() {
            if let RecvAncillaryMessage::ScmRights(rights) = item {
                descriptors.extend(rights);
            }
        }
        if descriptors.len() != 1 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "runc pidfd message contained {} descriptors instead of one",
                    descriptors.len()
                ),
            ));
        }
        Ok(descriptors.pop())
    }
}

const CN_IDX_PROC: u32 = 1;
const CN_VAL_PROC: u32 = 1;
const PROC_CN_MCAST_LISTEN: u32 = 1;
const PROC_CN_MCAST_IGNORE: u32 = 2;
const PROC_EVENT_EXIT: u32 = 0x8000_0000;
const NETLINK_HEADER_LEN: usize = 16;
const CONNECTOR_HEADER_LEN: usize = 20;
const PROC_EVENT_EXIT_LEN: usize = 40;
const PROC_EVENT_BUFFER: usize = 64 * 1024;
// The connector is host-wide, so lifecycle polling cannot be its backpressure
// mechanism. The reader drains continuously; this buffer covers scheduler
// stalls while sequence gaps remain fatal evidence loss.
const PROC_EVENT_SOCKET_BUFFER: usize = 4 * 1024 * 1024;
const PROC_EVENT_REPORTED_SOCKET_BUFFER: usize = 2 * PROC_EVENT_SOCKET_BUFFER;
const PROC_EVENT_IDLE_POLL: Duration = Duration::from_millis(1);

#[derive(Clone, Copy)]
pub(super) enum RawProcessResult {
    Exited(i32),
    Signaled(NonZeroU32),
    Unknown(u32),
}

impl RawProcessResult {
    pub(super) fn from_wait_status(status: u32) -> Self {
        let signal = status & 0x7f;
        if signal == 0 {
            return Self::Exited(i32::try_from((status >> 8) & 0xff).expect("exit code fits i32"));
        }
        if signal != 0x7f
            && let Some(signal) = NonZeroU32::new(signal)
        {
            return Self::Signaled(signal);
        }
        Self::Unknown(status)
    }
}

pub(super) struct ProcExitMonitor {
    receiver: Receiver<Result<RawProcessResult, String>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<AnyResult<()>>>,
}

impl ProcExitMonitor {
    #[cfg(test)]
    pub(super) fn inert_for_test() -> Self {
        let (_sender, receiver) = mpsc::channel();
        Self {
            receiver,
            stop: Arc::new(AtomicBool::new(false)),
            worker: None,
        }
    }

    pub(super) fn subscribe(target_pid: u32) -> AnyResult<Self> {
        let socket = socket_with(
            AddressFamily::NETLINK,
            SocketType::DGRAM,
            SocketFlags::CLOEXEC | SocketFlags::NONBLOCK,
            Some(NETLINK_CONNECTOR),
        )?;
        configure_proc_event_buffer(&socket)?;
        bind(&socket, &SocketAddrNetlink::new(0, CN_IDX_PROC))?;
        let address = SocketAddrNetlink::try_from(getsockname(&socket)?)?;
        send_control(&socket, address.pid(), PROC_CN_MCAST_LISTEN)?;
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let (sender, receiver) = mpsc::channel();
        let worker = thread::Builder::new()
            .name(format!("runlab-proc-exit-{target_pid}"))
            .spawn(move || {
                monitor_proc_events(&socket, address.pid(), target_pid, &worker_stop, &sender)
            })?;
        Ok(Self {
            receiver,
            stop,
            worker: Some(worker),
        })
    }

    pub(super) fn try_result(&mut self) -> AnyResult<Option<RawProcessResult>> {
        match self.receiver.try_recv() {
            Ok(Ok(result)) => Ok(Some(result)),
            Ok(Err(error)) => Err(anyhow::anyhow!(error)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => {
                bail!("proc connector reader stopped without a process result")
            }
        }
    }

    pub(super) fn unsubscribe(&mut self) -> AnyResult<()> {
        self.stop.store(true, Ordering::Release);
        let Some(worker) = self.worker.take() else {
            return Ok(());
        };
        worker
            .join()
            .map_err(|_| anyhow::anyhow!("proc connector reader thread panicked"))?
    }
}

impl Drop for ProcExitMonitor {
    fn drop(&mut self) {
        let _ = self.unsubscribe();
    }
}

struct ProcEventDecoder {
    target_pid: u32,
    sequences: std::collections::BTreeMap<u32, u32>,
}

impl ProcEventDecoder {
    fn new(target_pid: u32) -> Self {
        Self {
            target_pid,
            sequences: std::collections::BTreeMap::new(),
        }
    }

    fn parse_datagram(&mut self, bytes: &[u8]) -> AnyResult<Option<u32>> {
        let mut offset = 0;
        while offset < bytes.len() {
            if bytes.len() - offset < NETLINK_HEADER_LEN {
                bail!("truncated proc connector netlink header");
            }
            let message_len = usize::try_from(read_u32(bytes, offset)?)
                .context("netlink message length cannot be represented")?;
            if message_len < NETLINK_HEADER_LEN || message_len > bytes.len() - offset {
                bail!("invalid proc connector netlink message length");
            }
            let message_type = read_u16(bytes, offset + 4)?;
            if message_type == 2 {
                bail!("proc connector returned NLMSG_ERROR");
            }
            if message_type == 3
                && let Some(status) = self.parse_connector_message(
                    &bytes[offset + NETLINK_HEADER_LEN..offset + message_len],
                )?
            {
                return Ok(Some(status));
            }
            offset = offset
                .checked_add((message_len + 3) & !3)
                .context("netlink alignment overflow")?;
        }
        Ok(None)
    }

    fn parse_connector_message(&mut self, bytes: &[u8]) -> AnyResult<Option<u32>> {
        if bytes.len() < CONNECTOR_HEADER_LEN {
            bail!("truncated proc connector header");
        }
        if read_u32(bytes, 0)? != CN_IDX_PROC || read_u32(bytes, 4)? != CN_VAL_PROC {
            return Ok(None);
        }
        let sequence = read_u32(bytes, 8)?;
        let data_len = usize::from(read_u16(bytes, 16)?);
        if data_len > bytes.len() - CONNECTOR_HEADER_LEN {
            bail!("invalid proc connector payload length");
        }
        let data = &bytes[CONNECTOR_HEADER_LEN..CONNECTOR_HEADER_LEN + data_len];
        if data.len() < 16 {
            bail!("truncated proc event header");
        }
        let cpu = read_u32(data, 4)?;
        if let Some(previous) = self.sequences.insert(cpu, sequence)
            && sequence != previous.wrapping_add(1)
        {
            bail!("proc connector sequence gap on CPU {cpu}");
        }
        if read_u32(data, 0)? != PROC_EVENT_EXIT {
            return Ok(None);
        }
        if data.len() < PROC_EVENT_EXIT_LEN {
            bail!("truncated proc exit event");
        }
        let process_pid = read_u32(data, 16)?;
        let process_tgid = read_u32(data, 20)?;
        if process_pid == self.target_pid && process_tgid == self.target_pid {
            return Ok(Some(read_u32(data, 24)?));
        }
        Ok(None)
    }
}

fn configure_proc_event_buffer(socket: &OwnedFd) -> AnyResult<()> {
    match set_socket_recv_buffer_size_force(socket, PROC_EVENT_SOCKET_BUFFER) {
        Ok(()) => {}
        Err(rustix::io::Errno::PERM | rustix::io::Errno::ACCESS) => {
            set_socket_recv_buffer_size(socket, PROC_EVENT_SOCKET_BUFFER)?;
        }
        Err(error) => return Err(error.into()),
    }
    let actual = socket_recv_buffer_size(socket)?;
    // Linux reports twice the requested SO_RCVBUF to include bookkeeping.
    if actual < PROC_EVENT_REPORTED_SOCKET_BUFFER {
        bail!(
            "proc connector receive buffer is {actual} accounted bytes, below the required {PROC_EVENT_REPORTED_SOCKET_BUFFER} accounted bytes for {PROC_EVENT_SOCKET_BUFFER} usable bytes"
        );
    }
    Ok(())
}

fn monitor_proc_events(
    socket: &OwnedFd,
    port_id: u32,
    target_pid: u32,
    stop: &AtomicBool,
    sender: &mpsc::Sender<Result<RawProcessResult, String>>,
) -> AnyResult<()> {
    let observation = read_proc_events(socket, target_pid, stop);
    let unsubscribe = send_control(socket, port_id, PROC_CN_MCAST_IGNORE);
    match observation {
        Ok(Some(result)) => {
            let _ = sender.send(Ok(result));
        }
        Ok(None) => {}
        Err(error) => {
            let _ = sender.send(Err(format!("{error:#}")));
        }
    }
    unsubscribe
}

fn read_proc_events(
    socket: &OwnedFd,
    target_pid: u32,
    stop: &AtomicBool,
) -> AnyResult<Option<RawProcessResult>> {
    let mut decoder = ProcEventDecoder::new(target_pid);
    let mut buffer = vec![0_u8; PROC_EVENT_BUFFER].into_boxed_slice();
    while !stop.load(Ordering::Acquire) {
        let mut drained = false;
        loop {
            if stop.load(Ordering::Acquire) {
                return Ok(None);
            }
            let (received, full_length, address) =
                match recvfrom(socket, &mut buffer[..], RecvFlags::DONTWAIT) {
                    Ok(value) => value,
                    Err(error) if error == rustix::io::Errno::AGAIN => break,
                    Err(error) => return Err(error.into()),
                };
            drained = true;
            if full_length > received {
                bail!("proc connector datagram was truncated");
            }
            if address
                .map(SocketAddrNetlink::try_from)
                .transpose()?
                .is_some_and(|address| address.pid() != 0)
            {
                bail!("proc connector event did not originate from the kernel");
            }
            if let Some(status) = decoder.parse_datagram(&buffer[..received])? {
                return Ok(Some(RawProcessResult::from_wait_status(status)));
            }
        }
        if !drained {
            thread::sleep(PROC_EVENT_IDLE_POLL);
        }
    }
    Ok(None)
}

fn send_control(socket: &OwnedFd, port_id: u32, operation: u32) -> AnyResult<()> {
    let message = proc_connector_control_message(port_id, operation);
    let sent = sendto(
        socket,
        &message,
        SendFlags::empty(),
        &SocketAddrNetlink::new(0, 0),
    )?;
    if sent != message.len() {
        bail!("short proc connector control send");
    }
    Ok(())
}

fn proc_connector_control_message(port_id: u32, operation: u32) -> [u8; 40] {
    let mut message = [0_u8; 40];
    write_u32(&mut message, 0, 40);
    write_u16(&mut message, 4, 3);
    write_u32(&mut message, 8, 1);
    write_u32(&mut message, 12, port_id);
    write_u32(&mut message, 16, CN_IDX_PROC);
    write_u32(&mut message, 20, CN_VAL_PROC);
    write_u32(&mut message, 24, 1);
    write_u16(&mut message, 32, 4);
    write_u32(&mut message, 36, operation);
    message
}

fn read_u16(bytes: &[u8], offset: usize) -> AnyResult<u16> {
    let value = bytes
        .get(offset..offset + 2)
        .context("truncated native-endian u16")?;
    Ok(u16::from_ne_bytes([value[0], value[1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> AnyResult<u32> {
    let value = bytes
        .get(offset..offset + 4)
        .context("truncated native-endian u32")?;
    Ok(u32::from_ne_bytes([value[0], value[1], value[2], value[3]]))
}

fn write_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_ne_bytes());
}

fn write_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_ne_bytes());
}

pub(super) fn pidfd_process_id(pidfd: &OwnedFd) -> AnyResult<u32> {
    let path = PathBuf::from(format!("/proc/self/fdinfo/{}", pidfd.as_raw_fd()));
    let mut bytes = Vec::with_capacity(4097);
    File::open(path)?.take(4097).read_to_end(&mut bytes)?;
    if bytes.len() > 4096 {
        bail!("pidfd fdinfo exceeds 4096 bytes");
    }
    let text = std::str::from_utf8(&bytes).context("pidfd fdinfo is not UTF-8")?;
    let value = text
        .lines()
        .find_map(|line| line.strip_prefix("Pid:\t"))
        .context("pidfd fdinfo has no Pid field")?;
    let pid = value.parse::<u32>().context("invalid pidfd Pid field")?;
    if pid == 0 {
        bail!("pidfd Pid field is zero");
    }
    Ok(pid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn raw_wait_status_stays_mechanism_only() {
        assert!(matches!(
            RawProcessResult::from_wait_status(7 << 8),
            RawProcessResult::Exited(7)
        ));
        assert!(matches!(
            RawProcessResult::from_wait_status(9),
            RawProcessResult::Signaled(signal) if signal.get() == 9
        ));
        assert!(matches!(
            RawProcessResult::from_wait_status(0x7f),
            RawProcessResult::Unknown(0x7f)
        ));
    }

    #[test]
    fn connector_control_message_has_exact_kernel_layout() {
        let control = proc_connector_control_message(42, PROC_CN_MCAST_LISTEN);
        assert_eq!(read_u32(&control, 0).expect("netlink length"), 40);
        assert_eq!(read_u32(&control, 12).expect("netlink port"), 42);
        assert_eq!(
            read_u32(&control, 16).expect("connector index"),
            CN_IDX_PROC
        );
        assert_eq!(read_u32(&control, 36).expect("listen operation"), 1);
    }

    #[test]
    #[ignore = "requires root and the host PID namespace with proc connector enabled"]
    fn real_proc_exit_monitor_survives_eight_way_process_churn() {
        const CONCURRENCY: usize = 8;
        const CHILDREN_PER_WORKER: usize = 128;
        let mut targets = (0..CONCURRENCY)
            .map(|index| {
                Command::new("/bin/sh")
                    .args(["-c", &format!("sleep 3; exit {index}")])
                    .spawn()
                    .expect("target process")
            })
            .collect::<Vec<_>>();
        let monitors = targets
            .iter()
            .map(|target| ProcExitMonitor::subscribe(target.id()))
            .collect::<AnyResult<Vec<_>>>();
        let mut monitors = match monitors {
            Ok(monitors) => monitors,
            Err(error) => {
                for target in &mut targets {
                    let _ = target.kill();
                    let _ = target.wait();
                }
                panic!("proc exit monitor: {error:#}");
            }
        };
        let workers = (0..CONCURRENCY)
            .map(|_| {
                thread::spawn(|| {
                    let status = Command::new("/bin/sh")
                        .args([
                            "-c",
                            &format!(
                                "i=0; while [ \"$i\" -lt {CHILDREN_PER_WORKER} ]; do (:) & i=$((i + 1)); done; wait"
                            ),
                        ])
                        .status()
                        .expect("process churn worker");
                    assert!(status.success(), "process churn worker failed: {status}");
                })
            })
            .collect::<Vec<_>>();
        for worker in workers {
            worker.join().expect("process churn worker");
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let results = monitors
            .iter_mut()
            .map(|monitor| {
                loop {
                    match monitor.try_result()? {
                        Some(result) => return Ok(result),
                        None if std::time::Instant::now() < deadline => {
                            thread::sleep(Duration::from_millis(1));
                        }
                        None => bail!("target exit was not observed before the deadline"),
                    }
                }
            })
            .collect::<AnyResult<Vec<_>>>();
        let results = match results {
            Ok(results) => results,
            Err(error) => {
                for target in &mut targets {
                    let _ = target.kill();
                    let _ = target.wait();
                }
                panic!("proc exit observation: {error:#}");
            }
        };
        for (index, ((target, monitor), result)) in targets
            .iter_mut()
            .zip(&mut monitors)
            .zip(results)
            .enumerate()
        {
            let target_status = target.wait().expect("target reap");
            monitor.unsubscribe().expect("proc monitor unsubscribe");
            let expected = i32::try_from(index).expect("test index fits i32");
            assert_eq!(target_status.code(), Some(expected));
            assert!(matches!(result, RawProcessResult::Exited(code) if code == expected));
        }
    }
}
