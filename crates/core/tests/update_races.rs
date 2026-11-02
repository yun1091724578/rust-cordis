//! 并发 `update_with` 的收敛规格。
//!
//! 上游参照：cordiverse/cordis#194 与 PR #202。核心断言只有一条：
//! 旧 update 的 handler 不能把新 update 已提交的状态覆盖回去。旧 handler 可以在
//! 新 update 期间继续跑（inertia），但所有 chain 静默后必须对**最新**配置 replay。

use cordis_core::{App, Call, Config, Ctx, Dyn, Inject, Plugin, UpdateOutcome};
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::Notify;

fn json(value: &str) -> Config {
    serde_json::from_str(value).unwrap()
}

fn value_of(config: &Config) -> i64 {
    config
        .get("value")
        .and_then(|value| value.as_i64())
        .unwrap_or_default()
}

/// 插件在 apply 时把当前值写进 `current`；`internal/update` 的 handler
/// 对 `value == 1` 先等 `release`，用来制造“旧 handler 晚到”的窗口。
fn gated_plugin(
    current: Arc<AtomicI64>,
    applies: Arc<AtomicUsize>,
    started: Arc<Notify>,
    release: Arc<Notify>,
    veto_value: Option<i64>,
) -> Arc<dyn Plugin> {
    Arc::new(cordis_core::FnPlugin {
        name: "update-race",
        key: cordis_core::unique_key("update-race"),
        inject: Inject::default(),
        validator: None,
        f: Arc::new(move |ctx: Ctx, config: Config| {
            let current = current.clone();
            let applies = applies.clone();
            let started = started.clone();
            let release = release.clone();
            Box::pin(async move {
                applies.fetch_add(1, Ordering::SeqCst);
                current.store(value_of(&config), Ordering::SeqCst);

                let hook_current = current.clone();
                let hook_started = started.clone();
                let hook_release = release.clone();
                ctx.on("internal/update", move |call: Call| {
                    let current = hook_current.clone();
                    let started = hook_started.clone();
                    let release = hook_release.clone();
                    Box::pin(async move {
                        let config = call.args[1]
                            .downcast::<Config>()
                            .expect("第二个实参是 resolved config");
                        let value = value_of(&config);
                        if value == 1 {
                            started.notify_one();
                            release.notified().await;
                        }
                        if veto_value == Some(value) {
                            return None;
                        }
                        current.store(value, Ordering::SeqCst);
                        Some(Dyn::new(true))
                    })
                });

                ctx.provide("race.value", Dyn::new(value_of(&config)))?;
                Ok(())
            })
        }),
    })
}

fn spawn_update(
    fiber: &Arc<cordis_core::Fiber>,
    value: i64,
) -> tokio::task::JoinHandle<UpdateOutcome> {
    let fiber = fiber.clone();
    tokio::spawn(async move {
        fiber
            .update_with(json(&format!(r#"{{"value":{value}}}"#)), false)
            .await
            .expect("update 不应返回 Err")
    })
}

/// 旧 handler 在新 update 完成后才写状态：quiescence replay 必须把状态修回最新值。
#[tokio::test]
async fn superseded_handler_cannot_overwrite_latest_state() {
    let app = App::new();
    let current = Arc::new(AtomicI64::new(-1));
    let applies = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());

    let fiber = app.root.plugin(
        gated_plugin(
            current.clone(),
            applies.clone(),
            started.clone(),
            release.clone(),
            None,
        ),
        json(r#"{"value":0}"#),
    );
    fiber.settle().await;
    assert_eq!(current.load(Ordering::SeqCst), 0);

    let first = spawn_update(&fiber, 1);
    started.notified().await;

    let second = fiber
        .update_with(json(r#"{"value":2}"#), false)
        .await
        .expect("second update");
    assert_eq!(second, UpdateOutcome::Restarted);

    release.notify_one();
    assert_eq!(first.await.unwrap(), UpdateOutcome::Restarted);

    fiber.when_updates_settled().await;
    assert_eq!(
        current.load(Ordering::SeqCst),
        2,
        "旧 handler 的写入必须被 replay 修正"
    );
    assert_eq!(value_of(&fiber.resolved()), 2);
    assert_eq!(value_of(&fiber.config()), 2);
}

/// 旧 handler 晚到后否决：它不能把新 update 已 adopt 的 resolved/volatile 回滚掉。
#[tokio::test]
async fn superseded_veto_does_not_rollback_newer_config() {
    let app = App::new();
    let current = Arc::new(AtomicI64::new(-1));
    let applies = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());

    let fiber = app.root.plugin(
        gated_plugin(
            current.clone(),
            applies.clone(),
            started.clone(),
            release.clone(),
            Some(1),
        ),
        json(r#"{"value":0}"#),
    );
    fiber.settle().await;

    let first = spawn_update(&fiber, 1);
    started.notified().await;

    let second = fiber
        .update_with(json(r#"{"value":2}"#), false)
        .await
        .expect("second update");
    assert_eq!(second, UpdateOutcome::Restarted);

    release.notify_one();
    assert_eq!(first.await.unwrap(), UpdateOutcome::Vetoed);

    fiber.when_updates_settled().await;
    assert_eq!(current.load(Ordering::SeqCst), 2, "旧否决不能覆盖新配置");
    assert_eq!(value_of(&fiber.resolved()), 2);
    assert_eq!(value_of(&fiber.config()), 2);
}

/// 一串并发 update 最终必须落在最后一个值上。
#[tokio::test]
async fn burst_of_updates_settles_on_the_last_value() {
    let app = App::new();
    let current = Arc::new(AtomicI64::new(-1));
    let applies = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());

    let fiber = app.root.plugin(
        gated_plugin(
            current.clone(),
            applies.clone(),
            started.clone(),
            release.clone(),
            None,
        ),
        json(r#"{"value":0}"#),
    );
    fiber.settle().await;

    let mut pending = Vec::new();
    for value in 1..=5 {
        pending.push(spawn_update(&fiber, value));
    }
    // 至少等第一个 handler 真的进入；其余 update 可以自由并发。
    started.notified().await;
    release.notify_one();

    for task in pending {
        let outcome = task.await.unwrap();
        assert!(
            matches!(
                outcome,
                UpdateOutcome::Restarted | UpdateOutcome::Superseded
            ),
            "并发 update 只允许重启或被更新版本超越：{outcome:?}"
        );
    }

    fiber.when_updates_settled().await;
    assert_eq!(current.load(Ordering::SeqCst), 5);
    assert_eq!(value_of(&fiber.resolved()), 5);
    assert_eq!(value_of(&fiber.config()), 5);
}

/// `when_updates_settled` 在无并发时也必须立即返回，不能引入新的等待。
#[tokio::test]
async fn settled_helper_returns_without_updates() {
    let app = App::new();
    let fiber = app.root.plugin(
        gated_plugin(
            Arc::new(AtomicI64::new(-1)),
            Arc::new(AtomicUsize::new(0)),
            Arc::new(Notify::new()),
            Arc::new(Notify::new()),
            None,
        ),
        json(r#"{"value":0}"#),
    );
    fiber.settle().await;
    tokio::time::timeout(
        std::time::Duration::from_millis(100),
        fiber.when_updates_settled(),
    )
    .await
    .expect("没有 update 时不该等待");
}
