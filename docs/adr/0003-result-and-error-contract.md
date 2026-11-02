# ADR 0003：调用结果与错误契约

- **状态**：已冻结（词汇与失败路径证据；阶段 2 传输层已实施，分类/重试消费者留阶段 4）
- **日期**：2026-10-31
- **依据**：`crates/core/src/dyn_value.rs:143`（`ExternService` trait）、`:154`（`call`）、
  `crates/sdk/src/typed/service.rs`（错误字符串化）、
  `crates/host/src/lib.rs`（`DispatchPolicy` 与 `HOST_TIMEOUT`）、
  `docs/adr/0001-second-carrier.md` §2/§4、`docs/adr/0002-broker.md` D-d、
  `docs/support-contract.md`

## 决定摘要

1. **结束原因**与**执行认知**是两个维度，不是一个互斥枚举。
2. “确认未执行”只能由派发前的拒绝给出，且只限定为**目标业务方法未进入**。
3. 错误保留类别；人类可读消息继续存在，但不是唯一的程序判断依据。
4. SDK 提供重试判断所需的事实，不提供“允许重试”的权威布尔值。
5. 调用 ID、尝试 ID、幂等键是三个东西。
6. 本阶段不引入账本、墓碑、迟到结果恢复、自动补偿或崩溃后重试。

本 ADR 不修改代码。结构化错误载荷的传输见 ADR 0004；实施在阶段 2。

## 1. 两个维度

| 维度 | 冻结取值 | 回答的问题 |
|---|---|---|
| 结束原因 | `Returned` / `Failed` / `TimedOut` / `Cancelled` / `Disconnected` | 为什么这次调用停止等待 |
| 执行认知 | `NotExecuted` / `Reported` / `Unknown` | SDK 实际掌握什么证据 |

以下名字自本 ADR 起视为**契约词汇**；阶段 2 的 Rust 类型必须映射到它们，
不得再用 `Completed` / `Timeout` 等同义词另起一套。代码里的近名项也不能直接
当契约：`FiberState::Failed` 是生命周期状态，`mpsc::Disconnected` 是通道关闭，
`SettleOutcome::Settled` 是内部结算形态；阶段 2 必须显式映射。

两个维度正交，不能压成一个三态：

| 组合 | 含义 |
|---|---|
| `Returned` + `Reported` | 提供者报告了成功结果 |
| `Failed` + `Reported` | 提供者报告了业务错误；**不代表没有副作用** |
| `Failed` + `Unknown` | 协议/传输失败，无法确认是否执行 |
| `TimedOut` + `Unknown` | 看门狗不再等待；当前路径下唯一诚实的取值 |
| `Cancelled` + `NotExecuted` | 派发前取消，目标业务方法未进入 |
| `Cancelled` + `Unknown` | 派发后取消；**不证明远端已停止或已撤销** |
| `Disconnected` + `Unknown` | 仅保留给进程外载体 |

`TimedOut + Reported` 在当前路径上不可观测：`tokio::time::timeout` 先轮询内层 future，
T1 的提供者至少被 poll 过一次；T2 的 `OP_CALLBACK` 在返回 `PENDING` 前已经执行插件代码。
要把迟到结果升级为 `Reported`，需要账本——本阶段明确不做。

看门狗还有一个边界：它救不了**同步阻塞**的插件。extern 调用卡在宿主线程上时，
超时没有机会触发。

## 2. 断言来源

| 来源 | 能断言 | 不能断言 |
|---|---|---|
| 宿主 | 派发前拒绝 ⇒ `NotExecuted` | 派发后是否执行 |
| 适配器 | 目标业务方法未进入 ⇒ `NotExecuted`（**ADR 0004 已提供结构化载荷；适配器尚未填 `certainty=not_executed`**） | 方法内部是否产生副作用 |
| 提供者 | 它观察到的结果 ⇒ `Reported` | 结果是否已持久化、事务是否提交 |
| 无充分证据 | `Unknown` | — |

`NotExecuted` 的范围必须写死为“目标业务方法未进入”。解析、依赖校验、插件探测
本身可能运行代码，不能扩大成“整个请求没有产生任何副作用”。

当前可直接断言 `NotExecuted` 的失败面：

- `ResolveError` 的四个变体（`crates/core/src/store.rs:1469`）；
- `IfaceVersionMismatch`；
- “服务没有可调用面孔”；
- 未知方法。

它们都在 `iface_target` / `check_iface_call` 内、在 `iface.call` 之前产生
（`crates/core/src/context.rs:562-606`）。

**参数信封形状错误不在这一组。** 它由 SDK 适配器在 `iface.call` 内部校验
（`crates/sdk/src/typed/service.rs:42-68` 的 `open`，经宏生成的 adapter 分支调用，
`crates/macros/src/service.rs:232-234`）。宿主看不到“目标业务方法尚未进入”这件事；
阶段 2 传输层已支持结构化错误，但调用方仍需显式读取 `code/category`；分类与重试决策留到阶段 4。

## 3. 错误类别

| 类别 | 当前产生者 | 当前形态 |
|---|---|---|
| 解析与依赖 | `ResolveError` 四变体 | 类型化（core 内） |
| 协议与参数 | `IfaceVersionMismatch`、信封形状、未知方法 | 类型化 + 文本 |
| 生命周期 / 调度拒绝 | `ctx_fulfill` 未知/重复、卸载期注册 | 状态码 + 文本 |
| 业务错误 | 提供者 `Result::Err` | 跨 ABI 保留 `error` 文本；能力位为 1 时带 `code/category/certainty/retry/detail` 兄弟字段。同进程 typed 可保留自定义 `E`（仅直接 `ctx.iface`；经 `call_iface` 的 typed 面同样字符串化） |
| 超时 / 取消 / 连接 | `HOST_TIMEOUT`、`CancelToken` | 文本 + 可选结构化兄弟字段（`HOST_TIMEOUT/timeout`、`host_cancel/cancel`、`host_lifecycle/lifecycle`）；`Disconnected` 仅为将来保留 |
| 未知 / 旧协议错误 | 无法分类 | 文本 |

“连接中断”和“调度拒绝”今天没有完整生产者：前者属于进程外载体，
后者只在 `ctx_fulfill` 等少数入口以状态码形式出现。文档必须标注它们是保留类别，
不能声称已经覆盖。

类型化错误只存在于进程内。`ResolveError` / `IfaceVersionMismatch` 跨 ABI 后
都会变成字符串，T2 调用者拿不到类型。这是 ADR 0004 要解决的问题。

## 4. 重试判断

重试判断的输入至少包括：

1. 执行认知；
2. 副作用类别（无 / 有）；
3. 幂等机制是否**实际生效**；
4. 错误是否可能通过重试恢复；
5. 截止时间、预算、取消状态。

第 5 项的截止时间与预算**当前不存在**，取消状态只在事件派发路径上
（`Call.cancel`，`crates/core/src/event.rs:58`）。表格中这两列必须标注
“阶段 4 之后才有”，否则文档会暗示一个不存在的输入。

| 执行认知 | 副作用 | 幂等生效 | 判定 |
|---|---|---|---|
| `NotExecuted` | 任意 | — | 未发现重复执行阻碍 |
| `Reported`（成功） | 任意 | — | 不重试 |
| `Reported`（错误） | 无 | — | 由策略决定 |
| `Reported`（错误） | 有 | 是 | 仅同一幂等键可重试 |
| `Reported`（错误） | 有 | 否 / 未知 | 不自动重试；补偿或询问 |
| `Unknown` | 无 | — | 未发现重复执行阻碍 |
| `Unknown` | 有 | 是 | 仅同一幂等键可重试 |
| `Unknown` | 有 | 否 / 未知 | 永不自动重试 |

“未发现重复执行阻碍”不是“允许重试”：参数错误即使 `NotExecuted`，也没有理由
原样无限重试；无副作用的查询仍可能消耗费用与额度。

## 5. 三个 ID

| 名称 | 谁生成 | 生命周期 | 用途 |
|---|---|---|---|
| 调用 ID | 宿主（当前）；SDK 侧为阶段 4+ | 一次派发 | 追踪、取消级联、诊断 |
| 尝试 ID | 调用方 / 重试层 | 一次尝试 | 区分重试次数 |
| 幂等键 | **领域** | 跨尝试稳定 | 执行方去重 |

SDK 可以携带幂等键，**不证明去重有效**。每次尝试由 SDK 生成的 ID 只能追踪，
不能当作幂等键。

## 6. 明确不做

- TTL 墓碑；
- 持久化调用记录；
- 迟到结果恢复；
- 自动补偿；
- 崩溃后重试。

理由：活动派发记录持有 future、续延、流与插件实例句柄/续延 id（`crates/host/src/lib.rs` 的 `DispatchRecord`），
保留它就会破坏排水与资源释放；而进程内墓碑无法跨崩溃保留证据。
是否需要账本由真正执行副作用的系统决定。

## 7. 失败路径清单

| 入口 | 失败来源 | 当前形态 | 类别 | 可断言的认知 | 证据位置 |
|---|---|---|---|---|---|
| `Ctx::require` | 未声明 / 未就绪 / 跨域 | `ResolveError` | 解析与依赖 | `NotExecuted` | `crates/core/src/store.rs:1469`；`crates/core/tests/committed_view.rs:212`（NoInject）；`crates/core/tests/optional_inject.rs:49`（Inactive）；跨域见 `committed_view.rs`；真实宿主路径见 `crates/host/tests/sdk_apply_failure.rs::failed_apply_releases_sdk_callbacks_registered_before_the_error` |
| `Ctx::require_as` / `iface` | 类型不符 | `ResolveError::TypeMismatch` | 解析与依赖 | `NotExecuted` | `crates/sdk/tests/service_macro.rs:220`（`typed_accessor_reports_type_mismatch_not_noinject`） |
| `Ctx::get` | 不可用 ⇒ `None`（无错误） | `Option<Dyn>` | — | 不适用 | `crates/core/src/context.rs`（`get` 返回 `Option<Dyn>`） |
| `Ctx::call_iface` | 无面孔 / 版本过高 / 未知方法 | 文本 + `IfaceVersionMismatch` | 协议与参数 | `NotExecuted` | `crates/core/src/context.rs:562-606`；`crates/core/tests/kernel.rs:648`（版本/方法分发） |
| `Ctx::call_iface` | 提供者返回错误 | `Error`（跨 ABI 文本；`call_iface_detailed` 读 `ErrorPayload`） | 业务错误 | `Reported`（typed）；跨 ABI `args[0].error` 保留字符串，能力位为 1 时带结构化兄弟字段 | `crates/sdk/tests/service_macro.rs:155`（typed 方法）；`crates/host/tests/sdk_provider_error_detailed.rs::provider_business_error_reaches_detailed_callback_with_code`（provider 业务错误 + `callback_failed/business`）；`crates/host/tests/sdk_async_iface.rs::suspendable_iface_methods_share_everything_with_the_event_path`（跨 ABI 未知方法 / 版本 / 卸载排水）；`crates/host/tests/sdk_detailed_timeout.rs::call_iface_detailed_receives_timeout_payload_from_the_real_host`（详细入口） |
| `Ctx::call_iface` | 看门狗超时 | `Err("HOST_TIMEOUT…")`，记录已撤；能力位为 1 的异步回调带 `code/category/certainty` | 超时 | `Unknown` | `crates/host/tests/abi_outbound_timeout.rs::iface_call_times_out_and_unregisters_dispatch`（断言 `HOST_TIMEOUT/timeout/unknown`） |
| `Ctx::call_iface_stream` | 同步失败 / 流失败 / 取消 | `Err(String)` / 流错误 | 业务 / 超时 / 取消 | 部分 `Unknown` | `crates/host/tests/abi_stream.rs`；`crates/host/tests/abi_stream_timeout.rs`；`crates/core/tests/stream.rs:713`（同步失败） |
| `ctx_fulfill` | 未知 / 重复 / 已取消 dispatch | `STATUS_ERR` + `ErrorPayload`（能力位为 1 时结构化） | 生命周期 | **不适用**（结算失败；目标方法可能已进入，不能写成 `NotExecuted`） | `crates/host/tests/abi_m2.rs:152`（重复）、`:502`（未知）；`crates/host/tests/abi_m2c.rs:272`（abort 后）；结构化分类见 `crates/host/src/lib.rs::fulfill_failures_are_structured_as_lifecycle` |
| `ctx_next` | 超时 / 未知 / 重复 / 取消 | 超时 → `on_done.error`（能力位为 1 时带结构化兄弟）；未知/重复 → 同步 `STATUS_ERR` + `host_next/lifecycle`；取消/注销 → 丢弃帧 | 超时 / 生命周期 | 超时 `Unknown`；未知/重复：结算未发生；取消：不证明已停止 | `crates/host/tests/abi_next_timeout.rs:141`（超时 + `HOST_TIMEOUT/timeout/unknown`）；`crates/host/src/lib.rs::host_ctx_next`（同步错误分支）；`crates/host/tests/abi_m2b.rs`（取消/注销） |
| `ctx_stream_push` / `close` / `fail` | 窗口满 / 流已消失 / 失败 | 状态码 + 文本/`ErrorPayload`（能力位为 1 时结构化） | 流控 / 生命周期 | 由状态码表达，不套用本表 | `crates/host/tests/abi_stream.rs:359`、`:462`、`:515`、`:555` |
| 事件派发 `emit` / `dispatch` / `waterfall` | 监听器返回错误 | **无错误通道**：记日志并当作“没有意见” | 业务错误被吞 | 不适用 | `crates/host/src/lib.rs::host_ctx_on`（监听器失败记日志）；`crates/host/tests/sdk_callback_error.rs::structured_callback_error_is_readable_through_apply_via`（注册 listener、`emit`、断言日志的专门测试） |
| `App::plugin` | 校验 / 装载失败 | `Error` / 报告行 | 解析与依赖 | **不适用**（生命周期操作；校验/apply 都可能运行插件代码，不能套用调用认知） | `crates/core/tests/kernel.rs`（`ValidationError`）；`crates/host/tests/loader_release.rs` |
| `Fiber::dispose` | 清理失败 | `cleanup_error()`，无返回错误 | 生命周期 | 不适用 | `crates/core/tests/lifecycle_races.rs:502`；`crates/core/src/fiber.rs:495` |
| `probe_capabilities` | 文件 / ABI 不符 | `Error` 文本 | 协议 / ABI | 不适用（没有业务方法调用） | `crates/host/tests/probe_capabilities.rs:67`（成功）；`crates/host/tests/frozen_plugin.rs`（ABI 拒绝经 `LoadedPlugin::open`；probe 自身错误路径未单测） |

事件派发那一行是本次盘点最值得注意的：监听器失败没有调用方可见的通道，
只能看日志。这不是本 ADR 要改的（内核 handler 签名不可失败），但它必须被记录，
否则“每条失败路径都能映射”就是假的。

> 表中 `Unknown` 是**语义分类**，不等于结构化字段已经到调用者。事件派发没有
> 返回协议（已知例外）；`abi_m2_timeout` 只验证清理与日志措辞。阶段 2 已为
> iface / dispatch / next 与 iface 卸载通道补上端到端断言；不要为勾选项给事件
> 接口加协议。

## 8. 后果

- 文档中固定两句话：**失败不代表没有副作用；取消不代表已撤销副作用。**
- `crates/host/tests/abi_m2_timeout.rs::dispatch_policy_timeout_settles_a_never_fulfilled_continuation_as_timeout`
  已改名；测试内注释、断言消息改为“可读超时原因，不能写成业务失败”，且断言检查原因文本。
  本测试**不证明**调用者收到/识别结构化 `Unknown`；事件路径无返回协议，这是
  阶段 2 已知例外，不通过给事件接口加协议来消除。行为不改。
- 本 ADR 不引入任何类型。`NotExecuted` / `Reported` / `Unknown` 当前由
  ADR 0004 的开放字符串承载，不落 Rust 枚举；双向兼容矩阵见 ADR 0004 §3。
