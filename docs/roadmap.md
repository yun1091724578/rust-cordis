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

## 当前判断（2026-11-12）

| 工作线 | 状态 | 已落地 | 未闭合 |
|---|---|---|---|
| A. 低层 ABI 与发布 | **基本完成** | 0.1.0/0.1.1 发布；正式 `/2` 封存；公开仓 CI；版本矩阵；SCM 一次实测 | 严格同版本封存证据、历史发布版可重复测试 |
| B. 信任模型 | **已接受并实现** | ADR 0005；来源 allow-list、默认拒绝、seal sidecar、inspect/status provenance | 来源治理不是执行隔离 |
| C. 可信单租户底座 P0 | **按边界完成** | 来源策略、App 粒度策略、容量准入、锁纪律、容量 waterfall 专项测试 | per-plugin/method 明确不提供；永久 hang 属 T2 |
| D. 长期运行 P1 | **基本完成** | 1M soak、RSS、真实 `open_staged` soak、dhat、版本矩阵、semver 计划、调用缓冲池 | 从 `PluginDesc` 自动采集、升级/回溯求解 |
| E. agent 流处理 | **有界增量交付完成** | 六模式、23 个 agent_stream 单测、即时 JSONL、事件/终止/文本/工具上限、事件窗口、run_id、模型 idle timeout | 真实模型、慢消费者、取消/预算、SSE 在线 sink |
| F. T3 / 不可信插件 | **条件性待启动** | ADR 0001 预留位置 | 第三方/模型生成/多租户/硬抢占触发时另立 ADR |
| G. 维护治理 | **持续** | R4/R5/R6 已登记 | serde_yaml、宿主模块、锁/panic 审查 |

### 基线

- 支持契约：**26 行 / 17 条保证**
- API 清单：**23 常量 / 4 结构 / 41 字段 / 76 公开入口 / 32 载荷**
- 最新全绿三平台 CI：`43196946690`
- 正式 `/2` 封存：`dist/cordis-0.1.0-formal`、`dist/cordis-0.1.1-formal` 与 Release assets
- 仓库公开；标准 GitHub-hosted Runner 不再消耗私有分钟
- `desolate` agent_stream：23 个单测通过；该工程无 git

## 最近交付

### P0-3：容量与准入

- `HostLimits`：instances / dispatches / pending / streams；默认 1024/1024/256/32；
  0 非法，不解释成无限。
- 检查与插入同锁；拒绝返回 `host_capacity/capacity/not_executed`，不执行插件代码。
- 实例、派发、挂起、流拒绝与计数回基线：`crates/host/tests/capacity.rs`。
- 入站事件容量拒绝会**继续执行 waterfall 下游**；专项测试
  `capacity_rejection_does_not_drop_waterfall_tail`。

### P0-4：锁纪律（按 T2 边界关闭）

- 回调期间不持实例表/派发表锁；卸载清理等待 lifecycle 写锁不再阻塞 tokio worker。
- `wait_lifecycle_exclusive`：有 runtime 用 blocking pool；无 runtime 同步等待；
  JoinError/取消退回同步等待；锁中毒取回 poisoned guard。
- 测试：回调自卸载、阻塞回调 + 并发 dispose、实例归零。
- **永久 hang 不可抢占**；panic/锁中毒未发现可达 trigger，但不承诺不存在。

### P1-1：semver 依赖计划

- `crates/host/src/plan.rs`：`(id, version)`、provides、requires range；选择最高满足版本；
  拒绝缺失、不可满足、重复身份、同版本多提供者。
- 显式 `LoaderConfig.dependencyPlan` 在挂载前求解；非空时覆盖 manifest 计划。
- sidecar manifest 可选 `provides` / `requires`；`DiskPluginSource::plan_entry`
  读取 sidecar；初装、对账、`reload_one` 前都重跑预检。
- 同插件多实例的相同计划去重；`disabled: !!js` 保守纳入；混合来源（部分行无
  sidecar）跳过整次 manifest 预检，避免误拒。
- 有来源策略时 sidecar 先过 admission。
- reload 预检失败回滚 `main_only` / `owners` / `main_baseline`；
  `set_disabled` 失败回滚树并返回 `Err`。
- `LoaderConfig.strictSidecars` 默认 false：坏 sidecar 跳过预检；true 则装载前硬失败。
- seal 写真实 key：`sessions@1`、`tool.harness-echo@1`、`harness.status@1`、
  `harness.stream@1`，并保留 hello 的 PENDING 语义（不写硬 requires）。
- 旧 `/2` 没有依赖字段仍可 verify；`--require-v2` 可拒绝把 `/2` 改成 `/1` 绕过 provenance。
- 剩余：`PluginDesc` 自动采集、版本升级/回溯求解；manifest 声明只做预检，不是运行时授权。

### P1-4：调用缓冲池

- `invoke_raw_with` 使用线程本地 16 KiB `CallBuffer`；标准大小复用；
  `TOO_SMALL` 扩容后的大缓冲不入池；回收前清零，避免跨插件残留。
- 手动基准：
  ```text
  cargo test -p rust-cordis-host --lib call_buffer_pool_benchmark -- --ignored --nocapture
  ```
- 这是缓冲复用，不是零拷贝，也不改变 16 KiB 初始上限。

### E：agent 有界增量交付

- JSONL sink emit 即写/flush；超限事件在写文件前拒绝，不留下半行。
- `StreamLimits`：事件 64 KiB、终止 512 KiB、10k 事件、256 KiB 文本、1024 工具、
  流式内存窗口 256 条；0 值拒绝。
- 终止事件独立预算，不被 `maxEvents` 饿死。
- 工具 arguments/result 超预算截断为 `{truncated:true,...}`，保留原 status。
- sink 写入/flush 失败回退到写入前偏移；显式 `run_id` 进事件与 report；
  模型流 30 秒 idle timeout。
- 剩余：真实模型适配器、慢消费者/队列、取消与预算贯穿、sink 故障注入测试、
  SSE 在线 sink。

## 下一步（按顺序）

### 1. 真实模型适配器（基础版已落地）

- `desolate-agent-model` 支持可选 OpenAI-compatible 纯 HTTP SSE：
  - `choices[0].delta.content` → `text.delta`；
  - `[DONE]` → stream close；
  - 流内 `error.message` → `stream_fail`；
  - EOF without `[DONE]` → failure；
  - `--model-api-key-env` 从环境读取凭据，不进事件。
- `--model-endpoint` / `--model` 已接入 `desolate agent`；无 endpoint 时保留确定性模式。
- 已测：URL、SSE token、`[DONE]`、error、EOF、停止消费，共 7 个 adapter 单测。
- **未完成**：HTTPS/TLS、工具参数增量、usage、真实 provider 人工 smoke、
  本地 mock HTTP 的 CI 集成。HTTPS 需要 TLS 客户端或外部 reverse proxy。
- 不新增 `Last-Event-ID` replay，不把模型错误自动重试。

### 2. 取消与预算（模型预算基础版已落地）

- `desolate agent --total-timeout-ms`（默认 60000）包住整个 chain；
  `--first-token-timeout-ms`（默认 30000）与 `--idle-timeout-ms`（默认 30000）
  分别约束首包和 token 间隔。
- 到期输出 `timed_out/unknown`、`HOST_TIMEOUT/timeout`，report 标
  `incomplete=false`；模型超时不再继续调用工具。
- 已做立即总预算和“本地 HTTP 接受连接但不回包”的 first-token 端到端 smoke：
  4 个事件，最后是 `run.finished`。
- **未完成**：connect/工具/sink 的独立预算；模型、工具、sink 的显式取消；
  取消与 sink 写入竞争；provider 忽略 socket close 的兜底。
- 记录 termination reason、certainty、run_id、last_seq，避免把“断流”写成成功。

### 3. SSE 在线 sink

- 先做本地 HTTP SSE；终止后关闭响应体。
- 断流但没有终止事件 = truncated run；第一版不做 replay。
- JSONL 继续保留为本地证据格式。

### 4. P1 版本治理收尾

- 从 `PluginDesc` 自动采集 provides/requires，而不是只靠 sidecar/手写表。
- 决定是否引入 `provides`/`requires` 的 ABI 尾字段；ABI 仍精确匹配，不引入冲突的
  `abi_min/abi_max` 双轨规则。
- 版本升级/回溯求解必须另有真实消费者和测试，不先造抽象。

### 5. 发布证据收尾

- 严格同版本封存：用与封存 `source.sha256` 相同的源码树构建宿主再跑 smoke。
- 历史发布版 × 新宿主补可重复测试，不把单次手工演练写成保证。
- `--root` 的 toolchain 探测已走目标树目录；`RUSTFLAGS`/`.cargo/config`
  自定义 rustflags 仍待完整记录。
- `--require-v2` 已提供，但默认 verify 仍兼容 `/1`；发布门禁必须显式使用。
- seal source digest 仍缺 tag/commit 字段绑定。

## 阶段 0：验证基线

**已完成：** 三平台 CI、平台矩阵、支持契约、API 清单、冻结 ABI v1、资源基线、
1M soak、正式 `/2` 封存、公开仓。

**仍未闭合：**

- 严格同版本封存证据：两个正式封存的 smoke 都由当前 0.1.1 工作区宿主执行，
  `host.source_sha256 != source.sha256`。
- 历史发布版 × 新宿主仍缺可重复测试。
- nightly cron 首跑与 artifact 长期转存。

**出口：** `docs/release.md` 能逐条回答“哪个保证、哪个平台、哪次 run/commit”；
没有把结构检查或单次手工验证写成持续保证。

## 阶段 1：结果与错误语义

**完成：** `Returned/Failed/TimedOut/Cancelled/Disconnected`；
`NotExecuted/Reported/Unknown`；结构化错误载荷、旧 ABI 文本降级、bounded message；
事件路径仍无返回协议，是已知例外。

**边界：** 不承诺自动重试安全或 exactly-once；SDK 提供事实，不提供重试许可。

## 阶段 2：结构化错误与调用通道

**完成：** `error_format` 能力位、sidecar 载荷、host/SDK detailed 入口；
iface/dispatch/next 超时、卸载/取消、provider 业务错误；旧插件文本错误端到端。

**未完成：** 异步 `fulfill_error`；所有同步失败路径的 detailed payload；
真实旧宿主四格矩阵仍部分是桩。

## 阶段 3：声明与装载诊断

**完成：** 配置 schema 可见性、Pending 精确原因、来源/注入声明；
inspect/status provenance、默认拒绝来源策略。

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
| A5-3 正式 `/2` 封存 | 封存完成，strict same-version 未满足 | `xtask seal --root`；`dist/cordis-0.1.0-formal`、`0.1.1-formal`；Release assets；`--require-v2` 可用 |
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
