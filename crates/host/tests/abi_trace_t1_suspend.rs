//! T1 服务方法挂起后恢复：恢复点不在任何 `CALL_SCOPE` 窗口里，父为 0。
//!
//! ADR 0010 §2.6 要求“T1 挂起后在自己的任务里再发起 → 父为 0（提供者侧断链）”
//! 单独断言；本用例用 T1 服务方法 `await sleep` 后调 ABI 叶子服务来钉住它。

mod trace_support;

use cordis_abi::{Blob, Out, OP_APPLY, OP_CALLBACK, OP_DISPOSE, STATUS_UNSUPPORTED};
use cordis_core::{App, BoxFuture, Config, Ctx, ExternService, Inject, Outcome, Plugin, Result};
use serde_json::json;
use std::sync::Mutex;
use std::time::Duration;
use trace_support::{abi_plugin, collect, provide};

static CONSUMER_OK: Mutex<Option<bool>> = Mutex::new(None);

unsafe extern "C" fn leaf_call(handle: u64, op: u32, _a: Blob, _b: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => provide(handle, "suspend-leaf@1", r#"["run"]"#, 1, out),
        OP_CALLBACK => (*out).write(br#"{"ret":"leaf"}"#),
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

struct SuspendService {
    ctx: Ctx,
}

impl ExternService for SuspendService {
    fn service(&self) -> &str {
        "suspend@1"
    }

    fn version(&self) -> u32 {
        1
    }

    fn methods(&self) -> Vec<String> {
        vec!["run".into()]
    }

    fn call(
        &self,
        _caller_meta: Option<Config>,
        _method: String,
        _args: Config,
    ) -> BoxFuture<Result<Config>> {
        let ctx = self.ctx.clone();
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            ctx.call_iface("suspend-leaf@1", 1, "run", json!([])).await
        })
    }
}

fn suspend_provider() -> std::sync::Arc<dyn Plugin> {
    cordis_core::plugin_fn(
        "trace-suspend",
        Inject::of(&["suspend-leaf@1"]),
        |ctx: Ctx, _config: Config| {
            Box::pin(async move {
                ctx.provide_iface(
                    "suspend@1",
                    std::sync::Arc::new(SuspendService { ctx: ctx.clone() }),
                )?;
                Ok(())
            })
        },
    )
}

fn t1_consumer() -> std::sync::Arc<dyn Plugin> {
    cordis_core::plugin_fn(
        "trace-suspend-consumer",
        Inject::of(&["suspend@1"]),
        |ctx: Ctx, _config: Config| {
            Box::pin(async move {
                let result = ctx.call_iface("suspend@1", 1, "run", json!([])).await;
                *CONSUMER_OK.lock().unwrap() = Some(result.is_ok());
                Ok(())
            })
        },
    )
}

#[tokio::test]
async fn t1_resume_is_a_root() {
    *CONSUMER_OK.lock().unwrap() = None;

    let app = App::new();
    let records = collect(&app);

    let leaf = {
        let fiber = app.root.plugin(
            abi_plugin("trace-suspend-leaf", Inject::default(), leaf_call),
            Config::Null,
        );
        fiber.settle().await;
        fiber
    };
    let provider = {
        let fiber = app.root.plugin(suspend_provider(), Config::Null);
        fiber.settle().await;
        fiber
    };
    let consumer = {
        let fiber = app.root.plugin(t1_consumer(), Config::Null);
        fiber.settle().await;
        fiber
    };

    assert_eq!(*CONSUMER_OK.lock().unwrap(), Some(true), "挂起应正常恢复");

    {
        let records = records.lock().unwrap();
        assert_eq!(records.len(), 1, "只有恢复后的叶子一跳：{records:?}");
        let record = &records[0];
        assert_eq!(&*record.name, "suspend-leaf@1.run");
        assert_eq!(
            record.parent_call_id, 0,
            "T1 恢复点不在任何 CALL_SCOPE 里，父必须是 0"
        );
        assert_ne!(record.call_id, 0);
        assert_eq!(record.callee_uid, leaf.uid());
        assert_eq!(&*record.callee_plugin, "trace-suspend-leaf");
        assert_eq!(record.outcome, Outcome::Ok);
    }

    consumer.dispose().await;
    provider.dispose().await;
    leaf.dispose().await;
}
