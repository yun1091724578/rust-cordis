//! `ctx_next` 剩余链的看门狗。
//!
//! 独立测试二进制：`DispatchPolicy` 是进程级设置。夹具铁律：fake `CallFn`
//! 内禁止 assert，结局只记进 static `Mutex`。

use cordis_abi::{
    Blob, Out, KIND_EVENT, KIND_NEXT, OP_APPLY, OP_CALLBACK, OP_DISPOSE, STATUS_ERR, STATUS_OK,
    STATUS_PENDING, STATUS_UNSUPPORTED,
};
use cordis_core::{App, Config, Ctx, ExternValue, Inject, Plugin, Ret};
use cordis_host::{
    apply_via_with_error_format, dispatches_len, host_api, pending_len, set_dispatch_policy,
    waterfall_len, DispatchPolicy,
};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

static HANDLE: AtomicU64 = AtomicU64::new(0);
static DISPATCH: AtomicU64 = AtomicU64::new(0);
static EVENTS: Mutex<Vec<String>> = Mutex::new(Vec::new());
static NEXT_ERROR: Mutex<Value> = Mutex::new(Value::Null);

unsafe fn on_event(handle: u64, event: &str, cb: u64) -> i32 {
    let Some(api) = host_api().ctx_on else {
        return -1;
    };
    let mut buffer = [0u8; 256];
    let mut out = Out {
        ptr: buffer.as_mut_ptr(),
        cap: buffer.len(),
        len: 0,
    };
    api(handle, Blob::new(event.as_bytes()), cb, &mut out)
}

unsafe fn next_call(handle: u64, dispatch_id: u64, on_done: u64) -> (i32, String) {
    let Some(api) = host_api().ctx_next else {
        return (-1, "宿主不支持 ctx_next".into());
    };
    let payload = json!({ "args": [] }).to_string();
    let mut buffer = [0u8; 512];
    let mut out = Out {
        ptr: buffer.as_mut_ptr(),
        cap: buffer.len(),
        len: 0,
    };
    let status = api(
        handle,
        dispatch_id,
        Blob::new(payload.as_bytes()),
        on_done,
        &mut out,
    );
    (
        status,
        String::from_utf8_lossy(&buffer[..out.len]).into_owned(),
    )
}

unsafe fn fulfill(handle: u64, dispatch_id: u64, value: Value) -> i32 {
    let Some(api) = host_api().ctx_fulfill else {
        return -1;
    };
    let payload = value.to_string();
    let mut buffer = [0u8; 512];
    let mut out = Out {
        ptr: buffer.as_mut_ptr(),
        cap: buffer.len(),
        len: 0,
    };
    api(handle, dispatch_id, Blob::new(payload.as_bytes()), &mut out)
}

unsafe extern "C" fn next_plugin(handle: u64, op: u32, a: Blob, _: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => {
            HANDLE.store(handle, Ordering::SeqCst);
            let status = on_event(handle, "w-next", 1);
            if status == STATUS_OK {
                (*out).write(b"")
            } else {
                status
            }
        }
        OP_CALLBACK => {
            let payload: Value = serde_json::from_slice(a.as_bytes()).unwrap_or(Value::Null);
            match payload.get("kind").and_then(Value::as_str) {
                Some(KIND_EVENT) => {
                    let dispatch_id = payload
                        .get("dispatch_id")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    DISPATCH.store(dispatch_id, Ordering::SeqCst);
                    EVENTS.lock().unwrap().push(format!("event:{dispatch_id}"));
                    let (status, text) = next_call(handle, dispatch_id, 2);
                    if status == STATUS_OK {
                        STATUS_PENDING
                    } else {
                        (*out).write(text.as_bytes());
                        STATUS_ERR
                    }
                }
                Some(KIND_NEXT) => {
                    let arg = payload["args"][0].clone();
                    *NEXT_ERROR.lock().unwrap() = arg.clone();
                    let error = arg
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("<no-error>")
                        .to_string();
                    EVENTS.lock().unwrap().push(format!("next:{error}"));
                    // 收到错误后显式结算，让这次 waterfall 正常收尾。
                    let status = fulfill(
                        handle,
                        DISPATCH.load(Ordering::SeqCst),
                        json!("timeout-fallback"),
                    );
                    if status == STATUS_OK {
                        (*out).write(b"")
                    } else {
                        status
                    }
                }
                _ => (*out).write(b""),
            }
        }
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

fn abi_plugin() -> Arc<dyn Plugin> {
    Arc::new(cordis_core::FnPlugin {
        name: "next-timeout",
        key: cordis_core::unique_key("next-timeout"),
        inject: Inject::default(),
        validator: None,
        f: Arc::new(|ctx: Ctx, config: Config| {
            apply_via_with_error_format(
                next_plugin,
                ctx,
                config,
                "next-timeout",
                cordis_abi::ERROR_FORMAT_V1,
            )
        }),
    })
}

#[tokio::test]
async fn ctx_next_times_out_and_delivers_an_error_to_on_done() {
    set_dispatch_policy(DispatchPolicy {
        outbound_timeout_ms: Some(50),
        ..DispatchPolicy::default()
    });
    HANDLE.store(0, Ordering::SeqCst);
    DISPATCH.store(0, Ordering::SeqCst);
    EVENTS.lock().unwrap().clear();
    *NEXT_ERROR.lock().unwrap() = Value::Null;

    let app = App::new();
    let fiber = app.root.plugin(abi_plugin(), Config::Null);
    fiber.settle().await;
    let handle = HANDLE.load(Ordering::SeqCst);
    assert_ne!(handle, 0, "插件必须拿到 ABI 句柄");

    let tail: cordis_core::BoxFuture<Ret> = Box::pin(async {
        std::future::pending::<()>().await;
        None
    });
    let ret = tokio::time::timeout(
        Duration::from_secs(2),
        app.root.waterfall("w-next", vec![], Some(tail), None),
    )
    .await
    .expect("超时后 waterfall 必须由插件的 fulfill 收尾");
    assert_eq!(
        ret.and_then(|value| value.downcast::<ExternValue>())
            .map(|value| value.0.clone()),
        Some(json!("timeout-fallback")),
        "插件在错误回调里 fulfill 的终值必须回到派发方"
    );

    let events = EVENTS.lock().unwrap().clone();
    let next = events
        .iter()
        .find(|line| line.starts_with("next:"))
        .unwrap_or_else(|| panic!("必须收到 kind:next：{events:?}"));
    assert!(
        next.contains("HOST_TIMEOUT") && next.contains("waterfall"),
        "错误必须可读并指出剩余链：{next}"
    );
    let error = NEXT_ERROR.lock().unwrap().clone();
    assert_eq!(error["code"], "HOST_TIMEOUT", "next 超时必须带稳定 code");
    assert_eq!(error["category"], "timeout");
    assert_eq!(
        error["certainty"], "unknown",
        "调用方不能把 next 超时误读成未执行"
    );
    assert_eq!(pending_len(handle), 0, "fulfill 后不得留下挂起条目");
    assert_eq!(waterfall_len(handle), 0);
    assert_eq!(dispatches_len(handle), 0, "派发记录必须归零");

    fiber.dispose().await;
    set_dispatch_policy(DispatchPolicy::default());
}
