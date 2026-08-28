use std::collections::BTreeMap;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use chrono::Utc;
use run_engine::{EngineObserver, EngineStage, ProgramStream};
use run_protocol::ProgramId;
use serde_json::{Value, json};

const OBSERVATION_QUEUE_CAPACITY: usize = 256;

pub(crate) struct RunObservation {
    inner: Arc<ObservationInner>,
    writer: Option<JoinHandle<()>>,
}

struct ObservationInner {
    sender: SyncSender<Value>,
    dropped_observation: AtomicBool,
    streams: Mutex<BTreeMap<(ProgramId, ProgramStream), Utf8Stream>>,
}

#[derive(Default)]
struct Utf8Stream {
    pending_offset: u64,
    pending: Vec<u8>,
}

impl RunObservation {
    pub(crate) fn stderr(run_id: &str) -> Self {
        Self::with_writer(run_id, std::io::stderr())
    }

    fn with_writer(run_id: &str, writer: impl Write + Send + 'static) -> Self {
        let (sender, receiver) = sync_channel(OBSERVATION_QUEUE_CAPACITY);
        let inner = Arc::new(ObservationInner {
            sender,
            dropped_observation: AtomicBool::new(false),
            streams: Mutex::new(BTreeMap::new()),
        });
        let writer = thread::spawn(move || write_records(&receiver, writer));
        let observation = Self {
            inner,
            writer: Some(writer),
        };
        observation.send(json!({
            "kind": "run.stream",
            "schema_version": 1,
            "run_id": run_id,
        }));
        observation
    }

    pub(crate) fn engine_observer(&self) -> Arc<dyn EngineObserver> {
        Arc::clone(&self.inner) as Arc<dyn EngineObserver>
    }

    pub(crate) fn stage(&self, stage: &'static str) {
        self.send(json!({
            "kind": "run.stage",
            "observed_at": observed_at(),
            "stage": stage,
        }));
    }

    pub(crate) fn finish(mut self) {
        self.report_dropped_observation();
        drop(self.inner);
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
    }

    pub(crate) fn report_dropped_observation(&self) {
        if self.inner.dropped_observation.swap(false, Ordering::AcqRel) {
            self.send(json!({
                "kind": "transport.diagnostic",
                "observed_at": observed_at(),
                "operation": "stderr_observation",
                "message": "real-time observation records were dropped because the stderr consumer could not keep up",
            }));
        }
    }

    fn send(&self, value: Value) {
        self.inner.enqueue(value);
    }
}

impl ObservationInner {
    fn enqueue(&self, value: Value) {
        match self.sender.try_send(value) {
            Ok(()) | Err(TrySendError::Disconnected(_)) => {}
            Err(TrySendError::Full(_)) => {
                self.dropped_observation.store(true, Ordering::Release);
            }
        }
    }
}

impl EngineObserver for ObservationInner {
    fn stage(&self, stage: EngineStage) {
        let stage = match stage {
            EngineStage::Executing => "executing",
            EngineStage::Stopping => "stopping",
            EngineStage::Capturing => "capturing",
        };
        self.enqueue(json!({
            "kind": "run.stage",
            "observed_at": observed_at(),
            "stage": stage,
        }));
    }

    fn program_output(
        &self,
        program_id: &ProgramId,
        stream: ProgramStream,
        byte_offset: u64,
        bytes: &[u8],
    ) {
        let Ok(mut streams) = self.streams.lock() else {
            return;
        };
        let state = streams.entry((program_id.clone(), stream)).or_default();
        for record in state.push(program_id, stream, byte_offset, bytes) {
            self.enqueue(record);
        }
    }

    fn program_stream_closed(&self, program_id: &ProgramId, stream: ProgramStream) {
        let Ok(mut streams) = self.streams.lock() else {
            return;
        };
        let Some(mut state) = streams.remove(&(program_id.clone(), stream)) else {
            return;
        };
        if let Some(record) = state.finish(program_id, stream) {
            self.enqueue(record);
        }
    }
}

impl Utf8Stream {
    fn push(
        &mut self,
        program_id: &ProgramId,
        stream: ProgramStream,
        byte_offset: u64,
        bytes: &[u8],
    ) -> Vec<Value> {
        if self.pending.is_empty() {
            self.pending_offset = byte_offset;
        } else {
            debug_assert_eq!(
                self.pending_offset
                    .saturating_add(u64::try_from(self.pending.len()).unwrap_or(u64::MAX)),
                byte_offset
            );
        }
        self.pending.extend_from_slice(bytes);
        let mut records = Vec::new();
        match std::str::from_utf8(&self.pending) {
            Ok(text) => {
                if !text.is_empty() {
                    records.push(stream_record(
                        program_id,
                        stream,
                        self.pending_offset,
                        "text",
                        text,
                    ));
                    self.pending_offset = self
                        .pending_offset
                        .saturating_add(u64::try_from(self.pending.len()).unwrap_or(u64::MAX));
                    self.pending.clear();
                }
            }
            Err(error) if error.error_len().is_none() => {
                let valid = error.valid_up_to();
                if valid != 0 {
                    let text = std::str::from_utf8(&self.pending[..valid])
                        .expect("validated UTF-8 prefix")
                        .to_owned();
                    records.push(stream_record(
                        program_id,
                        stream,
                        self.pending_offset,
                        "text",
                        &text,
                    ));
                    self.pending.drain(..valid);
                    self.pending_offset = self
                        .pending_offset
                        .saturating_add(u64::try_from(valid).unwrap_or(u64::MAX));
                }
            }
            Err(_) => {
                let encoded = BASE64.encode(&self.pending);
                records.push(stream_record(
                    program_id,
                    stream,
                    self.pending_offset,
                    "base64",
                    &encoded,
                ));
                self.pending_offset = self
                    .pending_offset
                    .saturating_add(u64::try_from(self.pending.len()).unwrap_or(u64::MAX));
                self.pending.clear();
            }
        }
        records
    }

    fn finish(&mut self, program_id: &ProgramId, stream: ProgramStream) -> Option<Value> {
        if self.pending.is_empty() {
            return None;
        }
        let encoded = BASE64.encode(&self.pending);
        self.pending.clear();
        Some(stream_record(
            program_id,
            stream,
            self.pending_offset,
            "base64",
            &encoded,
        ))
    }
}

fn stream_record(
    program_id: &ProgramId,
    stream: ProgramStream,
    byte_offset: u64,
    encoding: &str,
    data: &str,
) -> Value {
    let mut value = json!({
        "kind": match stream {
            ProgramStream::Stdout => "program.stdout",
            ProgramStream::Stderr => "program.stderr",
        },
        "observed_at": observed_at(),
        "program_id": program_id.as_str(),
        "byte_offset": byte_offset,
    });
    value[encoding] = Value::String(data.to_owned());
    value
}

fn observed_at() -> String {
    Utc::now().to_rfc3339()
}

fn write_records(receiver: &Receiver<Value>, mut writer: impl Write) {
    let mut writable = true;
    while let Ok(record) = receiver.recv() {
        if writable {
            writable = serde_json::to_writer(&mut writer, &record).is_ok()
                && writer.write_all(b"\n").is_ok()
                && writer.flush().is_ok();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_split_across_reads_stays_text() {
        let id = ProgramId::primary();
        let mut stream = Utf8Stream::default();
        assert!(
            stream
                .push(&id, ProgramStream::Stdout, 0, &[0xe4, 0xbd])
                .is_empty()
        );
        let records = stream.push(&id, ProgramStream::Stdout, 2, &[0xa0, b'\n']);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["kind"], "program.stdout");
        assert_eq!(records[0]["byte_offset"], 0);
        assert_eq!(records[0]["text"], "你\n");
        assert!(records[0].get("base64").is_none());
    }

    #[test]
    fn invalid_bytes_are_base64_with_exact_offsets() {
        let id = ProgramId::primary();
        let mut stream = Utf8Stream::default();
        let records = stream.push(&id, ProgramStream::Stderr, 0, &[b'a', 0xff, b'b']);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["base64"], "Yf9i");
        assert_eq!(records[0]["byte_offset"], 0);
        assert!(records[0].get("text").is_none());
    }
}
