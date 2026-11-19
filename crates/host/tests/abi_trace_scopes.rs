//! 根与跳的边界：事件回调、T1 同步段、T1 异步任务、T1 apply。
//!
//! 断言 ADR 0010 的压栈规则：只有有记录的 iface 入站与续延交付压
//! `CALL_SCOPE`；事件、T1 的异步起点都是根。

mod trace_support;

use cordis_abi::{Blob, Out, OP_APPLY, OP_CALLBACK, OP_DISPOSE, STATUS_OK, STATUS_UNSUPPORTED};
use cordis_core::{App, BoxFuture, Config, Ctx, ExternService, Inject, Outcome, Plugin, Result};
use serde_json::{json, Value};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use trace_support::{abi_plugin, call_iface, collect, provide};

fn async_done() -> &'static tokio::sync::Notify {
    static NOTIFY: OnceLock<tokio::sync::Notify> = OnceLock::new();
    NOTIFY.get_or_init(tokio::sync::Notify::new)
}

static WRAP_RESULT: Mutex<Option<Value>> = Mutex::new(None);

unsafe fn forward(handle: u64, target: &str, out: *mut Out) -> i32 {
    let (status, text) = call_iface(handle, target, "run", 0);
    if status != STATUS_OK {
        (*out).write(text.as_bytes());
        return status;
    }
    let reply: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if reply.get("pending").and_then(Value::as_bool) != Some(false) {
        return STATUS_UNSUPPORTED;
    }
    let ret = json!({ "ret": reply.get("result").cloned().unwrap_or(Value::Null) }).to_string();
    (*out).write(ret.as_bytes());
    STATUS_OK
}

// ---------------------------------------------------------------- 事件根

unsafe extern "C" fn evt_leaf_call(handle: u64, op: u32, _a: Blob, _b: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => provide(handle, "evt-leaf@1", r#"["run"]"#, 2, out),
        OP_CALLBACK => (*out).write(br#"{"ret":"leaf"}"#),
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

unsafe extern "C" fn evt_call(handle: u64, op: u32, _a: Blob, _b: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => provide(handle, "evt@1", r#"["run"]"#, 1, out),
        OP_CALLBACK => forward(handle, "evt-leaf@1", out),
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

#[tokio::test]
async fn event_listener_starts_a_root_chain() {
    let app = App::new();
    let records = collect(&app);
    let leaf = {
        let fiber = app.root.plugin(
            abi_plugin("trace-evt-leaf", Inject::default(), evt_leaf_call),
            Config::Null,
        );
        fiber.settle().await;
        fiber
    };
    let provider = {
        let fiber = app.root.plugin(
            abi_plugin("trace-evt", Inject::of(&["evt-leaf@1"]), evt_call),
            Config::Null,
        );
        fiber.settle().await;
        fiber
    };

    let root = app.root.clone();
    let _listener = app.root.on("trace-evt", move |_call| {
        let ctx = root.clone();
        Box::pin(async move {
            let _ = ctx.call_iface("evt@1", 1, "run", json!([])).await;
            None
        })
    });
    app.root.emit("trace-evt", Vec::new()).await;

    {
        let records = records.lock().unwrap();
        assert_eq!(
            records.len(),
            2,
            "事件不产记录，只记它发起的 iface 跳：{records:?}"
        );
        let evt = records
            .iter()
            .find(|record| &*record.name == "evt@1.run")
            .expect("evt@1 必须有记录");
        let leaf_record = records
            .iter()
            .find(|record| &*record.name == "evt-leaf@1.run")
            .expect("evt-leaf@1 必须有记录");
        assert_eq!(evt.parent_call_id, 0, "事件回调是根");
        assert_eq!(leaf_record.parent_call_id, evt.call_id);
        assert_eq!(evt.outcome, Outcome::Ok);
        assert_eq!(leaf_record.outcome, Outcome::Ok);
    }

    provider.dispose().await;
    leaf.dispose().await;
}

// ---------------------------------------------------------------- T1 跳过 / T1 异步起点

struct NativeForward {
    ctx: Ctx,
}

impl ExternService for NativeForward {
    fn service(&self) -> &str {
        "native@1"
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
        Box::pin(async move { ctx.call_iface("leaf@1", 1, "run", json!([])).await })
    }
}

fn native_plugin() -> std::sync::Arc<dyn Plugin> {
    cordis_core::plugin_fn(
        "trace-native",
        Inject::of(&["leaf@1", "leaf2@1"]),
        |ctx: Ctx, _config: Config| {
            Box::pin(async move {
                ctx.provide_iface(
                    "native@1",
                    std::sync::Arc::new(NativeForward { ctx: ctx.clone() }),
                )?;
                let task_ctx = ctx.clone();
                tokio::spawn(async move {
                    let _ = task_ctx.call_iface("leaf2@1", 1, "run", json!([])).await;
                    async_done().notify_one();
                });
                Ok(())
            })
        },
    )
}

unsafe extern "C" fn t1_leaf_call(handle: u64, op: u32, _a: Blob, _b: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => {
            let status = provide(handle, "leaf@1", r#"["run"]"#, 2, out);
            if status != STATUS_OK {
                return status;
            }
            provide(handle, "leaf2@1", r#"["run"]"#, 3, out)
        }
        OP_CALLBACK => (*out).write(br#"{"ret":"leaf"}"#),
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

unsafe extern "C" fn wrap_call(handle: u64, op: u32, _a: Blob, _b: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => provide(handle, "wrap@1", r#"["run"]"#, 1, out),
        OP_CALLBACK => forward(handle, "native@1", out),
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

unsafe extern "C" fn wrap_consumer_call(
    handle: u64,
    op: u32,
    _a: Blob,
    _b: Blob,
    out: *mut Out,
) -> i32 {
    match op {
        OP_APPLY => {
            let (status, text) = call_iface(handle, "wrap@1", "run", 0);
            if status == STATUS_OK {
                *WRAP_RESULT.lock().unwrap() =
                    Some(serde_json::from_str(&text).unwrap_or(Value::Null));
            }
            (*out).write(b"");
            STATUS_OK
        }
        OP_CALLBACK => (*out).write(b""),
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

#[tokio::test]
async fn t1_sync_segment_is_skipped_but_t1_async_task_is_a_root() {
    *WRAP_RESULT.lock().unwrap() = None;

    let app = App::new();
    let records = collect(&app);

    let leaf = {
        let fiber = app.root.plugin(
            abi_plugin("trace-t1-leaf", Inject::default(), t1_leaf_call),
            Config::Null,
        );
        fiber.settle().await;
        fiber
    };
    let native = {
        let fiber = app.root.plugin(native_plugin(), Config::Null);
        fiber.settle().await;
        fiber
    };
    let wrap = {
        let fiber = app.root.plugin(
            abi_plugin("trace-wrap", Inject::of(&["native@1"]), wrap_call),
            Config::Null,
        );
        fiber.settle().await;
        fiber
    };
    let consumer = {
        let fiber = app.root.plugin(
            abi_plugin(
                "trace-wrap-consumer",
                Inject::of(&["wrap@1"]),
                wrap_consumer_call,
            ),
            Config::Null,
        );
        fiber.settle().await;
        fiber
    };

    {
        let result = WRAP_RESULT.lock().unwrap();
        let result = result.as_ref().expect("consumer 必须拿到同步结果");
        assert_eq!(result["pending"], false, "同步链必须内联完成：{result}");
        assert_eq!(result["result"], "leaf", "结果必须穿过 T1 一跳：{result}");
    }

    tokio::time::timeout(Duration::from_secs(2), async {
        async_done().notified().await;
    })
    .await
    .expect("T1 异步任务必须在时限内完成");

    {
        let records = records.lock().unwrap();
        assert_eq!(records.len(), 3, "wrap、leaf、leaf2 各一条：{records:?}");
        let wrap_record = records
            .iter()
            .find(|record| &*record.name == "wrap@1.run")
            .expect("wrap@1 必须有记录");
        let sync_leaf = records
            .iter()
            .find(|record| &*record.name == "leaf@1.run")
            .expect("同步段里的 leaf@1 必须有记录");
        let async_leaf = records
            .iter()
            .find(|record| &*record.name == "leaf2@1.run")
            .expect("异步任务里的 leaf2@1 必须有记录");

        assert_eq!(wrap_record.parent_call_id, 0, "consumer 的 apply 是根");
        assert_eq!(
            sync_leaf.parent_call_id, wrap_record.call_id,
            "T1 同步段被跳过，leaf 的父仍是 wrap"
        );
        assert_eq!(
            async_leaf.parent_call_id, 0,
            "T1 自己的异步任务是根，不能继承过去的跳"
        );
        for leaf_record in [sync_leaf, async_leaf] {
            assert_eq!(leaf_record.outcome, Outcome::Ok);
            assert_ne!(leaf_record.call_id, 0);
            assert_eq!(leaf_record.callee_uid, leaf.uid());
            assert_eq!(&*leaf_record.callee_plugin, "trace-t1-leaf");
        }
        assert_eq!(wrap_record.callee_uid, wrap.uid());
        assert_eq!(&*wrap_record.callee_plugin, "trace-wrap");
    }

    consumer.dispose().await;
    wrap.dispose().await;
    native.dispose().await;
    leaf.dispose().await;
}

// ---------------------------------------------------------------- T1 起点

unsafe extern "C" fn r2_call(handle: u64, op: u32, _a: Blob, _b: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => provide(handle, "r2@1", r#"["run"]"#, 2, out),
        OP_CALLBACK => (*out).write(br#"{"ret":"r2"}"#),
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

unsafe extern "C" fn r1_call(handle: u64, op: u32, _a: Blob, _b: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => provide(handle, "r1@1", r#"["run"]"#, 1, out),
        OP_CALLBACK => forward(handle, "r2@1", out),
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

fn t1_root_plugin() -> std::sync::Arc<dyn Plugin> {
    cordis_core::plugin_fn(
        "trace-t1-root",
        Inject::of(&["r1@1"]),
        |ctx: Ctx, _config: Config| {
            Box::pin(async move {
                ctx.call_iface("r1@1", 1, "run", json!([])).await?;
                Ok(())
            })
        },
    )
}

#[tokio::test]
async fn t1_apply_starts_a_root_chain() {
    let app = App::new();
    let records = collect(&app);

    let r2 = {
        let fiber = app.root.plugin(
            abi_plugin("trace-r2", Inject::default(), r2_call),
            Config::Null,
        );
        fiber.settle().await;
        fiber
    };
    let r1 = {
        let fiber = app.root.plugin(
            abi_plugin("trace-r1", Inject::of(&["r2@1"]), r1_call),
            Config::Null,
        );
        fiber.settle().await;
        fiber
    };
    let t1 = {
        let fiber = app.root.plugin(t1_root_plugin(), Config::Null);
        fiber.settle().await;
        fiber
    };

    {
        let records = records.lock().unwrap();
        assert_eq!(records.len(), 2, "r1、r2 各一条：{records:?}");
        let r1_record = records
            .iter()
            .find(|record| &*record.name == "r1@1.run")
            .expect("r1@1 必须有记录");
        let r2_record = records
            .iter()
            .find(|record| &*record.name == "r2@1.run")
            .expect("r2@1 必须有记录");
        assert_eq!(r1_record.parent_call_id, 0, "T1 apply 是根");
        assert_eq!(r2_record.parent_call_id, r1_record.call_id);
        assert_eq!(r1_record.outcome, Outcome::Ok);
        assert_eq!(r2_record.outcome, Outcome::Ok);
    }

    t1.dispose().await;
    r1.dispose().await;
    r2.dispose().await;
}
