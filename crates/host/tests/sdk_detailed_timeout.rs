//! SDK 详细异步错误通道：`call_iface_detailed` 的 `on_done` 必须拿到
//! `ErrorPayload`，而不是只有字符串。
//!
//! 独立测试二进制：`DispatchPolicy` 与 SDK apply 槽都是进程级设置。

use cordis_abi::sdk;
use cordis_abi::ErrorPayload;
use cordis_core::{
    App, BoxFuture, Config, Ctx, ExternService, Inject, Plugin, Result as CoreResult,
};
use cordis_host::{
    apply_via_with_error_format, dispatches_len, host_api, pending_len, set_dispatch_policy,
    CallFn, DispatchPolicy,
};
use serde_json::json;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct NeverService;

impl ExternService for NeverService {
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
        Box::pin(std::future::pending::<CoreResult<Config>>())
    }
}

fn provider() -> Arc<dyn Plugin> {
    cordis_core::plugin_fn(
        "detailed-provider",
        Inject::default(),
        |ctx: Ctx, _config: Config| {
            Box::pin(async move {
                ctx.provide_iface("slow@1", Arc::new(NeverService))?;
                Ok(())
            })
        },
    )
}

#[tokio::test]
async fn call_iface_detailed_receives_timeout_payload_from_the_real_host() {
    set_dispatch_policy(DispatchPolicy {
        outbound_timeout_ms: Some(50),
        ..DispatchPolicy::default()
    });

    unsafe { sdk::attach(host_api()) };
    let seen = Arc::new(Mutex::new(None::<ErrorPayload>));
    let calls = Arc::new(AtomicU64::new(0));
    let seen_apply = seen.clone();
    let calls_apply = calls.clone();
    let plugin_handle = Arc::new(AtomicU64::new(0));
    let handle_apply = plugin_handle.clone();
    let desc = sdk::build_plugin(
        "sdk-detailed-timeout",
        &[],
        move |ctx: &sdk::Ctx, _config: sdk::Json| {
            handle_apply.store(ctx.handle(), Ordering::SeqCst);
            let seen = seen_apply.clone();
            let calls = calls_apply.clone();
            ctx.call_iface_detailed("slow@1", 1, "hang", json!([]), move |result| {
                calls.fetch_add(1, Ordering::SeqCst);
                *seen.lock().unwrap() = Some(result.expect_err("超时必须交付错误载荷"));
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
        name: "sdk-detailed-timeout",
        key: cordis_core::unique_key("sdk-detailed-timeout"),
        inject: Inject::of(&["slow@1"]),
        validator: None,
        f: Arc::new(move |ctx: Ctx, config: Config| {
            apply_via_with_error_format(call, ctx, config, "sdk-detailed-timeout", error_format)
        }),
    });
    let consumer_fiber = app.root.plugin(consumer, Config::Null);
    consumer_fiber.settle().await;

    let started = std::time::Instant::now();
    loop {
        if seen.lock().unwrap().is_some() {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "SDK 详细续延必须在看门狗时限内收到错误"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let payload = seen.lock().unwrap().clone().expect("错误载荷必须被记录");
    assert!(payload.error.contains("HOST_TIMEOUT"), "{payload:?}");
    assert_eq!(payload.code, "HOST_TIMEOUT");
    assert_eq!(payload.category, "timeout");
    assert_eq!(payload.certainty, "unknown");
    assert_eq!(calls.load(Ordering::SeqCst), 1, "详细续延必须恰好执行一次");

    let handle = plugin_handle.load(Ordering::SeqCst);
    assert_ne!(handle, 0, "插件必须拿到 ABI 句柄");
    assert_eq!(dispatches_len(handle), 0);
    assert_eq!(pending_len(handle), 0);

    consumer_fiber.dispose().await;
    provider_fiber.dispose().await;
    set_dispatch_policy(DispatchPolicy::default());
}
