# ADR 0006：公开 API 稳定与 0.2 迁移边界

- **状态**：已接受（0.1 约束）；0.2 暂缓（2026-11-15）
- **日期**：2026-11-13
- **依据**：`docs/api-inventory.md`、`docs/support-contract.md`、`crates/host/tests/embedding_contract.rs`

## 1. 背景

`rust-cordis` 是插件式服务运行时 / SDK，不是 agent 产品，也不是网络微服务框架。
嵌入方需要稳定的公开构造入口，但 0.1 已经暴露了若干配置 DTO 的公开字段。
直接加 `#[non_exhaustive]` 或 builder 会让现有字面量构造在无迁移说明的情况下失效。

## 2. 0.1 决策

- `DispatchPolicy`、`HostLimits` 继续允许公开字面量构造。
- 为需要校验的配置增加**最小构造函数**，而不是 builder：
  - `LoaderConfig::new(path)`
  - `HostLimits::new(instances, dispatches, pending, streams) -> Result<Self, String>`
  - `DispatchPolicy::new(timeout_ms, outbound_timeout_ms)`
- `Capabilities` 已有私有 `schema` / `schema_error` 字段，外部本来就不能字面量构造；
  不再额外加 opaque 标记。
- `crates/host/tests/embedding_contract.rs` 故意固定 0.1 字面量构造契约：
  未来加 `#[non_exhaustive]` 会先让测试变红。
- 公开面以 `cargo xtask inventory --check` 为门禁；新增/删除受控入口必须同步
  `docs/api-inventory.md`。

## 3. 0.2 评估规则

不一次性把所有公开结构 opaque 化。逐类型判断：

1. **配置 DTO**（可序列化、可被嵌入方构造）：优先保留字面量构造；只增加校验构造函数。
2. **运行时句柄**（`LoadedPlugin`、`StagedFile`、loader state）：保持不透明。
3. **返回型诊断**（`Capabilities`）：字段只读即可；不承诺外部构造。
4. 只有当真实消费者需要“新增字段不破坏源码”时，才引入 `#[non_exhaustive]` 或 builder，
   并同时提供 0.1 → 0.2 迁移说明。

## 4. 0.2 暂缓决定

2026-11-15 决定：**当前不启动 0.2 breaking 变更**，0.1.x 继续保留字面量构造。

启动 0.2 的条件（至少一个，且有真实消费者）：

1. 外部消费者需要“新增公开字段但不破坏源码”，且构造函数无法满足；
2. 需要 builder 才能表达的必填/互斥配置组合，且已有两个以上消费者重复痛感；
3. 某个公开类型的字段语义必须收窄，无法通过新增构造函数兼容。

启动时必须同时提供：

- 0.1 → 0.2 迁移说明；
- `trybuild`/compile-fail 或等价契约测试；
- 至少一个真实消费者完成迁移；
- 发布门禁与回滚说明。

在此之前，不引入 `#[non_exhaustive]`、builder 或 0.2 版本号。

## 5. 不在本 ADR 范围

- ABI 前缀/结构变更；ABI 仍精确匹配。
- agent/model/SSE 协议；它们是仓外消费者证据，不进入核心公开面。
- 网络微服务、远程协议、多租户安全底座。

## 6. 验收

- `cargo xtask inventory --check` 通过；
- `cargo xtask docs` 通过；
- `embedding_contract` 的公开构造/服务组合测试通过；
- 0.1 不因 `#[non_exhaustive]` 或 builder 产生静默破坏。
