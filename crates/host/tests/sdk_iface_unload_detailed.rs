//! 插件卸载时，SDK 详细 iface 续延必须收到生命周期错误，而不是
//! `plugin_error/business/reported`。
//!
//! 独立测试二进制：SDK apply 槽与 `DispatchPolicy` 都是进程级设置。

use cordis_abi::sdk;
use cordis_abi::ErrorPayload;
use cordis_core::{
    App, BoxFuture, Config, Ctx, ExternService, Inject, Plugin, Result as CoreResult,
};
use cordis_host::{apply_via_with_error_format, host_api, pending_len, CallFn};
use serde_json::json;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

static STARTED: AtomicBool = AtomicBool::new(false);

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

fn provider() -> Arc<dyn Plugin> {
    cordis_core::plugin_fn(
        "detailed-unload-provider",
        Inject::default(),
        |ctx: Ctx, _config: Config| {
            Box::pin(async move {
                ctx.provide_iface("slow@1", Arc::new(HangService))?;
                Ok(())
            })
        },
    )
}

#[tokio::test]
async fn sdk_detailed_iface_receives_lifecycle_error_on_provider_unload() {
    STARTED.store(false, Ordering::SeqCst);
    unsafe { sdk::attach(host_api()) };

    let seen = Arc::new(Mutex::new(None::<ErrorPayload>));
    let calls = Arc::new(AtomicU64::new(0));
    let seen_apply = seen.clone();
    let calls_apply = calls.clone();
    let plugin_handle = Arc::new(AtomicU64::new(0));
    let handle_apply = plugin_handle.clone();

    let desc = sdk::build_plugin(
        "sdk-iface-unload",
        &[],
        move |ctx: &sdk::Ctx, _config: sdk::Json| {
            handle_apply.store(ctx.handle(), Ordering::SeqCst);
            let seen = seen_apply.clone();
            let calls = calls_apply.clone();
            ctx.call_iface_detailed("slow@1", 1, "hang", json!([]), move |result| {
                calls.fetch_add(1, Ordering::SeqCst);
                *seen.lock().unwrap() = Some(result.expect_err("卸载必须交付错误载荷"));
            })?;
            Ok(())
        },
    );
    let call: CallFn = unsafe { (*desc).call };
    let error_format = unsafe { cordis_abi::PluginDesc::error_format_at(desc) };

    let app = App::new();
    let provider_fiber = app.root.plugin(provider(), Config::Null);
    provider_fiber.settle().await;
    let consumer: Arc<dyn Plugin> = Arc::new(cordis_core::FnPlugin {
        name: "sdk-iface-unload",
        key: cordis_core::unique_key("sdk-iface-unload"),
        inject: Inject::of(&["slow@1"]),
        validator: None,
        f: Arc::new(move |ctx: Ctx, config: Config| {
            apply_via_with_error_format(call, ctx, config, "sdk-iface-unload", error_format)
        }),
    });
    let consumer_fiber = app.root.plugin(consumer, Config::Null);
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
        if seen.lock().unwrap().is_some() {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "卸载必须让详细续延结算"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let payload = seen.lock().unwrap().clone().expect("错误载荷必须被记录");
    assert!(payload.error.contains("卸载"), "{payload:?}");
    assert_eq!(payload.code, "host_lifecycle", "{payload:?}");
    assert_eq!(payload.category, "lifecycle", "{payload:?}");
    assert_eq!(payload.certainty, "unknown", "{payload:?}");
    assert_eq!(calls.load(Ordering::SeqCst), 1, "详细续延必须恰好结算一次");

    let handle = plugin_handle.load(Ordering::SeqCst);
    assert_ne!(handle, 0, "插件必须拿到 ABI 句柄");
    assert_eq!(pending_len(handle), 0);
    consumer_fiber.dispose().await;
}
