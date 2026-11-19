//! 在飞实例卸载的**判别性**用例：调用方活过被调方。
//!
//! 根 ctx 没有 inject 依赖，提供者卸载不会把它一起拆掉；提供者的 pending sink
//! 收到 `host_lifecycle`，`settle` 仍会写一条记录。若 `callee_uid` 是结算时现场
//! 读取，这条记录会得到 `UID_DISPOSED`；只有发起时快照才能通过。

mod trace_support;

use cordis_abi::{
    Blob, Out, OP_APPLY, OP_CALLBACK, OP_DISPOSE, STATUS_PENDING, STATUS_UNSUPPORTED,
};
use cordis_core::{App, Config, FiberState, Inject, UID_ROOT};
use cordis_host::pending_len;
use serde_json::json;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use trace_support::{abi_plugin, collect, provide};

static PROVIDER_HANDLE: AtomicU64 = AtomicU64::new(0);

unsafe extern "C" fn hang_call(handle: u64, op: u32, _a: Blob, _b: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => {
            PROVIDER_HANDLE.store(handle, Ordering::SeqCst);
            provide(handle, "inflight@1", r#"["run"]"#, 1, out)
        }
        OP_CALLBACK => {
            (*out).write(b"");
            STATUS_PENDING
        }
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

#[tokio::test]
async fn unloading_a_pending_provider_keeps_the_start_time_callee() {
    PROVIDER_HANDLE.store(0, Ordering::SeqCst);

    let app = App::new();
    let records = collect(&app);

    let provider = {
        let fiber = app.root.plugin(
            abi_plugin("trace-unload", Inject::default(), hang_call),
            Config::Null,
        );
        fiber.settle().await;
        fiber
    };
    assert_eq!(provider.state(), FiberState::Active);
    let provider_uid = provider.uid();
    let handle = PROVIDER_HANDLE.load(Ordering::SeqCst);
    assert_ne!(handle, 0, "provider 必须拿到 ABI 句柄");

    let root = app.root.clone();
    let call =
        tokio::spawn(async move { root.call_iface("inflight@1", 1, "run", json!([])).await });
    tokio::time::timeout(Duration::from_secs(2), async {
        while pending_len(handle) != 1 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("调用必须先挂起");

    provider.dispose().await;
    let result = tokio::time::timeout(Duration::from_secs(2), call)
        .await
        .expect("卸载必须让在飞调用结算")
        .expect("调用任务不得 panic");
    let error = result.expect_err("卸载后调用必须以错误结算");
    assert!(
        error.to_string().contains("插件已卸载"),
        "卸载必须以生命周期错误交付：{error}"
    );

    {
        let records = records.lock().unwrap();
        assert_eq!(records.len(), 1, "一次结算只写一条记录：{records:?}");
        let record = &records[0];
        assert_eq!(&*record.name, "inflight@1.run");
        assert_ne!(record.call_id, 0, "入站派发 id 必须可复用");
        assert_eq!(record.caller_uid, UID_ROOT, "根 ctx 发起");
        assert_eq!(
            record.callee_uid, provider_uid,
            "callee_uid 必须是发起时快照；结算时现场读会得到 UID_DISPOSED"
        );
        assert_ne!(record.callee_uid, u64::MAX);
        assert_eq!(&*record.callee_plugin, "trace-unload");
        assert_eq!(
            record.outcome,
            cordis_core::Outcome::Err("host_lifecycle".into())
        );
    }
}
