//! entry-level `inject: { required, optional }` 的装配语义。

use cordis_core::{plugin_fn, App, Config, Ctx, Dyn, FiberState, Inject, Plugin};
use cordis_host::mount;
use serde_json::json;
use std::sync::{Arc, Mutex};

fn probe(seen: Arc<Mutex<Vec<String>>>) -> Arc<dyn Plugin> {
    plugin_fn(
        "inject-scope-probe",
        Inject::default(),
        move |ctx: Ctx, _cfg| {
            let seen = seen.clone();
            Box::pin(async move {
                let required = match ctx.require("required") {
                    Ok(_) => "ok".to_string(),
                    Err(err) => err.to_string(),
                };
                let optional = match ctx.require("opt") {
                    Ok(_) => "ok".to_string(),
                    Err(err) => err.to_string(),
                };
                seen.lock().unwrap().push(format!(
                    "required={required};optional={optional};get={}",
                    ctx.get("required").is_some()
                ));
                Ok(())
            })
        },
    )
}

#[tokio::test]
async fn scoped_inject_is_not_mistaken_for_service_names() {
    let app = App::new();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let fiber = mount(
        &app.root,
        probe(seen.clone()),
        Config::Null,
        Some(json!({"required": [], "optional": ["opt"]})),
    );
    fiber.settle().await;

    assert_eq!(fiber.state(), FiberState::Active);
    let log = seen.lock().unwrap().clone();
    assert_eq!(log.len(), 1, "{log:?}");
    assert!(
        log[0].contains("required=cannot get property \"required\" without inject"),
        "required 是形状字段，不是服务名：{log:?}"
    );
    assert!(
        log[0].contains("optional=cannot get required service \"opt\" in inactive context"),
        "可选依赖缺席应报 Inactive：{log:?}"
    );
}

#[tokio::test]
async fn required_inject_gates_activation_until_the_service_exists() {
    let app = App::new();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let fiber = mount(
        &app.root,
        probe(seen.clone()),
        Config::Null,
        Some(json!({"required": ["svc"], "optional": []})),
    );
    fiber.settle().await;
    assert_eq!(fiber.state(), FiberState::Pending);
    assert!(seen.lock().unwrap().is_empty());

    let provider = plugin_fn(
        "inject-scope-provider",
        Inject::default(),
        |ctx: Ctx, _cfg| {
            Box::pin(async move {
                ctx.provide("svc", Dyn::new(1u32))?;
                Ok(())
            })
        },
    );
    app.plugin(provider, Config::Null).await.unwrap();
    fiber.settle().await;

    assert_eq!(fiber.state(), FiberState::Active);
    assert_eq!(seen.lock().unwrap().len(), 1);
}
