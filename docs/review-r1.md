# R1 · 简化与范围评审（Karpathy 视角）

对象：`docs/rsi-sdk-roadmap.html` v1。只读仓库核实，未改代码。

---

### [P0] WP4 的外部驱动路径不成立：`cordis status` 不是查询运行中宿主
**问题**：evolve.py 计划"轮询 `cordis status --json` 直到 `mounts[solver].epoch` 递增 / `failed` 含 solver"。但 `status` 是**自己起一个新 App 短暂挂载再关闭**（`crates/cli/src/main.rs:23-26` "短暂挂载后打印诊断并关闭（会执行插件 apply/cleanup）"），看不到 `cordis run` 进程里的 epoch、failed、回滚状态。`loader.report` 仅为进程内服务（`loader.rs:1798 publish_report` 用 `ctx.set`），`run` 不落盘。
**处置**：改写 WP4——e2e 用 Rust 进程内驱动（沿用 `host/tests/loader_reconcile.rs:417` 的 `require("loader.report")` 模式），evolve.py 降为文档样例或删除。省掉 CI 装 Python、Windows/Linux 双平台子进程编排。

### [P0] WP1 事件派发埋点的前提错误：出口有 4 处且 handler 无身份、无结果
**证据**：`store.rs:1318` 把 `Vec<Handler>` 交给 `event::dispatch`，`event.rs:174/194/215/230` 四种 mode 各自展开循环；`store.rs:1244` 构建 handler 列表时**丢掉了 hook_ctx**，`Handler = Arc<dyn Fn(Call)->BoxFuture<Ret>>`（`event.rs:61`）无 callee 身份；`Ret = Option<Dyn>`（`event.rs:12`）**不可失败**，事件路径根本产不出 `Outcome::Err`。方案自估 "+2 人日"低估，且 WP4 五条断言全部只涉及 iface 调用（evaluator→"solver" 服务），事件 trace 零消费者。
**处置**：阶段 1 **只 trace iface 调用**。`context.rs:468 call_iface`（及 :495 `call_iface_stream`）是单点、带 `Result<Config>`、持有 caller_meta；host 侧 PENDING 在 `lib.rs:1735 settle` 补 outcome。`TraceKind` 字段随之删除。

### [P1] WP4 场景证明不了"内核归因的差异化"
**问题**：§1 表的卖点是"容器+git 拿不到 caller/callee/耗时"。但 WP4 里 evaluator **直接**调 solver，evaluator 自己就知道这四项，不需要内核 trace。断言 2 "trace 中 2 条 Err，callee=solver" 用 evaluator 自写日志同样通过。
**处置**：场景加一层间接调用（evaluator → pipeline 插件 → solver），断言归因能把失败归到 solver 而非 pipeline。否则 WP1 没有验收载体，应整体砍掉。

### [P1] WP2 大半已存在，引用的"HMR 指纹"是错位置、错语义
**证据**：① `report().capabilities[]`（`loader.rs:1615-1665`）已是逐行 `{label, plugin, inject, provision, volatile}` 清单，G3"无逐实例列表"不成立。② `loader.rs:4427` 是 `on_plugin` 自卸载观察者，不是指纹；指纹在 `:299 fingerprint()`，是 `(mtime, len)`，**不是 sha256**，不能"复用"成 `source_digest`。真正的内容哈希已有：`host/src/source.rs:248 sha256_file`，且 `cordis status --json provenance[].sha256 / source_revision` 已暴露（`cli/src/lib.rs:161-169`）。③ `Mounted` 已持有 `fiber: Weak<Fiber>`（`loader.rs:273`）和 `label_of_fiber`（`:1830`），uid↔label 翻译无需 `Store::all_fibers()`。
**处置**：WP2 缩为"在 `capabilities` 行追加 `uid`、`sha256`"（≤0.5 人日）。砍 `all_fibers()`、砍 `config_digest`（WP4 不改配置，无消费者）、砍 `epoch`（进程内测试可直接比 uid 变化）。变体身份用 sidecar manifest 的 `source_revision` 由演化脚本自己写入。

### [P1] WP2b 不需要新 CLI flag
**证据**：`cordis inspect --json` 已输出每行 `plugin/inject/provision/schema`（`cli/src/lib.rs:273-286`），方案第 1 条"缺哪项补哪项"已满足。
**处置**：接口锁 = 演化脚本对两次 `inspect --json` 的 rows 做 JSON diff，10 行 Python/Rust 测试代码。砍 `validate --interface-lock`。

### [P1] TraceRecord 字段过半无消费者
- `seq`：JSONL 行序即顺序，砍。
- `kind`：仅 iface 后无意义，砍。
- `started: Instant`：不可序列化，JsonlSink 必然另造墙钟字段；e2e 不读，砍或改 `ts_ms`。
- `caller_uid`/`callee_uid` 与 `*_plugin` 二选一：有 WP2 的 uid→label 表时留 uid 即可。
- `Outcome::Pending`：方案说"settle 时补写"意味着改记录或发两条，语义未定。只在落定时发一条，砍 Pending。
保留：`name, caller_uid, callee_uid, elapsed, outcome`。

### [P1] 范围蔓延：不能追溯到 §1 的改动
- WP0 第 2 条"方案.md §7 追加第 12 条 ctx.effect 偏离"——与四个开放问题无关，另开提交。
- WP1 `cordis run --trace-out`、`JsonlSink`——WP4 示例宿主可在进程内 `set_trace_sink`，CLI 面不必扩。sink 落盘留给示例代码。
- §7 "有界 channel + 独立写盘线程"——为一个未存在的性能问题预设实现，砍。

### [P2] G6 "Windows 文件锁未验证" 为已解决项
`loader.rs:261-264` `staged` 字段注释明写"Windows 上不能原地覆盖已加载的库"，`:3410 stage_plugin` 先复制再加载。G6、§7 第 2 行风险、以及 2 人日缓冲中对应份额可删。

### [P2] 模糊验收
- WP2 "手工核对与 YAML 一一对应"→ 改为断言 `capabilities.len() == 启用行数`。
- WP1 "elapsed > 0" 几乎恒真；改为"PENDING 等待 50ms 后 elapsed ≥ 50ms"。
- WP1 "派发路径不分配"难以断言；改为 `sink.is_none()` 早返回 + 代码评审，删测试。
- WP0 "R2 确认可理解"、V7/V8 "评审检查"不是可执行验收，移出 §6 表。

---

## 总体结论

**未通过**简化评审：两处 P0 使 WP4 验收载体与 WP1 核心埋点在现有代码上不可按方案实施；WP2/WP2b 约 70% 功能已存在，方案基于错误的代码事实（G3、G6、:4427）立项。方向（理解甲、不动 ABI、外部循环）正确，保留。

| WP | 处置 | 修订估算 |
|---|---|---|
| WP0 | 保留 ADR 0009（精简到一页）；砍 §7 第 12 条 | 0.5 |
| WP1 | 保留，缩为 iface-only，5 字段，进程内 sink，无 CLI flag | 3 |
| WP2 | 缩为 capabilities 行加 `uid`+`sha256`；砍 all_fibers/config_digest/epoch | 0.5 |
| WP2b | 砍 CLI flag，改为测试内 inspect JSON diff | 0.5 |
| WP4 | 改写为 Rust 进程内 e2e，加间接调用层，evolve.py 降为样例 | 4 |
| 缓冲 | 砍 Windows 文件锁份额 | 1 |

**修订后约 9.5 人日**（原 20）。前置条件：先修正 §2 G3/G6 与附录 A 的 `loader.rs:4427` 引用，再进 R2/R3。
