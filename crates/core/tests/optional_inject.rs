//! 行级 optional inject：不阻塞装载，但声明了访问意图。

use cordis_core::{plugin_fn, App, Config, Ctx, Dyn, Inject, Plugin};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

fn reader(applies: Arc<AtomicUsize>, seen: Arc<Mutex<Vec<String>>>) -> Arc<dyn Plugin> {
    plugin_fn(
        "optional-reader",
        Inject::default(),
        move |ctx: Ctx, _cfg| {
            let applies = applies.clone();
            let seen = seen.clone();
            Box::pin(async move {
                applies.fetch_add(1, Ordering::SeqCst);
                let required = match ctx.require("optional-svc") {
                    Ok(_) => "ok".to_string(),
                    Err(err) => err.to_string(),
                };
                let has = ctx.get("optional-svc").is_some();
                seen.lock()
                    .unwrap()
                    .push(format!("require={required};get={has}"));
                Ok(())
            })
        },
    )
}

#[tokio::test]
async fn optional_inject_does_not_block_and_reports_inactive_when_missing() {
    let app = App::new();
    let applies = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let fiber = app.store.plugin_with_optional(
        &app.root,
        reader(applies.clone(), seen.clone()),
        Config::Null,
        Inject::default(),
        &["optional-svc".to_string()],
    );
    fiber.settle().await;

    assert!(!fiber.is_disposed(), "可选依赖缺席不得阻塞激活");
    assert_eq!(applies.load(Ordering::SeqCst), 1);
    let log = seen.lock().unwrap().clone();
    assert_eq!(log.len(), 1, "{log:?}");
    assert!(
        log[0].contains("inactive context"),
        "声明过的可选依赖缺席应是 Inactive，而不是 NoInject：{log:?}"
    );
    assert!(log[0].contains("get=false"), "{log:?}");
}

#[tokio::test]
async fn optional_inject_is_readable_when_the_service_exists() {
    let app = App::new();
    let provider = plugin_fn("optional-provider", Inject::default(), |ctx: Ctx, _cfg| {
        Box::pin(async move {
            ctx.provide("optional-svc", Dyn::new(7u32))?;
            Ok(())
        })
    });
    app.plugin(provider, Config::Null).await.unwrap();

    let applies = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let fiber = app.store.plugin_with_optional(
        &app.root,
        reader(applies.clone(), seen.clone()),
        Config::Null,
        Inject::default(),
        &["optional-svc".to_string()],
    );
    fiber.settle().await;

    assert!(!fiber.is_disposed());
    let log = seen.lock().unwrap().clone();
    assert_eq!(log, vec!["require=ok;get=true".to_string()], "{log:?}");
}

#[tokio::test]
async fn optional_provider_changes_do_not_reload_the_consumer() {
    let app = App::new();
    let provider = plugin_fn("optional-provider", Inject::default(), |ctx: Ctx, _cfg| {
        Box::pin(async move {
            ctx.provide("optional-svc", Dyn::new(7u32))?;
            Ok(())
        })
    });
    let provider = app.plugin(provider, Config::Null).await.unwrap();

    let applies = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let consumer = app.store.plugin_with_optional(
        &app.root,
        reader(applies.clone(), seen.clone()),
        Config::Null,
        Inject::default(),
        &["optional-svc".to_string()],
    );
    consumer.settle().await;
    assert_eq!(applies.load(Ordering::SeqCst), 1);

    provider.dispose().await;
    tokio::task::yield_now().await;
    consumer.settle().await;

    assert!(!consumer.is_disposed(), "可选依赖消失不应自动卸载消费者");
    assert_eq!(
        applies.load(Ordering::SeqCst),
        1,
        "可选依赖变化不应触发重新 apply"
    );
}
