//! 入站超时与出站看门狗同时配置：谁先到点谁写，且永远只有一条记录。
//!
//! 场景 A（`timeout_ms=20`, `outbound=200`）：入站 settle 超时 → `call_id!=0`；
//! 场景 B（`timeout_ms=200`, `outbound=20`）：出站看门狗先到 → `call_id=0`。
//! 两个场景都必须恰好一条 `HOST_TIMEOUT`。

mod trace_support;

use cordis_abi::{
    Blob, Out, OP_APPLY, OP_CALLBACK, OP_DISPOSE, STATUS_OK, STATUS_PENDING, STATUS_UNSUPPORTED,
};
use cordis_core::{App, Config, FiberState, Inject, Outcome};
use cordis_host::{set_dispatch_policy, DispatchPolicy};
use serde_json::Value;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use trace_support::{abi_plugin, call_iface, collect, provide};

static CALLBACK: Mutex<Option<Value>> = Mutex::new(None);

fn callback_notify() -> &'static tokio::sync::Notify {
    static NOTIFY: OnceLock<tokio::sync::Notify> = OnceLock::new();
    NOTIFY.get_or_init(tokio::sync::Notify::new)
}

unsafe extern "C" fn never_call(handle: u64, op: u32, _a: Blob, _b: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => provide(handle, "never@1", r#"["run"]"#, 1, out),
        OP_CALLBACK => {
            (*out).write(b"");
            STATUS_PENDING
        }
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

unsafe extern "C" fn consumer_call(handle: u64, op: u32, a: Blob, _b: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => {
            let (status, text) = call_iface(handle, "never@1", "run", 7);
            if status != STATUS_OK {
                (*out).write(text.as_bytes());
                return status;
            }
            let reply: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            if reply.get("pending").and_then(Value::as_bool) != Some(true) {
                return STATUS_UNSUPPORTED;
            }
            (*out).write(b"");
            STATUS_OK
        }
        OP_CALLBACK => {
            let payload: Value = serde_json::from_slice(a.as_bytes()).unwrap_or(Value::Null);
            if payload.get("id").and_then(Value::as_u64) == Some(7) {
                *CALLBACK.lock().unwrap() =
                    Some(payload["args"].get(0).cloned().unwrap_or(Value::Null));
                callback_notify().notify_one();
            }
            (*out).write(b"")
        }
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

async fn scenario(
    policy: DispatchPolicy,
) -> (
    App,
    std::sync::Arc<cordis_core::Fiber>,
    std::sync::Arc<cordis_core::Fiber>,
    std::sync::Arc<Mutex<Vec<cordis_core::TraceRecord>>>,
) {
    set_dispatch_policy(policy);
    *CALLBACK.lock().unwrap() = None;
    let app = App::new();
    let records = collect(&app);
    let provider = app.root.plugin(
        abi_plugin("trace-never", Inject::default(), never_call),
        Config::Null,
    );
    provider.settle().await;
    let consumer = app.root.plugin(
        abi_plugin(
            "trace-never-consumer",
            Inject::of(&["never@1"]),
            consumer_call,
        ),
        Config::Null,
    );
    consumer.settle().await;
    assert_eq!(provider.state(), FiberState::Active);
    assert_eq!(consumer.state(), FiberState::Active);
    (app, provider, consumer, records)
}

async fn wait_callback() -> Value {
    tokio::time::timeout(Duration::from_secs(2), async {
        while CALLBACK.lock().unwrap().is_none() {
            callback_notify().notified().await;
        }
    })
    .await
    .expect("看门狗必须在时限内交付");
    CALLBACK.lock().unwrap().clone().expect("已等待到回调")
}

#[tokio::test]
async fn inbound_and_outbound_timeouts_yield_exactly_one_record() {
    // 场景 A：入站先到点。
    let (app, provider, consumer, records) =
        scenario(DispatchPolicy::new(Some(20), Some(200))).await;
    let error = wait_callback().await;
    assert_eq!(error["code"], "HOST_TIMEOUT", "入站超时码：{error}");
    // 等过另一条看门狗的截止点（200ms），确认不会补第二条。
    tokio::time::sleep(Duration::from_millis(250)).await;
    {
        let records = records.lock().unwrap();
        assert_eq!(records.len(), 1, "入站超时只写一条：{records:?}");
        let record = &records[0];
        assert_ne!(record.call_id, 0, "入站结算超时必须复用派发 id");
        assert_eq!(record.callee_uid, provider.uid());
        assert_eq!(record.outcome, Outcome::Err("HOST_TIMEOUT".into()));
        assert!(
            record.elapsed >= Duration::from_millis(20),
            "elapsed 覆盖入站等待：{:?}",
            record.elapsed
        );
    }
    consumer.dispose().await;
    provider.dispose().await;
    drop(app);

    // 场景 B：出站先到点。
    let (_app, provider, consumer, records) =
        scenario(DispatchPolicy::new(Some(200), Some(20))).await;
    let error = wait_callback().await;
    assert_eq!(error["code"], "HOST_TIMEOUT", "出站超时码：{error}");
    tokio::time::sleep(Duration::from_millis(250)).await;
    {
        let records = records.lock().unwrap();
        assert_eq!(records.len(), 1, "出站超时不得再补一条：{records:?}");
        let record = &records[0];
        assert_eq!(record.call_id, 0, "入站从未结算");
        assert_eq!(record.callee_uid, 0);
        assert_eq!(&*record.callee_plugin, "never@1");
        assert_eq!(record.outcome, Outcome::Err("HOST_TIMEOUT".into()));
    }
    consumer.dispose().await;
    provider.dispose().await;
    set_dispatch_policy(DispatchPolicy::default());
}
