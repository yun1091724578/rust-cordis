//! M5-1：**原生插件路径的失败面**（`core/tests/failure_modes.rs`）。
//!
//! 两条规格：
//! 1. `apply` panic ⇒ fiber 收敛到 `FAILED` 并留下可读错误（而不是无声挂在 `LOADING`）；
//! 2. P2-13 的**成文断言**：在飞的 apply 不被抢占——tracked effect 等价、emission 不等价。
//!    上游 JS 在每个 yield 点检查 epoch，被抢占就中止当前 apply；Rust 里做不到抢占一个
//!    正在 await 的异步函数，所以这条偏离必须被钉成可执行的断言，而不是留一句注释。

use cordis_core::{plugin_fn, App, BoxFuture, Config, Ctx, Disposable, FiberState, Inject, Result};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 带超时的轮询：把"护栏生效了"与"状态机卡死"区分开（后者会红在超时消息上）。
async fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while !condition() {
        assert!(Instant::now() < deadline, "等 {what} 超时");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn native_apply_panic_fails_fiber_not_hangs() {
    let app = App::new();
    let plugin = plugin_fn("panicky", Inject::default(), |_ctx: Ctx, _cfg: Config| {
        let future: BoxFuture<Result<()>> = Box::pin(async { panic!("apply 内爆了") });
        future
    });
    let fiber = app.root.plugin(plugin, Config::Null);

    // 有护栏 ⇒ FAILED；没有护栏 ⇒ 永远停在 LOADING：panic 被 `tokio::spawn` 吞进
    // `JoinError` 后没人再把状态机推下去（"失败"于是变成了"静默消失"）。
    wait_until("apply panic 后收敛到 FAILED", || {
        fiber.state() == FiberState::Failed
    })
    .await;
    let error = fiber.error().expect("失败的 fiber 必须留下可读错误");
    assert!(error.contains("panic"), "{error}");
}

/// `apply` 的**同步段**也可能 panic（`FnPlugin` 直接调用闭包，future 还没创建）。
/// 护栏必须把这次调用本身包住，否则 panic 会从 `settle()` 逃出去、fiber 停在 LOADING。
#[tokio::test]
async fn synchronous_apply_panic_fails_fiber_not_hangs() {
    let app = App::new();
    let plugin = plugin_fn(
        "sync-panicky",
        Inject::default(),
        |_ctx: Ctx, _cfg: Config| -> BoxFuture<Result<()>> {
            panic!("apply 在返回 future 之前爆了")
        },
    );
    let fiber = app.root.plugin(plugin, Config::Null);

    wait_until("同步 apply panic 后收敛到 FAILED", || {
        fiber.state() == FiberState::Failed
    })
    .await;
    let error = fiber.error().expect("失败的 fiber 必须留下可读错误");
    assert!(error.contains("panic"), "{error}");
}

/// P2-13：**在飞的 apply 不被抢占**——tracked effect 等价、emission 不等价。
///
/// 断言的两半：
/// - tracked 等价：跑完的 apply 注册的 effect 会被紧随的卸载**全部**回收（无残留）；
/// - emission 不等价：抢占点**之后**发出的 `emit` 依然会到达监听者（JS 里不会）。
#[tokio::test]
async fn preempted_apply_emits_then_leaves_no_tracked_residue() {
    let app = App::new();
    let emitted = Arc::new(AtomicUsize::new(0));
    let cleaned = Arc::new(AtomicUsize::new(0));

    let seen = emitted.clone();
    app.root.on("probe", move |_call| {
        seen.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { None })
    });

    let cleaned_in = cleaned.clone();
    let plugin = plugin_fn("slow", Inject::default(), move |ctx: Ctx, _cfg: Config| {
        let cleaned = cleaned_in.clone();
        Box::pin(async move {
            // 抢占窗口**之前**：注册一个被追踪的 effect。
            ctx.effect(move |_| {
                Disposable::sync(move || {
                    cleaned.fetch_add(1, Ordering::SeqCst);
                })
            });
            // 让出足够长时间，让测试在这个窗口里请求卸载。
            tokio::time::sleep(Duration::from_millis(50)).await;
            // 抢占点**之后**：JS 早已中止，Rust 仍然会把这个事件发出去。
            ctx.emit("probe", vec![]).await;
            Ok(())
        })
    });

    let fiber = app.root.plugin(plugin, Config::Null);
    wait_until("apply 注册 effect", || fiber.effect_count() == 1).await;

    // 在飞期间请求卸载：`dispose` 会在 driver 锁上等这次 apply 跑完。
    fiber.dispose().await;

    assert_eq!(
        cleaned.load(Ordering::SeqCst),
        1,
        "被抢占的 apply 注册的 effect 仍然会被回收"
    );
    assert_eq!(fiber.effect_count(), 0, "tracked effect 不得留下残留");
    assert_eq!(
        emitted.load(Ordering::SeqCst),
        1,
        "抢占点之后的 emit 依然到达监听者——emission 不等价（P2-13）"
    );
}
