//! 阶段 3：Pending 诊断必须给出精确 key 和原因，不靠日志猜。

use cordis_core::{plugin_fn, App, Check, Config, Dyn, FiberState, Inject, PendingReason};
use std::sync::Arc;

fn noop(
    _ctx: cordis_core::Ctx,
    _config: Config,
) -> cordis_core::BoxFuture<cordis_core::Result<()>> {
    Box::pin(async { Ok(()) })
}

#[tokio::test]
async fn missing_inject_reports_missing_key() {
    let app = App::new();
    let fiber = app
        .plugin(
            plugin_fn("needs-missing", Inject::of(&["missing@1"]), noop),
            Config::Null,
        )
        .await
        .unwrap();
    assert_eq!(fiber.state(), FiberState::Pending);
    let pending = fiber.pending_injects();
    assert_eq!(pending.len(), 1, "{pending:?}");
    assert_eq!(pending[0].key, "missing@1");
    assert_eq!(pending[0].reason, PendingReason::Missing);
    assert!(pending[0].related.is_empty(), "{pending:?}");
    fiber.dispose().await;
}

#[tokio::test]
async fn version_mismatch_reports_related_provider_key() {
    let app = App::new();
    let provider = app
        .plugin(
            plugin_fn("tools-v1", Inject::default(), |ctx: cordis_core::Ctx, _| {
                Box::pin(async move {
                    ctx.provide("tools@1", Dyn::new(1u32))?;
                    Ok(())
                })
            }),
            Config::Null,
        )
        .await
        .unwrap();
    provider.settle().await;
    assert_eq!(provider.state(), FiberState::Active);

    let consumer = app
        .plugin(
            plugin_fn("needs-v2", Inject::of(&["tools@2"]), noop),
            Config::Null,
        )
        .await
        .unwrap();
    let pending = consumer.pending_injects();
    assert_eq!(pending.len(), 1, "{pending:?}");
    assert_eq!(pending[0].key, "tools@2");
    assert_eq!(pending[0].reason, PendingReason::VersionMismatch);
    assert_eq!(pending[0].related, vec!["tools@1".to_string()]);

    consumer.dispose().await;
    provider.dispose().await;
}

#[tokio::test]
async fn unready_provider_is_distinguished_from_missing() {
    let app = App::new();
    let check: Check = Arc::new(|_| false);
    let provider = app
        .plugin(
            plugin_fn(
                "slow-tools",
                Inject::default(),
                move |ctx: cordis_core::Ctx, _| {
                    let check = check.clone();
                    Box::pin(async move {
                        ctx.provide_checked("slow@1", Dyn::new(1u32), check)?;
                        Ok(())
                    })
                },
            ),
            Config::Null,
        )
        .await
        .unwrap();
    provider.settle().await;

    let consumer = app
        .plugin(
            plugin_fn("needs-slow", Inject::of(&["slow@1"]), noop),
            Config::Null,
        )
        .await
        .unwrap();
    let pending = consumer.pending_injects();
    assert_eq!(pending.len(), 1, "{pending:?}");
    assert_eq!(pending[0].key, "slow@1");
    assert_eq!(pending[0].reason, PendingReason::ProviderNotReady);

    consumer.dispose().await;
    provider.dispose().await;
}

#[tokio::test]
async fn active_fiber_has_no_pending_injects() {
    let app = App::new();
    let provider = app
        .plugin(
            plugin_fn(
                "ready-provider",
                Inject::default(),
                |ctx: cordis_core::Ctx, _| {
                    Box::pin(async move {
                        ctx.provide("provided", Dyn::new(1u32))?;
                        Ok(())
                    })
                },
            ),
            Config::Null,
        )
        .await
        .unwrap();
    let fiber = app
        .plugin(
            plugin_fn("ready", Inject::of(&["provided"]), noop),
            Config::Null,
        )
        .await
        .unwrap();
    assert_eq!(fiber.state(), FiberState::Active);
    assert!(fiber.pending_injects().is_empty());
    fiber.dispose().await;
    provider.dispose().await;
}

#[tokio::test]
async fn provider_in_another_realm_is_reported_as_realm_mismatch() {
    let app = App::new();
    let realm = app.root.isolate("tools@1", Some(7));
    let provider = realm.plugin(
        plugin_fn(
            "realm-tools",
            Inject::default(),
            |ctx: cordis_core::Ctx, _| {
                Box::pin(async move {
                    ctx.provide("tools@1", Dyn::new(1u32))?;
                    Ok(())
                })
            },
        ),
        Config::Null,
    );
    provider.settle().await;
    assert_eq!(provider.state(), FiberState::Active);

    let consumer = app
        .plugin(
            plugin_fn("needs-realm-tools", Inject::of(&["tools@1"]), noop),
            Config::Null,
        )
        .await
        .unwrap();
    let pending = consumer.pending_injects();
    assert_eq!(pending.len(), 1, "{pending:?}");
    assert_eq!(
        pending[0].reason,
        PendingReason::RealmMismatch,
        "{pending:?}"
    );
    assert_eq!(pending[0].related, vec!["tools@1".to_string()]);

    consumer.dispose().await;
    provider.dispose().await;
}

#[tokio::test]
async fn provider_that_has_not_applied_is_missing_not_not_ready() {
    let app = App::new();
    let provider = app.root.plugin(
        plugin_fn(
            "not-applied",
            Inject::default(),
            |ctx: cordis_core::Ctx, _| {
                Box::pin(async move {
                    ctx.provide("late-api", Dyn::new(1u32))?;
                    Ok(())
                })
            },
        ),
        Config::Null,
    );
    assert_ne!(provider.state(), FiberState::Active);

    let consumer = app.root.plugin(
        plugin_fn("needs-late-api", Inject::of(&["late-api"]), noop),
        Config::Null,
    );
    let pending = consumer.pending_injects();
    assert_eq!(pending.len(), 1, "{pending:?}");
    assert_eq!(pending[0].reason, PendingReason::Missing, "{pending:?}");
    assert!(pending[0].related.is_empty(), "{pending:?}");

    consumer.dispose().await;
    provider.dispose().await;
}

#[tokio::test]
async fn repeated_inject_keys_are_reported_once() {
    let app = App::new();
    let consumer = app
        .plugin(
            plugin_fn("needs-twice", Inject::of(&["dup@1", "dup@1"]), noop),
            Config::Null,
        )
        .await
        .unwrap();
    let pending = consumer.pending_injects();
    assert_eq!(pending.len(), 1, "{pending:?}");
    assert_eq!(pending[0].key, "dup@1");
    consumer.dispose().await;
}

#[tokio::test]
async fn multiple_missing_keys_are_sorted_and_reported_individually() {
    let app = App::new();
    let consumer = app
        .plugin(
            plugin_fn("needs-many", Inject::of(&["z@1", "a@1"]), noop),
            Config::Null,
        )
        .await
        .unwrap();
    let pending = consumer.pending_injects();
    let keys: Vec<&str> = pending.iter().map(|item| item.key.as_str()).collect();
    assert_eq!(keys, vec!["a@1", "z@1"]);
    assert!(pending
        .iter()
        .all(|item| item.reason == PendingReason::Missing && item.related.is_empty()));
    consumer.dispose().await;
}

#[test]
fn pending_reason_names_are_stable() {
    assert_eq!(PendingReason::Missing.as_str(), "missing");
    assert_eq!(PendingReason::VersionMismatch.as_str(), "version_mismatch");
    assert_eq!(
        PendingReason::ProviderNotReady.as_str(),
        "provider_not_ready"
    );
    assert_eq!(PendingReason::RealmMismatch.as_str(), "realm_mismatch");
    assert_eq!(PendingReason::Cycle.as_str(), "cycle");
}
