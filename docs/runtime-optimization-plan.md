# 运行时正确性与可维护性优化实施方案

先固定关闭、异常结算和句柄回收的不变量，再处理配置采纳、验证证据与结构拆分。本方案不授权一次性重写。

- 日期：2026-11-16；源码审查基线：`acae24f`。行号属于该快照，后续以符号和测试名定位。
- 状态：实施规格已确认，代码按小批推进；实际红绿证据和各项进度仅见 roadmap，不由本文推断任务已完成。
- 执行入口及任务状态唯一来源：[roadmap](roadmap.md#运行时优化专项2026-11-16)。本文定义规格、依赖、验收与审查记录，不维护第二份完成状态表。
- 范围：可信 T1/T2 运行时。不新增隔离保证、不改变 ABI v1、不启动 0.2，不自动发布 typed SDK。
- 本次只修改文档。workflow、发布配置和共享基础设施变更必须另获维护者授权。

## 1. 基线、证据与纠偏

上轮本机通过 `cargo.cmd xtask ci`、workspace all-targets 测试以及额外 doctest；doctest 已接入 `cargo test --workspace --doc`，当前 6 passed / 0 ignored，5 段 macros 示例为 text 并在 sdk 侧有等价编译示例。历史绿灯不证明新增故障路径已覆盖，也不替代三平台结果。

证据等级：**S** 为静态调用链成立，尚无本轮动态复现；**R** 为待复现风险；**D** 为已声明限制或证据缺口，不直接归类为 bug。反证成立时缩小或撤回任务，不为维持结论而改代码。

验收按任务类型选择，不统一要求红测：

| 类型 | 修改前证据 | 修改后验收 |
|---|---|---|
| bug 修复 | 确定性业务失败断言及复现命令 | 最小修复转绿与保护测试 |
| 结构移动 | 行为特征测试及原有绿基线 | 前后均绿，所有权和调用点无遗漏 |
| 评估 | 可重跑语料、基准和比较方法 | 差异记录及迁移或不迁移决策 |
| 新能力 | 旧限制探针 | 新接口正反测试及兼容性检查；不要求不存在的 API 在旧版编译 |
| 文档 | 来源与引用核对 | 链接、符号、状态、保证的一致性 |

编译错误、零测试执行、故意破坏断言均不是业务红测。混合任务拆分验收类型。

必须保留的纠偏：

1. GitHub CI 未显式调用 docs/inventory/matrix，但 `xtask/tests/cli.rs` 的真实文档测试会执行它们，远端不是完全漏检。
2. 已有旧插件到新宿主、新 SDK 到截短旧 HostApi 的测试；真实历史二进制覆盖方向需另核，不从零重建 ABI 矩阵。
3. 已有 pending 原因、cycles、realm、逐行失败和树快照；图形导出不属本轮必要工作。
4. 默认 `!!js` 是受限解释器，尚未证明任意代码执行；YAML 维护风险不是已证实漏洞。
5. Windows GNU 中文构建路径问题不通过向 `crates/core` 增加 PathEncoding 抽象解决；Unix 非 UTF-8 路径是 host 平台边界的独立问题。
6. 不新增 DevContainer、无锁容器、通用事务框架、broker、自动重试或 T3。

## 2. 优先级与执行图

P1 表示优先验证和修复，不表示已有生产事故；未动态复现的风险不标成 P0 漏洞。S 约 1–2 工程日、M 约 3–5 工程日、L 须继续拆分，不包含跨平台等待与评审时间。

| ID | 优先级 / 量级 | 目标 | 证据 | 依赖或排期 |
|---|---|---|---|---|
| O1 | P1 / M | 关闭准入、任务退出和交付所有权 | S | 无 |
| O2 | P1 / S | 派生句柄原子回收 | S | 无；与 O1 同文件修改串行 |
| O3 | P1 / L | 首次及后台 poll 异常与正常收尾 | S | O1；按通道分批 |
| O4 | P1 / M | owner、调用 lease 与 observer 保活 | S | O1、O3 清理边界 |
| O5a | P2 / M | reload 候选统一采纳 | R | 排在 O1–O4 后，不是技术依赖 |
| O5b | P2 / S | Replace 失败语义及恢复边界 | R | 与 O5a 分批验证；同属 O5 |
| O6 | P1 / S | 消费者隔离及业务断言 | S/D | 可独立先做 |
| O7 | P2 / M | 声明预检报告与实际绑定边界 | D/R | 提交接线协调 O5a/O9；联动测试可提前 |
| O8 | P2 / M | credit 等待与 ABI 扩容边界 | S/D | O1、O3、O4；分两批 |
| O9 | P2 / M | 来源策略局部化且贯穿 reload | D | O6；公开 API 先评审，与 O7 协调 |
| O10 | P2 / M | 门禁、包消费者及历史证据 | D | O6；workflow 另行授权 |
| O11 | P3 / L | 按状态所有权拆宿主模块 | D | O1–O5、O8 不变量固定 |
| O12 | P3 / M | YAML 语料及迁移评估 | D | 语料可并行；迁移排在 O5 后 |
| O13 | P3 / S | 接入文档与平台路径边界 | D/S | 文档可并行；路径修复独立测试 |

推荐 O6 与 O1 测试设计并行，随后 O1 → O2 → O3 → O4。O5/O7 是配置线，O8 是资源线；O9/O10 不阻塞前四项。O11 不与行为修复混提交。同一时间只安排一个实施者写 `crates/host/src/lib.rs`，其他人员设计探针、审查或编写不冲突的测试。

## 3. 第一批：运行时不变量

### O1：关闭准入、任务所有权与再入

**依据：** `crates/host/src/lib.rs::apply_via_with_keepalive_and_format`（3853–3942）排水之后交付 abort 与 OP_DISPOSE；`insert_dispatch`（1129–1179）没有 closed 检查，`with_resolved_payload` 不提供工作准入。`unregister_dispatch_node` 正常完成时直接移除记录；iface/dispatch 先摘表后交付，`fulfill` 还可能摘掉 next 共用的记录。业务记录消失不代表任务已经退出。

`HostStreamObserver::credit_available/finished` 经 `deliver_to_plugin` 取出裸 `CallFn` 后放锁调用，没有相应关闭闸门或调用 lease；这条路径同时属于 O4/O8，不能只修普通 iface 调用。

**目标不变量：** 关闭线性化点之后拒绝新工作；此前接受的任务均结束或取消并等待。业务终态与任务退出分离：只要任务仍活着，其 done 必须对关闭等待者可达。允许清理所需读取和合法终态，不把 closed 等同于所有 API 禁用。

**实施步骤：**

1. 审计 `call_inbound`、`host_ctx_call_iface`、`dispatch_impl` 的 insert，以及 `ctx_next`。emit 经过 insert，但独立 spawn，不经过 `complete_or_defer`，不能写成绕过 insert。
2. 为所有 spawn、挂接和派生任务列出清单：创建点、登记 owner、业务 record、任务 done、所有权转移点、关闭等待者及自等待排除。包含 emit、stream timeout/settle、observer、自 dispose 和子 fiber；不能机械地把每个任务交给正在被它等待的关闭流程。
3. 审计全部摘除路径：正常完成、同步返回、fulfill、stream observer、取消、超时、panic、卸载。明确摘表后交付期间以及 next 仍执行却被 fulfill 摘表时，谁保有 done 和调用 lease。
4. 登记与排水使用同一同步协议，记录线性化点及锁序；保留 dispatches → reclaiming 原子转移，并使正常摘表后的任务所有权也原子转入关闭可见的等待集合或等价机制。不得只读取一次 AtomicBool，不得出现无人拥有 done 的窗口。
5. 不持表锁调用插件或 drop 最后强引用；不以新的同步 lifecycle 读锁包围所有 API。保留 committed 服务读取、一次 abort 和 OP_DISPOSE，不提前注销句柄规避问题。
6. 正常 credit 受关闭准入约束。卸载所需终态驳回由关闭流程持有效 lease，在 OP_DISPOSE 前完成；一律改成 `call_live` 可能丢掉终态，不是合格方案。所有等待关系须排除自等待。

**新增确定性测试：** 卸载 abort 重入、OP_DISPOSE 重入；暂停在正常摘表后交付前并发关闭；next 未结束时 fulfill 摘掉共用记录；observer 取出回调目标后暂停、并发关闭再恢复；满窗口关闭。fake C 入口只记录，不 panic/assert；Rust 测试体断言。

**验收：** 关闭后新服务执行计数不增加；已接受任务全部 drop；close 返回后释放 gate 不再产生副作用；done 始终可达且无自等待，通知/dispose 各至多一次。observer 在保护期不析构，已接受等待者收到一次约定终态。不能只看 `loaded_instances()==0` 或失效句柄的 `dispatches_len()==0`。

**错误语义：** 生命周期错误不冒充容量拒绝。仅能证明特定子调用未执行时用 `not_executed`，不能推断上游 handler 未执行；否则保持 `unknown`。waterfall 容量拒绝继续尾链的行为不变。

**保护测试：** `crates/host/tests/abi_iface_unload_abort.rs`、`crates/host/tests/abi_outbound_close.rs`、`crates/host/tests/abi_outbound_abort_close.rs`、`crates/host/tests/abi_outbound_cancel_close_race.rs`、`crates/host/tests/lock_discipline.rs`、`crates/host/tests/capacity.rs`。新场景尚待新增。

### O2：派生句柄原子回收

**依据：** `crates/host/src/lib.rs::host_ctx_dispose`（2389–2421）分开查 owner、查 fiber、remove.expect；竞争删除可触发持锁 panic 和表 poison。

**最小改动：** 同一临界区检查存在与 owner、取得 fiber、确认必要 runtime、摘表；锁外 spawn、释放条目及最后强引用。重复回收返回失效错误，不先 remove 再校验，不用统一 `into_inner()` 掩盖不变量损坏。自 dispose/子 fiber 的关闭等待责任按 O1 清单审核。

**新增测试：** 双 dispose、dispose 与根注销竞争、外 owner 拒绝后条目仍在、活子 fiber 无 runtime 失败但不丢句柄、无活 fiber 的 isolate/intercept 同步回收。Barrier 只保证共同起跑；用私有 gate 或内部表探针命中旧窗口，可能 poison/abort 的用例独立进程执行。

**验收：** 最多一次成功摘表，无 panic/poison，其他派生句柄继续工作，清理不持表锁进入插件。回归 `crates/host/tests/abi_m1.rs` 与根注销测试。

### O3：panic、取消与正常完成统一收尾

**依据：** `crates/host/src/lib.rs::complete_or_defer` 首次和后台 poll 缺完整异常收尾；iface 在 poll 前登记，首次 panic 可能残留记录，后台 panic 可能不回调直到取消/卸载。实现内已落地：`CatchUnwind` / `DeferFailure` / `panic_payload`，测试为 `o3_` 前缀。

分三批：iface 首次/Pending 后 panic；dispatch/next 的 record、续延 kind 与 waterfall frame；独立 spawn 的 emit 及 on_done panic。next 复用入站记录，不能照搬 iface 整记录删除。

先画正常、同步返回、fulfill、observer、取消、超时、panic、卸载的摘除/交付/退出图。`unregister_dispatch_node` 移除记录以及 iface/dispatch 摘表后交付都必须保留关闭可见的 done；fulfill 摘 next 共用记录不能丢失未退出任务。业务终态只能认领一次，done sender 析构只表示任务退出，不能当业务交付证明。按 O1 原子转移且禁止自等待，复用其摘表后暂停与 fulfill/next 竞态测试。

保持同步快速路径、取消优先及文本降级；不把 panic 当空结果或 `not_executed`。只捕获合法 Rust unwind，不承诺捕获 `extern "C"` abort、OOM abort、panic=abort 或永久同步 hang，不在 fake C 回调注入 panic。

**验收：** 各通道首次/后台 panic 有可观察错误或明确 emit 诊断；正常完成同样证明任务退出、记录回基线；cancel/close 不双交付，certainty 保守。用 oneshot/Notify 控制 poll，不靠 sleep。保护 host `--lib`、`crates/host/tests/panic_boundary.rs`、`crates/host/tests/abi_outbound_abort_close.rs`、`crates/host/tests/abi_outbound_cancel_close_race.rs` 及现有错误通道；现有 panic_boundary 不等于覆盖 future panic。

### O4：keepalive 所有权与 observer lease

**依据：** `crates/host/src/lib.rs::apply_via_with_keepalive` 将 Arc 移入只保存 Weak 的 `register_instance_with_keepalive`，唯一强引用可能在 apply 前释放。标准 LoadedPlugin 另持 Arc，不能泛化为必然 UAF。另审 `HostStreamObserver::credit_available/finished → deliver_to_plugin` 复制裸 `CallFn` 的无 lease 路径。（本批已修：强 owner 移入实例表槽位，随注销在锁外释放；见 roadmap O4 记录。）

区分实例生命周期强 owner、实际调用短期 lease、关闭后陈旧句柄拒绝。普通 Instance/PluginIface 克隆不能无限延长 owner；最后 Arc drop 不在表锁内。Any 可能回指，禁止把所有 Weak 改 Arc，禁止 dispatch record 持有含自身表的 Instance 形成环，不删事件快照必要保活。

observer 正常 credit 受关闭闸门约束；终态驳回由关闭流程持 lease 在 OP_DISPOSE 前交付，不允许改用 `call_live` 后静默丢终态。取目标与取得 lease 必须遵守同一关闭协议。

**新增验证：** 唯一 Arc<DropProbe> 移交公开 API，覆盖 apply、普通回调、abort、cleanup、dispose；事件快照及 observer 取目标后暂停，并发关闭后恢复；满窗口关闭保活且一次终态。旧 iface 不可调用已关闭实例，也不可永久拖住库。保护期内不析构，最后合法调用后可析构，无环，staged DLL 文件可释放。回归 `crates/host/tests/abi_lifetime.rs`、`crates/host/tests/loader_release.rs`、`crates/core/tests/weak_audit.rs`。唯一 Arc 移交公开入口的用例已落地为 `crates/host/tests/abi_keepalive_owner.rs`。

## 4. 第二批：配置、证据与资源

### O5：reload 候选采纳与 Replace 语义（分批）

**O5a 候选采纳。** `crates/host/src/loader.rs::LoaderState::reload` 先推进 main/owners/baseline 再预检失败回滚；第二次 reconcile 预检、队列替换和并发回写可能改变前提。先作为 R 类验证，O1–O4 前置仅是合入排期，不是技术依赖。

候选必须包含 `{entries, main, owners, baseline}` 及 generation，一起传入 `request_reconcile`，一起合并、淘汰和采纳；不能只排队 entries 而提前提交元数据。定义同一同步协议下请求登记的单调 generation 顺序，而非 parse 完成顺序：最新请求替换待处理旧候选，旧解析/预检稍晚完成不能复活。最终预检后再次检查 generation 与回写版本，才原子采纳整份候选。

运行时回写成功提交或修改已采纳目标时推进状态版本，使基于旧版本的在飞/排队候选失效；失效候选必须重新读取/构造并预检，不能用旧 baseline 覆盖回写。磁盘基线比较和原子替换仍保留。最终预检失败或候选过期不采纳；采纳后 mount/dispose 等动作失败则保留目标意图并报告实际状态，下一轮继续对账，不能把任意动作失败都解释为回滚候选。

使用可控 PluginSource/gate 测试两次预检不同、reload 交错、运行时回写交错、排队候选被替换及 driver future 被取消。取消时明确 driver owner、接续执行者、队列唤醒与清理责任，不能留下半份元数据或永久占用 running 标记。禁止持 std Mutex 跨 await 或锁内调用来源/用户代码。（已落地：Candidate 携带整份元数据、代次单调、回写版本失效、按磁盘重建；测试落点 `crates/host/src/loader/loader_adopt_tests.rs`，因需断言私有字段放在 crate 内）

**O5b Replace 语义。** `Action::Replace` 先 dispose 再 mount，不等同 DLL swap 的 RolledBack/Lost。可前移的 artifact 检查先决策再前移；不承诺插件副作用事务回滚。单独验收：前移的预检失败时旧服务仍可用；dispose 后 hook 否决不得声称旧实例保留；新 mount 失败时报告、实例、已采纳目标一致；下一轮对账可恢复。是否重建旧实例另行决策。（已落地：Replace 先 `source.load` 决策再拆卸、失败保留旧实例、成功复用同一 artifact；短路与失败的报告语义已分开；测试落点 `crates/host/tests/loader_reconcile.rs`。未做：旧实例重建、artifact 之外的额外前移。）

**验收：** 未采纳候选不改变已采纳回写元数据，旧请求不覆盖新请求，外部编辑不被覆盖，磁盘失败可诊断。回归 `crates/host/tests/loader_reconcile.rs`、`crates/host/tests/dependency_plan.rs`、`crates/host/tests/loader_release.rs`；不可复现时记录反证并缩小任务。

### O6：独立消费者的可信断言

`tests/consumer-service/tests/{abi_service_policy,abi_service_dev,abi_service_failures}.rs` 与共享夹具 `tests/common/mod.rs` 里的消费者测试修改同进程全局策略；mounted_labels 仅证明名字存在。优先独立进程，或覆盖全部相关测试的串行隔离；复位全局状态、清理临时资源，不在多线程 Tokio 测试持 std Mutex guard 跨 await。（已落地：拆分为独立测试二进制 + 真实服务调用断言 + 准入负向用例；`xtask consumer` 已自动覆盖，未改 CI。）

等待明确 Active/就绪条件，实际调用 greeting/hello 并验证返回值和错误路径，关闭后验证资源恢复。移除 provider、破坏返回值、consumer 停 Pending 均应使测试失败；开发和生产准入两条路径都实际调用服务。

`cargo xtask consumer` 先构建真实 DLL，但不执行正式 seal 场景。正式消费者须另跑 `cargo xtask seal-consume <已验证封存目录>`；直接跑独立 workspace 时缺环境变量会直接 return，不是有效验收。正式场景只断言该版本支持的公共行为，记录封存版本、产物 hash、宿主 commit、实际命令及服务调用结果，不伪造跨版本保证。

### O7：声明预检报告与联动测试

`crates/host/src/loader.rs::preflight_source_dependency_plan`（2338 附近）缺计划返回 Ok；resolve 结果不参与 core 注册绑定。遵守 [ADR 0007](adr/0007-version-is-diagnostic-only.md)，声明检查不是 runtime verified。

报告区分：不适用行、无声明 `Ok(None)`、`Err` 降级（包括 sidecar admission 失败）、计划不完整、检查通过；记录相关行、原因和整次预检是否跳过。明确 `strict_sidecars` 对 Err 与 Ok(None) 的现状，不静默扩大含义。默认宽松时预检 Err 降级不代表后续 load admission 放行，加载仍须独立校验。

显式计划覆盖 manifest 的规则不变。当前相同插件声明去重不表达完整 realm 实例图；测试同域不同版本、跨 realm 同键、动态 disabled、混合来源，同时记录预检及实际 Active/绑定结果。若确认预检产生跨 realm 误判，先决定缩小承诺、调整报告或另行修正预检范围，不在本任务默认引入 realm 感知求解。不改变 core 同域单提供者规则，不引入运行时 semver 自动绑定。

联动测试可先行；候选采纳接线与 O5a、来源策略接线与 O9 协调。报告若公开，按 [ADR 0006](adr/0006-public-api-stability.md) 评估兼容性并更新清单与消费者。（已落地：crate 私有 PreflightReport 经 report()["preflight"] 暴露，逐行分类 + 声明级标注；联动测试落在 crates/host/tests/dependency_plan.rs 与 loader_adopt_tests.rs。跨 realm 误报按「缩小承诺 + 标注 notModeled」处理，realm 感知求解另立后续项。）

### O8：资源边界与测量

**O8a credit。** `crates/core/src/stream.rs::StreamSink::push_awaiting`（`push_with` 的满窗口分支）满窗口 waiting 继续追加。先确定 outstanding credit 上限、重复 ID 与拒绝反馈，审查 SDK 使用方式。公开 `PushOutcome` 只有 `Sent/Pending/Gone`：新增拒绝必须兼容评审，不能直接扩 enum，也不能伪装成 Gone。不能盲目去重或静默丢回调。

区分已接受等待者和拒绝登记者：前者按约定收到一次 credit 或终态；后者立即收到明确拒绝且不进入等待集合，不要求其获得已接受等待者的未来回调。覆盖慢消费者、重复登记、多 producer、finish/fail/drop、满窗口关闭，并复用 O1/O4 的 observer 取目标暂停/关闭/恢复测试。`HostStreamObserver::credit_available/finished → deliver_to_plugin` 的正常 credit 受闸门保护，卸载终态由关闭流程持 lease 在 OP_DISPOSE 前完成，保持保活和一次终态。数据窗口有界不代表 chunk 字节有界，字节预算须测量和兼容评审。（已落地：宿主层每流 64 条挂起推送上限、预留式记账；core 不改。重复 id 仍按次数。）

**O8b ABI 扩容。** `crates/host/src/lib.rs::invoke_raw_with`（`STATUS_TOO_SMALL` 分支按插件回填的 `out.len` 换缓冲）分配只读缓冲。定义最大合法输出及超限错误，分配前校验并使用可失败分配，不实际申请巨量内存。测试上限、上限加一、usize 极值、注入分配失败、二次 TOO_SMALL。仍最多一次只读重试，写 op 禁止重放。上限根据合法输出和消费者需求确定，合入前写入测试及文档。（已落地：只读重试上限 1 MiB + 可失败分配 + 二次 TOO_SMALL 可读诊断；SDK 侧只防越界，不设业务载荷上限。）

**测量：** 记录构建模式、工具链、平台、payload、并发、慢消费者速度、峰值内存、吞吐及 p95/p99。复用 buffer benchmark、soak/dhat，不新建性能框架，不把本机指标写成跨平台保证。（本轮未新增指标输出，保留现有 benchmark/soak/dhat，记后续。）

## 5. 后续维护批次

### O9：局部来源策略

全局 `ACTIVE_POLICY/ALLOW_UNVERIFIED` 位于 `crates/host/src/source.rs:47–104`。先评审不可变显式来源策略，保留旧入口兼容回落，不破坏公开单位结构 DiskPluginSource 的构造方式。定义三态：显式严格按绑定策略校验；显式开发仅按自身开发配置放行；兼容回落才读取全局。显式无严格策略不等于兼容回落。

策略须贯穿 plan/probe/load/reload、`reload_one` 以及暂存副本二次验证。特别是 `crates/host/src/loader.rs::reload_one` 绕过 `state.source.load`、直接读取全局准入的路径，不能仅给 DiskPluginSource 添加字段就宣布完成。与 O7 协调预检和加载使用同一策略快照，预检降级不取消实际准入。

验证双来源并发：A 开发放行不影响显式严格 B；初装后更改全局再 reload，B 仍拒绝未批准新产物，包括暂存副本替换/二次校验。隔离保证只针对显式入口，兼容回落仍受全局影响。明确策略创建、使用、替换和关闭责任，不重写成大 HostRuntime。

### O10：门禁与发布证据

1. 先列实际覆盖：本地 xtask 已有 fixture fmt 和主 soak 注册检查，远端已有默认 feature 的 MSRV 检查；xtask 集成测试间接检查 docs/inventory/matrix。远端补 fixture fmt，显式 doctest 和 feature/MSRV 组合分别接入适用入口；不要把已有项当成全新缺口。现状：A/B 已落地（doctest 进 `ci`、clippy/check 带 `--all-features`、`soak`/`open_staged_soak`/`dhat_profile` 均有 `--ignored --list` 守卫、新增 `cargo xtask dhat`）；C（隔离目录 package）、D（五类证据表）、E（formal_seal 业务断言）、G（契约收口）未做；F（远端 workflow 接线）待授权。逐条证据见 roadmap 的 O10-A/B 记录。
2. 对 open_staged/dhat 补注册与实际执行防空跑，不能把主 soak 注册检查等同所有资源测试覆盖。昂贵测试不直接塞进 PR；可建议保留 dhat 日志，但 workflow/存储变更须另获授权。
3. 关键 ignore 示例改成可编译示例或外部消费者用例；`cargo test --doc` 不验证 ignore。
4. 隔离目录验收待发布 package，不依赖仓库源码 path。先离线/本地隔离源演练依赖包，再按发布流程验证 registry；明确依赖顺序、锁文件和网络条件，不要求实际发布才可测，不自动上传包。
5. 区分 ABI 布局、截短宿主桩、真实 DLL、业务结果、历史运行五类证据；包括 `crates/abi` 的布局证据，不能以 inspect 成功替代服务调用。历史结果绑定版本/hash、宿主 commit、命令及实际调用结果。

**验收：** 改坏可编译示例会红，所选非默认 feature/MSRV 组合可编译，包消费者无源码 path 后门，资源测试确实执行。workflow、部署、发布设置另开授权批次。

### O11：宿主按所有权拆分

O1–O5、O8 不变量和测试固定后，记录表 owner、锁序、销毁点和插件调用点，再分批移动 handles/dispatch/FFI buffer/staging。保留公开 facade 和行为，不升级协议或重写状态机。测试移动单列；用行为特征及前后绿验收，不制造假红测，不以行数为目标。

### O12：YAML 语料与替代库评估

复用 `crates/host/tests/patch.rs`、`crates/host/tests/interpolation.rs`，增加脱敏配置：`!!js`、引号、块标量、其他标签、BOM、null、嵌套组、patch 归属、回写和非法输入。先查现状：`crates/host/src/patch.rs::dump_entries` 将 Entry 转值再用 serde_yaml 序列化（1102–1105），并非原文 round-trip；不得预设注释、键序、引号无损是已有保证。

建立可重跑旧/新解析器语义与回写对比，逐项决定需要保留的语义和格式，新增无损需求另行决策。核对候选维护状态、许可、MSRV、标签行为；不预设兼容 fork 合适。语料差异决策前不替换依赖，可明确选择不迁移，不与 reload 重构混批。

### O13：接入与平台边界

同步 README 发布入口与可核验证据，提供进程内 typed 和动态库 ABI 两条最小路径，复用消费者验证；typed 门面发布另决策。`crates/host/src/dylib.rs` 的 Unix to_string_lossy 路径单列原始非 UTF-8 文件名实际加载回归，修复限于 host OS 边界。Windows encode_wide 与 GNU 构建路径问题分别说明，不增加 core 路径抽象。

## 6. 测试命令与合入门禁

以下为现有命令；新场景实现后再登记 target。本机 `cargo.cmd` 包含缓存代理及工具链配置，需先核对本机环境，不能推广为通用 Windows 要求；普通环境直接 `cargo`。本次仅文档修订，不运行全套测试。

```text
cargo test -p rust-cordis-host --lib --locked
cargo test -p rust-cordis-host --test abi_m1 --locked
cargo test -p rust-cordis-host --test lock_discipline --locked
cargo test -p rust-cordis-host --test abi_iface_unload_abort --locked
cargo test -p rust-cordis-host --test abi_outbound_close --locked
cargo test -p rust-cordis-host --test abi_outbound_abort_close --locked
cargo test -p rust-cordis-host --test abi_outbound_cancel_close_race --locked
cargo test -p rust-cordis-host --test capacity --locked
cargo test -p rust-cordis-host --test abi_lifetime --locked
cargo test -p rust-cordis-host --test loader_release --locked
cargo test -p rust-cordis-host --test loader_reconcile --locked
cargo test -p rust-cordis-host --test dependency_plan --locked
cargo test -p rust-cordis-core --test weak_audit --locked
cargo test -p rust-cordis-core --test stream --locked
cargo xtask consumer
cargo xtask seal-consume <已验证封存目录>
cargo xtask ci
cargo test --workspace --doc --locked
```

consumer 不执行正式 seal；缺环境变量直接 return 不能算 seal 通过。封存目录须先按发布流程验证。包名为 rust-cordis-host/rust-cordis-core，不是 Rust 库标识 cordis_host/cordis_core。可先本地运行 `cargo check --workspace --all-targets --all-features --locked`；MSRV/远端组合由 O10 确认。

每批合入前交付：

1. 按 §1 类型提供基线与命令：bug 的实际业务失败断言；结构移动的前后绿；评估的可重跑比较及不迁移选项；新能力的旧限制探针和新接口正反测试；文档的引用一致性。不得制造假红测。
2. 相应目标测试结果；生命周期批次附活任务、记录、句柄、库文件的适用资源断言。
3. 完整 diff、公开 API 调用点及错误语义审查；代码实施批次完成适用完整本地门禁，三平台独立记录。本次文档任务只核对引用和 diff。
4. 状态只更新 roadmap；保证变动同步 support-contract、错误/API 清单及相关 ADR，不为形式新增 ADR。
5. 新错误说明 code/category/certainty 与旧 ABI 文本兼容性；结构改动不得增加 0.1 公共结构必填字段或未经评审扩充公开 enum。

## 7. 风险、停止条件与回退

- 复现失败且找到实际防护：记录反证，降级或关闭任务。
- 死锁、回收退化、旧 ABI 行为改变：停止后续批次，保留复现，不延长超时或忽略测试过门禁。
- 必须 breaking：按 ADR 0006 请求决策，不自行启动 0.2。
- credit 拒绝、扩容上限、Replace 恢复、来源作用域须明确语义，依据消费者与测试定案。
- 独立小批为回退单位；git revert 等仓库写操作另获授权，保留复现与诊断，不删除红测隐藏回归。
- 前四项修复不依赖大模块拆分。

## 8. 实施记录模板

每项在变更说明填写，不提前填“通过”：

```text
任务 ID / 类型（bug、结构、新能力、评估、文档）/ 负责人 / 源码基线：
触发前提与证据等级：
测试或探针位置 / 重跑命令：
修改前证据（业务失败、行为绿基线、旧限制、比较语料或引用核对）：
修改后验收（转绿、前后绿、新接口正反、评估及不迁移决策或文档一致性）：
线性化点 / owner / done 转移 / 关闭等待者 / 自等待排除：
公开 API / ABI / 错误语义影响：
目标测试 / 适用完整门禁 / 平台结果：
历史消费者的版本 / hash / 宿主 commit / 实际服务调用：
资源恢复证据与未覆盖边界：
审查人、阻塞问题及处理：
roadmap 状态与关联文档更新：
```

## 9. 文档独立审核记录

首轮三个独立审核已返回：A（运行时安全）和 C（架构配置）提出阻塞项；B（测试发布）可确认并提出修订意见。本文已吸收摘表后 done、observer lease、候选队列及 generation、reload_one 策略、分类验收、命令与门禁等意见。

2026-11-16 修订后，三个独立 Agent 的复核均已返回：

| 视角 | 首轮重点 | 修订后结论 |
|---|---|---|
| A：运行时正确性 | 正常摘表后的 done 所有权、observer lease 为阻塞；补工作入口清单与 O8 边界 | 方案复核通过，两项阻塞闭合；未发现本轮范围内新增阻塞 |
| B：测试与交付 | 正式 seal 入口、完整源码路径、现有与待补门禁、本机包装器说明 | 首轮无阻塞；轻量复核确认四项建议已吸收，通过 |
| C：架构与配置 | 整份候选采纳、reload_one 策略贯穿、分类验收为阻塞 | 方案复核通过，三项阻塞闭合；roadmap 与规格一致 |

以上是文档可实施性确认，不是代码修复、动态复现、测试或跨平台结果。具体等待集合实现、credit 拒绝接口、扩容上限等仍须在对应实施批次定案。代码任务状态仍仅由 roadmap 维护；未将任何代码任务改成完成。
