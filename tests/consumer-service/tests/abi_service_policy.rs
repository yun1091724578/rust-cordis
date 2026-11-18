//! 真实 ABI 消费者：**生产准入路径**（显式 allow-list + sidecar manifest）。
//!
//! 进程级隔离：本文件是一个独立测试二进制，来源策略不会跨文件串门；文件内若将来
//! 增加用例，由 [`GATE`] 串行。准入与 `allow_unverified` 的复位由
//! [`Admission`] 的 `Drop` 保证（panic 也复位）。
//!
//! `cargo xtask consumer` 会构建 greeting/hello 两个 cdylib 并设置
//! `CORDIS_CONSUMER_PLUGIN_DIR`；直接 `cargo test` 缺少该环境变量不是受支持路径。
//!
//! 断言的是**业务结果**，不是"名字挂上了"：`hello` 真的跨 ABI 调用了 `greeting`
//! 的服务方法与接口方法，值由配置里注入的探针串驱动。

mod common;

use common::{assert_composed_service_chain, composed_config, Admission, Sandbox, GATE};
use cordis_core::FiberState;
use cordis_host::{loaded_instances, staged_leaked};
use serde_json::json;

#[tokio::test]
async fn real_abi_plugins_pass_source_policy_and_compose() {
    let _gate = GATE.lock().await;
    let mut sandbox = Sandbox::new("policy");
    let _admission = Admission::enforce(sandbox.allowed());
    sandbox.write_config(&composed_config("consumer-probe", 2));

    sandbox
        .load(true)
        .await
        .expect("source-policy ABI service composition must load");

    // 1) 两行都真的被拉起来了（不是"名字在表里"）。
    let state = sandbox.state();
    assert_eq!(state.mounted_labels(), ["greeting", "hello"]);
    for label in ["greeting", "hello"] {
        let fiber = state
            .fiber_of(label)
            .unwrap_or_else(|| panic!("{label} 必须有挂载 fiber"));
        assert_eq!(
            fiber.state(),
            FiberState::Active,
            "{label} 必须已激活（inject 已满足）"
        );
    }
    assert!(
        state.pending_report().is_empty(),
        "全部就绪时不该有未就绪行: {:?}",
        state.pending_report()
    );
    assert_eq!(
        state.report()["failed"],
        json!([]),
        "装载报告不得有逐行失败: {}",
        state.report()
    );

    // 2) 业务断言：hello.reply / greeting.text / 负向解析。
    assert_composed_service_chain(sandbox.app(), "consumer-probe", 2);

    // 3) 关闭后资源恢复。
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
