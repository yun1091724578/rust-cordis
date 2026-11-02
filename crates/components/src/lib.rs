//! cordis-components：标准组件库。
//!
//! 组件在这里就是**普通 cordis 插件**：`app.plugin(组件::plugin(), config)`。
//! 目前只有一个，但它是骨架里的关键便利设施：
//!
//! - [`registry`]（M7，G7 的最小版）：具名槽位注册表——**换适配器 ≠ 重启所有依赖者**。
//!
//! 明确**不做**的（方案 §0.3 的取代清单）：加权 broker（round-robin / least-loaded / drain）
//! 没有消费者——逐条判定见 `docs/adr/0002-broker.md`（真实制品的 10 条候选里 8 条无需求，
//! 另两条只到"单键原地换值"与"稳定句柄"，都已被 [`registry`] 覆盖）；
//! 日志面板 / 配置监视器 / 健康检查属于领域词汇，不进骨架。

pub mod registry;
