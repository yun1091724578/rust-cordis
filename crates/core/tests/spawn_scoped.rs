//! 受管后台任务（`Ctx::spawn_scoped` / `Fiber::spawn_scoped`）：
//! 生命周期归属 fiber、卸载取消并等待、自然结束自动摘除、ADR 0011 correlation 透传。

use cordis_core::{
    current_correlation_id, plugin_fn, scope_correlation, App, Config, Ctx, FiberState, Inject,
    SpawnScopedError,
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
