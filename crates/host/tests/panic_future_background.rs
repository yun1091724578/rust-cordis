//! O3：宿主 poll 边界 panic 的**业务级**结局——后台 poll 那一条。
//!
//! 独立测试二进制，只走公开入口：`app.root.on` 的 panicking listener + `FnPlugin`
//! 的 `ctx_dispatch`。夹具铁律：假 `CallFn` 内禁止 assert / panic（跨 `extern "C"`
//! 的 panic 会 abort 整个进程）。
//!
//! 本文件必须保持**单测试**：结尾断言进程级绝对值 `loaded_instances() == 0`，
//! 第二个测试会让别的实例同时活着，libtest 并行时二者互相踩。

use cordis_abi::{
    Blob, Out, KIND_DISPATCH, OP_APPLY, OP_CALLBACK, OP_DISPOSE, STATUS_OK, STATUS_UNSUPPORTED,
};
use cordis_core::{App, Config, Ctx, Inject, Plugin};
use cordis_host::{apply_via_with_error_format, dispatches_len, host_api};
use serde_json::Value;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

fn callback_notify() -> &'static tokio::sync::Notify {
    static NOTIFY: OnceLock<tokio::sync::Notify> = OnceLock::new();
    NOTIFY.get_or_init(tokio::sync::Notify::new)
}

unsafe fn dispatch(handle: u64, event: &str, body: &str, continuation: u64) -> (i32, String) {
    let Some(api) = host_api().ctx_dispatch else {
        return (-1, "宿主不支持 ctx_dispatch".into());
    };
    let mut buffer = vec![0u8; 4096];
    let mut out = Out {
        ptr: buffer.as_mut_ptr(),
        cap: buffer.len(),
        len: 0,
    };
    let status = api(
        handle,
        Blob::new(event.as_bytes()),
        Blob::new(body.as_bytes()),
        continuation,
        &mut out,
    );
    (
        status,
        String::from_utf8_lossy(&buffer[..out.len.min(buffer.len())]).into_owned(),
    )
}

/// 看门狗：真正的同步靠 `Notify`。
async fn wait_callbacks(callbacks: &Mutex<Vec<Value>>) {
    let notify = callback_notify();
    tokio::time::timeout(Duration::from_secs(2), async {
        while callbacks.lock().unwrap().is_empty() {
            notify.notified().await;
        }
    })
    .await
    .expect("看门狗：回调未在时限内到达");
}

static FLOW_APPLY: Mutex<Option<(i32, String)>> = Mutex::new(None);
static FLOW_CALLBACKS: Mutex<Vec<Value>> = Mutex::new(Vec::new());
static FLOW_DISPOSES: AtomicU64 = AtomicU64::new(0);
static FLOW_HANDLE: AtomicU64 = AtomicU64::new(0);

fn flow_plugin() -> Arc<dyn Plugin> {
    cordis_core::plugin_fn(
        "panic-flow",
        Inject::default(),
        |ctx: Ctx, _config: Config| {
            Box::pin(async move {
                apply_via_with_error_format(
                    flow_call,
                    ctx,
                    Config::Null,
                    "panic-flow",
                    cordis_abi::ERROR_FORMAT_V1,
                )
                .await
            })
        },
    )
}

unsafe extern "C" fn flow_call(handle: u64, op: u32, args: Blob, _: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => {
            FLOW_HANDLE.store(handle, Ordering::SeqCst);
            let outcome = dispatch(handle, "boom", r#"{"args":[],"mode":"bail"}"#, 11);
            if let Ok(mut slot) = FLOW_APPLY.lock() {
                *slot = Some(outcome);
            }
            (*out).write(b"")
        }
        OP_CALLBACK => {
            let payload: Value = serde_json::from_slice(args.as_bytes()).unwrap_or(Value::Null);
            if let Ok(mut callbacks) = FLOW_CALLBACKS.lock() {
                callbacks.push(payload);
            }
            callback_notify().notify_one();
            (*out).write(b"")
        }
        OP_DISPOSE => {
            FLOW_DISPOSES.fetch_add(1, Ordering::SeqCst);
            (*out).write(b"")
        }
        _ => STATUS_UNSUPPORTED,
    }
}

#[tokio::test]
async fn o3_background_panic_reaches_plugin_once_without_echo_at_close() {
    FLOW_APPLY.lock().unwrap().take();
    FLOW_CALLBACKS.lock().unwrap().clear();
    FLOW_DISPOSES.store(0, Ordering::SeqCst);
    FLOW_HANDLE.store(0, Ordering::SeqCst);

    let gate = Arc::new(tokio::sync::Notify::new());
    let app = App::new();
    // 注入点：`app.root.on` 的监听器闭包——先等闸门，再在后台 poll 里 panic。
    let hook = {
        let gate = gate.clone();
        app.root.on("boom", move |_call| {
            let gate = gate.clone();
            Box::pin(async move {
                gate.notified().await;
                panic!("dispatch 监听器后台 poll panic（集成测试注入）")
            })
        })
    };

    let fiber = app.root.plugin(flow_plugin(), Config::Null);
    fiber.settle().await;

    let handle = FLOW_HANDLE.load(Ordering::SeqCst);
    assert_ne!(handle, 0, "插件必须拿到 ABI 句柄");
    let (status, text) = FLOW_APPLY
        .lock()
        .unwrap()
        .clone()
        .expect("apply 期必须真调过一次 ctx_dispatch");
    assert_eq!(status, STATUS_OK, "未就绪时应当挂起：{text}");
    let pending: Value = serde_json::from_str(&text).expect("挂起响应应当是 JSON");
    assert_eq!(
        pending["pending"], true,
        "挂起响应应当带 pending:true：{text}"
    );
    assert_eq!(dispatches_len(handle), 1, "挂起期间必须有一条在飞记录");

    gate.notify_one();
    wait_callbacks(&FLOW_CALLBACKS).await;

    let callbacks = FLOW_CALLBACKS.lock().unwrap().clone();
    assert_eq!(callbacks.len(), 1, "必须恰好一条回调：{callbacks:?}");
    assert_eq!(callbacks[0]["kind"], KIND_DISPATCH);
    assert_eq!(callbacks[0]["id"], 11);
    assert_eq!(callbacks[0]["args"][0]["code"], "host_panic");
    assert_eq!(callbacks[0]["args"][0]["certainty"], "unknown");
    assert_eq!(
        callbacks[0]["args"][0]["retry"], "unknown",
        "宿主不能声称重试安全：{callbacks:?}"
    );
    assert_eq!(
        dispatches_len(handle),
        0,
        "交付之后记录必须被摘掉（插件的下一次派发不该被配额吃掉）"
    );

    fiber.dispose().await;
    assert_eq!(
        FLOW_CALLBACKS.lock().unwrap().len(),
        1,
        "关闭不得补发第二条：{:?}",
        FLOW_CALLBACKS.lock().unwrap()
    );
    assert_eq!(FLOW_DISPOSES.load(Ordering::SeqCst), 1);
    drop(hook);
    assert_eq!(cordis_host::loaded_instances(), 0);
}
