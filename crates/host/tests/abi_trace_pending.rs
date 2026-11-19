//! PENDING 续延链：`consumer → m@1 →(PENDING 30ms) p@1`，恢复后再调 `c@1`。
//!
//! 断言 ADR 0010 的两条事实：
//! - `deliver_outbound` 交付续延时压的是**发起方那一跳**，所以恢复后发起的
//!   `c@1` 仍挂在 `m@1` 的派发 id 上；
//! - PENDING 的等待期计入 `elapsed`，且每跳恰好一条记录。

mod trace_support;

use cordis_abi::{
    Blob, Out, OP_APPLY, OP_CALLBACK, OP_DISPOSE, STATUS_OK, STATUS_PENDING, STATUS_UNSUPPORTED,
};
use cordis_core::{App, Config, FiberState, Inject, Outcome};
use cordis_host::host_api;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use trace_support::{abi_plugin, call_iface, collect, provide};

static MID_DISPATCH: Mutex<u64> = Mutex::new(0);
static P_DISPATCH: AtomicU64 = AtomicU64::new(0);
static CONSUMER_RESULT: Mutex<Option<Value>> = Mutex::new(None);

fn result_notify() -> &'static tokio::sync::Notify {
    static NOTIFY: OnceLock<tokio::sync::Notify> = OnceLock::new();
    NOTIFY.get_or_init(tokio::sync::Notify::new)
}

unsafe extern "C" fn leaf_call(handle: u64, op: u32, _a: Blob, _b: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => provide(handle, "c@1", r#"["run"]"#, 3, out),
        OP_CALLBACK => (*out).write(br#"{"ret":"leaf"}"#),
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

unsafe extern "C" fn pending_call(handle: u64, op: u32, a: Blob, _b: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => provide(handle, "p@1", r#"["run"]"#, 2, out),
        OP_CALLBACK => {
            let payload: Value = serde_json::from_slice(a.as_bytes()).unwrap_or(Value::Null);
            let Some(dispatch_id) = payload.get("dispatch_id").and_then(Value::as_u64) else {
                return STATUS_UNSUPPORTED;
            };
            P_DISPATCH.store(dispatch_id, Ordering::SeqCst);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(30));
                let Some(fulfill) = host_api().ctx_fulfill else {
                    return;
                };
                let mut buffer = [0u8; 256];
                let mut slot = Out {
                    ptr: buffer.as_mut_ptr(),
                    cap: buffer.len(),
                    len: 0,
                };
                let _ = fulfill(
                    handle,
                    dispatch_id,
                    Blob::new(br#"{"done":true}"#),
                    &mut slot,
                );
            });
            (*out).write(b"");
            STATUS_PENDING
        }
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

unsafe extern "C" fn mid_call(handle: u64, op: u32, a: Blob, _b: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => provide(handle, "m@1", r#"["run"]"#, 1, out),
        OP_CALLBACK => {
            let payload: Value = serde_json::from_slice(a.as_bytes()).unwrap_or(Value::Null);
            if payload.get("id").and_then(Value::as_u64) == Some(1) {
                let Some(dispatch_id) = payload.get("dispatch_id").and_then(Value::as_u64) else {
                    return STATUS_UNSUPPORTED;
                };
                *MID_DISPATCH.lock().unwrap() = dispatch_id;
                let (status, text) = call_iface(handle, "p@1", "run", 7);
                if status != STATUS_OK {
                    (*out).write(text.as_bytes());
                    return status;
                }
                let reply: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                if reply.get("pending").and_then(Value::as_bool) == Some(true) {
                    (*out).write(b"");
                    return STATUS_PENDING;
                }
                return STATUS_UNSUPPORTED;
            }
            if payload.get("id").and_then(Value::as_u64) == Some(7) {
                let (status, text) = call_iface(handle, "c@1", "run", 0);
                if status != STATUS_OK {
                    (*out).write(text.as_bytes());
                    return status;
                }
                let reply: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                let result = reply.get("result").cloned().unwrap_or(Value::Null);
                let Some(fulfill) = host_api().ctx_fulfill else {
                    return STATUS_UNSUPPORTED;
                };
                let mut buffer = [0u8; 256];
                let mut slot = Out {
                    ptr: buffer.as_mut_ptr(),
                    cap: buffer.len(),
                    len: 0,
                };
                if fulfill(
                    handle,
                    *MID_DISPATCH.lock().unwrap(),
                    Blob::new(result.to_string().as_bytes()),
                    &mut slot,
                ) != STATUS_OK
                {
                    return cordis_abi::STATUS_ERR;
                }
                (*out).write(b"");
                return STATUS_OK;
            }
            STATUS_UNSUPPORTED
        }
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

unsafe extern "C" fn consumer_call(handle: u64, op: u32, a: Blob, _b: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => {
            let (status, text) = call_iface(handle, "m@1", "run", 7);
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
                *CONSUMER_RESULT.lock().unwrap() = payload["args"]
                    .get(0)
                    .and_then(|arg| arg.get("result"))
                    .cloned();
                result_notify().notify_one();
            }
            (*out).write(b"")
        }
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

#[tokio::test]
async fn pending_continuation_keeps_the_callers_hop_and_counts_wait_time() {
    *MID_DISPATCH.lock().unwrap() = 0;
    P_DISPATCH.store(0, Ordering::SeqCst);
    *CONSUMER_RESULT.lock().unwrap() = None;

    let app = App::new();
    let records = collect(&app);

    let leaf = {
        let fiber = app.root.plugin(
            abi_plugin("trace-c", Inject::default(), leaf_call),
            Config::Null,
        );
        fiber.settle().await;
        fiber
    };
    let pending = {
        let fiber = app.root.plugin(
            abi_plugin("trace-p", Inject::default(), pending_call),
            Config::Null,
        );
        fiber.settle().await;
        fiber
    };
    let mid = {
        let fiber = app.root.plugin(
            abi_plugin("trace-m", Inject::of(&["p@1", "c@1"]), mid_call),
            Config::Null,
        );
        fiber.settle().await;
        fiber
    };
    let consumer = {
        let fiber = app.root.plugin(
            abi_plugin("trace-consumer-p", Inject::of(&["m@1"]), consumer_call),
            Config::Null,
        );
        fiber.settle().await;
        fiber
    };
    for fiber in [&leaf, &pending, &mid, &consumer] {
        assert_eq!(fiber.state(), FiberState::Active, "夹具必须全部装载成功");
    }

    tokio::time::timeout(Duration::from_secs(2), async {
        while CONSUMER_RESULT.lock().unwrap().is_none() {
            result_notify().notified().await;
        }
    })
    .await
    .expect("PENDING 链必须在时限内结算");
    assert_eq!(
        CONSUMER_RESULT.lock().unwrap().as_ref(),
        Some(&json!("leaf")),
        "最终结果必须来自恢复后的 c@1"
    );

    {
        let records = records.lock().unwrap();
        assert_eq!(records.len(), 3, "每跳恰好一条记录：{records:?}");
        let by_name = |name: &str| {
            records
                .iter()
                .find(|record| &*record.name == name)
                .unwrap_or_else(|| panic!("缺少记录 {name}：{records:?}"))
        };
        let mid_record = by_name("m@1.run");
        let pending_record = by_name("p@1.run");
        let leaf_record = by_name("c@1.run");

        assert_eq!(mid_record.parent_call_id, 0, "consumer 的 apply 是根");
        assert_eq!(
            pending_record.parent_call_id, mid_record.call_id,
            "PENDING 入站的父是发起它的那一跳"
        );
        assert_eq!(
            leaf_record.parent_call_id, mid_record.call_id,
            "续延恢复后的调用仍挂在发起方那一跳上"
        );
        assert_eq!(mid_record.outcome, Outcome::Ok);
        assert_eq!(pending_record.outcome, Outcome::Ok);
        assert_eq!(leaf_record.outcome, Outcome::Ok);
        assert_eq!(
            pending_record.call_id,
            P_DISPATCH.load(Ordering::SeqCst),
            "记录里的 call_id 必须是提供者看到的 dispatch_id"
        );
        assert_eq!(mid_record.caller_uid, consumer.uid());
        assert_eq!(pending_record.caller_uid, mid.uid());
        assert_eq!(leaf_record.caller_uid, mid.uid());
        assert_eq!(mid_record.callee_uid, mid.uid());
        assert_eq!(pending_record.callee_uid, pending.uid());
        assert_eq!(leaf_record.callee_uid, leaf.uid());
        assert_eq!(&*mid_record.callee_plugin, "trace-m");
        assert_eq!(&*pending_record.callee_plugin, "trace-p");
        assert_eq!(&*leaf_record.callee_plugin, "trace-c");
        assert!(
            pending_record.elapsed >= Duration::from_millis(30),
            "PENDING 等待期必须计入 elapsed：{:?}",
            pending_record.elapsed
        );
        assert!(
            mid_record.elapsed >= Duration::from_millis(30),
            "外层调用覆盖整条等待链：{:?}",
            mid_record.elapsed
        );
    }

    consumer.dispose().await;
    mid.dispose().await;
    pending.dispose().await;
    leaf.dispose().await;
}
