# R2 评审 · RSI 研究者视角（契合度）

对象：`docs/rsi-sdk-roadmap.html` v1。只读评审，代码引用已核对。

### [P0] 核心用例被 ADR 0005 明文禁止，A5 不成立
**问题**：A5 把"变体在容器里过了评估"等同于受信任。ADR 0005 §2.2：模型生成且未经人工审查的代码在 T3 落地前不得进入运行时；loader 默认拒绝无来源插件（cli/src/main.rs:20-31）。
**论据**：过评估 ≠ 人审。RSI 每代几十个变体没人审，evolve.py 只能常开 `--allow-unverified`，即永久开发模式。
**处置**：改 ADR 0005 加"演化沙箱模式"条款；或承认阶段 1 只覆盖已选中变体的线上部署，评估期不在本宿主跑。现在两头都想占。

### [P0] 归因差异化在 WP4 场景里不可证
**问题**：PluginRSI"一次只变一个插件"是干预式因果归因；WP1 trace 是观察式。单变体循环里 evaluator 直接调 solver 打分，本来就知道哪个用例失败，trace 零增量。TS 侧 `tools/result`（tools/src/index.ts:199）也已有每次工具调用结果。
**Rust 真正多出的**：①宿主内多插件调用链 A→B→C 时定位哪一跳失败/变慢；②耗时归因；③允许一代改多个插件、用 trace 事后分解，降低 ModularRSI 的信用分配成本。
**处置**：WP4 改为"两个被演化插件 + 一条调用链"，否则 WP1 卖点无验收载体。

### [P1] TraceRecord 缺 task/episode 关联，与变体身份无法对齐
**问题**：无 task_id；uid→变体靠 WP2 的瞬时 report，热换后 uid 变、report 不留历史。rubric 由谁持有也没写。
**处置**：加 `correlation: Option<Box<str>>`（root 发起方经 ctx 透传，宿主不解释）；mount/epoch 变化也写入同一 JSONL 使其自含。rubric 宿主不持有，放变体旁的 YAML，方案写明。

### [P1] 评估期同进程热换是反模式，与方案自身定位矛盾
**问题**：`cordis run` 一配置一进程，无多实例。顺序热换 v1→v2→v3 时，ADR 0005 §1.1.5 承认的 dylib static 共享会让 v2 污染 v3 分数。Hyperagents 每代新容器正是为了消除这种泄漏。
**论据**：§1 表把 reversibility 价值限定为"线上部署"，WP4 验收 5"PID 不变"却把热换当评估路径。
**处置**：评估期每变体一进程（天然并行与隔离）；热换只用于部署已选中变体。

### [P1] 接口锁漏掉真正的契约：服务方法面
**问题**：`PluginDesc.schema` 是**配置** schema（abi/src/lib.rs:434），不是 IO schema。方法与版本在运行时 `provide_iface` 注册（context.rs:448；`ExternService::methods`，dyn_value.rs:143），不在 PluginDesc。v3 改 solver 方法参数形状**不会被 WP2b 拒绝**，验收 3 只在 v3 恰好改配置 schema 时通过。
**prompt/rubric**：prompt 在 TS `ToolDefinition.description`，rubric 在演化脚本，宿主没有对应位，不管是对的；但方法面必须进锁。
**处置**：lock 改由 `cordis status --json` 挂载后导出 `{service, version, methods}`，或要求 provision 声明方法签名并校验。

### [P2] "编译进宿主 = 不可演化"推理不成立
演化循环可以重编宿主（Hyperagents 改的就是整个 harness）。固定 verification 是循环的治理约束，T1 只是让误演化评估器需要重编译、走人类 PR。改为："T1 不在 stage 阶段的写入面上，评估器变更只经人工 PR"。

### [P1] 没人会把 Python 插件改写成 Rust cdylib
**问题**：PluginRSI 插件 = YAML+Python；deepseek-harness 工具 = TS 进程内函数；两仓对 rust-cordis 零引用。LLM 主导的秒级 episode 里，μs 级调用与热换收益不可见；成本是 ABI、unsafe、Windows 文件锁。
**唯一真实理由**：已用 rust-cordis 做线上宿主（desolate 类），想把演化产物部署回去。这是部署场景，不是研究场景。
**处置**：要让研究者接入，必须有子进程插件载体（stdio JSON-RPC，任意语言，每变体一进程、可 kill）——即 ADR 0005 的 T3 子进程路线，远比 Wasm 便宜。S2-3 改为"子进程优先"并提前到阶段 1 决策门；否则 §7"两个月无接入即冻结"大概率触发。

## 总体结论
**有条件不通过**。三个差异化：
- **接口锁**：方向对，但四项不含服务方法面，验收 3 是假阳性。补方法面后站得住。
- **归因数据源**：在"宿主内多插件调用链 + 一代多变"场景下确实是容器/PluginRSI 拿不到的，但方案没给这个场景。补场景后站得住。
- **运行宿主热换/回滚**：对评估期是反模式，对部署期有价值，方案混写。站不住，应改为"部署底座"。
加上 A5 与 ADR 0005 正面冲突，阶段 1 必须先回答"谁来跑未审变体"，否则方案在自己的信任模型下跑不起来。最有价值的修订：子进程载体 + 方法面接口锁 + 带 correlation 的 trace，定位收窄为"演化产物的部署与观测地板"。
