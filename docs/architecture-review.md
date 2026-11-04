# 架构现实审查（2026-11-04 更新）

审查基线：`9f5ddfb` 的源码快照，加上 2026-11-04 的仓外 R1/R3 drill（`../desolate`）。
本轮重新跑了本地 `cargo xtask ci` 与仓外消费工程；远端平台结果仍按其 run/commit
引用，不把一次本地结果写成常驻保证。本文是现状审查，不是某个提交的回归归因；
没有足够证据的风险不列为已证实运行时缺陷。执行顺序、状态与验收统一放在
[roadmap](roadmap.md) 的 R1～R6，不另设第二份任务表。

## 已证实的交付与使用缺口

### R2：typed SDK 不在首发范围，低层 ABI 已决策

`crates/macros/src/service.rs` 生成 `::cordis::typed`、`::cordis::core` 路径；
`crates/sdk/Cargo.toml` 定义库名 `cordis`，但 `publish = false`。SDK 有实际职责，
不能作为“空壳”删除。当前四个可发布 crate 不包含这个标准使用入口。
`.github/workflows/ci.yml` 的 package job 使用 `--no-verify`；workspace 内测试
不能证明打包后的 crate 在独立工程里能完成标准 typed 插件集成。

**2026-11-03 决策**：首发支持**低层 ABI + 固定 revision 源码依赖**；registry
typed SDK 不在首发范围。`../desolate` 已按这条路径在 workspace 外完成编译、
低层 ABI 宏展开、真实 DLL 装载、调用成功/失败、卸载、重载与 v1→v2 升级，
记录见 `../desolate/R1-REPORT.md`。这关闭的是“交付路径选择”，不是
“registry typed SDK 已可用”；`cordis-sdk` 继续保持 `publish = false`，
`cordis-macros` 虽可打包，但单独发布不构成 typed 门面，只有未来明确承诺
registry 交付时再处理门面发布与元数据。

### R3：详细调用路径已仓外验证，完整重试留阶段 4

`crates/abi/src/sdk.rs` 的 `call_iface_detailed`、`dispatch_detailed`、`next_detailed`
返回 `PluginResult`，回调接收 `Result<Json, ErrorPayload>`。启动失败仍可能是文本，
异步结果有结构；“详细”不意味着所有失败统一成结构化错误。现有 roadmap/ADR 已登记
这个边界，因此不把它当成本轮新增回归。

独立调用者必须处理返回值与回调，且回调可能内联执行。验收应覆盖三个详细入口的
实际失败，不能只验证超时回调。是否新增统一 API 由真实消费者决定，保留旧 API 的
兼容性。
`../desolate` 已新增 `plugins/detailed-probe` 与 `plugins/fault-provider`，用真实
T2 调用方覆盖 `call_iface_detailed` 的同步启动错误、provider 业务错误回调、
`HOST_TIMEOUT`、卸载/取消结算，以及续延恰好一次；`sync_start` 场景从无 runtime
线程触发同步 `PluginResult` 错误。2026-11-04 又补了 `dispatch_ok` /
`dispatch_timeout`、`next_ok` / `next_timeout`：真实宿主分别返回正常值或结构化
`HOST_TIMEOUT`，并在卸载后检查续延没有再次投递。探针还有一个窄策略消费者，按
`category` / `certainty` 选择 `accept` / `fail_fast` / `degrade_unknown` /
`unavailable`，记录 `automatic_retry=false`；它不解析文本，也不自动重试。R3 的
仓外使用契约至此闭合；完整重试 / 补偿仍属阶段 4，因为还缺副作用类别、幂等是否
实际生效、截止时间与预算等输入。

### Agent 集成：首条真实链路已完成，边界已显形

`../desolate` 新增了 `agent` 任务：用户请求 → 确定性模型 token 流 → 真实
归档搜索工具 → 正常/失败/超时未知 → JSON 报告 → 关闭。六条路径均通过：
正常、模型流失败、工具超时、普通模式真实超时、工具错误、模型启动失败。工具
结果按 `Ok(Ok)` / `Ok(Err)` / `Err(elapsed)` 实际分支分类，测试模式不负责解释
结果；`Ok(Err)` 无证据时保留 `unknown`。统一收尾路径先生成结果，再发最多一个
`run.finished`、释放插件并关闭 App；部分快照按消息保存。工具超时路径记录
`timeout_unknown` 和 `side_effect_observed=true`。证据见
`../desolate/AGENT-REPORT.md`，协议与设计见
[agent-stream-protocol](agent-stream-protocol.md)。

这条链路证实了两个必须保留的边界。第一，失败的流只带文本原因：已经消费的 token
可见，未读缓冲会被丢弃；如果 agent 需要保留全部部分输出，流错误协议要单独设计。
第二，`DispatchPolicy` 不是宿主调用的通用截止时间；宿主必须自己设置 deadline，
不能把看门狗当执行隔离。链路没有引入自动重试、账本、broker 或进程外载体；这些
仍按真实消费者出现再设计。

### R1：仓外技术接入与样例归档任务已完成，待真实数据反馈

`examples/harness-spike` 使用 SDK 和 registry，ADR 0002 还引用 TS 下游制品的需求。
这些是集成与需求证据，不是独立 Rust 生产部署证据。2026-11-03 新增的
`../desolate` 是仓外独立 crate 图，直接 path 依赖固定 revision，已记录需求场景、
构建命令、调用成功/失败、卸载、重载与 v1→v2 升级；它补上了“独立工程实际使用”
这一格证据。

R1 的验收条件不是“必须由第三方维护”。独立工程可以由同一维护者负责，第三方复现
只是额外证据。是否算 R1，关键在 `../desolate` 是否用于实际归档任务并形成使用/
维护反馈。2026-11-04 已用一个 31 篇的自造样例（25 个 `.md` + 6 个 `.markdown`，
嵌套目录与混合标签）跑通索引、搜索、重载、v1→v2 升级、标签过滤和报告写出；
报告在 `../desolate/reports/archive-report.json`，记录在
`../desolate/R1-REPORT.md`。这补上了任务形状，但仍不是第三方生产数据或长期维护
反馈，因此 R1 仍未闭合，不应写成“等第三方接入”，也不应把样例任务写成生产采用。

## 已知架构边界，保持显式

- 直接 typed 调用没有调用事实参数；宏生成 adapter 的 `_caller_meta` 不转交业务方法。
  手写 `ExternService` 有显式入口。不要靠 task-local 宣称三条路径等价。
- `components::registry::replace` 对已有槽位调用 `ctx.set`，不进行新旧 adapter 契约校验。
  替换不等于卸载重载；`Arc` 保住对象不证明它依赖的外部资源仍存活。只有实际热替换需求
  才安排一致性检查/退场协议，不能把它改写成现存的两阶段装载缺陷。
- `ErrorPayload` 已传递错误类别与执行认知，但它们不是宿主认证、重试许可或幂等证明。
  业务副作用、账本和审批仍属于领域服务或宿主产品。
- ABI 尺寸探测只在同一受支持布局和版本契约下支持扩展；不证明任意插件永不崩溃。
  边界约束是避免跨模块转移释放责任，不是整个调用链零分配；JSON 编解码和插件状态
  仍会分配。同进程插件与宿主同信任域。

## 维护性工作与未证实风险

### R4：YAML 维护风险需要语义迁移方案

`Cargo.toml` 使用 `serde_yaml = "0.9"`，锁文件为 deprecated 版本。
[上游停止发布的说明](https://github.com/dtolnay/serde-yaml/releases/tag/0.9.34)
是维护风险的依据，不是本项目已有可利用漏洞的证明。
`crates/host/src/patch.rs` 依赖标签改写与回写行为，换解析器可能改变配置含义；
替换评估须使用现有 interpolation/patch 测试和真实配置语料，而非只比较 API 兼容性。

### R5：大模块增加状态不变量的审查成本

当前 `host/src/lib.rs` 5117 行（末尾测试模块从 3817 行开始）、`loader.rs` 3788 行
（测试从 3117 行开始）、`abi/src/sdk.rs` 3317 行（测试从 2182 行开始）。
测试不等于生产复杂度，但生产部分仍聚集多种状态和生命周期职责。
拆分先明确锁与所有权，再移动实现；不以文件行数为完成标准，不同时改协议或重写状态机。

`lock().unwrap()` 数量本身不能证明关闭失败。比如 `with_stream_instance` 在
`catch_unwind` 之前取得实例表锁，值得沿持锁 panic 来源审查，但尚未证明该锁能被
用户代码毒化。必须找到可达触发与影响路径，做隔离故障注入后才定缺陷等级。
不得统一使用 `into_inner()` 继续使用可能破坏不变量的状态。

### R6：证据门禁不能替代语义评审

`xtask docs` 验证引用有效，`matrix` 验证矩阵结构；它们不证明测试充分或平台真的执行。
旧红绿记录是历史记录，不等于可从当前 Git 重放的完整变更序列。保留现有检查器，
新增检查须有具体失败场景，避免为叙述完整性再造一层门禁。

当前已存在 Git 历史与 CI 配置；“没有 .git”不再是现状。SDK / components 有消费点且
不发布；没有理由按代码行数删除。项目没有依赖 `libloading`；也不能用依赖少或 unsafe
集中推导安全已经得到证明。旧设计文档保留原路径，加历史提示，避免破坏交叉引用。

## 本轮结论的使用方式

R2 的交付路径已定为低层 ABI；R1 的技术接入与 31 篇样例归档任务已完成，待真实
数据与维护反馈，A5 按发布范围收尾；R3 的 `call_iface_detailed` /
`dispatch_detailed` / `next_detailed` 仓外验证与窄策略消费者已完成，完整重试 /
补偿留阶段 4；真实 agent 首链路已完成，下一步是真实模型、取消与预算，流失败保留
部分输出仍需协议决定；R4/R5 独立小批推进。不因本次审查自动启动 broker、账本、
隐式上下文、沙箱或进程外载体。本文列出的缺口尚未因文档更新而解决，不能提升支持
契约的保证等级。
