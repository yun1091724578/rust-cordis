//! 真实 ABI 消费者的**失败注入**：未就绪、准入拒绝、值真的沿服务链流动。
//!
//! 同一个二进制里的三个用例共享进程级准入全局量，所以每个用例都先拿 [`GATE`]
//! （`tokio::sync::Mutex`，跨 await 持有），并用 [`Admission`] 的 `Drop` 复位。
//!
//! 这些用例对实现是**可证伪**的：
//! - 去掉 `hello` 的 `inject: ["greeting"]`（或让 greeting 自动就绪）⇒ 用例 1 红；
//! - 让装载器跳过来源准入 ⇒ 用例 2 红（空 allow-list 居然装上了）；
//! - 让 `hello.reply.message` 写死成常量 ⇒ 用例 3 红。

mod common;

use common::{
    assert_composed_service_chain, composed_config, extern_value, Admission, Sandbox, GATE,
};
use cordis_core::{ExternValue, FiberState};
use cordis_host::loader::{reload_one, ReloadOutcome};
use cordis_host::{loaded_instances, staged_leaked};
use serde_json::json;

/// 只配 `hello`：它声明了 `inject: ["greeting"]`，于是停在 Pending、不产出服务值。
#[tokio::test]
async fn hello_without_greeting_stays_pending_and_reports() {
    let _gate = GATE.lock().await;
    let mut sandbox = Sandbox::new("pending");
    let _admission = Admission::dev();
    sandbox.write_config("- id: hello\n  name: hello\n  config:\n    times: 2\n");

    sandbox
        .load(true)
        .await
        .expect("缺一个提供者不该让整个装载失败");

    let state = sandbox.state();
    assert_eq!(state.mounted_labels(), ["hello"], "行照旧挂上，只是没就绪");
    let pending = state.pending_report();
    assert_eq!(
        pending.len(),
        1,
        "hello 缺 greeting 必须被报为未就绪: {pending:?}"
    );
    assert_eq!(pending[0].label, "hello");
    assert!(
        pending[0].missing.iter().any(|item| item.key == "greeting"),
        "缺的必须精确报成 greeting 注入: {:?}",
        pending[0].missing
    );
    let fiber = state.fiber_of("hello").expect("hello 有挂载 fiber");
    let fiber_state = fiber.state();
    // 立刻放掉 fiber 的强引用：它持有插件 `Arc`（进而持有暂存副本），
    // 留到 `close()` 之后会让"没有残留"的断言假红。
    drop(fiber);
    assert_eq!(
        fiber_state,
        FiberState::Pending,
        "inject 未满足的行必须停在 Pending（不是 Active/Failed）"
    );
    assert!(
        sandbox
            .app()
            .root
            .require_as::<ExternValue>("hello.reply")
            .is_err(),
        "未激活的 hello 不得产出服务值"
    );
    assert_eq!(
        state.report()["failed"],
        json!([]),
        "Pending 不是失败：报告里不能出现逐行失败: {}",
        state.report()
    );

    sandbox.close().await;
    assert_eq!(loaded_instances(), 0, "关闭必须释放全部 ABI 实例");
    assert_eq!(staged_leaked(), 0, "暂存副本必须全部被认领/回收");
    assert!(
        sandbox.leftovers().is_empty(),
        "插件目录不得残留暂存副本: {:?}",
        sandbox.leftovers()
    );
    sandbox.assert_removed();
}

/// 空 allow-list：严格模式下装载整体失败；即使降级，也一行都挂不上。
#[tokio::test]
async fn empty_allow_list_rejects_loading() {
    let _gate = GATE.lock().await;
    let _admission = Admission::enforce(Vec::new());

    // ① fail loud：默认 strict ⇒ 装载器直接返回错误。
    let mut strict = Sandbox::new("admission-strict");
    strict.write_config(&composed_config("consumer-probe", 2));
    let err = strict
        .load(true)
        .await
        .expect_err("空 allow-list 必须拒绝装载（strict 模式）");
    assert!(
        err.contains("allow-list"),
        "拒绝理由必须指向来源准入，而不是别的失败: {err}"
    );
    assert_eq!(loaded_instances(), 0, "被准入拒绝的插件不得留下实例");
    assert_eq!(
        staged_leaked(),
        0,
        "准入拒绝发生在暂存之前，不得留下暂存副本"
    );
    assert!(
        strict.leftovers().is_empty(),
        "插件目录不得残留暂存副本: {:?}",
        strict.leftovers()
    );
    strict.close().await;
    strict.assert_removed();

    // ② 降级路径：strict=false 只把失败降级成报告，挂载表仍然是空的。
    let mut lenient = Sandbox::new("admission-lenient");
    lenient.write_config(&composed_config("consumer-probe", 2));
    lenient
        .load(false)
        .await
        .expect("strict=false 时逐行失败降级为报告");
    let state = lenient.state();
    assert!(
        state.mounted_labels().is_empty(),
        "准入拒绝的行一行都不该挂上: {:?}",
        state.mounted_labels()
    );
    let failed = state.report()["failed"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        failed
            .iter()
            .any(|msg| msg.as_str().unwrap_or("").contains("allow-list")),
        "报告必须逐行给出准入拒绝理由: {}",
        state.report()
    );
    assert_eq!(loaded_instances(), 0, "被准入拒绝的插件不得留下实例");
    assert_eq!(staged_leaked(), 0, "准入拒绝不得留下暂存副本");
    assert!(
        lenient.leftovers().is_empty(),
        "插件目录不得残留暂存副本: {:?}",
        lenient.leftovers()
    );

    lenient.close().await;
    assert_eq!(loaded_instances(), 0);
    assert_eq!(staged_leaked(), 0);
    lenient.assert_removed();
}

/// 改 `greeting` 的 `config.text` ⇒ `hello.reply.message` 随之变化。
///
/// 这条用例把"值真的从服务链来"钉死：如果 `hello` 读的是写死的默认值、或者
/// 新值没有真的穿过 greeting 服务的边界，第二次断言就红。
///
/// 现状：一次对账只等被驱动的那一行 settle，**不等**依赖者的重新 settle——内核在
/// 提供者 effect 重启后是 `spawn(fiber.settle())`、不 await 依赖者。所以换值之后
/// `hello` 是否已经回来取决于浮空调度，是个竞态窗口，不是稳定状态。这里用公开的
/// 确定性驱动 [`reload_one`] 把 `hello` 重新挂起来，再断言它读到的是**新值**。
///
/// 守卫：本条用例依赖"依赖者的自动重挂与对账不同步、需要显式驱动才有确定性"这一
/// 内核现状。`reload_one` 现在的价值是**消除竞态**；如果将来内核改成对账内 await
/// 依赖者 settle，这段驱动会退化成空操作（用例仍是绿的，但不再验证任何东西）——
/// 届时去掉它、直接断言新值即可。
#[tokio::test]
async fn hello_reply_follows_greeting_service_value() {
    let _gate = GATE.lock().await;
    let mut sandbox = Sandbox::new("flow");
    let _admission = Admission::dev();
    sandbox.write_config(&composed_config("first-probe", 2));

    sandbox
        .load(true)
        .await
        .expect("first-probe composition must load");
    assert_composed_service_chain(sandbox.app(), "first-probe", 2);
    let before = extern_value(sandbox.app(), "hello.reply")["message"].clone();
    assert_eq!(before, json!("first-probe × 2"));

    // 改配置 → 一次确定性 reload（不依赖墙钟轮询）。
    sandbox.write_config(&composed_config("second-probe", 2));
    match sandbox.state().reload().await {
        ReloadOutcome::Reconciled(outcome) => assert!(
            outcome.failures.is_empty(),
            "reload 不得有逐行失败: {:?}",
            outcome.failures
        ),
        other => panic!("reload 必须真的被采纳（不是 Coalesced/Unsettled）: {other:?}"),
    }
    assert_eq!(
        extern_value(sandbox.app(), "greeting.text")["text"],
        json!("second-probe"),
        "greeting 服务必须换成新配置的值"
    );

    // `ctx` 是根上下文的克隆，它连同订阅者一起吊住插件 `Arc`（进而吊住暂存副本）。
    // 收进块作用域：刷新完立刻释放，别让它活到 close/drop/目录断言之后。
    {
        let ctx = sandbox.app().root.clone();
        reload_one(&ctx, sandbox.state(), "hello")
            .await
            .expect("hello 必须能被重新挂起来");
    }

    let after = extern_value(sandbox.app(), "hello.reply")["message"].clone();
    assert_eq!(
        after,
        json!("second-probe × 2"),
        "hello.reply 必须跟着 greeting 服务的值变化（值来自服务链，不是常量）"
    );
    assert_ne!(before, after, "配置变了，消费者看到的值必须变");
    assert!(
        sandbox.state().pending_report().is_empty(),
        "换值后仍应全部就绪: {:?}",
        sandbox.state().pending_report()
    );
    assert_eq!(
        sandbox.state().fiber_of("hello").map(|fiber| fiber.state()),
        Some(FiberState::Active),
        "重挂后的 hello 必须真的激活"
    );

    sandbox.close().await;
    assert_eq!(loaded_instances(), 0, "关闭必须释放全部 ABI 实例");
    assert_eq!(staged_leaked(), 0, "暂存副本必须全部被认领/回收");
    assert!(
        sandbox.leftovers().is_empty(),
        "插件目录不得残留暂存副本: {:?}",
        sandbox.leftovers()
    );
    sandbox.assert_removed();
}
