//! P0 回归：实例注销后，事件快照与延迟 iface 续延都不得再进入插件代码。
//!
//! 这些用例不需要真实 DLL：`apply_via` + 假 `CallFn` 就等价于 ABI 装载路径。
//! 计数用 `AtomicUsize` 而不是 Drop 探针，避免 fake `CallFn` 里 assert（D10）。

use cordis_abi::{
    Blob, Out, OP_APPLY, OP_CALLBACK, OP_DISPOSE, STATUS_OK, STATUS_PENDING, STATUS_UNSUPPORTED,
};
use cordis_core::{App, Config, Ctx, FnPlugin, Inject, Plugin};
use cordis_host::{apply_via, host_api, pending_len};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

fn plugin(name: &'static str, inject: Inject, call: cordis_host::CallFn) -> Arc<dyn Plugin> {
    Arc::new(FnPlugin {
        name,
        key: cordis_core::unique_key(name),
        inject,
        validator: None,
        f: Arc::new(move |ctx: Ctx, config: Config| apply_via(call, ctx, config, name)),
    })
}

fn make_out(buffer: &mut [u8]) -> Out {
    Out {
        ptr: buffer.as_mut_ptr(),
        cap: buffer.len(),
        len: 0,
    }
}

// ---------------------------------------------------------------- 事件快照

static EVENT_CALLS: AtomicUsize = AtomicUsize::new(0);
static EVENT_DISPOSED: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn event_call(handle: u64, op: u32, _a: Blob, _b: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => {
            let on = host_api().ctx_on.expect("ctx_on");
            let mut buffer = [0u8; 128];
            let mut slot = make_out(&mut buffer);
            let status = on(handle, Blob::new(b"snapshot"), 1, &mut slot);
            if status != STATUS_OK {
                return status;
            }
            (*out).write(b"")
        }
        OP_CALLBACK => {
            EVENT_CALLS.fetch_add(1, Ordering::SeqCst);
            (*out).write(br#"{"ret":null}"#)
        }
        OP_DISPOSE => {
            EVENT_DISPOSED.fetch_add(1, Ordering::SeqCst);
            (*out).write(b"")
        }
        _ => STATUS_UNSUPPORTED,
    }
}

/// 事件派发已经取到 handler 快照、但 ABI 监听器还没跑时，插件被卸载。
/// 释放后快照继续跑：不得调用已注销实例，也不得 panic。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn event_snapshot_does_not_call_a_disposed_instance() {
    EVENT_CALLS.store(0, Ordering::SeqCst);
    EVENT_DISPOSED.store(0, Ordering::SeqCst);

    let app = App::new();
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    {
        let started = started.clone();
        let release = release.clone();
        app.root.on("snapshot", move |_call| {
            let started = started.clone();
            let release = release.clone();
            Box::pin(async move {
                started.notify_one();
                release.notified().await;
                None
            })
        });
    }

    let fiber = app.root.plugin(
        plugin("snapshot-abi", Inject::default(), event_call),
        Config::Null,
    );
    fiber.settle().await;

    let root = app.root.clone();
    let emit = tokio::spawn(async move {
        root.emit("snapshot", vec![]).await;
    });

    // 等 blocker 进入 handler；此时 handler 快照已经包含 ABI 监听器。
    tokio::time::timeout(Duration::from_secs(2), started.notified())
        .await
        .expect("blocker 应开始");

    fiber.dispose().await;
    assert_eq!(
        EVENT_DISPOSED.load(Ordering::SeqCst),
        1,
        "正常卸载应收到 OP_DISPOSE"
    );

    release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), emit)
        .await
        .expect("事件派发不得挂死")
        .expect("emit task 不应 panic");

    assert_eq!(
        EVENT_CALLS.load(Ordering::SeqCst),
        0,
        "快照不得调用已注销实例"
    );
}

// ---------------------------------------------------------------- 延迟 iface 续延

static PROVIDER_HANDLE: AtomicU64 = AtomicU64::new(0);
static CONSUMER_HANDLE: AtomicU64 = AtomicU64::new(0);
static IFACE_DISPATCH: AtomicU64 = AtomicU64::new(0);
static CONSUMER_CALLBACKS: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn provider_call(handle: u64, op: u32, a: Blob, _b: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => {
            PROVIDER_HANDLE.store(handle, Ordering::SeqCst);
            let api = host_api().ctx_provide_iface.expect("ctx_provide_iface");
            let mut buffer = [0u8; 128];
            let mut slot = make_out(&mut buffer);
            let status = api(
                handle,
                Blob::new(b"svc"),
                1,
                Blob::new(br#"["m"]"#),
                10,
                &mut slot,
            );
            if status != STATUS_OK {
                return status;
            }
            (*out).write(b"")
        }
        OP_CALLBACK => {
            // 挂起：把 dispatch_id 留给测试，稍后由 provider 侧 fulfill。
            let payload = String::from_utf8_lossy(unsafe { a.as_bytes() });
            let value: serde_json::Value = serde_json::from_str(&payload).unwrap_or_default();
            if value.get("id").and_then(|id| id.as_u64()) == Some(10) {
                if let Some(id) = value.get("dispatch_id").and_then(|id| id.as_u64()) {
                    IFACE_DISPATCH.store(id, Ordering::SeqCst);
                }
                return STATUS_PENDING;
            }
            (*out).write(b"")
        }
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

unsafe extern "C" fn consumer_call(handle: u64, op: u32, _a: Blob, _b: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => {
            CONSUMER_HANDLE.store(handle, Ordering::SeqCst);
            let api = host_api().ctx_call_iface.expect("ctx_call_iface");
            let mut buffer = [0u8; 256];
            let mut slot = make_out(&mut buffer);
            let status = api(
                handle,
                Blob::new(b"svc"),
                1,
                Blob::new(b"m"),
                Blob::new(b"{}"),
                20,
                &mut slot,
            );
            if status != STATUS_OK {
                return status;
            }
            (*out).write(b"")
        }
        OP_CALLBACK => {
            CONSUMER_CALLBACKS.fetch_add(1, Ordering::SeqCst);
            (*out).write(b"")
        }
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

/// 消费者卸载后，这场在飞调用必须**被撤销**：消费者侧记录排空，任务被取消，
/// 提供者的挂起续延也随之撤销。稍后 provider 再 fulfill 不得成功，更不得回调消费者。
///
/// 这比"只屏蔽结果回调"更强：调用方放弃等待后，提供者不应该继续持有一条
/// 已经没人接收的续延。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deferred_iface_callback_is_suppressed_after_consumer_unload() {
    IFACE_DISPATCH.store(0, Ordering::SeqCst);
    CONSUMER_CALLBACKS.store(0, Ordering::SeqCst);

    let app = App::new();
    let provider = app.root.plugin(
        plugin("iface-provider", Inject::default(), provider_call),
        Config::Null,
    );
    provider.settle().await;
    assert_eq!(provider.state(), cordis_core::FiberState::Active);

    let consumer = app.root.plugin(
        plugin("iface-consumer", Inject::of(&["svc"]), consumer_call),
        Config::Null,
    );
    consumer.settle().await;
    assert_eq!(consumer.state(), cordis_core::FiberState::Active);

    // 等 provider 的挂起回调把 dispatch_id 交给测试。
    for _ in 0..200 {
        if IFACE_DISPATCH.load(Ordering::SeqCst) != 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let dispatch_id = IFACE_DISPATCH.load(Ordering::SeqCst);
    assert_ne!(dispatch_id, 0, "provider 应已挂起");
    let consumer_handle = CONSUMER_HANDLE.load(Ordering::SeqCst);

    consumer.dispose().await;
    assert_eq!(
        pending_len(consumer_handle),
        0,
        "消费者卸载必须排空 iface 记录"
    );
    assert_eq!(
        pending_len(PROVIDER_HANDLE.load(Ordering::SeqCst)),
        0,
        "调用方放弃等待后，提供者的挂起续延也必须被撤销"
    );
    let after_drain = CONSUMER_CALLBACKS.load(Ordering::SeqCst);

    // 提供者现在才完成：这场调用已经被撤销，fulfill 必须报"不存在/已结算"。
    let mut buffer = [0u8; 128];
    let mut slot = make_out(&mut buffer);
    let status = unsafe {
        host_api().ctx_fulfill.expect("ctx_fulfill")(
            PROVIDER_HANDLE.load(Ordering::SeqCst),
            dispatch_id,
            Blob::new(br#"{"result":1}"#),
            &mut slot,
        )
    };
    assert_ne!(
        status, STATUS_OK,
        "已撤销的派发不能再 fulfill 成功（否则插件会以为结果有人接收）"
    );
    let message = String::from_utf8_lossy(&buffer[..slot.len]).into_owned();
    assert!(
        message.contains("不存在") || message.contains("已结算"),
        "错误应说明派发已撤销：{message}"
    );

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        CONSUMER_CALLBACKS.load(Ordering::SeqCst),
        after_drain,
        "消费者卸载后的延迟 on_done 不得再回调"
    );
}
