//! 结构化关闭报告（`CloseReport`）与三段生命周期等待预算（`LifecyclePolicy`）验收测试。

use cordis_core::{
    plugin_fn, App, CleanupErrorKind, CloseStatus, Config, Ctx, Disposable, FiberState, Inject,
    LifecyclePolicy, ReloadDeferred,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[test]
fn lifecycle_policy_rejects_zero_budgets_and_preserves_valid_values() {
    assert!(LifecyclePolicy::new(Some(0), None, None).is_err());
    assert!(LifecyclePolicy::new(None, Some(0), None).is_err());
    assert!(LifecyclePolicy::new(None, None, Some(0)).is_err());

    let policy = LifecyclePolicy::new(Some(10), Some(20), Some(30)).expect("合法预算应通过");
    assert_eq!(policy.apply_timeout_ms, Some(10));
    assert_eq!(policy.drain_timeout_ms, Some(20));
    assert_eq!(policy.dispose_timeout_ms, Some(30));
    assert_eq!(
        LifecyclePolicy::default(),
        LifecyclePolicy {
            apply_timeout_ms: None,
            drain_timeout_ms: None,
            dispose_timeout_ms: None,
        }
    );
}

#[tokio::test]
async fn close_with_report_returns_completed_when_clean() {
    let app = App::new();
    let cleaned = Arc::new(AtomicBool::new(false));
    let cleaned_flag = cleaned.clone();

    let plugin = plugin_fn("clean-plugin", Inject::default(), move |ctx: Ctx, _| {
        let cleaned = cleaned_flag.clone();
        Box::pin(async move {
            ctx.effect(move |_| {
                Disposable::sync(move || {
                    cleaned.store(true, Ordering::SeqCst);
                })
            });
            Ok(())
        })
    });
    app.plugin(plugin, Config::Null).await.expect("挂载成功");

    let report = app.close_with_report(None).await;
    assert_eq!(report.status, CloseStatus::Completed);
    assert!(!report.still_running);
    assert!(report.errors.is_empty());
    assert!(cleaned.load(Ordering::SeqCst));
}

#[tokio::test]
async fn close_with_report_aggregates_native_disposer_panics_across_tree() {
    let app = App::new();
    let sibling_cleaned = Arc::new(AtomicBool::new(false));
    let sibling_flag = sibling_cleaned.clone();

    let parent = plugin_fn("parent-plugin", Inject::default(), move |ctx: Ctx, _| {
        let sibling = sibling_flag.clone();
        Box::pin(async move {
            ctx.effect(move |_| {
                Disposable::sync(move || {
                    sibling.store(true, Ordering::SeqCst);
                })
            });
            ctx.effect(|_| Disposable::sync(|| panic!("boom-parent")));

            let child = plugin_fn("child-plugin", Inject::default(), |child_ctx: Ctx, _| {
                Box::pin(async move {
                    child_ctx.effect(|_| Disposable::sync(|| panic!("boom-child")));
                    Ok(())
                })
            });
            ctx.plugin_and_await(child, Config::Null).await?;
            Ok(())
        })
    });
    app.plugin(parent, Config::Null).await.expect("挂载成功");

    let report = app.close_with_report(None).await;
    assert_eq!(report.status, CloseStatus::CompletedWithErrors);
    assert!(!report.still_running);
    assert!(
        sibling_cleaned.load(Ordering::SeqCst),
        "单个 disposer panic 不得阻断后续恢复栈执行"
    );
    assert_eq!(report.errors.len(), 2, "父子两个 disposer panic 都应汇总");
    assert!(
        report
            .errors
            .iter()
            .all(|err| err.kind == CleanupErrorKind::DisposerPanic),
        "应归类为 DisposerPanic: {:?}",
        report.errors
    );
    assert!(
        report.errors[0].fiber == "child-plugin" && report.errors[0].message.contains("boom-child"),
        "LIFO 下子插件先清理: {:?}",
        report.errors
    );
    assert!(
        report.errors[1].fiber == "parent-plugin"
            && report.errors[1].message.contains("boom-parent"),
        "父插件随后清理: {:?}",
        report.errors
    );
}

#[tokio::test]
async fn apply_timeout_ms_fails_hanging_apply_and_rolls_back_partial_effects() {
    let app = App::new();
    app.set_lifecycle_policy(LifecyclePolicy::new(Some(40), None, None).unwrap())
        .unwrap();
    assert_eq!(app.lifecycle_policy().apply_timeout_ms, Some(40));

    let rolled_back = Arc::new(AtomicBool::new(false));
    let rolled_back_flag = rolled_back.clone();
    let hanging = plugin_fn("hanging-apply", Inject::default(), move |ctx: Ctx, _| {
        let rolled_back = rolled_back_flag.clone();
        Box::pin(async move {
            ctx.effect(move |_| {
                Disposable::sync(move || {
                    rolled_back.store(true, Ordering::SeqCst);
                })
            });
            std::future::pending::<()>().await;
            Ok(())
        })
    });

    let err = tokio::time::timeout(Duration::from_secs(2), app.plugin(hanging, Config::Null))
        .await
        .expect("apply_timeout_ms 必须及时中止等待")
        .err()
        .expect("超时后的 apply 必须失败");
    assert!(
        err.to_string().contains("apply 超时"),
        "错误信息应指出 apply 超时: {err}"
    );
    assert!(
        rolled_back.load(Ordering::SeqCst),
        "超时失败后必须回收已注册的半途 effect"
    );

    let report = app.close_with_report(None).await;
    assert_eq!(report.status, CloseStatus::Completed);
    assert!(!report.still_running);
}

#[tokio::test]
async fn drain_timeout_ms_defers_stuck_scoped_task_and_reports_timed_out() {
    let app = App::new();
    app.set_lifecycle_policy(LifecyclePolicy::new(None, Some(40), None).unwrap())
        .unwrap();

    let release = Arc::new(tokio::sync::Notify::new());
    let release_task = release.clone();
    let exited = Arc::new(AtomicBool::new(false));
    let exited_task = exited.clone();
    let started = Arc::new(tokio::sync::Notify::new());
    let started_task = started.clone();

    let plugin = plugin_fn("stuck-task", Inject::default(), move |ctx: Ctx, _| {
        let release = release_task.clone();
        let exited = exited_task.clone();
        let started = started_task.clone();
        Box::pin(async move {
            ctx.spawn_scoped(move |_cancel| async move {
                // 故意忽略取消信号，直到外部放行。
                started.notify_one();
                release.notified().await;
                exited.store(true, Ordering::SeqCst);
            })
            .expect("spawn_scoped 成功");
            Ok(())
        })
    });
    app.plugin(plugin, Config::Null).await.expect("挂载成功");
    started.notified().await;

    let store = app.store.clone();
    let report = tokio::time::timeout(Duration::from_secs(2), app.close_with_report(None))
        .await
        .expect("drain_timeout_ms 必须及时返回关闭报告");

    assert_eq!(report.status, CloseStatus::TimedOut);
    assert!(report.still_running);
    assert!(
        report
            .errors
            .iter()
            .any(|err| err.kind == CleanupErrorKind::Timeout),
        "超时关闭报告必须包含 Timeout 错误: {:?}",
        report.errors
    );
    assert!(!exited.load(Ordering::SeqCst), "放行前受管后台任务仍在运行");
    assert!(store.pending_background_cleanups() >= 1);

    release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), store.wait_background_cleanups())
        .await
        .expect("放行后后台清理必须收敛");
    assert!(exited.load(Ordering::SeqCst));
    assert_eq!(store.pending_background_cleanups(), 0);
}

#[tokio::test]
async fn dispose_timeout_ms_reports_timed_out_while_background_disposer_finishes() {
    let app = App::new();
    app.set_lifecycle_policy(LifecyclePolicy::new(None, None, Some(40)).unwrap())
        .unwrap();

    let release = Arc::new(tokio::sync::Notify::new());
    let release_disposer = release.clone();
    let finished = Arc::new(AtomicBool::new(false));
    let finished_disposer = finished.clone();

    let plugin = plugin_fn("slow-disposer", Inject::default(), move |ctx: Ctx, _| {
        let release = release_disposer.clone();
        let finished = finished_disposer.clone();
        Box::pin(async move {
            ctx.effect(move |_| {
                Disposable::new(move || {
                    Box::pin(async move {
                        release.notified().await;
                        finished.store(true, Ordering::SeqCst);
                    })
                })
            });
            Ok(())
        })
    });
    app.plugin(plugin, Config::Null).await.expect("挂载成功");

    let store = app.store.clone();
    let report = tokio::time::timeout(Duration::from_secs(2), app.close_with_report(None))
        .await
        .expect("dispose_timeout_ms 必须及时返回关闭报告");

    assert_eq!(report.status, CloseStatus::TimedOut);
    assert!(report.still_running);
    assert!(report
        .errors
        .iter()
        .any(|err| err.kind == CleanupErrorKind::Timeout));
    assert!(!finished.load(Ordering::SeqCst));
    assert!(store.pending_background_cleanups() >= 1);

    release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), store.wait_background_cleanups())
        .await
        .expect("放行后后台清理必须完成");
    assert!(finished.load(Ordering::SeqCst));
    assert_eq!(store.pending_background_cleanups(), 0);
}

#[tokio::test]
async fn reentrant_dispose_with_report_returns_deferred_without_deadlock() {
    let app = App::new();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let tx = Arc::new(std::sync::Mutex::new(Some(tx)));

    let plugin = plugin_fn("self-close", Inject::default(), move |ctx: Ctx, _| {
        let tx = tx.clone();
        Box::pin(async move {
            let fiber = ctx.fiber();
            ctx.spawn_scoped(move |_cancel| async move {
                let report = fiber.dispose_with_report(None).await;
                if let Some(tx) = tx.lock().unwrap().take() {
                    let _ = tx.send(report);
                }
            })
            .expect("spawn_scoped 成功");
            Ok(())
        })
    });
    let fiber = app.plugin(plugin, Config::Null).await.expect("挂载成功");

    let report = tokio::time::timeout(Duration::from_secs(2), rx)
        .await
        .expect("重入 dispose_with_report 不得自锁")
        .expect("收到报告");
    assert_eq!(report.status, CloseStatus::Deferred);
    assert!(report.still_running);

    tokio::time::timeout(Duration::from_secs(2), fiber.wait_background_cleanups())
        .await
        .expect("后台清理收敛");
    assert_eq!(fiber.state(), FiberState::Disposed);
    app.close().await;
}

#[tokio::test]
async fn drain_timeout_ms_bounds_waiting_for_stuck_dependent_consumer() {
    let app = App::new();
    app.set_lifecycle_policy(LifecyclePolicy::new(None, Some(40), None).unwrap())
        .unwrap();

    let release = Arc::new(tokio::sync::Notify::new());
    let release_consumer = release.clone();
    let consumer_done = Arc::new(AtomicBool::new(false));
    let consumer_done_flag = consumer_done.clone();

    let provider = plugin_fn("stuck-dep-provider", Inject::default(), |ctx: Ctx, _| {
        Box::pin(async move {
            ctx.provide("dep.svc", cordis_core::Dyn::new(1u32))?;
            Ok(())
        })
    });
    let provider_fiber = app
        .plugin(provider, Config::Null)
        .await
        .expect("提供者挂载");

    let consumer = plugin_fn(
        "stuck-dep-consumer",
        Inject::of(&["dep.svc"]),
        move |ctx: Ctx, _| {
            let release = release_consumer.clone();
            let done = consumer_done_flag.clone();
            Box::pin(async move {
                ctx.effect(move |_| {
                    Disposable::new(move || {
                        Box::pin(async move {
                            release.notified().await;
                            done.store(true, Ordering::SeqCst);
                        })
                    })
                });
                Ok(())
            })
        },
    );
    app.plugin(consumer, Config::Null)
        .await
        .expect("消费者挂载");

    let report = tokio::time::timeout(
        Duration::from_secs(2),
        provider_fiber.dispose_with_report(None),
    )
    .await
    .expect("drain_timeout_ms 必须限制等待卡死消费者的时间");

    assert_eq!(report.status, CloseStatus::TimedOut);
    assert!(report.still_running);
    assert!(
        report
            .errors
            .iter()
            .any(|err| err.kind == CleanupErrorKind::Timeout && err.message.contains("等待依赖者")),
        "应报告等待依赖者超时: {:?}",
        report.errors
    );
    assert!(!consumer_done.load(Ordering::SeqCst));

    release.notify_one();
    tokio::time::timeout(
        Duration::from_secs(2),
        provider_fiber.wait_background_cleanups(),
    )
    .await
    .expect("放行消费者后后台清理必须收敛");
    assert!(consumer_done.load(Ordering::SeqCst));
    assert_eq!(app.store.pending_background_cleanups(), 0);
    app.close().await;
}

#[tokio::test]
async fn set_lifecycle_policy_rejects_literal_zero_without_overwriting_existing() {
    let app = App::new();
    let valid = LifecyclePolicy::new(Some(100), Some(200), Some(300)).unwrap();
    app.set_lifecycle_policy(valid).expect("合法预算应生效");
    assert!(app
        .set_lifecycle_policy(LifecyclePolicy {
            apply_timeout_ms: Some(0),
            drain_timeout_ms: None,
            dispose_timeout_ms: None,
        })
        .is_err());
    assert_eq!(
        app.lifecycle_policy(),
        valid,
        "拒绝非法预算时不得改写原配置"
    );
    app.close().await;
}

#[tokio::test]
async fn concurrent_dispose_with_report_deduplicates_errors_and_clears_hooks_after_wait() {
    let app = App::new();
    let release = Arc::new(tokio::sync::Notify::new());
    let release_disposer = release.clone();
    let entered = Arc::new(tokio::sync::Notify::new());
    let entered_disposer = entered.clone();

    // 在根上下文挂一个 hook，验证 close_with_report 超时后 wait_background_cleanups 返回时 hook 已清空。
    app.root.on("root/hook", |_| Box::pin(async { None }));
    assert_eq!(app.store.hook_count("root/hook"), 1);

    let plugin = plugin_fn("dup-check", Inject::default(), move |ctx: Ctx, _| {
        let release = release_disposer.clone();
        let entered = entered_disposer.clone();
        Box::pin(async move {
            ctx.effect(|_| Disposable::sync(|| panic!("second-panic")));
            ctx.effect(move |_| {
                Disposable::new(move || {
                    Box::pin(async move {
                        entered.notify_one();
                        release.notified().await;
                        panic!("first-panic");
                    })
                })
            });
            Ok(())
        })
    });
    let fiber = app.plugin(plugin, Config::Null).await.expect("挂载成功");

    // 两个调用方并发调用 dispose_with_report：应各自看到精确且不重复的两条错误。
    let f1 = fiber.clone();
    let f2 = fiber.clone();
    let t1 = tokio::spawn(async move { f1.dispose_with_report(None).await });
    entered.notified().await;
    let t2 = tokio::spawn(async move { f2.dispose_with_report(None).await });
    tokio::task::yield_now().await;
    release.notify_one();

    let r1 = t1.await.unwrap();
    let r2 = t2.await.unwrap();
    assert_eq!(r1.status, CloseStatus::CompletedWithErrors);
    assert_eq!(r2.status, CloseStatus::CompletedWithErrors);
    assert_eq!(
        r1.errors.len(),
        2,
        "首个调用方应收到精确 2 条错误: {:?}",
        r1.errors
    );
    assert_eq!(
        r2.errors.len(),
        2,
        "并发第二个调用方既不漏记也不重复记: {:?}",
        r2.errors
    );

    // 再测试 App::close_with_report 超时后，wait_background_cleanups 返回时全局 hook 已确定性清空。
    let app2 = App::new();
    app2.root.on("close/hook", |_| Box::pin(async { None }));
    assert_eq!(app2.store.hook_count("close/hook"), 1);
    let rel2 = Arc::new(tokio::sync::Notify::new());
    let rel2_d = rel2.clone();
    let p2 = plugin_fn("hook-drain", Inject::default(), move |ctx: Ctx, _| {
        let rel = rel2_d.clone();
        Box::pin(async move {
            ctx.effect(move |_| {
                Disposable::new(move || Box::pin(async move { rel.notified().await }))
            });
            Ok(())
        })
    });
    app2.plugin(p2, Config::Null).await.expect("挂载成功");
    let store2 = app2.store.clone();
    let rep2 = app2
        .close_with_report(Some(Duration::from_millis(30)))
        .await;
    assert_eq!(rep2.status, CloseStatus::TimedOut);
    assert!(rep2.still_running);
    assert_eq!(
        store2.hook_count("close/hook"),
        1,
        "后台清理尚未完成时不得提前清空根级 hook"
    );
    rel2.notify_one();
    tokio::time::timeout(Duration::from_secs(2), store2.wait_background_cleanups())
        .await
        .expect("后台清理与收尾完成");
    assert_eq!(
        store2.hook_count("close/hook"),
        0,
        "wait_background_cleanups 返回时根级 hook 必须已经确定性清空"
    );
}

#[tokio::test]
async fn fail_dispose_effects_respects_dispose_timeout_and_preserves_lifo() {
    let app = App::new();
    app.set_lifecycle_policy(LifecyclePolicy::new(None, None, Some(40)).unwrap())
        .unwrap();

    let release = Arc::new(tokio::sync::Notify::new());
    let release_eff2 = release.clone();
    let order = Arc::new(std::sync::Mutex::new(Vec::<&'static str>::new()));
    let order_1 = order.clone();
    let order_2 = order.clone();

    let failing = plugin_fn(
        "fail-slow-disposer",
        Inject::default(),
        move |ctx: Ctx, _| {
            let release = release_eff2.clone();
            let o1 = order_1.clone();
            let o2 = order_2.clone();
            Box::pin(async move {
                ctx.effect(move |_| {
                    Disposable::sync(move || {
                        o1.lock().unwrap().push("eff1");
                    })
                });
                ctx.effect(move |_| {
                    Disposable::new(move || {
                        let release = release.clone();
                        let o2 = o2.clone();
                        Box::pin(async move {
                            release.notified().await;
                            o2.lock().unwrap().push("eff2");
                        })
                    })
                });
                Err("apply-failed-after-effects".into())
            })
        },
    );

    let err = tokio::time::timeout(Duration::from_secs(2), app.plugin(failing, Config::Null))
        .await
        .expect("Fiber::fail 的恢复栈清理必须受 dispose_timeout_ms 约束，不得永久卡死")
        .err()
        .expect("apply 失败必须返回 Err");
    assert!(err.to_string().contains("apply-failed-after-effects"));

    assert!(
        app.store.pending_background_cleanups() >= 1,
        "超时恢复栈应登记到后台清理台账"
    );
    assert!(
        order.lock().unwrap().is_empty(),
        "eff2 未完成前 eff1 不得抢跑破坏 LIFO: {:?}",
        *order.lock().unwrap()
    );

    // 放行前关闭 App：必须看到 TimedOut 且 errors 非空（带明确归因）。
    let store = app.store.clone();
    let close = app.close_with_report(None).await;
    assert_eq!(close.status, CloseStatus::TimedOut);
    assert!(close.still_running);
    assert!(
        close
            .errors
            .iter()
            .any(|e| e.fiber == "fail-slow-disposer" && e.kind == CleanupErrorKind::Timeout),
        "关闭报告必须归因到仍在后台清理的子 fiber: {:?}",
        close.errors
    );

    release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), store.wait_background_cleanups())
        .await
        .expect("放行后后台清理必须收敛");
    assert_eq!(
        *order.lock().unwrap(),
        vec!["eff2", "eff1"],
        "严格 LIFO 顺序必须在后台收尾中保持"
    );
    assert_eq!(store.pending_background_cleanups(), 0);
}

#[tokio::test]
async fn child_prior_timeout_is_attributed_in_app_close_with_report() {
    let app = App::new();
    app.set_lifecycle_policy(LifecyclePolicy::new(None, Some(30), None).unwrap())
        .unwrap();
    let started = Arc::new(tokio::sync::Notify::new());
    let started_t = started.clone();
    let release = Arc::new(tokio::sync::Notify::new());
    let release_t = release.clone();

    let plugin = plugin_fn(
        "prior-timeout-child",
        Inject::default(),
        move |ctx: Ctx, _| {
            let started = started_t.clone();
            let release = release_t.clone();
            Box::pin(async move {
                ctx.spawn_scoped(move |_cancel| async move {
                    started.notify_one();
                    release.notified().await;
                })
                .expect("spawn_scoped 成功");
                Ok(())
            })
        },
    );
    let child_fiber = app.plugin(plugin, Config::Null).await.expect("挂载成功");
    started.notified().await;

    // 子 fiber 先单独因 drain_timeout_ms 超时关闭：其 DisposeDone 已完成（已从 root 摘除），
    // 但后台仍留有 BackgroundCleanupGuard 跟踪未退出任务。
    let child_rep = child_fiber.dispose_with_report(None).await;
    assert_eq!(child_rep.status, CloseStatus::TimedOut);
    assert!(child_rep.still_running);
    assert!(!child_rep.errors.is_empty());

    // 之后再以无超时的 `App::close_with_report(None)` 关闭整个 App：
    // 收集器晚于子 fiber 报错挂载，但仍须返回 `TimedOut` 且给出非空子 fiber 归因。
    let store = app.store.clone();
    let app_rep = tokio::time::timeout(Duration::from_secs(2), app.close_with_report(None))
        .await
        .expect("root 无挂死 disposer 应立即返回");
    assert_eq!(app_rep.status, CloseStatus::TimedOut);
    assert!(app_rep.still_running);
    assert!(
        app_rep
            .errors
            .iter()
            .any(|e| e.fiber == "prior-timeout-child" && e.kind == CleanupErrorKind::Timeout),
        "App::close_with_report 必须包含此前超时子 fiber 的归因: {:?}",
        app_rep.errors
    );

    release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), store.wait_background_cleanups())
        .await
        .expect("后台清理收敛");
    assert_eq!(store.pending_background_cleanups(), 0);
}

#[test]
fn lifecycle_policy_budgets_do_not_panic_without_tokio_runtime() {
    use std::future::Future;
    use std::task::{Context, Poll, Waker};

    fn poll_sync<F: Future>(fut: F) -> F::Output {
        let mut fut = std::pin::pin!(fut);
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        for _ in 0..1_024 {
            if let Poll::Ready(val) = fut.as_mut().poll(&mut cx) {
                return val;
            }
        }
        panic!("同步 poll 超过上限未就绪");
    }

    let app = App::new();
    app.set_lifecycle_policy(LifecyclePolicy::new(Some(50), Some(50), Some(50)).unwrap())
        .unwrap();

    let cleaned = Arc::new(AtomicBool::new(false));
    let cleaned_flag = cleaned.clone();
    let plugin = plugin_fn("no-tokio-plugin", Inject::default(), move |ctx: Ctx, _| {
        let cleaned = cleaned_flag.clone();
        Box::pin(async move {
            ctx.effect(move |_| {
                Disposable::sync(move || {
                    cleaned.store(true, Ordering::SeqCst);
                })
            });
            Ok(())
        })
    });

    poll_sync(app.plugin(plugin, Config::Null)).expect("无 Tokio 运行时下带预算装载不得 panic");
    let report = poll_sync(app.close_with_report(Some(Duration::from_millis(50))));
    assert_eq!(report.status, CloseStatus::Completed);
    assert!(!report.still_running);
    assert!(report.errors.is_empty());
    assert!(cleaned.load(Ordering::SeqCst));
}

#[tokio::test]
async fn restart_unload_respects_dispose_timeout_and_blocks_overlapping_load() {
    let app = App::new();
    app.set_lifecycle_policy(LifecyclePolicy::new(None, None, Some(40)).unwrap())
        .unwrap();

    let release = Arc::new(tokio::sync::Notify::new());
    let release_d = release.clone();
    let apply_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let apply_count_p = apply_count.clone();
    let cleaned = Arc::new(AtomicBool::new(false));
    let cleaned_p = cleaned.clone();

    let plugin = plugin_fn(
        "slow-restart-unload",
        Inject::default(),
        move |ctx: Ctx, _| {
            let release = release_d.clone();
            let count = apply_count_p.clone();
            let cleaned = cleaned_p.clone();
            Box::pin(async move {
                let n = count.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    ctx.effect(move |_| {
                        let release = release.clone();
                        let cleaned = cleaned.clone();
                        Disposable::new(move || {
                            Box::pin(async move {
                                release.notified().await;
                                cleaned.store(true, Ordering::SeqCst);
                            })
                        })
                    });
                }
                Ok(())
            })
        },
    );
    let fiber = app.plugin(plugin, Config::Null).await.expect("初装成功");
    assert_eq!(apply_count.load(Ordering::SeqCst), 1);

    // 触发 restart：旧代次 disposer 卡住超过 dispose_timeout_ms，必须及时返回且不得进入第 2 次 apply。
    // 这只是暂态：返回 `ReloadDeferred`（等待清理），停在 Pending 而非 Failed。
    let err = tokio::time::timeout(Duration::from_secs(2), fiber.restart())
        .await
        .expect("restart 卸载必须受 dispose_timeout_ms 约束，不得永久挂死")
        .expect_err("清理未完成时 restart 应报告推迟");
    let deferred = err
        .downcast_ref::<ReloadDeferred>()
        .unwrap_or_else(|| panic!("应返回 ReloadDeferred，实际: {err}"));
    assert!(deferred.is_waiting_for_cleanup());
    assert!(deferred.is_scheduled());
    assert_eq!(fiber.state(), FiberState::Pending);
    assert!(fiber.error().is_none(), "暂态清理超时不得记成 Failed 错误");
    assert!(fiber
        .cleanup_errors()
        .iter()
        .any(|e| e.kind == CleanupErrorKind::Timeout && e.message.contains("清理超时")));
    assert_eq!(
        apply_count.load(Ordering::SeqCst),
        1,
        "旧代次清理未完成前不得重入执行新一代 apply"
    );
    assert!(fiber.pending_background_cleanups() >= 1);

    // 后台清理未完成时再次 update：同样推迟，不得并发装载新一代。
    let err2 = fiber
        .update(Config::Null)
        .await
        .expect_err("后台清理未完成时 update 应报告推迟");
    assert!(err2
        .downcast_ref::<ReloadDeferred>()
        .is_some_and(ReloadDeferred::is_waiting_for_cleanup));
    assert_eq!(apply_count.load(Ordering::SeqCst), 1);

    // 放行旧代次 disposer：清理归零后无需人工干预，自动装载新一代。
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), fiber.wait_background_cleanups())
        .await
        .expect("旧代次后台清理完成");
    assert!(cleaned.load(Ordering::SeqCst));
    tokio::time::timeout(Duration::from_secs(2), async {
        while fiber.state() != FiberState::Active {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("清理归零后应自动重试并恢复 Active");
    assert_eq!(apply_count.load(Ordering::SeqCst), 2);

    let close = app.close_with_report(None).await;
    assert_eq!(close.status, CloseStatus::Completed);
}

async fn wait_state(fiber: &Arc<cordis_core::Fiber>, want: FiberState) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while fiber.state() != want {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "等待 {want:?} 超时，当前 {:?} / error={:?}",
            fiber.state(),
            fiber.error()
        )
    });
}

/// 提供者重启时消费者卸载慢于 drain 预算：提供者不得永久卡在 Failed，清理归零后自动恢复。
#[tokio::test]
async fn provider_restart_with_slow_consumer_auto_recovers_after_cleanup() {
    let app = App::new();
    app.set_lifecycle_policy(LifecyclePolicy::new(None, Some(30), None).unwrap())
        .unwrap();
    let release = Arc::new(tokio::sync::Notify::new());
    let rel = release.clone();

    let provider = plugin_fn("live-provider", Inject::default(), |ctx: Ctx, _| {
        Box::pin(async move {
            ctx.provide("live.svc", cordis_core::Dyn::new(1u32))?;
            Ok(())
        })
    });
    let provider_fiber = app.plugin(provider, Config::Null).await.unwrap();
    let consumer = plugin_fn(
        "live-consumer",
        Inject::of(&["live.svc"]),
        move |ctx: Ctx, _| {
            let rel = rel.clone();
            Box::pin(async move {
                ctx.effect(move |_| {
                    let rel = rel.clone();
                    Disposable::new(move || Box::pin(async move { rel.notified().await }))
                });
                Ok(())
            })
        },
    );
    let consumer_fiber = app.plugin(consumer, Config::Null).await.unwrap();

    let err = tokio::time::timeout(Duration::from_secs(2), provider_fiber.restart())
        .await
        .expect("drain 预算必须限制等待")
        .expect_err("依赖者清理未完成时应报告推迟");
    assert!(err
        .downcast_ref::<ReloadDeferred>()
        .is_some_and(ReloadDeferred::is_waiting_for_cleanup));
    assert_eq!(provider_fiber.state(), FiberState::Pending);
    assert!(provider_fiber.error().is_none());

    release.notify_one();
    wait_state(&provider_fiber, FiberState::Active).await;
    wait_state(&consumer_fiber, FiberState::Active).await;
    // 新一代消费者又注册了等待同一 Notify 的 disposer：关闭前先放行。
    release.notify_one();
    let close = app.close_with_report(None).await;
    assert_eq!(close.status, CloseStatus::Completed);
}

/// 提供者消失时消费者卸载慢于 dispose 预算：提供者回来后消费者应自动重新激活。
#[tokio::test]
async fn consumer_slow_unload_on_provider_loss_reactivates_when_provider_returns() {
    let app = App::new();
    app.set_lifecycle_policy(LifecyclePolicy::new(None, None, Some(30)).unwrap())
        .unwrap();
    let release = Arc::new(tokio::sync::Notify::new());
    let rel = release.clone();

    let provider = plugin_fn("live-provider-b", Inject::default(), |ctx: Ctx, _| {
        Box::pin(async move {
            ctx.provide("live.b", cordis_core::Dyn::new(1u32))?;
            Ok(())
        })
    });
    let provider_fiber = app.plugin(provider.clone(), Config::Null).await.unwrap();
    let consumer = plugin_fn(
        "live-consumer-b",
        Inject::of(&["live.b"]),
        move |ctx: Ctx, _| {
            let rel = rel.clone();
            Box::pin(async move {
                ctx.effect(move |_| {
                    let rel = rel.clone();
                    Disposable::new(move || Box::pin(async move { rel.notified().await }))
                });
                Ok(())
            })
        },
    );
    let consumer_fiber = app.plugin(consumer, Config::Null).await.unwrap();

    provider_fiber.dispose().await;
    wait_state(&consumer_fiber, FiberState::Pending).await;
    assert!(consumer_fiber.error().is_none(), "暂态清理超时不得记成错误");

    // 清理未归零时提供者就回来：消费者等清理归零后再激活。
    app.plugin(provider, Config::Null).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(consumer_fiber.state(), FiberState::Pending);
    release.notify_one();
    wait_state(&consumer_fiber, FiberState::Active).await;
    // 新一代消费者又注册了等待同一 Notify 的 disposer：关闭前先放行。
    release.notify_one();
    let close = app.close_with_report(None).await;
    assert_eq!(close.status, CloseStatus::Completed);
}
