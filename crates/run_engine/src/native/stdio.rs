use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::process::{ChildStderr, ChildStdin, ChildStdout};
use std::sync::Arc;

use chrono::Local;
use run_protocol::{
    MAX_CAPTURED_STREAM_BYTES, OperationError, OperationReport, OperationStage, StdinOutput,
    StdinWriteFacts, StreamFacts,
};
use rustix::{fs::OFlags, fs::fcntl_getfl, fs::fcntl_setfl};

use crate::{EngineEventSink, ProgramStream};

const PIPE_PUMP_BYTE_BUDGET: usize = 1024 * 1024;

pub(super) struct InputTransfer {
    pipe: Option<ChildStdin>,
    bytes: Vec<u8>,
    written: usize,
    error: Option<String>,
}

impl InputTransfer {
    pub(super) fn new(pipe: ChildStdin, bytes: Vec<u8>) -> Self {
        let error = set_nonblocking(&pipe).err().map(|error| error.to_string());
        Self {
            pipe: error.is_none().then_some(pipe),
            bytes,
            written: 0,
            error,
        }
    }

    pub(super) fn pump(&mut self) {
        let Some(pipe) = &mut self.pipe else {
            return;
        };
        let mut pumped = 0;
        while self.written < self.bytes.len() && pumped < PIPE_PUMP_BYTE_BUDGET {
            match pipe.write(&self.bytes[self.written..]) {
                Ok(0) => {
                    self.error = Some("stdin pipe accepted zero bytes".to_owned());
                    self.pipe = None;
                    return;
                }
                Ok(count) => {
                    self.written += count;
                    pumped += count;
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return,
                Err(error) => {
                    self.error = Some(error.to_string());
                    self.pipe = None;
                    return;
                }
            }
        }
        if self.written == self.bytes.len() {
            self.pipe = None;
        }
    }

    pub(super) fn freeze(&mut self) {
        if self.pipe.is_some() && self.written < self.bytes.len() && self.error.is_none() {
            self.error =
                Some("stdin transfer stopped when execution entered termination".to_owned());
        }
        self.pipe = None;
    }

    pub(super) const fn is_closed(&self) -> bool {
        self.pipe.is_none()
    }

    pub(super) fn finish(mut self) -> StdinOutput {
        self.pump();
        let facts = StdinWriteFacts::new(u64::try_from(self.written).unwrap_or(u64::MAX));
        let write = if let Some(error) = self.error {
            OperationReport::<StdinWriteFacts>::failed_with_facts(
                facts,
                operation_error(OperationStage::StdinWrite, error),
                [],
            )
        } else if self.pipe.is_some() {
            OperationReport::<StdinWriteFacts>::failed_with_facts(
                facts,
                operation_error(
                    OperationStage::StdinWrite,
                    "stdin transfer did not complete before stream drain deadline",
                ),
                [],
            )
        } else {
            OperationReport::succeeded(facts)
        };
        self.pipe = None;
        StdinOutput::new(write, OperationReport::succeeded(()))
    }
}

pub(super) struct StreamDrain {
    pipe: Option<Box<dyn Read>>,
    bytes: Vec<u8>,
    omitted: bool,
    eof: bool,
    error: Option<String>,
    live_event: Option<Box<StreamLiveEvent>>,
}

struct StreamLiveEvent {
    program_id: run_protocol::ProgramId,
    stream: ProgramStream,
    byte_offset: u64,
    closed: bool,
    event_sink: Arc<dyn EngineEventSink>,
}

impl StreamDrain {
    pub(super) fn from_stdout(pipe: ChildStdout) -> Self {
        Self::new(pipe)
    }

    pub(super) fn from_stderr(pipe: ChildStderr) -> Self {
        Self::new(pipe)
    }

    fn new<R: Read + AsFd + 'static>(pipe: R) -> Self {
        let error = set_nonblocking(&pipe).err().map(|error| error.to_string());
        Self {
            pipe: error.is_none().then_some(Box::new(pipe)),
            bytes: Vec::new(),
            omitted: false,
            eof: false,
            error,
            live_event: None,
        }
    }

    pub(super) fn forward_events(
        &mut self,
        program_id: run_protocol::ProgramId,
        stream: ProgramStream,
        event_sink: Arc<dyn EngineEventSink>,
    ) {
        self.live_event = Some(Box::new(StreamLiveEvent {
            program_id,
            stream,
            byte_offset: 0,
            closed: false,
            event_sink,
        }));
    }

    pub(super) fn pump(&mut self) {
        if self.pipe.is_none() {
            return;
        }
        let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
        let mut pumped = 0;
        while pumped < PIPE_PUMP_BYTE_BUDGET {
            let read = self
                .pipe
                .as_mut()
                .expect("checked stream pipe")
                .read(&mut buffer);
            match read {
                Ok(0) => {
                    self.eof = true;
                    self.pipe = None;
                    self.observe_closed();
                    return;
                }
                Ok(count) => {
                    pumped += count;
                    self.observe_bytes(&buffer[..count]);
                    let available = MAX_CAPTURED_STREAM_BYTES.saturating_sub(self.bytes.len());
                    let keep = available.min(count);
                    self.bytes.extend_from_slice(&buffer[..keep]);
                    self.omitted |= keep < count;
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return,
                Err(error) => {
                    self.error = Some(error.to_string());
                    self.pipe = None;
                    self.observe_closed();
                    return;
                }
            }
        }
    }

    pub(super) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub(super) fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub(super) const fn is_closed(&self) -> bool {
        self.pipe.is_none()
    }

    pub(super) fn finish(mut self, stage: OperationStage) -> OperationReport<StreamFacts> {
        self.pump();
        if self.pipe.is_some() && self.error.is_none() {
            self.error = Some("stream did not reach EOF before stream drain deadline".to_owned());
        }
        self.pipe = None;
        self.observe_closed();
        let facts = StreamFacts::new(self.bytes, self.omitted, self.eof)
            .expect("nonblocking drainer preserves stream shape");
        match self.error {
            Some(message) => OperationReport::<StreamFacts>::failed_with_facts(
                facts,
                operation_error(stage, message),
                [],
            ),
            None => OperationReport::succeeded(facts),
        }
    }

    fn observe_bytes(&mut self, bytes: &[u8]) {
        let Some(live_event) = &mut self.live_event else {
            return;
        };
        live_event.event_sink.program_output(
            &live_event.program_id,
            live_event.stream,
            live_event.byte_offset,
            bytes,
        );
        live_event.byte_offset = live_event
            .byte_offset
            .saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
    }

    fn observe_closed(&mut self) {
        let Some(live_event) = &mut self.live_event else {
            return;
        };
        if !live_event.closed {
            live_event.closed = true;
            live_event
                .event_sink
                .program_stream_closed(&live_event.program_id, live_event.stream);
        }
    }
}

fn set_nonblocking(fd: &impl AsFd) -> std::io::Result<()> {
    let flags = fcntl_getfl(fd)?;
    Ok(fcntl_setfl(fd, flags | OFlags::NONBLOCK)?)
}

fn operation_error(stage: OperationStage, message: impl Into<String>) -> OperationError {
    OperationError::new(Local::now().fixed_offset(), stage, message, None)
        .expect("stdio operation messages are non-empty")
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::thread;
    use std::time::{Duration, Instant};

    use run_protocol::OperationStatus;

    use super::*;

    #[derive(Default)]
    struct CountingEventSink {
        bytes: AtomicU64,
        closed: AtomicBool,
    }

    impl EngineEventSink for CountingEventSink {
        fn program_output(
            &self,
            _program_id: &run_protocol::ProgramId,
            _stream: ProgramStream,
            byte_offset: u64,
            bytes: &[u8],
        ) {
            assert_eq!(byte_offset, self.bytes.load(Ordering::Acquire));
            self.bytes.fetch_add(
                u64::try_from(bytes.len()).expect("test stream length"),
                Ordering::AcqRel,
            );
        }

        fn program_stream_closed(
            &self,
            _program_id: &run_protocol::ProgramId,
            _stream: ProgramStream,
        ) {
            assert!(!self.closed.swap(true, Ordering::AcqRel));
        }
    }

    #[test]
    fn stream_drain_keeps_exact_binary_limit_and_continues_to_eof() {
        let (reader, mut writer) = UnixStream::pair().expect("stream pair");
        let producer = thread::spawn(move || {
            let mut chunk = vec![0_u8; 64 * 1024];
            chunk[0] = 0xff;
            let mut remaining = MAX_CAPTURED_STREAM_BYTES + 17;
            while remaining != 0 {
                let count = remaining.min(chunk.len());
                writer.write_all(&chunk[..count]).expect("write stream");
                remaining -= count;
            }
        });
        let mut drain = StreamDrain::new(reader);
        let event_sink = Arc::new(CountingEventSink::default());
        drain.forward_events(
            run_protocol::ProgramId::primary(),
            ProgramStream::Stdout,
            event_sink.clone(),
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        while !drain.is_closed() {
            drain.pump();
            assert!(Instant::now() < deadline, "stream drain did not reach EOF");
            thread::yield_now();
        }
        producer.join().expect("stream producer");

        let report = drain.finish(OperationStage::StdoutRead);
        let facts = report.facts().expect("stream facts");
        assert_eq!(report.status(), OperationStatus::Succeeded);
        assert_eq!(facts.bytes().len(), MAX_CAPTURED_STREAM_BYTES);
        assert_eq!(&facts.bytes()[..2], &[0xff, 0]);
        assert!(facts.omitted_after_limit());
        assert!(facts.eof());
        assert_eq!(
            event_sink.bytes.load(Ordering::Acquire),
            u64::try_from(MAX_CAPTURED_STREAM_BYTES + 17).expect("test stream length")
        );
        assert!(event_sink.closed.load(Ordering::Acquire));
    }
}
