# 分阶段执行计划

本文件是唯一执行计划入口：当前判断、下一步顺序、阶段出口与证据索引都以这里为准。
根目录的设计文档保留设计背景，不作为当前状态来源。

- 支持范围、非目标与保证索引：`docs/support-contract.md`
- 结果与错误契约：`docs/adr/0003-result-and-error-contract.md`、`docs/adr/0004-abi-error-evolution.md`
- 信任模型与插件来源：`docs/adr/0005-trust-model-and-plugin-source-boundary.md`
- 公开 API 稳定与 0.2 边界：`docs/adr/0006-public-api-stability.md`
- 版本诊断边界：`docs/adr/0007-version-is-diagnostic-only.md`
- 来源准入的作用域与优先级：`docs/adr/0008-source-admission-scope.md`
- RSI 部署与观测底座的位置：`docs/adr/0009-rsi-deployment-substrate.md`
- 发布门禁与未确认项：`docs/release.md`
- 封存格式：`docs/release-seal.md`
- 公开 API / 载荷清单：`docs/api-inventory.md`
- 架构现实审查：`docs/architecture-review.md`
- 冻结 ABI v1：`tests/fixtures/abi-v1/README.md`

## 执行规则

- 按任务类型验收：bug 修复须确定性业务红测；结构移动保留行为特征并前后绿；评估提供可重跑比较及不迁移决策；新能力提供旧限制探针和新接口正反测试，不要求不存在的 API 在旧版编译。
- 文档项核对来源、引用和状态一致性，不制造假红测；“保证”行必须被 `cargo xtask docs` 校验到真实、未忽略的测试。
- 阶段结束时同时更新本文件、支持契约和发布门禁；只改代码不改状态视为未完成。
- “完成”只指验收出口满足，不等于交付物存在或局部测试通过。

## 当前判断（2026-11-12）

| 工作线 | 状态 | 已落地 | 未闭合 |
|---|---|---|---|
| A. 低层 ABI 与发布 | **基本完成** | 0.1.2 发布；0.1.2 严格同源 `/2` 封存；0.1.0/0.1.1 正式封存；公开仓 CI；版本矩阵；SCM 一次实测 | 历史发布版可重复测试 |
| B. 信任模型 | **已接受并实现** | ADR 0005；来源 allow-list、默认拒绝、seal sidecar、inspect/status provenance | 来源治理不是执行隔离 |
| C. 可信单租户底座 P0 | **按边界完成** | 来源策略、App 粒度策略、容量准入、锁纪律、容量 waterfall 专项测试 | per-plugin/method 明确不提供；永久 hang 属 T2 |
| D. 长期运行 P1 | **基本完成** | 1M soak、RSS、真实 `open_staged` soak、dhat、版本矩阵、semver 计划、调用缓冲池 | 从 `PluginDesc` 自动采集、升级/回溯求解 |
| E. 仓外消费者证据 | **外部验证，不是本仓主线** | `desolate` 的 agent 链、HTTP SSE adapter、预算 smoke | agent/model/SSE 协议不进入核心 SDK；只有被两类消费者共同需要的原语才立 ADR |
| F. T3 / 不可信插件 | **条件性待启动** | ADR 0001 预留位置 | 第三方/未按 §2.3 签发的模型生成/多租户/硬抢占触发时另立 ADR |
| G. 维护治理 | **持续** | R4/R5/R6 已登记 | serde_yaml、宿主模块、锁/panic 审查 |

### 基线

- 支持契约：**26 行 / 17 条保证**
- API 清单：**23 常量 / 4 结构 / 41 字段 / 80 公开入口 / 32 载荷**
- 最新全绿三平台 CI：`43196946690`
- 正式 `/2` 封存：`dist/cordis-0.1.0-formal`、`dist/cordis-0.1.1-formal`、`dist/cordis-0.1.2-formal`（0.1.2 严格同源）与 Release assets
- 仓库公开；标准 GitHub-hosted Runner 不再消耗私有分钟
- `desolate` 消费者证据：27 个 host 单测 + 7 个 HTTP adapter 单测；该工程无 git，
  不作为本仓交付门禁

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
- 有准入结论（含默认拒绝）时 sidecar 先过 admission。
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

### E：仓外消费者证据（`desolate`，非本仓交付）

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

0.2 公开类型迁移**暂缓**；当前优先收口发布证据与维护线，不为了版本号而制造
breaking 变更。

本仓是 **SDK / 运行时底座**，不是 agent 产品，也不是网络微服务框架。
`desolate` 只是消费者验证工程；agent 事件、模型适配器、SSE 传输属于消费者侧，
不能反向变成 host/core 的产品主线。

RSI 方向按下述口径推进：本仓做**演化产物的部署与观测地板**，不做演化循环
（ADR 0009）。阶段 1 的 WP0（两份 ADR）已完成，WP1（iface 归因）/ WP2（变体
身份）/ WP2b（契约方法面）/ WP4（端到端示例）见 `rsi-sdk-roadmap.html`。

### 运行时优化专项（2026-11-16）

实施规格见 [运行时正确性与可维护性优化实施方案](runtime-optimization-plan.md)。
这是现有维护线的具体化：先复现并修复关闭、异常结算和句柄回收问题，再拆模块。
以下为专项执行状态，不覆盖历史已通过证据；未标明已验证的新增路径仍待复现。
任务状态仅在此处维护；实施方案只维护规格与验收。

| ID | 任务 | 当前状态 |
|---|---|---|
| O1 | 关闭准入、摘表后 done 所有权与再入 | 本批实现验收完成：A/B 及本轮 C/D/E 已验收；C 原始先红证据缺口显式保留，后补安全变异不替代原始 TDD 证据 |
| O2 | 派生句柄原子回收 | 实现验收完成：同锁 owner/runtime 校验与 remove、root 锁外析构；6 项测试红绿闭合，完整本机 CI 通过；额外全 feature / 全 target Clippy 失败保留，详见本批记录 |
| O3 | 首次与后台 poll panic 及正常收尾 | 实现验收完成：首/后台 poll panic 结算，见下方 O3 记录 |
| O4 | 公开 keepalive 与 observer lease 所有权 | 实现验收完成：observer lease 与唯一 Arc owner 均已落地，见下方 O4 记录 |
| O5a | reload 整份候选排队、淘汰与采纳 | 实现验收完成：候选携带元数据、代次排序、回写版本失效与重建，见下方 O5a 记录 |
| O5b | Replace 失败语义与下一轮恢复 | 实现验收完成：Replace 预检前移、失败语义与下一轮恢复，见下方 O5b 记录 |
| O6 | 消费者全局状态隔离与服务结果断言 | 实现验收完成：消费者进程隔离、真实服务断言与准入负向用例，见下方 O6 记录 |
| O7 | 声明预检分类报告与真实绑定联动测试 | 实现验收完成：预检分类报告、真实绑定联动测试，见下方 O7 记录 |
| O8 | credit 拒绝兼容性与 ABI 扩容边界 | 实现验收完成：credit 上限、只读扩容上限与测量边界，见下方 O8 记录 |
| O9 | 贯穿 reload_one 与暂存验证的局部来源策略 | 实现验收完成：显式来源准入、双源隔离与 reload/probe 单快照，见下方 O9 记录 |
| O10 | 门禁与发布证据补缺 | 进行中：本地门禁（A/B）与 package 隔离验收（C）已落地；剩余证据表、契约收口与 workflow 授权项 |
| O11 | 宿主按状态所有权拆分 | 等待正确性批次完成 |
| O12 | YAML 语料与替代库评估 | 语料待启动，替换未决策 |
| O13 | 接入文档与平台路径边界 | 待实施 |

代码合入顺序优先 O1 → O2 → O3 → O4；O6 可独立先行。O5a/O5b 排在前四项之后是排期，不是技术依赖。
新增竞态与异常路径完成复现前，不把旧 P0 锁纪律或关闭测试的绿灯扩大解释为已覆盖。
实施方案经 A（运行时）、B（测试交付）、C（架构配置）独立审核、修订及复核后通过；记录见方案第 9 节。这仅确认文档可实施性，不表示代码修复或测试通过。
文档阶段未修改运行时代码、CI 或发布配置；现在按小批 TDD 实施，验收通过才进入下一子任务。

#### O1-A：关闭后新增工作准入（2026-11-16）

- 红测：`abi_close_admission` 原实现放行关闭回调中的 iface；`--lib closed_` 两项分别暴露 next 消费有效 frame、关闭返回 capacity。均为实际业务断言失败。
- 修复：insert/next 准入和 closed 写入使用同一 dispatches 锁；关闭后不消费 frame、不挂接 task。新拒绝为 `host_lifecycle/lifecycle/not_executed`，仅表示该子调用未执行；已接受调用卸载仍为 `unknown`，清理 get/require 保留。
- 绿测：host lib 60 passed / 1 ignored（原有手工 benchmark），指定 ABI/关闭/容量/next 集成测试 21 passed；新增 `abi_close_admission.rs` 一项通过。
- 完整门禁：首次 `cargo.cmd xtask ci` 在两项 Clippy 检查失败；按现有局部错误载荷约定修正后完整重跑退出 0。不是忽略失败后继续。
- 独立验收：并发/锁序、测试/错误契约两个视角均无阻塞；Clippy 修正后再次独立核查通过。只确认本批，不代表 O1 全部完成，也不替代三平台结果。
- 历史批次下一子项（O1-A 当时记录，非当前待办）：正常摘表后的 done 可达性及任务退出所有权；observer 调用 lease 仍与 O4 协调。未提交代码，未改 CI/发布配置。

#### O1-B：业务终态与任务退出分离（2026-11-16）

- 红测：正常摘表后关闭丢失退出凭证、fulfill 摘表后关闭丢失 pending next，两个实际断言失败；stream settle 关闭后仍准入的探针失败。修复后转绿。
- 实现：实例级 tasks 独立保存 cancel/done，任务退出主动摘除；future/交付捕获物先析构，最后发布退出。正常业务摘表不自取消，关闭仍能取消并等待已无业务记录的任务。stream settle 纳入同一退出协议；子 fiber/自卸载 driver 不纳入自身等待集合。
- 首轮独立验收发现同步成功收流后返回 PENDING 被误报为错误。新增真实 fulfill/stream_close 业务红测后修复，保留成功流及 EOF。取消子树收集与摘表改为同锁；终态旧 dispatch id 的 abort 失效仍是原契约，不扩展永久取消树。
- 第二轮要求补足正常 next 完成，而非只测关闭取消。已补真实 next pending、正常/fulfill 摘表、放行尾链、一次正确回调、关闭前 tasks 自动归零；这是补充保护测试，不冒充旧码红测。
- 最终验证：`--lib o1_` 7 passed，host lib 67 passed / 1 ignored；host 全部 integration 曾通过；最终 `cargo.cmd xtask ci` 退出 0。两次 fmt/命令问题均修正后重跑，不计作业务红测。
- 验收：运行时所有权/锁序复核无阻塞；端到端验收提出的测试缺口已补并独立确认。复核仅静态读代码，动态结果来自上述实际执行；未声称三平台通过。
- 历史批次边界及下一步（O1-B 当时记录，非当前待办）：O3 的 panic 错误交付、O4 的 observer lease 尚未完成，O1 整项仍在进行中；下一步先处理 observer 的关闭交付协议，不跳至 O2。

#### O1-C：observer credit 债务与调用 lease（本轮）

- 实现：observer credit 债务按登记次数结算，不按 id 去重；以 `ActiveCall` 替代跨 FFI 持有的 lifecycle 读锁，实际插件调用持有 lease，关闭交付覆盖 abort 与 dispose。
- 原始证据缺口：首次实施中断，原红测证据无法恢复。不能称本项具有原始先红证据，也不能用后补变异改写首次实施经过。
- 后补安全变异：漏交 credit 时实际结果为 `[]`，预期为 `[9,9,10]`；close 不持 lease 时，abort/dispose 观察到 `alive == false`。两项均暴露安全断言失败，仅作为后补变异证据。
- 恢复与验证：撤销变异后恢复 hash 一致，8 项测试通过。两个独立角度完成各轮审查，最终均无阻塞；该结论不补齐原始 TDD 证据。

#### O1-D：正常异步 iface/dispatch 终态认领（本轮）

- 实现：正常异步 iface/dispatch 终态原子认领，交付所有权与关闭竞争遵循同一协议。
- 红绿证据：两个实际业务红测均为 callback 次数实际 0、预期 1；修复后转绿。
- 审查补充：精确断言 `kind/id/result`，补齐 legacy 关闭错误验证；4 项测试通过。两个独立角度完成各轮审查，最终均无阻塞。

#### O1-E：取消/超时交付 lease 与宿主清理（本轮）

- 实现：取消/超时 claim 取得交付 lease；host-only 路径禁止 FFI，但仍清理流、tx、任务与 credit，不能因无法调用插件而跳过宿主清理。
- 红绿证据：初轮两个竞争红测修复后转绿。审查发现 `deadWeak` 跳过宿主清理，随后新增真实 abort/timeout 业务红测，再修复转绿；不是仅凭静态审查宣布闭合。
- 补充保护：覆盖 mixed pending credit 与真实 next，并单独验证 task 退出；6 项测试通过。两个独立角度完成各轮审查，最终均无阻塞。

#### O1 本批汇总与剩余边界

- 当前结论：O1 本批实现验收完成；A/B 历史证据保留，C/D/E 证据分别记录。C 原始先红证据缺口仍在，不宣称整个 O1 已具备完整原始 TDD 红绿链。
- 最终验证：host lib 85 passed / 1 ignored，observer 18 passed。主代理实际完整执行 `.\cargo.cmd xtask ci`，日志 `bg-dda536.log:1425` 记录退出 0，包含真实 DLL 路径；不是本次文档更新重新执行的测试。
- 未覆盖边界：本机门禁不代表所有平台通过；不扩展永久取消树，不宣称 next 独立关闭终态、O3 panic 错误交付或 O4 唯一 Arc/强 owner 问题已修复。O4 仅 observer lease 随 O1 完成，唯一 owner 仍待后续批次。
- 后续顺序：按原排期进入 O2，再处理 O3 与 O4 剩余项。本次仅更新 roadmap，不修改代码、CI 或发布配置，不提交；历史测试、门禁和失败记录不回溯改写。

#### O2：派生句柄原子回收（本轮）

- 当前结论：O2 实现验收完成；运行时代码与测试契约两个独立视角均通过。O3 仍待复现 / 待实施，作为下一项推进；上文 O1 汇总中的进入 O2 是当时的排期记录。
- 实现：派生句柄的 owner/runtime 校验与 remove 在同一锁内完成；root 回收将析构移到锁外。
- 红绿证据：新增 `derived_dispose_tests.rs`，共 6 项测试。旧实现实际运行结果为 3 passed / 3 failed：两个竞争测试的 child 均退出 101，触发 owner checked panic；锁探针观察到 instance `(false, true)`、derived `(true, false)`。修复后 6 passed。
- 回归验证：host lib 91 passed / 1 ignored，`abi_m1` 4 passed、`m1_use` 4 passed、lock 1 passed；默认 feature 的 `clippy --all-targets` 通过，`--all-features` lib 通过。
- 完整门禁：主代理实际完整执行 `.\cargo.cmd xtask ci`，日志 `bg-8e9664.log` 记录退出 0，真实 DLL 层也通过。以上为已执行结果，本次文档更新未重新运行测试或门禁。
- 额外检查失败保留：`clippy --all-targets --all-features` 因未修改的 `dhat_profile.rs:14` 中 unused import `Plugin` 失败。本批不修该项，不称全 feature 全绿。
- 边界：未修复派生创建与 root 注销之间的迟到插入，不扩展 runtime shutdown；本机验证不是三平台验证。本次仅更新 O2 实际完成记录，不修改代码、CI 或发布配置，不提交。

#### O3：panic 结算与收尾（2026-11-17）

- 当前结论：O3 实现验收完成；首 poll 与后台 poll 的 panic 都收敛为可观察结算，正常收尾路径未变。上文 O2 记录中的 O3 待办是当时的排期记录。
- 红测：`--lib o3_` 首轮 1 passed / 7 failed，失败均为业务断言（`首 poll panic 不得留下永远不结算的登记记录 left:1 right:0`、后台 panic 的「看门狗超时」、emit 记录残留）。另在仓库外克隆树把 `CatchUnwind` 收回 `resume_unwind`，取得两条集成测试的真实失败：`dispatches` 残留 1、回调未到达。`o3_next_first_poll_panic` 与 `o3_on_done_panic` 是保护/诊断测试，已在测试注释标注，不冒充旧码红测。
- 实现：新增借用式 `CatchUnwind`（不改 `TaskWork` 字段析构序）、`DeferFailure{NoRuntime,Panic}`、`panic_payload`（`code=host_panic`，category/certainty/retry 保持 `unknown`，不写 `not_executed`）；iface/dispatch 首 poll panic 先摘表再返回错误；next 不摘入站记录；emit 只诊断并摘表、不伪造回调；`on_done` panic 记诊断。生产路径只有 `eprintln` 加 `cfg(test)` 诊断登记。
- 绿测：`--lib o3_` 9 passed；host lib 100 passed / 1 ignored；`panic_future_iface_sync`、`panic_future_background` 各 1 passed。初次版本两测试同二进制断言 `loaded_instances()==0` 并行互踩，已按仓库约定拆成一测一二进制，并去掉 fake C 入口里的 `unwrap`。
- 验收：运行时角度（恰好一次交付、无二次 poll、锁序、O1 析构序、错误契约）通过；测试角度在首轮阻塞（并行污染）修复后复核通过；兼容/范围角度确认仅改 host、无新公共 API、无 core/ABI/依赖变更、非 panic 路径错误码无漂移。
- 门禁：`.\cargo.cmd xtask ci` 退出 0（含真 DLL 层）。复核另做变异验证：摘掉 `catch_unwind` 后 8 failed / 1 passed，并判定只有「（scene，…已在 poll 边界截获」这条文案能把 O3 与 ABI 兜底区分开；恢复后 SHA256 一致、生产零改动。
- 未覆盖边界：不承诺 `extern "C"` abort、OOM/abort、panic=abort、panic-in-Drop、永久同步 hang；`spawn_stream_settle` 的 poll 仍是盲区（只影响流式 settle，归 O4/O8 批次）；`on_panic` 交付闭包自身未 catch；首 poll panic 的「`TaskExit` 释放 → 调用点摘表」之间仍有极窄窗口（需插件从无 `ActiveCall` 的线程调用并恰好撞上关闭排水）。
- 契约落点：新的宿主侧保证「future poll panic → 恰好一次 `host_panic` / emit 诊断」尚未写入 `docs/support-contract.md` 与 `docs/platform-matrix.md`，原因是契约行需要同步 `xtask/src/contract.rs` 的硬编码行数计数（26/17）与矩阵四平台列；按最小改动原则留到 O10 契约收口时一并处理，这里显式记录，避免被读成已完成。

#### O4：keepalive 所有权（2026-11-17）

- 范围：O4 分两块。observer 调用 lease 已随 O1-C 完成；本批只做公开 keepalive 的唯一 Arc owner，其余语义不动。
- 红测：新增 `crates/host/tests/abi_keepalive_owner.rs`（一测一二进制），把唯一 `Arc<DropProbe>` 经 `apply_via_with_keepalive` 移交。旧实现实际红在第一条断言「apply 成功后探针不得 drop」（`left:true right:false`），观测值 `dropped_after_apply=true event_calls=0 disposes=0`。第二条（保护期内事件监听器必须被调用一次）与第三条（关闭后 `OP_DISPOSE` 恰一次）是独立判据，不是同一条断言的重复计数。
- 实现：强 owner 存进实例表槽位（`KeepaliveOwner`），`Instance.keepalive` 仍是 `Weak`；`unregister_instance` 在两把表锁释放之后才 drop instance 与 owner；`call_lease`/`admit_call`/close lease/drain 的 host-only 分支逻辑未改；`apply_via`（keepalive=None）语义不变。
- 测试前提变更（显式记录，不能读成删测试）：注册期内 dead Weak 不再自然可达，o1c/o1e 的 dead-weak 用例改用 `#[cfg(test)]` 的 `force_dead_weak` 探针复现旧状态，全部原断言保留；另新增 `take_slot_owner` 探针，使五处用 `DropProbe` alive 代理 lease 的断言恢复判别力（`cancel_close_window`、`o1d_outbound_terminal_window`、`o1c_admitted_callback_lease_and_close_wait`、`o1c_close_lease_covers_gone_abort_dispose`、`o1d_close_wins_keeps_outbound_for_abort`）。
- 变异证据：弱化 close lease 时 `o1c_close_lease_covers_gone_abort_dispose`、`o1c_admitted_callback_lease_and_close_wait`、`o1d_outbound_terminal_window`、`o1d_close_wins_keeps_outbound_for_abort` 均真实变红，失败文本分别为 `close lost lease`、`callback lost its lease` 等；弱化 call lease 时 `observer must hold a strong call lease` 变红。恢复用仓库外备份，未用 `git checkout`/`reset`；恢复后 SHA256 与 `git status` 与开工一致。
- 绿测：host lib 100 passed / 1 ignored；`--lib o1c_` 8、`o1_` 7、`o1e_` 6、`o3_` 9；新增 `abi_keepalive_owner` 1 passed；`abi_lifetime` 2、`loader_release` 4、`staged` 4、`abi_stream` 5、`abi_m2` 9 等全部 ok；`cordis-core` `weak_audit` 7 passed。
- 真 DLL 证据：这是 O4 唯一不能静态判断的部分，实际跑过 —— `frozen_plugin` 1 passed（Windows 上有判别力：fiber drop 后文件仍被占用，直到插件对象释放才可删除）、`open_staged_soak` 200 轮 1 passed、`soak` 1000 轮 1 passed；均 0 failed。
- 门禁：完整 `.\cargo.cmd xtask ci` 退出 0，123 个测试二进制 652 passed / 0 failed / 3 ignored（`soak`、`open_staged_soak`、`dhat_profile`），真 DLL e2e 10 阶段全过。
- 未覆盖边界与使用方义务：effect 不执行会把库钉到进程结束（旧实现是提前释放，因此这是新增的可观察窗口）；`OP_CLEANUP` 用捕获的裸 `CallFn`、不持 lease，安全性依赖 effect 的 LIFO 顺序；`apply_via(None)` 仍是零保护的 raw seam；owner 不得回指 `Ctx`/fiber，否则可能与 effect 互钉；部分 `assert_gone_before_dispose` 调用点仍是 `Fixture::new(None)`，其 alive 半边无判别力（ordering 半边有效）。
- 契约落点：与 O3 同样，新语义尚未写入 `docs/support-contract.md` 与 `docs/platform-matrix.md`（需同步 `xtask/src/contract.rs` 的 26/17 计数与矩阵四平台列），留到 O10 收口；这里显式记录，避免被读成已完成。
- 兼容性：公开 `pub` 项集合与 HEAD 逐行比对无差异，只新增 `#[cfg(test)]` 探针；未改 ABI/core/依赖。属源码兼容但行为变化，已在本记录登记。

#### O5a：reload 候选采纳（2026-11-17）

- 范围：reload 的候选排队、淘汰与采纳；`Action::Replace` 的失败语义仍归 O5b。
- 风险核实：四条指控中三条确认为真实缺陷 —— 解析期即提交 `main/owners/baseline`，且回滚快照在闸门外，会吞掉并发回写；候选只队列 `entries`、不带代次；第二次预检失败没有回滚。第四条（采纳会用候选覆盖回写元数据）经反证不成立：采纳确实会写元数据，但受损的是 tree —— 候选 `entries` 是旧快照，退回去的是它。
- 红测（实际失败断言）：第二次预检失败后元数据停在候选（`main_only` 实测 2，期望 1）；失败候选回滚吞掉并发回写（磁盘停在 `value: 9`，第二次回写被「已被外部修改但尚未载入」拒绝）；迟到旧解析把已挂载实例换回旧配置；过期候选被静默采纳（`failures` 为空）；`main_only` 写入发生在 `config_gate` 外（计数 2 对 0）；`take` 之后未被采纳的候选丢失。
- 实现：新增 `ParsedSnapshot` 与 `Candidate{gen, base_writeback, entries, adopt_metadata, main, owners, baseline}`；`parse_snapshot` 改为纯函数，删除解析期提交与回滚；采纳集中在 `adopt`，在 mutation 与 `config_gate` 的同一同步段内校验代次与回写版本后，整份写元数据与 tree；`next_gen`/`adopted_gen` 单调，登记取号与写槽、驱动取槽都在同一处完成；新增回写版本计数，并在 persist 成功、commit 观察者、self-dispose 处推进；`ReconcileDriver::drop` 先回填后清位，对「回填被拒」与「清位窗口内被顶掉」各安排一次性补偿驱动；commit 观察者与 `mark_self_disposed` 的「改树 + 推进版本 + 写主树 + 落盘」收进同一 `config_gate`，日志移到锁外。
- 规格对齐：验收发现「失效候选只报失败、不重建」与方案第 128 行相反，且 watcher 已对齐指纹、不会重试，于是补「按磁盘重建 + 重跑预检」路径：按代次计上限 3 次，超限与读/解析/预检失败都进 `failures` + Error 日志 + 报告，不静默；只有文件派生候选重建，programmatic 候选过期只报失败。
- 验收：三轮独立审查。运行时角度发现 TOCTOU（校验后、写树前被回写插队）与丢唤醒（回填被拒后无人驱动），已修并复验通过；测试角度发现用例空转（旧实现回滚后断言仍绿）与用墙钟观测「blocked」，已补解析后即时断言与「已进入提交观察者」的到达信号；兼容性角度确认公开面零变化，但指出未落实重建，已按上文补齐。最终复核通过。
- 变异证据：伪重建（只刷新 `base_writeback`、不重读磁盘）会让强化后的用例红（`plan_calls` 4 对 5，且树采纳 2 而非 3）；摘掉 catch/闸门类改动的红证据见对应条目；恢复均用仓库外备份并核对 SHA256，未用 `git checkout`/`reset`。
- 绿测：`loader_adopt_tests` 14 passed（目标用例各连跑 5 次稳定）；host lib 114 passed / 1 ignored；`loader_reconcile` 23、`dependency_plan` 19、`loader_volatile` 6、`loader_release` 4；完整 `.\cargo.cmd xtask ci` 退出 0（含真 DLL e2e）。
- 未覆盖边界（如实记录）：重建期间文件再次变化为另一份合法内容、文件正在被写（稳读不通过）、重建期间外部登记更新请求、重建的读/解析/预检失败分支、重建期间被取消的组合路径，都只有代码路径、没有用例；`no_save` 纯内存运行值在重建后按文件收敛（有意变化，规格未显式讨论）；上限按「驱动实例 × 代次」计，abort 后补偿驱动会重新获得预算；`persist` 在闸门内做阻塞 IO（既有）；`set_disabled` 仍不推进回写版本，排队中的文件候选可能覆盖一次运行期启停（既有语义「文件是真相」）。
- 契约落点：新的「运行时回写会使在飞文件候选过期、并按磁盘重建」语义尚未写入 `docs/support-contract.md` 与 `docs/platform-matrix.md`，留到 O10，与 O3/O4 同一处理。
- 工作区：O1–O5a 改动仍未提交，未改 CI 或发布配置。

#### O5b：Replace 语义（2026-11-17）

- 范围：`Action::Replace` 的失败语义、artifact 前移与下一轮恢复；`Action::Mount`/`Dispose` 的报告语义不动。
- 风险核实：「先 dispose 再 mount」是设计确认，不是缺陷。四条验收里「报告/实例/目标一致」与「下一轮可恢复」原本已满足，证据是：失败后装载记录已摘（`fiber_of` 为 `None`）、半成品 fiber 已被 dispose（`fibers_by_plugin_name("fails")` 为空）、tree 已是新目标不回滚、下一轮 `plan(&tree_snapshot())` 必给 `Action::Mount`。真正缺的是两条：artifact 检查没有前移（`load` 发生在拆卸之后），以及短路时报告说谎（旧实例已被拆下，却仍记 `skipped`）。
- 第一批（报告语义）：Replace 失败文案改为「换实例失败（旧实例已卸下、未恢复）」；`loader/patch-context` 短路从 `skipped` 改记 `failures` + `Warn`，理由是一次真实拆卸不该记成"跳过"。只改 Replace 分支，Mount/Dispose 的 `skipped` 不变。红证据：失败用例报「失败文案必须写明拆卸事实：["provider: 故意让 apply 失败"]」；短路用例报 `skipped: ["provider"]`、`failures` 为空。另两条（下一轮可恢复、无残骸）修复前已绿，如实标注为守卫测试。
- 第二批（artifact 前移）：Replace 现在先 `source.load` 决策，失败则保留旧实例（文案「换实例预检失败（保留当前实例）」），成功才拆卸并复用同一 artifact，绝不二次 load。红证据：旧实现写错 name 会让服务消失（`left: None right: Some(1)`），trace 显示 dispose 先于 load；短路时只 load 一次。
- 第三批（泄漏与断言补强）：`DiskPluginSource::load` 的 open 失败分支补暂存副本回收（对齐 `reload_one`），红证据为 `stagedLeaked` 从基线 0 变 1；给测试夹具加 artifact drop 探针（`FakeSource::probe` / `DropProbe`），红证据为 `mem::forget` 时旧断言全绿、只有新断言失败（`["provides"]` 计数 0≠1）。
- 验收：三轮独立审查。兼容性角度确认公开面零变化、`applied()`/`is_noop()` 自洽、无消费点被误导（唯一外溢是短路会让 status 不健康，CLI 自身不注册该监听器故不可达）；运行角度确认前移顺序、资源回收（短路/保留 next/panic 路径）、并发（mutation 下、旧实例仍 ACTIVE、服务名不冲突）与报告自洽，未发现阻塞；并指出权限不对称（短路能拦挂载不能拦加载）已写进代码注释。
- 绿测：`loader_reconcile` 31 passed（新增用例各连跑 5 次）、`dependency_plan` 19、`loader_volatile` 6、`loader_release` 4、host lib 114 passed / 1 ignored；完整 `.\cargo.cmd xtask ci` 退出 0（含真 DLL e2e）。
- 未覆盖边界（如实记录）：预检进行中其他线程照常解析旧服务名的并发读没有用例；预检成功但 dispose 失败没有失败通道（`fiber.dispose` 无返回值）；真实 `DiskPluginSource` 的 veto 丢弃未用真 DLL 验证（依赖字段声明序）；监听者保留 `next` 稍后调用会回落二次 load（既有 waterfall 边界）；同名列 + 插件加载期注册 OS 级全局资源时，预检在旧模块存活时打开第二副本可能永久预检失败（行为从"先卸后装能成"变成"改不动"，需知会）；`report()` 不表达"实例落后于目标"的持续态、`capabilities().js` 在预检失败后静默为空；Rust 侧仍无自动重试，恢复靠下一次配置变更或 `sync()`。
- 契约落点：新的「换实例失败/短路/预检失败」报告语义与 HMR 路径的差异尚未写入 `docs/support-contract.md` 与 `docs/platform-matrix.md`，留到 O10。
- 工作区：O1–O5b 改动仍未提交，未改 CI 或发布配置。

#### O6：独立消费者的可信断言（2026-11-17）

- 范围：`tests/consumer-service` 消费者测试的可信度（进程隔离、真实服务结果断言、准入负向证据）；不改宿主生产语义。
- 风险核实（逐条对照）：① 两个测试同进程共享全局策略成立 —— `ACTIVE_POLICY`、`ALLOW_UNVERIFIED`、`loaded_instances`、`staged_unadopted` 都是进程级，任一用例改策略或装载插件都会污染另一个；② `mounted_labels` 只证明两行 apply 成功返回并记录在表里，不证明服务值 / Active / 版本 / 派发；③ 没有显式 `Active` 断言，但也确实没有提前断言 —— 靠配置里 `greeting` 排在 `hello` 之前的顺序造成隐含依赖；④ 关闭后只验证 `loaded_instances`，未验证 `staged` 与暂存文件；⑤ 开发与生产两条准入路径都没真正调用服务，且完全没有「应拒者被拒」的负向证据。
- 实现：把 `abi_service.rs` 拆成 `abi_service_policy.rs` / `abi_service_dev.rs` 两个测试二进制（保留原测试名），新增 `abi_service_failures.rs` 三个失败注入用例，共享夹具集中到 `tests/common/mod.rs`；DLL 复制到 `%TEMP%/cordis-consumer-<pid>-<label>-<seq>` 沙箱并在副本旁写 sidecar（sha256 对副本计算），不再污染 `target/debug`；每个文件用 `tokio::sync::Mutex` 串行闸门；`Admission` 用 RAII 在 `Drop` 复位 `active_policy` 与 `allow_unverified`。
- 新增断言：`fiber_of(..).state() == Active`、`pending_report` 为空、`report.failed` 为空；`hello.reply` 的 `message == "consumer-probe × 2"`、`inline_call == "CONSUMER-PROBE hello!"`、`version_error` 含「请求接口版本 3」、`dispatch_result` 含 `consumer-probe → self`；`greeting.text`；`require_as("nonexistent.svc")` 为 `NoInject`；关闭后 `loaded_instances() == 0`、`staged_leaked() == 0`、`leftovers()` 为空、沙箱目录已删除。
- 失败注入：只配 `hello` 时停在 `Pending`（缺 `greeting`）且 `report.failed` 仍为空；空 allow-list 在 strict 下拒绝装载、在 lenient 下降级并逐行报 allow-list；改 `greeting.config.text` 后 `hello.reply` 随之变化（证明值真的走服务链，用 `reload_one` 消除依赖者重挂的竞态）。
- 变异证据（可证伪性）：把 `hello` 的 `message` 改成常量 → 两个正向用例红（`left "MUTATION-A" right "consumer-probe × 2"`）；让 `hello` 接受 v3 版本 → `version_error` 断言红；让 admission 忽略 allow-list → 负向用例红。三处均整文件备份 / 还原并核对 SHA256，工作区无变异残留。
- 验收发现并修复的问题：Windows 删除目录的硬断言与「删除失败不算断言」自相矛盾（改为统一重试后再断言，文案区分环境占用与泄漏）；`leftovers()` 在目录缺失时静默返回空（改为显式失败）；consumer 包未显式声明 tokio 的 `sync` 特性、靠宿主特性合并借宿（对外部消费者是错误示范，已补声明，lock 无需改）；长命 `ctx` 跨 `close`（已收进块作用域）；值变化用例的守卫注释与内核实际行为不符（内核 `spawn` 后不 await 依赖者，属竞态窗口，注释已改写）。
- 绿测：consumer 包 8 passed / 6 个测试二进制，连跑 5 次稳定；`cargo xtask consumer`、`cargo xtask ci` 退出 0（含真 DLL e2e）；`%TEMP%` 与 `target/debug` 无残留。
- 未覆盖边界（如实记录）：`ACTIVATIONS` 是 DLL 级 static，跨 App、跨测试共享，不能断言 apply 恰好一次；`staged_leaked` 是台账语义，不等价于文件已删除（文件层面由 `leftovers` + `assert_removed` 覆盖）；某个用例失败时同二进制的后续用例可能因进程级实例计数级联变红；负向解析只断言到 `NoInject`（根 ctx 无 fiber，`Inactive` 不可达）；直接 `cargo test` 仍需 `CORDIS_CONSUMER_PLUGIN_DIR`，缺环境会明确 panic 而非静默跳过；侧车篡改（hash 不符）仍由 `crates/host/tests/source_policy.rs` 覆盖，未搬到消费者层。
- 契约落点：消费者层的真实服务调用与准入负向证据是否要写进 `docs/support-contract.md` / `docs/platform-matrix.md`，留到 O10 契约收口。
- 工作区：O1–O6 改动仍未提交，未改 CI / workflow / xtask。

#### O7：声明预检分类报告与绑定联动（2026-11-17）

- 范围：`crates/host/src/loader.rs` 的声明预检与 `crates/host/tests/dependency_plan.rs` 的联动用例；不改门禁条件与错误文案。
- 风险核实：预检只有 `Result<(), String>` 一个出口，`Ok` 混着「不适用 / 无声明跳过 / 真通过」三种原因；`plan::resolve` 的 `ResolvedPlan` 求解完就被丢弃；`report()` 没有任何预检字段；四类联动场景（同域不同版本、动态 disabled、混合来源、跨私有域同键）里只有「动态 disabled 保守纳入」有测试，且没有一处把预检结论与实际绑定串起来。同时确认两点不是缺陷：宽松降级不等于 admission 放行（load 侧仍独立验证），预检也确实先于任何 `source.load`。
- 实现：新增 crate 私有 `PreflightReport`（`NotApplicable` / `Incomplete` / `Downgraded` / `Checked`），逐行记录 `index` / `label` / `plugin` 与判定，`Checked` 收进原本被丢弃的 `ResolvedPlan` 提供者结论；结论落在 `LoaderState` 私有槽，经 `report()["preflight"]` 暴露（零公开 API 变更）。报告显式标注 `scope=declaration`、`runtimeVerified=false`、`realmModel=notModeled`，兑现 ADR 0007「声明不是运行时 verified」。门禁条件与错误文案逐字未变。
- 红测：8 条用例先红，红在 `report` 没有 `preflight` 键（`Null` vs `"incomplete"` / `"downgraded"` / `"checked"` / `"notApplicable"`）；另有联动用例在拿到预检结论后红。
- 联动结论（重要）：同域不同版本 → 预检 `checked` 但实际因同域重复注册失败（声明通过 ≠ 绑定成功）；动态 disabled → 预检 `checked` 但提供者休眠、消费者 PENDING；混合来源 → `incomplete` 且消费者 PENDING、装载不失败；跨不同私有域的同键同版本 → 预检按全局键空间判歧义、在任何 load 之前失败，而运行期本来合法。
- 跨 realm 决策：按方案「不默认引入 realm 感知求解」，本轮选择缩小承诺 + 报告标注 + 钉住现状（用例名直写 `misreports`），并在 sidecar 预检的 resolve 失败文案追加「声明预检不建模 realm」提示。评估过的最小修法并不小：要么复制 realm 身份推导形成第二真源，要么给 `plan::resolve` 增加 realm 维度（公开 API 变更 + 求解器契约变更），因此记为后续独立项。
- 验收发现并修复的问题（三轮）：① 报告在 `adopt` 之前写入，被拒候选会覆盖已采纳结论（改为采纳成功后才记录）；② 对账预检失败既不写 `last` 也不发布，`report.failed` 看不到（已补对称）；③ `reload_one` 在 mutation 锁外取快照/预检/写报告，可能与并发 `adopt` 分属两棵树（已整体移入临界区）；④ 命名易误读（`providers`→`declaredProviders`、`realms`→`realmModel`、新增 `skipped`、`NoEnabledRows` 的 `declaration` 改为 `none`）；⑤ 断言缺口（`sidecarManifest` 仅由 demo 快照间接覆盖、非 `checked` 分类缺 `declaredProviders` 负断言、e/f/g/h 未走统一 helper、publish 未用已发布读口断言）。
- 绿测：`dependency_plan` 28 passed（O7 新增 9 条）、`loader_adopt_tests` 新增 4 条、host lib 118 passed / 1 ignored；`loader_reconcile` 31、`loader_volatile` 6、`loader_release` 4、`loader_scope` 5；`examples/demo/expected.txt` 重新生成（只改装载报告那一行，含新 `preflight` 键）；完整 `xtask ci` 退出 0。
- 未覆盖边界（如实记录）：静态 disabled 行在报告里仍不可见（只能从 `report.disabled` 间接推）；同一棵树既有 `Ok(None)` 又有宽松 `Err` 时报告只报 `downgraded`、隐含 `incomplete`（无用例）；报告未标注属于哪一次采纳（读者需按「`failed`=最近一轮、`preflight`=最近已采纳树」的约定分辨）；CLI `status` 尚未暴露 `preflight`；「声明不可用」与「无声明」都只会跳过整次预检，不会逐行降级。
- 契约落点：预检报告是声明级诊断、不是运行时保证，因此不写入 `support-contract` 的「保证」表；是否要在 O10 增加一行「声明提供/不保证」的说明另议。
- 工作区：O1–O7 改动仍未提交，未改 CI / workflow / xtask。

#### O8：资源边界（2026-11-17）

- 范围：O8a 的每流挂起 credit 上限与 O8b 的只读扩容上限；`crates/core` 的流窗口语义、`PushOutcome` 与公开 `HostLimits` 均不改。
- 风险核实：credit 登记有三层结构 —— core 的 `waiting` 队列、host 侧按 `(dispatch, on_credit)` 计数、SDK 侧 continuation 表 —— 三层都没有上限，`maxDispatches`/`maxStreams` 覆盖不到。只读扩容按插件回填的 `out.len` 直接 `vec![0u8; n]`：`n > isize::MAX` 会 panic，真实 OOM 会 abort，且完全没有上限。已满足项也记录，不重复实现：写 op 不重放、只读重试最多一次、收尾恰好一次、卸载时等待者先驳回后 `OP_DISPOSE`。
- 实现（O8a-1）：宿主层私有常量「每流 64 条挂起推送」，`reserve_credit`/`release_credit` 在同一 `credits` 临界区完成检查与记账（检查与记账同锁，无 TOCTOU）；`Sent` 与 `Gone` 当场回滚，只有 `Pending` 兑现；拒绝走容量载荷（`host_capacity` / `capacity`，文案改为「这块未被接受」并带 `detail` 的 scope/used/limit），不伪装 `StreamGone`、不扩 `PushOutcome`、不去重（重复 id 仍按次数计）。
- 实现（O8b-1）：只读重试加私有上限 1 MiB（对齐 `MAX_CONFIG_SCHEMA_BYTES`），`read_retry_cap` 先拒 `isize::MAX` 再拒上限，校验在分配之前；`CallBuffer::try_with_capacity` 改 `try_reserve` 可失败分配；二次 `TOO_SMALL` 改为可读诊断（不再把刚清零的缓冲按 `out.len` 吐成一串 NUL）；`invoke_raw_read`/`call_op_read` 加 `debug_assert` 限定 `OP_VOLATILE_PATHS`，防业务只读 op 静默继承。SDK 侧 `retry_cap` 改为只防越界（> `isize::MAX`）+ 可失败分配，不设业务载荷上限。
- 红测：满窗口后换不同 id 连推，第 65 次得 `STATUS_ERR` + `host_capacity`，且 credits/waiting 停在 64；被拒后这条流仍能正常排空与收尾；同 id 重复推送仍按次数计；只读上限 +1 与 `usize::MAX`/`isize::MAX` 被拒且不 panic；二次 `TOO_SMALL` 的诊断不含 NUL。
- 验收发现并修复的阻塞：SDK 侧最初也加了 1 MiB 上限，会误伤业务载荷 —— 宿主对 `provide`/`set` 没有字节上限，而 `Ctx::get` 用 `.ok()?` 把「值太大」静默压成 `None`（插件读成「服务不存在」），属语义回归，比原状更坏。已改为只防越界；并补两条端到端用例：真 DLL 的 1.5 MiB 服务值经 `ctx.get` 读回逐字相等（`tests/consumer-service/tests/abi_service_large_value.rs`）；ABI 层（`crates/host/tests/sdk_large_service_value.rs`）用真实 SDK 与 `HostApi` 验证 `ctx.get` / `require_detailed` 双入口读回完整值，并用计数包装表断言「确实发生了扩容重试」（把起始缓冲放大就会红）。
- 其它修正：credit 拒绝文案不再复用「拒绝这次派发」；`with_stream_instance` 注释改为「结构化字段当前不透出，插件只看到文本」；注释纠正「panic 漏一张票」的不存在窗口（实际会中毒 `credits` 与 `dispatches` 两把锁，属 fail-loud）；`certainty=not_executed` 对 credit 拒绝的含义注明为「这块数据未入队」；SDK 文档删掉写死的 64，并修正 `Ctx::get` 的 `None` 语义（也可能来自读取失败）。
- 绿测：host lib 128 passed / 1 ignored；abi lib 36；core `stream` 17；`abi_stream` 5、`capacity` 6；新大值用例各连跑 5 次；完整 `.\cargo.cmd xtask ci` 退出 0（含 consumer 与真 DLL e2e）。
- 未覆盖边界（如实记录）：不设 chunk 字节预算 —— 窗口按块数计，块大小无界，按测量 + 边界处理；不承诺 per-plugin / 实例总内存；重复 id 故意不去重；分配失败注入未落地（1 MiB 上限下 `try_reserve` 失败实际不可达）；`PushOutcome::Gone` 的回滚分支没有专门用例；`Ctx::get` 仍把任何读取失败塌成 `None`（只收窄了「值太大」这条主因）；plugin→host 的入站回包仍受 16 KiB 不可重试边界。
- 测量：现有 buffer benchmark / soak / dhat 保持手工与 nightly，不新建框架；本轮未新增指标输出，记为后续。
- 契约落点：新增的上限与拒绝语义尚未写入 `docs/support-contract.md`（需同步 `xtask/src/contract.rs` 的行数/保证计数），留到 O10，与 O3/O4/O5 同一处理。
- 工作区：O1–O8 改动仍未提交，未改 CI / workflow / xtask。

#### O9：局部来源策略（2026-11-17）

- 范围：来源准入的作用域（`SourceAdmission` / `PolicyPluginSource`）与 `reload_one`、`probe_capabilities_in`、`plan_entry` 的读口；CLI 与 ABI 不改。
- 风险核实：① 两个 App 并发互相污染是确定的 —— `ACTIVE_POLICY` / `ALLOW_UNVERIFIED` 是进程级量，读口 `source::admission()` 没有任何 App 维度，后写覆盖先写；dev 方向是 A 的 `--allow-unverified` 让 B 的裸源拿到 `Ok(None)` ⇒ 跳过校验 ⇒ 未登记 artifact 真的走到 `stage_plugin` 与 `LoadLibrary`（错误从"不在 allow-list 中"变成"打不开动态库"），严格方向是 B 的 `set_active_policy(Some(..))` 把 A 的本地放行一并拒掉。② `reload_one` 绕过来源对象直接读全局：`let active_policy = crate::source::admission()?;`，`state.source` 上的结论根本没被读。③ 探测与装载可能是两份快照：`probe_capabilities_in` 自己校验一次，再让 `source.load` 读第二次准入，两步之间全局被改就"验证按一种、打开按另一种"，暂存副本也不再与已验证 hash 绑定。另核实 `plan_entry` 的旧读口直接取 `active_policy()`：无策略时**跳过校验**（不报错），且校验发生在读 manifest 之后。
- 决策：见 `docs/adr/0008-source-admission-scope.md` —— 准入结论随来源对象走（`PluginSource::admission`），三态优先级严格 > 开发 > 回落；载体不是 `LoaderConfig`（12 个公开字段、仓内外三十余处字面量构造，加字段属 0.2 破坏），也不是按 App 注册表（`load` 签名里没有 `ctx`）。
- 实现：`SourceAdmission` 三态（`strict` / `dev_unverified` / `fallback`），`resolve()` 只有 `fallback` 读全局，`mode()` 给诊断字面量；`PluginSource` 三个默认方法（`admission` / `load_admitted` / `plan_entry_admitted`，后两个缺省忽略参数、逐字转发）；`PolicyPluginSource::strict` / `::dev_unverified` 转发**外层**结论并覆写 `admission()`（嵌套以最外层为准）；`DiskPluginSource` 保持单位结构，`load` / `plan_entry` 取 `self.admission()` 后转 `load_with` / `plan_entry_with`，裸源行为不变；`reload_one` 改走 `state.source.admission()` 且一次操作只取一次（verify 与 `open_staged_verified` 共用）；探测新增私有 `probe_capabilities_with`，把同一份结论显式交给 `load_admitted`；`deny` 与预检失败都推进指纹并写入 `report.failed`，不再 `?` 早退、也不用空 outcome 覆盖上一轮 `reconcile` 计数。
- 红测（实际失败断言）：显式严格忽略全局 dev —— 全局 `set_allow_unverified(true)` 后旧路径放行到加载器，`error.contains("allow-list")` 红（实际是 `LoadLibrary`/`dlopen` 打不开），`assert_no_staged_copy` 同时看到 `.r1` 副本；显式 dev 不写全局 —— 用全局开关冒充局部放行时 `assert!(!allow_unverified())` 红（实际 `true`），同一刻裸源也不再报"未配置插件来源策略"；双源并发 —— 顺序执行时"写全局→调用→复位"照样绿，所以用三方 `Barrier` 把两次 `load` 钉在同一刻，该实现下 strict 侧的 `!contains("LoadLibrary")` 红、`active_policy()/allow_unverified()` 未被改写的断言也红；`reload_one` 走来源策略 —— 旧实现读全局，`error.contains("allow-list")` 红（实际是打不开动态库），`staged_files` 非空、`stale_labels` 仍为 `["provider"]`；暂存副本替换被拒 —— `#[cfg(test)]` 的 `before_staged_open` 回调把 `.rN` 换成别的字节，断言 `error.contains("暂存副本 SHA256 与已校验 artifact 不匹配")` 红（实际是加载失败）；deny 推进指纹 —— 旧实现 `admission()?` 早退，`stale_labels` 仍是 `["provider"]`（"准入拒绝也要推进指纹"红）、`report.failed` 不刷新；probe 单快照 —— 用 `NaiveSource`（声明严格、`load` 回落全局）复现旧形状，合法 artifact 探测失败在"必须探测成功"，实际错误是"未配置插件来源策略"；默认拒绝的预检 —— `load_with_source_preflights_with_the_sources_own_admission` 的行为红是"预检按全局放行、拒绝被推迟到 `load`"，错误里没有 `未通过来源准入` 前缀。`bare_disk_source_still_reads_the_process_wide_policy`、`bare_disk_source_plan_entry_still_reads_the_process_wide_policy` 是守卫（改前改后都绿），后者用 `!contains("sidecar manifest 无效")` 钉住"准入先于清单结论"。
- 验收修复：① probe 丢快照 —— 最初只把结论用于 verify、随后仍调 `source.load`，改为 `load_admitted` 显式传参；② reload 预检失败不推进指纹 —— 最初只 `?` 早退且用 `ReconcileOutcome::default()` 覆盖 `last`，改为与准入拒绝同构（`reload_one_preflight_failure_advances_the_fingerprint_and_keeps_the_counts`）；③ `plan_entry` 须先准入后 manifest —— `admission.resolve()?` 提到 `read_manifest` 之前，否则"准入没配"与"发布方把清单写坏了"在两个环境给不同首错；④ 清单 marker 从注释改为签名绑定 —— `xtask/src/inventory.rs` 新增项最初用文档注释串当 marker，改成真实签名（`pub fn strict(policy: impl Into<Arc<PluginSourcePolicy>>) -> Self` 等），否则清单可以命中注释而不命中代码；⑤ 双源用例加 `Barrier` —— 见红测第三条；⑥ `loader_reconcile` 全局守卫 —— 磁盘用例统一走 `reset_globals()` / `GlobalsGuard`（`set_active_policy(None)` + `set_allow_unverified(false)`），避免残留全局让后续用例在放宽或更严的状态下静默跑偏。
- 绿测与门禁：`source_policy` 23 passed、`source_policy_reload` 3、`dependency_plan` 30、`loader_reconcile` 31、`probe_source_admission` 1、host lib 129 passed / 1 ignored（原有手工 benchmark）；`inventory` 受控公开入口 80 → 87（本轮 7 条，`--check` 通过）；完整 `.\cargo.cmd xtask ci` 退出 0。
- 未覆盖边界（如实记录）：初装路径上的"暂存副本替换"没有 hook —— 只有 `reload_one` 有 `before_staged_open`，`DiskPluginSource::load_with` 的 verify 与 stage 之间仍是代码级防线、无用例；`ctx_use` 挂子组件仍走进程级 `set_plugin_source` seam，不受来源对象结论约束；CLI 诊断只报全局 mode（`admission_mode()`），看不到某个来源自己的 `mode()`；`load_admitted` / `plan_entry_admitted` 是公开方法，同进程调用方可以显式降级（这是"调用方选择"，不是缺陷，但也不是隔离）；显式策略的探测仍依赖 `set_plugin_source` 这条全局 seam，没有"per-probe 来源"入口。
- 契约落点：准入三态是否要写进 `docs/support-contract.md` 的**保证表**（新增行会改动 `xtask/src/contract.rs` 的 26/17 计数）留到 O10；本轮只修正既有"声明"行的措辞，未增删行。
- 工作区：O1–O9 改动仍未提交，未改 CI / workflow / xtask（`xtask/src/inventory.rs` 只补了清单条目与计数）。

#### O10-A/B：本地门禁与证据（2026-11-17）

- 范围：O10 拆分后本轮只做 A/B 两批 —— A 把 doctest 接入门禁并把 ignore 示例转正，B 让资源测试不能空跑。C（隔离目录 package 验收）、D（五类证据表）、E（formal_seal 业务断言）、G（契约收口）、F（远端 workflow 接线）均未做，见下文。
- 改动（A）：`xtask ci` 新增两步 —— `cargo check --workspace --all-targets --all-features --locked`（暖缓存 2.7 s）与 `cargo test --workspace --doc --locked`（暖缓存 10.2 s，6 passed / 0 failed）；clippy 步骤加 `--all-features`，此前只按默认 feature 检查，`dhat-heap` 一类代码在 `#![cfg]` 裁掉时无人编译。
- 改动（A，示例转正）：5 段 macros 示例改为 `text` 并写明理由 —— proc-macro crate 的文档里没有 `cordis` extern crate，代码块编不过；编译保护改由 sdk doctest 与 `sdk/tests/events_macro.rs`、`sdk/tests/plugin_macro.rs` 承担。doctest 从「1 passed / 10 ignored」变为「6 passed / 0 ignored」。
- 改动（B）：`--ignored --list` 守卫抽成可复用函数，覆盖 `soak`、`open_staged_soak`、`dhat_profile`（dhat 带 `--features dhat-heap`）；匹配改为行级等值而非子串；失败时打印子进程 stderr。`soak` 现在显式读取并打印 `CORDIS_SOAK_CYCLES` 与 `CORDIS_STAGED_SOAK_CYCLES`（位置参数只影响主 soak，staged 默认 200），cycles 为 0 或非法时拒绝；`open_staged_soak` 新增每轮计数断言。
- 改动（B，新入口）：新增 `xtask dhat` 子命令（feature 守卫 + 执行 + 打印 stats）；新增 3 条 CLI 测试（非法 staged cycles、dhat 未知参数、`dhat --help`），xtask cli 测试 20 → 23。
- 判据证据（可证伪）：改坏 doctest 里的方法名会红，说明新步骤真的编译并运行示例；删掉 `open_staged_soak` 的 `#[ignore]` 后 `xtask soak 1` 会以「未注册为 `#[ignore]` 测试」拒绝，不静默跳过；不带 feature 的 `dhat --list` 是 0 个测试的假绿入口，加 feature 守卫后不再能冒充通过；守卫失败时子进程 stderr 原文可见（实测，非仅设计）。
- 绿测与耗时：完整 `.\cargo.cmd xtask ci` 退出 0，6.68 分钟，137 个 test result / 729 passed / 0 failed / 3 ignored（`soak`、`open_staged_soak`、`dhat_profile`）。
- 未覆盖 / 待授权：C 未做，消费者仍靠仓库源码 path 依赖；D 未做，ABI 布局 / 截短桩 / 真实 DLL / 业务结果 / 历史运行仍无统一证据表；E 未做，formal_seal 仍只验装载与关闭、无业务结果断言；G 未做，`docs/support-contract.md` 与 `docs/platform-matrix.md` 未改，O3/O4/O5/O6/O8/O9 的契约落点仍欠着（需同步 `xtask/src/contract.rs` 的 26/17 计数）；F 待授权，`nightly-soak.yml` 里的 dhat 步骤仍是裸跑、绕过新守卫。
- 本机限制：`xtask dhat` 端到端在本机跑不起来 —— `backtrace` 的 dbghelp 导入库需要 `dlltool`/`as`，本机 mingw self-contained 缺 `as`；`--all-features` 的 clippy/check 只到编译期，抓不到链接期。
- 工作区：O1–O10 改动仍未提交；本轮只更新文档，未改代码、测试、CI/workflow 与发布配置。

#### O10-C：隔离目录 package 验收（2026-11-18）

- 范围：`cargo xtask consumer` 用的是仓库源码 path 依赖，所以「crates.io 上那份包能不能用」从来没有被验证过 —— 打包会展开 workspace 继承字段、把 path 依赖改写成 registry 依赖，`include` 写漏、忘提交新模块这类问题只会在发布之后暴露。本轮新增 `cargo xtask package-check [--keep] [--offline|--online] [--reuse-dir] [--dir <绝对路径>]`，只做本地验收：不发布、不写 registry、不改 `Cargo.lock`。
- 实现（七阶段）：`prepare → package → unpack → consumer → resolve → build → run`。`prepare` 判隔离与 ASCII、清上次残留；`package` 先把 core/abi/macros 一起打包、再单独打 host（host 的 path 依赖必须先改写成 registry 形式）；`unpack` 用系统 `tar` 解到临时目录；`consumer` 生成一个只认解包目录的独立工程；`resolve` 先生成 `Cargo.lock`、再用 `cargo metadata` 核对依赖图；`build` 用消费者独占的 `CARGO_TARGET_DIR`；`run` 执行并按文本核判据行。默认 `--locked --offline`，`--online` 只用于补离线缓存。
- 实现（隔离与清理）：临时目录必须是绝对路径、纯 ASCII（GNU ld 处理不了非 ASCII）、且在仓库之外；`..` 与驱动器根一律拒绝，盘符大小写与 8.3 短名靠「已存在的最深祖先 + 仓库根各自 canonicalize 后再比」归一（否则 `C:\repo\target\..\..\x` 这类写法会把消费者塞回仓库并继承仓库的 `.cargo/config.toml`）。目录必须带 `package-check-root.marker` 才归本次运行所有，清理、幂等清空与 `--reuse-dir` 共用同一套归属判定，`--dir` 打错一个字符不会递归删别人的目录；失败保留目录并打印阶段名、原因与路径，成功清理重试 3 次。
- 实现（新鲜度双保险）：`CARGO_TARGET_DIR/package/*.crate` 跨运行保留，cargo 在版本未变、有人预置或外层短路时可能不重写它 —— 打包前先删掉这四个目标产物（日志写明删了哪个），`collect` 再断言 mtime ≥ 本次开始时间（留 2 s slack 覆盖 FAT/exFAT/网络盘的 mtime 粒度）。包清单只从 `ALL_PACKAGES` 一张表派生，新增 crate 不会漏删。
- 实现（三层反后门判据）：①生成出来的消费者清单文本；②解包后的四份打包清单文本 —— 全文不得出现仓库根，绝对 `path` 只能落在解包目录内且真实存在，相对 `path` 不得含 `..`；③`cargo metadata` 溯源 —— 依赖图里任何包都不得解析到仓库源码，且 core/abi/host 必须全部落在解包目录内（两侧都 canonicalize）；漏掉任一层，「产物可用」都可能只是被源码兜住。
- 实现（真实服务调用）：消费者用打包产物里的公开 API 装载插件 —— 插件侧 `ctx.require("probe.input")` 跨 ABI 读、`ctx.provide("probe.reply")` 跨 ABI 写，宿主再 `require_as` 读回逐字段比对（探针值由消费者注入，写死默认文案会立刻红），并断言装载后恰好 1 个 ABI 实例、关闭后归零。只认退出码不够：`service call ok`、`instances after close: 0` 两行缺一即失败，空壳 `exit(0)` 不能冒充通过。
- 判据证据（可证伪，实测）：改坏解包产物后 `--reuse-dir` 复跑必红 —— `装载后应有 1 个 ABI 实例，实际 2`，失败落在 `run` 阶段；只改仓库源码后消费者仍绿、一个 crate 都没重编，说明产物确实是本次新写的、源码不参与构建；三层文本判据各有单测（仓库根 needle、解包目录外的绝对 path、含 `..` 的相对 path）；`--dir C:\` 退出 2（驱动器根会被当成无主空目录整盘删除）；旧 `.crate` 会被删除并在日志里说明。
- 绿测与耗时：xtask 单测 88 passed、cli 32 passed / 1 ignored；`cargo xtask package-check` 正常路径实测约 52–104 s（随冷/暖缓存浮动）；完整 `.\cargo.cmd xtask ci` 退出 0。
- 未覆盖边界（如实记录）：只在 Windows GNU 实测，MSVC / Linux / macOS 未跑；解包依赖系统 `tar`；消费者用的是不经动态库的 ABI 形状插件，真 DLL 路由仍归 `xtask consumer` / `seal-consume`；`--reuse-dir` 是调试口（须带 marker，成功行明写「不作为发布证据」）；本判据不进入 `xtask ci`，与 `seal` / `soak` / `dhat` 一样按需运行。
- 工作区：O1–O10 改动仍未提交；本轮只更新文档，未改代码、测试、CI/workflow 与发布配置。

### RSI：部署与观测底座（2026-11-18）

阶段 1 的方案与三轮评审见 `rsi-sdk-roadmap.html`（同目录）。工作包顺序：WP0（本 ADR）
→ WP1（iface 归因）/ WP2（变体身份）/ WP2b（契约方法面）→ WP4（端到端示例）。

#### RSI-WP0：RSI 部署与观测底座的 ADR（2026-11-18）

- 范围：新增 ADR 0009（位置：演化循环的部署与观测地板），修订 ADR 0005（§2.3 受审流水线签发），并同步三处会与之矛盾的对外口径 —— `README.md` 的来源段、`docs/support-contract.md` 的来源"声明"行、`docs/roadmap.md` 的两处 T3 触发条件。不写代码，不动 ABI、CLI 与 `xtask/src/contract.rs` 的行数。
- 风险核实：① ADR 0005 §2.2 明文把“模型生成且未经人工审查”列为不允许，§6 复评条件也按同一口径写 —— 不修则演化产物只能常开 `--allow-unverified`，等于永久开发模式；② §2.2 末段已写明最低要求是“组织控制 + review + 固定 revision + 受控构建 + 产物 hash/来源证明 + allow-list”，且明确“同一流水线本身不是信任证明”，所以 §2.3 必须逐项映射而不是放宽；③ `PluginDesc.schema` 是配置 schema（`crates/abi/src/lib.rs:434`），服务方法名只在运行期 `provide_iface` 注册（`crates/core/src/dyn_value.rs:143`），契约锁不能只写 schema；④ `Context::call_iface`（`crates/core/src/context.rs:468-486`）是 T1→T2 与 T2→T2 的单一汇合点，而 `Store::handlers`（`crates/core/src/store.rs:1218-1248`）丢弃 hook 身份、事件返回值不可失败，事件派发不适合做阶段 1 的归因面；⑤ HMR 指纹是 `(mtime, len)`（`crates/host/src/loader.rs:299-308`），不能当内容哈希。
- 决策：见 ADR 0005 §2.3 与 ADR 0009 —— 受审流水线代替逐变体人审（审评估器/门禁/签发逻辑）；宿主只做已选中变体的部署、热换、回滚与归因；归因 iface-only；变体身份用 `capabilities` 行的 `uid`+`sha256`；契约 = 声明三项（inject/provision/schema）+ 服务方法名集合 + `iface_spec_sha256`，比对在外部做。
- 改动：ADR 0005 头部状态/摘要、§1.2、§2.2 不允许项、新增 §2.3（原 §2.3/§2.4 顺延为 §2.4/§2.5）、§I1、§5、§6；新增 ADR 0009；README 与 support-contract 各改一句；roadmap 增 ADR 索引行并把两处"模型生成"触发条件限定为"未按 §2.3 签发"。
- 验收（文档项，核对来源与一致性）：ADR 0009 的每条事实引用都能在代码中命中（iface 汇合点、handlers 丢身份、指纹语义、schema 是配置 schema、`ExternService::methods` 只有方法名、sha256_file 位置）；ADR 0005 §2.2/§2.3/§I1/§5/§6 五处对“演化产物”的口径互不矛盾，且 §2.3 与 §2.2 末段的六项最低要求一一对应；`.\cargo.cmd run -p xtask -- docs` 与 `.\cargo.cmd run -p xtask -- inventory --check` 均退出 0。
- 验收修复（三轮独立评审两轮后）：① “缺 attestation 即由 loader 拒绝”是虚报 —— `PluginSourcePolicy::verify` 不认识 attestation，改为“校验发生在准入，不在运行时”，并写明无签名时的可信上限；② attestation 不绑定产物 → 要求评估记录含 variant revision + artifact SHA256 并与 manifest 交叉相等；③ 归档仓库可能提交二进制 → 明令禁止，否则“从该 revision 构建”退化；④ 只有已审 `pipeline_revision` 可签发、不得有强制签发 override；⑤ 补 ABI 精确门禁与 `xtask seal` 的适用范围；⑥ 契约锁只能到方法名（`ExternService::methods() -> Vec<String>`，无参数形状），删掉“形状”承诺并把 IO schema 锁列为范围外；⑦ 归因埋点若只在 `call_iface` future 内，超时/取消/panic 三种丢弃都不会落记录 → 共享 outcome cell + Drop guard，且三条路径分别记 `host_timeout`/`host_cancel`/`host_drop`；⑧ `uid` 是 per-Store 且卸载后置 `u64::MAX` → `callee_uid` 在发起时快照，加稳定 `callee_plugin`，不引入 `mount_gen`；⑨ “插件无法抹除记录”改限定为“无法经公开 API”，恶意插件仍可改内存；⑩ `sha256` 取暂存副本而非原路径；⑪ attestation 摘要**不写进 policy JSON**（`AllowedPlugin` 无该字段、`from_json` 无 `deny_unknown_fields`，会静默丢弃），改放准入台账；⑫ 准入合并须与签发流水线职责分离，否则交叉相等只是自证；⑬ “变体不得用 `dev_unverified`”改为部署纪律并如实说明 loader 无法强制；⑭ 导出面不存在 → WP2b 增 `Store::services_of(&Arc<Fiber>)`（`UID_DISPOSED` 返回空并标记）与 `StatusReport.capabilities`（typed row，`StatusReport` 派生 `Eq`），方法名在 shutdown 前抓取；⑮ 契约加 `iface_spec_sha256`（接口规范原文 hash），让宿主侧导出与作者侧 YAML 同源；⑯ trace sink 挂 `App` 而非公开可达的 `Context::store`，否则插件可替换/摘除记录；⑰ panic 记 `host_drop`（不新增第四种码）；⑱ 金丝雀基线固定为“同一 App、热换前、同一用例集”，硬判据为同用例 Ok→Err / 新 Err，不用跨代分数比较；⑲ 记录点拆成两处（正常完成在 `call_iface`，异常终止在宿主包装层），异常码用已有的 `host_timeout`/`host_cancel`/`host_panic`，`host_drop` 只兜底；⑳ 回滚语义写明两种：apply 失败重挂旧模块（static 保留）、改 YAML 重投是新模块（static 全新），用例须幂等；㉑ 对齐约定补齐 `plugin` 与 `seq`、快照取 swap 后值、同一时钟、消费方轮询写入；㉒ `services_of` 改为薄封装既有 `impls_owned_by`，并声明“待验证面 0.2 前可改可删”。
- 未覆盖（如实记录）：`PluginSourcePolicy::verify` **尚未**校验 `attestation`，§2.3 目前只是准入口径与示例要求；WP1/WP2 的代码与 trace sink 未做；support-contract 的保证计数不变（未增删行）；IO schema 级契约锁未做（需扩 `PluginDesc`）。
- 工作区：本轮只改上述 5 个文档，未改代码、测试、CI/workflow。

### 1. 公开 API / ABI 稳定与版本治理

- 决策记录：`docs/adr/0006-public-api-stability.md`。
- 先冻结 SDK 的受控公开面：ABI 前缀/结构、host/core/SDK 入口、错误载荷。
- 0.1 **保留** `DispatchPolicy` / `HostLimits` 的公开字面量构造；
  `crates/host/tests/embedding_contract.rs::config_dtos_remain_literal_constructible_in_0_1`
  故意固定它，未来加 `#[non_exhaustive]` 会先让测试变红。
- `HostLimits::new` 已提供带校验的安全构造入口；0 在注册前失败。
- `Capabilities` 已有私有 schema 字段，外部本来就不能字面量构造；无需额外 opaque 化。
- **0.2 暂缓（2026-11-15）**：没有真实消费者要求 breaking 变更；0.1.x 继续
  保留字面量构造。启动条件与迁移要求见 ADR 0006 §4；在此之前不引入
  `#[non_exhaustive]`、builder 或 0.2 版本号。
- 已加外部消费者迁移契约：`tests/consumer-service/tests/constructor_migration.rs`
  只用 `LoaderConfig::new` / `HostLimits::new` / `DispatchPolicy::new` 构造，
  不依赖公开字段字面量；`xtask consumer` 会持续编译运行。
- 版本号、ABI 精确匹配、插件 semver 分开表达；不在同一版引入冲突的
  `abi_min/abi_max` 双轨规则。
- 验收：`inventory --check`、支持契约、API 文档与至少两个嵌入示例一致。

### 2. 消费者中立的嵌入契约

- **已落地基础测试**：`crates/host/tests/embedding_contract.rs` 只使用公开
  API（`App` + `LoaderConfig::new` + `load_with_source` + `PluginSource`），
  完成装载、挂载、关闭；不引用 `crate::` 内部模块或 agent 词汇。
- 同一文件增加服务组合用例：provider 通过公开 `ctx.provide` 注册，
  consumer 通过 `Inject::of` 注入；证明 SDK 可作为进程内服务运行时底座。
- **独立服务消费者**：`tests/consumer-service` 是独立 workspace，只用公开
  path 依赖 + 公开 API；`cargo xtask consumer` 已接入 `xtask ci`，三平台
  GitHub CI 也直接运行该步骤和 consumer fmt 检查。
- `xtask consumer` 先构建真实 `greeting` / `hello` cdylib，再验证 ABI 装载、
  跨插件服务注入（hello inject greeting）与 `App::close` 后实例归零；另有
  进程内 fake service 基础契约。
- 生产准入路径已有独立测试：真实 sidecar + `PluginSourcePolicy` 校验通过后
  才装载并组合；`set_allow_unverified(true)` 的开发模式测试仍保留。
- 正式 `/2` 封存消费者演练：`cargo xtask seal-consume <dir>` 用封存的
  `allow-list.json` 装载 `greeting`/`hello` 并关闭；`0.1.0-formal` 与
  `0.1.1-formal` 均通过。该命令是发布门禁，不进入普通 PR CI。
- 该测试同时固定了一个边界：进程内 fake plugin 不经过 ABI 实例表，
  `loaded_instances()` 只适合 cdylib 路径，不能拿它当通用挂载计数。
- 仍需用两个真实消费者验证同一组原语：服务运行时与 agent 运行时。
- 只把**两者都需要**的能力放进 core/host：生命周期、依赖注入、服务接口、
  配置 schema、错误通道、容量与取消边界。
- agent 协议、模型 provider、SSE、JSONL 继续留在消费者工程；不把它们的字段
  加进 `PluginDesc` 或 host 公共类型。
- 验收：两个消费者都不需要 fork runtime；核心 crate 不依赖 agent 词汇。

### 3. 插件身份与依赖治理

- **已落地最小原语**：`Plugin::version() -> Option<&str>`，默认 None；
  `Provisioned` / `VolatilePlugin` wrapper 会保留版本；`Capabilities.version`
  与 `inspect` 文本/JSON 已展示该字段；`#[cordis::plugin(version = "…")]`
  可在 typed 插件上声明。它只是诊断/身份元数据，不替代 ABI 精确匹配或来源策略。
  冻结的 `PluginDesc` 没有 semver 字段，ABI 路径仍为 None。
- 版本诊断边界见 ADR 0007：`Plugin::version` **只展示**，不参与服务解析、
  挂载准入、ABI 匹配或来源策略；`plugin_versions_do_not_gate_service_composition`
  故意让未来误用变红。
- sidecar `PluginManifest.version` 仍只作为 manifest 身份/诊断字段，并在依赖
  计划里参与重复身份判断；它不推导 semver range，也不替代 ABI。
- 从 `PluginDesc` 自动采集 provides/requires，作为 sidecar 的补充，而不是只靠
  手写表。
- 决定是否需要 ABI 尾字段；先做真实消费者需要的版本诊断，不先造升级/回溯求解。
- 继续保留 `strictSidecars` 的显式严格模式；默认宽松是兼容选择，不是安全承诺。

### 4. 微服务运行时原语审查

- 审查服务注册表、生命周期、配置装配、错误语义和取消边界是否足以支撑
  **进程内可信服务组合**。
- 网络协议、远程调用、独立故障域不属于 T2；需要时另立 T3/传输 ADR。
- 不把 realm/scope 宣传成安全边界或租户隔离。

### 5. 发布证据收尾

- 0.1.2 严格同源封存已完成；0.1.0/0.1.1 保留历史跨源证据。
- 历史发布版 × 新宿主已加手动 `compat-historical.yml`，不在 PR 门禁。
- `--require-v2` 与 `seal-consume` 已进入发布 checklist。
- nightly 首次 cron 已通过（`43527774240`）；日志已按唯一文件名转存到 Release
  `nightly-soak`，手动 run `43703088323` 验证。
- 仍需：SCM 持续重跑、`RUSTFLAGS`/`.cargo/config` 自定义 rustflags 记录；
  seal 已记录 `source.commit` / `source.tag`，但 `--verify` 保持兼容、
  不要求它们。
- `--root` 的 toolchain 探测已走目标树目录；历史 tag 无 `--source-policy` 时
  明确回退当前宿主并记录跨源，不伪装 strict。

## 仓外消费者证据（非主线）

### `desolate` agent 链

- 可选 OpenAI-compatible 纯 HTTP SSE adapter：`delta.content`、`[DONE]`、
  流内 error、EOF without `[DONE]`；7 个 adapter 单测。
- `--total-timeout-ms` / `--first-token-timeout-ms` / `--idle-timeout-ms`
  已验证 `timed_out/unknown` 收尾；属于消费者预算，不是 host 取消协议。
- 未完成：HTTPS/TLS、工具参数增量、usage、真实 provider smoke、SSE sink、
  显式 provider cancel。
- 这些能力不进入本仓 roadmap 主队列；如果某个原语被服务运行时和 agent
  运行时共同需要，先写 ADR 再进 core/host。

## 阶段 0：验证基线

**已完成：** 三平台 CI、平台矩阵、支持契约、API 清单、冻结 ABI v1、资源基线、
1M soak、正式 `/2` 封存、公开仓。

**仍未闭合：**

- 0.1.2 正式 tag 已生成严格同源封存：`source.sha256 == host.source_sha256`
  （`830bf97d…`，224 文件），`seal --verify --require-v2` 与 `seal-consume`
  均通过。0.1.0/0.1.1 仍是历史跨源证据；历史 tag 不回溯补严格同源。
- 历史发布版 × 新宿主已有手动 `compat-historical.yml`，仍不在 PR 门禁。
- nightly 首次 cron 已通过；日志已转存到 Release `nightly-soak`。

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
| A5-2 nightly soak | 进行中 | 1M Linux、RSS 预算、dhat；首次 cron [43527774240](https://github.com/yun1091724578/rust-cordis/actions/runs/43527774240) 通过（40m49s，log SHA256 `0E2117B8…`）；日志已转存到 Release `nightly-soak`，run [43703088323](https://github.com/yun1091724578/rust-cordis/actions/runs/43703088323) 验证唯一 asset |
| A5-3 正式 `/2` 封存 | 0.1.2 完成，strict same-version 满足 | `xtask seal --root v0.1.2`；`dist/cordis-0.1.2-formal`（`source == host`，`830bf97d…`）；0.1.0/0.1.1 保留为历史跨源封存；Release assets；`--require-v2` 可用 |
| A5-4 历史 × 新宿主 | 手动 CI 通过，不在 PR 门禁 | `compat-historical.yml` 在 Windows runner 下载 0.1.0/0.1.1/0.1.2 assets 并依次 `seal-consume`；run [43674995479](https://github.com/yun1091724578/rust-cordis/actions/runs/43674995479) 通过 |
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

- T3 / WASM / 子进程：只有第三方、未按 §2.3 签发的模型生成、多租户或硬抢占需求出现
- 有界线程池 + 超时放弃：不能取消同步调用，会留下执行中的插件线程
- 远程协议 / 多租户安全底座
- 为了统一形式新增事件返回协议
- 没有消费者和 ADR 的阶段 4/5/6 能力
- 把单次 SCM 实测或单次历史演练写成持续保证

