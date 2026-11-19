//! O3：宿主 poll 边界 panic 的**业务级**结局——iface 同步首 poll 那一条。
//!
//! 独立测试二进制，只走公开入口：`FnPlugin` + `Ctx::provide_iface` 的
//! `PanickingService`。夹具铁律：假 `CallFn` 内禁止 assert / panic（跨
//! `extern "C"` 的 panic 会 abort 整个进程）。
//!
//! 本文件必须保持**单测试**：结尾断言进程级绝对值 `loaded_instances() == 0`，
//! 第二个测试会让别的实例同时活着，libtest 并行时二者互相踩。

use cordis_abi::{
    Blob, Out, OP_APPLY, OP_CALLBACK, OP_DISPOSE, STATUS_ERR, STATUS_OK, STATUS_UNSUPPORTED,
};
use cordis_core::{
    App, BoxFuture, Config, Ctx, ExternService, Inject, Outcome, Plugin, Result, TraceRecord,
    TraceSink,
};
use cordis_host::{apply_via_with_error_format, dispatches_len, host_api};
use serde_json::Value;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

struct Collector(Arc<Mutex<Vec<TraceRecord>>>);

impl TraceSink for Collector {
    fn record(&self, record: TraceRecord) {
        self.0.lock().unwrap().push(record);
    }
}

/// 在实例里真调一次宿主函数表（与插件侧 SDK 走同一条路）。
unsafe fn call_iface(handle: u64, service: &str, method: &str, continuation: u64) -> (i32, String) {
    let Some(api) = host_api().ctx_call_iface else {
        return (-1, "宿主不支持 ctx_call_iface".into());
    };
    let mut buffer = vec![0u8; 4096];
    let mut out = Out {
        ptr: buffer.as_mut_ptr(),
        cap: buffer.len(),
        len: 0,
    };
    let status = api(
        handle,
        Blob::new(service.as_bytes()),
        1,
        Blob::new(method.as_bytes()),
        Blob::new(b"[]"),
        continuation,
        &mut out,
    );
    (
        status,
        String::from_utf8_lossy(&buffer[..out.len.min(buffer.len())]).into_owned(),
    )
}

static IFACE_APPLY: Mutex<Option<(i32, String)>> = Mutex::new(None);
static IFACE_CALLBACKS: Mutex<Vec<Value>> = Mutex::new(Vec::new());
static IFACE_DISPOSES: AtomicU64 = AtomicU64::new(0);
static IFACE_CONSUMER: AtomicU64 = AtomicU64::new(0);

/// 一 poll 就 panic 的接口提供者（注入点只有 Rust）。
struct PanickingService;

impl ExternService for PanickingService {
    fn service(&self) -> &str {
        "panic@1"
    }

    fn version(&self) -> u32 {
        1
    }

    fn methods(&self) -> Vec<String> {
        vec!["go".to_string()]
    }

    fn call(
        &self,
        _caller_meta: Option<Config>,
        _method: String,
        _args: Config,
    ) -> BoxFuture<Result<Config>> {
        Box::pin(async move { panic!("iface 提供者在 poll 时 panic（集成测试注入）") })
    }
}

fn iface_provider() -> Arc<dyn Plugin> {
    cordis_core::plugin_fn(
        "panic-provider",
        Inject::default(),
        |ctx: Ctx, _config: Config| {
            Box::pin(async move {
                ctx.provide_iface("panic@1", Arc::new(PanickingService))?;
                Ok(())
            })
        },
    )
}

fn iface_consumer() -> Arc<dyn Plugin> {
    Arc::new(cordis_core::FnPlugin {
        name: "panic-consumer",
        key: cordis_core::unique_key("panic-consumer"),
        inject: Inject::of(&["panic@1"]),
        validator: None,
        f: Arc::new(|ctx: Ctx, config: Config| {
            apply_via_with_error_format(
                iface_consumer_call,
                ctx,
                config,
                "panic-consumer",
                cordis_abi::ERROR_FORMAT_V1,
            )
        }),
    })
}

unsafe extern "C" fn iface_consumer_call(
    handle: u64,
    op: u32,
    args: Blob,
    _: Blob,
    out: *mut Out,
) -> i32 {
    match op {
        OP_APPLY => {
            IFACE_CONSUMER.store(handle, Ordering::SeqCst);
            let outcome = call_iface(handle, "panic@1", "go", 7);
            if let Ok(mut slot) = IFACE_APPLY.lock() {
                *slot = Some(outcome);
            }
            // 装载本身成功：这样关闭时插件还会收到一次 OP_DISPOSE（可观测）。
            (*out).write(b"");
            STATUS_OK
        }
        OP_CALLBACK => {
            let payload: Value = serde_json::from_slice(args.as_bytes()).unwrap_or(Value::Null);
            if let Ok(mut callbacks) = IFACE_CALLBACKS.lock() {
                callbacks.push(payload);
            }
            (*out).write(b"")
        }
        OP_DISPOSE => {
            IFACE_DISPOSES.fetch_add(1, Ordering::SeqCst);
            (*out).write(b"")
        }
        _ => STATUS_UNSUPPORTED,
    }
}

#[tokio::test]
async fn o3_iface_panic_is_a_structured_business_error() {
    IFACE_APPLY.lock().unwrap().take();
    IFACE_CALLBACKS.lock().unwrap().clear();
    IFACE_DISPOSES.store(0, Ordering::SeqCst);
    IFACE_CONSUMER.store(0, Ordering::SeqCst);

    let app = App::new();
    let records = Arc::new(Mutex::new(Vec::new()));
    assert!(
        app.set_trace_sink(Arc::new(Collector(records.clone()))),
        "trace sink 只允许安装一次"
    );
    let provider = app.root.plugin(iface_provider(), Config::Null);
    provider.settle().await;
    let consumer = app.root.plugin(iface_consumer(), Config::Null);
    consumer.settle().await;

    let handle = IFACE_CONSUMER.load(Ordering::SeqCst);
    assert_ne!(handle, 0, "consumer 必须拿到 ABI 句柄");

    let (status, text) = IFACE_APPLY
        .lock()
        .unwrap()
        .clone()
        .expect("apply 期必须真调过一次 ctx_call_iface");
    assert_eq!(
        status, STATUS_ERR,
        "提供者 panic 必须变成一次可读失败：{text}"
    );
    let payload: Value =
        serde_json::from_str(&text).expect("consumer 声明了 error_format=1，错误应当结构化");
    assert_eq!(payload["code"], "host_panic", "稳定 code：{payload}");
    assert_eq!(payload["category"], "unknown", "分类保持默认：{payload}");
    assert_eq!(
        payload["certainty"], "unknown",
        "宿主不能声称提供者没执行：{payload}"
    );
    assert_eq!(
        payload["retry"], "unknown",
        "宿主不能声称重试安全：{payload}"
    );
    assert_eq!(
        dispatches_len(handle),
        0,
        "同步拿到错误之后不得残留派发记录"
    );

    // ADR 0009 §2.2 第 4 点：panic 走的是“入站没有结算”的补写路径，
    // 记录由宿主写，插件无法自报。
    {
        let records = records.lock().unwrap();
        assert_eq!(
            records.len(),
            1,
            "callee panic 必须恰好一条记录：{records:?}"
        );
        let record = &records[0];
        assert_eq!(&*record.name, "panic@1.go");
        assert_eq!(record.call_id, 0, "panic 没有可复用的入站派发 id");
        assert_eq!(record.callee_uid, 0);
        assert_eq!(&*record.callee_plugin, "panic@1");
        assert_eq!(record.outcome, Outcome::Err("host_panic".into()));
        assert_eq!(record.caller_uid, consumer.uid());
    }

    consumer.dispose().await;
    assert!(
        IFACE_CALLBACKS.lock().unwrap().is_empty(),
        "已经同步失败的一次调用，关闭时不得再补一条回调：{:?}",
        IFACE_CALLBACKS.lock().unwrap()
    );
    assert_eq!(IFACE_DISPOSES.load(Ordering::SeqCst), 1, "只卸载一次");

    provider.dispose().await;
    assert_eq!(cordis_host::loaded_instances(), 0, "不得残留实例句柄");
}
