//! 流式挂起的看门狗：`DispatchPolicy` 必须覆盖 `call_iface_stream` 的挂起路径。
//!
//! 独立测试二进制：看门狗是进程级设置。

use cordis_abi::{
    Blob, Out, OP_APPLY, OP_CALLBACK, OP_DISPOSE, STATUS_PENDING, STATUS_UNSUPPORTED,
};
use cordis_core::{App, Config, Ctx, FiberState, Inject, Plugin};
use cordis_host::{
    apply_via, dispatches_len, host_api, pending_len, set_dispatch_policy, streams_len,
    DispatchPolicy,
};
use serde_json::json;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

static HANDLE: AtomicU64 = AtomicU64::new(0);
const IFACE: u64 = 31;

unsafe extern "C" fn hanging_stream_call(
    handle: u64,
    op: u32,
    _: Blob,
    _: Blob,
    out: *mut Out,
) -> i32 {
    match op {
        OP_APPLY => {
            HANDLE.store(handle, Ordering::SeqCst);
            let Some(provide) = host_api().ctx_provide_iface else {
                return STATUS_UNSUPPORTED;
            };
            let mut buffer = [0u8; 256];
            let mut iface_out = Out {
                ptr: buffer.as_mut_ptr(),
                cap: buffer.len(),
                len: 0,
            };
            provide(
                handle,
                Blob::new(b"s@1"),
                1,
                Blob::new(br#"["hang"]"#),
                IFACE,
                &mut iface_out,
            );
            (*out).write(b"")
        }
        // 挂起后既不推块也不收尾：只能由宿主看门狗收回。
        OP_CALLBACK => STATUS_PENDING,
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

#[tokio::test(start_paused = true)]
async fn stream_dispatch_policy_timeout_fails_the_stream_and_removes_the_record() {
    set_dispatch_policy(DispatchPolicy {
        timeout_ms: Some(10),
        ..DispatchPolicy::default()
    });

    let app = App::new();
    let plugin: Arc<dyn Plugin> = Arc::new(cordis_core::FnPlugin {
        name: "stream-timeout",
        key: cordis_core::unique_key("stream-timeout"),
        inject: Inject::default(),
        validator: None,
        f: Arc::new(|ctx: Ctx, config: Config| {
            apply_via(hanging_stream_call, ctx, config, "stream-timeout")
        }),
    });
    let fiber = app.root.plugin(plugin, Config::Null);
    fiber.settle().await;
    assert_eq!(fiber.state(), FiberState::Active);
    let handle = HANDLE.load(Ordering::SeqCst);
    assert_ne!(handle, 0);

    let mut stream = app
        .root
        .call_iface_stream("s@1", 1, "hang", json!([]))
        .expect("开流必须成功");
    assert_eq!(pending_len(handle), 1, "挂起的流必须在宿主侧留一条记录");

    let outcome = tokio::time::timeout(Duration::from_secs(1), stream.next())
        .await
        .expect("看门狗必须在时限内结束这条流");
    let error = outcome.expect_err("超时的流必须以错误结束，而不是 Ok(None)");
    assert!(
        error.to_string().contains("超时"),
        "错误必须说明是超时：{error}"
    );

    assert_eq!(pending_len(handle), 0, "超时后挂起条目必须撤销");
    assert_eq!(streams_len(handle), 0, "超时后流记录必须撤销");
    assert_eq!(dispatches_len(handle), 0, "超时后全部派发记录必须归零");

    fiber.dispose().await;
    set_dispatch_policy(DispatchPolicy::default());
}
