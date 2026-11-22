//! JSONL trace sinks (sync + bounded async) and attribution helpers for RSI.

use cordis_core::{Outcome, TraceRecord, TraceSink};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::Mutex;
use std::thread::JoinHandle;

fn record_to_json(record: &TraceRecord) -> Value {
    let outcome = match &record.outcome {
        Outcome::Ok => json!({ "status": "ok" }),
        Outcome::Err(code) => json!({ "status": "err", "code": code }),
    };
    let mut row = json!({
        "kind": "call",
        "ts_ms": record.ts_ms,
        "name": record.name,
        "call_id": record.call_id,
        "parent_call_id": record.parent_call_id,
        "caller_uid": record.caller_uid,
        "callee_uid": record.callee_uid,
        "callee_plugin": record.callee_plugin,
        "elapsed_us": record.elapsed.as_micros() as u64,
    });
    if let Some(correlation_id) = record.correlation_id.as_deref() {
        row["correlation_id"] = json!(correlation_id);
    }
    if let (Some(row), Value::Object(outcome)) = (row.as_object_mut(), outcome) {
        row.extend(outcome);
    }
    row
}

pub struct JsonlSink {
    path: PathBuf,
    file: Mutex<File>,
}

impl JsonlSink {
    pub fn open(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = File::create(&path)?;
        Ok(Self {
            path,
            file: Mutex::new(file),
        })
    }

    pub fn marker(&self, phase: &str) {
        self.write(json!({ "kind": "marker", "phase": phase }));
    }

    fn write(&self, value: Value) {
        let mut file = self
            .file
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _ = writeln!(file, "{value}");
        let _ = file.flush();
    }

    pub fn lines(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.path)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }
}

impl TraceSink for JsonlSink {
    fn record(&self, record: TraceRecord) {
        self.write(record_to_json(&record));
    }
}

enum AsyncMsg {
    Line(Value),
    Flush(mpsc::Sender<()>),
    Shutdown,
}

/// Non-blocking JSONL trace sink for multi-session concurrent hosts.
///
/// `TraceSink::record` only performs a non-blocking `try_send` onto a bounded
/// channel so concurrent Tokio worker threads never contend on file I/O locks.
pub struct AsyncJsonlSink {
    path: PathBuf,
    tx: SyncSender<AsyncMsg>,
    dropped: AtomicU64,
    reported_dropped: AtomicU64,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl AsyncJsonlSink {
    pub fn open(path: impl AsRef<Path>, capacity: usize) -> std::io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let mut file = File::create(&path)?;
        let (tx, rx) = mpsc::sync_channel::<AsyncMsg>(capacity.max(1));
        let worker = std::thread::Builder::new()
            .name("cordis-trace-writer".to_string())
            .spawn(move || {
                while let Ok(msg) = rx.recv() {
                    match msg {
                        AsyncMsg::Line(value) => {
                            let _ = writeln!(file, "{value}");
                        }
                        AsyncMsg::Flush(ack) => {
                            let _ = file.flush();
                            let _ = ack.send(());
                        }
                        AsyncMsg::Shutdown => {
                            let _ = file.flush();
                            break;
                        }
                    }
                }
                let _ = file.flush();
            })?;
        Ok(Self {
            path,
            tx,
            dropped: AtomicU64::new(0),
            reported_dropped: AtomicU64::new(0),
            worker: Mutex::new(Some(worker)),
        })
    }

    pub fn marker(&self, phase: &str) {
        self.emit_dropped_summary();
        self.enqueue(json!({ "kind": "marker", "phase": phase }));
    }

    pub fn dropped_count(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Block until all previously enqueued records are flushed to disk.
    pub fn flush(&self) {
        self.emit_dropped_summary();
        let (ack_tx, ack_rx) = mpsc::channel();
        if self.tx.send(AsyncMsg::Flush(ack_tx)).is_ok() {
            let _ = ack_rx.recv();
        }
    }

    fn emit_dropped_summary(&self) {
        let total = self.dropped.load(Ordering::Acquire);
        let prev = self.reported_dropped.swap(total, Ordering::AcqRel);
        if total > prev {
            let _ = self.tx.send(AsyncMsg::Line(json!({
                "kind": "dropped",
                "dropped": total - prev,
                "total_dropped": total,
            })));
        }
    }

    pub fn lines(&self) -> Vec<Value> {
        self.flush();
        std::fs::read_to_string(&self.path)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    fn enqueue(&self, value: Value) {
        match self.tx.try_send(AsyncMsg::Line(value)) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

impl Drop for AsyncJsonlSink {
    fn drop(&mut self) {
        self.emit_dropped_summary();
        let _ = self.tx.send(AsyncMsg::Shutdown);
        if let Some(worker) = self
            .worker
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            let _ = worker.join();
        }
    }
}

impl TraceSink for AsyncJsonlSink {
    fn record(&self, record: TraceRecord) {
        self.enqueue(record_to_json(&record));
    }
}

/// Records inside one marker window, excluding markers.
pub fn window(lines: &[Value], phase: &str) -> Vec<Value> {
    let mut started = false;
    let mut rows = Vec::new();
    for line in lines {
        if line["kind"] == "marker" {
            if line["phase"] == phase {
                if started {
                    break;
                }
                started = true;
            }
            continue;
        }
        if started {
            rows.push(line.clone());
        }
    }
    rows
}

/// Leaf errors: an error row whose `call_id` is not the parent of another row.
pub fn leaf_errors(rows: &[Value]) -> Vec<Value> {
    let parents: HashSet<u64> = rows
        .iter()
        .filter_map(|row| row["parent_call_id"].as_u64())
        .collect();
    rows.iter()
        .filter(|row| row["status"] == "err")
        .filter(|row| {
            row["call_id"]
                .as_u64()
                .is_none_or(|id| id == 0 || !parents.contains(&id))
        })
        .cloned()
        .collect()
}

/// Aggregate leaf errors per session (`correlation_id`) and callee plugin.
///
/// Grouping per session first prevents parent/child `call_id` ambiguity and
/// isolates regressions to the exact session and variant that failed.
pub fn leaf_errors_by_session(rows: &[Value]) -> HashMap<String, HashMap<String, usize>> {
    let mut by_session: HashMap<String, Vec<Value>> = HashMap::new();
    for row in rows {
        if row["kind"] != "call" {
            continue;
        }
        let session = row["correlation_id"]
            .as_str()
            .unwrap_or("<uncorrelated>")
            .to_string();
        by_session.entry(session).or_default().push(row.clone());
    }
    let mut result = HashMap::new();
    for (session, session_rows) in by_session {
        let mut counts: HashMap<String, usize> = HashMap::new();
        for leaf in leaf_errors(&session_rows) {
            let plugin = leaf["callee_plugin"]
                .as_str()
                .unwrap_or("<unknown>")
                .to_string();
            *counts.entry(plugin).or_default() += 1;
        }
        if !counts.is_empty() {
            result.insert(session, counts);
        }
    }
    result
}
