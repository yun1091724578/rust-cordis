//! 取消路径：提供者返回 `STATUS_PENDING` 后，由 `ctx_abort` 收回派发。
//!
//! 断言 ADR 0009 的三类异常分类之一：取消结算写 `host_cancel`（不是超时码），
//! 且同一次调用只有这一条记录。

mod trace_support;

use cordis_abi::{
    Blob, Out, OP_APPLY, OP_CALLBACK, OP_DISPOSE, STATUS_OK, STATUS_PENDING, STATUS_UNSUPPORTED,
};
use cordis_core::{App, Config, FiberState, Inject, Outcome};
use cordis_host::host_api;
use serde_json::Value;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use trace_support::{abi_plugin, call_iface, collect, provide};

static CONSUMER_ERROR: Mutex<Option<Value>> = Mutex::new(None);
static H_DISPATCH: AtomicU64 = AtomicU64::new(0);

fn error_notify() -> &'static tokio::sync::Notify {
    static NOTIFY: OnceLock<tokio::sync::Notify> = OnceLock::new();
    NOTIFY.get_or_init(tokio::sync::Notify::new)
}

unsafe extern "C" fn hang_call(handle: u64, op: u32, a: Blob, _b: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => provide(handle, "h@1", r#"["run"]"#, 2, out),
        OP_CALLBACK => {
            let payload: Value = serde_json::from_slice(a.as_bytes()).unwrap_or(Value::Null);
            let Some(dispatch_id) = payload.get("dispatch_id").and_then(Value::as_u64) else {
                return STATUS_UNSUPPORTED;
            };
            H_DISPATCH.store(dispatch_id, Ordering::SeqCst);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(20));
                let Some(abort) = host_api().ctx_abort else {
                    return;
                };
                let mut buffer = [0u8; 256];
                let mut slot = Out {
                    ptr: buffer.as_mut_ptr(),
                    cap: buffer.len(),
                    len: 0,
                };
                let _ = abort(handle, dispatch_id, &mut slot);
            });
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
            let (status, text) = call_iface(handle, "h@1", "run", 7);
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
                *CONSUMER_ERROR.lock().unwrap() =
                    Some(payload["args"].get(0).cloned().unwrap_or(Value::Null));
                error_notify().notify_one();
            }
            (*out).write(b"")
        }
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

#[tokio::test]
async fn cancelled_pending_call_is_recorded_as_host_cancel() {
    *CONSUMER_ERROR.lock().unwrap() = None;
    H_DISPATCH.store(0, Ordering::SeqCst);

    let app = App::new();
    let records = collect(&app);

    let provider = {
        let fiber = app.root.plugin(
            abi_plugin("trace-h", Inject::default(), hang_call),
            Config::Null,
        );
        fiber.settle().await;
        fiber
    };
    let consumer = {
        let fiber = app.root.plugin(
            abi_plugin("trace-consumer-c", Inject::of(&["h@1"]), consumer_call),
            Config::Null,
        );
        fiber.settle().await;
        fiber
    };
    assert_eq!(provider.state(), FiberState::Active);
    assert_eq!(consumer.state(), FiberState::Active);
    let provider_uid = provider.uid();

    tokio::time::timeout(Duration::from_secs(2), async {
        while CONSUMER_ERROR.lock().unwrap().is_none() {
            error_notify().notified().await;
        }
    })
    .await
    .expect("取消必须在时限内交付");

    let error = CONSUMER_ERROR
        .lock()
        .unwrap()
        .clone()
        .expect("已等待到回调");
    assert_eq!(error["code"], "host_cancel", "取消码不能写成超时：{error}");

    {
        let records = records.lock().unwrap();
        assert_eq!(records.len(), 1, "取消只写一条记录：{records:?}");
        let record = &records[0];
        assert_eq!(&*record.name, "h@1.run");
        assert_ne!(record.call_id, 0, "入站派发 id 必须可复用");
        assert_eq!(
            record.call_id,
            H_DISPATCH.load(Ordering::SeqCst),
            "记录里的 call_id 必须是提供者看到的 dispatch_id"
        );
        assert_eq!(record.caller_uid, consumer.uid());
        assert_ne!(provider_uid, u64::MAX);
        assert_eq!(record.callee_uid, provider_uid, "callee_uid 是发起时快照");
        assert_ne!(record.callee_uid, u64::MAX);
        assert_eq!(record.outcome, Outcome::Err("host_cancel".into()));
        assert_eq!(&*record.callee_plugin, "trace-h");
    }

    consumer.dispose().await;
    provider.dispose().await;
}
