//! 运行时 `provide` / `inject` 形成的服务依赖环必须在建边时被拒绝。
//!
//! 声明级环由 loader 报告；这里覆盖的是两个已激活 fiber 在运行期新增 inject 时
//! 形成的环。若允许插入，卸载会变成“双方互相等待 settle”的死锁。

use cordis_core::{plugin_fn, App, Config, Ctx, Dyn, FiberState, Inject, Level, PendingReason};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[tokio::test]
async fn runtime_service_cycle_is_rejected_before_it_can_deadlock() {
    let app = App::new();
    let errors = Arc::new(Mutex::new(Vec::<String>::new()));
    let captured = errors.clone();
    app.store
        .set_log_sink(Arc::new(move |level, _name, message| {
            if level == Level::Error && message.contains("运行时服务依赖环") {
                captured.lock().unwrap().push(message.to_string());
            }
        }));

    // B 先提供 Y，不声明 inject。
    let b = app
        .plugin(
            plugin_fn("cycle-b", Inject::default(), |ctx: Ctx, _cfg| {
                Box::pin(async move {
                    ctx.provide("cycle-y", Dyn::new(2u32))?;
                    Ok(())
                })
            }),
            Config::Null,
        )
        .await
        .unwrap();
    assert_eq!(b.state(), FiberState::Active);

    // A 依赖 Y，并提供 X：此时 A -> B 是唯一一条依赖边。
    let a = app
        .plugin(
            plugin_fn("cycle-a", Inject::of(&["cycle-y"]), |ctx: Ctx, _cfg| {
                Box::pin(async move {
                    ctx.provide("cycle-x", Dyn::new(1u32))?;
                    Ok(())
                })
            }),
            Config::Null,
        )
        .await
        .unwrap();
    assert_eq!(a.state(), FiberState::Active);

    // 运行期给 B 增加 X 依赖：B -> A 与已有的 A -> B 会形成环。
    b.add_inject("cycle-x");
    b.check_impl("cycle-x");

    assert!(
        b.dep("cycle-x").is_none(),
        "形成环的依赖边必须被拒绝，而不是插入后等待死锁"
    );
    let errors = errors.lock().unwrap().clone();
    assert!(
        errors
            .iter()
            .any(|message| message.contains("cycle-b") && message.contains("cycle-x")),
        "必须留下可定位的环诊断：{errors:?}"
    );

    // 拒绝后 B 仍应能正常收敛（因为 inject 未满足而退回 Pending），
    // 且卸载不会出现互相等待。
    b.refresh_desired();
    let pending = b.pending_injects();
    assert!(
        pending
            .iter()
            .any(|item| item.key == "cycle-x" && item.reason == PendingReason::Cycle),
        "环拒绝必须出现在 Pending 诊断里：{pending:?}"
    );
    tokio::time::timeout(Duration::from_secs(2), b.settle())
        .await
        .expect("拒绝环后 B 的卸载不得死锁");
    tokio::time::timeout(Duration::from_secs(2), a.settle())
        .await
        .expect("B 卸载后 A 也必须能收敛");
    assert_ne!(b.state(), FiberState::Active);
    assert_ne!(a.state(), FiberState::Active);

    tokio::time::timeout(Duration::from_secs(2), b.dispose())
        .await
        .expect("B dispose 不得死锁");
    tokio::time::timeout(Duration::from_secs(2), a.dispose())
        .await
        .expect("A dispose 不得死锁");
}
