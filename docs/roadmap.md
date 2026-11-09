# 分阶段执行计划

本文件是唯一执行计划入口：当前判断、下一批顺序、逐阶段交付物 / 验收出口与验证记录都在这里。
根目录的设计文档（`方案.md`、`补全方案.md`、`SDK方案.md`、`原理.md` 等）保留设计与
历史交付背景，不作为当前阶段状态来源。

- 支持范围、非目标与保证索引：`docs/support-contract.md`
- 结果与错误决策：`docs/adr/0003-result-and-error-contract.md`、`docs/adr/0004-abi-error-evolution.md`
- 信任模型与插件来源：`docs/adr/0005-trust-model-and-plugin-source-boundary.md`
- 发布门禁与未确认项：`docs/release.md`
- 首发产物封存格式：`docs/release-seal.md`
- 公开面与载荷清单：`docs/api-inventory.md`
- 冻结 ABI v1 的 provenance：`tests/fixtures/abi-v1/README.md`
- 架构现实审查与问题依据：`docs/architecture-review.md`（2026-11-03；不另设执行计划）

## 执行与维护规则

- **代码项走 TDD**：先写会失败的测试（red），再写最小实现（green），最后重构。
- **回归测试要有红档**：临时破坏实现，测试必须失败；还原后再绿。
  没有红档的回归测试只记录“代码改过”，不证明行为被钉住。
- **文档项不走 TDD**，但它引用的测试名由 `cargo xtask docs` 校验：
  “保证”行必须指向存在、带 `#[test]` / `#[tokio::test]` 且未 `#[ignore]` 的测试。
  这是引用有效性检查，不证明断言覆盖充分，也不证明平台结果；红档要求与已执行记录分开。
- 每个阶段或批次结束时，同时更新本文件、`docs/support-contract.md` 的索引状态和
  `docs/release.md` 的对应门禁；只改代码不改状态，视为未完成。
- “完成”只指**验收出口满足**。交付物存在、局部测试通过、或表格里写“部分完成”，
  都不等于阶段完成。

## 总览（2026-11-07，ADR 0005 后）

| 工作线 | 当前判断 | 已落地证据 | 未闭合 / 下一出口 |
|---|---|---|---|
| A. 低层 ABI 交付 | **完成** | 0.1.0/0.1.1 发布；封存、同版本加载、registry-only consumer、三平台 CI | 历史兼容的可重复测试 |
| B. 信任模型 | **已接受（ADR 0005）** | README/支持契约已加入来源边界与不可抢占声明；产品口径已收口 | 来源 allow-list 实现与验证 |
| C. 可信单租户底座 P0 | **进行中** | 来源策略模块、`LoadedPlugin::open_verified`、`--source-policy` 的 loader/CLI 强制与 inspect/status provenance 已落地；现有生命周期/资源基线/错误通道 | 每 App dispatch policy、容量准入、锁纪律、不可抢占披露 |
| D. 长期运行 P1 | **待启动** | 1M soak、resource baseline、staged primitive | semver/依赖求解、RSS/dhat、真实 open_staged、缓冲池、版本矩阵 |
| E. agent 流处理 | **原型完成** | 六模式状态机、9+2 单测、多消息快照、统一收尾 | 有界增量交付、真实模型适配器、取消/预算 |
| F. T3 / 不可信插件 | **条件性待启动** | ADR 0001 预留位置；无实现 | 只有第三方/模型生成/多租户需求出现时另立 T3 ADR |
| G. 维护治理 | **持续** | R4/R5/R6 已登记 | serde_yaml 迁移、宿主模块拆分、锁/panic 审查 |

阶段 0 的平台与发布基线：支持契约 **26 行 / 17 条保证**；0.1.1 三平台 CI
[`42332542433`](https://github.com/yun1091724578/rust-cordis/actions/runs/42332542433)
/ [`42332542329`](https://github.com/yun1091724578/rust-cordis/actions/runs/42332542329)
通过；0.1.0/0.1.1 已发布并封存；v0.1.1 宿主加载 0.1.0-rc2 封存插件通过一次；
SCM 一次真实 elevated run 通过；默认 1M soak 已通过。历史兼容仍待可重复测试，
nightly cron/长期转存仍待补。

当前主线：**P0 可信单租户底座 + agent 有界增量交付 + R1 真实使用反馈**。
历史兼容有一次正式版本通过但仍未形成可重复测试；阶段 2 / 3 的完成只覆盖既定范围，
不等于 SDK 已经通过任意升级验证。

### 信任模型约束（ADR 0005）

1. T1/T2 与宿主共享地址空间、权限和生命周期；`realm/scope` 不是安全边界。
2. T2 同步 `apply`/回调不可抢占；`DispatchPolicy` 只覆盖等待 `STATUS_PENDING`
   结算，不覆盖正在执行的调用。
3. T2 无法给单个插件设硬内存上限；cgroup/Job Object 限制整个宿主进程。
4. Rust 不保证无逻辑泄漏；只证明已枚举路径回基线并做进程级观测。
5. `abi_version` 是线格式门禁，不是插件 semver，也不证明行为兼容。
6. 同一 dylib 的 static 由所有实例共享；插件作者新增全局状态必须按 handle 分片。
7. 来源治理（review、revision、hash、allow-list）不是执行隔离；流水线/密钥
   被攻破时，“可信”也会失效。

ADR 0005 已接受；P0 整改完成前，本仓只能称为“可信组件运行时”，不能称为
“agent / 微服务安全底座”。

## 当前执行优先级（架构现实审查后）

以下是新增工作的优先级；后面的 A5 表保留发布批次状态，不再代表唯一开发队列。
审查证据与已知边界见 [架构现实审查](architecture-review.md)。

### 信任模型整改（ADR 0005，P0/P1）

| 顺序 | 工作 | 交付 / 关闭条件 | 边界 |
|---|---|---|---|
| P0-1 | 来源与身份（实现完成，待 fresh review） | sidecar manifest + SHA256 allow-list 校验、`open_verified`/`open_staged_verified`；`xtask seal` 产出 sidecar + `allow-list.json` 并纳入 `SHA256SUMS`；CLI `--source-policy` 在 `dlopen` 前强制，**无策略默认拒绝**，`--allow-unverified` 仅显式开发模式；inspect/status 展示 artifact / SHA256 / source_revision / manifest | 来源治理不是安全隔离 |
| P0-2 | 每 App 的 dispatch policy | 不再只依赖进程级 `set_dispatch_policy`；按 App/插件/方法配置；只覆盖挂起结算；长调用显式 opt-out | 不覆盖同步 `apply`/回调 |
| P0-3 | 容量与准入 | instances/dispatches/队列有上限；超限可读拒绝；按 App/实例配置 | 不能给 T2 插件内存硬上限 |
| P0-4 | 锁纪律与故障注入 | 不持锁调插件；`call_inbound` 读锁路径审查；hang/panic/重入/卸载竞态有测试 | 锁中毒只在可达路径确认后定级 |
| P0-5 | 失败模式对外披露 | README/支持契约/release 写明不可抢占、部署级 supervisor 兜底 | 不改写成“稳定保证” |
| P1-1 | semver + 依赖求解 | `provides/requires` range；mount 前解析、歧义/缺失拒绝；`(id, version)` 身份 | ABI 仍精确匹配；不引入冲突的 abi range |
| P1-2 | 版本矩阵 CI | 插件 vN 在宿主 vN/vN+1 各跑一遍 | 同版本加载不等于跨版本兼容 |
| P1-3 | 内存观测 | nightly RSS / dhat；真实 `open_staged` soak；泄漏预算 | 进程级观测不是 per-plugin 硬上限 |
| P1-4 | 性能与故障隔离 | 缓冲池化；若保留线程池，证明超时后不卸载、不释放插件；线程泄漏登记 | 真正的抢占仍属 T3 |

P0 **不包含**“有界专用线程池 + 超时放弃等待”；线程池不能取消同步调用，
超时放弃会留下仍在执行插件代码的线程，卸载时可能 use-after-unload。

### 接入与维护线（原 R1–R6）

| 顺序 | 工作 / 状态 | 交付与验收出口 |
|---|---|---|
| 1 | **R1 仓外真实接入（技术接入 + 31 篇样例归档任务完成，待真实数据反馈）** | 2026-11-03 在 `../desolate` 建立独立 crate 图（直接 path 依赖，固定 revision `9f5ddfb`），真实 DLL 装载、配置失败、缺失 root、卸载、重载与 v1→v2 升级均通过。2026-11-04 用自造 31 篇样例（25 个 `.md` + 6 个 `.markdown`）完成索引、搜索、重载、升级、标签过滤与报告写出，报告见 `../desolate/reports/archive-report.json`，记录见 `../desolate/R1-REPORT.md`。这补上了任务形状，但还不是第三方生产数据或长期维护反馈；第三方复现是额外证据，不是必选门禁。 |
| 1 | **R2 交付入口闭合（已决策：首发低层 ABI）** | 2026-11-03 决定首发支持低层 ABI + 固定 revision 源码依赖；`../desolate` 已验证仓外编译、宏展开、真实 DLL 装载、调用失败、卸载、重载与 v1→v2 升级。registry typed SDK 不在首发范围（`rust-cordis-sdk` 仍 `publish = false`；`rust-cordis-macros` 可打包但不能单独提供 typed 门面）；证据见 `../desolate/R1-REPORT.md` 与 `release.md`。 |
| 2 | **R3 调用通道使用契约（完成：仓外三入口 + 窄策略消费者）** | `../desolate/plugins/detailed-probe` 已覆盖同步启动错误、provider 业务错误回调、`HOST_TIMEOUT`、卸载/取消结算与恰好一次；2026-11-04 又覆盖 `dispatch_ok` / `dispatch_timeout`、`next_ok` / `next_timeout`，并在卸载后检查续延未再次投递。探针的窄策略消费者按 `category` / `certainty` 选择 `accept` / `fail_fast` / `degrade_unknown` / `unavailable`，记录 `automatic_retry=false`，不解析文本、不自动重试；记录见 `../desolate/R3-REPORT.md`。完整重试/补偿决策仍属阶段 4。 |
| 2 | **真实 agent 集成（首条链路完成，待真实模型）** | `../desolate` 的 `agent` 任务已跑通六模式：正常、模型流失败、工具超时未知、普通模式下真实超时、工具错误、模型启动失败。工具结果按 `Ok(Ok)` / `Ok(Err)` / `Err(elapsed)` 实际分支分类，测试模式不负责解释结果；`Ok(Err)` 无证据时保留 `unknown`。统一收尾路径先生成结果，再发最多一个 `run.finished`、释放插件并关闭 App；部分快照按消息保存。宿主自己持有 100ms 截止时间，不把 `DispatchPolicy` 当通用预算。记录见 `../desolate/AGENT-REPORT.md`；事件协议与设计见 [agent-stream-protocol](agent-stream-protocol.md)。下一步是有界增量交付，再接真实模型适配器、取消与预算。 |
| 2 | **A5-3 正式封存（完成）/ A5-5 剩余项** | 0.1.0/0.1.1 已封存并发布，同版本 `inspect` 与 registry-only consumer 通过；SCM 一次真实 elevated run 通过；`repository`/`homepage` 已补。剩余：nightly 持续运行与 artifact 长期转存、SCM CI 重跑、历史兼容的可重复测试。 |
| 3 | **R4 YAML 依赖治理（待启动）** | 对停止维护的 `serde_yaml` 制定替换或限期保留决定。比较真实配置语料、`!!js` 标签、patch 回写、数值/空值/重复键行为、错误诊断、MSRV 与解析资源边界；有差异就明确迁移规则。不得只换 crate 名，也不默认去掉 YAML。 |
| 3 | **R5 宿主模块整理与 panic 边界审查（待启动）** | 先记录状态所有权、锁顺序、回调与卸载约束；按 instance/dispatch/stream/staging 等职责分批机械迁移，保持公开面与行为。每批运行相关生命周期/重入/取消测试；锁中毒仅在找出具体可达 panic 路径后升级为缺陷，不统一吞掉 poison。 |
| 持续 | **R6 证据与文档维护** | 保留现有 CI，按变更影响选择额外验证；发布候选跑完整门禁。新增工具须说明现有命令无法覆盖的失败场景，不以行数或测试数扩充流程。历史文档加版本提示；平台结果保持绑定 run/commit，不把检查器通过写成行为证明。 |

R1 的记录必须区分“需求来源”“仓内集成”“独立工程实际使用”；目前有
`harness-spike` 与 ADR 0002 的 TS 下游取证，不能据此声称已有 Rust 生产消费者，
也不能断言项目没有任何消费者。2026-11-03 的技术接入与 2026-11-04 的 31 篇样例
归档任务都记录在 `../desolate/R1-REPORT.md`：任务形状已跑通，但仍待真实数据与
维护反馈，不能把样例任务写成生产采用。第三方复现是额外证据，不是必选门禁；
独立工程可以由同一维护者负责。

## A5 发布批次（保留状态）

| 顺序 | 工作 | 关闭条件 | 依赖 / 边界 |
|---|---|---|---|
| 1 | **A5-1 三平台 CI 首跑 + 平台矩阵实填** | Linux / Windows MSVC / macOS 实际编译、运行全量测试与真 DLL e2e；把每个平台的真实结果逐条填入 `docs/platform-matrix.md` | **已完成**：2026-11-02 [run 41460414477](https://github.com/yun1091724578/rust-cordis/actions/runs/41460414477)（commit `ea0489e`）三平台 `test` job 全绿；17 条保证 × 4 平台已填 `通过` |
| 2 | A5-2 夜间 soak | 在专机或 CI 定时跑更长轮次，记录资源 / 暂存台账 / 失败日志并归档 | **进行中**：2026-11-03 手动 dispatch [run 41562347773](https://github.com/yun1091724578/rust-cordis/actions/runs/41562347773) 首次 20k 通过，默认 1,000,000 轮也由 [run 41592298543](https://github.com/yun1091724578/rust-cordis/actions/runs/41592298543) 验证；每日 02:17 CST 的 `nightly-soak` workflow 已配置；cron 首跑与 artifact 长期转存未闭合 |
| 3 | **A5-3 正式首发封存** | 在干净发布源码树上重跑 `cargo xtask seal`，归档整个 `seal.json` / `SHA256SUMS` / `plugins` / `smoke` 目录；同版本 `inspect` 证据随产物归档 | **已完成**：0.1.0 rc2 与 0.1.1 封存/复验通过；0.1.1 源码摘要 `a1eab4cb…` |
| 4 | A5-4 历史产物 × 后续宿主 | 后续宿主版本加载已封存的首发产物，再把“历史发布版”从**待验证**转成**保证**或明确拒绝 | **已验证一次**：v0.1.1 宿主加载 0.1.0-rc2 封存插件通过（`dist/cordis-0.1.1-historical.log`）；仍缺可重复测试，因此不支持契约仍为待验证 |
| 5 | A5-5 关闭其余发布前项 | `Cargo.toml` 的 `repository` / `homepage` 元数据、公开发布地址、Windows SCM 真实 `sc create` / `sc start` / `sc stop` 实测；`release.md` 的发布前项关闭或明确延期 | 元数据已补；SCM 一次真实 elevated run 通过；剩余 nightly 长期转存与 SCM CI 重跑 |

**A5-1/A5-3 已完成；A5-2 已启用并完成首次手动 20k；0.1.0/0.1.1 已发布并封存；
A5-4 已用正式 v0.1.1 宿主验证一次；A5-5 剩余 nightly 长期转存与 SCM CI 重跑。
接入线 R1 技术接入与 31 篇样例归档任务已完成，待真实数据反馈；R2 已决定首发低层
ABI；R3 仓外使用契约与窄策略消费者已完成，完整重试/补偿留阶段 4；真实 agent
首条链路已完成正常、流失败、工具超时、普通模式真实超时、工具错误、模型启动
失败六模式，下一步是有界增量交付、真实模型与取消/预算。** 阶段 0 的平台出口已有
可引用的三平台基线（v0.1.1 run `42332542433` / `42332542329`），平台矩阵 17 条保证
已逐条填 `通过`；nightly soak 已有手动 dispatch 的 20,000 轮与默认 1,000,000 轮
通过、`soak.log` artifact 和每日 02:17 CST 的 schedule，cron 首跑与长期转存仍未
闭合。运行时代码、依赖或工具链变更需重跑受影响的平台验证；资源生命周期变更补跑
soak，发布候选执行完整门禁。旧 run/commit 的结果不能自动延续为新版本的验证结论。

A5-3 已完成；A5-4 已有一次正式 v0.1.1 验证，但仍缺可重复测试，因此支持契约保持
待验证；A5-2 的持续运行与 artifact 长期归档由 A5-5 收口。R1 的技术接入与 31 篇
样例归档任务已完成（`../desolate`），待真实数据与长期维护反馈；第三方复现只是
额外证据。R2 已决定首发走低层 ABI + 固定 revision 源码依赖，registry typed SDK
不在首发范围。R3 的仓外三入口与窄策略消费者已完成，完整重试/补偿仍留在阶段 4。
真实 agent 首链路已完成；下一步是有界增量交付、真实模型适配器、取消与预算，
并用真实使用反馈决定流式错误是否需要协议调整，而不是继续增加框架层。

**现在不要启动**：

- 阶段 4：没有真实调用事实消费者，不为了形式统一引入隐式上下文。
- 阶段 5 / 6：没有消费者和独立 ADR 的能力只登记，不实现。
- 事件接口：不新增返回协议；事件派发的调用者认知保持已知例外。
- T3：第三方/模型生成/多租户/硬抢占需求未出现前，不实现 WASM 或子进程载体。
- 线程池抢占：T2 同步调用不可取消；不要用“超时放弃等待”伪装成执行隔离。

## 第一批（交付物完成）

| # | 工作 | 产物 | 状态 |
|---|---|---|---|
| 1 | 支持契约 | `docs/support-contract.md` | 完成 |
| 2 | 结果与错误契约 ADR | `docs/adr/0003-result-and-error-contract.md` | 完成 |
| 3 | ABI 错误演进 ADR | `docs/adr/0004-abi-error-evolution.md` | 设计已接受；阶段 2 已实施，边界见 ADR |
| 4 | 保证索引检查器 | `cargo xtask docs` + `xtask/src/contract.rs` 单测 | 完成（55 bin + 17 CLI；文档 26 行 / 17 保证） |
| 5 | 失败路径清单 | ADR 0003 §7 | 完成 |
| 6 | 资源增长基线测试 | `crates/host/tests/resource_baseline.rs` + `staged.rs` | 完成（8 轮 × 7 路径；64 轮暂存；A5 另有本地 1000 轮与 nightly 20k/1M soak） |
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
  - 当前清单：23 个 ABI 常量、4 个结构 / 41 个字段、72 个宿主 / 核心 / SDK 公开入口、
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
- [ ] **正式首发产物封存**：`dist/cordis-0.1.0-rc2` 是候选封存，不是正式首发归档；第一次 release 时仍需在干净发布源码树上重跑 `cargo xtask seal` 并把整个封存目录归档到发布存储。
- [ ] **正式首发产物 × 同版本宿主**：候选封存的同版本 `inspect` 结果已存在，但正式首发时仍需重新归档；它只证明发布产物可用，**不证明跨版本兼容**。
- [ ] **历史产物 × 后续宿主版本**：等后续宿主版本加载已封存的首发产物，才把索引中的
      “历史发布版”从**待验证**转成**保证**或明确拒绝；不能停留在模糊状态。
- [x] **soak 入口与本地短时演练**：`cargo xtask soak [cycles]` 运行被忽略的
      `crates/host/tests/soak.rs`（默认 1000 轮；每轮成功 / 失败 + 两轮暂存，
      每 4 轮丢弃在飞 Future，每 3 轮重载）。2026-11-02 本地 1000 轮通过，
      资源、hook / runtime 与 `staged_leaked` 回基线。这是本地演练，
      **不是夜间 soak**。
- [x] **夜间 soak 入口、首次 20k 与默认 1M**：新增 `.github/workflows/nightly-soak.yml`，
      每日 02:17 CST 运行默认 1,000,000 轮并上传 `soak.log` artifact；2026-11-03
      手动 dispatch [run 41562347773](https://github.com/yun1091724578/rust-cordis/actions/runs/41562347773)
      的 20,000 轮首次通过（约 20 秒，Linux 单测试），`loaded_instances=0`、
      `staged_leaked=0`、`runtime=0`、`hooks=0`，日志 artifact 为
      `soak-log-41562347773`。2026-11-03 Windows GNU 本地 20,000 轮也通过（exit 0）。
      默认 1,000,000 轮随后由 [run 41592298543](https://github.com/yun1091724578/rust-cordis/actions/runs/41592298543)
      验证通过（约 38 分钟，`calls=applies=cleanups=disposes=2916668`，artifact
      `soak-log-41592298543-c1000000`）。后续 nightly 由 schedule 持续积累；
      cron 首跑尚未发生，artifact 保留 90 天，正式发布前需转存长期存储。
- [ ] 关闭 `release.md` 的其余发布前项：`Cargo.toml` 的 `repository` / `homepage`
      元数据、公开发布地址、Windows SCM 真实 `sc create` / `sc stop`；三平台结果与
      平台矩阵见上。

**阶段 0 出口（未满足）**：`docs/release.md` 必须能回答“哪些保证在哪些平台实际验证过”，
而不是只有一条 CI 通过结论。平台矩阵已经用三平台 CI 实填，夜间 soak 已有首次手动 20k
与默认 1,000,000 轮通过、每日 workflow；剩下的是 cron 首跑、正式首发产物封存、同版本
产物证据归档、历史发布版 × 新宿主，以及 nightly 证据的长期转存。该出口仍由 A5 关闭。

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
      `call_iface_detailed` 有真宿主 e2e；`dispatch_detailed` / `next_detailed`
      已有仓外真宿主回调覆盖（正常结果、结构化超时、卸载后不重复投递）；
      仓外窄策略消费者按 `category` / `certainty` 选择动作且不自动重试；
      完整重试/补偿策略仍属阶段 4
- [x] 边界测试：超长错误、非法 JSON、缺字段、未知码、空消息、未知字段
- [x] 手写插件测试：v1 夹具加载不越界 + `error_format` 探针
- [x] 超时清理 / 日志措辞（事件路径不证明调用者 `Unknown`）
- [x] 真实旧 ABI 插件返回 `STATUS_ERR`：独立 `tests/fixtures/abi-err-v1` 返回纯文本，
      `legacy_error` 验证新宿主保留文本、结构字段为 `unknown`，明确不是 `not_executed`；
      `tests/fixtures/abi-v1` 的源码字节未改；A5-1 只把 README 中按 CRLF 检出算出的
      `Cargo.toml` 哈希更正为 LF 字节哈希。
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
- [ ] 错误分类 / 重试消费者：仓外窄策略消费者已验证 `code` / `category` /
      `certainty` / `retry` 可驱动 `accept` / `fail_fast` / `degrade_unknown` /
      `unavailable`；完整重试/补偿只在真实调用方需要时实现，不自动重放有副作用的
      调用，仍需副作用类别、幂等是否生效、截止时间与预算
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
| 契约测试 | 语义与降级规则是否保持 | `support-contract.md` + `cargo xtask docs` | 26 行 / 17 保证；本地与首次三平台 CI 均绿 |
| 确定性交错测试 | 取消、完成、关闭同时发生时谁先 | `lifecycle_races.rs`、`abi_m2c.rs` | 已有测试；新增交错随变更补 |
| 资源回归 | 成功与失败路径是否都能回收 | `weak_audit.rs`、`resource_baseline.rs`、`staged.rs`、`soak.rs` | 8 轮 × 7 路径 + 64 轮暂存 + 本地 1000 轮 + CI nightly 20k/1M + 本地加强断言 20k |
| 兼容测试 | 旧 ABI 二进制（冻结快照）是否仍工作 | `abi_matrix.rs`、`tests/fixtures/abi-v1`、`frozen_plugin.rs`、`legacy_error.rs` | 冻结 v1 与真实 72B 旧插件错误已绿；历史发布版待 A5；旧宿主格是桩；旧 Rust 调用者 API 稳定性仍待单列 |
| 多平台验证 | 装载、卸载、信号、文件替换 | CI + `release.md` | Windows GNU 本地 + Windows MSVC / Linux / macOS 首次 CI 均通过；后续变更需重跑 |
| 压力测试 | 重复操作后是否稳定 | `resource_baseline.rs`（8 轮）/ `staged.rs`（64 轮）/ `soak.rs`（nightly 1M） | 本地 1000 轮、CI nightly 20k/1M 与本地加强断言 20k 有；1M 约 38 分钟，仍不是整夜；后续 nightly 持续积累 |
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
- 当前：55 bin + 17 CLI；文档 26 行 / 17 保证；`cargo xtask ci` 已包含
  `inventory --check`、`docs` 与 `matrix`，首发 `seal` 与 `soak` 是发布时入口，不在 `ci` 内。

### 第一批 #6：资源增长基线

- 范围：8 轮 × 成功 / 失败 / 丢弃 Future / 带在飞派发卸载 / waterfall / stream /
  同 key 重载；暂存台账 64 轮（`staged.rs`）。
- 绿档：`cargo test -p rust-cordis-host --test resource_baseline`（1 passed）与
  `cargo test -p rust-cordis-host --test staged`（4 passed）。
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
  72 个宿主 / 核心 / SDK 受控入口与 32 条载荷逐项校验源码 marker（必须恰好命中一次）。
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
  `not_executed`；冻结的 `abi-v1` 源码未修改（A5-1 仅更正 README 的 LF 哈希）。
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
- 门禁：`cargo xtask docs` 通过（26 行 / 17 保证）；`inventory` 23 / 4 / 41 / 56 / 32；
  demo 的装载报告已加入 `"pending":[]` 并重新通过字节级快照（4998 bytes）。

### A5-1：首次三平台 CI

- 2026-11-02 在私有仓库 `yun1091724578/rust-cordis` 首次运行 GitHub Actions；
  最终 [run 41460414477](https://github.com/yun1091724578/rust-cordis/actions/runs/41460414477)
  （commit `ea0489e`）的 `test (windows-latest)`、`test (ubuntu-latest)`、
  `test (macos-latest)`、`lint`、`msrv`、`package` 全部成功；2026-11-03 评审修正后
  [run 41560497289](https://github.com/yun1091724578/rust-cordis/actions/runs/41560497289)
  （commit `617f470`）再次全绿。
- 前三次运行暴露并修复了四个真实的跨平台问题，另有一次 lint 格式失败：
  1. `tests/fixtures/abi-v1` 的 `Cargo.toml` / `src/lib.rs` 冻结哈希依赖检出换行；
     新增 `.gitattributes` 固定 `tests/fixtures/**` 为 `eol=lf`，更新 README 中
     `Cargo.toml` 的 LF 哈希，并让 `frozen_plugin` 在算哈希前归一化 CRLF。
  2. demo 的配置目录与 Unix 二进制 `cordis-demo` 同名；改为 `cordis-demo-config`。
  3. harness-spike 同理，配置目录改为 `harness-spike-config`。
  4. inventory 读取源码时未归一化 CRLF，marker 在 Windows MSVC 上匹配失败；
     `read` 现在先归一化为 LF，并新增 CRLF 回归用例。
  5. 新增的 CRLF 测试未过 rustfmt，`lint` job 失败；`ea0489e` 格式化后通过。
- 平台矩阵 17 条保证在 Windows GNU / Windows MSVC / Linux / macOS 四列均填 `通过`；
  Windows GNU 来自 2026-11-03 本地 `.\cargo.cmd xtask ci`（exit 0），其余来自上述 CI 的
  完整测试 + demo + e2e。
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
- 每轮成功 / 回调失败 + 两轮暂存台账 `stage -> adopt -> drop`；每 4 轮额外
  丢弃在飞 Future、每 3 轮额外做**带在飞派发**的同 key 重载（1000 轮分别为
  250 / 334 次）。重载前先证明 `pending_len(old_handle) == 1`，再断言 update
  把在飞调用结算为没有意见。
- 断言：mount 后 effect 数；dispose **前**派发 / 挂起为零；dispose 后
  `cleanup_error == None`、effect 为 0、`loaded_instances == 0`、
  `runtime_count == 0`、事件 hook 为 0；`staged_leaked` 回到 0。最终对
  `CALLS` / `APPLIES` / `CLEANUPS` / `DISPOSES` 断言**精确总数**
  `2*cycles + ceil(cycles/4) + 2*ceil(cycles/3)`，不再接受“至少一次”。
- 失败诊断：`FailureTrace` 在 panic 展开时打印当前 cycle / stage / handle
  以及 `loaded_instances`、`staged_leaked`、`pending_len`、`dispatches_len`、
  `effect_count`、runtime / hook 快照；progress 行只是每 1000 轮的心跳。
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

### A5-2：nightly soak 入口与首次手动 20k

- 新增 `.github/workflows/nightly-soak.yml`：每日 02:17 CST（18:17 UTC）运行
  默认 1,000,000 轮，`workflow_dispatch` 可覆盖 `cycles`；日志通过 `tee` 写入
  `soak.log`，并由 `actions/upload-artifact@v4` 上传，artifact 保留 90 天。
  run step 单独设 170 分钟超时，job 超时不会先吃掉日志上传。
- `cargo xtask soak` 先运行 `--ignored --list`，确认
  `repeated_soak_cycles_leave_no_residue` 仍存在且被 `#[ignore]`；测试被删、
  改名或去掉忽略标记时直接失败，避免 `cargo test --ignored` 的 0 测试假绿。
- `.cargo/config.toml` 的 `xtask` alias 已加 `--locked`，nightly 最外层构建
  不会静默重写 `Cargo.lock`。
- 2026-11-03 **手动 dispatch** [run 41562347773](https://github.com/yun1091724578/rust-cordis/actions/runs/41562347773)
  （commit `289d6b8`）：20,000 轮通过，`elapsed_ms=20236`；
  `loaded_instances=0`、`staged_leaked=0`、`runtime=0`、`hooks=0`；
  artifact `soak-log-41562347773` 已上传。该 artifact 来自加强断言之前的
  版本；cron 尚未触发。
- 2026-11-03 Windows GNU 本地 `.\cargo.cmd xtask soak 20000` 也通过
  （exit 0，`elapsed_ms=133869`，同样的资源计数回零）。
- 2026-11-03 加强断言后 Windows GNU 本地再次 20,000 轮通过：
  `elapsed_ms=201893`，`calls=applies=cleanups=disposes=58334`；该版本包含
  带在飞派发的同 key 重载与精确总数断言。
- 2026-11-03 默认 1,000,000 轮手动 [run 41592298543](https://github.com/yun1091724578/rust-cordis/actions/runs/41592298543)
  通过：`elapsed_ms=2265533`（约 37 分 46 秒），
  `calls=applies=cleanups=disposes=2916668`；artifact
  `soak-log-41592298543-c1000000` 已下载抽查，日志含 `rustc 1.99.1`、
  `Cargo.lock` SHA-256 `7419d157…` 和完整 `soak complete` 行。
- `soak.rs` 每 1,000 轮输出一次 `soak progress:`（第 1 轮、第 1001… 轮与末轮），
  全部断言通过后才输出 `soak complete:`；失败时由 `FailureTrace` 打印当前
  cycle / stage 和 pending / dispatch / effect / staged 快照。
- 边界：当前 nightly 只在 Linux runner 跑单个测试；默认 1,000,000 轮实测约 38 分钟，
  首次手动 20k 约 20 秒，仍**不是整夜长跑**；暂存部分仍是 `stage_plugin` /
  `StagedFile` primitive，不覆盖真 DLL `open_staged`；soak 是重复型压力，不扩展
  waterfall / stream / 带在飞派发 dispose 的路径覆盖。artifact 保留 90 天，
  正式发布前需转存长期存储。后续 nightly 结果持续积累，不能把首次 20k 当成永久
  保证；`#[ignore]` 测试也不能作为支持契约里 `保证` 行的证据。

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
