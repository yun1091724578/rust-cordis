//! iface 在飞记录被卸载排水时，abort 必须使用 `kind:"iface"` 和生命周期原因，
//! 不能伪装成 `kind:"dispatch"` + 用户取消。

use cordis_abi::{
    Blob, Out, KIND_IFACE, OP_APPLY, OP_CALLBACK, OP_DISPOSE, STATUS_OK, STATUS_UNSUPPORTED,
};
use cordis_core::{
    App, BoxFuture, Config, Ctx, ExternService, Inject, Plugin, Result as CoreResult,
};
use cordis_host::{apply_via_with_error_format, host_api};
use serde_json::Value;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

static STARTED: AtomicBool = AtomicBool::new(false);
static CONSUMER: AtomicU64 = AtomicU64::new(0);
static CALLBACKS: Mutex<Vec<Value>> = Mutex::new(Vec::new());

struct HangService;

impl ExternService for HangService {
    fn service(&self) -> &str {
        "slow@1"
    }

    fn version(&self) -> u32 {
        1
    }

    fn methods(&self) -> Vec<String> {
        vec!["hang".into()]
    }

    fn call(
        &self,
        _caller_meta: Option<Config>,
        _method: String,
        _args: Config,
    ) -> BoxFuture<CoreResult<Config>> {
        STARTED.store(true, Ordering::SeqCst);
        Box::pin(std::future::pending::<CoreResult<Config>>())
    }
}

unsafe extern "C" fn consumer_call(handle: u64, op: u32, a: Blob, _: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => {
            CONSUMER.store(handle, Ordering::SeqCst);
            let Some(call) = host_api().ctx_call_iface else {
                return STATUS_UNSUPPORTED;
            };
            let mut buffer = [0u8; 512];
            let mut slot = Out {
                ptr: buffer.as_mut_ptr(),
                cap: buffer.len(),
                len: 0,
            };
            let status = call(
                handle,
                Blob::new(b"slow@1"),
                1,
                Blob::new(b"hang"),
                Blob::new(b"[]"),
                7,
                &mut slot,
            );
            if status != STATUS_OK {
                return status;
            }
            let text = String::from_utf8_lossy(&buffer[..slot.len]).into_owned();
            (*out).write(text.as_bytes())
        }
        OP_CALLBACK => {
            let payload: Value = serde_json::from_slice(a.as_bytes()).unwrap_or(Value::Null);
            CALLBACKS.lock().unwrap().push(payload);
            (*out).write(b"")
        }
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

fn provider() -> Arc<dyn Plugin> {
    cordis_core::plugin_fn(
        "slow-provider",
        Inject::default(),
        |ctx: Ctx, _config: Config| {
            Box::pin(async move {
                ctx.provide_iface("slow@1", Arc::new(HangService))?;
                Ok(())
            })
        },
    )
}

fn consumer() -> Arc<dyn Plugin> {
    Arc::new(cordis_core::FnPlugin {
        name: "slow-consumer",
        key: cordis_core::unique_key("slow-consumer"),
        inject: Inject::of(&["slow@1"]),
        validator: None,
        f: Arc::new(|ctx: Ctx, config: Config| {
            apply_via_with_error_format(
                consumer_call,
                ctx,
                config,
                "slow-consumer",
                cordis_abi::ERROR_FORMAT_V1,
            )
        }),
    })
}

#[tokio::test]
async fn iface_unload_abort_uses_iface_kind_and_lifecycle_cause() {
    STARTED.store(false, Ordering::SeqCst);
    CALLBACKS.lock().unwrap().clear();
    CONSUMER.store(0, Ordering::SeqCst);

    let app = App::new();
    let provider_fiber = app.root.plugin(provider(), Config::Null);
    provider_fiber.settle().await;
    let consumer_fiber = app.root.plugin(consumer(), Config::Null);
    consumer_fiber.settle().await;

    let started = std::time::Instant::now();
    while !STARTED.load(Ordering::SeqCst) {
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "provider 方法必须开始执行"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    provider_fiber.dispose().await;

    let started = std::time::Instant::now();
    loop {
        if !CALLBACKS.lock().unwrap().is_empty() {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "卸载必须向 iface 续延交付 abort"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let payload = CALLBACKS.lock().unwrap().first().cloned().unwrap();
    assert_eq!(payload["kind"], KIND_IFACE, "iface 不能收到 kind=dispatch");
    assert_eq!(payload["id"], 7);
    let error = &payload["args"][0];
    assert!(
        error["error"]
            .as_str()
            .is_some_and(|text| text.contains("卸载")),
        "卸载原因必须可读：{payload}"
    );
    assert_eq!(error["code"], "host_lifecycle", "{payload}");
    assert_eq!(error["category"], "lifecycle", "{payload}");
    assert_eq!(error["certainty"], "unknown", "{payload}");

    consumer_fiber.dispose().await;
}
