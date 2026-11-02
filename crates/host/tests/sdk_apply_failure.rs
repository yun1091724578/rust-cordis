//! 初始化失败实例的 SDK 回调回收。
//!
//! 独立测试二进制：`sdk::build_plugin` 的 apply 槽是进程级的。

use cordis_abi::sdk;
use cordis_core::{App, Config, Ctx, FiberState, Inject, Plugin};
use cordis_host::{apply_via_with_error_format, host_api, CallFn};
use serde_json::json;
use std::sync::{Arc, Mutex, Weak};

struct Probe;

static WEAK: Mutex<Option<Weak<Probe>>> = Mutex::new(None);

#[tokio::test]
async fn failed_apply_releases_sdk_callbacks_registered_before_the_error() {
    unsafe { sdk::attach(host_api()) };
    assert_eq!(
        host_api().error_format(),
        cordis_abi::ERROR_FORMAT_V1,
        "生产 HostApi 必须声明结构化错误能力"
    );
    let desc = sdk::build_plugin(
        "sdk-apply-failure",
        &[],
        move |ctx: &sdk::Ctx, _config: sdk::Json| {
            let host_error = ctx
                .require_detailed("missing")
                .expect_err("未提供的依赖必须失败");
            assert_eq!(host_error.code, "host_require");
            assert_eq!(host_error.category, "resolve");
            let probe = Arc::new(Probe);
            *WEAK.lock().unwrap() = Some(Arc::downgrade(&probe));
            ctx.on("apply/fail", move |_args| {
                let _held = probe.clone();
                Ok(json!(null))
            })?;
            Err("intentional apply failure".to_string())
        },
    );
    let call: CallFn = unsafe { (*desc).call };
    let error_format = unsafe { cordis_abi::PluginDesc::error_format_at(desc) };

    let app = App::new();
    let plugin: Arc<dyn Plugin> = Arc::new(cordis_core::FnPlugin {
        name: "sdk-apply-failure",
        key: cordis_core::unique_key("sdk-apply-failure"),
        inject: Inject::default(),
        validator: None,
        f: Arc::new(move |ctx: Ctx, config: Config| {
            apply_via_with_error_format(call, ctx, config, "sdk-apply-failure", error_format)
        }),
    });
    let fiber = app.root.plugin(plugin, Config::Null);
    fiber.settle().await;

    assert_eq!(fiber.state(), FiberState::Failed);
    let error = fiber.error().expect("失败的 apply 必须留下可读错误消息");
    assert!(
        error.contains("intentional apply failure"),
        "apply 错误必须保留人类可读消息：{error}"
    );
    assert!(
        !error.contains("\"code\""),
        "apply 错误不能被结构化信封污染：{error}"
    );
    assert_eq!(
        app.store.hook_count("apply/fail"),
        0,
        "宿主监听器必须随失败装载回收"
    );
    assert!(
        WEAK.lock()
            .unwrap()
            .as_ref()
            .and_then(Weak::upgrade)
            .is_none(),
        "失败 apply 注册的 SDK 回调必须释放它捕获的对象"
    );

    fiber.dispose().await;
    assert!(
        WEAK.lock()
            .unwrap()
            .as_ref()
            .and_then(Weak::upgrade)
            .is_none(),
        "显式处置之后仍不得残留回调"
    );
}
