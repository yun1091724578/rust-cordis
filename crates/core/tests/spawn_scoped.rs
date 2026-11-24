//! 受管后台任务（`Ctx::spawn_scoped` / `Fiber::spawn_scoped`）：
//! 生命周期归属 fiber、卸载取消并等待、自然结束自动摘除、ADR 0011 correlation 透传。

use cordis_core::{
    current_correlation_id, plugin_fn, scope_correlation, App, Config, Ctx, FiberState, Inject,
    ReloadDeferred, SpawnScopedError, UpdateOutcome,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{oneshot, Notify};

#[tokio::test]
async fn spawn_scoped_cancels_and_waits_for_tasks_on_fiber_dispose() {
    let app = App::new();
    let started = Arc::new(Notify::new());
    let cleaned_up = Arc::new(AtomicBool::new(false));

    let started_in = started.clone();
    let cleaned_in = cleaned_up.clone();
    let fiber = app
        .plugin(
            plugin_fn("worker", Inject::default(), move |ctx: Ctx, _| {
                let started = started_in.clone();
                let cleaned = cleaned_in.clone();
                Box::pin(async move {
                    ctx.spawn_scoped(move |cancel| async move {
                        started.notify_one();
                        cancel.cancelled().await;
                        // 模拟协作式清理耗时：dispose 必须等到此处结束再返回。
                        tokio::time::sleep(Duration::from_millis(30)).await;
                        cleaned.store(true, Ordering::SeqCst);
                    })?;
                    Ok(())
                })
            }),
            Config::Null,
        )
        .await
        .expect("插件挂载成功");

    started.notified().await;
    assert_eq!(fiber.scoped_task_count(), 1);

    tokio::time::timeout(Duration::from_secs(2), fiber.dispose())
        .await
        .expect("fiber.dispose 不得挂起");

    assert!(
        cleaned_up.load(Ordering::SeqCst),
        "fiber.dispose 返回前必须已取消并等待受管任务完成清理"
    );
    assert_eq!(fiber.scoped_task_count(), 0);
    app.close().await;
}

#[tokio::test]
async fn spawn_scoped_completing_tasks_do_not_accumulate() {
    let app = App::new();
    let fiber = app.root_fiber();

    let mut receivers = Vec::new();
    for _ in 0..50 {
        let (tx, rx) = oneshot::channel();
        app.root
            .spawn_scoped(move |_cancel| async move {
                tokio::task::yield_now().await;
                let _ = tx.send(());
            })
            .expect("spawn_scoped 应成功");
        receivers.push(rx);
    }
    for rx in receivers {
        rx.await.expect("任务必须正常完成");
    }

    // 等待 Drop 摘除槽位（同一任务末尾 `NativeTaskDone` 析构）。
    for _ in 0..50 {
        if fiber.scoped_task_count() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(
        fiber.scoped_task_count(),
        0,
        "自然结束的任务必须自动摘除，不得在长期存活 fiber 上线性累积"
    );
    app.close().await;
}

#[tokio::test]
async fn spawn_scoped_panicking_task_removes_slot_and_does_not_block_dispose() {
    let app = App::new();
    let fiber = app.root_fiber();

    app.root
        .spawn_scoped(|_cancel| async move {
            panic!("boom inside scoped task");
        })
        .expect("spawn_scoped 应成功");

    for _ in 0..100 {
        if fiber.scoped_task_count() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(
        fiber.scoped_task_count(),
        0,
        "panic 的任务也必须自动摘除槽位"
    );

    tokio::time::timeout(Duration::from_secs(2), app.close())
        .await
        .expect("App::close 不得被已 panic 的任务阻塞");
}

#[tokio::test]
async fn spawn_scoped_failed_apply_cancels_and_waits_for_tasks() {
    let app = App::new();
    let started = Arc::new(Notify::new());
    let cleaned_up = Arc::new(AtomicBool::new(false));
    let captured_ctx: Arc<Mutex<Option<Ctx>>> = Arc::new(Mutex::new(None));

    let started_in = started.clone();
    let cleaned_in = cleaned_up.clone();
    let ctx_slot = captured_ctx.clone();
    let fiber = app.root.plugin(
        plugin_fn("failing-worker", Inject::default(), move |ctx: Ctx, _| {
            let started = started_in.clone();
            let cleaned = cleaned_in.clone();
            let ctx_slot = ctx_slot.clone();
            Box::pin(async move {
                *ctx_slot.lock().unwrap() = Some(ctx.clone());
                let started_task = started.clone();
                ctx.spawn_scoped(move |cancel| async move {
                    started_task.notify_one();
                    cancel.cancelled().await;
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    cleaned.store(true, Ordering::SeqCst);
                })?;
                started.notified().await;
                Err("apply failed intentionally".into())
            })
        }),
        Config::Null,
    );

    fiber.settle().await;
    assert_eq!(fiber.state(), FiberState::Failed);
    assert!(
        cleaned_up.load(Ordering::SeqCst),
        "apply 失败回收必须取消并等待已启动的受管任务"
    );
    assert_eq!(fiber.scoped_task_count(), 0);

    let failed_ctx = captured_ctx.lock().unwrap().clone().expect("已捕获 ctx");
    assert_eq!(
        failed_ctx.spawn_scoped(|_| async {}).unwrap_err(),
        SpawnScopedError::InactiveFiber,
        "已失败 fiber 的闸门关闭，不得再接受新任务"
    );
    app.close().await;
}

#[tokio::test]
async fn spawn_scoped_rejects_disposed_context() {
    let app = App::new();
    let captured_ctx: Arc<Mutex<Option<Ctx>>> = Arc::new(Mutex::new(None));
    let ctx_slot = captured_ctx.clone();
    let fiber = app
        .plugin(
            plugin_fn("ephemeral", Inject::default(), move |ctx: Ctx, _| {
                let ctx_slot = ctx_slot.clone();
                Box::pin(async move {
                    *ctx_slot.lock().unwrap() = Some(ctx);
                    Ok(())
                })
            }),
            Config::Null,
        )
        .await
        .unwrap();

    let ctx = captured_ctx.lock().unwrap().clone().unwrap();
    fiber.dispose().await;

    assert_eq!(
        ctx.spawn_scoped(|_| async {}).unwrap_err(),
        SpawnScopedError::InactiveFiber
    );
    app.close().await;
}

#[test]
fn spawn_scoped_outside_tokio_runtime_returns_no_runtime() {
    let app = App::new();
    let err = app
        .root
        .spawn_scoped(|_| async {})
        .expect_err("无 Tokio 运行时必须返回错误而不是 panic");
    assert_eq!(err, SpawnScopedError::NoRuntime);
    assert!(err.to_string().contains("Tokio"), "{err}");
}

#[tokio::test]
async fn spawn_scoped_inherits_correlation_id_from_ctx_and_scope() {
    let app = App::new();

    // 1. 来自 Ctx::with_correlation
    let (tx1, rx1) = oneshot::channel();
    let sess_ctx = app.root.with_correlation("sess-from-ctx");
    sess_ctx
        .spawn_scoped(move |_| async move {
            let before = current_correlation_id().map(|s| s.to_string());
            tokio::task::yield_now().await;
            let after = current_correlation_id().map(|s| s.to_string());
            let _ = tx1.send((before, after));
        })
        .expect("spawn_scoped 应成功");
    let (before, after) = rx1.await.unwrap();
    assert_eq!(before.as_deref(), Some("sess-from-ctx"));
    assert_eq!(after.as_deref(), Some("sess-from-ctx"));

    // 2. 来自当前宿主调用作用域 scope_correlation
    let (tx2, rx2) = oneshot::channel();
    scope_correlation("sess-from-scope", async {
        app.root
            .spawn_scoped(move |_| async move {
                tokio::task::yield_now().await;
                let seen = current_correlation_id().map(|s| s.to_string());
                let _ = tx2.send(seen);
            })
            .expect("spawn_scoped 应成功");
    })
    .await;
    assert_eq!(rx2.await.unwrap().as_deref(), Some("sess-from-scope"));

    // 3. Ctx 自身绑定的 correlation_id 优先于外层作用域
    let (tx3, rx3) = oneshot::channel();
    scope_correlation("outer-scope", async {
        sess_ctx
            .spawn_scoped(move |_| async move {
                let seen = current_correlation_id().map(|s| s.to_string());
                let _ = tx3.send(seen);
            })
            .expect("spawn_scoped 应成功");
    })
    .await;
    assert_eq!(rx3.await.unwrap().as_deref(), Some("sess-from-ctx"));

    app.close().await;
}

#[tokio::test]
async fn spawn_scoped_returned_token_cancels_single_task_without_affecting_siblings() {
    let app = App::new();
    let (tx_a, rx_a) = oneshot::channel();
    let (tx_b_ready, rx_b_ready) = oneshot::channel();
    let b_cancelled = Arc::new(AtomicBool::new(false));
    let b_cancelled_in = b_cancelled.clone();

    let token_a = app
        .root
        .spawn_scoped(move |cancel| async move {
            cancel.cancelled().await;
            let _ = tx_a.send(true);
        })
        .unwrap();

    let _token_b = app
        .root
        .spawn_scoped(move |cancel| async move {
            let _ = tx_b_ready.send(());
            cancel.cancelled().await;
            b_cancelled_in.store(true, Ordering::SeqCst);
        })
        .unwrap();

    rx_b_ready.await.unwrap();
    token_a.cancel();
    assert!(rx_a.await.unwrap(), "任务 A 必须收到单独取消");
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(
        !b_cancelled.load(Ordering::SeqCst),
        "取消任务 A 不得波及同 fiber 下的兄弟任务 B"
    );

    app.close().await;
    assert!(
        b_cancelled.load(Ordering::SeqCst),
        "App::close 时兄弟任务 B 仍会被统一取消并等待"
    );
}

#[tokio::test]
async fn spawn_scoped_restart_cancels_old_tasks_and_allows_new_generation() {
    use std::sync::atomic::AtomicUsize;

    let app = App::new();
    let generation = Arc::new(AtomicUsize::new(0));
    let gen1_started = Arc::new(Notify::new());
    let gen1_cleaned = Arc::new(AtomicBool::new(false));
    let gen2_started = Arc::new(Notify::new());
    let gen2_cleaned = Arc::new(AtomicBool::new(false));

    let g = generation.clone();
    let g1_s = gen1_started.clone();
    let g1_c = gen1_cleaned.clone();
    let g2_s = gen2_started.clone();
    let g2_c = gen2_cleaned.clone();

    let fiber = app
        .plugin(
            plugin_fn("reloadable", Inject::default(), move |ctx: Ctx, _| {
                let gen_no = g.fetch_add(1, Ordering::SeqCst) + 1;
                let (started, cleaned) = if gen_no == 1 {
                    (g1_s.clone(), g1_c.clone())
                } else {
                    (g2_s.clone(), g2_c.clone())
                };
                Box::pin(async move {
                    ctx.spawn_scoped(move |cancel| async move {
                        started.notify_one();
                        cancel.cancelled().await;
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        cleaned.store(true, Ordering::SeqCst);
                    })?;
                    Ok(())
                })
            }),
            Config::Null,
        )
        .await
        .expect("首次装载必须成功");

    gen1_started.notified().await;
    assert_eq!(fiber.scoped_task_count(), 1);

    // 热重载：必须先取消并等待第一代受管任务，再启动第二代。
    fiber.restart().await.expect("restart 必须成功");
    assert!(
        gen1_cleaned.load(Ordering::SeqCst),
        "restart 返回前必须已取消并等待旧代任务完成清理"
    );
    gen2_started.notified().await;
    assert_eq!(fiber.scoped_task_count(), 1);
    assert!(!gen2_cleaned.load(Ordering::SeqCst));

    app.close().await;
    assert!(gen2_cleaned.load(Ordering::SeqCst));
    assert_eq!(fiber.scoped_task_count(), 0);
}

#[tokio::test]
async fn spawn_scoped_sync_closure_panic_leaves_no_slot_and_unblocks_dispose() {
    let app = App::new();
    let fiber = app.root_fiber();

    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = app.root.spawn_scoped(|_cancel| -> std::future::Ready<()> {
            panic!("sync closure panic before future spawn");
        });
    }));
    assert!(caught.is_err(), "同步闭包 panic 应向上传播");
    assert_eq!(
        fiber.scoped_task_count(),
        0,
        "同步闭包 panic 不得在 native_tasks 留下空槽位"
    );

    // 闸门 in_flight 也必须归零，否则 App::close() 会卡死在 close_effects()。
    tokio::time::timeout(Duration::from_secs(2), app.close())
        .await
        .expect("同步闭包 panic 后 App::close 不得卡死");
}

#[tokio::test]
async fn fiber_spawn_scoped_inherits_ambient_correlation_and_rejects_failed_fiber() {
    let app = App::new();
    let root_fiber = app.root_fiber();

    // 1. Fiber::spawn_scoped 继承当前宿主作用域的 correlation_id，但不受其它派生 Ctx 污染。
    let (tx, rx) = oneshot::channel();
    scope_correlation("ambient-fiber-cid", async {
        root_fiber
            .spawn_scoped(move |_| async move {
                tokio::task::yield_now().await;
                let _ = tx.send(current_correlation_id().map(|s| s.to_string()));
            })
            .expect("Fiber::spawn_scoped 应成功");
    })
    .await;
    assert_eq!(rx.await.unwrap().as_deref(), Some("ambient-fiber-cid"));

    // 2. 无宿主作用域时为 None（即使曾用带 correlation 的派生 Ctx 挂载过插件）。
    let (tx_none, rx_none) = oneshot::channel();
    let correlated_mount_ctx = app.root.with_correlation("mount-only-cid");
    let mounted_fiber = correlated_mount_ctx
        .plugin_and_await(
            plugin_fn("clean-svc", Inject::default(), |_ctx, _| {
                Box::pin(async move { Ok(()) })
            }),
            Config::Null,
        )
        .await
        .unwrap();
    mounted_fiber
        .spawn_scoped(move |_| async move {
            let _ = tx_none.send(current_correlation_id().map(|s| s.to_string()));
        })
        .expect("Fiber::spawn_scoped 应成功");
    assert_eq!(
        rx_none.await.unwrap(),
        None,
        "Fiber::spawn_scoped 不应泄漏挂载期 Ctx 的 correlation_id"
    );

    // 3. Failed 状态的 fiber 拒绝新任务。
    let failed_fiber = app.root.plugin(
        plugin_fn("always-fails", Inject::default(), |_ctx, _| {
            Box::pin(async move { Err("fail".into()) })
        }),
        Config::Null,
    );
    failed_fiber.settle().await;
    assert_eq!(failed_fiber.state(), FiberState::Failed);
    assert_eq!(
        failed_fiber.spawn_scoped(|_| async {}).unwrap_err(),
        SpawnScopedError::InactiveFiber
    );

    app.close().await;
}

#[tokio::test]
async fn spawn_scoped_self_dispose_initiates_shutdown_and_exits_without_deadlock() {
    let app = App::new();
    let disposer_ran = Arc::new(AtomicBool::new(false));
    let disposer_ran_in = disposer_ran.clone();
    let (task_returned_tx, task_returned_rx) = oneshot::channel();
    let task_returned_tx = Arc::new(Mutex::new(Some(task_returned_tx)));
    let task_returned_in = task_returned_tx.clone();

    let fiber = app
        .plugin(
            plugin_fn("self-disposer", Inject::default(), move |ctx: Ctx, _| {
                let disposer_ran = disposer_ran_in.clone();
                let task_returned_tx = task_returned_in.clone();
                Box::pin(async move {
                    let _ = ctx.effect(move |_| {
                        cordis_core::Disposable::sync(move || {
                            disposer_ran.store(true, Ordering::SeqCst);
                        })
                    });
                    let owner = ctx.fiber();
                    ctx.spawn_scoped(move |_cancel| async move {
                        owner.dispose().await;
                        if let Some(tx) = task_returned_tx.lock().unwrap().take() {
                            let _ = tx.send(true);
                        }
                    })?;
                    Ok(())
                })
            }),
            Config::Null,
        )
        .await
        .expect("插件挂载成功");

    assert!(
        tokio::time::timeout(Duration::from_secs(2), task_returned_rx)
            .await
            .expect("受管任务内调用 fiber.dispose().await 不得自锁")
            .unwrap(),
        "受管任务应正常返回并退出"
    );

    // 外部再次 await dispose()，等待后台清理任务彻底完成。
    tokio::time::timeout(Duration::from_secs(2), fiber.dispose())
        .await
        .expect("后台清理必须完成");

    assert_eq!(fiber.state(), FiberState::Disposed);
    assert!(
        disposer_ran.load(Ordering::SeqCst),
        "fiber 的 effect 必须被释放"
    );
    assert_eq!(fiber.scoped_task_count(), 0);
    app.close().await;
}

#[tokio::test]
async fn spawn_scoped_reentrant_provider_ancestor_and_restart_do_not_deadlock() {
    use std::sync::atomic::AtomicUsize;

    // 1. 消费者受管任务调用提供者 fiber.dispose().await 不得自锁。
    {
        let app = App::new();
        let provider = app
            .plugin(
                plugin_fn("prov", Inject::default(), |ctx: Ctx, _| {
                    Box::pin(async move {
                        let _ = ctx.provide("svc", cordis_core::Dyn::new(1u32))?;
                        Ok(())
                    })
                }),
                Config::Null,
            )
            .await
            .unwrap();

        let (done_tx, done_rx) = oneshot::channel();
        let done_tx = Arc::new(Mutex::new(Some(done_tx)));
        let done_tx_in = done_tx.clone();
        let provider_in = provider.clone();
        let _consumer = app
            .plugin(
                plugin_fn("cons", Inject::of(&["svc"]), move |ctx: Ctx, _| {
                    let done_tx = done_tx_in.clone();
                    let provider = provider_in.clone();
                    Box::pin(async move {
                        ctx.spawn_scoped(move |_cancel| async move {
                            provider.dispose().await;
                            if let Some(tx) = done_tx.lock().unwrap().take() {
                                let _ = tx.send(());
                            }
                        })?;
                        Ok(())
                    })
                }),
                Config::Null,
            )
            .await
            .unwrap();

        tokio::time::timeout(Duration::from_secs(2), done_rx)
            .await
            .expect("消费者受管任务内 dispose 提供者不得自锁")
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), provider.dispose())
            .await
            .expect("提供者清理必须完成");
        app.close().await;
    }

    // 2. 子 fiber 受管任务调用祖先 root_fiber.dispose().await 不得自锁。
    {
        let app = App::new();
        let root = app.root_fiber();
        let (done_tx, done_rx) = oneshot::channel();
        let done_tx = Arc::new(Mutex::new(Some(done_tx)));
        let done_tx_in = done_tx.clone();
        let root_in = root.clone();
        let _child = app
            .plugin(
                plugin_fn("child", Inject::default(), move |ctx: Ctx, _| {
                    let done_tx = done_tx_in.clone();
                    let root = root_in.clone();
                    Box::pin(async move {
                        ctx.spawn_scoped(move |_cancel| async move {
                            root.dispose().await;
                            if let Some(tx) = done_tx.lock().unwrap().take() {
                                let _ = tx.send(());
                            }
                        })?;
                        Ok(())
                    })
                }),
                Config::Null,
            )
            .await
            .unwrap();

        tokio::time::timeout(Duration::from_secs(2), done_rx)
            .await
            .expect("子 fiber 受管任务内 dispose 祖先不得自锁")
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), app.close())
            .await
            .expect("App::close 必须收敛");
    }

    // 3. 受管任务内调用所属 fiber.restart().await 不得自锁，必须显式返回 ReloadDeferred 且后台完成重载。
    {
        let app = App::new();
        let gen = Arc::new(AtomicUsize::new(0));
        let gen_in = gen.clone();
        let second_gen_ready = Arc::new(Notify::new());
        let second_gen_in = second_gen_ready.clone();
        let (restart_res_tx, restart_res_rx) = oneshot::channel();
        let restart_res_tx = Arc::new(Mutex::new(Some(restart_res_tx)));
        let restart_res_in = restart_res_tx.clone();
        let fiber = app
            .plugin(
                plugin_fn("self-restarter", Inject::default(), move |ctx: Ctx, _| {
                    let gen = gen_in.clone();
                    let second_gen_ready = second_gen_in.clone();
                    let restart_res_tx = restart_res_in.clone();
                    Box::pin(async move {
                        let current = gen.fetch_add(1, Ordering::SeqCst) + 1;
                        if current == 1 {
                            let owner = ctx.fiber();
                            ctx.spawn_scoped(move |_cancel| async move {
                                let res = owner.restart().await;
                                if let Some(tx) = restart_res_tx.lock().unwrap().take() {
                                    let _ = tx.send(res);
                                }
                            })?;
                        } else {
                            second_gen_ready.notify_one();
                        }
                        Ok(())
                    })
                }),
                Config::Null,
            )
            .await
            .unwrap();

        let err = tokio::time::timeout(Duration::from_secs(2), restart_res_rx)
            .await
            .expect("受管任务重入 restart 不得自锁")
            .unwrap()
            .expect_err("受管任务重入 restart 不得在重载完成前返回 Ok(())");
        let deferred = err
            .downcast_ref::<ReloadDeferred>()
            .expect("重入 restart 应显式返回 ReloadDeferred");
        assert!(deferred.is_scheduled(), "Tokio 运行时内后台收敛应已调度");
        assert_eq!(deferred.fiber_name(), "self-restarter");

        tokio::time::timeout(Duration::from_secs(2), second_gen_ready.notified())
            .await
            .expect("受管任务触发 restart 后新一代必须装载完成");
        fiber.settle().await;
        assert_eq!(gen.load(Ordering::SeqCst), 2);
        assert_eq!(fiber.state(), FiberState::Active);
        app.close().await;
    }
}

#[tokio::test]
async fn spawn_scoped_reentrant_restart_reports_deferred_when_next_gen_apply_fails() {
    use std::sync::atomic::AtomicUsize;

    // 1. 自身受管任务调用 fiber.restart().await，新一代 apply() 明确失败：
    //    调用方绝不得收到 Ok(())，必须收到 ReloadDeferred，且外部 settle 后看到 Failed。
    {
        let app = App::new();
        let gen = Arc::new(AtomicUsize::new(0));
        let gen_in = gen.clone();
        let (res_tx, res_rx) = oneshot::channel();
        let res_tx = Arc::new(Mutex::new(Some(res_tx)));
        let res_tx_in = res_tx.clone();

        let fiber = app
            .plugin(
                plugin_fn("fail-on-gen2", Inject::default(), move |ctx: Ctx, _| {
                    let gen = gen_in.clone();
                    let res_tx = res_tx_in.clone();
                    Box::pin(async move {
                        let current = gen.fetch_add(1, Ordering::SeqCst) + 1;
                        if current == 1 {
                            let owner = ctx.fiber();
                            ctx.spawn_scoped(move |_cancel| async move {
                                let res = owner.restart().await;
                                if let Some(tx) = res_tx.lock().unwrap().take() {
                                    let _ = tx.send(res);
                                }
                            })?;
                            Ok(())
                        } else {
                            Err("gen2 apply failed intentionally".into())
                        }
                    })
                }),
                Config::Null,
            )
            .await
            .expect("第一代必须装载成功");

        let err = tokio::time::timeout(Duration::from_secs(2), res_rx)
            .await
            .expect("受管任务内调用 restart 不得自锁")
            .unwrap()
            .expect_err("新一代 apply 失败时，重入 restart 绝不得返回 Ok(())");
        let deferred = err
            .downcast_ref::<ReloadDeferred>()
            .expect("重入 restart 必须显式区分请求已受理（ReloadDeferred）与重载已完成");
        assert!(deferred.is_scheduled());
        assert_eq!(deferred.fiber_name(), "fail-on-gen2");

        // 外部 settle 等待后台重载收敛到最终态。
        tokio::time::timeout(Duration::from_secs(2), fiber.settle())
            .await
            .expect("外部 settle 必须收敛");
        assert_eq!(gen.load(Ordering::SeqCst), 2);
        assert_eq!(fiber.state(), FiberState::Failed);
        assert!(
            fiber
                .error()
                .unwrap_or_default()
                .contains("gen2 apply failed intentionally"),
            "实际 error: {:?}",
            fiber.error()
        );
        app.close().await;
    }

    // 2. 消费者受管任务调用提供者 provider.restart().await，提供者新一代 apply() 失败：
    //    同样不得返回 Ok(())。
    {
        let app = App::new();
        let prov_gen = Arc::new(AtomicUsize::new(0));
        let prov_gen_in = prov_gen.clone();
        let provider = app
            .plugin(
                plugin_fn("prov-fail-gen2", Inject::default(), move |ctx: Ctx, _| {
                    let prov_gen = prov_gen_in.clone();
                    Box::pin(async move {
                        let current = prov_gen.fetch_add(1, Ordering::SeqCst) + 1;
                        if current == 1 {
                            let _ = ctx.provide("svc", cordis_core::Dyn::new(1u32))?;
                            Ok(())
                        } else {
                            Err("provider gen2 boom".into())
                        }
                    })
                }),
                Config::Null,
            )
            .await
            .unwrap();

        let (res_tx, res_rx) = oneshot::channel();
        let res_tx = Arc::new(Mutex::new(Some(res_tx)));
        let res_tx_in = res_tx.clone();
        let provider_in = provider.clone();
        let _consumer = app
            .plugin(
                plugin_fn(
                    "cons-restarts-prov",
                    Inject::of(&["svc"]),
                    move |ctx: Ctx, _| {
                        let res_tx = res_tx_in.clone();
                        let provider = provider_in.clone();
                        Box::pin(async move {
                            ctx.spawn_scoped(move |_cancel| async move {
                                let res = provider.restart().await;
                                if let Some(tx) = res_tx.lock().unwrap().take() {
                                    let _ = tx.send(res);
                                }
                            })?;
                            Ok(())
                        })
                    },
                ),
                Config::Null,
            )
            .await
            .unwrap();

        let err = tokio::time::timeout(Duration::from_secs(2), res_rx)
            .await
            .expect("消费者受管任务重入 restart 提供者不得自锁")
            .unwrap()
            .expect_err("提供者新一代失败时，重入 restart 不得返回 Ok(())");
        let deferred = err
            .downcast_ref::<ReloadDeferred>()
            .expect("应返回 ReloadDeferred");
        assert!(deferred.is_scheduled());
        assert_eq!(deferred.fiber_name(), "prov-fail-gen2");

        tokio::time::timeout(Duration::from_secs(2), provider.settle())
            .await
            .expect("外部 settle 必须收敛");
        assert_eq!(provider.state(), FiberState::Failed);
        assert!(provider
            .error()
            .unwrap_or_default()
            .contains("provider gen2 boom"));
        app.close().await;
    }
}

#[tokio::test]
async fn spawn_scoped_reentrant_update_with_reports_deferred_when_next_gen_apply_fails() {
    // 1. 重入 update_with() 触发重载且新一代 apply() 失败：不得返回 Ok(UpdateOutcome::Restarted)。
    {
        let app = App::new();
        let (res_tx, res_rx) = oneshot::channel();
        let res_tx = Arc::new(Mutex::new(Some(res_tx)));
        let res_tx_in = res_tx.clone();

        let fiber = app
            .plugin(
                plugin_fn(
                    "self-updater",
                    Inject::default(),
                    move |ctx: Ctx, cfg: Config| {
                        let res_tx = res_tx_in.clone();
                        Box::pin(async move {
                            if cfg.get("fail").and_then(|v| v.as_bool()) == Some(true) {
                                return Err("updated config apply failed".into());
                            }
                            let owner = ctx.fiber();
                            ctx.spawn_scoped(move |_cancel| async move {
                                let res = owner
                                    .update_with(serde_json::json!({ "fail": true }), false)
                                    .await;
                                if let Some(tx) = res_tx.lock().unwrap().take() {
                                    let _ = tx.send(res);
                                }
                            })?;
                            Ok(())
                        })
                    },
                ),
                serde_json::json!({ "fail": false }),
            )
            .await
            .expect("首次装载必须成功");

        let err = tokio::time::timeout(Duration::from_secs(2), res_rx)
            .await
            .expect("受管任务内调用 update_with 不得自锁")
            .unwrap()
            .expect_err("重入 update_with 在重载完成前绝不得返回 Ok(UpdateOutcome::Restarted)");
        let deferred = err
            .downcast_ref::<ReloadDeferred>()
            .expect("重入 update_with 应显式返回 ReloadDeferred");
        assert!(deferred.is_scheduled());
        assert_eq!(deferred.fiber_name(), "self-updater");

        tokio::time::timeout(Duration::from_secs(2), fiber.settle())
            .await
            .expect("外部 settle 必须收敛");
        assert_eq!(fiber.state(), FiberState::Failed);
        assert!(
            fiber
                .error()
                .unwrap_or_default()
                .contains("updated config apply failed"),
            "实际 error: {:?}",
            fiber.error()
        );
        app.close().await;
    }

    // 2. 重入 update() 同样透传 ReloadDeferred，不误报 Ok(())。
    {
        let app = App::new();
        let (res_tx, res_rx) = oneshot::channel();
        let res_tx = Arc::new(Mutex::new(Some(res_tx)));
        let res_tx_in = res_tx.clone();

        let fiber = app
            .plugin(
                plugin_fn(
                    "self-update-compat",
                    Inject::default(),
                    move |ctx: Ctx, cfg: Config| {
                        let res_tx = res_tx_in.clone();
                        Box::pin(async move {
                            if cfg.get("fail").and_then(|v| v.as_bool()) == Some(true) {
                                return Err("compat update apply failed".into());
                            }
                            let owner = ctx.fiber();
                            ctx.spawn_scoped(move |_cancel| async move {
                                let res = owner.update(serde_json::json!({ "fail": true })).await;
                                if let Some(tx) = res_tx.lock().unwrap().take() {
                                    let _ = tx.send(res);
                                }
                            })?;
                            Ok(())
                        })
                    },
                ),
                serde_json::json!({ "fail": false }),
            )
            .await
            .unwrap();

        let err = tokio::time::timeout(Duration::from_secs(2), res_rx)
            .await
            .unwrap()
            .unwrap()
            .expect_err("重入 update() 不得误报 Ok(())");
        assert!(err.downcast_ref::<ReloadDeferred>().is_some());

        fiber.settle().await;
        assert_eq!(fiber.state(), FiberState::Failed);
        app.close().await;
    }

    // 3. 若 internal/update 钩子否决了更新，无需重载，重入 update_with 仍同步返回 Vetoed。
    {
        let app = App::new();
        let (res_tx, res_rx) = oneshot::channel();
        let res_tx = Arc::new(Mutex::new(Some(res_tx)));
        let res_tx_in = res_tx.clone();

        let fiber = app
            .plugin(
                plugin_fn("veto-in-scoped", Inject::default(), move |ctx: Ctx, _| {
                    let res_tx = res_tx_in.clone();
                    Box::pin(async move {
                        ctx.on("internal/update", |_call| Box::pin(async { None }));
                        let owner = ctx.fiber();
                        ctx.spawn_scoped(move |_cancel| async move {
                            let res = owner
                                .update_with(serde_json::json!({ "v": 2 }), false)
                                .await;
                            if let Some(tx) = res_tx.lock().unwrap().take() {
                                let _ = tx.send(res);
                            }
                        })?;
                        Ok(())
                    })
                }),
                serde_json::json!({ "v": 1 }),
            )
            .await
            .unwrap();

        let outcome = tokio::time::timeout(Duration::from_secs(2), res_rx)
            .await
            .unwrap()
            .unwrap()
            .expect("被钩子否决的更新不触发重载，应同步返回 Vetoed");
        assert_eq!(outcome, UpdateOutcome::Vetoed);
        assert_eq!(fiber.state(), FiberState::Active);
        app.close().await;
    }
}

#[tokio::test]
async fn spawn_scoped_unpolled_drop_releases_resources_before_signaling_close() {
    struct BlockingResource {
        entered: std::sync::mpsc::Sender<()>,
        release: Arc<Mutex<Option<std::sync::mpsc::Receiver<()>>>>,
        dropped: Arc<AtomicBool>,
    }
    impl Drop for BlockingResource {
        fn drop(&mut self) {
            let _ = self.entered.send(());
            if let Some(rx) = self.release.lock().unwrap().take() {
                let _ = rx.recv();
            }
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    let app = App::new();
    let root_fiber = app.root_fiber();

    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let dropped = Arc::new(AtomicBool::new(false));
    let resource = BlockingResource {
        entered: entered_tx,
        release: Arc::new(Mutex::new(Some(release_rx))),
        dropped: dropped.clone(),
    };

    // 独立单线程运行时：不驱动其事件循环，从而保证任务在首次 poll 之前就被 drop。
    let other_rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("构建独立运行时");
    {
        let _enter = other_rt.enter();
        app.root
            .spawn_scoped(move |_cancel| async move {
                let _hold = resource;
                std::future::pending::<()>().await;
            })
            .expect("spawn_scoped 应成功");
    }
    assert_eq!(root_fiber.scoped_task_count(), 1);

    // 在后台 OS 线程上销毁 other_rt，触发未首次 poll 任务的析构。
    let drop_thread = std::thread::spawn(move || {
        drop(other_rt);
    });

    // 等到资源进入 Drop 并阻塞在 release_rx 上。
    entered_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("未首次 poll 的任务必须开始析构其捕获资源");

    assert!(!dropped.load(Ordering::SeqCst), "资源析构此时仍被阻塞");
    assert_eq!(
        root_fiber.scoped_task_count(),
        1,
        "资源析构未完成前不得提前摘除任务槽位"
    );

    // 并发发起 App::close()：在资源完成释放前绝不得提前返回。
    let close_done = Arc::new(AtomicBool::new(false));
    let close_done_in = close_done.clone();
    let close_handle = tokio::spawn(async move {
        app.close().await;
        close_done_in.store(true, Ordering::SeqCst);
    });

    tokio::time::sleep(Duration::from_millis(80)).await;
    assert!(
        !close_done.load(Ordering::SeqCst),
        "任务资源析构尚未结束时，App::close() 不得提前返回"
    );

    // 放行资源析构；之后任务完成信号才可发出，App::close() 应立即完成。
    release_tx.send(()).unwrap();
    drop_thread.join().unwrap();

    tokio::time::timeout(Duration::from_secs(2), close_handle)
        .await
        .expect("资源释放后 App::close() 必须及时返回")
        .expect("close 任务不得 panic");
    assert!(dropped.load(Ordering::SeqCst));
    assert_eq!(root_fiber.scoped_task_count(), 0);
}

#[tokio::test]
async fn native_stream_unpolled_drop_releases_resources_before_signaling_close() {
    use cordis_core::{AsyncStream, BoxFuture, ExternService, Result, StreamSink};

    struct BlockingResource {
        entered: std::sync::mpsc::Sender<()>,
        release: Arc<Mutex<Option<std::sync::mpsc::Receiver<()>>>>,
        dropped: Arc<AtomicBool>,
    }
    impl Drop for BlockingResource {
        fn drop(&mut self) {
            let _ = self.entered.send(());
            if let Some(rx) = self.release.lock().unwrap().take() {
                let _ = rx.recv();
            }
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    struct StreamSvc {
        resource: Mutex<Option<BlockingResource>>,
    }
    impl ExternService for StreamSvc {
        fn service(&self) -> &str {
            "stream-svc"
        }

        fn version(&self) -> u32 {
            1
        }

        fn methods(&self) -> Vec<String> {
            vec!["open".to_string()]
        }

        fn call(
            &self,
            _caller_meta: Option<Config>,
            _method: String,
            _args: Config,
        ) -> BoxFuture<Result<Config>> {
            Box::pin(async { Ok(Config::Null) })
        }

        fn call_stream(
            &self,
            _caller_meta: Option<Config>,
            _method: String,
            _args: Config,
            _sink: StreamSink,
        ) -> BoxFuture<Result<()>> {
            let res = self.resource.lock().unwrap().take();
            Box::pin(async move {
                let _hold = res;
                std::future::pending::<()>().await;
                Ok(())
            })
        }
    }

    let app = App::new();
    let root_fiber = app.root_fiber();
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let dropped = Arc::new(AtomicBool::new(false));
    let svc = Arc::new(StreamSvc {
        resource: Mutex::new(Some(BlockingResource {
            entered: entered_tx,
            release: Arc::new(Mutex::new(Some(release_rx))),
            dropped: dropped.clone(),
        })),
    });
    let _provide = app.root.provide_iface("stream-svc", svc).unwrap();

    let other_rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("构建独立运行时");
    let _stream: AsyncStream = {
        let _enter = other_rt.enter();
        app.root
            .call_iface_stream("stream-svc", 1, "open", Config::Null)
            .expect("call_iface_stream 应成功")
    };
    assert_eq!(root_fiber.scoped_task_count(), 1);

    let drop_thread = std::thread::spawn(move || {
        drop(other_rt);
    });
    entered_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("原生流任务未首次 poll 被丢弃时必须先析构资源");
    assert_eq!(
        root_fiber.scoped_task_count(),
        1,
        "原生流任务资源释放完成前不得摘除任务槽位"
    );

    let close_done = Arc::new(AtomicBool::new(false));
    let close_done_in = close_done.clone();
    let close_handle = tokio::spawn(async move {
        app.close().await;
        close_done_in.store(true, Ordering::SeqCst);
    });

    tokio::time::sleep(Duration::from_millis(80)).await;
    assert!(
        !close_done.load(Ordering::SeqCst),
        "原生流任务资源尚未释放完成时，App::close() 不得提前返回"
    );

    release_tx.send(()).unwrap();
    drop_thread.join().unwrap();

    tokio::time::timeout(Duration::from_secs(2), close_handle)
        .await
        .expect("资源释放后 App::close() 必须返回")
        .expect("close 任务不得 panic");
    assert!(dropped.load(Ordering::SeqCst));
    assert_eq!(root_fiber.scoped_task_count(), 0);
}
