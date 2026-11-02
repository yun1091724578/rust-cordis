# ADR 0001：第二载体（T3 / 跨进程 / Wasm）

- **状态**：已决定（**不实现**，只登记决策与路径）
- **日期**：2026-10-16
- **依据**：《SDK方案.md》§4 M8、D1、D7、D8；《原理.md》§6.4、§8.5、§9.2；
  论文 §6.2（cross-process）、§6.3（沙箱）、§6.4（模块装卸三模型）
- **决定**：**骨架内不做第二载体**；本文把「为什么不做」「接口契约因此被钉成什么样」
  「将来要做时的落地路径」三件事写死，以免日后重新论证或走偏。

---

## 1. 背景：载体为什么是可替换的

论文 §6.4 把"加载"建模为**对 context 的一个 effect**，其逆撤销模块引入的符号注册；
`dlopen`/`dlclose` 只是这条语义的一种落点。本仓按同一口径切了三层：

| 层 | crate | 知道什么 | 不知道什么 |
|---|---|---|---|
| 内核 | `cordis-core` | 服务、依赖、epoch、effect、事件 | 动态库、JSON、文件、ABI |
| 载体 | `cordis-abi` | 单入口 + op 码 + `PluginDesc`/`HostApi` 内存布局 | 平台 API（**零平台依赖**：只 `serde_json` + `std::os::raw::c_char`） |
| 适配 | `cordis-host` | `dlopen`/`LoadLibrary`、YAML、HMR、暂存副本 | 内核内部（只经 `Plugin` trait 接） |

可替换性由四个**已经存在**的接缝保证（这也是本文的证据面）：

1. **`PluginSource`**（`crates/host/src/loader.rs`）：「一行配置 → 一个 `Arc<dyn Plugin>`」
   这条链唯一抽象。真实现 `DiskPluginSource`（解析 → 暂存 → 打开 → 探活）；
   测试用假源直接给 `Arc<dyn Plugin>`——**装载层语义（隔离域、intercept、label 稳定化、
   事务性换入、对账）已经在无动态库的情况下被端到端测试过**（`loader_reconcile.rs`、
   `loader_scope.rs`）。
2. **`HostApi` op 协议**（`crates/abi/src/lib.rs`）：宿主按 op 码调用插件
   （`OP_APPLY/DISPOSE/CALLBACK/CLEANUP/PING/VOLATILE_*`），插件按字段表回调宿主
   （22 个 `Option<fn>`）。**跨边界一律 JSON**、内存全由宿主持有——这正是搬到
   IPC/Wasm 所需的形状（无共享指针、无跨边界借用）。
3. **`probe_capabilities_in(path, search)`**：预挂载探测与真装载**共用** `PluginSource`，
   所以"审阅再决定装不装"这条链和载体无关。
4. **`LoadedPlugin` 是唯一适配器**：ABI ↔ 内核之间只有它一个 `impl Plugin`
   （反向是 `PluginIface`，把 `ctx.call_iface` 送回插件）。换载体 = 换掉这一个实现，
   内核与 `cordis-abi` 一行不动。

**版本门禁也是载体无关的**：`ABI_VERSION`（major 相等）+ `PluginDesc.size` 逐字段
`field_at` 探测（"恰好够 / 差一字节" 全表对照）+ `PLUGIN_DESC_PREFIX_SIZE` 冻结前缀 +
`attach` 的 copy-on-attach（`crates/host/tests/abi_matrix.rs` 自动渲染
`兼容矩阵.md`）。跨进程载体只需复用同一套判界规则。

---

## 2. 问题：第二载体要解决什么、以及它解决不了什么

**要解决的**：让插件跑到**另一个地址空间**（子进程 / Wasm 实例）里，从而
① 崩溃不带走宿主；② 可以用 OS/运行时级边界约束它。

**解决不了的（D1 的诚实边界，两条硬话）**：

1. **T3 不是信任边界。** SDK **不承诺沙箱**：真正的隔离需要语言之外的执行边界
   （OS 进程 + 权限、Wasm 运行时的能力配置），那是宿主产品的事，不是组合模型的
   一部分。论文 §8.5 说得同样直白：沙箱化不可信代码需要超出语言级的手段。
   ⇒ 文档与代码里都**不得**出现"装了 T3 就安全了"的暗示。
2. **模型自生成组件永远不得是 T2。** T2 与宿主同地址空间、且**完全信任**
   （`dlopen` 进来的代码就是进程的一部分）。模型产出的代码要么进 T3，要么不进。

**因此第二载体在骨架里的定位**：**"载体可换"的证明**，不是安全方案。
它存在的意义是让"内核不知道 ABI 存在"这句话可被证伪——一旦有人把它做出来，
内核里若需要改一行，就说明分层错位。

---

## 3. 决定之一：骨架内**不做**第二载体（含成本）

| 成本项 | 量级 | 说明 |
|---|---|---|
| op 协议 ×2（请求/应答 + 续延 + 取消 + 排水） | 大 | 现有 op 是"同地址空间 + 同步重入"的面；跨进程要把**每一次重入**变成异步往返 |
| 生命周期 | 中 | 进程死亡 = 插件消失，要映射成 `Fiber::FAILED` + 不重入（论文 §4.4 的失败分支） |
| 装载层 | 中 | 预检/暂存/指纹/HMR 都要按"远端文件"重写一遍 |
| 测试基建 | 大 | 现状靠 `CallFn` 假插件（`D9`）；跨进程需要一套与之等价的远程假插件 |
| 没有消费者 | — | 本仓的验收目标是 **harness 骨架**；harness 的插件（工具、模型适配器）今天都落在 T1/T2 |

**结论**：投入产出比不成立。**移出骨架**；D1 的两条硬话与 T3 的契约（下节）写进文档，
等真有沙箱需求时按 §5 的路径做。

---

## 4. 决定之二：T3 的接口契约**必须是异步的**（这一条现在就要守）

跨进程/跨 Wasm 的调用**不能同步重入**：宿主无法阻塞等一次远程往返，远端也无法在
宿主栈上回调。于是 T3 的接口契约被钉成：

1. **一次调用 = 一个 id + 一次（或多次）回调**；返回值经 `ctx_fulfill(id, value)` 送达。
   本仓已具备这条链：`STATUS_PENDING` + 挂起表 + `PendingContinuation::settle` +
   看门狗（`DispatchPolicy`）+ 排水（卸载先结算在飞续延）。
2. **提供者侧也要能挂起**。今天 `ctx.provide_iface` 注册的方法体是**同步**的
   （`IfaceFn: Fn(Json, Option<Json>) -> Result<Json, String>`），宿主侧虽已支持
   "接口方法挂起"（`PluginIface::call` 走的是 `pending.settle("接口方法")`），
   SDK 面**没有**暴露可挂起的接口方法形态。
   ⇒ **✅ 已补（本轮）**：`ctx.provide_iface_async` + `AsyncIfaceFn`
   （方法体拿到 `dispatch_id`；`Pending` 就挂起、`Reply` 就立即答复）。
   宿主侧**一行没改**——这正是"异步入站是载体层的能力，不是某个入口的特权"的证据。
   回归：`crates/host/tests/sdk_async_iface.rs`（挂起→结算、同步方法、立即答复、
   未知方法、版本协商、**卸载排水**、**16 KiB 结果上限**七条断言）。它同时是 §5 流式推广的第一块拼图。
3. **不引入同步阻塞原语**：任何"等远端"的新 op 都必须以"发起 + 续延"表达。
4. **取消要能穿过去**：`ctx_abort`/`CancelToken` 的语义（BFS 收集受害者、
   remove-then-settle、自然完成抑制）在远程载体上要能被映射成"发一条 abort 消息 +
   本地立刻结算"——**本地绝不等待远端确认**（否则一次网络抖动就把宿主钉住）。

---

## 5. 决定之三：D7 流式的推广路径（骨架内只允许 T1）

**问题**：harness 的 LLM 适配器返回 token 流；ABI v2 的异步入站是"**一次挂起 + 一次
结算**"，表达不了"多块"。

**已经跑通的原料**：`ctx_next`（M2b）反转了控制流——插件可以把"继续算"交回宿主，
并在宿主完成后被回调。流式 = 把这条链**推广成 N 次回调 + 背压窗口**：

```
T1 用（今天就能用）：async 迭代器，零 ABI 成本                        ← 骨架内的唯一答案
T2 侧（**✅ 已实现**，M8-后续②；落地形态与三处偏离见 5.1）：
  ① open-stream(id, meta)      → 返回流句柄（宿主侧一笔在飞记录）
  ② push-chunk(id, chunk)      → 满窗口时返回 PENDING + 一个回调 id（= 背压）
  ③ close(id) / abort(id)      → 正常收尾 / 取消（复用现有取消树）
  ④ 宿主侧 API：AsyncStream（`next()`  await / `credit()` 归还窗口）
  ⑤ 帧与流的生命周期同 `dispatch_id`：卸载/取消先排水（同 M2b 的四条清理路径）
```

### 5.1 落地形态与三处偏离（M8-后续②，2026-10-16）

- **不设 `open-stream`**：流就是那次派发本身——`call_iface_stream` 把流挂在既有的派发记录上
  （`DispatchRecord.stream`），因此复用 `dispatch_id`，少一个 id 空间与一次往返。
- **`stream_fail` 取代 ADR 的 "abort"**：与宿主→插件的 `ctx_abort`（取消）区分开，
  语义是"提供者宣告这条流失败"。
- **额度由 `next()` 隐式归还**（一名额一次 credit 回调），不引入 `credit()` API。
- 三件套 = `ctx_stream_push` / `ctx_stream_close` / `ctx_stream_fail`（ABI **#20–#22**，尾追加）；
  credit 走**续延 id 路由**（`kind:"stream-credit"`，与 `KIND_NEXT` 同形：插件侧不需要为它加解析
  分支）；窗口 = `cordis_core::STREAM_WINDOW`（4）= 底层 `mpsc` 的容量（满 ⇒ `STATUS_PENDING`）。
- **超额兑现的一点**：`ExternService::call_stream` 有默认实现（**返回值 = 长度 1 的流**），
  于是不写流式代码的提供者也能被按流调用；`ctx_fulfill` 对流派发同样按"补最后一件并收尾"处理。

**为什么要背压窗口而不是"随便推"**：不设窗口就等于让插件决定宿主的内存占用；
而"窗口用尽 ⇒ 返回 PENDING ⇒ 宿主归还额度时回调"这套语义，**恰好是现有挂起机制的
一次复用**，不需要新的内核机制。

**验收口径（已按此验收）**：一个 T2 插件推 100 块、窗口 = 4；宿主消费到第 5 块前，
插件观测到"最多 4 块未被归还"；中途 `abort` ⇒ 插件收到取消、`pending_len == 0`、
不留帧（`waterfall_len == 0`）。
证据：`crates/host/tests/abi_stream.rs`（100 块 / 窗口 4：插件观测的未归还**峰值恰为 4**、
至少被挡一次、结束计数全零；消费者 `drop` ⇒ 插件收到 `StreamCredit::Gone`、推送拿到
`STATUS_STREAM_GONE`；卸载 ⇒ 等待中的 credit 先被驳回、流以错误结束，**然后**才 `OP_DISPOSE`）；
真 DLL 层：`examples/harness-spike` 第 **8.5** 步（`harness.stream@1` 推 20 块、被窗口挡 16 次，
stdout 连跑 3 次 SHA256 全同）。

---

## 6. 后果

- 骨架的 tier 表维持三档，其中 **T3 = 计划中**（`SDK指南.md` §2）；
- **T1 承担一切流式与性能敏感路径**（D7）；T2 承担第三方组件（受审计）；
- 本文的 §4 是**现在就要守**的约束（新 op / 新 SDK API 都要按异步契约设计），
  §5 是**将来实现时的规格**；
- 一旦有人实现第二载体，判定标准只有一条：**内核与 `cordis-abi` 一行不改**
  （只新增一个 `impl Plugin` + 一套远端假插件测试）。

## 7. 何时重新审视

① 出现"必须跑不可信第三方代码"的真实需求；② 出现"插件崩溃必须不影响宿主"的
SLA 要求；③ Wasm 运行时的能力配置成熟到可以当作**策略**（而不是承诺）下发。
任一条件成立，就按 §5 的路径新开一个里程碑，并按 §4 的契约补 `AsyncIfaceFn` 与流式 op。
