//! 后台 poll 边界 panic：调用先挂起，之后某次 poll unwind。
//!
//! 覆盖 `complete_or_defer` 的 `on_panic` 分支：它必须写一条 `host_panic` 记录
//! 并把同一条错误交付给调用方（不能只写日志、也不能写第二条）。

mod trace_support;

use cordis_abi::{Blob, Out, OP_APPLY, OP_CALLBACK, OP_DISPOSE, STATUS_OK, STATUS_UNSUPPORTED};
use cordis_core::{App, BoxFuture, Config, Ctx, ExternService, Inject, Outcome, Plugin, Result};
use serde_json::Value;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use trace_support::{abi_plugin, call_iface, collect};

static CALLBACKS: Mutex<u64> = Mutex::new(0);
static CALLBACK_ERROR: Mutex<Option<Value>> = Mutex::new(None);

fn callback_notify() -> &'static tokio::sync::Notify {
    static NOTIFY: OnceLock<tokio::sync::Notify> = OnceLock::new();
    NOTIFY.get_or_init(tokio::sync::Notify::new)
}

struct AsyncPanicService;

impl ExternService for AsyncPanicService {
    fn service(&self) -> &str {
        "apanic@1"
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
        Box::pin(async {
            tokio::task::yield_now().await;
            panic!("后台 poll 边界 panic（集成测试注入）")
        })
    }
}

fn panic_provider() -> std::sync::Arc<dyn Plugin> {
    cordis_core::plugin_fn(
        "trace-apanic",
        Inject::default(),
        |ctx: Ctx, _config: Config| {
            Box::pin(async move {
                ctx.provide_iface("apanic@1", std::sync::Arc::new(AsyncPanicService))?;
                Ok(())
            })
        },
    )
}

unsafe extern "C" fn consumer_call(handle: u64, op: u32, a: Blob, _b: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => {
            let (status, text) = call_iface(handle, "apanic@1", "run", 7);
            if status != STATUS_OK {
                (*out).write(text.as_bytes());
                return status;
            }
            let reply: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            if reply.get("pending").and_then(Value::as_bool) != Some(true) {
                return STATUS_UNSUPPORTED;
            }
            (*out).write(b"");
            STATUS_OK
        }
        OP_CALLBACK => {
            let payload: Value = serde_json::from_slice(a.as_bytes()).unwrap_or(Value::Null);
            if payload.get("id").and_then(Value::as_u64) == Some(7) {
                *CALLBACKS.lock().unwrap() += 1;
                *CALLBACK_ERROR.lock().unwrap() =
                    Some(payload["args"].get(0).cloned().unwrap_or(Value::Null));
                callback_notify().notify_one();
            }
            (*out).write(b"")
        }
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

#[tokio::test]
async fn background_panic_writes_exactly_one_record_and_one_callback() {
    *CALLBACKS.lock().unwrap() = 0;
    *CALLBACK_ERROR.lock().unwrap() = None;

    let app = App::new();
    let records = collect(&app);

    let provider = {
        let fiber = app.root.plugin(panic_provider(), Config::Null);
        fiber.settle().await;
        fiber
    };
    let consumer = {
        let fiber = app.root.plugin(
            abi_plugin(
                "trace-apanic-consumer",
                Inject::of(&["apanic@1"]),
                consumer_call,
            ),
            Config::Null,
        );
        fiber.settle().await;
        fiber
    };

    tokio::time::timeout(Duration::from_secs(2), async {
        while CALLBACK_ERROR.lock().unwrap().is_none() {
            callback_notify().notified().await;
        }
    })
    .await
    .expect("后台 panic 必须在时限内交付");

    let error = CALLBACK_ERROR
        .lock()
        .unwrap()
        .clone()
        .expect("已等待到回调");
    assert_eq!(error["code"], "host_panic", "panic 码必须稳定：{error}");
    assert_eq!(*CALLBACKS.lock().unwrap(), 1, "只能交付一次回调");

    {
        let records = records.lock().unwrap();
        assert_eq!(records.len(), 1, "后台 panic 只写一条记录：{records:?}");
        let record = &records[0];
        assert_eq!(&*record.name, "apanic@1.run");
        assert_eq!(record.call_id, 0, "入站没有结算，不能复用派发 id");
        assert_eq!(record.callee_uid, 0);
        assert_eq!(&*record.callee_plugin, "apanic@1");
        assert_eq!(record.outcome, Outcome::Err("host_panic".into()));
        assert_eq!(record.caller_uid, consumer.uid());
    }

    consumer.dispose().await;
    provider.dispose().await;
}
