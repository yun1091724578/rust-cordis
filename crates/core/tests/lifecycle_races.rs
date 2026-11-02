//! 生命周期迁移的同步/等待契约。
//!
//! 四条测试对应四类真实竞态：提供者资源关闭过早、effect 登记与卸载交错、
//! dispose Future 被丢弃导致恢复栈丢失、重复 dispose 提前返回。

use cordis_core::{plugin_fn, App, Config, Ctx, Disposable, Dyn, FiberState, Inject, Level};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

struct Resource {
    open: Arc<AtomicBool>,
}

/// 关闭后的上下文不能再挂插件；可检查入口返回错误而不是 panic。
#[tokio::test]
async fn try_plugin_on_disposed_context_returns_error() {
    let app = App::new();
    let root = app.root.clone();
    app.close().await;

    let error = match root.try_plugin(noop_plugin(), Config::Null) {
        Ok(_) => panic!("已处置上下文必须拒绝挂载"),
        Err(error) => error,
    };
    assert!(
        error.contains("disposed") || error.contains("关闭"),
        "错误必须说明上下文已关闭：{error}"
    );
}

fn noop_plugin() -> Arc<dyn cordis_core::Plugin> {
    plugin_fn("race-fiber", Inject::default(), |_ctx: Ctx, _cfg| {
        Box::pin(async { Ok(()) })
    })
}

/// 提供者必须先等消费者拆完，再执行自己的恢复栈；否则消费者 cleanup 读到的
/// 服务虽然还在，但其背后的资源已经被关闭。
#[tokio::test]
async fn provider_waits_for_consumer_before_closing_resources() {
    let app = App::new();
    let open = Arc::new(AtomicBool::new(true));
    let observed = Arc::new(Mutex::new(None));

    let provider_open = open.clone();
    let provider = plugin_fn("provider", Inject::default(), move |ctx: Ctx, _cfg| {
        let open = provider_open.clone();
        Box::pin(async move {
            ctx.provide("svc", Dyn::new(Resource { open: open.clone() }))?;
            // 后注册 ⇒ 先执行。旧实现会在这里关闭资源，然后才去等消费者。
            ctx.effect(move |_| Disposable::sync(move || open.store(false, Ordering::SeqCst)));
            Ok(())
        })
    });

    let consumer_observed = observed.clone();
    let consumer = plugin_fn("consumer", Inject::of(&["svc"]), move |ctx: Ctx, _cfg| {
        let observed = consumer_observed.clone();
        Box::pin(async move {
            ctx.effect(move |ctx| {
                Disposable::sync(move || {
                    let seen = ctx
                        .require("svc")
                        .ok()
                        .and_then(|value| value.downcast::<Resource>())
                        .map(|resource| resource.open.load(Ordering::SeqCst));
                    *observed.lock().unwrap() = seen;
                })
            });
            Ok(())
        })
    });

    let provider_fiber = app.plugin(provider, Config::Null).await.unwrap();
    let consumer_fiber = app.plugin(consumer, Config::Null).await.unwrap();
    assert_eq!(consumer_fiber.state(), FiberState::Active);

    tokio::time::timeout(Duration::from_secs(2), provider_fiber.dispose())
        .await
        .expect("提供者卸载不得挂起");
    consumer_fiber.settle().await;

    assert_eq!(
        *observed.lock().unwrap(),
        Some(true),
        "消费者拆除期间，提供者资源必须仍然可用"
    );
    assert!(!open.load(Ordering::SeqCst), "消费者拆完后提供者才关闭资源");
}

/// 另一个线程已经在 setup 里、但尚未返回时，卸载必须等它登记完再把恢复栈取走；
/// 反过来，卸载开始后新 effect 必须被拒绝。
#[test]
fn effect_setup_racing_with_dispose_is_accounted_for() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let app = App::new();
        let fiber = app.plugin(noop_plugin(), Config::Null).await.unwrap();

        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let closed = Arc::new(AtomicBool::new(false));
        let setup_closed = closed.clone();
        let setup_fiber = fiber.clone();
        let setup = std::thread::spawn(move || {
            setup_fiber.effect(move |_| {
                started_tx.send(()).expect("started");
                release_rx.recv().expect("release");
                Disposable::sync(move || setup_closed.store(true, Ordering::SeqCst))
            });
        });

        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("setup 必须已经进入");
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            let _ = release_tx.send(());
        });

        tokio::time::timeout(Duration::from_secs(2), fiber.dispose())
            .await
            .expect("dispose 必须等 setup 完成");
        setup.join().expect("setup 线程");
        release.join().expect("release 线程");

        assert!(
            closed.load(Ordering::SeqCst),
            "迟到的 disposer 仍必须被执行"
        );
        assert_eq!(fiber.effect_count(), 0, "不得把 effect 留在已处置 fiber 上");
        assert_eq!(fiber.state(), FiberState::Disposed);
        assert!(fiber.ctx_opt().is_none(), "处置完成后 context 必须清空");
    });
}

/// dispose Future 被 timeout 丢弃时，恢复栈仍归 fiber 所有；再次 dispose
/// 必须继续执行剩余的 disposer，而不是停在 Unloading。
#[tokio::test]
async fn dropping_dispose_future_keeps_the_remaining_restore_stack() {
    let app = App::new();
    let fiber = app.plugin(noop_plugin(), Config::Null).await.unwrap();

    let closed = Arc::new(AtomicBool::new(false));
    let resource_closed = closed.clone();
    fiber.effect(move |_| Disposable::sync(move || resource_closed.store(true, Ordering::SeqCst)));

    let entered = Arc::new(tokio::sync::Notify::new());
    let blocker = Arc::new(tokio::sync::Notify::new());
    let entered2 = entered.clone();
    let blocker2 = blocker.clone();
    fiber.effect(move |_| {
        Disposable::new(move || {
            entered2.notify_one();
            let blocker = blocker2.clone();
            Box::pin(async move { blocker.notified().await })
        })
    });

    let first = tokio::time::timeout(Duration::from_millis(50), fiber.dispose()).await;
    assert!(first.is_err(), "第一个 dispose 应被阻塞中的 disposer 卡住");
    tokio::time::timeout(Duration::from_secs(1), entered.notified())
        .await
        .expect("异步 disposer 应当已经开始");
    blocker.notify_one();

    tokio::time::timeout(Duration::from_secs(2), fiber.dispose())
        .await
        .expect("第二次 dispose 必须继续完成剩余恢复栈");

    assert!(
        closed.load(Ordering::SeqCst),
        "剩余 disposer 不得随 Future 丢弃"
    );
    assert_eq!(fiber.effect_count(), 0);
    assert_eq!(fiber.state(), FiberState::Disposed);
    assert!(fiber.ctx_opt().is_none());
}

/// 重复 dispose 不是“看到终态就返回”，而是等待已有清理完成。
#[tokio::test]
async fn repeated_dispose_waits_for_inflight_cleanup() {
    let app = App::new();
    let fiber = app.plugin(noop_plugin(), Config::Null).await.unwrap();

    let entered = Arc::new(tokio::sync::Notify::new());
    let blocker = Arc::new(tokio::sync::Notify::new());
    let entered2 = entered.clone();
    let blocker2 = blocker.clone();
    fiber.effect(move |_| {
        Disposable::new(move || {
            entered2.notify_one();
            let blocker = blocker2.clone();
            Box::pin(async move { blocker.notified().await })
        })
    });

    let first = {
        let fiber = fiber.clone();
        tokio::spawn(async move { fiber.dispose().await })
    };
    tokio::time::timeout(Duration::from_secs(1), entered.notified())
        .await
        .expect("清理任务应当已经开始");

    let second = tokio::time::timeout(Duration::from_millis(100), fiber.dispose()).await;
    assert!(second.is_err(), "第二次 dispose 不得在清理完成前提前返回");

    blocker.notify_one();
    tokio::time::timeout(Duration::from_secs(2), first)
        .await
        .expect("第一次 dispose 必须完成")
        .expect("dispose 任务不得 panic");

    tokio::time::timeout(Duration::from_secs(2), fiber.dispose())
        .await
        .expect("清理完成后 dispose 应立即返回");
    assert_eq!(fiber.state(), FiberState::Disposed);
    assert!(fiber.ctx_opt().is_none());
}

/// `App::close()` 也必须等待子 fiber 正在进行的异步清理，而不是看到
/// `uid == DISPOSED` 就返回。
#[tokio::test]
async fn app_close_waits_for_inflight_child_cleanup() {
    let app = App::new();
    let child = app.plugin(noop_plugin(), Config::Null).await.unwrap();

    let entered = Arc::new(tokio::sync::Notify::new());
    let blocker = Arc::new(tokio::sync::Notify::new());
    let entered2 = entered.clone();
    let blocker2 = blocker.clone();
    child.effect(move |_| {
        Disposable::new(move || {
            entered2.notify_one();
            let blocker = blocker2.clone();
            Box::pin(async move { blocker.notified().await })
        })
    });

    let first = {
        let child = child.clone();
        tokio::spawn(async move { child.dispose().await })
    };
    tokio::time::timeout(Duration::from_secs(1), entered.notified())
        .await
        .expect("子 fiber 清理应当已经开始");

    let mut close = Box::pin(app.close());
    let early = tokio::time::timeout(Duration::from_millis(100), &mut close).await;
    assert!(early.is_err(), "App::close 不得在子 fiber 清理完成前返回");

    blocker.notify_one();
    tokio::time::timeout(Duration::from_secs(2), close)
        .await
        .expect("释放阻塞后 App::close 必须完成");
    tokio::time::timeout(Duration::from_secs(2), first)
        .await
        .expect("子 dispose 必须完成")
        .expect("子 dispose 不得 panic");

    assert_eq!(child.state(), FiberState::Disposed);
    assert!(child.ctx_opt().is_none());
}

/// 祖先消费者是**明确的支持范围外拓扑**：提供者无法等待正在拆自己的祖先，
/// 否则父子 driver 互等。这里锁定两件事：卸载不挂起，且必须留下可诊断告警。
#[tokio::test]
async fn ancestor_consumer_is_outside_supported_scope_and_logged() {
    let app = App::new();
    let warnings = Arc::new(Mutex::new(Vec::<(String, String)>::new()));
    let captured = warnings.clone();
    app.store
        .set_log_sink(Arc::new(move |level, name, message| {
            if level == Level::Warn {
                captured
                    .lock()
                    .unwrap()
                    .push((name.to_string(), message.to_string()));
            }
        }));

    let open = Arc::new(AtomicBool::new(true));
    let provider_open = open.clone();
    let child_provider = plugin_fn(
        "ancestor-provider",
        Inject::default(),
        move |ctx: Ctx, _cfg| {
            let open = provider_open.clone();
            Box::pin(async move {
                ctx.provide("ancestor-svc", Dyn::new(Resource { open: open.clone() }))?;
                ctx.effect(move |_| Disposable::sync(move || open.store(false, Ordering::SeqCst)));
                Ok(())
            })
        },
    );

    // 祖先先以 PENDING 存在，随后由它的 ctx 挂载后代提供者；这样依赖边方向
    // 是“祖先前向引用后代服务”，正是支持范围外的形状。
    let ancestor = app.plugin(
        plugin_fn(
            "ancestor-consumer",
            Inject::of(&["ancestor-svc"]),
            |_ctx: Ctx, _cfg| Box::pin(async { Ok(()) }),
        ),
        Config::Null,
    );
    let ancestor = ancestor.await.unwrap();
    let provider = ancestor.ctx().plugin(child_provider, Config::Null);
    provider.settle().await;
    ancestor.settle().await;
    assert_eq!(ancestor.state(), FiberState::Active);

    tokio::time::timeout(Duration::from_secs(2), provider.dispose())
        .await
        .expect("支持范围外的拓扑也不得把卸载挂死");
    tokio::time::timeout(Duration::from_secs(2), ancestor.settle())
        .await
        .expect("祖先消费者必须仍能收敛");

    let warnings = warnings.lock().unwrap().clone();
    assert!(
        warnings.iter().any(|(name, message)| {
            name == "ancestor-provider" && message.contains("生命周期支持范围外")
        }),
        "必须留下明确的“不支持拓扑”诊断，而不是静默跳过：{warnings:?}"
    );
    assert!(!open.load(Ordering::SeqCst), "提供者恢复栈最终仍要执行");
}

/// 丢弃 `dispose` Future 的时机如果落在 `close_effects()` 的等待窗口内
/// （uid 已置为 DISPOSED、清理任务尚未 spawn），后续 `dispose()` 仍必须完成清理。
#[tokio::test]
async fn dropping_dispose_future_during_effect_gate_wait_still_finishes_cleanup() {
    let app = App::new();
    let fiber = app.plugin(noop_plugin(), Config::Null).await.unwrap();

    let closed = Arc::new(AtomicBool::new(false));
    let setup_closed = closed.clone();
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let setup_fiber = fiber.clone();
    let setup = std::thread::spawn(move || {
        setup_fiber.effect(move |_| {
            started_tx.send(()).expect("started");
            release_rx.recv().expect("release");
            Disposable::sync(move || setup_closed.store(true, Ordering::SeqCst))
        });
    });
    started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("setup 必须已经进入");

    // 第一次 dispose 停在 close_effects() 的 Notify 等待上；timeout 把 Future 丢掉。
    let first = tokio::time::timeout(Duration::from_millis(50), fiber.dispose()).await;
    assert!(first.is_err(), "close_effects 仍应等待在途 setup");
    let _ = release_tx.send(());

    tokio::time::timeout(Duration::from_secs(2), fiber.dispose())
        .await
        .expect("闸门等待期丢弃 Future 后，第二次 dispose 不得永久挂起");
    setup.join().expect("setup 线程");

    assert!(
        closed.load(Ordering::SeqCst),
        "迟到的 disposer 仍必须被执行"
    );
    assert_eq!(fiber.effect_count(), 0);
    assert_eq!(fiber.state(), FiberState::Disposed);
    assert!(fiber.ctx_opt().is_none());
}

/// 消费者**已经进入**自己的清理任务（uid = DISPOSED，disposer 尚未跑完）时，
/// 提供者卸载必须等它真正拆完，再关闭自己的资源。
#[tokio::test]
async fn provider_waits_for_an_already_disposing_consumer() {
    let app = App::new();
    let open = Arc::new(AtomicBool::new(true));
    let observed = Arc::new(Mutex::new(None));

    let provider_open = open.clone();
    let provider = plugin_fn(
        "inflight-provider",
        Inject::default(),
        move |ctx: Ctx, _cfg| {
            let open = provider_open.clone();
            Box::pin(async move {
                ctx.provide("inflight-svc", Dyn::new(Resource { open: open.clone() }))?;
                ctx.effect(move |_| Disposable::sync(move || open.store(false, Ordering::SeqCst)));
                Ok(())
            })
        },
    );
    let provider_fiber = app.plugin(provider, Config::Null).await.unwrap();

    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let consumer_observed = observed.clone();
    let entered2 = entered.clone();
    let release2 = release.clone();
    let consumer = plugin_fn(
        "inflight-consumer",
        Inject::of(&["inflight-svc"]),
        move |ctx: Ctx, _cfg| {
            let entered = entered2.clone();
            let release = release2.clone();
            let observed = consumer_observed.clone();
            Box::pin(async move {
                ctx.effect(move |ctx| {
                    Disposable::new(move || {
                        let entered = entered.clone();
                        let release = release.clone();
                        let observed = observed.clone();
                        Box::pin(async move {
                            entered.notify_one();
                            release.notified().await;
                            let seen = ctx
                                .require("inflight-svc")
                                .ok()
                                .and_then(|value| value.downcast::<Resource>())
                                .map(|resource| resource.open.load(Ordering::SeqCst));
                            *observed.lock().unwrap() = seen;
                        })
                    })
                });
                Ok(())
            })
        },
    );
    let consumer_fiber = app.plugin(consumer, Config::Null).await.unwrap();

    // 消费者先进入清理，并卡在自己的 disposer 里。
    let consumer_dispose = tokio::spawn({
        let fiber = consumer_fiber.clone();
        async move { fiber.dispose().await }
    });
    tokio::time::timeout(Duration::from_secs(1), entered.notified())
        .await
        .expect("消费者 disposer 应当已经开始");

    let mut provider_dispose = Box::pin(provider_fiber.dispose());
    let early = tokio::time::timeout(Duration::from_millis(100), &mut provider_dispose).await;
    // 无论断言是否成立，先放行消费者，避免把阻塞任务留给运行时销毁。
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), consumer_dispose)
        .await
        .expect("消费者卸载应完成")
        .expect("消费者 dispose 不得 panic");
    // 只有提供者确实还在等消费者时才需要继续 poll；提前完成时再 poll 会
    // 触发 "async fn resumed after completion"。
    if early.is_err() {
        tokio::time::timeout(Duration::from_secs(2), provider_dispose)
            .await
            .expect("提供者卸载应完成");
    }

    assert!(
        early.is_err(),
        "已开始拆除的消费者完成前，提供者不得提前完成卸载并关闭资源"
    );
    assert_eq!(
        *observed.lock().unwrap(),
        Some(true),
        "消费者拆除期间，提供者资源必须仍然可用"
    );
    assert!(!open.load(Ordering::SeqCst), "消费者拆完后提供者才关闭资源");
}

/// 清理任务内部 panic 不能提前置完成位；监督者应兜底排水并记录诊断。
#[tokio::test]
async fn cleanup_panic_is_contained_and_reported() {
    let app = App::new();
    // 故意让错误日志通道自身 panic：它会在 disposer 的 JoinError 处理阶段
    // 把 panic 抛回 unload 任务，用来验证 settle/supervisor 确实接住了清理 panic。
    app.store.set_log_sink(Arc::new(|level, _name, message| {
        if level == Level::Error && message.contains("disposer") {
            panic!("测试用日志通道 panic");
        }
    }));

    let provider = plugin_fn(
        "panic-cleanup-provider",
        Inject::default(),
        |ctx: Ctx, _cfg| {
            Box::pin(async move {
                ctx.effect(|_| Disposable::sync(|| panic!("disposer 内爆")));
                Ok(())
            })
        },
    );
    let provider = app.plugin(provider, Config::Null).await.unwrap();

    tokio::time::timeout(Duration::from_secs(2), provider.dispose())
        .await
        .expect("清理 panic 不得让 dispose 永久挂起");
    assert_eq!(provider.state(), FiberState::Disposed);
    assert_eq!(provider.effect_count(), 0, "兜底排水仍要清空恢复栈");
    let cleanup_error = provider.cleanup_error().expect("必须记录清理异常");
    assert!(
        cleanup_error.contains("清理任务 panic"),
        "诊断要指向真正的异常阶段：{cleanup_error}"
    );
}
