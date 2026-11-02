//! 事件派发的语义规格：`isBailed` 的判定、五种调度的形态、`parallel` 的并发性。
//!
//! # 参考实现
//! `events.ts:6-8`：
//! ```ts
//! export function isBailed(value: any) {
//!   return value !== null && value !== false && value !== undefined
//! }
//! ```
//! 注意 `false` **明确不是** bail 值——一个"我不表态"或"否决但不短路"的钩子
//! 返回 `false` 时，派发必须继续往后走。
//!
//! `events.ts:93-98`：
//! ```ts
//! async parallel(...args) {
//!   const results = await Promise.allSettled(callbacks.map(async cb => ...))
//! }
//! ```
//! `parallel` 与 `emit` 的区别就在 `Promise.allSettled`——**它是并发的**。

use cordis_core::{is_bailed, App, BoxFuture, Call, Ctx, Dyn, ExternValue, Filter, Mode, Ret};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 造一个"立刻返回固定值"的 handler。
fn reply(value: Ret) -> impl Fn(Call) -> BoxFuture<Ret> + Send + Sync + 'static {
    move |_call: Call| {
        let value = value.clone();
        Box::pin(async move { value })
    }
}

// ---------------------------------------------------------------- isBailed

/// bail 值的判定必须与 `events.ts:6-8` 逐条一致。
#[test]
fn bail_values_match_cordis() {
    // JS 的 null / undefined → 不短路
    assert!(
        !is_bailed(&None),
        "None 对应 JS 的 null/undefined，不是 bail 值"
    );

    // ★ JS 的 false → **不短路**。这是最容易漏掉的一条：
    //   `Some(Dyn::new(false))` 必须和 `None` 一样继续往下派发。
    assert!(
        !is_bailed(&Some(Dyn::new(false))),
        "false 在 JS 里不是 bail 值（events.ts:7），必须与 None 同等对待"
    );

    // 其余任何值（含 0、空串、unit）都是 bail 值。
    assert!(is_bailed(&Some(Dyn::new(true))));
    assert!(is_bailed(&Some(Dyn::new(()))));
    assert!(
        is_bailed(&Some(Dyn::new(0u32))),
        "0 是 bail 值（JS 只排除 null/false/undefined）"
    );
    assert!(is_bailed(&Some(Dyn::new(String::new()))), "空串是 bail 值");
}

/// 端到端：一个返回 `false` 的 handler **不能**截断 `bail` 派发。
///
/// 这是"权限钩子返回 false 表示不表态"的典型场景——JS 里会继续问下一个 handler，
/// 最终拿到后者的值。若把 `false` 误判为 bail 值，这里会返回 `false` 而漏掉真正的结论。
#[tokio::test]
async fn a_false_result_does_not_short_circuit_bail_dispatch() {
    let app = App::new();
    let calls = Arc::new(AtomicUsize::new(0));

    let seen = calls.clone();
    let _abstain = app.root.on("perm", move |_call: Call| {
        let seen = seen.clone();
        Box::pin(async move {
            seen.fetch_add(1, Ordering::SeqCst);
            Some(Dyn::new(false)) // 明确"不表态"
        })
    });

    let seen = calls.clone();
    let _decide = app.root.on("perm", move |_call: Call| {
        let seen = seen.clone();
        Box::pin(async move {
            seen.fetch_add(1, Ordering::SeqCst);
            Some(Dyn::new(String::from("second")))
        })
    });

    let result = app
        .store
        .dispatch(Mode::Bail, "perm", &app.root, Vec::new(), None, None)
        .await;

    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "第一个 handler 返回 false 不应截断派发"
    );
    assert_eq!(
        result
            .and_then(|v| v.downcast::<String>())
            .map(|s| (*s).clone()),
        Some(String::from("second")),
        "应当拿到第二个 handler 的结论，而不是那个 false"
    );
}

/// 同一个规格在 `serial` 上同样成立。
#[tokio::test]
async fn a_false_result_does_not_short_circuit_serial_dispatch() {
    let app = App::new();
    let _first = app.root.on("perm", reply(Some(Dyn::new(false))));
    let _second = app.root.on("perm", reply(Some(Dyn::new(42u32))));

    let result = app
        .store
        .dispatch(Mode::Serial, "perm", &app.root, Vec::new(), None, None)
        .await;

    assert_eq!(
        result.and_then(|v| v.downcast::<u32>()).map(|v| *v),
        Some(42)
    );
}

// ---------------------------------------------------------------- parallel

/// `parallel` 必须**并发**执行（`events.ts:95` 的 `Promise.allSettled`）。
///
/// 4 个各睡 100ms 的 handler：并发约 100ms，串行约 400ms。阈值取 250ms，
/// 两侧都留了 2 倍以上的余量。
#[tokio::test]
async fn parallel_runs_handlers_concurrently_and_waits_for_all() {
    let app = App::new();
    let completed = Arc::new(AtomicUsize::new(0));

    for _ in 0..4 {
        let completed = completed.clone();
        let _hook = app.root.on("tick", move |_call: Call| {
            let completed = completed.clone();
            Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(100)).await;
                completed.fetch_add(1, Ordering::SeqCst);
                None
            })
        });
    }

    let started = Instant::now();
    app.store
        .dispatch(Mode::Parallel, "tick", &app.root, Vec::new(), None, None)
        .await;
    let elapsed = started.elapsed();

    assert_eq!(
        completed.load(Ordering::SeqCst),
        4,
        "parallel 必须等所有 handler 跑完"
    );
    assert!(
        elapsed < Duration::from_millis(250),
        "parallel 必须是并发的：4 个各 100ms 的 handler 串行要 400ms，实测 {elapsed:?}"
    );
}

/// 并发不能靠"丢掉慢的"实现：慢 handler 的副作用必须完整落地。
///
/// 先完成的 handler 不能导致派发提前返回（对应 `allSettled` 的"等全部"语义）。
#[tokio::test]
async fn parallel_does_not_return_before_the_slowest_handler_finishes() {
    let app = App::new();
    let slow_done = Arc::new(AtomicUsize::new(0));

    let _fast = app.root.on("tick", reply(None));

    let done = slow_done.clone();
    let _slow = app.root.on("tick", move |_call: Call| {
        let done = done.clone();
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(120)).await;
            done.fetch_add(1, Ordering::SeqCst);
            None
        })
    });

    app.store
        .dispatch(Mode::Parallel, "tick", &app.root, Vec::new(), None, None)
        .await;

    assert_eq!(
        slow_done.load(Ordering::SeqCst),
        1,
        "派发返回时，最慢的 handler 也应当已经跑完"
    );
}

// ---------------------------------------------------------------- 跨 ABI 的 bail 值

/// M1-3（红→绿）：`ExternValue(Bool(false))` 与原生 `false` 同义——**不是** bail 值。
///
/// 背景：ABI 适配层把插件返回的 JSON 包进 `ExternValue`（`host/lib.rs` 的 `host_ctx_on`），
/// 而 `is_bailed` 只认原生 `bool` ⇒ 跨 ABI 的 `false` 会被误判为短路值。这与
/// `events.ts:7`（`value !== false`）冲突，也让 ABI 插件无法表达"不表态"。
#[test]
fn extern_false_is_not_a_bail_value() {
    assert!(
        !is_bailed(&Some(Dyn::new(ExternValue(json!(false))))),
        "跨 ABI 的 JSON false 必须与原生 false 同义（不短路）"
    );
    assert!(
        !is_bailed(&Some(Dyn::new(ExternValue(json!(null))))),
        "跨 ABI 的 JSON null 与 JS 的 null 同义（不短路）"
    );
    // 其余 JSON 值都是 bail 值（与 JS 只排除 null/false/undefined 一致）。
    assert!(is_bailed(&Some(Dyn::new(ExternValue(json!(0))))));
    assert!(is_bailed(&Some(Dyn::new(ExternValue(json!(""))))));
    assert!(is_bailed(&Some(Dyn::new(ExternValue(json!(true))))));
}

/// M1-3（红→绿）：端到端——一个返回跨 ABI `false` 的监听器**不能**截断派发。
#[tokio::test]
async fn abi_false_does_not_short_circuit_bail() {
    let app = App::new();
    let calls = Arc::new(AtomicUsize::new(0));

    let seen = calls.clone();
    let _abstain = app.root.on("perm-abi", move |_call: Call| {
        let seen = seen.clone();
        Box::pin(async move {
            seen.fetch_add(1, Ordering::SeqCst);
            Some(Dyn::new(ExternValue(json!(false)))) // ABI 插件的"不表态"
        })
    });
    let seen = calls.clone();
    let _decide = app.root.on("perm-abi", move |_call: Call| {
        let seen = seen.clone();
        Box::pin(async move {
            seen.fetch_add(1, Ordering::SeqCst);
            Some(Dyn::new(ExternValue(json!("second"))))
        })
    });

    let result = app
        .store
        .dispatch(Mode::Bail, "perm-abi", &app.root, Vec::new(), None, None)
        .await;

    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "跨 ABI 的 false 不应截断派发"
    );
    let value = result
        .and_then(|value| value.downcast::<ExternValue>())
        .map(|value| value.0.clone());
    assert_eq!(value, Some(json!("second")), "应当拿到第二个监听器的结论");
}

/// `handlers()` 必须在调用用户 filter 前释放 Hook 表锁。
///
/// filter 里查询 `hook_count` 会再次锁同一张表；持锁调用就是自死锁。
/// 用独立线程 + `recv_timeout` 把死锁变成可失败的断言。
#[test]
fn filter_predicate_can_query_hook_count_without_deadlock() {
    let app = App::new();
    app.root
        .on("lock-probe", |_call: Call| Box::pin(async { None }));

    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let store = app.store.clone();
    let ctx = app.root.clone();
    let filter: Filter = Arc::new(|hook_ctx: &Ctx| {
        let _ = hook_ctx.hook_count("lock-probe");
        true
    });

    let thread = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async move {
            store
                .dispatch(
                    Mode::Emit,
                    "lock-probe",
                    &ctx,
                    Vec::new(),
                    None,
                    Some(&filter),
                )
                .await;
        });
        let _ = done_tx.send(());
    });

    assert!(
        done_rx.recv_timeout(Duration::from_secs(1)).is_ok(),
        "filter 调用 hook_count 时发生自死锁（Hook 表锁未释放）"
    );
    thread.join().expect("filter 线程不应 panic");
}
