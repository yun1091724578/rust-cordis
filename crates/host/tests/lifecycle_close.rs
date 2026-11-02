//! 统一关闭契约：`App::close` 必须排空宿主的在飞派发并回收实例。
//!
//! 独立测试二进制：`loaded_instances()` 是进程级计数，这里只跑一条关闭路径。

use cordis_abi::{
    Blob, Out, OP_APPLY, OP_CALLBACK, OP_DISPOSE, STATUS_OK, STATUS_PENDING, STATUS_UNSUPPORTED,
};
use cordis_core::{App, Config, Ctx, Inject, Plugin};
use cordis_host::{
    apply_via, dispatches_len, host_api, loaded_instances, pending_len, streams_len,
};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

static HANDLE: AtomicU64 = AtomicU64::new(0);
static DISPOSED: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn close_probe(handle: u64, op: u32, _: Blob, _: Blob, out: *mut Out) -> i32 {
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
            if on(handle, Blob::new(b"close/event"), 1, &mut event_out) != STATUS_OK {
                return STATUS_UNSUPPORTED;
            }
            (*out).write(b"")
        }
        // 永不应答：关闭时必须由宿主把挂起记录结算失败。
        OP_CALLBACK => STATUS_PENDING,
        OP_DISPOSE => {
            DISPOSED.fetch_add(1, Ordering::SeqCst);
            (*out).write(b"")
        }
        _ => STATUS_UNSUPPORTED,
    }
}

async fn wait_for(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while !predicate() {
        assert!(Instant::now() < deadline, "等待条件超时");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn app_close_drains_pending_dispatch_and_unregisters_instance() {
    assert_eq!(loaded_instances(), 0, "测试开始前不得有残留实例");

    let app = App::new();
    let plugin: Arc<dyn Plugin> = Arc::new(cordis_core::FnPlugin {
        name: "lifecycle-close",
        key: cordis_core::unique_key("lifecycle-close"),
        inject: Inject::default(),
        validator: None,
        f: Arc::new(|ctx: Ctx, config: Config| {
            apply_via(close_probe, ctx, config, "lifecycle-close")
        }),
    });
    let fiber = app.plugin(plugin, Config::Null).await.unwrap();
    let handle = HANDLE.load(Ordering::SeqCst);
    assert_ne!(handle, 0);
    assert_eq!(loaded_instances(), 1);

    let root = app.root.clone();
    let store = app.store.clone();
    let waiting = tokio::spawn(async move { root.bail("close/event", vec![]).await });
    wait_for(|| pending_len(handle) == 1).await;

    app.close().await;

    let outcome = tokio::time::timeout(Duration::from_secs(2), waiting)
        .await
        .expect("被关闭排空的调用必须结束，不得悬空")
        .expect("调用任务不得 panic");
    assert!(outcome.is_none(), "关闭必须让在飞调用结束，而不是继续挂起");

    assert_eq!(
        DISPOSED.load(Ordering::SeqCst),
        1,
        "成功 apply 的实例必须收到 OP_DISPOSE"
    );
    assert!(fiber.is_disposed(), "App::close 必须处置插件 fiber");
    assert_eq!(loaded_instances(), 0, "实例必须从宿主表移除");
    assert_eq!(pending_len(handle), 0, "挂起续延必须清空");
    assert_eq!(dispatches_len(handle), 0, "派发记录必须清空");
    assert_eq!(streams_len(handle), 0, "流记录必须清空");
    assert_eq!(
        store.hook_count("close/event"),
        0,
        "插件注册的宿主 hook 必须摘除"
    );
}
