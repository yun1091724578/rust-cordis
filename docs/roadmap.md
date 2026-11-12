# 分阶段执行计划

本文件是唯一执行计划入口：当前判断、下一步顺序、阶段出口与证据索引都以这里为准。
根目录的设计文档保留设计背景，不作为当前状态来源。

- 支持范围、非目标与保证索引：`docs/support-contract.md`
- 结果与错误契约：`docs/adr/0003-result-and-error-contract.md`、`docs/adr/0004-abi-error-evolution.md`
- 信任模型与插件来源：`docs/adr/0005-trust-model-and-plugin-source-boundary.md`
- 发布门禁与未确认项：`docs/release.md`
- 封存格式：`docs/release-seal.md`
- 公开 API / 载荷清单：`docs/api-inventory.md`
- 架构现实审查：`docs/architecture-review.md`
- 冻结 ABI v1：`tests/fixtures/abi-v1/README.md`

## 执行规则

- 代码项走 TDD；回归测试要有红档。没有红档只算“代码改过”。
- 文档项不走 TDD；“保证”行必须被 `cargo xtask docs` 校验到真实、未忽略的测试。
- 阶段结束时同时更新本文件、支持契约和发布门禁；只改代码不改状态视为未完成。
- “完成”只指验收出口满足，不等于交付物存在或局部测试通过。

## 当前判断（2026-11-09）

| 工作线 | 状态 | 已落地 | 未闭合 |
|---|---|---|---|
| A. 低层 ABI 与发布 | **基本完成** | 0.1.0/0.1.1 已发布；正式 `/2` 封存已从 tag 生成并归档；公开仓 CI、版本矩阵、SCM 一次实测 | 严格同版本封存证据、历史兼容可重复测试 |
| B. 信任模型 | **已接受并实现** | ADR 0005；来源 allow-list、默认拒绝、seal sidecar、inspect/status provenance | 来源治理不是执行隔离，不能升级成安全承诺 |
| C. 可信单租户底座 P0 | **按边界完成** | 来源策略；App 粒度 dispatch policy；宿主容量准入；生命周期清理不阻塞 tokio worker；P0-4 边界已记录 | per-plugin/method 明确不提供；永久 hang 属 T2 |
| D. 长期运行 P1 | **进行中** | 1M soak、RSS 预算、真实 `open_staged` soak、dhat（Linux nightly）、版本矩阵 CI、semver 依赖计划解析核心、显式 `LoaderConfig.dependencyPlan` 装载前预检 | manifest/`PluginDesc` 采集与 mount 期自述校验、缓冲池 |
| E. agent 流处理 | **原型完成** | 六模式状态机、9+2 单测、多消息快照、统一收尾 | 有界增量交付、真实模型、取消/预算 |
| F. T3 / 不可信插件 | **条件性待启动** | ADR 0001 预留位置 | 只有第三方/模型生成/多租户触发时另立 ADR |
| G. 维护治理 | **持续** | R4/R5/R6 已登记 | serde_yaml、宿主模块、锁/panic 审查 |

阶段基线：

- 支持契约 **26 行 / 17 条保证**
- API 清单 **23 常量 / 4 结构 / 41 字段 / 76 公开入口 / 32 载荷**
- 0.1.1 三平台 CI：`42332542433`、tag `42332542329`
- 公开仓 CI：`42605690959` success
- 版本矩阵：`42606045837` success
- nightly 小轮次 + dhat：`42606720459` success
- 正式 `/2` 封存：`dist/cordis-0.1.0-formal`、`dist/cordis-0.1.1-formal` 与 GitHub Release assets
- 仓库已公开：标准 GitHub-hosted Runner 不再消耗私有仓库 Actions 分钟

## 最近交付

### P0-3：容量与准入（2026-11-09，已交付）

- `HostLimits`（`maxInstancesPerApp` / `maxDispatchesPerInstance` /
  `maxPendingPerInstance` / `maxStreamsPerInstance`），按 App 注册表 + 进程级回落；
  `LoaderConfig.hostLimits` 在挂载前登记，实例注册时取快照。
- 实例准入与插入在同一把实例表锁内完成；四类派发记录插入统一经
  `insert_dispatch`，超限返回 `host_capacity` / `capacity` / `not_executed`，
  不插入记录、不执行插件代码。
- 默认 1024 / 1024 / 256 / 32；0 是非法配置，不是“无限”。
- 证据：`crates/host/tests/capacity.rs`（实例 / 派发 / 挂起 / 流四条拒绝路径与
  计数回基线）、`crates/host/src/loader.rs::loader_config_parses_host_limits_from_yaml`。
- 边界：这是宿主侧表项准入，不是每插件的内存/CPU 硬上限；硬上限仍属 T3。

## 下一步（按顺序）

### 1. P0-4：锁纪律（按边界关闭）

- 已审计 `call_inbound` 的 lifecycle 读锁跨调用：回调期间不持实例表/派发表锁。
- 已修：卸载清理等待 `std::RwLock` 写锁时改用 `spawn_blocking`，不再阻塞 tokio worker。
- 已测：回调内 `ctx_dispose_self`、阻塞回调 + 并发 dispose，均无死锁且实例归零。
- **接受为 T2 边界**：永久 hang 不可抢占；panic/锁中毒没有找到可达 trigger，因此不承诺不存在。
- 后续只在真实故障或 T3 需求出现时重开；不为此引入伪抢占线程池。

### 2. P0-2：App 粒度决策（关闭）

- **明确只提供 App 粒度**，不提供 per-plugin/per-method 配置。
- 同一 App 下多个 loader 的 `dispatchPolicy` / `hostLimits` 是后设置覆盖；不是每 loader 独立。
- 策略在实例注册时快照，运行期变更不追溯。
- 这不是临时实现遗漏，而是当前 T2 的产品边界；不要把它写成 per-plugin 保证。

### 3. P1-1：semver range 与依赖求解（初装/对账/reload 预检）

- `crates/host/src/plan.rs` 提供纯数据求解器：`PlanEntry` 的
  `(id, version)`、`provides`、`requires` range；选择最高满足版本，
  拒绝缺失、不可满足、重复身份和同版本多提供者。
- **已接线（显式配置）**：`LoaderConfig.dependencyPlan` 用 `PlanEntrySpec`
  声明 `id` / `version` / `provides` / `requires`；非空时 `load_with_source`
  在**任何挂载之前**解析并调用 `resolve`。缺失、不可满足或歧义都会让本次装载
  大声失败，不加载、不应用任何插件。
- **已接线（manifest 采集）**：`PluginManifest` 增加可选
  `provides` / `requires`（`#[serde(default)]`，旧 manifest 仍然合法）；
  `PluginSource::plan_entry` 缺省返回 `Ok(None)`，`DiskPluginSource` 与 `load`
  共用 `resolve_plugin_path` 读取 sidecar manifest。显式 `dependencyPlan` 为空时，
  装载器在创建 `LoaderState` / 调用 `source.load` 之前逐行收集并在挂载前求解；
  非空时显式计划是**覆盖**，manifest 计划既不读取也不合并。
  没有任何 manifest 声明时不做预检（既有配置零行为变化）。
- 测试：`crates/host/src/plan.rs` 的 `plan::tests`，
  `crates/host/tests/dependency_plan.rs`（假源计数器证明失败发生在 `load`/`apply`
  之前；覆盖、空计划、损坏 manifest、DiskPluginSource 的 sidecar 采集与坏 semver），
  `crates/host/tests/source_policy.rs`（准入接受可选依赖字段），
  以及 `crates/host/src/loader.rs` 的 `dependencyPlan` YAML 解析测试。
- **边界与仍待办**：
  - 初装、对账和 `reload_one` 前都会重跑预检；对账失败不采纳新树，只把问题
    放进 outcome。
  - 只收集**静态启用**的非 group 行；带 `disabled: !!js …` 的行不参与初装预检。
  - `PluginDesc` 自述仍不参与求解；manifest 声明只用于预检，不改变运行时
    `inject` / 服务可见性，也不证明插件真的提供声明的服务。
  - `cargo xtask seal` 会为三个示例插件写出 `provides` / `requires`；
    `seal --verify` 仍要求自身规范字节，手工修改 sidecar 会失败。
  - 没有版本升级 / 回溯求解：一个服务键仍只能解析到一个提供者。
- ABI 仍精确匹配；不要同时引入互相冲突的 `abi_min/abi_max` 规则。

### 4. P1-4：缓冲池化与性能（semver 之后）

- 去掉每次调用 16 KiB 临时分配，先 benchmark 再改。
- 线程池仍不能取消同步调用；若保留，只能作为故障隔离，必须证明超时后不卸载、不释放插件，线程泄漏要登记。

### 5. agent 有界增量交付

- 单事件/队列/累计文本/终止快照分别限大小。
- 慢消费者、sink 失败、无终止事件 EOF、超大事件、终止写入与取消竞争。
- 之后接一个真实模型适配器，再做取消/预算；SSE 在线 sink 最后接。

## 阶段 0：验证基线

**已完成：**

- 三平台 CI 与平台矩阵 17 条保证
- 支持契约、API 清单、冻结 ABI v1
- 资源基线 / 暂存台账 / 1M soak
- 正式 `/2` 封存（0.1.0/0.1.1）
- 公开仓库；Actions 不再按私有分钟计费

**仍未闭合：**

- 严格同版本封存证据：两个正式封存的 smoke 都由当前 0.1.1 工作区宿主执行，
  `host.source_sha256 != source.sha256`；不能写成 strict same-version。
- 历史发布版 × 新宿主仍缺可重复测试。
- nightly cron 首跑与 artifact 长期转存。

**出口：** `docs/release.md` 能逐条回答“哪个保证、哪个平台、哪次 run/commit”；
没有把结构检查或单次手工验证写成持续保证。

## 阶段 1：结果与错误语义

**完成：**

- `Returned / Failed / TimedOut / Cancelled / Disconnected`
- `NotExecuted / Reported / Unknown`
- 结构化错误载荷、旧 ABI 文本降级、bounded message
- 事件路径仍无返回协议，是已知例外

**边界：** 不承诺自动重试安全或 exactly-once；SDK 提供事实，不提供重试许可。

## 阶段 2：结构化错误与调用通道

**完成：**

- `error_format` 能力位、sidecar 载荷、host/SDK detailed 入口
- iface/dispatch/next 超时、卸载/取消、provider 业务错误
- 旧插件文本错误端到端

**未完成：**

- 异步 `fulfill_error`
- 所有同步失败路径的 detailed payload
- 真实旧宿主四格矩阵仍部分是桩

## 阶段 3：声明与装载诊断

**完成：**

- 配置 schema 可见性、Pending 精确原因、来源/注入声明
- inspect/status provenance、默认拒绝来源策略

**边界：** schema 只展示，不在宿主侧校验；`related` 是全局候选，不证明隔离域可见。

## 阶段 4：调用事实与重试决策

**条件性，未启动。** 只有真实消费者需要完整调用事实通道时才做。
事件接口不新增返回协议。

## 阶段 5：组件与宿主能力

**条件性，未启动。** 没有真实消费者和独立 ADR 不实现。
不把 realm/scope 当安全边界。

## 阶段 6：产品化与进程外载体

**条件性，未启动。** 远程协议、独立部署、独立故障域不属于 T2；
第三方/不可信插件需要 T3（WASM 或可 kill 的子进程）。

## 发布与证据

### A5 批次

| 项 | 状态 | 证据 / 边界 |
|---|---|---|
| A5-1 三平台 CI | 完成 | `41460414477`；17 × 4 矩阵 |
| A5-2 nightly soak | 进行中 | 1M Linux、RSS 预算、dhat；cron 首跑/长期转存待补 |
| A5-3 正式 `/2` 封存 | 封存完成，strict same-version 未满足 | `xtask seal --root`；`dist/cordis-0.1.0-formal`、`0.1.1-formal`；Release assets |
| A5-4 历史 × 新宿主 | 已验证一次，仍待验证 | 0.1.1 宿主加载 0.1.0 formal smoke；缺可重复测试 |
| A5-5 其余发布项 | 大部分完成 | 公开仓、元数据、SCM 一次实测；nightly 转存/SCM CI 重跑待补 |

### 当前门禁

```text
cargo xtask ci        # fmt / inventory / docs / matrix / clippy / tests / demo / e2e
cargo xtask soak      # 资源 soak + 真实 open_staged soak（不进入 ci）
cargo xtask seal ...  # 正式 /2 封存与 verify（不进入 ci）
```

公开仓三平台 CI 后，标准 Runner 免费；不要用“本地通过”替代远端结果。

## 接入与维护线

| 线 | 状态 | 下一步 |
|---|---|---|
| R1 仓外真实接入 | 31 篇样例 + 真实 DLL 链路完成 | 真实生产数据 / 长期维护反馈 |
| R2 交付入口 | 首发低层 ABI；typed SDK 不发布 | 需要 typed 门面时另立发布决策 |
| R3 调用通道契约 | 三入口 + 窄策略消费者 | 完整重试/补偿仍属阶段 4 |
| R4 serde_yaml | 待启动 | 真实配置语料、`!!js`、patch 回写、错误诊断 |
| R5 宿主模块 / 锁 | 待启动 | 按 instance/dispatch/stream/staging 分批迁移 |
| R6 证据维护 | 持续 | run/commit 绑定；不把检查器通过写成行为证明 |

## 现在不要启动

- T3 / WASM / 子进程：只有第三方、模型生成、多租户或硬抢占需求出现
- 有界线程池 + 超时放弃：不能取消同步调用，会留下执行中的插件线程
- 远程协议 / 多租户安全底座
- 为了统一形式新增事件返回协议
- 没有消费者和 ADR 的阶段 4/5/6 能力
- 把单次 SCM 实测或单次历史演练写成持续保证
