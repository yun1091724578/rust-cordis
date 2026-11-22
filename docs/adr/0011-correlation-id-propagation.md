# ADR 0011：多会话并发下的关联标识（`correlation_id`）透传

- **状态**：已接受（2026-11-20；随实现与端到端并发测试一并落地）
- **日期**：2026-11-20
- **依据**：`docs/adr/0009-rsi-deployment-substrate.md`（§0 A6、§2.2、§7 复评条件）、
  `docs/adr/0010-call-graph-correlation.md`、`docs/adr/0006-public-api-stability.md`；
  `crates/core/src/trace.rs`、`crates/core/src/context.rs`、`crates/host/src/lib.rs`
- **决定摘要**：在 `TraceRecord` 上新增 `correlation_id: Option<Arc<str>>`（属 ADR 0009 §4
  的“待验证”公开面），并在 `cordis-core` 提供 `Context::with_correlation` /
  `Context::correlation_id` 与 Future 级 `scope_correlation` 两条入口；宿主在
  `call_inbound` / `PendingContinuation` / `Outbound` / `IfaceTrace` / `deliver_outbound`
  全路径自动快照并恢复关联标识。**不改 ABI v1、不加 `HostApi` 字段、不改变取消树语义**。

---

## 1. 背景：单机多会话并发下 ADR 0009 A6 不再成立

ADR 0009 假设评估 episode 在宿主内**串行**执行，trace 靠墙钟时间窗对齐（假设 A6）。
当真实 AI Agent 宿主在单机单 `App` 内并发运行多个会话、并共享同一批可演化 Rust T2 插件时：

1. 多个并发会话的时间窗完全重叠，仅靠 `ts_ms` 无法区分某条 `TraceRecord` 属于哪个会话；
2. 宿主在多线程 Tokio 运行时上驱动并发会话，T1 跨 `.await` 或 T2 经 `STATUS_PENDING`
   续延在任意 worker 线程恢复时，若不在入站/出站/续延边界保存并恢复会话标识，多跳调用链
   不仅无法归属会话，还可能因 thread-local 残留发生跨会话串线。

## 2. 决定

### 2.1 `TraceRecord` 与两条注入入口

- `TraceRecord` 追加 `pub correlation_id: Option<Arc<str>>`（用 `Arc<str>` 保证跨跳 O(1)
  克隆、热路径零堆分配）。
- **入口一（`Ctx` 派生，面向长命会话上下文）**：
  - `Context::with_correlation(self: &Ctx, id: impl Into<Arc<str>>) -> Ctx`
  - `Context::correlation_id(&self) -> Option<&str>`
  - 由 `extend` / `filter` / `isolate` / `intercept` 及在会话 ctx 下挂载的子 fiber 自动继承；
    共享 T2 插件挂载在无 correlation 的根树下，调用时自动继承发起方的 `correlation_id`。
- **入口二（Future 作用域，面向共享 `Ctx` 的并发异步任务）**：
  - `scope_correlation<F: Future>(id: impl Into<Arc<str>>, future: F)`：每次 `poll` 前通过
    RAII 守卫 `enter_correlation` 写入 thread-local `CORRELATION_ID`，返回或 unwind 时自动
    恢复上一个值（零 `unsafe`、跨 `.await` 与多线程 work-stealing 安全）。
- 优先级：`ctx.correlation_id()` 显式值 > 当前执行窗口的 `current_correlation_id()`。

### 2.2 宿主侧全路径透传（不改 ABI）

- `Context::call_iface`：解析失败（`resolve_failed`）与校验失败（`iface_check_failed`）
  记录该 `correlation_id`；`ScopedCaller` 在每次 `poll` 期间同时守护 `CALLER_UID` 与
  `CORRELATION_ID`。未安装 `TraceSink` 且 `ctx.correlation_id` 为 `None` 时仍维持 ADR 0009
  §2.2 的零开销约束（不包作用域、不做快照）。
- `call_inbound`：`traced` 时快照 `current_correlation_id()`，同步出口与 `PendingContinuation`
  共用同一快照；`PendingContinuation::settle` 落定时写回 `TraceRecord`。
- `host_ctx_call_iface`：同时快照 `trace_correlation` 到 `IfaceTrace`（覆盖仅出站超时、
  poll 边界 panic、NoRuntime 三类补写）与 `Outbound.trace_correlation`。
- `deliver_outbound`：在调用 `OP_CALLBACK` 交付出站续延前，与 `call_scope_push` 并排进入
  `enter_correlation(trace_correlation)`，使 T2 插件在 `STATUS_PENDING` 恢复后发起的后续
  调用继续携带原会话的 `correlation_id` 与 `parent_call_id`，并在回调返回或 unwind 时恢复。

### 2.3 公开面与 ABI 边界

- **ABI 零改动**：`crates/abi` 与 `crates/abi/tests/baseline.txt` 不变。
- 新增受控公开项登记至 `xtask/src/inventory.rs` 与 `docs/api-inventory.md`，按 ADR 0006 /
  ADR 0009 §4 归“待验证”面（0.2 前可改可删）：
  - `Context::with_correlation`
  - `Context::correlation_id`
  - `Context::correlation_id_arc`
  - `current_correlation_id`
  - `enter_correlation`
  - `scope_correlation`

## 3. 不承诺

- 与 ADR 0010 相同的边界：插件自建裸 OS 线程直接回调宿主时不继承 thread-local 关联标识；
  typed 直调（`ctx.iface::<dyn T>()`）、`call_iface_stream` 与事件派发仍不在 trace 覆盖面内；
- `correlation_id` 是观测归因键，不是安全租户边界或授权令牌（ADR 0005）。

## 4. 验收与证据

- `crates/core/src/trace_tests.rs`：ctx 派生继承、解析/校验失败记录、poll unwind 恢复、
  多线程并发 `scope_correlation` 跨 `.await` 零串线；
- `crates/host/src/lib.rs::continuation_is_delivered_inside_the_callers_hop`：续延交付恢复
  `correlation_id` 并在返回后清零；
- `crates/host/tests/abi_trace_correlation.rs`：16 个并发会话在 4 线程 Tokio 运行时上共享
  T2 `m@1 →(PENDING) p@1 → c@1` 链，断言 48 条记录零缺失、零串线、父链完整；以及仅出站
  超时保留发起会话的 `correlation_id`。
