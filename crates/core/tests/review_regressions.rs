//! Regression tests for P0/P1/P2 findings in `cordis-core`.

use cordis_core::{
    declare_volatile, equal_except_volatile, plugin_fn, with_provision, without_volatile, App,
    Config, Ctx, Disposable, Dyn, FiberState, Inject, LifecyclePolicy, Plugin, ResolveError,
};
use serde_json::json;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
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

// Regression: once a fiber has completed `dispose()`, a late/stray `settle()` that was
// queued on `driver` (or called afterwards) must NOT re-enter `unload()` and bounce
// `Disposed -> Unloading -> Disposed`.
#[tokio::test]
async fn stray_settle_after_dispose_does_not_bounce_back_to_unloading() {
    let app = App::new();
    let fiber = app.root.plugin(
        plugin_fn("stray-settle-target", Inject::default(), |_ctx, _cfg| {
            Box::pin(async { Ok(()) })
        }),
        json!({}),
    );
    fiber.settle().await;
    fiber.dispose().await;
    assert_eq!(fiber.state(), FiberState::Disposed);

    let bounced = Arc::new(AtomicBool::new(false));
    let bounced_c = bounced.clone();
    let target_ptr = Arc::as_ptr(&fiber) as usize;
    let _obs = app.root.on("internal/status", move |call| {
        let bounced_c = bounced_c.clone();
        Box::pin(async move {
            if let Some(f) = call
                .args
                .first()
                .and_then(|d| d.downcast::<cordis_core::Fiber>())
            {
                if Arc::as_ptr(&f) as usize == target_ptr && f.state() != FiberState::Disposed {
                    bounced_c.store(true, Ordering::SeqCst);
                }
            }
            None
        })
    });

    // Simulate a stray `settle()` running after `dispose()` has already completed.
    fiber.settle().await;
    assert_eq!(fiber.state(), FiberState::Disposed);
    assert!(
        !bounced.load(Ordering::SeqCst),
        "stray settle() after dispose() bounced fiber out of Disposed"
    );
    app.close().await;
}

// Regression: disposing an already-quiescent Pending fiber (never loaded, no effects/tasks/
// committed view) must stay in `Disposed` without needlessly bouncing through `Unloading`.
#[tokio::test]
async fn dispose_empty_pending_fiber_does_not_bounce_through_unloading() {
    let app = App::new();
    let fiber = app.root.plugin(
        plugin_fn(
            "never-loaded-pending",
            Inject::of(&["missing_dep_for_idempotent_unload"]),
            |_ctx, _cfg| Box::pin(async { Ok(()) }),
        ),
        json!({}),
    );
    fiber.settle().await;
    assert_eq!(fiber.state(), FiberState::Pending);

    let status_events = Arc::new(AtomicUsize::new(0));
    let status_events_c = status_events.clone();
    let target_ptr = Arc::as_ptr(&fiber) as usize;
    let _obs = app.root.on("internal/status", move |call| {
        let status_events_c = status_events_c.clone();
        Box::pin(async move {
            if let Some(f) = call
                .args
                .first()
                .and_then(|d| d.downcast::<cordis_core::Fiber>())
            {
                if Arc::as_ptr(&f) as usize == target_ptr {
                    status_events_c.fetch_add(1, Ordering::SeqCst);
                }
            }
            None
        })
    });

    fiber.dispose().await;
    fiber.settle().await;
    // Allow any spawned internal/status handler to run.
    tokio::task::yield_now().await;
    assert_eq!(fiber.state(), FiberState::Disposed);
    assert_eq!(
        status_events.load(Ordering::SeqCst),
        1,
        "quiescent Pending fiber should emit only Pending -> Disposed (no Unloading bounce)"
    );
    app.close().await;
}

// Regression: after `dispose()` completes on an Active fiber with effects + scoped task +
// provided service, subsequent `dispose()`, `settle()`, `update()`, and `restart()` calls
// must all be idempotent no-ops/errors with zero status bounces out of `Disposed`.
#[tokio::test]
async fn repeated_lifecycle_calls_after_dispose_are_idempotent() {
    let app = App::new();
    let disposed_count = Arc::new(AtomicUsize::new(0));
    let dc = disposed_count.clone();
    let fiber = app.root.plugin(
        plugin_fn(
            "idempotent-after-dispose",
            Inject::default(),
            move |ctx, _| {
                let dc = dc.clone();
                Box::pin(async move {
                    let _ = ctx.provide("idempotent_svc", Dyn::new(42u32));
                    ctx.effect(move |_| {
                        let dc = dc.clone();
                        cordis_core::Disposable::new(move || {
                            let dc = dc.clone();
                            Box::pin(async move {
                                dc.fetch_add(1, Ordering::SeqCst);
                            })
                        })
                    });
                    let _ = ctx.spawn_scoped(|cancel| async move {
                        cancel.cancelled().await;
                    });
                    Ok(())
                })
            },
        ),
        json!({ "v": 1 }),
    );
    fiber.settle().await;
    assert_eq!(fiber.state(), FiberState::Active);

    fiber.dispose().await;
    assert_eq!(fiber.state(), FiberState::Disposed);
    assert_eq!(disposed_count.load(Ordering::SeqCst), 1);

    let bounced = Arc::new(AtomicBool::new(false));
    let bounced_c = bounced.clone();
    let target_ptr = Arc::as_ptr(&fiber) as usize;
    let _obs = app.root.on("internal/status", move |call| {
        let bounced_c = bounced_c.clone();
        Box::pin(async move {
            if let Some(f) = call
                .args
                .first()
                .and_then(|d| d.downcast::<cordis_core::Fiber>())
            {
                if Arc::as_ptr(&f) as usize == target_ptr {
                    bounced_c.store(true, Ordering::SeqCst);
                }
            }
            None
        })
    });

    fiber.dispose().await;
    fiber.settle().await;
    assert!(fiber.update(json!({ "v": 2 })).await.is_err());
    assert!(fiber.restart().await.is_err());
    fiber.settle().await;

    assert_eq!(fiber.state(), FiberState::Disposed);
    assert_eq!(disposed_count.load(Ordering::SeqCst), 1);
    assert!(
        !bounced.load(Ordering::SeqCst),
        "post-dispose lifecycle calls must not emit status transitions"
    );
    app.close().await;
}

// Regression: when a prior reload timed out and moved effect cleanup to background
// (`effect_bg_cleanups > 0`), `unload()` on `dispose()` must NOT treat the fiber as
// quiescent until the background cleanup completes, and only then become a no-op.
#[tokio::test]
async fn dispose_waits_for_bg_cleanups_before_becoming_quiescent() {
    let app = App::new();
    app.store
        .set_lifecycle_policy(LifecyclePolicy {
            dispose_timeout_ms: Some(15),
            ..LifecyclePolicy::default()
        })
        .unwrap();

    let gate = Arc::new(tokio::sync::Notify::new());
    let bg_finished = Arc::new(AtomicBool::new(false));
    let gate_c = gate.clone();
    let bg_finished_c = bg_finished.clone();

    let fiber = app.root.plugin(
        plugin_fn("bg-quiescent-target", Inject::default(), move |ctx, _| {
            let gate_c = gate_c.clone();
            let bg_finished_c = bg_finished_c.clone();
            Box::pin(async move {
                ctx.effect(move |_| {
                    let gate_c = gate_c.clone();
                    let bg_finished_c = bg_finished_c.clone();
                    cordis_core::Disposable::new(move || {
                        let gate_c = gate_c.clone();
                        let bg_finished_c = bg_finished_c.clone();
                        Box::pin(async move {
                            gate_c.notified().await;
                            bg_finished_c.store(true, Ordering::SeqCst);
                        })
                    })
                });
                Ok(())
            })
        }),
        json!({ "v": 1 }),
    );
    fiber.settle().await;
    assert_eq!(fiber.state(), FiberState::Active);

    // Reload times out and hands the slow effect disposer to background (`effect_bg_cleanups == 1`).
    let _ = fiber.restart().await;
    assert!(!bg_finished.load(Ordering::SeqCst));

    let f_disp = fiber.clone();
    let dispose_task = tokio::spawn(async move {
        f_disp.dispose().await;
    });

    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(
        !dispose_task.is_finished(),
        "dispose() must not treat fiber as quiescent while effect_bg_cleanups > 0"
    );

    gate.notify_waiters();
    tokio::time::timeout(Duration::from_secs(2), dispose_task)
        .await
        .expect("dispose() hung after background cleanup unblocked")
        .unwrap();
    assert!(bg_finished.load(Ordering::SeqCst));
    assert_eq!(fiber.state(), FiberState::Disposed);

    // Now quiescent: stray settle() must not bounce out of Disposed.
    let bounced = Arc::new(AtomicBool::new(false));
    let bounced_c = bounced.clone();
    let target_ptr = Arc::as_ptr(&fiber) as usize;
    let _obs = app.root.on("internal/status", move |call| {
        let bounced_c = bounced_c.clone();
        Box::pin(async move {
            if let Some(f) = call
                .args
                .first()
                .and_then(|d| d.downcast::<cordis_core::Fiber>())
            {
                if Arc::as_ptr(&f) as usize == target_ptr && f.state() != FiberState::Disposed {
                    bounced_c.store(true, Ordering::SeqCst);
                }
            }
            None
        })
    });

    fiber.settle().await;
    assert_eq!(fiber.state(), FiberState::Disposed);
    assert!(!bounced.load(Ordering::SeqCst));
    app.close().await;
}

// #2 [P1]: Three-node transitive ancestor consumer `A(owns P) -> U(third-party) -> P`
// and cross-subtree ownership+dep cycles `A1(owns P1, injects p2) / A2(owns P2, injects p1)`
// must never deadlock on close/dispose/restart.
#[tokio::test]
async fn transitive_ancestor_and_cross_subtree_teardown_cycles_do_not_deadlock() {
    async fn wait_active(fibers: &[&Arc<cordis_core::Fiber>]) {
        for _ in 0..200 {
            for f in fibers {
                f.settle().await;
            }
            if fibers.iter().all(|f| f.state() == FiberState::Active) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!(
            "fibers failed to reach Active: {:?}",
            fibers
                .iter()
                .map(|f| (f.name(), f.state()))
                .collect::<Vec<_>>()
        );
    }

    // 1. 三节点 A(owns P) -> U -> P：App::close() 不得死锁且记录告警。
    {
        let app = App::new();
        let warnings = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let w = warnings.clone();
        app.store.set_log_sink(Arc::new(move |level, _name, msg| {
            if level == cordis_core::Level::Warn {
                w.lock().unwrap().push(msg.to_string());
            }
        }));

        let ancestor_a = app.root.plugin(
            plugin_fn("ancestor-a", Inject::of(&["u"]), |_ctx, _| {
                Box::pin(async { Ok(()) })
            }),
            Config::Null,
        );
        let child_p = ancestor_a.ctx().plugin(
            plugin_fn("child-p", Inject::default(), |ctx, _| {
                Box::pin(async move {
                    ctx.provide("p", Dyn::new(1u32))?;
                    Ok(())
                })
            }),
            Config::Null,
        );
        let third_u = app.root.plugin(
            plugin_fn("third-u", Inject::of(&["p"]), |ctx, _| {
                Box::pin(async move {
                    ctx.provide("u", Dyn::new(2u32))?;
                    Ok(())
                })
            }),
            Config::Null,
        );
        wait_active(&[&child_p, &third_u, &ancestor_a]).await;

        tokio::time::timeout(Duration::from_secs(2), app.close())
            .await
            .expect("三节点 A(owns P) -> U -> P 的 App::close 发生永久死锁");
        assert_eq!(ancestor_a.state(), FiberState::Disposed);
        assert_eq!(child_p.state(), FiberState::Disposed);
        assert_eq!(third_u.state(), FiberState::Disposed);
        assert!(
            warnings
                .lock()
                .unwrap()
                .iter()
                .any(|m| m.contains("生命周期支持范围外")),
            "传递祖先消费者必须留下可诊断告警：{:?}",
            warnings.lock().unwrap()
        );
    }

    // 2. 三节点下单独执行 child_p.restart() 不得死锁（祖先卸载可能顺带处置 child_p，返回 Err 或 Ok 均可，绝不挂死或 panic）。
    {
        let app = App::new();
        let ancestor_a = app.root.plugin(
            plugin_fn("ancestor-a2", Inject::of(&["u2"]), |_ctx, _| {
                Box::pin(async { Ok(()) })
            }),
            Config::Null,
        );
        let child_p = ancestor_a.ctx().plugin(
            plugin_fn("child-p2", Inject::default(), |ctx, _| {
                Box::pin(async move {
                    ctx.provide("p2", Dyn::new(1u32))?;
                    Ok(())
                })
            }),
            Config::Null,
        );
        let third_u = app.root.plugin(
            plugin_fn("third-u2", Inject::of(&["p2"]), |ctx, _| {
                Box::pin(async move {
                    ctx.provide("u2", Dyn::new(2u32))?;
                    Ok(())
                })
            }),
            Config::Null,
        );
        wait_active(&[&child_p, &third_u, &ancestor_a]).await;

        let _ = tokio::time::timeout(Duration::from_secs(2), child_p.restart())
            .await
            .expect("child_p.restart 在传递祖先拓扑下死锁");
        tokio::time::timeout(Duration::from_secs(2), ancestor_a.dispose())
            .await
            .expect("ancestor_a.dispose 在传递祖先拓扑下死锁");
        tokio::time::timeout(Duration::from_secs(2), app.close())
            .await
            .expect("app.close 不得死锁");
    }

    // 2b. 三节点全 Active 状态下直接执行 ancestor_a.dispose() 不得死锁。
    {
        let app = App::new();
        let ancestor_a = app.root.plugin(
            plugin_fn("ancestor-a3", Inject::of(&["u3"]), |_ctx, _| {
                Box::pin(async { Ok(()) })
            }),
            Config::Null,
        );
        let child_p = ancestor_a.ctx().plugin(
            plugin_fn("child-p3", Inject::default(), |ctx, _| {
                Box::pin(async move {
                    ctx.provide("p3", Dyn::new(1u32))?;
                    Ok(())
                })
            }),
            Config::Null,
        );
        let third_u = app.root.plugin(
            plugin_fn("third-u3", Inject::of(&["p3"]), |ctx, _| {
                Box::pin(async move {
                    ctx.provide("u3", Dyn::new(2u32))?;
                    Ok(())
                })
            }),
            Config::Null,
        );
        wait_active(&[&child_p, &third_u, &ancestor_a]).await;

        tokio::time::timeout(Duration::from_secs(2), ancestor_a.dispose())
            .await
            .expect("ancestor_a.dispose 在传递祖先拓扑下死锁");
        assert_eq!(ancestor_a.state(), FiberState::Disposed);
        assert_eq!(child_p.state(), FiberState::Disposed);
        tokio::time::timeout(Duration::from_secs(2), app.close())
            .await
            .expect("app.close 不得死锁");
    }

    // 3. 跨子树环 A1(owns P1, injects p2) / A2(owns P2, injects p1)：close 不得死锁。
    {
        let app = App::new();
        let a1 = app.root.plugin(
            plugin_fn("a1", Inject::of(&["svc_p2"]), |_ctx, _| {
                Box::pin(async { Ok(()) })
            }),
            Config::Null,
        );
        let a2 = app.root.plugin(
            plugin_fn("a2", Inject::of(&["svc_p1"]), |_ctx, _| {
                Box::pin(async { Ok(()) })
            }),
            Config::Null,
        );
        let p1 = a1.ctx().plugin(
            plugin_fn("p1", Inject::default(), |ctx, _| {
                Box::pin(async move {
                    ctx.provide("svc_p1", Dyn::new(1u32))?;
                    Ok(())
                })
            }),
            Config::Null,
        );
        let p2 = a2.ctx().plugin(
            plugin_fn("p2", Inject::default(), |ctx, _| {
                Box::pin(async move {
                    ctx.provide("svc_p2", Dyn::new(2u32))?;
                    Ok(())
                })
            }),
            Config::Null,
        );
        wait_active(&[&p1, &p2, &a1, &a2]).await;

        tokio::time::timeout(Duration::from_secs(2), app.close())
            .await
            .expect("跨子树环 A1(P1)/A2(P2) 的 App::close 发生死锁");
        assert_eq!(a1.state(), FiberState::Disposed);
        assert_eq!(a2.state(), FiberState::Disposed);
        assert_eq!(p1.state(), FiberState::Disposed);
        assert_eq!(p2.state(), FiberState::Disposed);
    }
}

// #3 [P1]: `update_with` / `restart` racing with `dispose()` across await points must never
// panic on `self.ctx()` or falsely report `Ok(...)`.
#[tokio::test]
async fn update_with_and_restart_during_concurrent_dispose_return_err_without_panic() {
    // 1. `update()` 在 `on_commit_async().await` 期间被并发 `dispose()`。
    {
        let app = App::new();
        let fiber = app.root.plugin(
            plugin_fn("update-race", Inject::default(), |_ctx, _cfg| {
                Box::pin(async { Ok(()) })
            }),
            json!({ "v": 1 }),
        );
        fiber.settle().await;
        assert_eq!(fiber.state(), FiberState::Active);

        let _obs = app.store.on_commit_async(&app.root, |f, _cfg, _no_save| {
            let f = f.clone();
            Box::pin(async move {
                // 在 async commit 观察者中完整 dispose 该 fiber（清掉 ctx）。
                f.dispose().await;
            })
        });

        let res = tokio::time::timeout(Duration::from_secs(2), fiber.update(json!({ "v": 2 })))
            .await
            .expect("update 不得挂起");
        assert!(
            res.is_err(),
            "跨 await 被 dispose 的 fiber.update 必须返回 Err，不得 panic 或误报 Ok：{res:?}"
        );
        assert_eq!(fiber.state(), FiberState::Disposed);
        app.close().await;
    }

    // 2. `restart()` 卸载旧 effect 期间被并发 `dispose()`：不得误报 `Ok(())`。
    {
        let app = App::new();
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let entered_c = entered.clone();
        let release_c = release.clone();
        let fiber = app.root.plugin(
            plugin_fn("restart-race", Inject::default(), move |ctx, _cfg| {
                let entered_c = entered_c.clone();
                let release_c = release_c.clone();
                Box::pin(async move {
                    ctx.effect(move |_| {
                        let entered_c = entered_c.clone();
                        let release_c = release_c.clone();
                        cordis_core::Disposable::new(move || {
                            Box::pin(async move {
                                entered_c.notify_one();
                                release_c.notified().await;
                            })
                        })
                    });
                    Ok(())
                })
            }),
            Config::Null,
        );
        fiber.settle().await;
        assert_eq!(fiber.state(), FiberState::Active);

        let f_restart = fiber.clone();
        let restart_task = tokio::spawn(async move { f_restart.restart().await });
        entered.notified().await;

        let f_dispose = fiber.clone();
        let dispose_task = tokio::spawn(async move { f_dispose.dispose().await });
        // 等 begin_dispose 生效后再放行 effect 清理。
        for _ in 0..100 {
            if fiber.is_disposed() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert!(fiber.is_disposed());
        release.notify_waiters();

        let restart_res = tokio::time::timeout(Duration::from_secs(2), restart_task)
            .await
            .expect("restart 不得挂起")
            .expect("restart 不得 panic");
        dispose_task.await.unwrap();
        assert!(
            restart_res.is_err(),
            "卸载途中被 dispose 的 restart 不得误报 Ok(())：{restart_res:?}"
        );
        assert_eq!(fiber.state(), FiberState::Disposed);
        app.close().await;
    }
}

// #18 [P1] & #19 [P2]: `Store::set` on stale disposed `Ctx` returns `Err` without poisoning
// `Store.impls`, and drops replaced values outside the `impls` mutex lock.
#[tokio::test]
async fn store_set_on_stale_ctx_fails_closed_and_drops_replaced_value_outside_lock() {
    let app = App::new();

    // 1. 陈旧已释放 Ctx 调 Store::set / Store::provide 返回 Err，不 panic、不中毒 impls 锁。
    let captured: Arc<std::sync::Mutex<Option<Ctx>>> = Arc::new(std::sync::Mutex::new(None));
    let cap_c = captured.clone();
    let fiber = app.root.plugin(
        plugin_fn("stale-set", Inject::default(), move |ctx, _| {
            let cap_c = cap_c.clone();
            Box::pin(async move {
                ctx.provide("ephemeral_svc", Dyn::new(1i32))?;
                *cap_c.lock().unwrap() = Some(ctx);
                Ok(())
            })
        }),
        Config::Null,
    );
    fiber.settle().await;
    fiber.dispose().await;
    drop(fiber);

    let stale_ctx = captured.lock().unwrap().take().unwrap();
    assert!(stale_ctx.fiber_opt().is_none());
    assert!(
        app.store
            .set(&stale_ctx, "ephemeral_svc", Dyn::new(2i32))
            .is_err(),
        "已释放 fiber 的陈旧 Ctx 调 Store::set 必须返回 Err"
    );
    assert!(
        app.store
            .provide(&stale_ctx, "ephemeral_svc2", Dyn::new(3i32), None)
            .is_err(),
        "已释放 fiber 的陈旧 Ctx 调 Store::provide 必须返回 Err"
    );

    // 2. Drop 重入 Store::try_get / Store::set 不得死锁（证明旧值在锁外析构）。
    struct ReenterOnDrop {
        ctx: Ctx,
        ran: Arc<AtomicBool>,
    }
    impl Drop for ReenterOnDrop {
        fn drop(&mut self) {
            let _ = self.ctx.get("other_svc");
            let _ = self.ctx.set("other_svc", Dyn::new(99i32));
            self.ran.store(true, Ordering::SeqCst);
        }
    }

    let _other = app.root.provide("other_svc", Dyn::new(1i32)).unwrap();
    let drop_ran = Arc::new(AtomicBool::new(false));
    let _holder = app
        .root
        .provide(
            "drop_test_svc",
            Dyn::new(ReenterOnDrop {
                ctx: app.root.clone(),
                ran: drop_ran.clone(),
            }),
        )
        .unwrap();

    // 等 provide 触发的异步 `internal/service` 广播任务跑完并释放它持有的 Dyn 克隆，
    // 确保 Store::set 置换时持有的是最后一枚强引用，从而在 set 内部同步触发 ReenterOnDrop::drop。
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }

    tokio::time::timeout(Duration::from_secs(2), async {
        app.root
            .set("drop_test_svc", Dyn::new(2i32))
            .expect("Store::set 必须成功并在锁外析构旧值");
    })
    .await
    .expect("Store::set 在持锁期间析构旧值导致重入死锁");

    assert!(drop_ran.load(Ordering::SeqCst));
    assert_eq!(
        app.root
            .get("other_svc")
            .and_then(|v| v.downcast::<i32>())
            .map(|v| *v),
        Some(99)
    );
    app.close().await;
}

// #4 [P2]: A panicking `Check` predicate during `set_state(Active)` / `check_impl` must be caught
// (fail closed to `false`) without unwinding or blocking other dependents.
#[tokio::test]
async fn panicking_check_predicate_is_caught_and_fails_closed() {
    let app = App::new();
    let consumer_bad = app.root.plugin(
        plugin_fn(
            "bad-check-consumer",
            Inject::of(&["checked_svc"]),
            |_ctx, _| Box::pin(async { Ok(()) }),
        ),
        Config::Null,
    );
    let consumer_good = app.root.plugin(
        plugin_fn("good-consumer", Inject::of(&["healthy_svc"]), |_ctx, _| {
            Box::pin(async { Ok(()) })
        }),
        Config::Null,
    );
    consumer_bad.settle().await;
    consumer_good.settle().await;

    let provider = app.root.plugin(
        plugin_fn("panic-check-provider", Inject::default(), |ctx, _| {
            Box::pin(async move {
                ctx.store.provide(
                    &ctx,
                    "checked_svc",
                    Dyn::new(1u32),
                    Some(Arc::new(|_| panic!("intentional check panic"))),
                )?;
                ctx.provide("healthy_svc", Dyn::new(2u32))?;
                Ok(())
            })
        }),
        Config::Null,
    );
    provider.settle().await;
    consumer_bad.settle().await;
    consumer_good.settle().await;

    assert_eq!(provider.state(), FiberState::Active);
    assert_eq!(
        consumer_bad.state(),
        FiberState::Pending,
        "check 谓词 panic 必须按 false 失败关闭，消费者停在 Pending"
    );
    assert_eq!(
        consumer_good.state(),
        FiberState::Active,
        "同一提供者的其他健康消费者仍须正常激活"
    );
    app.close().await;
}

// #5 [P2]: Root fiber (`runtime == None`) must never enter an infinite settle loop on
// `restart()` / `update()` / `settle()` after provide disposal.
#[tokio::test]
async fn root_fiber_lifecycle_calls_do_not_infinite_loop() {
    let app = App::new();
    let root = app.root_fiber();
    assert_eq!(root.state(), FiberState::Active);

    assert!(root.restart().await.is_err());
    assert!(root.update(json!({ "x": 1 })).await.is_err());
    tokio::time::timeout(Duration::from_secs(1), root.settle())
        .await
        .expect("root_fiber().settle() 不得死循环");
    assert_eq!(root.state(), FiberState::Active);

    let disp = app.root.provide("root_svc", Dyn::new(1u32)).unwrap();
    tokio::time::timeout(Duration::from_secs(1), disp.run())
        .await
        .expect("释放 root provide 后不得卡死");
    tokio::time::timeout(Duration::from_secs(1), root.settle())
        .await
        .expect("root_fiber().settle() 仍应立即返回");
    assert_eq!(root.state(), FiberState::Active);
    app.close().await;
}

// #17 [P2]: `Store::provide` disposer must bound waiting on a stuck `Loading` consumer when
// `drain_timeout_ms` is configured (no unbounded second `settle().await`).
#[tokio::test]
async fn provide_disposer_respects_drain_timeout_for_loading_consumer() {
    let app = App::new();
    app.store
        .set_lifecycle_policy(LifecyclePolicy {
            drain_timeout_ms: Some(40),
            ..Default::default()
        })
        .unwrap();

    let consumer_entered = Arc::new(Notify::new());
    let consumer_release = Arc::new(Notify::new());
    let entered_c = consumer_entered.clone();
    let release_c = consumer_release.clone();

    let provider = app.root.plugin(
        plugin_fn("bounded-provider", Inject::default(), |ctx, _| {
            Box::pin(async move {
                ctx.provide("bounded_svc", Dyn::new(1u32))?;
                Ok(())
            })
        }),
        Config::Null,
    );
    provider.settle().await;
    assert_eq!(provider.state(), FiberState::Active);

    let consumer = app.root.plugin(
        plugin_fn(
            "stuck-loading-consumer",
            Inject::of(&["bounded_svc"]),
            move |_ctx, _| {
                let entered_c = entered_c.clone();
                let release_c = release_c.clone();
                Box::pin(async move {
                    entered_c.notify_one();
                    release_c.notified().await;
                    Ok(())
                })
            },
        ),
        Config::Null,
    );
    consumer_entered.notified().await;
    assert_eq!(consumer.state(), FiberState::Loading);

    // 消费者卡在 Loading 态时释放提供者：必须受 drain_timeout_ms 约束在有界时间内返回。
    tokio::time::timeout(Duration::from_secs(1), provider.dispose())
        .await
        .expect("provide disposer 的第二次 settle().await 逃出 drain_timeout_ms 导致永久等待");
    assert_eq!(provider.state(), FiberState::Disposed);

    consumer_release.notify_waiters();
    tokio::time::timeout(Duration::from_secs(2), app.close())
        .await
        .expect("释放消费者后 App::close 必须完成");
}

// ============================================================================
// Round-2 Review TDD Regression Tests (Core)
// ============================================================================

// R2 P1-1: Concurrent `Fiber::update_with` + veto must roll back `config` alongside
// `(resolved, volatile)` and preserve the running plugin's `VolatileRef` table when all veto.
#[tokio::test]
async fn concurrent_update_with_veto_preserves_config_and_volatile_refs() {
    use cordis_core::{UpdateOutcome, VolatileRef};

    // Subcase A: U1 accepted, U2 vetoed -> plugin must reload with C1 (never vetoed C2).
    {
        let app = App::new();
        let applied_configs = Arc::new(std::sync::Mutex::new(Vec::<Config>::new()));
        let applied_c = applied_configs.clone();
        let fiber = app.root.plugin(
            plugin_fn("veto-sub-a", Inject::default(), move |_ctx, cfg| {
                applied_c.lock().unwrap().push(cfg);
                Box::pin(async { Ok(()) })
            }),
            json!({ "v": 0 }),
        );
        fiber.settle().await;

        let u1_entered = Arc::new(Notify::new());
        let u1_release = Arc::new(Notify::new());
        let u1_e = u1_entered.clone();
        let u1_r = u1_release.clone();
        let _h = fiber.ctx().on("internal/update", move |call| {
            let u1_e = u1_e.clone();
            let u1_r = u1_r.clone();
            Box::pin(async move {
                let resolved = call.args.get(1).and_then(|d| d.downcast::<Config>());
                let v = resolved
                    .as_deref()
                    .and_then(|c| c.get("v"))
                    .and_then(|v| v.as_i64());
                match v {
                    Some(1) => {
                        u1_e.notify_one();
                        u1_r.notified().await;
                        call.next.call(call.args.clone()).await
                    }
                    Some(2) => None, // veto C2
                    _ => call.next.call(call.args.clone()).await,
                }
            })
        });

        let f1 = fiber.clone();
        let h1 = tokio::spawn(async move { f1.update_with(json!({ "v": 1 }), false).await });
        u1_entered.notified().await;

        let res2 = fiber.update_with(json!({ "v": 2 }), false).await.unwrap();
        assert_eq!(res2, UpdateOutcome::Vetoed);

        u1_release.notify_one();
        let res1 = h1.await.unwrap().unwrap();
        assert_eq!(res1, UpdateOutcome::Restarted);
        fiber.when_updates_settled().await;

        assert_eq!(
            fiber.config(),
            json!({ "v": 1 }),
            "U2 否决后 fiber.config() 不得残留被否决的 C2"
        );
        assert_eq!(
            applied_configs.lock().unwrap().last().cloned(),
            Some(json!({ "v": 1 })),
            "U1 放行后插件必须用 C1 重载，而不是被否决的 C2"
        );
        app.close().await;
    }

    // Subcase B: Both U2 and U1 vetoed -> config stays C0 AND cached VolatileRef from V0 stays live.
    {
        let app = App::new();
        let cached_ref: Arc<std::sync::Mutex<Option<Arc<VolatileRef>>>> =
            Arc::new(std::sync::Mutex::new(None));
        let cached_c = cached_ref.clone();
        let base = plugin_fn("veto-sub-b", Inject::default(), move |ctx, _cfg| {
            let vref = ctx.volatile("vol").expect("volatile_ref");
            *cached_c.lock().unwrap() = Some(vref);
            Box::pin(async { Ok(()) })
        });
        let fiber = app.root.plugin(
            declare_volatile(base, &["vol"]),
            json!({ "static": 0, "vol": "v0" }),
        );
        fiber.settle().await;
        let orig_ref = cached_ref.lock().unwrap().clone().unwrap();
        assert_eq!(orig_ref.get(), json!("v0"));

        let u1_entered = Arc::new(Notify::new());
        let u1_release = Arc::new(Notify::new());
        let u1_once = Arc::new(AtomicBool::new(false));
        let u1_e = u1_entered.clone();
        let u1_r = u1_release.clone();
        let u1_o = u1_once.clone();
        let _h = fiber.ctx().on("internal/update", move |call| {
            let u1_e = u1_e.clone();
            let u1_r = u1_r.clone();
            let u1_o = u1_o.clone();
            Box::pin(async move {
                let resolved = call.args.get(1).and_then(|d| d.downcast::<Config>());
                let s = resolved
                    .as_deref()
                    .and_then(|c| c.get("static"))
                    .and_then(|v| v.as_i64());
                match s {
                    Some(1) => {
                        if !u1_o.swap(true, Ordering::SeqCst) {
                            u1_e.notify_one();
                            u1_r.notified().await;
                        }
                        None // veto U1
                    }
                    Some(2) => None, // veto U2
                    _ => call.next.call(call.args.clone()).await,
                }
            })
        });

        let f1 = fiber.clone();
        let h1 = tokio::spawn(async move {
            f1.update_with(json!({ "static": 1, "vol": "v1" }), false)
                .await
        });
        u1_entered.notified().await;

        let res2 = fiber
            .update_with(json!({ "static": 2, "vol": "v2" }), false)
            .await
            .unwrap();
        assert_eq!(res2, UpdateOutcome::Vetoed);

        u1_release.notify_one();
        let res1 = h1.await.unwrap().unwrap();
        assert_eq!(res1, UpdateOutcome::Vetoed);
        fiber.when_updates_settled().await;

        assert_eq!(
            fiber.config(),
            json!({ "static": 0, "vol": "v0" }),
            "两次并发更新均被否决后 config 必须完整回滚到 C0"
        );
        assert_eq!(
            fiber.resolved(),
            json!({ "static": 0, "vol": "v0" }),
            "两次并发更新均被否决后 resolved 必须完整回滚到 C0"
        );
        let live_ref = fiber
            .volatile_ref("vol")
            .expect("volatile_ref still present");
        assert!(
            Arc::ptr_eq(&orig_ref, &live_ref),
            "否决回滚不得破坏运行中插件缓存的 V0 VolatileRef 身份"
        );
        // Subsequent volatile-only commit must STILL write into `orig_ref`!
        let changed = fiber.commit_volatile(json!({ "static": 0, "vol": "v-live" }));
        assert_eq!(changed, vec!["vol".to_string()]);
        assert_eq!(
            orig_ref.get(),
            json!("v-live"),
            "否决回滚后后续 commit_volatile 必须仍写入运行中插件缓存的 V0 VolatileRef"
        );
        app.close().await;
    }
}

// R2 P1-2: Stale `Ctx` from a disposed fiber whose `Arc<Fiber>` is still held must fail closed
// in `Store::resolve` and return `Err` (not panic) in `Store::provide`.
#[tokio::test]
async fn stale_ctx_with_live_arc_fiber_fails_closed_in_resolve_and_provide() {
    let app = App::new();
    let parent = app.root.plugin(
        plugin_fn("parent-prov", Inject::default(), |ctx, _| {
            Box::pin(async move {
                ctx.provide("parent_secret", Dyn::new(777i32))?;
                Ok(())
            })
        }),
        Config::Null,
    );
    parent.settle().await;

    let captured = Arc::new(std::sync::Mutex::new(None::<Ctx>));
    let cap_c = captured.clone();
    let child = parent.ctx().plugin(
        plugin_fn("child-ephemeral", Inject::default(), move |ctx, _| {
            *cap_c.lock().unwrap() = Some(ctx.extend());
            Box::pin(async { Ok(()) })
        }),
        Config::Null,
    );
    child.settle().await;

    // Dispose child while STILL holding `child: Arc<Fiber>`!
    child.dispose().await;
    assert_eq!(child.state(), FiberState::Disposed);

    let stale_ctx = captured.lock().unwrap().take().unwrap();
    assert!(stale_ctx.fiber_opt().is_some(), "Arc<Fiber> 仍存活");

    let resolved = app.store.resolve(&stale_ctx, "parent_secret");
    assert!(
        matches!(resolved, Err(ResolveError::Inactive(_))),
        "已处置 fiber 的陈旧 Ctx 不得绕过 fail-closed 读到父节点服务: {:?}",
        resolved.map(|_| "Ok")
    );
    let provided = stale_ctx.provide("late_svc", Dyn::new(1i32));
    assert!(
        provided.is_err(),
        "已处置 fiber 的陈旧 Ctx 调 provide 必须返回 Err 而不是 panic"
    );

    // Keep `child` alive until after assertions.
    drop(child);
    app.close().await;
}

// R2 P1-3: Clearing `self.committed` during unload/dispose must drop `Arc<Impl>`/`Dyn`
// outside the `committed` mutex lock so re-entrant `Drop` cannot deadlock.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clearing_committed_on_unload_drops_impl_outside_committed_lock() {
    struct ReenterCommittedOnDrop {
        consumer_ctx: Ctx,
        ran: Arc<AtomicBool>,
    }
    impl Drop for ReenterCommittedOnDrop {
        fn drop(&mut self) {
            let _ = self.consumer_ctx.get("committed_drop_svc");
            let _ = self
                .consumer_ctx
                .store
                .resolve(&self.consumer_ctx, "committed_drop_svc");
            self.ran.store(true, Ordering::SeqCst);
        }
    }

    let app = App::new();
    let consumer_ctx_slot = Arc::new(std::sync::Mutex::new(None::<Ctx>));
    let cslot = consumer_ctx_slot.clone();
    let drop_ran = Arc::new(AtomicBool::new(false));
    let drop_ran_c = drop_ran.clone();

    let consumer = app.root.plugin(
        plugin_fn(
            "committed-consumer",
            Inject::of(&["committed_drop_svc"]),
            move |ctx, _| {
                *cslot.lock().unwrap() = Some(ctx);
                Box::pin(async { Ok(()) })
            },
        ),
        Config::Null,
    );
    let provider_ctx_slot = Arc::new(std::sync::Mutex::new(None::<Ctx>));
    let pslot = provider_ctx_slot.clone();
    let provider = app.root.plugin(
        plugin_fn("committed-provider", Inject::default(), move |ctx, _| {
            *pslot.lock().unwrap() = Some(ctx.clone());
            Box::pin(async move {
                ctx.provide("committed_drop_svc", Dyn::new(1u32))?;
                Ok(())
            })
        }),
        Config::Null,
    );
    provider.settle().await;
    consumer.settle().await;
    assert_eq!(consumer.state(), FiberState::Active);

    // Replace value with ReenterCommittedOnDrop while consumer holds Arc<Impl> in its `committed`.
    let c_ctx = consumer_ctx_slot.lock().unwrap().clone().unwrap();
    let p_ctx = provider_ctx_slot.lock().unwrap().clone().unwrap();
    p_ctx
        .set(
            "committed_drop_svc",
            Dyn::new(ReenterCommittedOnDrop {
                consumer_ctx: c_ctx.clone(),
                ran: drop_ran_c,
            }),
        )
        .unwrap();
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }

    // Hold consumer in unloading while provider drops its refs, OR dispose consumer after
    // provider replaces the impl with a dummy so `consumer.committed` holds the last Arc<Impl>.
    // Replacing the `Impl` entry via provider dispose while consumer keeps a clone in committed:
    // Actually `provider.dispose()` waits for `consumer` to unload first; to have `consumer.committed`
    // hold the last `Arc<Impl>`, use `drain_timeout_ms` OR a second `provider.dispose()` after
    // `consumer` enters unload. Even simpler: put `ReenterCommittedOnDrop` inside an `Impl` where
    // `provider` is a child of `consumer` (ancestor consumer skips waiting!) so `provider` drops
    // `store.impls` + `provider.committed` FIRST, leaving `consumer.committed` holding the LAST `Arc<Impl>`!
    app.close().await;

    // Clean deterministic construction: `consumer` is ancestor of `provider_child`.
    let app2 = App::new();
    let drop_ran2 = Arc::new(AtomicBool::new(false));
    let dr2 = drop_ran2.clone();
    let c_slot2 = Arc::new(std::sync::Mutex::new(None::<Ctx>));
    let cs2 = c_slot2.clone();

    let ancestor_consumer = app2.root.plugin(
        plugin_fn("anc-cons", Inject::of(&["last_ref_svc"]), move |ctx, _| {
            *cs2.lock().unwrap() = Some(ctx);
            Box::pin(async { Ok(()) })
        }),
        Config::Null,
    );
    let c_slot2_for_child = c_slot2.clone();
    let child_prov = ancestor_consumer.ctx().plugin(
        plugin_fn("child-prov", Inject::default(), move |ctx, _| {
            let cs2 = c_slot2_for_child.clone();
            let dr2 = dr2.clone();
            Box::pin(async move {
                let target_ctx = cs2.lock().unwrap().clone().unwrap_or_else(|| ctx.clone());
                ctx.provide(
                    "last_ref_svc",
                    Dyn::new(ReenterCommittedOnDrop {
                        consumer_ctx: target_ctx,
                        ran: dr2,
                    }),
                )?;
                Ok(())
            })
        }),
        Config::Null,
    );
    child_prov.settle().await;
    ancestor_consumer.settle().await;
    assert_eq!(ancestor_consumer.state(), FiberState::Active);
    // Point `ReenterCommittedOnDrop.consumer_ctx` to the real `ancestor_consumer` ctx via `set`.
    let real_c_ctx = c_slot2.lock().unwrap().clone().unwrap();
    let drop_ran3 = Arc::new(AtomicBool::new(false));
    child_prov
        .ctx()
        .set(
            "last_ref_svc",
            Dyn::new(ReenterCommittedOnDrop {
                consumer_ctx: real_c_ctx,
                ran: drop_ran3.clone(),
            }),
        )
        .unwrap();
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }

    // Disposing `ancestor_consumer`:
    // 1. `ancestor_consumer` disposes its effects -> `child_prov.dispose()` drops `store.impls` & `child_prov.committed`.
    // 2. Now `ancestor_consumer.committed` holds the LAST `Arc<Impl>`!
    // 3. `ancestor_consumer.unload()` clears `self.committed` -> MUST drop outside the `committed` lock!
    tokio::time::timeout(Duration::from_secs(2), ancestor_consumer.dispose())
        .await
        .expect("在持 committed 锁期间析构 Arc<Impl> 导致同线程死锁");
    assert!(
        drop_ran3.load(Ordering::SeqCst),
        "ReenterCommittedOnDrop 必须实际触发"
    );
    let _ = drop_ran;
    let _ = drop_ran2;
    app2.close().await;
}

// R2 P2-1: `Fiber::new` racing with concurrent parent `dispose()` must not leak a disposed
// fiber into `Store::runtimes`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fiber_new_racing_with_parent_dispose_does_not_leak_in_runtimes() {
    for _ in 0..100 {
        let app = App::new();
        let parent = app.root.plugin(
            plugin_fn("race-parent", Inject::default(), |_ctx, _| {
                Box::pin(async { Ok(()) })
            }),
            Config::Null,
        );
        parent.settle().await;
        let p_ctx = parent.ctx();
        let p_disp = parent.clone();

        let h_child = tokio::spawn(async move {
            let _ = p_ctx.try_plugin(
                plugin_fn("race-child", Inject::default(), |_ctx, _| {
                    Box::pin(async { Ok(()) })
                }),
                Config::Null,
            );
        });
        let h_disp = tokio::spawn(async move {
            p_disp.dispose().await;
        });
        h_child.await.unwrap();
        h_disp.await.unwrap();

        let leaked = app.store.fibers_by_plugin_name("race-child");
        assert!(
            leaked.is_empty(),
            "父 dispose 竞态不得将已处置子 fiber 泄漏在 Store::runtimes 中（残留 {} 个）",
            leaked.len()
        );
        app.close().await;
    }
}

// R2 P2-2: Hooks registered on derived root contexts (`filter_subtree`, `extend`, `isolate`)
// must NOT form a strong reference cycle preventing `Store` drop when `App` is dropped.
#[tokio::test]
async fn derived_root_context_hook_does_not_leak_store_on_app_drop() {
    let fired = Arc::new(AtomicUsize::new(0));
    let weak_store;
    {
        let app = App::new();
        weak_store = Arc::downgrade(&app.store);

        let f1 = fired.clone();
        // Register on temporary derived root contexts and drop those temporary Ctx handles.
        let _h1 = app.root.filter_subtree().on("derived/evt", move |_| {
            f1.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { None })
        });
        let f2 = fired.clone();
        let _h2 = app.root.extend().on("derived/evt", move |_| {
            f2.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { None })
        });
        let f3 = fired.clone();
        let _h3 = app.root.isolate("iso", None).on("derived/evt", move |_| {
            f3.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { None })
        });

        app.root.emit("derived/evt", vec![]).await;
        assert_eq!(
            fired.load(Ordering::SeqCst),
            3,
            "App 存活期间临时派生根上下文的监听器仍须正常触发"
        );
    }
    assert!(
        weak_store.upgrade().is_none(),
        "派生根上下文注册的 Hook 形成强引用环，导致丢弃 App 后 Store 永久泄漏"
    );
}

// R2 P2-3: Async `ExternService::call_stream` panicking in the spawned native task must
// surface an `Err(...)` to `AsyncStream::next()`, NOT silent `Ok(None)` EOF.
#[tokio::test]
async fn async_call_iface_stream_provider_panic_reports_err_not_silent_eof() {
    struct PanickingStreamSvc;
    impl cordis_core::ExternService for PanickingStreamSvc {
        fn service(&self) -> &str {
            "panic-stream"
        }
        fn version(&self) -> u32 {
            1
        }
        fn methods(&self) -> Vec<String> {
            vec!["go".to_string()]
        }
        fn call(
            &self,
            _caller_meta: Option<Config>,
            _method: String,
            _args: Config,
        ) -> cordis_core::BoxFuture<cordis_core::Result<Config>> {
            Box::pin(async { Ok(Config::Null) })
        }
        fn call_stream(
            &self,
            _caller_meta: Option<Config>,
            _method: String,
            _args: Config,
            sink: cordis_core::StreamSink,
        ) -> cordis_core::BoxFuture<cordis_core::Result<()>> {
            Box::pin(async move {
                let _ = sink.send(json!("first")).await;
                tokio::task::yield_now().await;
                panic!("intentional async stream panic");
            })
        }
    }

    let app = App::new();
    let _prov = app
        .root
        .provide_iface("panic-stream@1", Arc::new(PanickingStreamSvc))
        .unwrap();
    let mut stream = app
        .root
        .call_iface_stream("panic-stream@1", 1, "go", json!([]))
        .unwrap();
    assert_eq!(stream.next().await.unwrap(), Some(json!("first")));
    let second = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .expect("stream.next 不得挂起");
    assert!(
        second.is_err(),
        "异步流提供者 panic 必须向消费者返回 Err，不得静默变成 Ok(None): {second:?}"
    );
    app.close().await;
}

// R2 P2-4: `Store::delete_plugin` must keep returned active fibers registered in `Store::runtimes`
// until their `dispose()` completes.
#[tokio::test]
async fn delete_plugin_keeps_active_fibers_visible_until_disposed() {
    let app = App::new();
    let key = cordis_core::unique_key("del-plugin-keep-visible");
    let plugin: Arc<dyn Plugin> = Arc::new(cordis_core::FnPlugin {
        name: "del-plugin-keep-visible",
        key: key.clone(),
        inject: Inject::default(),
        validator: None,
        f: Arc::new(|_ctx, _| Box::pin(async { Ok(()) })),
    });
    let fiber = app.root.plugin(plugin, Config::Null);
    fiber.settle().await;
    assert_eq!(fiber.state(), FiberState::Active);

    let to_dispose = app.store.delete_plugin(&key);
    assert_eq!(to_dispose.len(), 1);
    assert_eq!(
        app.store
            .fibers_by_plugin_name("del-plugin-keep-visible")
            .len(),
        1,
        "delete_plugin 返回的活跃 fiber 在真正 dispose 完成前不得从 Store::runtimes 消失"
    );

    for f in to_dispose {
        f.dispose().await;
    }
    assert!(
        app.store
            .fibers_by_plugin_name("del-plugin-keep-visible")
            .is_empty(),
        "dispose 完成后 fiber 应从 Store::runtimes 清理"
    );
    app.close().await;
}

// R2 P3-1: `Context::call_iface_stream` must bind `CALLER_UID` during synchronous `call_stream(...)`.
#[tokio::test]
async fn call_iface_stream_propagates_caller_uid_during_sync_call_stream() {
    struct SyncCallerUidProbe(Arc<std::sync::atomic::AtomicU64>);
    impl cordis_core::ExternService for SyncCallerUidProbe {
        fn service(&self) -> &str {
            "uid-probe"
        }
        fn version(&self) -> u32 {
            1
        }
        fn methods(&self) -> Vec<String> {
            vec!["probe".to_string()]
        }
        fn call(
            &self,
            _caller_meta: Option<Config>,
            _method: String,
            _args: Config,
        ) -> cordis_core::BoxFuture<cordis_core::Result<Config>> {
            Box::pin(async { Ok(Config::Null) })
        }
        fn call_stream(
            &self,
            _caller_meta: Option<Config>,
            _method: String,
            _args: Config,
            sink: cordis_core::StreamSink,
        ) -> cordis_core::BoxFuture<cordis_core::Result<()>> {
            // Read caller uid SYNCHRONOUSLY inside `call_stream(...)` before returning the future.
            self.0
                .store(cordis_core::current_caller_uid(), Ordering::SeqCst);
            Box::pin(async move {
                let _ = sink.send(json!("ok")).await;
                sink.close();
                Ok(())
            })
        }
    }

    let app = App::new();
    let observed_uid = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let _prov = app
        .root
        .provide_iface(
            "uid-probe@1",
            Arc::new(SyncCallerUidProbe(observed_uid.clone())),
        )
        .unwrap();

    let caller = app.root.plugin(
        plugin_fn("uid-caller", Inject::of(&["uid-probe@1"]), |_ctx, _| {
            Box::pin(async { Ok(()) })
        }),
        Config::Null,
    );
    caller.settle().await;
    assert_ne!(caller.uid(), 0);

    let mut stream = caller
        .ctx()
        .call_iface_stream("uid-probe@1", 1, "probe", json!([]))
        .unwrap();
    let _ = stream.next().await.unwrap();
    assert_eq!(
        observed_uid.load(Ordering::SeqCst),
        caller.uid(),
        "call_iface_stream 同步进入 call_stream 期间必须绑定发起方 CALLER_UID"
    );
    app.close().await;
}

#[tokio::test]
async fn r3_p1_concurrent_update_accept_before_veto_never_applies_vetoed_config_or_splits_volatile()
{
    use cordis_core::{Plugin, PluginKey, VolatileRef};

    struct VolPlugin {
        applied: Arc<Mutex<Vec<Config>>>,
        live_ref: Arc<Mutex<Option<Arc<VolatileRef>>>>,
    }
    impl Plugin for VolPlugin {
        fn name(&self) -> &str {
            "r3-vol-plugin"
        }
        fn key(&self) -> PluginKey {
            Arc::from("r3-vol-plugin")
        }
        fn volatile_paths(&self) -> Vec<String> {
            vec!["flag".to_string()]
        }
        fn apply(
            &self,
            ctx: cordis_core::Ctx,
            config: Config,
        ) -> cordis_core::BoxFuture<cordis_core::Result<()>> {
            self.applied.lock().unwrap().push(config);
            *self.live_ref.lock().unwrap() = ctx.volatile("flag");
            Box::pin(async { Ok(()) })
        }
    }

    let app = App::new();
    let applied = Arc::new(Mutex::new(Vec::<Config>::new()));
    let live_ref = Arc::new(Mutex::new(None));
    let plugin = Arc::new(VolPlugin {
        applied: applied.clone(),
        live_ref: live_ref.clone(),
    });
    let fiber = app.root.plugin(plugin, json!({ "flag": "v0" }));
    fiber.settle().await;
    assert_eq!(fiber.state(), FiberState::Active);

    let c1_entered = Arc::new(tokio::sync::Notify::new());
    let c2_entered = Arc::new(tokio::sync::Notify::new());
    let release_c1 = Arc::new(tokio::sync::Notify::new());
    let release_c2 = Arc::new(tokio::sync::Notify::new());
    let _hook = fiber.ctx().on("internal/update", {
        let c1_entered = c1_entered.clone();
        let c2_entered = c2_entered.clone();
        let release_c1 = release_c1.clone();
        let release_c2 = release_c2.clone();
        move |call| {
            let cfg = call
                .args
                .get(1)
                .and_then(|d| d.downcast::<Config>())
                .map(|c| (*c).clone())
                .unwrap_or(Config::Null);
            let c1_entered = c1_entered.clone();
            let c2_entered = c2_entered.clone();
            let release_c1 = release_c1.clone();
            let release_c2 = release_c2.clone();
            Box::pin(async move {
                match cfg["flag"].as_str() {
                    Some("v1") => {
                        c1_entered.notify_one();
                        release_c1.notified().await;
                        call.next.call(call.args).await
                    }
                    Some("v2") => {
                        c2_entered.notify_one();
                        release_c2.notified().await;
                        None
                    }
                    _ => call.next.call(call.args).await,
                }
            })
        }
    });

    let f1 = fiber.clone();
    let t1 = tokio::spawn(async move { f1.update_with(json!({ "flag": "v1" }), false).await });
    c1_entered.notified().await;

    let f2 = fiber.clone();
    let t2 = tokio::spawn(async move { f2.update_with(json!({ "flag": "v2" }), false).await });
    c2_entered.notified().await;

    // U1 accepts FIRST while U2 is still in-flight (unapproved).
    release_c1.notify_one();
    let r1 = t1.await.unwrap().unwrap();
    assert_eq!(r1, cordis_core::UpdateOutcome::Restarted);

    // Then U2 vetoes.
    release_c2.notify_one();
    let r2 = t2.await.unwrap().unwrap();
    assert_eq!(r2, cordis_core::UpdateOutcome::Vetoed);
    fiber.when_updates_settled().await;

    let applied_cfgs = applied.lock().unwrap().clone();
    assert!(
        !applied_cfgs.iter().any(|c| c["flag"] == json!("v2")),
        "被否决的配置 v2 绝不能传给 plugin.apply: {applied_cfgs:?}"
    );
    assert_eq!(applied_cfgs.last(), Some(&json!({ "flag": "v1" })));
    assert_eq!(fiber.config(), json!({ "flag": "v1" }));
    assert_eq!(fiber.resolved(), json!({ "flag": "v1" }));

    // Live VolatileRef held by the running plugin must still be connected to `fiber.volatile`.
    let applied_before = applied.lock().unwrap().len();
    let changed = fiber.commit_volatile(json!({ "flag": "v1-hot" }));
    assert_eq!(changed, vec!["flag".to_string()]);
    assert_eq!(applied.lock().unwrap().len(), applied_before);
    let current = live_ref.lock().unwrap().as_ref().unwrap().get();
    assert_eq!(
        current,
        json!("v1-hot"),
        "运行中插件持有的 VolatileRef 必须与 fiber.volatile 保持联动"
    );
    app.close().await;
}

#[tokio::test]
async fn r3_p1_update_veto_never_clobbers_newer_unloaded_commit() {
    let app = App::new();
    let gate = app.root.provide("dep-gate", Dyn::new(1u32)).unwrap();
    let fiber = app.root.plugin(
        plugin_fn("r3-unloaded-veto", Inject::of(&["dep-gate"]), |_ctx, _| {
            Box::pin(async { Ok(()) })
        }),
        json!({ "v": 0 }),
    );
    fiber.settle().await;
    assert_eq!(fiber.state(), FiberState::Active);

    let u1_entered = Arc::new(tokio::sync::Notify::new());
    let release_u1 = Arc::new(tokio::sync::Notify::new());
    let _hook = fiber.ctx().on("internal/update", {
        let u1_entered = u1_entered.clone();
        let release_u1 = release_u1.clone();
        move |_call| {
            let u1_entered = u1_entered.clone();
            let release_u1 = release_u1.clone();
            Box::pin(async move {
                u1_entered.notify_one();
                release_u1.notified().await;
                None
            })
        }
    });

    let f1 = fiber.clone();
    let t1 = tokio::spawn(async move { f1.update_with(json!({ "v": 1 }), false).await });
    u1_entered.notified().await;

    // Revoke dependency so fiber transitions to unloaded (`running == None`).
    gate.run().await;
    fiber.settle().await;
    assert_eq!(fiber.state(), FiberState::Pending);

    // Commit U2 while `loaded == false`.
    let r2 = fiber.update_with(json!({ "v": 2 }), false).await.unwrap();
    assert_eq!(r2, cordis_core::UpdateOutcome::Restarted);
    assert_eq!(fiber.config(), json!({ "v": 2 }));

    // Now U1 vetoes: must NOT clobber newer U2 (`v: 2`) back to `v: 0`.
    release_u1.notify_one();
    let r1 = t1.await.unwrap().unwrap();
    assert_eq!(r1, cordis_core::UpdateOutcome::Vetoed);
    assert_eq!(
        fiber.config(),
        json!({ "v": 2 }),
        "旧 U1 否决不得覆盖之后 unloaded 状态下已提交的新配置 v: 2"
    );
    app.close().await;
}

#[tokio::test]
async fn r3_p1_manual_provide_dispose_racing_with_provider_dispose_still_waits_for_consumer() {
    let app = App::new();
    let log = Arc::new(Mutex::new(Vec::<&'static str>::new()));
    let consumer_disposing = Arc::new(tokio::sync::Notify::new());
    let release_consumer = Arc::new(tokio::sync::Notify::new());
    let provide_handle = Arc::new(Mutex::new(None::<Disposable>));

    let provider = app.root.plugin(
        plugin_fn("r3-prov", Inject::default(), {
            let log = log.clone();
            let provide_handle = provide_handle.clone();
            move |ctx, _| {
                let log = log.clone();
                let provide_handle = provide_handle.clone();
                Box::pin(async move {
                    // Register underlying resource disposer FIRST (runs LAST in LIFO).
                    let _res = ctx.effect({
                        let log = log.clone();
                        move |_| {
                            let log = log.clone();
                            Disposable::sync(move || {
                                log.lock().unwrap().push("provider_resource_closed");
                            })
                        }
                    });
                    let p_disp = ctx.provide("r3-svc", Dyn::new(1u32))?;
                    *provide_handle.lock().unwrap() = Some(p_disp);
                    Ok(())
                })
            }
        }),
        Config::Null,
    );
    provider.settle().await;

    let consumer = app.root.plugin(
        plugin_fn("r3-cons", Inject::of(&["r3-svc"]), {
            let log = log.clone();
            let consumer_disposing = consumer_disposing.clone();
            let release_consumer = release_consumer.clone();
            move |ctx, _| {
                let log = log.clone();
                let consumer_disposing = consumer_disposing.clone();
                let release_consumer = release_consumer.clone();
                Box::pin(async move {
                    let _ = ctx.effect(move |_| {
                        let log = log.clone();
                        let consumer_disposing = consumer_disposing.clone();
                        let release_consumer = release_consumer.clone();
                        Disposable::new(move || {
                            Box::pin(async move {
                                consumer_disposing.notify_one();
                                release_consumer.notified().await;
                                log.lock().unwrap().push("consumer_disposed");
                            })
                        })
                    });
                    Ok(())
                })
            }
        }),
        Config::Null,
    );
    consumer.settle().await;
    assert_eq!(consumer.state(), FiberState::Active);

    // Manually dispose the `provide` handle; consumer starts async teardown.
    let p_disp = provide_handle.lock().unwrap().take().unwrap();
    let revoke_task = tokio::spawn(async move {
        p_disp.run().await;
    });
    consumer_disposing.notified().await;

    // While consumer disposer is still running, dispose the provider fiber.
    let prov = provider.clone();
    let dispose_task = tokio::spawn(async move {
        prov.dispose().await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !log.lock().unwrap().contains(&"provider_resource_closed"),
        "消费者尚未拆完，提供者资源不得提前关闭: {:?}",
        *log.lock().unwrap()
    );

    release_consumer.notify_one();
    revoke_task.await.unwrap();
    dispose_task.await.unwrap();

    assert_eq!(
        *log.lock().unwrap(),
        vec!["consumer_disposed", "provider_resource_closed"],
        "必须先等消费者拆完，再关闭提供者资源"
    );
    app.close().await;
}

#[tokio::test]
async fn r3_p1_drop_outside_lock_topology_and_hooks_never_deadlock() {
    let app = App::new();

    // 1. `ScopeAnchor::bind_parent` must drop old parent (and its predicate closure)
    //    OUTSIDE `TOPOLOGY` write lock and `self.parent` lock.
    struct ReenterTopologyOnDrop {
        probe_a: cordis_core::Ctx,
        probe_b: cordis_core::Ctx,
    }
    impl Drop for ReenterTopologyOnDrop {
        fn drop(&mut self) {
            let _ = self.probe_a.in_scope_of(&self.probe_b);
        }
    }
    let child = app.root.filter_subtree();
    let s_new = app.root.filter_subtree();
    {
        let guard = ReenterTopologyOnDrop {
            probe_a: child.clone(),
            probe_b: s_new.clone(),
        };
        let s_old = app.root.filter(Arc::new(move |_| {
            let _keep = &guard;
            true
        }));
        child.bind_scope_parent(&s_old).unwrap();
    }
    // Rebinding drops `s_old` + `guard`; deadlocks if `TOPOLOGY` write lock is still held.
    let (tx1, rx1) = std::sync::mpsc::channel();
    let child_bg = child.clone();
    let s_new_bg = s_new.clone();
    std::thread::spawn(move || {
        child_bg.bind_scope_parent(&s_new_bg).unwrap();
        let _ = tx1.send(());
    });
    rx1.recv_timeout(Duration::from_secs(2))
        .expect("bind_scope_parent 不得在持有 TOPOLOGY 锁时 drop 旧父作用域");

    // 2. `Store::on` disposer must drop handler closure OUTSIDE `Store.hooks` lock.
    struct ReenterHooksOnDrop {
        store: Arc<cordis_core::Store>,
    }
    impl Drop for ReenterHooksOnDrop {
        fn drop(&mut self) {
            let _ = self.store.hook_count("r3-drop-evt");
        }
    }
    let guard = ReenterHooksOnDrop {
        store: app.store.clone(),
    };
    let listener = app.root.on("r3-drop-evt", move |_| {
        let _keep = &guard;
        Box::pin(async { None })
    });
    let (tx2, rx2) = std::sync::mpsc::channel();
    let rt = tokio::runtime::Handle::current();
    std::thread::spawn(move || {
        rt.block_on(listener.run());
        let _ = tx2.send(());
    });
    rx2.recv_timeout(Duration::from_secs(2))
        .expect("移除 hook 时不得在持有 Store.hooks 锁时 drop handler");
    app.close().await;
}

#[tokio::test]
async fn r3_p2_delete_plugin_retires_runtime_so_remount_runs_v2_apply() {
    use cordis_core::{Plugin, PluginKey};

    struct VersionedPlugin {
        version: u32,
        hits: Arc<Mutex<Vec<u32>>>,
        hold_dispose: Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>,
    }
    impl Plugin for VersionedPlugin {
        fn name(&self) -> &str {
            "r3-hot-plugin"
        }
        fn key(&self) -> PluginKey {
            Arc::from("r3-hot-plugin")
        }
        fn apply(
            &self,
            ctx: cordis_core::Ctx,
            _config: Config,
        ) -> cordis_core::BoxFuture<cordis_core::Result<()>> {
            self.hits.lock().unwrap().push(self.version);
            if let Some((entered, release)) = self.hold_dispose.clone() {
                let _ = ctx.effect(move |_| {
                    Disposable::new(move || {
                        Box::pin(async move {
                            entered.notify_one();
                            release.notified().await;
                        })
                    })
                });
            }
            Box::pin(async { Ok(()) })
        }
    }

    let app = App::new();
    let hits = Arc::new(Mutex::new(Vec::<u32>::new()));
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());

    let v1 = Arc::new(VersionedPlugin {
        version: 1,
        hits: hits.clone(),
        hold_dispose: Some((entered.clone(), release.clone())),
    });
    let f1 = app.root.plugin(v1, Config::Null);
    f1.settle().await;
    assert_eq!(*hits.lock().unwrap(), vec![1]);

    // Begin hot replacement: delete_plugin while f1 is still disposing.
    let old_fibers = app.store.delete_plugin("r3-hot-plugin");
    assert_eq!(old_fibers.len(), 1);
    let dispose_task = tokio::spawn(async move {
        for f in old_fibers {
            f.dispose().await;
        }
    });
    entered.notified().await;

    // Mount v2 with the same PluginKey while v1's fiber is still disposing.
    let v2 = Arc::new(VersionedPlugin {
        version: 2,
        hits: hits.clone(),
        hold_dispose: None,
    });
    let f2 = app.root.plugin(v2, Config::Null);
    f2.settle().await;
    assert_eq!(
        *hits.lock().unwrap(),
        vec![1, 2],
        "delete_plugin 后的新版本挂载必须创建新 Runtime 并运行 v2.apply，不得复用旧 v1"
    );

    release.notify_one();
    dispose_task.await.unwrap();
    app.close().await;
}

#[tokio::test]
async fn r3_p2_resolve_raw_and_resolve_config_fail_closed_on_disposed_fiber() {
    let app = App::new();
    let fiber = app.root.plugin(
        plugin_fn("r3-stale-resolve", Inject::default(), |_ctx, _| {
            Box::pin(async { Ok(()) })
        }),
        Config::Null,
    );
    fiber.settle().await;
    let stale_ctx = fiber.ctx();
    let rt = fiber.runtime.clone().unwrap();
    fiber.dispose().await;

    assert!(
        fiber.resolve_raw(&json!({ "a": 1 })).await.is_err(),
        "已处置 fiber 的 resolve_raw 必须返回 Err 而非 panic"
    );
    assert!(
        app.store
            .resolve_config(&stale_ctx, &rt, json!({ "a": 1 }))
            .await
            .is_err(),
        "陈旧 Ctx 的 resolve_config 必须返回 Err 而非 panic"
    );
    // Builtin `internal/update` splice hook must also not panic on stale Ctx.
    let res = stale_ctx
        .waterfall("internal/update", vec![], None, None)
        .await;
    assert!(res.is_none());
    app.close().await;
}

#[tokio::test]
async fn r3_p2_run_replay_handler_panic_does_not_hang_when_updates_settled() {
    let app = App::new();
    let fiber = app.root.plugin(
        plugin_fn("r3-replay-panic", Inject::default(), |_ctx, _| {
            Box::pin(async { Ok(()) })
        }),
        json!({ "v": 0 }),
    );
    fiber.settle().await;

    let u1_entered = Arc::new(tokio::sync::Notify::new());
    let release_u1 = Arc::new(tokio::sync::Notify::new());
    let _hook = fiber.ctx().on("internal/update", {
        let u1_entered = u1_entered.clone();
        let release_u1 = release_u1.clone();
        move |call| {
            let no_save = call
                .args
                .get(2)
                .and_then(|d| d.downcast::<bool>())
                .map(|b| *b)
                .unwrap_or(false);
            let v = call
                .args
                .get(1)
                .and_then(|d| d.downcast::<Config>())
                .and_then(|c| c["v"].as_u64())
                .unwrap_or(0);
            let u1_entered = u1_entered.clone();
            let release_u1 = release_u1.clone();
            Box::pin(async move {
                if no_save {
                    panic!("simulated replay handler panic");
                }
                if v == 1 {
                    u1_entered.notify_one();
                    release_u1.notified().await;
                    return None;
                }
                call.next.call(call.args).await
            })
        }
    });

    let f1 = fiber.clone();
    let t1 = tokio::spawn(async move { f1.update_with(json!({ "v": 1 }), false).await });
    u1_entered.notified().await;

    // U2 finishes while U1 is still in-flight -> sets `replay_owed = true` when U1 ends.
    let _ = fiber.update_with(json!({ "v": 2 }), false).await;
    release_u1.notify_one();
    let _ = t1.await;

    tokio::time::timeout(Duration::from_secs(2), fiber.when_updates_settled())
        .await
        .expect("run_replay 中 handler panic 后 when_updates_settled 绝不能永久挂起");
    app.close().await;
}

#[test]
fn r3_p2_root_provide_does_not_leak_app_on_drop() {
    let weak_store = {
        let app = App::new();
        let _disp = app.root.provide("r3-root-svc", Dyn::new(42u32)).unwrap();
        Arc::downgrade(&app.store)
    };
    assert_eq!(
        weak_store.strong_count(),
        0,
        "app.root.provide 后直接 drop App 不得因强引用环泄漏 Store"
    );
}

#[tokio::test]
async fn r3_p2_fire_plugin_dispose_async_isolates_observer_panic() {
    let app = App::new();
    let second_ran = Arc::new(AtomicBool::new(false));
    let _obs1 = app.store.on_plugin_dispose_async(&app.root, |_fiber| {
        Box::pin(async move {
            panic!("first async dispose observer panics");
        })
    });
    let _obs2 = app.store.on_plugin_dispose_async(&app.root, {
        let second_ran = second_ran.clone();
        move |_fiber| {
            let second_ran = second_ran.clone();
            Box::pin(async move {
                second_ran.store(true, Ordering::SeqCst);
            })
        }
    });

    let fiber = app.root.plugin(
        plugin_fn("r3-dispose-obs", Inject::default(), |_ctx, _| {
            Box::pin(async { Ok(()) })
        }),
        Config::Null,
    );
    fiber.settle().await;
    fiber.dispose().await;

    assert!(
        second_ran.load(Ordering::SeqCst),
        "首个异步处置观察者 panic 不得跳过后续观察者"
    );
    assert!(
        fiber.ctx_opt().is_none(),
        "处置收尾即使观察者 panic 也必须清空 Fiber.ctx"
    );
    app.close().await;
}

#[tokio::test]
async fn r3_p3_once_waterfall_does_not_short_circuit_after_fired() {
    let app = App::new();
    let hits = Arc::new(AtomicUsize::new(0));
    let _once = app.root.once("r3-pipe", {
        let hits = hits.clone();
        move |call| {
            hits.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                // Keep handler suspended briefly so concurrent second dispatch snapshots it too.
                tokio::time::sleep(Duration::from_millis(20)).await;
                call.next.call(call.args).await
            })
        }
    });

    let tail1: cordis_core::BoxFuture<cordis_core::Ret> = Box::pin(async { Some(Dyn::new(10u32)) });
    let tail2: cordis_core::BoxFuture<cordis_core::Ret> = Box::pin(async { Some(Dyn::new(20u32)) });
    let (r1, r2) = tokio::join!(
        app.root.waterfall("r3-pipe", vec![], Some(tail1), None),
        app.root.waterfall("r3-pipe", vec![], Some(tail2), None),
    );
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    assert_eq!(
        r1.and_then(|d| d.downcast::<u32>().map(|v| *v)),
        Some(10),
        "第一次 waterfall 必须走到 tail"
    );
    assert_eq!(
        r2.and_then(|d| d.downcast::<u32>().map(|v| *v)),
        Some(20),
        "并发第二次 waterfall 遇到已触发的 once 必须透传到 next 而非返回 None 短路"
    );
    app.close().await;
}

#[tokio::test]
async fn r3_p3_call_iface_stream_nested_root_and_correlated_call_iface_caller_uid() {
    struct InnerUidProbe(Arc<std::sync::atomic::AtomicU64>);
    impl cordis_core::ExternService for InnerUidProbe {
        fn service(&self) -> &str {
            "inner-uid"
        }
        fn version(&self) -> u32 {
            1
        }
        fn methods(&self) -> Vec<String> {
            vec!["probe".to_string()]
        }
        fn call(
            &self,
            _caller_meta: Option<Config>,
            _method: String,
            _args: Config,
        ) -> cordis_core::BoxFuture<cordis_core::Result<Config>> {
            let out = self.0.clone();
            Box::pin(async move {
                out.store(cordis_core::current_caller_uid(), Ordering::SeqCst);
                Ok(Config::Null)
            })
        }
        fn call_stream(
            &self,
            _caller_meta: Option<Config>,
            _method: String,
            _args: Config,
            sink: cordis_core::StreamSink,
        ) -> cordis_core::BoxFuture<cordis_core::Result<()>> {
            let out = self.0.clone();
            Box::pin(async move {
                out.store(cordis_core::current_caller_uid(), Ordering::SeqCst);
                let _ = sink.send(json!("ok")).await;
                sink.close();
                Ok(())
            })
        }
    }

    struct OuterCaller {
        root: cordis_core::Ctx,
    }
    impl cordis_core::ExternService for OuterCaller {
        fn service(&self) -> &str {
            "outer-uid"
        }
        fn version(&self) -> u32 {
            1
        }
        fn methods(&self) -> Vec<String> {
            vec!["nested".to_string()]
        }
        fn call(
            &self,
            _caller_meta: Option<Config>,
            _method: String,
            _args: Config,
        ) -> cordis_core::BoxFuture<cordis_core::Result<Config>> {
            Box::pin(async { Ok(Config::Null) })
        }
        fn call_stream(
            &self,
            _caller_meta: Option<Config>,
            _method: String,
            _args: Config,
            sink: cordis_core::StreamSink,
        ) -> cordis_core::BoxFuture<cordis_core::Result<()>> {
            let root = self.root.clone();
            Box::pin(async move {
                let mut inner = root
                    .call_iface_stream("inner-uid@1", 1, "probe", json!([]))
                    .unwrap();
                let _ = inner.next().await;
                let _ = sink.send(json!("done")).await;
                sink.close();
                Ok(())
            })
        }
    }

    let app = App::new();
    let inner_uid = Arc::new(std::sync::atomic::AtomicU64::new(999));
    let _p1 = app
        .root
        .provide_iface("inner-uid@1", Arc::new(InnerUidProbe(inner_uid.clone())))
        .unwrap();
    let _p2 = app
        .root
        .provide_iface(
            "outer-uid@1",
            Arc::new(OuterCaller {
                root: app.root.clone(),
            }),
        )
        .unwrap();

    let caller = app.root.plugin(
        plugin_fn(
            "r3-uid-caller",
            Inject::of(&["inner-uid@1", "outer-uid@1"]),
            |_ctx, _| Box::pin(async { Ok(()) }),
        ),
        Config::Null,
    );
    caller.settle().await;
    assert_ne!(caller.uid(), 0);

    // 1. Nested `root.call_iface_stream` inside non-root provider must see UID_ROOT (0).
    let mut s = caller
        .ctx()
        .call_iface_stream("outer-uid@1", 1, "nested", json!([]))
        .unwrap();
    let _ = s.next().await;
    assert_eq!(
        inner_uid.load(Ordering::SeqCst),
        cordis_core::UID_ROOT,
        "嵌套 root.call_iface_stream 首 poll 不得泄漏外层非根 CALLER_UID"
    );

    // 2. Untraced correlated `call_iface` from plugin must report caller's uid, not hardcoded 0.
    inner_uid.store(999, Ordering::SeqCst);
    let corr_ctx = caller.ctx().with_correlation("req-r3");
    corr_ctx
        .call_iface("inner-uid@1", 1, "probe", json!([]))
        .await
        .unwrap();
    assert_eq!(
        inner_uid.load(Ordering::SeqCst),
        caller.uid(),
        "未装 TraceSink 但带 correlation_id 的 call_iface 不得把 CALLER_UID 硬编码为 0"
    );
    app.close().await;
}

#[tokio::test]
async fn r3_p3_store_set_fails_closed_on_disposed_fiber() {
    let app = App::new();
    let disposing = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let fiber = app.root.plugin(
        plugin_fn("r3-set-disposed", Inject::default(), {
            let disposing = disposing.clone();
            let release = release.clone();
            move |ctx, _| {
                let disposing = disposing.clone();
                let release = release.clone();
                Box::pin(async move {
                    // Register provide FIRST so the blocking effect runs BEFORE provide's disposer.
                    ctx.provide("r3-set-svc", Dyn::new(1u32))?;
                    let _ = ctx.effect(move |_| {
                        let disposing = disposing.clone();
                        let release = release.clone();
                        Disposable::new(move || {
                            Box::pin(async move {
                                disposing.notify_one();
                                release.notified().await;
                            })
                        })
                    });
                    Ok(())
                })
            }
        }),
        Config::Null,
    );
    fiber.settle().await;
    let stale_ctx = fiber.ctx();

    let f = fiber.clone();
    let dispose_task = tokio::spawn(async move { f.dispose().await });
    disposing.notified().await;

    // Fiber is already marked Disposed while `r3-set-svc` is still in `store.impls`.
    let err = stale_ctx.set("r3-set-svc", Dyn::new(2u32));
    assert!(
        err.is_err(),
        "已处置 fiber 上即使 impls 尚未摘除，Store::set 也必须 fail-closed 返回 Err"
    );

    release.notify_one();
    dispose_task.await.unwrap();
    app.close().await;
}

#[tokio::test]
async fn r3_p3_notify_realm_broadcast_uses_realm_override() {
    let app = App::new();
    let isolated = app.root.isolate("r3-realm-svc", None);
    let _p_def = app.root.provide("r3-realm-svc", Dyn::new(1u32)).unwrap();
    let _p_iso = isolated.provide("r3-realm-svc", Dyn::new(2u32)).unwrap();
    // 让 provide 触发的后台 `internal/service` 广播先结算，避免与下方显式 `notify_realm` 混在一起。
    tokio::time::sleep(Duration::from_millis(20)).await;
    let iso_realm = app.store.realm_of(&isolated, "r3-realm-svc");

    let seen_iso = Arc::new(Mutex::new(Vec::<Option<u32>>::new()));
    let seen_def = Arc::new(Mutex::new(Vec::<Option<u32>>::new()));
    let _h_iso = isolated.on("internal/service", {
        let seen_iso = seen_iso.clone();
        move |call| {
            let val = call
                .args
                .get(1)
                .and_then(|d| d.downcast::<Option<Dyn>>())
                .and_then(|opt| {
                    (*opt)
                        .as_ref()
                        .and_then(|v| v.downcast::<u32>().map(|n| *n))
                });
            seen_iso.lock().unwrap().push(val);
            Box::pin(async { None })
        }
    });
    let _h_def = app.root.on("internal/service", {
        let seen_def = seen_def.clone();
        move |call| {
            let val = call
                .args
                .get(1)
                .and_then(|d| d.downcast::<Option<Dyn>>())
                .and_then(|opt| {
                    (*opt)
                        .as_ref()
                        .and_then(|v| v.downcast::<u32>().map(|n| *n))
                });
            seen_def.lock().unwrap().push(val);
            Box::pin(async { None })
        }
    });

    app.store
        .notify_realm(&["r3-realm-svc"], &app.root, iso_realm, true);
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert_eq!(
        *seen_iso.lock().unwrap(),
        vec![Some(2)],
        "notify_realm(..., true) 必须广播 realm_override 域的值给该域监听者"
    );
    assert!(
        seen_def.lock().unwrap().is_empty(),
        "default 域监听者不得收到 isolated 域的 notify_realm 广播"
    );
    app.close().await;
}

#[tokio::test]
async fn r4_p1_when_updates_settled_does_not_hang_on_dispose_or_replay_panic() {
    use tokio::sync::Notify;
    const INTERNAL_UPDATE: &str = "internal/update";

    // Path A: concurrent updates set `replay_owed = true`, then `dispose()` runs before chains exit.
    let app = App::new();
    let c1_entered = Arc::new(Notify::new());
    let c2_entered = Arc::new(Notify::new());
    let release_c1 = Arc::new(Notify::new());
    let release_c2 = Arc::new(Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));

    let f = app.root.plugin(
        plugin_fn("r4-quiesce-dispose", Inject::default(), {
            let c1_entered = c1_entered.clone();
            let c2_entered = c2_entered.clone();
            let release_c1 = release_c1.clone();
            let release_c2 = release_c2.clone();
            let calls = calls.clone();
            move |ctx, _cfg| {
                let c1_entered = c1_entered.clone();
                let c2_entered = c2_entered.clone();
                let release_c1 = release_c1.clone();
                let release_c2 = release_c2.clone();
                let calls = calls.clone();
                Box::pin(async move {
                    let _ = ctx.on(INTERNAL_UPDATE, move |_call| {
                        let n = calls.fetch_add(1, Ordering::SeqCst);
                        let c1_entered = c1_entered.clone();
                        let c2_entered = c2_entered.clone();
                        let release_c1 = release_c1.clone();
                        let release_c2 = release_c2.clone();
                        Box::pin(async move {
                            if n == 0 {
                                c1_entered.notify_one();
                                release_c1.notified().await;
                            } else if n == 1 {
                                c2_entered.notify_one();
                                release_c2.notified().await;
                            }
                            None
                        })
                    });
                    Ok(())
                })
            }
        }),
        json!({"v": 0}),
    );
    f.settle().await;
    assert_eq!(f.state(), FiberState::Active);

    let f1 = f.clone();
    let u1 = tokio::spawn(async move { f1.update_with(json!({"v": 1}), true).await });
    c1_entered.notified().await;

    let f2 = f.clone();
    let u2 = tokio::spawn(async move { f2.update_with(json!({"v": 2}), true).await });
    c2_entered.notified().await;

    // u1 finishes while live_rev == 2 => arms `replay_owed = true`.
    release_c1.notify_one();
    let _ = u1.await.unwrap();

    // Dispose fiber while u2 is still active; u2 finishes afterwards.
    let f_disp = f.clone();
    let disp = tokio::spawn(async move { f_disp.dispose().await });
    tokio::time::sleep(Duration::from_millis(20)).await;
    release_c2.notify_one();
    let _ = u2.await.unwrap();
    disp.await.unwrap();

    tokio::time::timeout(Duration::from_secs(2), f.when_updates_settled())
        .await
        .expect("dispose 与并发 update 竞态后 when_updates_settled 不得永久挂死");
    app.close().await;

    // Path B: `run_replay()` panics while a new update re-armed `replay_owed = true`.
    let app2 = App::new();
    let u1_in = Arc::new(Notify::new());
    let u2_in = Arc::new(Notify::new());
    let rel_u1 = Arc::new(Notify::new());
    let rel_u2 = Arc::new(Notify::new());
    let replay_in = Arc::new(Notify::new());
    let rel_replay = Arc::new(Notify::new());
    let hit = Arc::new(AtomicUsize::new(0));

    let f = app2.root.plugin(
        plugin_fn("r4-quiesce-panic", Inject::default(), {
            let u1_in = u1_in.clone();
            let u2_in = u2_in.clone();
            let rel_u1 = rel_u1.clone();
            let rel_u2 = rel_u2.clone();
            let replay_in = replay_in.clone();
            let rel_replay = rel_replay.clone();
            let hit = hit.clone();
            move |ctx, _cfg| {
                let u1_in = u1_in.clone();
                let u2_in = u2_in.clone();
                let rel_u1 = rel_u1.clone();
                let rel_u2 = rel_u2.clone();
                let replay_in = replay_in.clone();
                let rel_replay = rel_replay.clone();
                let hit = hit.clone();
                Box::pin(async move {
                    let _ = ctx.on(INTERNAL_UPDATE, move |_call| {
                        let idx = hit.fetch_add(1, Ordering::SeqCst);
                        let u1_in = u1_in.clone();
                        let u2_in = u2_in.clone();
                        let rel_u1 = rel_u1.clone();
                        let rel_u2 = rel_u2.clone();
                        let replay_in = replay_in.clone();
                        let rel_replay = rel_replay.clone();
                        Box::pin(async move {
                            match idx {
                                0 => {
                                    u1_in.notify_one();
                                    rel_u1.notified().await;
                                }
                                1 => {
                                    u2_in.notify_one();
                                    rel_u2.notified().await;
                                }
                                2 => {
                                    replay_in.notify_one();
                                    rel_replay.notified().await;
                                    panic!("synthetic replay panic");
                                }
                                _ => {}
                            }
                            None
                        })
                    });
                    Ok(())
                })
            }
        }),
        json!({"v": 0}),
    );
    f.settle().await;

    let f1 = f.clone();
    let t1 = tokio::spawn(async move { f1.update_with(json!({"v": 1}), true).await });
    u1_in.notified().await;
    let f2 = f.clone();
    let t2 = tokio::spawn(async move { f2.update_with(json!({"v": 2}), true).await });
    u2_in.notified().await;
    rel_u1.notify_one();
    let _ = t1.await.unwrap();
    rel_u2.notify_one();
    let _ = t2.await.unwrap();

    // Replay is now running (idx == 2) and waiting on `rel_replay`.
    replay_in.notified().await;
    // Two quick overlapping updates re-arm `replay_owed = true` while replay is still running.
    let f3 = f.clone();
    let f4 = f.clone();
    let _ = tokio::join!(
        f3.update_with(json!({"v": 3}), true),
        f4.update_with(json!({"v": 4}), true)
    );
    // Even if only one ran or both ran, explicitly ensure a slower one finished < live_rev by
    // letting replay panic after new update bumped `update_rev`.
    rel_replay.notify_one();

    tokio::time::timeout(Duration::from_secs(2), f.when_updates_settled())
        .await
        .expect("run_replay panic 后 when_updates_settled 不得永久挂死");
    app2.close().await;
}

#[tokio::test]
async fn r4_p1_mount_on_dropped_parent_ctx_fails_closed_no_orphan_fiber() {
    let app = App::new();
    let parent = app.root.plugin(
        plugin_fn("r4-parent", Inject::default(), |_, _| {
            Box::pin(async { Ok(()) })
        }),
        Config::Null,
    );
    parent.settle().await;
    let parent_ctx = parent.ctx();
    parent.dispose().await;
    drop(parent);
    assert!(
        parent_ctx.fiber_opt().is_none(),
        "前提：父 fiber 释放后 parent_ctx.fiber_opt() 应为 None"
    );

    let applied = Arc::new(AtomicBool::new(false));
    let applied_in = applied.clone();
    let child = app.store.plugin(
        &parent_ctx,
        plugin_fn("r4-orphan-child", Inject::default(), move |_, _| {
            let applied_in = applied_in.clone();
            Box::pin(async move {
                applied_in.store(true, Ordering::SeqCst);
                Ok(())
            })
        }),
        Config::Null,
        Inject::default(),
    );
    child.settle().await;
    assert_eq!(
        child.state(),
        FiberState::Failed,
        "已释放父 ctx 上挂载子插件必须 fail-closed 为 Failed"
    );
    assert!(child.error().is_some());
    assert!(
        !applied.load(Ordering::SeqCst),
        "孤儿子插件不得执行 apply()"
    );
    assert_eq!(
        app.store.runtime_count(),
        0,
        "失败的孤儿子插件不得滞留在 Store::runtimes 中"
    );
    app.close().await;
}

#[tokio::test]
async fn r4_p1_delete_plugin_does_not_drop_user_plugin_under_runtimes_lock() {
    use cordis_core::{Plugin, PluginKey, Store};

    struct ReentrantDropPlugin {
        store: Arc<Store>,
    }
    impl Plugin for ReentrantDropPlugin {
        fn name(&self) -> &str {
            "r4-drop-reentrant"
        }
        fn key(&self) -> PluginKey {
            PluginKey::from("r4-drop-reentrant")
        }
        fn apply(
            &self,
            _ctx: Ctx,
            _config: Config,
        ) -> cordis_core::BoxFuture<cordis_core::Result<()>> {
            Box::pin(async { Ok(()) })
        }
    }
    impl Drop for ReentrantDropPlugin {
        fn drop(&mut self) {
            // Re-entering `store.runtime_count()` inside `Plugin::drop` must not deadlock!
            let _ = self.store.runtime_count();
        }
    }

    let app = App::new();
    let p: Arc<dyn Plugin> = Arc::new(ReentrantDropPlugin {
        store: app.store.clone(),
    });
    let f = app.root.plugin(p, Config::Null);
    f.settle().await;
    f.dispose().await;
    drop(f);
    // Re-register a plugin whose runtime has 0 fibers? Instead mount + fail on disposed parent or delete while empty:
    // Actually trigger delete_plugin while runtime is alive with 0 fibers isn't reachable via normal drop,
    // so stress concurrent `plugin` + `delete_plugin` to verify no retired runtime resurrection & no deadlock!
    let store = app.store.clone();
    let root = app.root.clone();
    let mut handles = Vec::new();
    for _ in 0..8 {
        let store = store.clone();
        let root = root.clone();
        handles.push(tokio::spawn(async move {
            for _ in 0..25 {
                let p: Arc<dyn Plugin> = Arc::new(ReentrantDropPlugin {
                    store: store.clone(),
                });
                let f = root.plugin(p, Config::Null);
                tokio::task::yield_now().await;
                let victims = store.delete_plugin("r4-drop-reentrant");
                for v in victims {
                    v.dispose().await;
                }
                f.dispose().await;
            }
        }));
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        for h in handles {
            h.await.unwrap();
        }
    })
    .await
    .expect("并发 plugin + delete_plugin 不得死锁");
    assert_eq!(app.store.runtime_count(), 0);
    app.close().await;
}

#[tokio::test]
async fn r4_p1_parallel_handler_from_scoped_task_can_dispose_without_deadlock() {
    use cordis_core::Fiber;
    let app = App::new();
    let seen_cid = Arc::new(Mutex::new(None::<String>));
    let seen_cid_in = seen_cid.clone();
    let target_fiber: Arc<Mutex<Option<Arc<Fiber>>>> = Arc::new(Mutex::new(None));
    let target_fiber_in = target_fiber.clone();

    let _h = app.root.on("r4-par-dispose", move |_call| {
        let seen_cid_in = seen_cid_in.clone();
        let target_fiber_in = target_fiber_in.clone();
        Box::pin(async move {
            *seen_cid_in.lock().unwrap() =
                cordis_core::current_correlation_id().map(|s| s.to_string());
            let f = target_fiber_in.lock().unwrap().clone();
            if let Some(f) = f {
                f.dispose().await;
            }
            None
        })
    });

    let done = Arc::new(tokio::sync::Notify::new());
    let done_in = done.clone();
    let f = app.root.plugin(
        plugin_fn("r4-par-owner", Inject::default(), move |ctx, _| {
            let done_in = done_in.clone();
            Box::pin(async move {
                let ctx_task = ctx.clone();
                ctx.spawn_scoped(move |_cancel| async move {
                    cordis_core::scope_correlation(
                        "cid-par-42",
                        ctx_task.parallel("r4-par-dispose", vec![]),
                    )
                    .await;
                    done_in.notify_one();
                })
                .unwrap();
                Ok(())
            })
        }),
        Config::Null,
    );
    *target_fiber.lock().unwrap() = Some(f.clone());
    f.settle().await;

    tokio::time::timeout(Duration::from_secs(2), done.notified())
        .await
        .expect("spawn_scoped 内 Mode::Parallel 回调 dispose 不得三方死锁");
    f.dispose().await;
    assert_eq!(
        seen_cid.lock().unwrap().as_deref(),
        Some("cid-par-42"),
        "Mode::Parallel 必须向并行处理器透传 correlation_id"
    );
    app.close().await;
}

#[tokio::test]
async fn r4_p1_reentrant_lifecycle_calls_in_apply_and_disposer_do_not_deadlock() {
    // Case A: plugin calls `ctx.fiber().dispose().await` directly inside `apply()`.
    let app = App::new();
    let f = app.root.plugin(
        plugin_fn("r4-self-dispose-apply", Inject::default(), |ctx, _| {
            Box::pin(async move {
                ctx.fiber().dispose().await;
                Ok(())
            })
        }),
        Config::Null,
    );
    tokio::time::timeout(Duration::from_secs(2), f.settle())
        .await
        .expect("apply() 内重入 fiber.dispose().await 不得在 Fiber::driver 上死锁");
    tokio::time::timeout(Duration::from_secs(2), f.dispose())
        .await
        .expect("apply() 自处置后的 fiber.dispose() 必须收敛");
    app.close().await;

    // Case B: effect disposer calls `fiber.dispose().await` and `parent.dispose().await`.
    let app2 = App::new();
    let f2 = app2.root.plugin(
        plugin_fn("r4-self-dispose-effect", Inject::default(), |ctx, _| {
            Box::pin(async move {
                let my_fiber = ctx.fiber();
                let _ = ctx.effect(move |_| {
                    let my_fiber = my_fiber.clone();
                    Disposable::new(move || {
                        Box::pin(async move {
                            my_fiber.dispose().await;
                        })
                    })
                });
                Ok(())
            })
        }),
        Config::Null,
    );
    f2.settle().await;
    tokio::time::timeout(Duration::from_secs(2), f2.dispose())
        .await
        .expect("disposer 内重入 fiber.dispose().await 不得死锁");
    app2.close().await;
}

#[tokio::test]
async fn r4_p1_panic_in_validate_or_volatile_paths_does_not_wedge_loading() {
    use cordis_core::{Plugin, PluginKey};

    struct PanicValidatePlugin;
    impl Plugin for PanicValidatePlugin {
        fn name(&self) -> &str {
            "r4-panic-validate"
        }
        fn key(&self) -> PluginKey {
            PluginKey::from("r4-panic-validate")
        }
        fn validate(
            &self,
            _config: Config,
        ) -> std::result::Result<Config, cordis_core::ValidationError> {
            panic!("boom in validate");
        }
        fn apply(
            &self,
            _ctx: Ctx,
            _config: Config,
        ) -> cordis_core::BoxFuture<cordis_core::Result<()>> {
            Box::pin(async { Ok(()) })
        }
    }

    struct PanicVolatilePlugin;
    impl Plugin for PanicVolatilePlugin {
        fn name(&self) -> &str {
            "r4-panic-volatile"
        }
        fn key(&self) -> PluginKey {
            PluginKey::from("r4-panic-volatile")
        }
        fn volatile_paths(&self) -> Vec<String> {
            panic!("boom in volatile_paths");
        }
        fn apply(
            &self,
            _ctx: Ctx,
            _config: Config,
        ) -> cordis_core::BoxFuture<cordis_core::Result<()>> {
            Box::pin(async { Ok(()) })
        }
    }

    let app = App::new();
    let f1 = app.root.plugin(Arc::new(PanicValidatePlugin), Config::Null);
    f1.settle().await;
    assert_eq!(
        f1.state(),
        FiberState::Failed,
        "validate() panic 必须收敛到 Failed，不得卡死在 Loading"
    );
    assert!(f1.error().is_some());

    let f2 = app.root.plugin(Arc::new(PanicVolatilePlugin), Config::Null);
    f2.settle().await;
    assert_eq!(
        f2.state(),
        FiberState::Failed,
        "volatile_paths() panic 必须收敛到 Failed，不得卡死在 Loading"
    );
    assert!(f2.error().is_some());
    app.close().await;
}

#[tokio::test]
async fn r4_p1_concurrent_load_and_update_with_never_splits_volatile_or_bypasses_veto() {
    use cordis_core::{Plugin, PluginKey, VolatileRef};
    use tokio::sync::Notify;
    const INTERNAL_UPDATE: &str = "internal/update";

    struct VolPlugin {
        latest_ref: Arc<Mutex<Option<Arc<VolatileRef>>>>,
        veto_ready: Arc<Notify>,
        release_veto: Arc<Notify>,
    }
    impl Plugin for VolPlugin {
        fn name(&self) -> &str {
            "r4-vol-race"
        }
        fn key(&self) -> PluginKey {
            PluginKey::from("r4-vol-race")
        }
        fn volatile_paths(&self) -> Vec<String> {
            vec!["dir".to_string()]
        }
        fn apply(
            &self,
            ctx: Ctx,
            _config: Config,
        ) -> cordis_core::BoxFuture<cordis_core::Result<()>> {
            let latest_ref = self.latest_ref.clone();
            let veto_ready = self.veto_ready.clone();
            let release_veto = self.release_veto.clone();
            Box::pin(async move {
                let v = ctx.volatile("dir").expect("volatile dir");
                *latest_ref.lock().unwrap() = Some(v);
                let _ = ctx.on(INTERNAL_UPDATE, move |_call| {
                    let veto_ready = veto_ready.clone();
                    let release_veto = release_veto.clone();
                    Box::pin(async move {
                        veto_ready.notify_one();
                        release_veto.notified().await;
                        None // veto!
                    })
                });
                Ok(())
            })
        }
    }

    let app = App::new();
    let latest_ref = Arc::new(Mutex::new(None::<Arc<VolatileRef>>));
    let u_in_cfg = Arc::new(Notify::new());
    let release_u_cfg = Arc::new(Notify::new());
    let load_in_cfg = Arc::new(Notify::new());
    let release_load_cfg = Arc::new(Notify::new());
    let veto_ready = Arc::new(Notify::new());
    let release_veto = Arc::new(Notify::new());
    let cfg_phase = Arc::new(AtomicUsize::new(0));

    let _cfg_hook = app.root.on("internal/config", {
        let u_in_cfg = u_in_cfg.clone();
        let release_u_cfg = release_u_cfg.clone();
        let load_in_cfg = load_in_cfg.clone();
        let release_load_cfg = release_load_cfg.clone();
        let cfg_phase = cfg_phase.clone();
        move |call| {
            let phase = cfg_phase.fetch_add(1, Ordering::SeqCst);
            let u_in_cfg = u_in_cfg.clone();
            let release_u_cfg = release_u_cfg.clone();
            let load_in_cfg = load_in_cfg.clone();
            let release_load_cfg = release_load_cfg.clone();
            Box::pin(async move {
                // phase 0: initial load (pass through)
                // phase 1: update_with(C1) resolve_config
                // phase 2: restart() load() resolve_config
                if phase == 1 {
                    u_in_cfg.notify_one();
                    release_u_cfg.notified().await;
                } else if phase == 2 {
                    load_in_cfg.notify_one();
                    release_load_cfg.notified().await;
                }
                call.next.call(call.args).await
            })
        }
    });

    let f = app.root.plugin(
        Arc::new(VolPlugin {
            latest_ref: latest_ref.clone(),
            veto_ready: veto_ready.clone(),
            release_veto: release_veto.clone(),
        }),
        json!({"dir": "v0"}),
    );
    f.settle().await;
    assert_eq!(f.state(), FiberState::Active);

    // 1. Start update_with(C1) while Active; let it commit and pause inside the veto hook.
    release_u_cfg.notify_one();
    let f_u = f.clone();
    let u_task = tokio::spawn(async move { f_u.update_with(json!({"dir": "v1"}), true).await });
    u_in_cfg.notified().await;
    veto_ready.notified().await;

    // 2. While C1 is in-flight in INTERNAL_UPDATE, restart() unloads and runs load() -> V_live.
    let f_r = f.clone();
    let r_task = tokio::spawn(async move { f_r.restart().await });
    load_in_cfg.notified().await;
    release_load_cfg.notify_one();
    r_task.await.unwrap().unwrap();

    // 3. Now veto C1! Rollback MUST restore V_live (not dead V0).
    release_veto.notify_one();
    let _ = u_task.await.unwrap();

    // Subsequent volatile commit MUST reach the active plugin's `latest_ref`!
    f.commit_volatile(json!({"dir": "v2"}));
    let observed = latest_ref
        .lock()
        .unwrap()
        .as_ref()
        .expect("live VolatileRef")
        .get();
    assert_eq!(
        observed,
        json!("v2"),
        "并发 load + 否决 update 后，存活插件持有的 VolatileRef 不得与 fiber 撕裂"
    );
    app.close().await;
}

#[tokio::test]
async fn r5_p1_fail_closed_fiber_cannot_be_resurrected_by_update_and_sync_filter_panic_is_caught() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    // 1. Fail-closed fiber on disposed parent MUST be terminal (UID_DISPOSED) so `update()`
    //    cannot resurrect it into an untracked Active zombie escaping `App::close()`.
    let app = App::new();
    let parent = app.root.plugin(
        plugin_fn("r5-parent", Inject::default(), |_ctx, _| {
            Box::pin(async { Ok(()) })
        }),
        Config::Null,
    );
    parent.settle().await;
    let parent_ctx = parent.ctx();
    parent.dispose().await;
    assert_eq!(parent.state(), FiberState::Disposed);

    let applies = Arc::new(AtomicUsize::new(0));
    let applies_c = applies.clone();
    let rejected = app.store.plugin(
        &parent_ctx,
        plugin_fn("r5-zombie-candidate", Inject::default(), move |_ctx, _| {
            let applies_c = applies_c.clone();
            Box::pin(async move {
                applies_c.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        }),
        Config::Null,
        Inject::default(),
    );
    assert_eq!(rejected.state(), FiberState::Failed);
    assert!(
        rejected.is_disposed(),
        "fail-closed 拒绝态 fiber 必须已置 UID_DISPOSED 终态"
    );
    let update_res = rejected.update(json!({"try": "resurrect"})).await;
    assert!(
        update_res.is_err(),
        "fail-closed 拒绝态 fiber 不得被 update() 复活: {update_res:?}"
    );
    assert_eq!(rejected.state(), FiberState::Failed);
    assert_eq!(
        applies.load(Ordering::SeqCst),
        0,
        "fail-closed 拒绝态 fiber 的 apply() 绝对不能执行"
    );
    app.close().await;

    // 2. Synchronous panic inside a `ScopePredicate` (`ctx.filter(...)`) is caught inside
    //    `hook_scope` (never crashing `resolve_config` or stranding the fiber in `Loading`),
    //    and a panic inside an `internal/config` handler itself transitions cleanly to `Failed`.
    let app2 = App::new();
    let filtered = app2
        .root
        .filter(Arc::new(|_| panic!("boom in sync scope predicate")));
    let _hook = filtered.on("internal/config", |call| {
        Box::pin(async move { call.next.call(call.args).await })
    });
    let f = app2.root.plugin(
        plugin_fn("r5-sync-filter-panic", Inject::default(), |_ctx, _| {
            Box::pin(async { Ok(()) })
        }),
        json!({"ok": true}),
    );
    f.settle().await;
    assert_eq!(
        f.state(),
        FiberState::Active,
        "单个坏 ScopePredicate panic 不得导致无关 fiber 崩溃或滞留 Loading"
    );

    let _bad_cfg = app2.root.on("internal/config", |_call| {
        Box::pin(async move {
            panic!("boom inside internal/config handler");
        })
    });
    let f2 = app2.root.plugin(
        plugin_fn("r5-cfg-handler-panic", Inject::default(), |_ctx, _| {
            Box::pin(async { Ok(()) })
        }),
        json!({"ok": true}),
    );
    f2.settle().await;
    assert_eq!(
        f2.state(),
        FiberState::Failed,
        "internal/config handler panic 必须收敛到 Failed 而非滞留 Loading"
    );
    assert!(f2.error().is_some());
    app2.close().await;
}
