# 生命周期支持矩阵

本文是 `rust-cordis-core` 0.1 的生命周期**支持矩阵**。它区分“保证成立”的场景和
“明确不支持、只能尽力收敛”的场景。`docs/lifecycle.md` 描述实现与关闭顺序；
本文件描述使用者在生命周期上可以依赖什么。总览、非目标与保证索引见
[`support-contract.md`](support-contract.md)。

本表的“支持 / 不支持”是行为描述，**不直接等同于**支持契约的四类状态
（保证 / 声明 / 待验证 / 非目标）；逐行迁移到保证索引时再对齐。

## 支持矩阵

| 场景 | 状态 | 可依赖的行为 |
|---|---|---|
| 树形所有权（父 fiber 拥有子 fiber） | 支持 | 父卸载递归处置子 fiber；恢复栈按 LIFO 执行 |
| 非祖先服务消费者（消费者不在提供者子树中） | 支持 | 提供者先等直接消费者拆完（含已开始 dispose 的消费者），再执行自己的恢复栈 |
| 重复 `dispose()` / 多个关闭调用者 | 支持 | 所有调用者等待同一次清理完成 |
| 调用者丢弃 `dispose()` Future | 支持 | 清理任务在第一次 poll 内即启动；即使在 effect 闸门等待期丢弃，后续 dispose 仍能完成 |
| 同步 effect setup 与卸载交错 | 支持 | 关闭闸门后不再接受新 effect；等待已通过闸门的 setup 返回 |
| 手动释放 effect / 子 fiber 处置 | 支持 | 恢复栈同步摘除对应登记；不随运行时长累积空槽 |
| 宿主出站任务（iface / dispatch / next / emit） | 支持 | 卸载先取消任务并等待其结束，再 `OP_DISPOSE` 与注销实例；取消路径在同一把派发表锁内完成摘记录 + 待回收登记，close 取快照持同一把锁 |
| 原生流任务（`call_iface_stream` 的异步提供者） | 支持 | 丢弃流即取消；fiber 卸载 / `App::close` 先取消并等待任务结束；取消与结果同时就绪时取消固定优先；任务完成自行摘除 |
| 流结束语义（close / fail / 丢弃） | 支持 | 失败后不交付残留缓冲；EOF 之后不再接受新块；状态检查与提交共用同一把锁 |
| 非法 `disabled` / `isolate` 声明 | **拒绝** | 解析期返回字段级错误（`disabled` 必须是布尔或 `!!js`；`isolate` 必须是 `true|标签字符串` 映射），绝不静默落成默认行为 |
| 无终端运行（stdin EOF） | 支持 | EOF / stdin 不可用不触发关闭；只把有效输入行当作交互关闭，继续等待 Ctrl-C / SIGTERM |
| 对账 future 被取消 / timeout | 支持 | 单飞位由 RAII guard 复位；`pending` 留给下一次请求驱动，不再永久返回 `Coalesced` |
| 运行时回写遇到未载入的外部编辑 | 支持 | 写前比较主配置基线；不同则拒绝写回并保留外部编辑，等待重载 |
| 两个装载器首次加载同一 DLL | 支持 | 暂存路径含进程号 + 全局序号，不共享同一个 `.r1` 副本 |
| SDK 回调回收与析构重入 | 支持 | `forget` 先摘表、释放全部表锁，再 drop 回调；析构中重入注册/注销不会死锁 |
| 多隔离域同名服务 | 支持 | `get()` / `require()` 都按 `(name, realm)` 解析；提交视图保留域身份 |
| 作用域父键并发绑定 | 支持 | 拓扑检查与写入串行化；相反方向只能成功一个，遍历带环防御 |
| 单个 disposer panic | 支持 | 该 disposer 记日志后继续执行剩余恢复栈 |
| 祖先消费者（消费者是提供者的祖先） | **不支持** | 提供者跳过等待，可能先关闭资源；必须留下告警 |
| 服务依赖环（提供者/消费者互相依赖） | **拒绝成环** | 建边时检测可达性，拒绝会成环的依赖并报错；消费者保持未满足 |
| entry-level `inject: {required, optional}` | 支持 | required 照常门控；optional 不阻塞装载、不触发自动重载 |
| entry-level `inject` / `intercept` 的 `!!js` | **不支持** | 解析期 fail loud，错误带字段路径；目前只有 `config` / `disabled` 支持求值 |
| 生命周期内部清理任务 panic | **尽力保证** | 记录 `cleanup_error`，尝试兜底排水；不是事务性回滚 |

## 祖先消费者

### 定义

fiber `A` 是提供者 `P` 的祖先（`A` 在 `P` 的 parent 链上），同时 `A` 声明
`inject` 了 `P` 提供的服务。这通常只会在手工用祖先的 `Ctx` 挂载后代时出现；
装载器的正常树形装载不会生成这种形状。

### 为什么不在支持范围内

如果 `P` 等待 `A` 完成拆除，会形成等待环：

```text
P.unload  -> 等待 A.settle
A.settle  -> 持有自己的 driver，卸载子 fiber P
P.dispose -> 等待 A 走完
```

`A` 的 driver 已经被它自己的卸载路径持有，`P` 再等 `A` 就是永久等待。因此
`wait_for_dependents()` 和 `provide` 的 disposer 都会跳过祖先消费者，避免死锁。

### 实际行为

- 卸载不会因为该拓扑挂死；
- 提供者可能在祖先消费者剩余的 disposer 之前关闭资源；
- 运行时以 `Level::Warn` 记录一次明确诊断，消息以
  `生命周期支持范围外：提供者 <…> 的消费者 <…> 是它的祖先` 开头；
- `crates/core/tests/lifecycle_races.rs` 中的
  `ancestor_consumer_is_outside_supported_scope_and_logged` 锁定该行为。

### 规避方式

- 不要让祖先 `inject` 后代提供的服务；
- 把消费者移到提供者的兄弟/后代分支；
- 或引入第三方提供者，使依赖边不再沿 parent 链反向。

## 服务依赖环

声明级的环可由装载器报告（`strict_cycles` 可把环升级为装载失败）。

运行期新建依赖边时，`Fiber::insert_dependency` 会在全局依赖图锁内检查
“提供者是否已经可达消费者”。会成环的边被拒绝：

- 新依赖不会写入 `deps`；
- 消费者保持未满足，并在收敛时退回 `Pending`；
- 该服务名进入 `cycle_blocked`，卸载等待图也跳过这条边，避免“拒绝建边但等待图仍成环”；
- 运行时记录 `Level::Error` 诊断，消息以
  `检测到运行时服务依赖环：…` 开头。

回归测试：`crates/core/tests/runtime_cycles.rs` 覆盖两个已激活 fiber 在运行期新增
inject 后成环的场景，断言依赖被拒绝、诊断可定位、双方卸载不死锁。

## entry-level inject 作用域

行级 `inject` 支持三种形状：

```yaml
inject: [tools, sessions]              # 必需依赖
inject: { tools: { scope: a } }        # 必需依赖 + intercept 配置
inject:
  required: [tools]                    # 门控装载
  optional: [telemetry]                # 不门控
```

语义：

- `required` 与数组/平铺对象一样进入 fiber 的必需依赖集，服务未就绪时停在
  `PENDING`；平铺对象里的配置会覆盖插件自带配置。
- `optional` 在 fiber 开始收敛**之前**登记，不参与激活判定。服务存在时
  `ctx.require` / `ctx.get` 都能读到；缺席时 `ctx.require` 报
  `cannot get required service "…" in inactive context`（已声明但未就绪），
  而不是 `without inject`。
- 可选服务的出现/消失**不会**触发消费者自动重载；它是一条软依赖，不是
  声明级依赖环的边。
- `inject` / `intercept` 中出现 `!!js` 仍在**解析期** fail loud：

```text
entry-level inject 的 !!js 尚未支持（at inject.settings.namespace: …）；
目前只有 config 与 disabled 支持求值
```

回归测试：`crates/core/tests/optional_inject.rs`、
`crates/host/tests/entry_inject_scope.rs`、
`crates/host/tests/loader_capabilities.rs`，以及
`crates/host/tests/patch.rs` 的 `entry_inject_*` 用例。

## 清理任务 panic

- 用户 disposer 的 panic 在独立任务中隔离，其余 disposer 继续执行。
- 清理路径内部（包括日志通道、用户 `Check` 谓词等）发生 panic 时，
  `settle`/监督任务会截住它，尽力关闭闸门、排空恢复栈，并走到 `Disposed`
  （卸载期）或 `Failed`（重载期）。
- 诊断通过 `Fiber::cleanup_error()` 暴露，同时写入日志。
- 这不是事务性保证：如果 panic 已破坏锁或其他进程内状态，兜底排水也只能尽力而为。

## 同步 effect setup 契约

`ctx.effect` 的 setup 仍是同步函数。关闭流程等待它返回时使用异步通知，不会阻塞
async worker；但 setup 本身在调用线程上执行。**不要在 setup 里做长时间阻塞操作**，
否则关闭仍会被这段用户代码延迟。

## 相关回归测试

- `crates/core/tests/lifecycle_races.rs`：提供者/消费者顺序、effect 闸门、丢弃
  Future、重复 dispose、`App::close`、祖先消费者告警、清理 panic 兜底。
- `crates/core/tests/committed_view.rs`：拆除期读依赖、自己提供的服务、每次激活
  重拍、**提交视图按隔离域读取**。
- `crates/core/tests/hook_scope.rs`：作用域身份/父键查询、**并发反向绑定不成环**。
- `crates/core/tests/weak_audit.rs`：Weak 环审计、**手动释放 effect 与处置子 fiber
  后恢复栈登记立即回收**。
- `crates/host/tests/abi_outbound_close.rs`：**关闭取消并等待宿主出站任务**。
- `crates/host/tests/abi_outbound_abort_close.rs`：**先 abort 再 close 仍等待任务结束**。
- `crates/host/tests/abi_outbound_cancel_close_race.rs`：**取消回调进行中并发 close，不得提前返回**。
- `crates/core/tests/runtime_cycles.rs`：运行期服务环拒绝、诊断与卸载无死锁。
- `crates/core/tests/kernel.rs`：递归处置与根 fiber 关闭。
- `crates/host/tests/lifecycle_close.rs`：宿主派发排空与实例注销。
