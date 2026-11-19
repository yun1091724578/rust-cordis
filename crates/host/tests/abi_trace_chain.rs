//! 服务调用链的逐跳记录（ADR 0009 / ADR 0010）。
//!
//! 夹具是三个 ABI 形状插件：`consumer` → `mid@1` → `leaf@1`。中间一跳在自己的
//! `OP_CALLBACK` 里再发起一次 `ctx_call_iface`，于是宿主必须在同一条调用链上
//! 产出两条记录，并把第二条的 `parent_call_id` 挂到第一条的 `call_id` 上。
//!
//! 夹具铁律：fake `CallFn` 内禁止 assert，结局只记进 static `Mutex`。

use cordis_abi::{Blob, Out, OP_APPLY, OP_CALLBACK, OP_DISPOSE, STATUS_OK, STATUS_UNSUPPORTED};
use cordis_core::{App, Config, Ctx, FiberState, Inject, Outcome, Plugin, TraceRecord, TraceSink};
use cordis_host::{apply_via_with_error_format, host_api};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

static CONSUMER_OUT: Mutex<String> = Mutex::new(String::new());
static LEAF_CALLS: Mutex<u64> = Mutex::new(0);

struct Collector(Arc<Mutex<Vec<TraceRecord>>>);

impl TraceSink for Collector {
    fn record(&self, record: TraceRecord) {
        self.0.lock().unwrap().push(record);
    }
}

/// 给 `ctx_provide_iface` / `ctx_call_iface` 用的临时输出槽。
fn scratch_out(buffer: &mut [u8]) -> Out {
    Out {
        ptr: buffer.as_mut_ptr(),
        cap: buffer.len(),
        len: 0,
    }
}

unsafe extern "C" fn leaf_call(handle: u64, op: u32, _a: Blob, _b: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => {
            let Some(provide) = host_api().ctx_provide_iface else {
                return STATUS_UNSUPPORTED;
            };
            let mut buffer = [0u8; 256];
            let mut slot = scratch_out(&mut buffer);
            let status = provide(
                handle,
                Blob::new(b"leaf@1"),
                1,
                Blob::new(br#"["run"]"#),
                2,
                &mut slot,
            );
            if status != STATUS_OK {
                return status;
            }
            (*out).write(b"")
        }
        OP_CALLBACK => {
            *LEAF_CALLS.lock().unwrap() += 1;
            (*out).write(br#"{"ret":{"from":"leaf"}}"#)
        }
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

unsafe extern "C" fn mid_call(handle: u64, op: u32, a: Blob, _b: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => {
            let Some(provide) = host_api().ctx_provide_iface else {
                return STATUS_UNSUPPORTED;
            };
            let mut buffer = [0u8; 256];
            let mut slot = scratch_out(&mut buffer);
            let status = provide(
                handle,
                Blob::new(b"mid@1"),
                1,
                Blob::new(br#"["run"]"#),
                1,
                &mut slot,
            );
            if status != STATUS_OK {
                return status;
            }
            (*out).write(b"")
        }
        OP_CALLBACK => {
            let payload: Value = serde_json::from_slice(a.as_bytes()).unwrap_or(Value::Null);
            let method = payload.get("method").and_then(Value::as_str).unwrap_or("");
            if method != "run" {
                return STATUS_UNSUPPORTED;
            }
            let Some(call) = host_api().ctx_call_iface else {
                return STATUS_UNSUPPORTED;
            };
            let mut buffer = [0u8; 1024];
            let mut slot = scratch_out(&mut buffer);
            let status = call(
                handle,
                Blob::new(b"leaf@1"),
                1,
                Blob::new(b"run"),
                Blob::new(b"[]"),
                0,
                &mut slot,
            );
            if status != STATUS_OK {
                return status;
            }
            let text = String::from_utf8_lossy(&buffer[..slot.len]).into_owned();
            let reply: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            if reply.get("pending").and_then(Value::as_bool) != Some(false) {
                // 本用例只走同步路径：中间层挂起不在归因范围里。
                return STATUS_UNSUPPORTED;
            }
            let leaf = reply.get("result").cloned().unwrap_or(Value::Null);
            let text = json!({ "ret": { "from": "mid", "leaf": leaf } }).to_string();
            (*out).write(text.as_bytes())
        }
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

unsafe extern "C" fn consumer_call(handle: u64, op: u32, _a: Blob, _b: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => {
            let Some(call) = host_api().ctx_call_iface else {
                return STATUS_UNSUPPORTED;
            };
            let mut buffer = [0u8; 1024];
            let mut slot = scratch_out(&mut buffer);
            let status = call(
                handle,
                Blob::new(b"mid@1"),
                1,
                Blob::new(b"run"),
                Blob::new(b"[]"),
                0,
                &mut slot,
            );
            if status != STATUS_OK {
                return status;
            }
            *CONSUMER_OUT.lock().unwrap() =
                String::from_utf8_lossy(&buffer[..slot.len]).into_owned();
            (*out).write(b"")
        }
        OP_CALLBACK => (*out).write(b""),
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

fn abi_plugin(name: &'static str, inject: Inject, call: cordis_host::CallFn) -> Arc<dyn Plugin> {
    let key = cordis_core::unique_key(name);
    Arc::new(cordis_core::FnPlugin {
        name,
        key,
        inject,
        validator: None,
        f: Arc::new(move |ctx: Ctx, config: Config| {
            apply_via_with_error_format(call, ctx, config, name, cordis_abi::ERROR_FORMAT_V1)
        }),
    })
}

#[tokio::test]
async fn sync_chain_records_parent_links() {
    *CONSUMER_OUT.lock().unwrap() = String::new();
    *LEAF_CALLS.lock().unwrap() = 0;

    let app = App::new();
    let records = Arc::new(Mutex::new(Vec::new()));
    assert!(
        app.set_trace_sink(Arc::new(Collector(records.clone()))),
        "trace sink 只允许安装一次"
    );

    let leaf = app.root.plugin(
        abi_plugin("trace-leaf", Inject::default(), leaf_call),
        Config::Null,
    );
    leaf.settle().await;
    assert_eq!(leaf.state(), FiberState::Active, "leaf 必须装载成功");
    let mid = app.root.plugin(
        abi_plugin("trace-mid", Inject::of(&["leaf@1"]), mid_call),
        Config::Null,
    );
    mid.settle().await;
    assert_eq!(mid.state(), FiberState::Active, "mid 必须装载成功");
    let consumer = app.root.plugin(
        abi_plugin("trace-consumer", Inject::of(&["mid@1"]), consumer_call),
        Config::Null,
    );
    consumer.settle().await;
    assert_eq!(
        consumer.state(),
        FiberState::Active,
        "consumer 必须装载成功（apply 期的 iface 调用要能内联完成）"
    );

    let reply: Value =
        serde_json::from_str(CONSUMER_OUT.lock().unwrap().as_str()).expect("consumer 应拿到 JSON");
    assert_eq!(reply["pending"], false, "同步链必须内联完成：{reply}");
    assert_eq!(reply["result"]["from"], "mid");
    assert_eq!(reply["result"]["leaf"]["from"], "leaf");
    assert_eq!(*LEAF_CALLS.lock().unwrap(), 1, "叶子方法只应执行一次");

    {
        let records = records.lock().unwrap();
        assert_eq!(
            records.len(),
            2,
            "一次 A→B→C 链必须恰好两条记录：{records:?}"
        );
        let mid_record = records
            .iter()
            .find(|record| &*record.name == "mid@1.run")
            .expect("中间一跳必须有记录");
        let leaf_record = records
            .iter()
            .find(|record| &*record.name == "leaf@1.run")
            .expect("叶子一跳必须有记录");

        assert_eq!(mid_record.outcome, Outcome::Ok);
        assert_eq!(leaf_record.outcome, Outcome::Ok);
        assert_eq!(
            mid_record.parent_call_id, 0,
            "consumer 的调用点是 apply，没有父跳"
        );
        assert_eq!(
            leaf_record.parent_call_id, mid_record.call_id,
            "子调用的父必须是中间一跳的派发 id"
        );
        assert_ne!(mid_record.call_id, 0);
        assert_ne!(leaf_record.call_id, 0);
        assert_ne!(mid_record.call_id, leaf_record.call_id);
        assert_eq!(
            mid_record.caller_uid,
            consumer.uid(),
            "发起方是 consumer fiber"
        );
        assert_eq!(leaf_record.caller_uid, mid.uid(), "发起方是中间层 fiber");
        assert_eq!(mid_record.callee_uid, mid.uid());
        assert_eq!(&*mid_record.callee_plugin, "trace-mid");
        assert_eq!(leaf_record.callee_uid, leaf.uid());
        assert_eq!(&*leaf_record.callee_plugin, "trace-leaf");
    }

    consumer.dispose().await;
    mid.dispose().await;
    leaf.dispose().await;
}
