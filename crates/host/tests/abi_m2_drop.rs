//! M2：调用方丢弃 `Future` 时的清理契约。
//!
//! 独立测试二进制：不修改进程级看门狗设置，只验证"没有等到结算就被丢弃"的路径。

use cordis_abi::{
    Blob, Out, OP_APPLY, OP_CALLBACK, OP_DISPOSE, STATUS_PENDING, STATUS_UNSUPPORTED,
};
use cordis_core::{App, Config, Ctx, FiberState, Inject, Plugin};
use cordis_host::{apply_via, dispatches_len, host_api, pending_len};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

static HANDLE: AtomicU64 = AtomicU64::new(0);

unsafe extern "C" fn pending_call(handle: u64, op: u32, _: Blob, _: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => {
            HANDLE.store(handle, Ordering::SeqCst);
            let Some(on) = host_api().ctx_on else {
                return STATUS_UNSUPPORTED;
            };
            let mut buffer = [0u8; 64];
            let mut event_out = Out {
                ptr: buffer.as_mut_ptr(),
                cap: buffer.len(),
                len: 0,
            };
            on(handle, Blob::new(b"drop/event"), 1, &mut event_out);
            (*out).write(b"")
        }
        // 永不应答：让调用方只能超时丢弃 Future。
        OP_CALLBACK => STATUS_PENDING,
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

/// 原生调用方用 `tokio::time::timeout` 放弃等待后，挂起记录必须当场撤销。
#[tokio::test]
async fn dropping_the_call_future_removes_the_pending_record() {
    let app = App::new();
    let plugin: Arc<dyn Plugin> = Arc::new(cordis_core::FnPlugin {
        name: "m2-drop",
        key: cordis_core::unique_key("m2-drop"),
        inject: Inject::default(),
        validator: None,
        f: Arc::new(|ctx: Ctx, config: Config| apply_via(pending_call, ctx, config, "m2-drop")),
    });
    let fiber = app.root.plugin(plugin, Config::Null);
    fiber.settle().await;
    assert_eq!(fiber.state(), FiberState::Active);
    let handle = HANDLE.load(Ordering::SeqCst);
    assert_ne!(handle, 0);

    let outcome = tokio::time::timeout(
        Duration::from_millis(20),
        app.root.bail("drop/event", vec![]),
    )
    .await;
    assert!(outcome.is_err(), "插件永不应答，调用必须超时");

    assert_eq!(pending_len(handle), 0, "Future 被丢弃后挂起条目必须撤销");
    assert_eq!(dispatches_len(handle), 0, "派发记录也必须一并撤销");

    fiber.dispose().await;
}
