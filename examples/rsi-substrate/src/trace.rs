//! JSONL trace sink + marker-based windows for the RSI e2e.

use cordis_core::{Outcome, TraceRecord, TraceSink};
use serde_json::{json, Value};
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

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
        if let (Some(row), Value::Object(outcome)) = (row.as_object_mut(), outcome) {
            row.extend(outcome);
        }
        self.write(row);
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
    let parents: std::collections::HashSet<u64> = rows
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
