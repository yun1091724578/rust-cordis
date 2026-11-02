//! 出站 `ctx_dispatch` / `ctx_emit` 的看门狗。
//!
//! 独立测试二进制：`DispatchPolicy` 是进程级设置。夹具铁律：fake `CallFn`
//! 内禁止 assert，结局只记进 static `Mutex`。

use cordis_abi::{
    Blob, Out, KIND_DISPATCH, OP_APPLY, OP_CALLBACK, OP_DISPOSE, STATUS_OK, STATUS_UNSUPPORTED,
};
use cordis_core::{App, Config, Ctx, Dyn, Inject, Plugin};
use cordis_host::{
    apply_via_with_error_format, dispatches_len, host_api, set_dispatch_policy, DispatchPolicy,
};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

static CONSUMER: AtomicU64 = AtomicU64::new(0);
static APPLY_OUT: Mutex<String> = Mutex::new(String::new());
static CALLBACKS: Mutex<Vec<Value>> = Mutex::new(Vec::new());

unsafe extern "C" fn consumer_call(handle: u64, op: u32, a: Blob, _: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => {
            CONSUMER.store(handle, Ordering::SeqCst);
            let Some(dispatch) = host_api().ctx_dispatch else {
                return STATUS_UNSUPPORTED;
            };
            let mut buffer = [0u8; 512];
            let mut slot = Out {
                ptr: buffer.as_mut_ptr(),
                cap: buffer.len(),
                len: 0,
            };
            let status = dispatch(
                handle,
                Blob::new(b"slow"),
                Blob::new(br#"{"mode":"emit","args":[]}"#),
                7,
                &mut slot,
            );
            if status != STATUS_OK {
                return status;
            }
            let text = String::from_utf8_lossy(&buffer[..slot.len]).into_owned();
            *APPLY_OUT.lock().unwrap() = text.clone();
            (*out).write(text.as_bytes())
        }
        OP_CALLBACK => {
            let payload: Value = serde_json::from_slice(a.as_bytes()).unwrap_or(Value::Null);
            if payload.get("kind").and_then(Value::as_str) == Some(KIND_DISPATCH) {
                CALLBACKS.lock().unwrap().push(payload);
            }
            (*out).write(b"")
        }
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

fn consumer() -> Arc<dyn Plugin> {
    Arc::new(cordis_core::FnPlugin {
        name: "dispatch-consumer",
        key: cordis_core::unique_key("dispatch-consumer"),
        inject: Inject::default(),
        validator: None,
        f: Arc::new(|ctx: Ctx, config: Config| {
            apply_via_with_error_format(
                consumer_call,
                ctx,
                config,
                "dispatch-consumer",
                cordis_abi::ERROR_FORMAT_V1,
            )
        }),
    })
}

#[tokio::test]
async fn outbound_dispatch_times_out_and_unregisters_dispatch() {
    set_dispatch_policy(DispatchPolicy {
        outbound_timeout_ms: Some(50),
        ..DispatchPolicy::default()
    });
    CALLBACKS.lock().unwrap().clear();
    APPLY_OUT.lock().unwrap().clear();
    CONSUMER.store(0, Ordering::SeqCst);

    let app = App::new();
    // 原生监听器永不返回：只有宿主看门狗能结束这次派发。
    let _listener = app.root.on("slow", |_call| {
        Box::pin(async {
            std::future::pending::<()>().await;
            Some(Dyn::new(json!(null)))
        })
    });

    let fiber = app.root.plugin(consumer(), Config::Null);
    fiber.settle().await;

    let handle = CONSUMER.load(Ordering::SeqCst);
    assert_ne!(handle, 0, "consumer 必须拿到 ABI 句柄");
    assert_eq!(
        dispatches_len(handle),
        1,
        "派发应先挂起；apply 返回={:?}",
        APPLY_OUT.lock().unwrap().as_str()
    );

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if !CALLBACKS.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("看门狗必须在时限内交付错误");

    let payload = CALLBACKS.lock().unwrap().first().cloned().unwrap();
    let error = payload["args"][0]["error"]
        .as_str()
        .expect("超时必须走 error 分支");
    assert!(
        error.contains("HOST_TIMEOUT") && error.contains("slow"),
        "错误必须可读并指出事件：{error}"
    );
    assert_eq!(payload["args"][0]["code"], "HOST_TIMEOUT");
    assert_eq!(payload["args"][0]["category"], "timeout");
    assert_eq!(payload["args"][0]["certainty"], "unknown");
    assert_eq!(dispatches_len(handle), 0, "超时后必须撤销派发记录");

    fiber.dispose().await;
    set_dispatch_policy(DispatchPolicy::default());
}
