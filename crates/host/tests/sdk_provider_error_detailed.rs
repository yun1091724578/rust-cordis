//! provider 同步业务错误经 `call_iface_detailed` 必须保留 code/category，
//! 不能退化成只有字符串。
//!
//! 独立测试二进制：SDK apply 槽是进程级设置。

use cordis_abi::sdk;
use cordis_abi::ErrorPayload;
use cordis_core::{App, Config, Plugin};
use cordis_host::{apply_via_with_error_format, host_api, CallFn};
use serde_json::json;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[tokio::test]
async fn provider_business_error_reaches_detailed_callback_with_code() {
    unsafe { sdk::attach(host_api()) };

    let seen = Arc::new(Mutex::new(None::<ErrorPayload>));
    let seen_apply = seen.clone();
    let handle = Arc::new(AtomicU64::new(0));
    let handle_apply = handle.clone();

    let desc = sdk::build_plugin(
        "sdk-provider-error",
        &[],
        move |ctx: &sdk::Ctx, _config: sdk::Json| {
            handle_apply.store(ctx.handle(), Ordering::SeqCst);
            let method: sdk::IfaceFn = Arc::new(|_args, _meta| Err("provider blew up".to_string()));
            ctx.provide_iface("boom@1", 1, vec![("go", method)])?;
            let seen = seen_apply.clone();
            ctx.call_iface_detailed("boom@1", 1, "go", json!([]), move |result| {
                *seen.lock().unwrap() = Some(result.expect_err("provider 必须交付错误载荷"));
            })?;
            Ok(())
        },
    );
    let call: CallFn = unsafe { (*desc).call };
    let error_format = unsafe { cordis_abi::PluginDesc::error_format_at(desc) };

    let app = App::new();
    let plugin: Arc<dyn Plugin> = Arc::new(cordis_core::FnPlugin {
        name: "sdk-provider-error",
        key: cordis_core::unique_key("sdk-provider-error"),
        inject: cordis_core::Inject::default(),
        validator: None,
        f: Arc::new(move |ctx: cordis_core::Ctx, config: Config| {
            apply_via_with_error_format(call, ctx, config, "sdk-provider-error", error_format)
        }),
    });
    let fiber = app.root.plugin(plugin, Config::Null);
    fiber.settle().await;

    let started = std::time::Instant::now();
    loop {
        if seen.lock().unwrap().is_some() {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "详细续延必须收到 provider 业务错误"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let payload = seen.lock().unwrap().clone().expect("错误载荷必须被记录");
    assert_eq!(payload.error, "provider blew up");
    assert_eq!(payload.code, "callback_failed", "{payload:?}");
    assert_eq!(payload.category, "business", "{payload:?}");

    assert_ne!(handle.load(Ordering::SeqCst), 0);
    fiber.dispose().await;
}
