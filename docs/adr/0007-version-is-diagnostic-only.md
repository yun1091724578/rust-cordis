# ADR 0007：版本字段只做诊断，不做运行时门禁

- **状态**：已接受
- **日期**：2026-11-13
- **依据**：`docs/adr/0006-public-api-stability.md`、`crates/core/tests/plugin_version.rs`、`crates/host/tests/embedding_contract.rs`

## 1. 决策

`Plugin::version()` 是**展示/诊断元数据**：

- 不参与服务解析、依赖注入、挂载准入；
- 不参与 ABI 版本匹配；
- 不参与来源策略或授权；
- 不因为两个插件的 `Plugin::version` 不同而拒绝组合；
- CLI `inspect` 展示它，但明确标注能力来源是插件自述。

sidecar `PluginManifest.version` 保持现有角色：

- 是 manifest 的身份/诊断字段；
- 在依赖计划里作为 `PlanEntry.version` 参与**重复身份**判断；
- 不替代 ABI 匹配，也不推导 semver range；
- 没有 sidecar 的 ABI 插件继续没有版本。

`PluginDesc` 在冻结的 ABI v1 中没有 semver 字段；本轮不追加尾字段。

## 2. 不做什么

- 不新增 `abi_min/abi_max`。
- 不因版本不同自动重试、回退或升级。
- 不把 `Plugin::version` 写进依赖求解器。
- 不在没有真实消费者时引入版本冲突诊断或迁移计划。

## 3. 验收

- `plugin_version_is_optional_and_explicit`：默认 None，wrapper 保留版本。
- `capabilities_expose_optional_plugin_version`：诊断面可读。
- `plugin_versions_do_not_gate_service_composition`：provider/consumer 版本不同仍能完成服务组合。
- `inspect` 文本/JSON 只展示，不把它写成审批或 ABI 事实。
