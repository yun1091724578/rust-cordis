//! Regression tests for P0/P1/P2 findings in `cordis-core`.

use cordis_core::{
    declare_volatile, equal_except_volatile, plugin_fn, with_provision, without_volatile, App,
    Config, Ctx, Dyn, FiberState, Inject, LifecyclePolicy, Plugin, ResolveError,
};
use serde_json::json;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

// P0-1: CancelToken::cancelled() with >= 2 concurrent waiters must never lose wakeup.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_token_multi_waiter_no_lost_wakeup() {
    for _ in 0..300 {
        let token = cordis_core::CancelToken::new();
        let t1 = token.clone();
        let t2 = token.clone();
        let h1 = tokio::spawn(async move {
            t1.cancelled().await;
        });
        let h2 = tokio::spawn(async move {
            t2.cancelled().await;
        });
        token.cancel();

        tokio::time::timeout(Duration::from_secs(2), async {
            h1.await.unwrap();
            h2.await.unwrap();
        })
        .await
        .expect("CancelToken::cancelled() deadlocked with 2 concurrent waiters");
    }
}

// P1-1: Disposing a fiber while `apply` is awaiting must NOT transition Disposed -> Active.
#[tokio::test]
async fn fiber_load_disposed_mid_apply_does_not_become_active() {
    let app = App::new();
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let entered_c = entered.clone();
    let release_c = release.clone();

    let saw_active_after_disposed = Arc::new(AtomicBool::new(false));
    let saw_disposed = Arc::new(AtomicBool::new(false));
    let saw_active_c = saw_active_after_disposed.clone();
    let saw_disposed_c = saw_disposed.clone();

    let _h = app.root.on("internal/status", move |call| {
        let saw_active_c = saw_active_c.clone();
        let saw_disposed_c = saw_disposed_c.clone();
        Box::pin(async move {
            if let Some(fiber) = call
                .args
                .first()
                .and_then(|d| d.downcast::<cordis_core::Fiber>())
            {
                if fiber.name() == "slow-apply" {
                    match fiber.state() {
                        FiberState::Disposed => saw_disposed_c.store(true, Ordering::SeqCst),
                        FiberState::Active if saw_disposed_c.load(Ordering::SeqCst) => {
                            saw_active_c.store(true, Ordering::SeqCst);
                        }
                        _ => {}
                    }
                }
            }
            None
        })
    });

    let fiber = app.root.plugin(
        plugin_fn("slow-apply", Inject::default(), move |_ctx, _cfg| {
            let entered = entered_c.clone();
            let release = release_c.clone();
            Box::pin(async move {
                entered.notify_one();
                release.notified().await;
                Ok(())
            })
        }),
        Config::Null,
    );

    entered.notified().await;
    let dispose_task = {
        let f = fiber.clone();
        tokio::spawn(async move { f.dispose().await })
    };
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(fiber.is_disposed());

    release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), dispose_task)
        .await
        .expect("dispose timed out")
        .unwrap();

    assert_eq!(fiber.state(), FiberState::Disposed);
    assert!(
        !saw_active_after_disposed.load(Ordering::SeqCst),
        "disposed fiber must never transition back to Active"
    );
    app.close().await;
}

// P1-2: dispose() must wait for prior timed-out background cleanups before clearing committed/ctx.
#[tokio::test]
async fn fiber_dispose_waits_for_prior_timed_out_background_cleanup() {
    let app = App::new();
    app.store
        .set_lifecycle_policy(LifecyclePolicy {
            dispose_timeout_ms: Some(30),
            ..Default::default()
        })
        .unwrap();

    let bg_started = Arc::new(Notify::new());
    let bg_release = Arc::new(Notify::new());
    let bg_finished = Arc::new(AtomicBool::new(false));
    let bg_saw_ctx_alive = Arc::new(AtomicBool::new(false));

    let s = bg_started.clone();
    let r = bg_release.clone();
    let f_done = bg_finished.clone();
    let saw_ctx = bg_saw_ctx_alive.clone();

    let fiber = app.root.plugin(
        plugin_fn("bg-cleanup", Inject::default(), move |ctx, _cfg| {
            let s = s.clone();
            let r = r.clone();
            let f_done = f_done.clone();
            let saw_ctx = saw_ctx.clone();
            Box::pin(async move {
                let c2 = ctx.clone();
                ctx.effect(move |_| {
                    cordis_core::Disposable::new(move || {
                        let s = s.clone();
                        let r = r.clone();
                        let f_done = f_done.clone();
                        let saw_ctx = saw_ctx.clone();
                        let c2 = c2.clone();
                        Box::pin(async move {
                            s.notify_one();
                            r.notified().await;
                            if let Some(fib) = c2.fiber_opt() {
                                saw_ctx.store(fib.ctx_opt().is_some(), Ordering::SeqCst);
                            }
                            f_done.store(true, Ordering::SeqCst);
                        })
                    })
                });
                Ok(())
            })
        }),
        json!({"v": 1}),
    );
    fiber.settle().await;
    assert_eq!(fiber.state(), FiberState::Active);

    let _ = fiber.update(json!({"v": 2})).await;
    bg_started.notified().await;
    tokio::time::sleep(Duration::from_millis(60)).await;

    let dispose_fut = {
        let fib = fiber.clone();
        tokio::spawn(async move { fib.dispose().await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !dispose_fut.is_finished(),
        "dispose() completed prematurely while background cleanup was still running"
    );

    bg_release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), dispose_fut)
        .await
        .expect("dispose did not finish after background cleanup completed")
        .unwrap();
    assert!(bg_finished.load(Ordering::SeqCst));
    assert!(
        bg_saw_ctx_alive.load(Ordering::SeqCst),
        "fiber ctx was cleared before background cleanup finished"
    );
    app.close().await;
}

// P1-4: Panic in on_plugin observer during begin_dispose must not deadlock dispose().
#[tokio::test]
async fn begin_dispose_survives_panicking_on_plugin_observer() {
    let app = App::new();
    let fiber = app.root.plugin(
        plugin_fn("panic-obs", Inject::default(), |_ctx, _cfg| {
            Box::pin(async { Ok(()) })
        }),
        Config::Null,
    );
    fiber.settle().await;

    let _obs = app.store.on_plugin(&app.root, |f| {
        if f.is_disposed() {
            panic!("intentional observer panic on dispose");
        }
    });

    tokio::time::timeout(Duration::from_secs(2), fiber.dispose())
        .await
        .expect("dispose() deadlocked after on_plugin observer panic");
    assert_eq!(fiber.state(), FiberState::Disposed);
    app.close().await;
}

// P1-6: Disposed plugin Ctx must NOT receive unchecked root access in Store::resolve.
#[tokio::test]
async fn disposed_plugin_ctx_resolve_is_fail_closed() {
    let app = App::new();
    let _prov = app.root.provide("secret_svc", Dyn::new(42i32)).unwrap();

    let captured: Arc<std::sync::Mutex<Option<Ctx>>> = Arc::new(std::sync::Mutex::new(None));
    let cap_c = captured.clone();
    let fiber = app.root.plugin(
        plugin_fn("ephemeral", Inject::default(), move |ctx, _cfg| {
            *cap_c.lock().unwrap() = Some(ctx);
            Box::pin(async { Ok(()) })
        }),
        Config::Null,
    );
    fiber.settle().await;
    fiber.dispose().await;
    drop(fiber);

    let disposed_ctx = captured.lock().unwrap().take().unwrap();
    assert!(disposed_ctx.fiber_opt().is_none());
    let res = app.store.resolve(&disposed_ctx, "secret_svc");
    assert!(
        matches!(res, Err(ResolveError::Inactive(_))),
        "expected ResolveError::Inactive for disposed plugin ctx, got {:?}",
        res.map(|_| "Ok")
    );
    app.close().await;
}

// P1-5: Context::once manual dispose prevents execution of already-snapshotted or subsequent dispatches.
#[tokio::test]
async fn context_once_manual_dispose_sets_fired_and_cleans_up() {
    let app = App::new();
    let count = Arc::new(AtomicUsize::new(0));
    let c = count.clone();
    let disp = app.root.once("evt", move |_| {
        let c = c.clone();
        Box::pin(async move {
            c.fetch_add(1, Ordering::SeqCst);
            None
        })
    });
    disp.run().await;
    app.root.emit("evt", vec![]).await;
    assert_eq!(count.load(Ordering::SeqCst), 0);
    assert_eq!(app.store.hook_count("evt"), 0);
    app.close().await;
}

// P2-2: without_volatile must NOT mask keys inside JSON arrays.
#[test]
fn without_volatile_does_not_mask_keys_inside_arrays() {
    let paths = vec![vec!["a".to_string(), "b".to_string()]];
    let left = json!({ "a": [{ "b": 1 }] });
    let right = json!({ "a": [{ "b": 2 }] });
    assert_eq!(without_volatile(&left, &paths), left);
    assert!(
        !equal_except_volatile(&left, &right, &paths),
        "keys inside arrays must not be masked by object volatile paths"
    );
}

// P2-5: VolatilePlugin and Provisioned wrappers must forward config_schema().
#[test]
fn wrappers_forward_config_schema() {
    struct SchemaPlugin;
    impl Plugin for SchemaPlugin {
        fn name(&self) -> &str {
            "schema-p"
        }
        fn config_schema(&self) -> Option<String> {
            Some(r#"{"type":"object"}"#.to_string())
        }
        fn apply(
            &self,
            _ctx: Ctx,
            _config: Config,
        ) -> cordis_core::BoxFuture<cordis_core::Result<()>> {
            Box::pin(async { Ok(()) })
        }
    }

    let base: Arc<dyn Plugin> = Arc::new(SchemaPlugin);
    let v = declare_volatile(base.clone(), &["x"]);
    let p = with_provision(base, vec!["svc".to_string()]);
    assert_eq!(v.config_schema().as_deref(), Some(r#"{"type":"object"}"#));
    assert_eq!(p.config_schema().as_deref(), Some(r#"{"type":"object"}"#));
}

// P2-6: update_with with panicking on_commit observer must not advance committed_rev while leaving config stale.
#[tokio::test]
async fn update_with_on_commit_panic_does_not_leave_stale_config() {
    let app = App::new();
    let fiber = app.root.plugin(
        plugin_fn("commit-panic", Inject::default(), |_ctx, _cfg| {
            Box::pin(async { Ok(()) })
        }),
        json!({"v": 1}),
    );
    fiber.settle().await;

    let _obs = app.store.on_commit(&app.root, |_f, _cfg, _no_save| {
        panic!("boom in on_commit");
    });

    let f2 = fiber.clone();
    let res = tokio::spawn(async move { f2.update(json!({"v": 2})).await }).await;
    assert!(res.is_err(), "expected panic from on_commit");
    assert_eq!(fiber.config(), json!({"v": 2}));
    app.close().await;
}

// P1-3: Concurrent unregister_fiber and plugin mount for the same PluginKey must not orphan fiber.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_unregister_and_mount_does_not_orphan_fiber() {
    let app = App::new();
    let plugin: Arc<dyn Plugin> = plugin_fn("shared-key", Inject::default(), |_ctx, _cfg| {
        Box::pin(async { Ok(()) })
    });

    for _ in 0..80 {
        let f1 = app.root.plugin(plugin.clone(), Config::Null);
        f1.settle().await;
        let root_c = app.root.clone();
        let plugin_c = plugin.clone();
        let h_dispose = tokio::spawn(async move { f1.dispose().await });
        let h_mount = tokio::spawn(async move {
            let f2 = root_c.plugin(plugin_c, Config::Null);
            f2.settle().await;
            f2
        });
        h_dispose.await.unwrap();
        let f2 = h_mount.await.unwrap();
        let found = app.store.fibers_by_plugin_name("shared-key");
        assert!(
            found.iter().any(|f| Arc::ptr_eq(f, &f2)),
            "newly mounted fiber was orphaned from Store::runtimes!"
        );
        f2.dispose().await;
    }
    app.close().await;
}

// P2-1: Reentrant App::close must not drop self.root prematurely and clear hooks before dispose finishes.
#[tokio::test]
async fn reentrant_app_close_preserves_hooks_until_disposed() {
    let app = App::new();
    let hook_present_during_dispose = Arc::new(AtomicBool::new(false));
    let flag = hook_present_during_dispose.clone();
    let store = app.store.clone();
    let _h = app.root.on("keep-alive-evt", |_| Box::pin(async { None }));

    let done = Arc::new(Notify::new());
    let done_c = done.clone();
    let f = app.root.plugin(
        plugin_fn("slow-close", Inject::default(), move |ctx, _cfg| {
            let flag = flag.clone();
            let store = store.clone();
            Box::pin(async move {
                ctx.effect(move |_| {
                    cordis_core::Disposable::new(move || {
                        let flag = flag.clone();
                        let store = store.clone();
                        Box::pin(async move {
                            tokio::time::sleep(Duration::from_millis(40)).await;
                            flag.store(store.hook_count("keep-alive-evt") > 0, Ordering::SeqCst);
                        })
                    })
                });
                Ok(())
            })
        }),
        Config::Null,
    );
    f.settle().await;

    let app_cell = Arc::new(std::sync::Mutex::new(Some(app)));
    let app_cell_c = app_cell.clone();
    f.spawn_scoped(move |_cancel| async move {
        let app = app_cell_c.lock().unwrap().take().unwrap();
        app.close().await;
        done_c.notify_one();
    })
    .unwrap();

    done.notified().await;
    f.dispose().await;
    assert!(
        hook_present_during_dispose.load(Ordering::SeqCst),
        "App::close cleared hooks before root finished disposing"
    );
}

// Problem 6: Multi-threaded interleaving stress test for Fiber absorbing Disposed invariant.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fiber_lifecycle_absorbing_disposed_invariant_under_concurrent_races() {
    let app = App::new();
    let seen_disposed: Arc<std::sync::Mutex<std::collections::HashSet<usize>>> =
        Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
    let illegal_transition = Arc::new(AtomicBool::new(false));
    let seen_c = seen_disposed.clone();
    let illegal_c = illegal_transition.clone();
    let _h = app.root.on("internal/status", move |call| {
        let seen_c = seen_c.clone();
        let illegal_c = illegal_c.clone();
        Box::pin(async move {
            if let Some(fiber) = call
                .args
                .first()
                .and_then(|d| d.downcast::<cordis_core::Fiber>())
            {
                let ptr = Arc::as_ptr(&fiber) as usize;
                let st = fiber.state();
                let mut set = seen_c.lock().unwrap();
                if st == FiberState::Disposed {
                    set.insert(ptr);
                } else if set.contains(&ptr)
                    && !matches!(st, FiberState::Disposed | FiberState::Unloading)
                {
                    illegal_c.store(true, Ordering::SeqCst);
                }
            }
            None
        })
    });

    let mut keep_alive = Vec::with_capacity(80);
    for round in 0..80u64 {
        let fiber = app.root.plugin(
            plugin_fn("race-fiber", Inject::default(), move |_ctx, _cfg| {
                Box::pin(async move {
                    tokio::task::yield_now().await;
                    Ok(())
                })
            }),
            json!({ "r": round }),
        );
        keep_alive.push(fiber.clone());
        let f1 = fiber.clone();
        let f2 = fiber.clone();
        let f3 = fiber.clone();
        let h_update = tokio::spawn(async move {
            let _ = f1.update(json!({ "r": round + 1 })).await;
        });
        let h_restart = tokio::spawn(async move {
            let _ = f2.restart().await;
        });
        let h_dispose = tokio::spawn(async move {
            f3.dispose().await;
        });

        tokio::time::timeout(Duration::from_secs(3), async {
            h_update.await.unwrap();
            h_restart.await.unwrap();
            h_dispose.await.unwrap();
        })
        .await
        .expect("concurrent settle/update/restart/dispose deadlocked");

        assert_eq!(fiber.state(), FiberState::Disposed);
    }

    assert!(
        !illegal_transition.load(Ordering::SeqCst),
        "disposed fiber transitioned into a non-teardown state under concurrent races"
    );
    app.close().await;
}

// Direct test for Store::on_commit_async: runs after sync on_commit and is awaited before update() returns.
#[tokio::test]
async fn store_on_commit_async_runs_after_sync_and_is_awaited_by_update() {
    let app = App::new();
    let fiber = app.root.plugin(
        plugin_fn("async-commit-target", Inject::default(), |_ctx, _cfg| {
            Box::pin(async { Ok(()) })
        }),
        json!({ "v": 1 }),
    );
    fiber.settle().await;

    let order = Arc::new(std::sync::Mutex::new(Vec::<&'static str>::new()));
    let o1 = order.clone();
    let _sync = app.store.on_commit(&app.root, move |_f, _cfg, _no_save| {
        o1.lock().unwrap().push("sync");
    });
    let o2 = order.clone();
    let _async_obs = app
        .store
        .on_commit_async(&app.root, move |_f, cfg, no_save| {
            let o2 = o2.clone();
            Box::pin(async move {
                assert_eq!(cfg, json!({ "v": 2 }));
                assert!(!no_save);
                tokio::task::yield_now().await;
                o2.lock().unwrap().push("async");
            })
        });

    fiber.update(json!({ "v": 2 })).await.unwrap();
    assert_eq!(*order.lock().unwrap(), vec!["sync", "async"]);
    app.close().await;
}
