# ADR 0009：RSI 部署与观测底座的位置

- **状态**：已接受（2026-11-18；设计决策，保证与证据索引见 `docs/support-contract.md`）
- **日期**：2026-11-18
- **依据**：`docs/adr/0005-trust-model-and-plugin-source-boundary.md`（§2.3、§I1）、
  `docs/adr/0001-second-carrier.md`、`docs/adr/0006-public-api-stability.md`、
  `docs/adr/0007-version-is-diagnostic-only.md`、`docs/adr/0008-source-admission-scope.md`；
  `crates/core/src/context.rs`、`crates/core/src/store.rs`、
  `crates/host/src/loader.rs`、`crates/host/src/source.rs`、`crates/cli/src/lib.rs`
- **决定摘要**：本仓**不实现**自我改进/演化循环，而是演化循环的**部署与观测地板**：
  已选中变体的装载、热换、回滚，调用级归因数据，以及可冻结的插件契约。
  归因只覆盖 `call_iface`（服务接口调用），不覆盖事件派发；变体身份用
  `capabilities` 行的 `uid` + `sha256`；契约比对在外部做，不扩 CLI 面。
  公开面只按 §4 的表增加，不改 ABI。评估期的多实例隔离不在本仓责任内。

---

## 1. 背景：演化循环需要宿主，但不需要宿主参与演化

前沿的自改进/演化循环把"生成变体、评估、选父代、归档"放在容器与 git 里完成
（每代一个新容器，失败即丢弃）。它们的隔离与回滚不依赖进程内事务。

于是本仓面对的问题不是"如何演化"，而是两个更窄的问题：

1. **已选中的变体如何上线、出错如何回到上一版**——这是部署问题；
2. **调用链上的回归如何定位到具体一跳**——这是归因问题。

两者都不需要把 LLM、评分或选择策略放进 Rust。把循环搬进内核反而会与
ADR 0005 的信任模型冲突：循环产物是模型生成代码，逐个人工审查不成立。

## 2. 决定

### 2.1 位置：部署与观测地板，不是演化运行时

- **外部循环负责**：变体生成、评估执行、选择规则、归档库、每变体的隔离。
- **本仓负责**：装载已签发变体（ADR 0005 §2.3）、失败回滚、调用级归因、
  契约导出与冻结。**签发材料不在运行时校验**：`verify` 只比对
  id/semver/revision/hash/ABI，attestation 由部署准入台账核对。
- **交界是文件**：变体产物 + sidecar manifest + 配置行；宿主经 HMR 轮询拾取，
  观测结果由宿主自己落盘。本 ADR 不引入 RPC 控制面。

被否掉的两个位置：

- **不做演化运行时**：容器编排、评分、父代选择已有成熟实现，Rust 内核在
  这些路径上没有差异化；模型往返与工具 I/O 才是 agent 产品的瓶颈。
- **不做评估期隔离**：同一 dylib 的 static 是模块级共享（ADR 0005 §1.1 第 5 条），
  同进程顺序热换 v1→v2→v3 会让前一代的状态污染后一代的分数。每变体一进程
  由外部循环负责；本仓的热换只用于部署已选中变体与金丝雀回滚。

### 2.2 归因：服务调用，iface-only

新增 `TraceSink`（由宿主嵌入方安装），每次服务接口调用产出一条记录：
`ts_ms`（**完成时刻**）/ `name` / `call_id` / `parent_call_id` /
`caller_uid` / `callee_uid`（**发起时快照**）/ `callee_plugin` / `elapsed` /
`outcome`。

理由与边界：

- **`call_id` 复用入站 `dispatch_id`，父链的取值与压栈见 ADR 0010**。
  本 ADR 只定义字段与写入点；“父 = 最近的、有记录的 iface 跳”由
  `CALL_SCOPE` 在 `call_inbound` 与 `deliver_outbound` 两个窗口推出，
  不改 ABI、不加 `HostApi` 字段；
- **写入点：调用产生最终结果的地方，一次调用只写一条**：
  1. **core**：`iface_target` 解析失败与随后的版本/方法检查失败（入站派发
     记录尚未创建）。记 `call_id=0`、`callee_uid=0`、
     `callee_plugin=<请求的服务名>`、`outcome=Err(code)`；
  2. **host**：入站调用的最终结果。同步返回（`STATUS_OK` / `STATUS_ERR` /
     ABI 边界 panic）在 `call_inbound` 的出口；`STATUS_PENDING` 在 `settle`
     落定时。两处共用同一个 `dispatch_id`，且对同一次调用互斥；
  3. **host，仅出站看门狗单独超时**：只配了 `outbound_timeout_ms`、入站
     从未结算时，由出站包装层写一条 `call_id=0`、`outcome=HOST_TIMEOUT`
     的记录。它必须与 2 互斥（有入站结算就不写）；
  4. **host，出站 future 的 poll 边界 panic**：入站没有结算、调用在
     `complete_or_defer` 的 `CatchUnwind` 里 unwind（首 poll 或后台 poll），
     由同一份 `IfaceTrace` 写一条 `call_id=0`、`outcome=host_panic` 的记录。
     首 poll 与后台 `on_panic` 是互斥分支，与 2 也不重叠；
  5. **host，出站调用无法驱动（NoRuntime）**：首个 poll 未就绪且当前线程没有
     tokio runtime，调用方同步拿到 `host_call`；由同一份 `IfaceTrace` 写一条
     `call_id=0`、`outcome=host_call` 的记录。它只发生在“入站已发起但从未
     结算”的同步上下文，与 2 互斥。
  三类异常码沿用 `HOST_TIMEOUT` / `host_cancel` / `host_panic`，不新增 ABI
  状态码；结算通道关闭时沿用 `host_lifecycle`，出站无法驱动时沿用 `host_call`，
  插件自带业务码原样透传；
  解析失败另用 trace-only 内部码（`resolve_failed` / `iface_check_failed` /
  `status_<n>`）；`plugin_unsupported` / `plugin_protocol` 是既有调用协议码，
  trace 复用同名码，不是 trace 专用；
- **取消只写一次**：取消交付走 `drain_dispatch_tree` / `settle`，与超时/panic
  同在入站结算路径上；`revoke_dispatch` 等早退路径不写记录；
- **`callee_uid` 在发起时快照**：实例卸载后 `uid` 被置为 `u64::MAX`，在飞
  调用若在结算时才读，正好丢掉最需要归因的那条记录。`callee_plugin`
  （运行时名）跨热换稳定，兜住“记录写出时实例已不在”；
- **结果分类依据**：三类异常靠错误载荷里的 `code` 区分（`HOST_TIMEOUT` /
  `host_cancel` / `host_panic`），不靠调用方猜测；
- **解析不到实现时也要记**：见第 1 点。`callee_uid=0` 表示“没有记到实现方”
  （解析失败/仅出站超时/poll 边界 panic/NoRuntime），此时 `callee_plugin` 是
  **请求的服务名**，不是实例名。根提供者的 uid **也**是 `UID_ROOT=0`，因此
  不能只看 uid 区分“根/未知”：未记到实现方时 `callee_plugin` 是请求名，
  根实现时是 `"root"`；当前没有路径会产出空串；
- **外部丢弃调用 future 时没有记录**：调用方任务被整体取消、或调用方自己
  `select!` 掉 future，只剩宿主侧可观测的取消结果；
- 记录由**宿主**写：T2 插件无法经 ABI 写入或删除记录；T1 插件编译进宿主，
  能调用任何公开 API（包括 `Store::record_trace`），防不住——这是 T1 受信
  信任域的直接后果（ADR 0005 §2.1）。恶意插件仍可篡改宿主内存（§1.1 第 1 条）；
- **安装入口在 `App`，不在 `Context::store`**：`Context.store` 是公开字段，
  任何进程内插件都能触达；sink 槽只提供 `pub(crate)` 安装口、没有可替换的
  公开写口，避免“插件换掉/摘掉记录”；
- **不追踪事件派发**：事件返回值不可失败（`Ret = Option<Dyn>`），记录不出
  `Err`；且 `Store::handlers()` 构造 handler 列表时丢弃 hook 身份；
- **盲区**：调用 T1 提供者的**正常结算**不产生记录（不论 T1→T1 还是
  T2→T1；T1 的 `ExternService` 由 core 直接 `iface.call`，不经 `call_inbound`
  的宿主统计）；T2→T1 的看门狗超时 / poll 边界 panic / NoRuntime 仍由 host
  补写。解析/校验失败由 core 记一条 `call_id=0`。typed 直调
  `ctx.iface::<dyn T>()` 不经 `call_iface`；`call_iface_stream` 不覆盖；
  `admit_call`/容量拒绝在建派发记录之前，因“不新增码”也不记录；
  `host_ctx_call_iface` 的 args 不是合法 JSON 时在派发记录之前返回，不写记录；
- 未安装 sink 时每个入口只多一次原子读 + 一次时钟读，不取锁、不构造身份快照
  （实现约束；不满足即算回归）；
- `record` 必须同步、不阻塞调用线程、不重入宿主；实现内部可用短锁保护
  自己的状态（sink 里再发起调用会递归）。

### 2.3 变体身份：`capabilities` 行加 `uid` 与 `sha256`

`loader.report().capabilities[]` 已有逐行的 `label` / `plugin` / `js` / `inject` /
`provision` / `volatile`。追加两项：实现方 fiber 的 `uid`（与 trace 的
`caller_uid` / `callee_uid` 对齐）与 `sha256`（对**已暂存并加载的副本**取
`source::sha256_file`，而非原路径；无指纹/非文件来源填 `"builtin"`，文件来源
读取失败填 `"unavailable: ..."`）。HMR 的 `reload_one` 换副本后必须在
`commit_outcome` 重算，保证 `sha256 == hash(staged)`。

约束：

- `sha256` 存进私有 `Mounted`，**不改** `LoadedFile`（api-inventory 受控项，
  外部 `PluginSource` 用字面量构造，见 ADR 0006）；
- 不再另立 `mounts` 数组、`config_digest` 或 per-label `epoch`：前者与
  `capabilities` 重复，后者没有消费者；
- HMR 指纹是 `(mtime, len)`，不是内容哈希，不得当 `sha256` 用；
- `report` 载荷**只增不改**：新字段只追加，既有字段不重命名、语义不变；
- `uid` 是 **per-Store 计数器**，只在同一 Store 生命周期内有意义；实例卸载后
  `uid` 被置为 `u64::MAX`（报告只遍历已挂载行，因此该回退是防御性的）。变体级
  对齐靠 trace 的 `callee_plugin` + 消费方在每次 `report` 变化时切时间窗；
  本 ADR 不提供 `mount_gen`，也不承诺把“热换瞬间的在飞调用”归到正确的那一代；
- 变体不得用 `SourceAdmission::dev_unverified` 放行；这是部署纪律，
  loader 无法强制（它不区分变体与普通插件）。

### 2.4 契约：插件声明 + 服务方法名

“契约”指 host 侧可读、且变体不得改动的部分：

| 面 | 来源 | 备注 |
|---|---|---|
| `inject` | 插件声明（可合并配置行） | 行级声明会混入，比对时须固定口径 |
| `provision` | 插件声明 | 与 `inject` 对称 |
| `schema` | 插件自述 | 这是**配置** schema，不是服务 IO schema |
| `services[].methods` | `provide_iface` 运行时注册 | **只有方法名**；由 `status --json` 导出（阶段 1） |
| `iface_spec_sha256` | manifest 记录的接口规范原文 hash | 是否声明可选；`status --json` 的 `provenance` 与 `capabilities` 都会导出，缺失为 `null` |

边界：参数与返回值在 ABI 上是 `Config`，宿主看不到形状，所以方法面只能锁
“名字集合”（增删改名），锁不住参数类型变化。真正的 IO schema 锁需要给
`PluginDesc` 按方法增加 args/ret schema——这是另一项设计决策，不在本 ADR 与
阶段 1 内。`iface_spec_sha256` 只证明“两边看的是同一份规范”，不证明规范与
实现对得上。`services[].name` 是注册键；同一 fiber 在多个 isolate 域注册完全
相同的 `(name, version, methods)` 会在导出时去重，契约锁不区分 realm。`js`
仍在 `loader.report` 的 capabilities 行里，不在 `status` 的 typed row，也不在
契约锁范围。

导出面已按阶段 1 落地（WP2b）：`Store::services_of(&Arc<Fiber>)` 按 fiber
枚举带 JSON 面的服务（**同一 fiber 身份判据**，补上 version 与 `Dyn::adapter`
里的方法名，不另立第二套枚举语义；方法名排序去重；接收 fiber 而非裸 uid，
`UID_DISPOSED` 返回空，调用方用 `fiber.uid()` 区分“没有服务”与“实例已经
没了”），`StatusReport.capabilities` 是 typed row（仿 `RowProvenance`；
`StatusReport` 派生了 `Eq`，不能塞 `serde_json::Value`），其 `services` 在
`status_with_source` 关闭装载树**之前**抓取。`loader.report().capabilities[]`
同时补了 `schema`（插件自述的**配置** schema），使 `status --json` 的契约子集
可以只读一次导出。`js` 仍只在 `loader.report` 里，`StatusReport` 的 typed row
不带它；`schema` 只报原文/`null`，“未声明”与“声明坏了”的区分仍看 `inspect`
的 `schema_error`。sidecar manifest 可选记录 `iface_spec_sha256`，CLI 在
`provenance` 与 `capabilities` 两处导出；缺失为 `null`，宿主不校验规范与实现
是否一致。上述公开项按 ADR 0006 登记 `api-inventory`（CLI 类型不在受控清单
覆盖面内，按“待验证”面处理），归“待验证”面：**0.2 前可改可删，不入兼容面**。

契约比对由演化脚本或测试对两次导出做 diff，**不加** `validate --interface-lock`
一类 CLI flag。`attestation`（ADR 0005 §2.3）属于准入材料，不属于契约。

## 3. 不承诺

- **父链的边界（ADR 0010 §2.6）**：T1 异步起点、T1 挂起后的提供者侧、事件起点、
  T1 在宿主回调里发起、自建线程回调均无父；T2→T1→T2 跳过 T1。
  这些是语义边界，不是实现缺口。
- **不承诺跨变体隔离**：T2 同进程、同地址空间；评估期隔离是外部循环的事。
- **不承诺回滚等价于“状态复位”**：两种回滚路径的静态语义不同——apply 失败时
  回滚是重挂旧 `Arc<dyn Plugin>`（同一个已加载模块，`static` 保留）；改 YAML
  重投会走新的暂存文件（新模块，`static` 全新）。用例必须幂等或自行复位，
  否则预热过的静态状态会造成假绿、v2 遗留的线程/文件/端口会造成假红。
- **不承诺金丝雀分数与容器评估分数可比**：同进程顺序热换仍受 dylib static
  污染（ADR 0005 §1.1 第 5 条）。基线定义为“同一 App、热换前、同一用例集”；
  硬判据不是分数，而是“同一用例由 Ok 变 Err”或“出现
  `callee_plugin=<变体名>` 的新 Err”，回滚后要求同一用例回到 Ok；
  跨容器/跨进程的分数不做比较。
- **不承诺记录自带用例粒度**：`TraceRecord` 没有 case id。对齐靠两条约定：
  消费方（示例宿主）在 `report` 变化后落一份 mounts 快照（`plugin` / `label` /
  `uid` / `sha256` / 递增 `seq` / UTC 毫秒，取 swap 完成之后的值；这是落盘
  文件，不是 report 里的新数组），evaluator 落每个
  用例的起止时间戳（同一时钟）；`report` 是拉取式、没有变更通知，快照由
  **消费方**轮询写入，窗口误差至少是轮询间隔（HMR 默认 500ms）。
- **不承诺事件级归因**：事件派发不在 trace 覆盖面内。
- **不承诺 typed 直调与流式接口的归因**：见 §2.2 盲区。
- **不承诺运行中宿主的 RPC 控制**：交界是文件（§2.1）。
- **不承诺“契约不变即行为不变”**：契约只钉住声明与方法名集合，不钉住语义；
  行为回归靠评估器与 trace，不靠契约锁。
- **不承诺 trace 跨热换自动对齐变体**：见 §2.3 的 `uid` 生命周期。
- 不改变 ADR 0005 的任何禁用语口径；`attestation` 不是运行时信任。

## 4. 与既有 ADR 的关系

- **ADR 0005（信任模型）**：§2.3 给出演化产物的签发路径；本 ADR 只消费
  "已选中且已签发"的产物。若签发路径不成立（流水线被攻破），本 ADR 的
  部署能力不构成补偿。
- **ADR 0001（第二载体）**：本 ADR 不选 T3。若要直接运行未经评估的产物，
  仍需 T3（子进程优先于 Wasm：可 kill、任意语言、无需 ABI 冻结）。
- **ADR 0006（公开 API 稳定）**：本 ADR 新增的公开项全部列在下表，
  一律属“待验证”面（0.2 前可改可删），需登记 `api-inventory`：

  | 项 | 形态 | 用途 |
  |---|---|---|
  | `TraceSink` / `TraceRecord`（含 `Outcome`） | 新类型 | 记录与 sink 契约 |
  | `App::set_trace_sink` | 新方法 | 安装入口（不挂公开的 `Store`） |
  | `Store::record_trace` | 新方法 | 宿主写记录的 seam（T1 可调用，见 §2.2） |
  | `Store::has_trace_sink` | 新方法 | 无 sink 时跳过身份快照构造 |
  | `current_caller_uid` | 新函数 | host 读取 poll 级发起方 uid |
  | `Store::services_of` | 新方法（WP2b 已实现） | 按 fiber 枚举服务与方法名 |
  | `ServiceInfo` | 新类型（WP2b 已实现） | `name` / `version` / sorted `methods` |
  | `StatusReport.capabilities` + `CapabilityRow` | 新字段 + 新类型（WP2b 已实现） | 导出契约行（含 `services` / `iface_spec_sha256`） |
  | `Provenance.iface_spec_sha256` | 新字段（CLI，WP2b 已实现） | sidecar manifest 的接口规范 hash |

  不构成 0.2 迁移；上表之外不再新增公开面。`callee_uid` / `callee_plugin`
  由入站结算路径已有信息构造，不为此新增入口。
- **ADR 0007（版本只做诊断）**：`sha256` 与 `uid` 是身份与诊断，不升级为
  行为兼容的证明。
- **ADR 0008（来源准入作用域）**：变体准入沿用来源对象自带的结论
  （`PolicyPluginSource::strict`），不使用进程级开发放行。
- **ADR 0010（调用链关联）**：定义 `parent_call_id` 的语义与 `CALL_SCOPE`
  的两个压栈窗口；本 ADR 的写入点、结果码与盲区不变。

## 5. 后果

**正面**

- 演化循环与宿主解耦：宿主换实现语言或换选择策略都不影响部署路径；
- 归因记录由宿主产生，T2 插件无法经 ABI 自报或删除；T1 插件可调用公开 API
  （受信域，见 §2.2）；
- 复用既有回滚与 volatile 语义，不新增 ABI 或状态码；
- 逐跳记录足以回答“哪个插件的调用在失败/变慢”，不需要新协议或新状态机。

**负面**

- 归因覆盖面窄（iface-only），事件密集型插件的调用链仍不可见；
- **父链只覆盖宿主能观测的窗口**：T1 异步起点、T1 Pending、事件起点、
  自建线程的回调为 0；跨热换的对齐仍靠消费方切窗；
- 契约锁只到方法名集合，参数类型漂移检测不了；
- `uid` 只在单个 Store 生命周期内有效，跨热换的 trace 需要消费方切分时间窗；
- 新增 §4 表中的公开项，须按 ADR 0006 登记，并在 0.2 时统一评估；
- 单实例宿主意味着金丝雀期间新旧变体不同时在线，回滚是“换回上一版”而非
  “按比例切流”；
- 若没有真实消费者，本 ADR 的实施投入会沉没。

## 6. 验收与证据

本 ADR 是设计决策，验收证据登记在 `docs/support-contract.md`：

- 逐跳事实：A→B→C 中 C 失败时产出三条记录（若 A 是根则两条），
  `parent_call_id` 串成链；按叶子 Err 聚合能把失败归到 C（细节见 ADR 0010 §6）；
- 一跳一条：同一次调用只产出一条记录；出站包装层**只在**三种“入站从未结算”
  的情况下补写 `call_id=0` 的记录：仅出站看门狗超时、poll 边界 panic、
  NoRuntime；有入站结算则不写；
- 仅出站超时：只配 `outbound_timeout_ms` 时仍有一条 `HOST_TIMEOUT`，
  `call_id=0`、`callee_uid=0`、`callee_plugin=请求名`、`elapsed≈超时值`；
  入站与出站都配超时时，谁先到点谁写一条（入站 → `call_id!=0`，出站 →
  `call_id=0`），不得出现第二条；
- 挂起结算：PENDING 30ms 后 `elapsed ≥ 30ms`；
- 异常终止：超时 → `HOST_TIMEOUT`；取消 → `host_cancel`；poll 边界 panic →
  `host_panic`；三者都不得记成同一种码（写死成超时码时前两条红）；
- 解析失败：请求一个不存在/版本不匹配/方法不匹配的服务 → 产出 Err 记录，
  `call_id=0`、`callee_uid=0`、`callee_plugin` 是请求的服务名；
- callee panic 分两种，必须分别断言：
  1. **宿主 poll 边界 unwind**（T1 native 提供者或调用方 future）：一条
     `call_id=0`、`callee_uid=0`、`host_panic` 的记录；
  2. **ABI 边界的 `STATUS_ERR`**（T2 插件自己把 panic 收敛成错误码）：走
     入站结算，记录带真实 `call_id` / `callee_uid`，码是插件报的（如
     `host_panic` 或业务码）。不能用进程内 `extern "C"` 夹具制造真 panic
     （会 abort 整个进程）；T2 这一侧只能以 `STATUS_ERR` 表现；
- 在飞换实例：
  - **调用方是 inject 依赖者**：loader 的 `Replace` 先卸提供者，内核等依赖者
    退出，依赖者的出站 future 被丢弃，按 §2.2“外部丢弃无记录”处理；
  - **调用方活过被调方**（根 ctx，或消费者不声明依赖）：提供者卸载把
    `host_lifecycle` 送进 pending sink，`settle` 仍写一条记录；此时
    `callee_uid` 必须是发起时快照（结算时现场读会得到 `UID_DISPOSED`）。
    判别性用例是 `abi_trace_unload`；
  - **不要**用“依赖者一起卸载后仍有一条记录”当判据（该判据与卸载级联矛盾）；
- 变体身份：同内容不同 mtime 的 `sha256` 相同，改一字节则不同；热换后同
  label 的 `uid` 变化；
- 契约：增删或改名一个 `services[].methods` 条目时比对结果不一致，重编实现
  而声明与方法名不变时一致；`cli/tests/status.rs::status_services_methods`
  固定“两个方法按名字排序导出、关闭后仍可读”的判据；
- ABI 形状不变：`crates/abi/tests/v1_shape.rs` 与 `baseline.txt` 不动。

## 7. 复评触发条件

出现任一情况，必须重开本 ADR：

- 需要事件/waterfall 级或 T1 异步起点的父链（见 ADR 0010 §7）；
- 需要跨进程/跨重启的链路拼接；
- 需要并行 episode 或按比例切流（单实例假设不成立）；
- 需要 RPC 控制面（文件驱动不足以满足消费方）；
- 需要在评估期直接运行未评估产物（触发 T3，见 ADR 0001）；
- 契约锁被发现不足以拦住真实的跨代破坏。
