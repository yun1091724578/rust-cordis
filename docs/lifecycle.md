# 生命周期与关闭契约

公开入口：

```rust
app.close().await;
```

`close(self)` 消费 `App`，避免关闭后继续挂载。外部若仍持有 `Ctx`/`Store` 克隆，只保证这棵 root 树拥有的资源无残留。

## 关闭顺序

1. `root_fiber().dispose().await`：递归处置子 fiber，按 LIFO 释放 effect。
2. host 的 cleanup effect 置 `closed`，等待在飞读锁退出。
3. 排空 dispatch 记录：
   - 先取消宿主创建的后台任务（`ctx_call_iface` / `ctx_dispatch` / `ctx_next` /
     纯 `emit` 的异步兜底），并等待任务真正结束；
   - 未收尾的流以错误结束；
   - 挂起的入站续延以错误结算；
   - 出站续延送达 abort。
4. 成功 apply 过的实例收到 `OP_DISPOSE`。
5. 注销实例与派生句柄。
6. 清空根级 hook（`RootKeepalive` 的同步兜底仍然保留）。

## 退出路径

| 路径 | 触发 |
|---|---|
| 正常完成 | `STATUS_OK` |
| 取消 | `CancelToken` / `ctx_abort` |
| 超时 | `DispatchPolicy.timeout_ms` / `DispatchPolicy.outbound_timeout_ms` |
| 调用方放弃 | 调用 Future 被丢弃 |
| 初始化失败 | `OP_APPLY` 非 `OK` 或 panic |
| 应用关闭 | `App::close` |
| 插件自卸载 | `ctx.dispose_self` |
| 消费者离场 | 流 Future 被丢弃 |
| HMR | 旧实例卸载后新实例挂载 |

`DispatchPolicy.outbound_timeout_ms` 覆盖 `ctx_call_iface`、`ctx_dispatch` /
`ctx_emit` 和 `ctx_next` 的剩余链。前三者超时后撤销自己的派发记录并以
`{ "error": ... }` 交付；`ctx_next` 超时同样交付可读错误，但**不会**替插件
`fulfill`——插件可以选择兜底结算或让这次派发继续挂起，直到取消/卸载排水。

## 进程信号

`cordis run` 安装最小的平台处理器：

- Windows：`SetConsoleCtrlHandler` 捕获 Ctrl-C、Ctrl-Break、控制台关闭；
- Windows 服务：`StartServiceCtrlDispatcherW` + `RegisterServiceCtrlHandlerExW` 捕获 `SERVICE_CONTROL_STOP` / `SERVICE_CONTROL_SHUTDOWN`；
- Unix：捕获 `SIGINT` / `SIGTERM`；
- 同时保留 Enter（stdin 一行）作为显式关闭方式。

处理器只置一个原子 flag；真正的排水仍由异步路径调用 `App::close`。
Windows 的关闭/注销事件处理器会等待清理完成（最多 4 秒），避免系统在排水中途结束进程。
服务模式下收到 `SERVICE_CONTROL_STOP` 后先报告 `SERVICE_STOP_PENDING`，清理完成再报告 `SERVICE_STOPPED`。

回归测试：
- `crates/cli/tests/shutdown.rs`：处理器安装幂等、关闭请求能唤醒 `wait()`。
- `crates/cli/tests/signal_unix.rs`：Linux/macOS CI 中向独立子进程发 `SIGTERM`，断言以 0 退出。
- `crates/cli/tests/service_windows.rs`：未由 SCM 启动时 `service` 快速失败并返回 4。
- `crates/cli/src/service.rs` 单测：`SERVICE_STOPPED` 携带 service-specific exit code，`SERVICE_RUNNING` 接受 stop/shutdown。

## 迁移同步保证

- **关闭开始后不再接受遗漏记账的 effect**：effect 登记走 `effect_gate`；卸载先关闸门，再以异步通知等待所有已经通过闸门的 setup 返回，不阻塞 async worker。
- **所有关闭调用者等待同一次清理完成**：`dispose()` 首次调用在第一个 poll 内同步启动
  一条共享清理任务；重复调用等待同一个 `watch` 完成信号。调用者的 Future 被 timeout
  丢弃不会取消清理，即使丢弃发生在 `close_effects()` 等待在途 setup 的窗口内。
- **非祖先消费者拆除完成前，提供者资源保持可用**：`unload()` 先通知并等待直接、非祖先
  依赖者拆完——已进入 `dispose()` 的消费者按完成信号等待，其余按 `settle()` 等待——
  然后才执行自己的恢复栈。
- **恢复栈归 fiber 所有**：`dispose_effects()` 每次只从 fiber 的列表中 `pop` 一个，未执行的 disposer 不会随调用者 Future 一起丢弃。手动执行返回的 `Disposable` 会**同步摘除**对应登记；子 fiber 清理完成时也会摘除父级那条“递归 dispose”登记——长期存活的 fiber 不会随临时 effect/子 fiber 数量线性累积空槽。
- **宿主出站任务属于关闭流程**：异步兜底任务的控制柄（取消令牌 + 完成信号）登记在派发记录上；卸载时先取消、再等待任务结束，然后才投递 `OP_DISPOSE` / 注销实例。只撤记录或只送 abort 不算关闭完成。取消路径（`ctx_abort` / 超时）在**派发表锁内**完成“摘记录 + 把完成信号放入实例级待回收集合”，关闭取快照时也持同一把锁——因此不会出现“两张表都空、任务却仍活着”的窗口；插件回调仍在锁外执行。
- **原生流任务属于调用点 fiber**：`call_iface_stream` 的异步提供者任务登记在 fiber 的原生任务表中；丢弃 `AsyncStream` 会发取消，fiber 卸载 / `App::close` 会先取消再等待任务结束。任务启动前先检查已发生的取消，等待中的 `select!` 使用 `biased` 让取消分支固定优先——取消与结果同时就绪时不会先跑业务 future。任务完成时自行摘除登记，因此不会像一次性 effect 那样在长生命周期 fiber 上线性累积空槽。
- **流收尾与块提交共用线性化点**：`AsyncStream::next` 的“检查失败/EOF + 取块”与 `StreamSink::send` 的“复查收尾 + 提交块”都在 `StreamState.life` 锁内完成。失败后不再交付残留缓冲；消费者读到 EOF 后也不会再出现新块。
- **提交视图保留域身份**：`require()` 按 `(name, realm)` 读 `committed`，与 `get()` 的隔离域语义一致；同名服务在默认域与派生域各有一份时不会串线，拆除期读旧依赖的保证不变。
- **清理异常可见**：`settle` 与监督任务截住内部 panic，尽力兜底排水，并通过 `Fiber::cleanup_error()` 暴露诊断。
- **关闭后不再接受挂载**：`Ctx::try_plugin` 对 disposed/unloading 上下文返回可读错误；`Ctx::plugin` 保留 panic 入口。若检查后父 fiber 恰好进入 unloading，返回的 fiber 以 `FAILED` 收敛，调用方可在 `settle()` 后读取错误。

## 并发 update

`Fiber::update_with` 允许并发调用，但提交与状态收敛按 `update_rev` 排序：

- 解析可以并发/乱序完成；只有 `update_rev` 更大的那次能提交 `config`。
- 被更晚 update 超越的调用返回 `UpdateOutcome::Superseded`，不派发 handler。
- 已经提交、handler 仍在运行的旧 update 不会被强行取消；等所有在飞 chain
  结束后，对**最新** `resolved` 配置 replay 一次 `internal/update`。
- replay 不递增 revision、不重启、不落盘；它只修正 handler 写入的状态。
- `Fiber::when_updates_settled().await` 等待 active chain 与 replay 全部结束。

回归测试：`crates/core/tests/update_races.rs`。

## 配置回写

- `config` / `disabled` 按**字段级 patch 归属**回写：主配置行被 patch 覆盖时，
  运行期修改写到最后一个实际覆盖该字段的 patch 层，不会写回主配置后被覆盖。
- 写入采用同目录临时文件 + 原子替换；Windows 使用
  `MoveFileExW(REPLACE_EXISTING)`，不再有“删目标再 rename”的缺失窗口。
- 内容相同不写，避免无意义的 mtime 变化触发其他进程的 HMR。
- `append_atomically` 全程持有同目录 lock 文件，避免并发追加丢记录；进程崩溃
  留下的锁在 30 秒后视为陈旧。
- **不覆盖尚未载入的外部编辑**：主配置整份回写前，先比较磁盘内容与“最近一次成功
  解析”的基线；两者不同就拒绝写回并保留外部编辑，等待下一轮重载。磁盘内容比较与
  替换在同一把配置锁内完成（编辑器仍可能在本进程锁外写入，因此这是尽力而为，
  但消除了“编辑已落盘、watcher 还没跑到”的确定性覆盖窗口）。
- **对账单飞位可取消**：`request_reconcile` 的 `reconciling` 由 RAII guard 持有；
  调用 future 被 timeout/abort 丢弃时复位，`pending` 留给下一次请求驱动，不会
  让后续请求永久返回 `Coalesced`。
- **暂存副本路径按装载器隔离**：文件名在 `.r<revision>` 前带有进程号 + 全局序号；
  两个装载器首次加载同一 DLL（都为 revision 1）不会争用同一个 `.r1.dll`。
  清理扫描仍按末尾 `.r<digits>` 识别。
- **SDK 回调回收不在锁内析构**：`forget` 先把各表里的值取出到局部变量，所有表锁
  释放后才 drop；回调捕获对象的 `Drop` 即使重入注册/注销也不会在同一张表上死锁。

回归测试：`crates/host/tests/loader_reconcile.rs` 的两个 `*_owner_layer` 用例、
`cancelled_reconcile_does_not_stick_the_single_flight_flag`、
`runtime_writeback_refuses_to_overwrite_an_unloaded_external_edit`，以及
`crates/host/tests/staged.rs` 的 `same_revision_staging_uses_distinct_paths`；
`crates/host/src/loader.rs` 的 `concurrent_append_*` / `identical_content_*`；
`crates/abi/src/sdk.rs` 的 `callback_cleanup_drops_outside_registry_locks`。

## 支持范围

**祖先消费者是明确不支持的生命周期拓扑**：消费者位于提供者的 parent 链上，却
`inject` 了提供者提供的服务。提供者会跳过等待该祖先，避免父子 driver 互等；
因此提供者资源可能早于祖先剩余的恢复栈关闭。运行时会记录
`生命周期支持范围外：…` 的 `Warn` 日志。运行期新增依赖若会形成服务环，则直接
拒绝该依赖并记录 `Error`，消费者保持未满足。

完整矩阵、运行时行为和规避方式见 [`support.md`](support.md)。

## 不变量

每条退出路径都必须满足：

- `loaded_instances()` 不增加；
- `pending_len(handle)`、`dispatches_len(handle)`、`streams_len(handle)` 归零；
- 插件注册的宿主 hook 被摘除；
- SDK Registry 不再强持回调捕获值；
- volatile 副本不残留；
- 暂存副本被认领并删除。

回归测试：

- `crates/host/tests/lifecycle_close.rs`：`App::close` 排空挂起派发并注销实例。
- `crates/host/tests/abi_m2_drop.rs`：调用方丢弃 Future。
- `crates/host/tests/abi_stream_timeout.rs`：流式超时。
- `crates/host/tests/abi_outbound_timeout.rs`：`ctx_call_iface` 出站超时与记录撤销。
- `crates/host/tests/abi_outbound_dispatch_timeout.rs`：`ctx_dispatch` 出站超时与记录撤销。
- `crates/host/tests/abi_next_timeout.rs`：`ctx_next` 剩余链超时、错误交付与插件兜底结算。
- `crates/host/tests/sdk_apply_failure.rs`：初始化失败回收 SDK 回调。
- `crates/abi/src/sdk.rs` 单测：宿主拒绝 `ctx.on` / `on_async` / `provide_iface` / `effect` 时撤销已登记回调。
- `crates/core/tests/kernel.rs`：根树递归处置与 `App::close`。
- `crates/core/tests/lifecycle_races.rs`：关闭竞态、祖先消费者告警、清理 panic 兜底、disposed ctx 挂载拒绝。
- `crates/core/tests/stream.rs`：原生流任务随 `App::close` / 丢弃流取消并等待；取消与结果同时就绪时取消优先；失败优先于残留缓冲；EOF 之后不再提交新块。
- `crates/cli/tests/shutdown.rs`：stdin EOF 不触发关闭，进程继续等待系统信号。
- `crates/host/tests/patch.rs`：非法 `disabled` / `isolate`（含 patch 层）返回字段级错误。
- `crates/cli/tests/inspect.rs`：`inspect --json` 复用 `parse_inject`，区分 required / optional。
