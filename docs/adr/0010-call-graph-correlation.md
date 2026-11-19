# ADR 0010：调用链关联（call graph）

- **状态**：已接受（2026-11-18；设计决策，实现与 WP1 一并落地）
- **日期**：2026-11-18
- **依据**：`docs/adr/0009-rsi-deployment-substrate.md`（§2.2 记录字段、§6 验收）、
  `docs/adr/0005-trust-model-and-plugin-source-boundary.md`；
  `crates/host/src/lib.rs` 的 `call_inbound` / `settle` / `host_ctx_call_iface` /
  `deliver_outbound` / `scope_push`；`crates/core/src/context.rs` 的 `call_iface`
- **决定摘要**：新增一个**只由 iface 跳维护**的调用作用域（host 私有
  `CALL_SCOPE`），在 iface 入站调用的执行窗口与续延交付窗口压栈；
  `Outbound` 拆出独立的 `trace_parent`（不复用取消树的 `parent`），
  `PluginIface::call` 用它作为子跳的 `parent_call_id`。不改 ABI、不改
  `CtxCallIfaceFn` 形参、不新增 `HostApi` 字段。

---

## 1. 背景：现有代码里“父子”差在哪

现有实现已经给每次入站调用分配 `dispatch_id`（`DISPATCH_SEQ`，与出站
`iface_id`、内部 `dispatch_impl` 共用一个计数器），`DispatchRecord.parent`
也已经是 `Option<u64>`。缺口有四个，且都是**取值时机与来源**问题：

- `scope_top()` 是 `thread_local` 的 `DISPATCH_SCOPE`，只在同步执行窗口内
  有效；`PluginIface::call` 在入口读一次作为 `parent`，插件挂起（PENDING）
  并在 `deliver_outbound` 里恢复时**作用域已弹出**，续延里发起的下一跳父为空；
- `call_inbound` 不是 iface 专用：事件监听（`:3728`）与流（`:3242`）也走它。
  在它内部无条件压栈会把事件 dispatch id 当成父，而事件 id 没有 trace 记录
  ——正是“父指向幽灵节点”；
- `DispatchRecord.parent` 一身二用：它同时是取消树的父（`drain_dispatch_tree`
  `:1093`、`ctx_abort` 级联）。直接改它的取值来源会静默改变取消语义；
- `deliver_outbound` 拿到的 `dispatch_id` 是**出站记账 id**（`:3388` 的
  `iface_id`），不是被调方的入站 `call_id`；压它会让续延里发起的调用父
  指向一条不存在的记录。

## 2. 决定

### 2.1 语义：父 = 最近的、有记录的 iface 跳

`parent_call_id` 的定义是“发起这次调用时，正在执行的那条 **iface 跳**的
`call_id`”；若当前上下文不是 iface 跳（事件回调、waterfall、T1 的独立异步
任务），父为 `0`（根/未归因）。typed 直调不产生记录，不在此表范围内。

### 2.2 独立的 `CALL_SCOPE`，只由 iface 跳压栈

host 新增私有 `thread_local! CALL_SCOPE: RefCell<Vec<u64>>` 与
`call_scope_push(id) -> Guard`（RAII 弹出）。压栈点只有两处，且都能区分
“这次调用是否有 trace 记录”：

1. **iface 入站调用的执行窗口**：`call_inbound` 增加两个 host 内部参数
   （`trace` / `trace_parent`，见 §2.3）。**只有 iface 请求传 `trace=true`
   （`:3189`）**；流（`call_stream` `:3242`）、事件监听（`:3728`）、测试路径传
   `false`——流与事件都没有 trace 记录，压栈会把父指向幽灵 id。仅当 `trace`
   为真、且记录创建成功后，在现有 `scope_push`（`:1863`）旁边再压
   `call_scope_push(dispatch_id)`。守卫与现有作用域同块析构，不跨 `await`；
2. **续延交付窗口**：`deliver_outbound` 在 `table.remove` 之后、`drop`
   之前读出出站记录上的 `trace_parent`，`drop` 之后用
   `call_scope_push` 压栈；守卫覆盖 `call_plugin(OP_CALLBACK)` 整个调用。

`deliver_next`（`:3678`）与事件/waterfall 的 `Scoped` 只压
`DISPATCH_SCOPE`，不压 `CALL_SCOPE`。

**写端**：`host_ctx_call_iface` 创建 iface 出站记录时，在 `Outbound`
（`:166`）上写 `trace_parent = call_scope_top()`；`dispatch_impl`
（`:3865`/`:3873`，emit 也经它）的 `Outbound` 写 `None`，因为它们不是
iface 出站。没有这一句，`deliver_outbound` 读到的永远是 `None`，续延链
不成立。`DispatchRecord.parent` 保持原样（取消树）。

**第三个交付点**：`drain_dispatch_tree`（`:1142`）经
`deliver_abort_direct` 向插件投递取消/超时通知，这是终止路径，**不压**
`CALL_SCOPE`：终止回调里再发起的调用父为 0，不承诺归属。

### 2.3 拆出 `trace_parent`，取消父保持不变

`Outbound`（host 私有，`:166`）增加一个字段 `trace_parent: Option<u64>`：

- `DispatchRecord.parent` 继续由 `scope_top()` 计算，只服务取消级联；
- `Outbound.trace_parent` 由 `call_scope_top()` 计算，只服务 trace；
- `call_inbound` 的 `parent` 形参仍是取消父；另加两个 host 内部形参：
  `trace: bool`（是否写记录）与 `trace_parent: Option<u64>`（trace 父），
  一起写进记录；
- `PluginIface::call` 在调用入口同时取两者（同一线程同一时刻，与
  `host_ctx_call_iface` 的取值一致），分别传给 `call_inbound`。

这样取消语义零变化，父链也不会被事件 id 污染。`Outbound` 的构造点
（`:3397`、`:3877`）都要补 `trace_parent`；漏改编译会报 E0063，不会静默
写 `None`。

### 2.4 记录的 `call_id` 与 PENDING 携带

`TraceRecord.call_id` 就是该跳的入站 `dispatch_id`。为了让 PENDING 路径在
`settle` 时仍能写出与发起时一致的事实，`PendingContinuation`（host 私有，
`:1679`，不跨 FFI）携带：`trace_parent`、`traced`、callee 快照
`(uid, name)`、`caller_uid`、`name`、`started`。**不重复存 `call_id`**
——它已有 `dispatch_id`。`traced` 必须单独存在，因为
`trace_parent=None` 既可能是“根跳”（要写记录，父为 0），也可能是
“未追踪的跳”（事件/流，不写记录）；只看 `Option` 区分不了。
callee/发起方快照都必须在这里做：`settle` 可能晚于 provider 卸载
（fiber uid 被置为 `UID_DISPOSED`，`fiber.rs:1895`），host 侧没有其它稳定
读口。`settle` 的解构同步更新，只在 `traced` 为真时写记录；同步路径在
`call_inbound` 出口直接用现场身份写记录。

超时与出站 future 的 poll 边界 panic（ADR 0009 §2.2 的第 3/4 类写入点）
都没有入站记录：`call_id=0`，`parent_call_id` 取 `host_ctx_call_iface` 当时的
`call_scope_top()`（没有则为 0）。两类共用 `IfaceTrace` 快照，且与入站结算
写入互斥（同一次 poll 内入站写入必然返回 `Ready`）。

### 2.5 不改 ABI、不改公开形参

- `CtxCallIfaceFn` 的形参已随 `ABI_VERSION=1` 冻结，不加 id 参数；
- 不加 `HostApi` 尾字段：父链完全由宿主在两侧压栈推出，插件无需上报；
- `PluginIface::call` 的签名不变；新增参数只出现在 host 私有函数
  `call_inbound` 上；
- `DispatchRecord` / `PendingContinuation` / `Inbound` 均 host 私有、
  无 serde、不进 `crates/abi`，字段变更不构成 ABI 或公开面变化。

### 2.6 链路能覆盖到哪

| 场景 | 父链 | 说明 |
|---|---|---|
| T2 → T2，同步 | 完整 | iface 入站窗口 |
| T2 → T2，PENDING 后在续延里再发起 | 完整 | `deliver_outbound` 恢复的是**调用方**那一跳（本 ADR 的核心修复） |
| T2 → T1，T1 在同步段里再调 T2 | 完整（跳过 T1） | T1 那一跳没有记录，C 的父是 A |
| T1 挂起后在自己的任务里再发起 | 父为 0 | 提供者侧断链；`thread_local` 不跨 `await` |
| T1 自己的异步任务发起 | 父为 0 | 这是根 |
| T1 在宿主回调（on_done/waterfall）里发起 | 父为 0 | 阶段 1 只在 iface 窗口压 `CALL_SCOPE` |
| 事件回调里发起 | 父为 0（根 emit）；跳内同步 emit 时父=发出事件的跳；异步派发为 0 | 事件派发没有记录，事件入站 `trace=false`；跳内同步执行会继承当前 `CALL_SCOPE`（§6） |
| typed 直调 / `call_iface_stream` | 无记录 / 不覆盖 | 既有盲区（ADR 0009 §2.2） |
| 插件自建线程里回调 `call_iface` | 父为 0 | 线程局部不跨线程；与 `DISPATCH_SCOPE` 的既有约束一致 |

### 2.7 记录保留与查询

`DispatchRecord` 在完成时即删除（`:1868`/`:1896`/`:3507`/`:4026`），
父子关系只存在于 `TraceRecord` 流里。**不新增保留表**：消费方按
`call_id`/`parent_call_id` 重建树，叶子 Err 即“自身失败”的候选。
同一 `call_id` 至多一条记录（ADR 0009 §2.2 的“一跳一条”）。

## 3. 不承诺

- **不承诺跨进程/跨重启**：`DISPATCH_SEQ` 是进程内计数器；trace 文件跨进程
  拼接必须带进程标识（由消费方负责）。
- **不承诺事件与 waterfall 的因果**：它们不进 `CALL_SCOPE`，父为 0。
- **不承诺 T1 异步起点、T1 Pending、自建线程的父链**：见 §2.6。
- **不承诺插件自报**：T2 插件无法经 ABI 写记录；T1 插件可调用公开 API
  （含 `Store::record_trace`），这是受信域的已知例外（ADR 0009 §2.2）。

## 4. 与既有 ADR 的关系

- **ADR 0009**：本 ADR 补 `parent_call_id` 的取值与压栈位置；
  写入点、结果码、盲区不变。`TraceRecord` 的两个 id 字段已由 0009 定义，
  **本 ADR 不新增字段、零公开面变化**；实现时仍按 0009 §4 登记
  `api-inventory`（Type 级）。
- **ADR 0005 §2.3**：不改变准入或签发口径。
- **ADR 0006**：不新增公开类型或方法；`TraceRecord` 字段变化按“待验证”
  处理，不构成 0.2 迁移。
- **ADR 0007**：`call_id` 是关联标识，不承担版本或兼容语义。

## 5. 后果

**正面**

- A→B→C 的失败可归到叶子跳：B 透传时，B 的记录是 C 的父，按“叶子 Err”
  聚合不会把失败同时记到 B 和 C；
- 取消树的 `parent` 与 trace 的 `trace_parent` 分离，取消级联行为零变化；
- 事件不污染父链：由其发起的调用父为 0（事件派发本身没有记录），
  避免“父指向幽灵节点”；`revoke_dispatch` 早退路径不写记录，子跳的父仍可能
  指向无记录 id，此边界见 ADR 0009 §2.2。

**负面**

- `thread_local` 语义决定“只有同步执行窗口内的调用才有父”：
  T1 异步起点、T1 挂起后的提供者侧、自建线程永久为 0；
- `call_inbound` 多两个内部形参（`trace` / `trace_parent`）、
  `Outbound` 多一个字段（`trace_parent`）、`PendingContinuation` 多几个记录
  快照字段，需要回归所有 PENDING 路径（既有 5 个结算入口）；
- 跨热换的代际对齐仍靠消费方切窗（ADR 0009 §2.3）。

## 6. 验收与证据

- 同步链：A→B→C 中 C 返回 Err，三条记录的 `parent_call_id` 依次指向上一跳；
  按叶子 Err 聚合只记到 C；
- 续延链：B 的调用 PENDING，`OP_CALLBACK` 交付后 B 再调 C，C 的父是 B；
  未在 `deliver_outbound` 压 `CALL_SCOPE` 时该用例红；
- 事件不污染：从**根上下文** emit 事件，回调里发起 A→B，A 的父为 0、
  B 的父为 A。若 emit 本身发生在某条 iface 跳内，事件**不压** `CALL_SCOPE`，
  但同步执行的监听器会继承当前栈顶（父=发出事件的跳）；异步派发或根 emit
  时父为 0。测试必须固定起点，避免歧义；
- T1 提供者：T2→T1→T2 中 C 的父是 A（T1 同步段内发起）；T1 挂起后在自己的
  任务里再发起时父为 0（提供者侧断链，`abi_trace_t1_suspend` 单独断言，
  不写成保证）；
- T1 起点：T1 任务里直接发起 A→B，A 的父为 0；
- 取消父不变：`DispatchRecord.parent` 仍取 `scope_top()`；既有取消级联测试
  （`crates/host/tests/abi_m2c.rs:366` 一带的父派发级联用例）必须保持绿，
  `trace_parent` 的引入不得改变这条路径；
- 一跳一条：PENDING 结算与超时/取消路径都只产一条，且 `call_id` 一致；
- 失败路径：解析失败记录 `call_id=0`、父为 0；出站看门狗单独超时记录
  `call_id=0`、父为发起时的 `call_scope_top()`。

## 7. 复评触发条件

- 需要事件/waterfall 级别的因果；
- 需要 T1 异步/Pending 起点的父链（需把作用域从 thread-local 改为随
  future 传播，会触碰 core 的调用路径）；
- 需要跨进程或跨重启的链路拼接；
- 插件需要在 ABI 上自报调用关系（与“宿主写记录”冲突）。
