//! 出站 `ctx_call_iface` 的看门狗。
//!
//! 独立测试二进制：`DispatchPolicy` 是进程级设置。夹具铁律：fake `CallFn`
//! 内禁止 assert，结局只记进 static `Mutex`。

use cordis_abi::{
    Blob, Out, KIND_IFACE, OP_APPLY, OP_CALLBACK, OP_DISPOSE, STATUS_OK, STATUS_UNSUPPORTED,
};
use cordis_core::{App, BoxFuture, Config, Ctx, ExternService, Inject, Plugin, Result};
use cordis_host::{
    apply_via_with_error_format, dispatches_len, host_api, pending_len, set_dispatch_policy,
    DispatchPolicy,
};
use serde_json::Value;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

static CONSUMER: AtomicU64 = AtomicU64::new(0);
static APPLY_OUT: Mutex<String> = Mutex::new(String::new());
static CALLBACKS: Mutex<Vec<Value>> = Mutex::new(Vec::new());

struct NeverService;

impl ExternService for NeverService {
    fn service(&self) -> &str {
        "slow@1"
    }

    fn version(&self) -> u32 {
        1
    }

    fn methods(&self) -> Vec<String> {
        vec!["hang".into()]
    }

    fn call(
        &self,
        _caller_meta: Option<Config>,
        _method: String,
        _args: Config,
    ) -> BoxFuture<Result<Config>> {
        Box::pin(std::future::pending::<Result<Config>>())
    }
}

unsafe extern "C" fn consumer_call(handle: u64, op: u32, a: Blob, _: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => {
            CONSUMER.store(handle, Ordering::SeqCst);
            let Some(call) = host_api().ctx_call_iface else {
                return STATUS_UNSUPPORTED;
            };
            let mut buffer = [0u8; 512];
            let mut slot = Out {
                ptr: buffer.as_mut_ptr(),
                cap: buffer.len(),
                len: 0,
            };
            let status = call(
                handle,
                Blob::new(b"slow@1"),
                1,
                Blob::new(b"hang"),
                Blob::new(b"[]"),
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
            if payload.get("kind").and_then(Value::as_str) == Some(KIND_IFACE) {
                CALLBACKS.lock().unwrap().push(payload);
            }
            (*out).write(b"")
        }
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

fn provider() -> Arc<dyn Plugin> {
    cordis_core::plugin_fn(
        "slow-provider",
        Inject::default(),
        |ctx: Ctx, _config: Config| {
            Box::pin(async move {
                ctx.provide_iface("slow@1", Arc::new(NeverService))?;
                Ok(())
            })
        },
    )
}

fn consumer() -> Arc<dyn Plugin> {
    Arc::new(cordis_core::FnPlugin {
        name: "slow-consumer",
        key: cordis_core::unique_key("slow-consumer"),
        inject: Inject::of(&["slow@1"]),
        validator: None,
        f: Arc::new(|ctx: Ctx, config: Config| {
            apply_via_with_error_format(
                consumer_call,
                ctx,
                config,
                "slow-consumer",
                cordis_abi::ERROR_FORMAT_V1,
            )
        }),
    })
}

#[tokio::test]
async fn iface_call_times_out_and_unregisters_dispatch() {
    set_dispatch_policy(DispatchPolicy {
        outbound_timeout_ms: Some(50),
        ..DispatchPolicy::default()
    });
    CALLBACKS.lock().unwrap().clear();
    APPLY_OUT.lock().unwrap().clear();
    CONSUMER.store(0, Ordering::SeqCst);

    let app = App::new();
    let provider_fiber = app.root.plugin(provider(), Config::Null);
    provider_fiber.settle().await;
    let consumer_fiber = app.root.plugin(consumer(), Config::Null);
    consumer_fiber.settle().await;

    let handle = CONSUMER.load(Ordering::SeqCst);
    assert_ne!(handle, 0, "consumer 必须拿到 ABI 句柄");
    assert_eq!(
        dispatches_len(handle),
        1,
        "调用应先挂起；apply 返回={:?}",
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
        error.contains("HOST_TIMEOUT") && error.contains("slow@1.hang"),
        "错误必须可读并指出调用：{error}"
    );
    assert_eq!(
        payload["args"][0]["code"], "HOST_TIMEOUT",
        "超时必须带稳定 code"
    );
    assert_eq!(
        payload["args"][0]["category"], "timeout",
        "超时必须带 timeout 分类"
    );
    assert_eq!(
        payload["args"][0]["certainty"], "unknown",
        "调用方不能把超时误读成未执行"
    );
    assert_eq!(dispatches_len(handle), 0, "超时后必须撤销派发记录");
    assert_eq!(pending_len(handle), 0, "超时后挂起表必须归零");

    consumer_fiber.dispose().await;
    provider_fiber.dispose().await;
    set_dispatch_policy(DispatchPolicy::default());
}
