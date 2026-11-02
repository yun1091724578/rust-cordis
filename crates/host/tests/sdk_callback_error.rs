//! `apply_via_with_error_format` 的真实 SDK 回调错误：宿主必须解码成可读消息。
//!
//! 独立测试二进制：`sdk::build_plugin` 的 apply 槽是进程级的。

use cordis_abi::sdk;
use cordis_core::{App, Config, Ctx, FiberState, Inject, Plugin};
use cordis_host::{apply_via_with_error_format, host_api, CallFn};
use std::sync::{Arc, Mutex};

#[tokio::test]
async fn structured_callback_error_is_readable_through_apply_via() {
    unsafe { sdk::attach(host_api()) };
    let desc = sdk::build_plugin("sdk-callback-error", &[], |ctx: &sdk::Ctx, _config| {
        ctx.on("boom", |_args| Err("callback boom".to_string()))?;
        Ok(())
    });
    let call: CallFn = unsafe { (*desc).call };
    let error_format = unsafe { cordis_abi::PluginDesc::error_format_at(desc) };

    let app = App::new();
    let logs = Arc::new(Mutex::new(Vec::new()));
    {
        let logs = logs.clone();
        app.store
            .set_log_sink(Arc::new(move |_level, _name, message| {
                logs.lock().unwrap().push(message.to_string());
            }));
    }

    let plugin: Arc<dyn Plugin> = Arc::new(cordis_core::FnPlugin {
        name: "sdk-callback-error",
        key: cordis_core::unique_key("sdk-callback-error"),
        inject: Inject::default(),
        validator: None,
        f: Arc::new(move |ctx: Ctx, config: Config| {
            apply_via_with_error_format(call, ctx, config, "sdk-callback-error", error_format)
        }),
    });
    let fiber = app.root.plugin(plugin, Config::Null);
    fiber.settle().await;
    assert_eq!(fiber.state(), FiberState::Active);

    app.root.emit("boom", Vec::new()).await;

    let text = logs.lock().unwrap().join("\n");
    assert!(
        text.contains("事件监听器失败: callback boom"),
        "回调错误必须保留可读消息：{text}"
    );
    assert!(
        !text.contains("\"code\""),
        "回调错误不能被结构化信封污染：{text}"
    );

    fiber.dispose().await;
}
