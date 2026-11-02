# 分阶段执行计划

本文件是唯一执行计划入口：当前判断、下一批顺序、逐阶段交付物 / 验收出口与验证记录都在这里。
根目录的设计文档（`方案.md`、`补全方案.md`、`SDK方案.md`、`原理.md` 等）保留设计与
历史交付背景，不作为当前阶段状态来源。

- 支持范围、非目标与保证索引：`docs/support-contract.md`
- 结果与错误决策：`docs/adr/0003-result-and-error-contract.md`、`docs/adr/0004-abi-error-evolution.md`
- 发布门禁与未确认项：`docs/release.md`
- 首发产物封存格式：`docs/release-seal.md`
- 公开面与载荷清单：`docs/api-inventory.md`
- 冻结 ABI v1 的 provenance：`tests/fixtures/abi-v1/README.md`

## 执行与维护规则

- **代码项走 TDD**：先写会失败的测试（red），再写最小实现（green），最后重构。
- **回归测试要有红档**：临时破坏实现，测试必须失败；还原后再绿。
  没有红档的回归测试只记录“代码改过”，不证明行为被钉住。
- **文档项不走 TDD**，但它引用的测试名由 `cargo xtask docs` 校验：
  “保证”行必须指向存在、带 `#[test]` / `#[tokio::test]` 且未 `#[ignore]` 的测试。
- 每个阶段或批次结束时，同时更新本文件、`docs/support-contract.md` 的索引状态和
  `docs/release.md` 的对应门禁；只改代码不改状态，视为未完成。
- “完成”只指**验收出口满足**。交付物存在、局部测试通过、或表格里写“部分完成”，
  都不等于阶段完成。

## 总览（2026-11-02）

| 阶段 | 当前判断 | 已落地证据 | 未闭合 / 下一出口 |
|---|---|---|---|
| 0：验证基线 | 交付物完成，出口未满足 | 支持契约 25 行 / 17 条保证；API 清单；冻结 ABI v1 夹具；资源基线；`.\cargo.cmd xtask ci` 在 Windows GNU 全量通过；**三平台 CI 首跑通过**（[run 41460414477](https://github.com/yun1091724578/rust-cordis/actions/runs/41460414477)，17 条保证 × 4 平台矩阵已实填）；首发封存入口与同版本 `inspect` 本地演练（r3）；`cargo xtask soak` 本地 1000 轮通过 | **A5 关闭**：正式首发产物归档、历史产物 × 后续宿主、夜间 soak、其余发布前项 |
| 1：结果语义 | 完成（含已知例外） | ADR 0003 已冻结；八个契约词汇；§7 失败路径证据列；超时措辞收窄 | 事件路径没有返回协议，是已知例外，不为此新增协议 |
| 2：结构化错误 | 完成（A4 已收口，含已知边界） | ADR 0004 设计已接受；能力位 / 编解码 / 双向 `STATUS_ERR` / 详细入口 / 有界降级；异步 `on_done`、卸载 / 取消、provider 业务错误、超时和真实旧 ABI 错误均有测试；兼容矩阵是 **2 格真 e2e + 2 个降级分支桩 / 单测** | 分类 / 重试消费者留到阶段 4；事件路径无返回协议；详细入口的同步失败仍为 `String`，宿主预解析仍文本 |
| 3：声明与诊断 | 完成 | 配置 schema 在 `Capabilities` / CLI 可见；`Fiber::pending_injects` + `report.pending` + `cordis status` 解释缺失键 / 版本 / 未就绪 / 跨域 / 环；能力来源标注为插件自述 | 不承诺按 schema 自动校验；方法 I/O schema / 副作用声明留待消费者 |
| 4：调用事实 | 条件性待启动 | — | 有真实消费者再设计；不引入 task-local |
| 5：组件与宿主能力 | 条件性待启动 | — | 按门禁逐项 ADR；没有消费者只登记不实现 |
| 6：产品化与进程外载体 | 条件性待启动 | — | 安装 / 激活 / 迁移是三条链路；载体不是安全边界 |

当前主线：**A5（平台与发布证据）**；阶段 0 的出口仍由 A5 关闭。

## 下一批（按顺序）

| 顺序 | 工作 | 关闭条件 | 依赖 / 边界 |
|---|---|---|---|
| 1 | **A5-1 三平台 CI 首跑 + 平台矩阵实填** | Linux / Windows MSVC / macOS 实际编译、运行全量测试与真 DLL e2e；把每个平台的真实结果逐条填入 `docs/platform-matrix.md` | **已完成**：2026-11-02 [run 41460414477](https://github.com/yun1091724578/rust-cordis/actions/runs/41460414477)（commit `ea0489e`）三平台 `test` job 全绿；17 条保证 × 4 平台已填 `通过` |
| 2 | A5-2 夜间 soak | 在专机或 CI 定时跑更长轮次，记录资源 / 暂存台账 / 失败日志并归档 | **下一步**；A5-1 已完成；它推进发布证据，但不关闭平台出口 |
| 3 | A5-3 正式首发封存 | 在干净发布源码树上重跑 `cargo xtask seal`，归档整个 `seal.json` / `SHA256SUMS` / `plugins` / `smoke` 目录；同版本 `inspect` 证据随产物归档 | 三平台结果已通过；等发布候选版本冻结后再做；r3 只是本地演练，不是正式发布 |
| 4 | A5-4 历史产物 × 后续宿主 | 后续宿主版本加载已封存的首发产物，再把“历史发布版”从**待验证**转成**保证**或明确拒绝 | 被后续宿主版本阻塞；不能用同版本加载冒充跨版本兼容 |
| 5 | A5-5 关闭其余发布前项 | `Cargo.toml` 的 `repository` / `homepage` 元数据、公开发布地址、Windows SCM 真实 `sc create` / `sc start` / `sc stop` 实测；`release.md` 的发布前项关闭或明确延期 | 私有仓库已建立；公开发布地址 / 管理员权限仍待确认 |

**A5-1 已完成，下一步做 A5-2 夜间 soak。** 阶段 0 的平台出口已经由真实三平台结果回答：
Windows MSVC、Linux、macOS 的 `test` job 全绿，平台矩阵 17 条保证已逐条填 `通过`。
剩下的是长跑证据、正式首发封存和历史产物验证，不再需要重复证明平台维度。

A5-2 应在专机或 CI 定时跑更长轮次，记录资源 / 暂存台账 / 失败日志并归档；当前本地
1000 轮只是入口演练，不能替代夜间 soak。A5-3 必须等发布候选版本冻结后再执行；A5-4
必须等后续宿主版本，不能用同版本加载冒充跨版本兼容。

**现在不要启动**：

- 阶段 4：没有真实调用事实消费者，不为了形式统一引入隐式上下文。
- 阶段 5 / 6：没有消费者和独立 ADR 的能力只登记，不实现。
- 事件接口：不新增返回协议；事件派发的调用者认知保持已知例外。

## 第一批（交付物完成）

| # | 工作 | 产物 | 状态 |
|---|---|---|---|
| 1 | 支持契约 | `docs/support-contract.md` | 完成 |
| 2 | 结果与错误契约 ADR | `docs/adr/0003-result-and-error-contract.md` | 完成 |
| 3 | ABI 错误演进 ADR | `docs/adr/0004-abi-error-evolution.md` | 设计已接受；阶段 2 已实施，边界见 ADR |
| 4 | 保证索引检查器 | `cargo xtask docs` + `xtask/src/contract.rs` 单测 | 完成（55 bin + 17 CLI；文档 25 行 / 17 保证） |
| 5 | 失败路径清单 | ADR 0003 §7 | 完成 |
| 6 | 资源增长基线测试 | `crates/host/tests/resource_baseline.rs` + `staged.rs` | 完成（8 轮 × 7 路径；64 轮暂存；A5 另有本地 1000 轮 soak） |
| 7 | 真实旧插件夹具 | `tests/fixtures/abi-v1` + `frozen_plugin.rs` | 完成（冻结 ABI v1；历史发布版仍待验证） |

A1 / A2 / A3 的红绿记录见文末“附录：验证记录”。

## 阶段 0：可持续验证的基线

**目标**：文档里的每条断言要么有测试，要么明确标为声明 / 待验证 / 非目标。

已完成交付物：

- [x] 集中的支持范围与非目标（`docs/support-contract.md`）
- [x] 保证索引与检查命令（`cargo xtask docs`）
- [x] 公开 API、状态码、载荷格式清单
  - `docs/api-inventory.md` 由 `cargo xtask inventory --write` 生成；`--check` 已接进
    `cargo xtask ci` 与 xtask CLI 用例。
  - 当前清单：23 个 ABI 常量、4 个结构 / 41 个字段、56 个宿主 / 核心 / SDK 公开入口、
    32 条载荷；ABI 前缀 / 尺寸由 `兼容矩阵.md` 与 `abi_matrix.rs` 防漂移。
- [x] 冻结 ABI v1 兼容夹具（`tests/fixtures/abi-v1` + `crates/host/tests/frozen_plugin.rs`）
  - 冻结副本（去掉 sdk）+ 最小入口，编译成真 cdylib；
  - 真正的 72B v1 结构体（无 `provision`）；volatile、probe / load / mount / 回调 /
    dispose；坏版本 / 坏尺寸两条拒绝路径；
  - `frozen_plugin.rs` 实算 `src/lib.rs` / `Cargo.toml` 的 SHA256 与 README 比对，
    “永不跟随 HEAD”从纪律变成门禁。
- [x] 资源增长回归（`crates/host/tests/resource_baseline.rs` + `staged.rs`）
  - 8 轮 × 成功 / 失败 / 丢弃 Future / 带在飞派发卸载 / waterfall / stream / 同 key 重载；
    断言 `loaded_instances` / `dispatches_len` / `pending_len` / `waterfall_len` /
    `streams_len` / `effect_count` 回到基线；
  - 暂存台账 64 轮：`staged_leaked()` 入账 +1、认领归零、drop 删文件；
  - “先卸库后删文件”的顺序由 #7 冻结夹具覆盖（Windows 可判别；Unix 上 unlink 已映射
    `.so` 恒成功）；demo / e2e 仍保留顺序断言。
- [x] C0 统一验证入口：2026-11-02 `.\cargo.cmd xtask ci` 在 Windows GNU 全量通过
  （含 `matrix` 结构检查）；裸 `cargo xtask ci` 仍受本机 PATH / 工具链影响。

### A5：平台与发布证据

- [x] **三平台 CI 首跑**：Linux / Windows MSVC / macOS 实际编译、运行测试与真 DLL e2e；
      2026-11-02 [run 41460414477](https://github.com/yun1091724578/rust-cordis/actions/runs/41460414477)
      三平台 `test` job 全绿。
- [x] 保证 × 平台矩阵结构：`docs/platform-matrix.md` 由 `cargo xtask matrix` 检查，
      17 条保证逐条有行、四个平台列与证据；Windows GNU 已填本地结果。
- [x] **平台矩阵真实结果**：Windows MSVC / Linux / macOS 首次 CI 后逐条填 `通过`；
      记录 run URL 与 commit，不能只写结构通过。
- [x] **首发封存入口与同版本加载检查**：`cargo xtask seal` 构建宿主目标 release 插件，
      记录源码树 / 工具链 / ABI / SHA256，保存 `smoke/inspect.txt`，并强制三个预期
      artifact 集合；`cargo xtask seal --verify` 只读复验。2026-11-02 在 Windows GNU
      本地演练通过（r3），格式见 `docs/release-seal.md`。这只证明入口可用，
      **不是正式发布**，也不证明产物真实性 / 签名。
- [ ] **正式首发产物封存**：第一次 release 时重新运行 `cargo xtask seal`，把整个封存
      目录归档到发布存储；这是后续历史兼容的基线。
- [ ] **正式首发产物 × 同版本宿主**：把首发时 `seal` 的同版本 `inspect` 加载结果
      与产物一起归档；它只证明发布产物可用，**不证明跨版本兼容**。
- [ ] **历史产物 × 后续宿主版本**：等后续宿主版本加载已封存的首发产物，才把索引中的
      “历史发布版”从**待验证**转成**保证**或明确拒绝；不能停留在模糊状态。
- [x] **soak 入口与本地短时演练**：`cargo xtask soak [cycles]` 运行被忽略的
      `crates/host/tests/soak.rs`（默认 1000 轮；每轮成功 / 失败 + 两轮暂存，
      每 4 轮丢弃在飞 Future，每 3 轮重载）。2026-11-02 本地 1000 轮通过，
      资源、hook / runtime 与 `staged_leaked` 回基线。这是本地演练，
      **不是夜间 soak**。
- [ ] 夜间 soak：在专机或 CI 定时跑更长轮次，记录资源 / 暂存台账与失败日志；
      当前只有 1000 轮本地结果与 8 轮 / 64 轮基线。
- [ ] 关闭 `release.md` 的其余发布前项：仓库 URL、Windows SCM 真实 `sc create` /
      `sc stop`；三平台结果与平台矩阵见上。

**阶段 0 出口（未满足）**：`docs/release.md` 必须能回答“哪些保证在哪些平台实际验证过”，
而不是只有一条 CI 通过结论。平台矩阵已经用首次三平台 CI 实填；剩下的是正式首发产物
封存、同版本产物证据归档、历史发布版 × 新宿主，以及夜间 soak。该出口仍由 A5 关闭。

## 阶段 1：冻结结果与错误语义

**目标**：语义先定，类型后置。

- [x] ADR 0003：两个维度、断言来源、错误类别、重试边界、同步阻塞限制
- [x] 失败路径清单（ADR 0003 §7）并为每行补“证据位置”
- [x] 评审并冻结 `Returned` / `Failed` / `TimedOut` / `Cancelled` / `Disconnected`
      与 `NotExecuted` / `Reported` / `Unknown` 的最终命名
- [x] 明确超时语义与现有日志措辞的边界；适用通道的端到端断言留给 A4
- [x] 不引入：类型、账本、墓碑、迟到结果恢复、自动补偿、崩溃后重试

**阶段 1 出口（已满足，含已知例外）**：每条**有契约通道**的调用失败路径都能映射到契约，
且不声称掌握比实际更多的信息。事件派发没有返回协议，已列为已知例外；不为了形式统一
给事件接口新增返回协议，也不把日志措辞写成业务失败。

## 阶段 2：结构化错误传输（完成，A4 已收口）

**目标**：先让错误跨 ABI 可传输、可解析、可选详细读取；分类 / 重试消费者留到阶段 4。

已完成：

- [x] ADR 0004 设计接受：能力位位置、载荷字段、双向降级规则
- [x] 能力位 / 编解码与 apply / callback 分流测试（覆盖矩阵分流分支）
- [x] 有界降级：消息 20k→16k、中文边界、极小缓冲；`out.len` 永不超 `cap`
- [x] 详细入口：`require_detailed` / `set_detailed` / `call_plugin_detailed` /
      `call_iface_detailed` / `dispatch_detailed` / `next_detailed`；
      旧字符串 API 保持兼容；同消息异 `code` 可区分。
      `call_iface_detailed` 有真宿主 e2e；`dispatch_detailed` 有非法内联 JSON 单测；
      `next_detailed` 有续延单测；`dispatch_detailed` 尚无真宿主回调用例（见 ADR §6）
- [x] 边界测试：超长错误、非法 JSON、缺字段、未知码、空消息、未知字段
- [x] 手写插件测试：v1 夹具加载不越界 + `error_format` 探针
- [x] 超时清理 / 日志措辞（事件路径不证明调用者 `Unknown`）
- [x] 真实旧 ABI 插件返回 `STATUS_ERR`：独立 `tests/fixtures/abi-err-v1` 返回纯文本，
      `legacy_error` 验证新宿主保留文本、结构字段为 `unknown`，明确不是 `not_executed`；
      `tests/fixtures/abi-v1` 的冻结哈希未改。
- [x] `on_done` / `args[0].error` 结构化兄弟与详细通道：`call_iface_detailed` 等入口
      接收 `ErrorPayload`，旧字符串入口只取 `.error`；不重放有副作用的调用。
- [x] 超时通道端到端：`abi_outbound_timeout` / `abi_outbound_dispatch_timeout` /
      `abi_next_timeout` 断言可读原因、`HOST_TIMEOUT/timeout/unknown`；事件路径保持例外。
- [x] 卸载 / 取消原因：`abi_iface_unload_abort` 断言 iface abort 的 `kind:"iface"`、
      `host_lifecycle/lifecycle/unknown`；`sdk_iface_unload_detailed` 覆盖 SDK 详细续延。
- [x] provider 业务错误：`sdk_provider_error_detailed` 断言 `callback_failed/business`。
- [x] 非法内联 JSON：`dispatch_settles_text_continuation_on_malformed_inline_json` /
      `dispatch_settles_detailed_continuation_on_malformed_inline_json` 证明续延被结算，
      不再只 `forget`。
- [x] 兼容矩阵证据：旧插件 × 新宿主（真 cdylib）、新插件 × 新宿主（真 SDK + 真宿主）
      是 2 格真 e2e；旧插件 × 旧宿主没有独立证据，新插件 × 旧宿主是截断 `HostApi`
      的写 / 读单测。ADR 0004 明确标注“不是四格端到端”。
- [x] 文档收尾：ADR 0004 状态、`support-contract.md` 对应行、`release.md` 兼容门禁
      和本文件计数。

**阶段 2 出口（已满足，含已知边界）**：能力位 / 编解码 / 详细入口 / 有界降级 / 文本回退测试通过；
真实旧 ABI 插件错误、异步详细通道、卸载 / 取消原因、provider 业务错误、超时通道可重复；
兼容矩阵是 2 格真 e2e + 2 个降级分支桩 / 单测，不写成四格端到端。
分类 / 重试消费者不在本阶段，留到阶段 4；详细入口的同步失败仍为 `String`，宿主预解析仍文本。

**边界**：

- 事件接口仍不新增返回协议；事件派发的调用者认知是已知例外。
- `apply` 生命周期保持文本边界；异步 `on_done` 现在保留 `error` 字符串并可按能力位带
  结构化兄弟字段，但不改变调用结果。
- 结构化字段当前只被解码和读取，不参与自动重试或权限判断；SDK 提供事实，不提供许可。
- 挂起 provider 只能 `fulfill(value)`，异步业务错误需要新的 `fulfill_error` 语义；
  `call_iface_detailed` / `dispatch_detailed` / `next_detailed` 的同步失败仍是
  `String`。这两项均未排期：`fulfill_error` 未在阶段 3 实现，阶段 4 只在出现
  真实消费者时评估。ADR 0004 §7 已登记。

## 阶段 3：声明与装载诊断（完成）

**目标**：先连通已有机制，再讨论新协议。

- [x] 配置 schema：显式提供、SDK 校验格式、host 透出到 `Capabilities` / CLI
  - `PluginDesc.schema` 只用于**配置**，保持原义；
  - `cordis_plugin!` 支持 `schema:`，`build_plugin_with_schema` 在 SDK 构造器
    校验“合法 JSON 对象”（首次入口调用时执行，不是编译期）；非法 schema
    在入口被截住并降级为空描述表，宿主报可读装载错误；
  - 原生 `#[cordis::plugin(schema = "…")]` 在宏展开期校验 JSON 对象；
  - schema 展示上限 `MAX_CONFIG_SCHEMA_BYTES`（1 MiB）；超限按非法声明处理，
    不解析 / 复制；`Capabilities` / `LoadedPlugin` / CLI 用 `schema_error` 区分
    “未声明”与“声明了但不合法”；
  - `Capabilities::schema()` / `LoadedPlugin::schema()` / CLI `inspect` 展示原文；
  - **展示不等于验证**：宿主不按 schema 校验配置，插件自己的 `validate` / `apply`
    才是实际裁决；`Capabilities` 只加了私有字段 + 方法，没有新增公开字段。
- [x] Pending 诊断：缺哪个精确键、当前有哪些相关提供者、
  未就绪 / 版本键不同 / 跨域 / 依赖环分别是哪一种。
  - `Fiber::pending_injects()` 返回 `PendingInject { key, reason, related }`；
    `reason` 为 `missing` / `version_mismatch` / `provider_not_ready` /
    `realm_mismatch` / `cycle`；
  - 版本不匹配只依据**当前域可见**的同 base key；全局候选只进 `related`；
  - 提供者尚未 apply / provide 时还没有实现，仍报 `missing`；声明级环由
    `report.cycles` 解释，行内 reason 可能仍是 `missing`；
  - `LoaderState::pending_report()` 与 `report.pending` 给出行级诊断；
  - 新 CLI `cordis status <config>` 装载后打印诊断并关闭（`inspect` 仍永不挂载）；
    同时列出 `disabled` / `failed`，失败行退出码非 0。
- [x] 能力与来源声明：`Capabilities::origin()`、`inspect` / `status` 输出
  `plugin-self-description (not approval)`；manifest 仍是插件自述，不是审批结果。

**阶段 3 出口（已满足）**：用户能在装载前通过 `inspect` 读到 schema / inject /
provision / volatile；能在装载后通过 `status`（或 `report.pending` / `report.cycles`）
解释缺哪个 key、是缺提供者、版本键不同、提供者未就绪、跨 isolate 域还是依赖环，
并能看到失败 / 禁用行。每一类诊断都有可重复测试。

**边界**：不承诺方法 I/O schema、副作用声明、工具描述——等待工具层消费者提案。
给现有公开结构增加 schema / 诊断信息时优先加方法；`Capabilities` 的 schema 是
私有字段 + accessor，仍会让外部 struct 字面量失效（0.x 的已知源级破坏）。
`PendingReason` 新增 `realm_mismatch` 变体，外部穷尽匹配会失效；`StatusReport`
新增 `disabled` / `failed` 字段，外部 struct 字面量也会失效。两者都是 0.x
已登记的源级变化，不冒充兼容。

## 阶段 4：调用事实通道

**目标**：出现真实需求后再设计，不为了形式统一引入隐式上下文。

- [ ] 写清三条路径的当前能力（直接 typed / 宏生成 adapter / 手写 `ExternService`）
- [ ] 明确不引入 task-local
- [ ] 真实需求出现后：区分调用方声明与宿主产生的事实；
      定义字段生成方、覆盖权限、传播范围与可信边界
- [ ] 错误分类 / 重试消费者：只有真实调用方需要时，才把
      `code` / `category` / `certainty` / `retry` 接入决策；不自动重放有副作用的调用
- [ ] 业务实现必须有显式读取入口，否则不增加无消费者的信封字段

**阶段 4 出口**：一个实际消费者通过该通道完成需求，并证明现有调用方式不受影响。

## 阶段 5：按需增加组件与宿主能力

**门禁**：下表的每一项都是条件分支。任何一条要变成承诺，必须先有独立 ADR 与验收标准；
没有真实消费者时只登记，不实现。

| 能力 | 启动条件 | 位置 | 验收重点 |
|---|---|---|---|
| 并发限制、排队 | 出现明确容量约束 | components | 有界队列、取消出队、关闭排水 |
| 重试 | 有可证明安全的具体调用 | 组件 / 领域层 | 取消、预算、未知结果不导致不安全重放 |
| 权限与审批 | 存在不同信任或授权主体 | 宿主 | 所有受管入口经过检查，绕行边界明确 |
| 幂等与恢复 | 有真实持久副作用和重试需求 | 领域服务 / harness | 稳定幂等键、重复执行、结果查询 |
| 执行账本 | 需要跨崩溃恢复 | 领域存储层 | 执行前后故障注入、未知结果处理 |
| 路由 / broker | 存在多实现选择需求 | components | 选择、退场与在飞调用契约 |

不为提前支持策略把额度令牌或队列塞进 core。只有跨实现确实通用、
且无法从组件内部获取的观测，才考虑新增核心钩子。

## 阶段 6：产品化安装与进程外载体

**门禁**：安装、激活与迁移是三条独立链路；在没有真实下游需求前，
不把任何一条写成默认能力，也不把载体写成安全边界。

- [ ] 插件包格式、版本固定、来源与校验和、安装前兼容检查、原子切换、残留诊断
- [ ] 明确区分安装、激活与业务数据迁移
- [ ] 仅在出现独立故障域 / 独立部署需求时启动进程外载体
- [ ] 第一个原型验证：请求关联、断连后的未知结果、取消与关闭、有界传输、
      进程崩溃、协议协商、远端重启后的旧句柄失效

“不可信代码”不是载体的单独触发条件：它需要载体**加** OS / Wasm 执行边界策略，
后者是产品决策。参见 `docs/adr/0001-second-carrier.md` §2。

## 贯穿验收

| 验收层 | 核心问题 | 本仓落点 | 当前判断 |
|---|---|---|---|
| 契约测试 | 语义与降级规则是否保持 | `support-contract.md` + `cargo xtask docs` | 25 行 / 17 保证；本地与首次三平台 CI 均绿 |
| 确定性交错测试 | 取消、完成、关闭同时发生时谁先 | `lifecycle_races.rs`、`abi_m2c.rs` | 已有测试；新增交错随变更补 |
| 资源回归 | 成功与失败路径是否都能回收 | `weak_audit.rs`、`resource_baseline.rs`、`staged.rs`、`soak.rs` | 8 轮 × 7 路径 + 64 轮暂存 + 本地 1000 轮 soak；夜间长跑未做 |
| 兼容测试 | 旧 ABI 二进制（冻结快照）是否仍工作 | `abi_matrix.rs`、`tests/fixtures/abi-v1`、`frozen_plugin.rs`、`legacy_error.rs` | 冻结 v1 与真实 72B 旧插件错误已绿；历史发布版待 A5；旧宿主格是桩；旧 Rust 调用者 API 稳定性仍待单列 |
| 多平台验证 | 装载、卸载、信号、文件替换 | CI + `release.md` | Windows GNU 本地 + Windows MSVC / Linux / macOS 首次 CI 均通过；后续变更需重跑 |
| 压力测试 | 长时间重复操作后是否稳定 | `resource_baseline.rs`（8 轮）/ `staged.rs`（64 轮）/ `soak.rs`（1000 轮） | 本地 1000 轮有；夜间长跑未做 |
| 故障注入 | 明确故障点是否得到契约允许的结果 | 阶段 2 / 5 | 已覆盖错误边界；崩溃恢复随恢复协议建设 |

崩溃恢复测试必须随恢复协议与持久化实现一起建设；当前可做的是异常退出、
超时、资源回收与旧插件兼容，不需要等待账本。

## 完成标准

本计划的“完成”只指对应阶段的验收出口满足；交付物存在或局部测试通过都不算完成。

最近阶段不是“具备所有 agent 能力”，而是：

- 插件失败可解释；
- 调用结果不误导；
- 升级兼容可验证；
- 资源生命周期可证明。

阶段 2、3 已收口（含已知边界）。A5 关闭后，
阶段 0 才从“交付物完成、出口未满足”改为“完成”。

## 附录：验证记录（红档 / 绿档）

### 第一批 #4：保证索引检查器

- 红档：`cargo test -p xtask` —— 7 个用例失败（解析为空、缺节不报错、列数不校验、
  保证无证据不报错、缺失测试不报错、未知状态不报错、真实索引为空）。
- 绿档：8 个用例通过；`cargo xtask docs` 对 `docs/support-contract.md` 通过。
- 第二轮 P0 / P1：新增 9 个 bin 用例失败（真测试判定、路径逃逸、表头、围栏、转义、
  重复节、非保证行路径、索引行数、panic 行状态）；修复后 17 bin + 3 CLI 通过。
- 当前：55 bin + 17 CLI；文档 25 行 / 17 保证；`cargo xtask ci` 已包含
  `inventory --check`、`docs` 与 `matrix`，首发 `seal` 与 `soak` 是发布时入口，不在 `ci` 内。

### 第一批 #6：资源增长基线

- 范围：8 轮 × 成功 / 失败 / 丢弃 Future / 带在飞派发卸载 / waterfall / stream /
  同 key 重载；暂存台账 64 轮（`staged.rs`）。
- 绿档：`cargo test -p cordis-host --test resource_baseline`（1 passed）与
  `cargo test -p cordis-host --test staged`（4 passed）。
- 红档：把 `PendingDropGuard` 的 `if !stream_owned` 临时改成
  `if false && !stream_owned`，资源基线在“丢弃轮归零”处超时失败；还原后复绿。
  红档钉住的是**挂起记录撤销**；waterfall / stream / 重载 / effect 由正向对照钉住
  （分别观测 1 / 1 / apply+1 / 精确 5），未分别做变异。
- 覆盖：成功 / 失败返回值可区分；丢弃 Future；带在飞派发卸载；waterfall 帧；
  stream 记录；同 key 重载（`fiber.update`）；64 轮暂存台账。

### 第一批 #7：冻结 ABI v1 夹具

- 红档 1：测试先落，夹具目录不存在 ⇒ 在“夹具源码必须存在”处失败。
- 绿档：冻结 cdylib 构建成功；新宿主 probe（volatile `["data"]`、`provision` 空）、
  原始 desc 尺寸 = 72、坏版本 / 坏尺寸均被拒、load / mount / 回调 / dispose、暂存删除、
  `staged_leaked() == 0`。
- 红档 2：`bad-version` feature 让宿主以“ABI 主版本是 2，宿主需要 1”拒绝；
  `bad-size` feature 以“PluginDesc 尺寸 71 小于冻结前缀 72”拒绝。
- 红档 3：夹具末尾加一行注释、不更新 README ⇒ 冻结门禁报
  `left 2b7b… / right 378e…`。
- 红档 4：把 `field_at` 的 `size` 判界临时改成 `if true` ⇒
  `abi_matrix` 的前缀 `provision` 断言失败（读到非空 `["tools"]`）。
- 夹具是**真正的 72B v1 结构体**（无 `provision`）；`abi_matrix` 的正向对照
  使用非空 `["tools"]`，证明“前缀案例读到 None”不是空断言。

### A1：结果语义冻结

- ADR 0003 状态改为“已冻结”；§1 锁定八个契约词汇（5 个结束原因 + 3 个执行认知）；
  §7 每行补“证据位置”。
- `abi_m2_timeout` 的测试名从 `...as_failure` 改为 `...as_timeout`，注释与断言改为
  “可读超时原因，不能写成业务失败”；该测试通过。
- 本测试只证明清理 / 日志措辞；事件路径没有返回协议，无法做调用者 `Unknown`
  的端到端断言，列为已知例外，不为此给事件接口加协议。

### A2：API 清单生成器

- 生成器 `xtask/src/inventory.rs`：自动扫描 23 个 ABI 常量、4 个结构 / 41 个字段；
  56 个宿主 / 核心 / SDK 受控入口与 32 条载荷逐项校验源码 marker（必须恰好命中一次）。
- `cargo xtask inventory --check` 通过；篡改清单 / 缺 payload 的用例红；
  `cargo xtask inventory --write` 生成 `docs/api-inventory.md`。
- A2 当时 `cargo test -p xtask`：38 bin + 12 CLI；`cargo xtask ci` 包含
  `inventory --check`、`docs` 与 `matrix`。

### A3 / 阶段 2 核心：结构化错误传输

- ABI：`ERROR_FORMAT_V1 = 1`；`PluginDesc.error_format` 与 `HostApi.error_format`
  尾追加并按 `size` 判界；`PluginDesc::field_at` / `error_format_at` 用裸指针读取，
  避免对 72B v1 形成 96B `&PluginDesc` 越界引用。
- 两个 `STATUS_ERR` 方向都支持结构化载荷；缺能力 / 解析失败 / 放不下时回退文本；
  文本按 UTF-8 边界截断，`out.len` 永不超 `cap`。
- 详细入口与兼容：`Ctx::require_detailed` / `Ctx::set_detailed` /
  `call_plugin_detailed` 保留完整 `ErrorPayload`；旧 `require` / `set` / `call_plugin`
  只取 `.error`；同消息异 `code` 可区分。
- 测试覆盖：`crates/abi/tests/error_format.rs`（能力位、编解码、边界）、
  v1 shape、宿主 `abi_matrix` 的 `error_format@88` 探针、apply / callback 失败、
  生产 `HostApi` 的真实 `require_detailed`、内联 16 KiB 明确失败、
  20k→16k、中文边界、极小缓冲、超 `cap` 诊断。
- 门禁：fmt / docs / inventory / clippy / ABI+Host 全量 / demo+e2e 全绿；
  ABI lib 32 passed；host lib 45 passed；A3 当时 xtask 38 bin + 12 CLI；
  inventory 23 / 4 / 41 / 56 / 32。`.\cargo.cmd xtask ci` 已全量通过；
  裸 `cargo xtask ci` 仍受本机 PATH / 工具链影响。
- A4 已收口：真实旧 ABI 错误、异步详细通道、卸载 / 取消、provider 业务错误、超时端到端；
  兼容矩阵是 2 格真 e2e + 2 个降级分支桩 / 单测，见“阶段 2”；分类 / 重试消费者留到阶段 4。

### A4 / C0：错误端到端与统一验证入口

- C0：2026-11-02 `.\cargo.cmd xtask ci` 在 Windows GNU 全量通过
  （fmt / inventory / docs / matrix / clippy / test / demo / e2e）。包装器提供 ASCII target、
  `RUSTFLAGS=-C prefer-dynamic=no` 与工具链 self-contained PATH；裸 `cargo xtask ci`
  仍可能触发 `dlltool/as` 环境问题。
- 旧 ABI 错误：新增 `tests/fixtures/abi-err-v1`（72B v1、纯文本 `STATUS_ERR`），
  `legacy_error` 验证文本保留、`code/category/certainty=unknown`，且明确不是
  `not_executed`；冻结的 `abi-v1` 未修改。
- 异步详细通道：`call_iface_detailed` / `dispatch_detailed` / `next_detailed` 接收
  `ErrorPayload`；旧字符串入口只取 `.error`。宿主在能力位为 1 时把
  `code/category/certainty/retry/detail` 写入 `args[0]` 兄弟字段。
- 超时：`abi_outbound_timeout` / `abi_outbound_dispatch_timeout` /
  `abi_next_timeout` 均断言可读原因与 `HOST_TIMEOUT/timeout/unknown`；
  `sdk_detailed_timeout` 走真实宿主 + SDK 详细入口。
- 兼容矩阵：旧插件 × 新宿主（真 cdylib）、新插件 × 新宿主（真 SDK + 真宿主）是真 e2e；
  旧插件 × 旧宿主无独立证据（`error_format=0` 是插件能力位），新插件 × 旧宿主是截断
  `HostApi.size` 的写 / 读单测；ADR 0004 明确写“不是四格端到端”；
  ADR 0004 §3 明确标注桩范围。
- 红档 1：把 `error_arg` 的兄弟字段关闭 ⇒ `abi_outbound_timeout` 在
  `code=HOST_TIMEOUT` 断言处失败；还原后复绿。
- 红档 2：把 `decode_plugin_error` 的纯文本兜底标成 `not_executed` ⇒
  `legacy_error` 在 `certainty` 断言处失败；还原后复绿。
- 红档 3：把 `PendingSink` 的结构化失败重新映射成
  `plugin_error/business/reported` ⇒ `pending_settle_preserves_a_structured_host_failure`
  在 `host_lifecycle` 断言处失败；还原后复绿。
- 红档 4：把 `ContinuationKind::Iface` 错标成 `KIND_DISPATCH` ⇒
  `abi_iface_unload_abort` 在 `kind:"iface"` 断言处失败；还原后复绿。
- 门禁更新：ABI lib 32 passed；host lib 45 passed；A4 当时 xtask 38 bin + 12 CLI；
  inventory 23 / 4 / 41 / 56 / 32；`.\cargo.cmd xtask ci` 全量通过。

### B：阶段 3 声明与装载诊断

- 配置 schema：`build_plugin_with_schema` / `cordis_plugin! schema:` 在 SDK 构造器
  校验合法 JSON 对象（首次入口调用时执行）；非法 schema 在入口被截住并降级为
  空描述表；原生 `#[cordis::plugin(schema = "…")]` 在宏展开期校验。schema 展示
  上限 1 MiB，超限 / 非法与“未声明”通过 `schema_error` 区分。`Capabilities::schema()`、
  `LoadedPlugin::schema()`、CLI `inspect` 展示原文；宿主**不**按 schema 验证配置，
  插件自己的 validate / apply 才裁决。测试：`crates/abi/tests/schema.rs`、
  `macro_schema.rs`、`invalid_schema_entry.rs`、`crates/macros/src/plugin.rs`、
  `loader_capabilities`、`probe_capabilities`、`cli/tests/inspect.rs`。
- Pending：`Fiber::pending_injects()` 返回精确 key + `missing` /
  `version_mismatch` / `provider_not_ready` / `realm_mismatch` / `cycle` +
  同 base 相关 key；版本判断只使用当前域可见候选；提供者尚未 apply 时仍报
  `missing`；`LoaderState::pending_report()` 与 `report.pending` 给出行级诊断；
  `cordis status` 装载后打印 pending / disabled / failed / cycles 并关闭，失败行退出码非 0。
  测试：`core/tests/pending_diagnostics.rs`、`runtime_cycles.rs`、
  `host/tests/loader_cycles.rs`、`cli/tests/status.rs`（含声明环、失败、禁用行）。
- 来源声明：`Capabilities::origin()` 与 `inspect` / `status` 输出
  `plugin-self-description (not approval)`；支持契约新增“声明”行，明确不是审批。
- 兼容性：`Capabilities` 增加私有 `schema` 字段 + accessor；外部 struct 字面量会失效，
  这是阶段 3 的已知 0.x 源级破坏（方法优先，但不加公开字段）。
- 门禁：`cargo xtask docs` 通过（25 行 / 17 保证）；`inventory` 23 / 4 / 41 / 56 / 32；
  demo 的装载报告已加入 `"pending":[]` 并重新通过字节级快照（4998 bytes）。

### A5-1：首次三平台 CI

- 2026-11-02 在私有仓库 `yun1091724578/rust-cordis` 首次运行 GitHub Actions；
  最终 [run 41460414477](https://github.com/yun1091724578/rust-cordis/actions/runs/41460414477)
  （commit `ea0489e`）的 `test (windows-latest)`、`test (ubuntu-latest)`、
  `test (macos-latest)`、`lint`、`msrv`、`package` 全部成功。
- 前三次运行暴露并修复了四个真实的跨平台问题：
  1. `tests/fixtures/abi-v1/Cargo.toml` 的冻结哈希依赖 CRLF 检出；新增
     `.gitattributes` 固定 `tests/fixtures/**` 为 `eol=lf`，并更新 README 哈希。
  2. demo 的配置目录与 Unix 二进制 `cordis-demo` 同名；改为 `cordis-demo-config`。
  3. harness-spike 同理，配置目录改为 `harness-spike-config`。
  4. inventory 读取源码时未归一化 CRLF，marker 在 Windows MSVC 上匹配失败；
     `read` 现在先归一化为 LF，并新增 CRLF 回归用例。
- 平台矩阵 17 条保证在 Windows GNU / Windows MSVC / Linux / macOS 四列均填 `通过`；
  Windows GNU 来自本地 `.\cargo.cmd xtask ci`，其余来自上述 CI 的完整测试 + demo + e2e。
- 记录边界：这是**一次**通过的 CI 结果，不是持续保证；后续变更仍需重新跑三平台。

### A5：首发封存入口与同版本加载检查

- 命令：`cargo xtask seal [--out <dir>]` 构建三个宿主目标 release cdylib，写
  `seal.json` / `SHA256SUMS` / `smoke/cordis.yml` / `smoke/inspect.txt`，
  并用同版本 `cordis-cli inspect` 加载三个产物（`LoadedPlugin::open` +
  `OP_PING` / `OP_VOLATILE_PATHS`，不挂载），逐个断言 `(plugin: <名称>)`。
  `cargo xtask seal --verify <dir>` 不构建，只校验三个预期 artifact 的
  package / crate 集合、目录无未登记文件、路径、大小、SHA256、smoke 配置 /
  inspect 证据与 `SHA256SUMS`。
- 不可覆盖：输出目录非空直接拒绝；构建期间工作树摘要变化也拒绝提交；临时目录
  使用随机后缀，不删除已存在的同名目录。
- 红档：分别临时关闭 artifact SHA256 比对、让 `safe_relative_path` 恒真、关闭预期
  artifact 集合检查、关闭 `plugins/` 目录枚举、把 inspect 身份断言退回只匹配 label；
  对应用例依次失败，还原后复绿。
- 绿档：2026-11-02 `.\cargo.cmd xtask seal --out ...-r3` 通过，封存目录
  `dist/cordis-plugins-0.1.0-x86_64-pc-windows-gnu-r3/`；同版本加载检查通过；
  `smoke/inspect.txt` 保存三个插件的真实输出；`seal --verify` 通过。这是
  **本地演练**，正式首发归档仍未完成；r3 的源码快照早于后续文档改动，正式首发
  必须在干净源码树上重跑。
- 边界：`seal --verify` 只做完整性校验，不是真实性 / 签名验证。
- 测试：xtask 55 bin + 17 CLI；`seal --verify` 成功路径、失败路径、拒绝覆盖与
  冲突参数均有 CLI 用例。SHA-256 使用空串、56 / 64 / 112 字节与 1,000,000
  字节已知向量；cargo artifact JSON 解析有独立用例。

### A5：soak 入口与本地 1000 轮

- 入口：`cargo xtask soak [cycles]` 设置 `CORDIS_SOAK_CYCLES` 后运行
  `crates/host/tests/soak.rs`；测试默认 `#[ignore]`，不进入 `ci`。
- 每轮成功 / 失败 + 两轮暂存台账 `stage -> adopt -> drop`；每 4 轮额外
  丢弃在飞 Future、每 3 轮额外同 key 重载（1000 轮分别为 250 / 334 次）。
- 断言：mount 后 effect 数；dispose **前**派发 / 挂起为零；dispose 后
  `cleanup_error == None`、effect 为 0、`loaded_instances == 0`、
  `runtime_count == 0`、事件 hook 为 0；`OP_CLEANUP` / `OP_DISPOSE`
  至少各投递一次；`staged_leaked` 回到 0。
- 红档 1：临时让 `StagedFile::drop` 不删除文件 ⇒ 3 轮 soak 在
  “drop 后暂存文件必须删除”失败；还原后复绿。
- 红档 2：临时不移除成功 / 失败的派发记录 ⇒ 3 轮 soak 在成功轮
  dispose **前**的挂起 / 派发残留处失败；还原后复绿。这证明断言不再是
  dispose 后的空检查。
- 绿档：2026-11-02 `.\cargo.cmd xtask soak` 默认 1000 轮通过（1 passed，
  约 7–10s）。这是本地短时演练，不是夜间长跑。
- 边界：暂存部分是 `stage_plugin` / `StagedFile` 的 primitive 记账，不覆盖
  loader 真 DLL 的 `LoadedPlugin::open_staged`；带在飞派发 dispose、
  `ctx_dispose_self` 与 `App::close` 仍由各自专项测试覆盖。
- 测试计数：xtask 55 bin + 17 CLI；`soak 0` / 非数字 / 多余参数退出码 2，
  `soak --help` 退出码 0，均有 CLI 用例。

### A5：harness step 9 的异步卸载等待

- 2026-11-02 一次全量 `xtask ci` 在 harness step 9 失败：自我卸载后
  `sessions@1` 仍返回 `Ok(Number(20))`，随后单独 e2e 与全量 ci 均通过。
- 机制：`ctx_dispose_self` 用 `runtime.spawn(fiber.dispose())` 异步拆 fiber；
  loader 行先摘除，不代表 effect 清理、`OP_DISPOSE`、实例注销已完成。
  测试原先只等 `state.fiber_of("harness")` 消失，存在窗口。这个窗口也是
  产品可观测面的一部分：`dispose_self()` 返回不代表 teardown 完成，完成点应看
  fiber 的 `dispose()` / `loaded_instances()`。
- 修复：step 9 在 emit 前捕获该行 fiber；行摘除后调用同一个
  `row.dispose().await`（幂等，不睡墙钟），断言 `cleanup_error == None`、
  `effect_count == 0`、`loaded_instances == 0`，再直接断言 `sessions@1` 不可解析。
- 红档：未稳定复现（原失败只观察到一次）；修复后用**直接** `unwrap_err()` 和
  `err.contains("sessions@1")` 收紧断言，不再接受任意错误。
- 验证：修复后连续 5 次 `xtask e2e` 通过；随后全量 `xtask ci` 通过。未复现原
  失败，但收敛点从“行消失”改为“fiber 清理完成”，不再依赖时序。
