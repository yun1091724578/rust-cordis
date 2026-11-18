//! 真实 ABI 消费者：**开发准入路径**（`set_allow_unverified(true)`，无策略）。
//!
//! 与 `abi_service_policy.rs` 分成两个测试二进制：准入模式是进程级全局量，
//! 两条路径同进程跑会互相覆盖——拆开后每条路径都有独立的进程与独立的复位责任。
//! 两个文件跑的是**同一份业务断言**（[`assert_composed_service_chain`]），
//! 区别只在准入配置。

mod common;

use common::{assert_composed_service_chain, composed_config, Admission, Sandbox, GATE};
use cordis_core::FiberState;
use cordis_host::{loaded_instances, staged_leaked};
use serde_json::json;

#[tokio::test]
async fn real_abi_plugins_compose_services_and_close() {
    let _gate = GATE.lock().await;
    let mut sandbox = Sandbox::new("dev");
    let _admission = Admission::dev();
    sandbox.write_config(&composed_config("consumer-probe", 2));

    sandbox
        .load(true)
        .await
        .expect("real ABI service composition must load");

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
        loaded_instances() >= 2,
        "两个 ABI 插件都必须有实例（当前 {}）",
        loaded_instances()
    );
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

    assert_composed_service_chain(sandbox.app(), "consumer-probe", 2);

    sandbox.close().await;
    assert_eq!(loaded_instances(), 0, "close must release ABI instances");
    assert_eq!(staged_leaked(), 0, "暂存副本必须全部被认领/回收");
    assert!(
        sandbox.leftovers().is_empty(),
        "插件目录不得残留暂存副本: {:?}",
        sandbox.leftovers()
    );
    sandbox.assert_removed();
}
