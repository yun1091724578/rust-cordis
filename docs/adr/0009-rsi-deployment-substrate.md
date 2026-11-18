# ADR 0009：RSI 部署与观测底座的位置

- **状态**：已接受（2026-11-18；设计决策，实现见 `docs/roadmap.md` 的对应条目）
- **日期**：2026-11-18
- **依据**：`docs/adr/0005-trust-model-and-plugin-source-boundary.md`（§2.3、§I1）、
  `docs/adr/0001-second-carrier.md`、`docs/adr/0006-public-api-stability.md`、
  `docs/adr/0007-version-is-diagnostic-only.md`、`docs/adr/0008-source-admission-scope.md`；
  `crates/core/src/context.rs`、`crates/core/src/store.rs`、
  `crates/host/src/loader.rs`、`crates/host/src/source.rs`、`crates/cli/src/lib.rs`
- **决定摘要**：本仓**不实现**自我改进/演化循环，而是演化循环的**部署与观测地板**：
  已选中变体的装载、热换、回滚，调用级归因数据，以及可冻结的插件契约。
  归因只覆盖 `call_iface`（服务接口调用），不覆盖事件派发；变体身份用
  `capabilities` 行的 `uid` + `sha256`，不新增公开结构；契约比对在外部做，
  不扩 CLI 面。评估期的多实例隔离不在本仓责任内。

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
  契约导出与冻结。
- **交界是文件**：变体产物 + sidecar manifest + 配置行；宿主经 HMR 轮询拾取，
  观测结果由宿主自己落盘。本 ADR 不引入 RPC 控制面。

被否掉的两个位置：

- **不做演化运行时**：容器编排、评分、父代选择已有成熟实现，Rust 内核在
  这些路径上没有差异化；模型往返与工具 I/O 才是 agent 产品的瓶颈。
- **不做评估期隔离**：同一 dylib 的 static 是模块级共享（ADR 0005 §1.1 第 5 条），
  同进程顺序热换 v1→v2→v3 会让前一代的状态污染后一代的分数。每变体一进程
  由外部循环负责；本仓的热换只用于部署已选中变体与金丝雀回滚。

### 2.2 归因：`call_iface` 单点，iface-only

新增 `TraceSink`（core 侧，宿主嵌入方可安装），每次服务接口调用产出一条记录：
`ts_ms`（**完成时刻**）/ `name` / `caller_uid` / `callee_uid`（**发起时快照**）/
`callee_plugin` / `elapsed` / `outcome`。

理由与边界：

- `call_iface` 是 T1→T2 与 T2→T2 的单一汇合点，且 `PluginIface::call` 内部
  已等待挂起结算，`elapsed` 覆盖 `STATUS_PENDING` 的等待期；
- `callee_uid` 必须在调用发起时快照：实例卸载后 `uid` 被置为 `u64::MAX`，
  在飞调用若在 drop 时才读，正好丢掉最需要归因的那条记录。`callee_plugin`
  （运行时名）跨热换稳定，用来兜住“记录写出时实例已不在”的情况；
- 记录由**宿主**写，插件无法经公开 API 写入或删除记录（恶意插件仍可篡改宿主
  内存，见 ADR 0005 §1.1 第 1 条）；
- **丢弃路径不止一条**：超时（`with_iface_timeout` 丢弃 future）、取消
  （排水/`complete_or_defer` 的 `None` 分支）、panic。实现用一个共享 outcome
  cell：包装层在超时/取消时写入 `host_timeout` / `host_cancel`，Drop guard
  只在 cell 仍为空时兜底 `host_drop`，且只发一条。panic 不新增第四种码
  （判 `std::thread::panicking()` 也只能在 guard 里），阶段 1 记 `host_drop`；
- **安装入口在 `App`，不在 `Context::store`**：`Context.store` 是公开字段，
  任何进程内插件都能触达；sink 若挂在 `Store` 上，插件可替换或摘除记录，
  “宿主写、插件不可改”的论证就不成立；
- **不追踪事件派发**：事件返回值不可失败（`Ret = Option<Dyn>`），记录不出
  `Err`；且 `Store::handlers()` 构造 handler 列表时丢弃 hook 身份。阶段 1
  无消费者，不做；
- **盲区**：typed 直调 `ctx.iface::<dyn T>()` 不经 `call_iface`；
  `call_iface_stream` 阶段 1 不覆盖。需要归因的调用必须走 `call_iface`；
- 未安装 sink 时必须只多一次原子读、不取锁（实现约束；不满足即算回归）。

### 2.3 变体身份：`capabilities` 行加 `uid` 与 `sha256`

`loader.report().capabilities[]` 已有逐行的 `label` / `plugin` / `inject` /
`provision` / `volatile`。追加两项：实现方 fiber 的 `uid`（与 trace 的
`caller_uid` / `callee_uid` 对齐）与 `sha256`（对**已暂存并加载的副本**取
`source::sha256_file`，而非原路径；T1 内建填 `builtin`）。

约束：

- `sha256` 存进私有 `Mounted`，**不改** `LoadedFile`（api-inventory 受控项，
  外部 `PluginSource` 用字面量构造，见 ADR 0006）；
- 不再另立 `mounts` 数组、`config_digest` 或 per-label `epoch`：前者与
  `capabilities` 重复，后者没有消费者；
- HMR 指纹是 `(mtime, len)`，不是内容哈希，不得当 `sha256` 用；
- `uid` 是 **per-Store 计数器**，只在同一 Store 生命周期内有意义；实例卸载后
  `uid` 被置为 `u64::MAX`。变体级对齐靠 trace 的 `callee_plugin` +
  消费方在每次 `report` 变化时切时间窗；本 ADR 不提供 `mount_gen`，
  也不承诺把“热换瞬间的在飞调用”归到正确的那一代；
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
| `iface_spec_sha256` | manifest 记录的接口规范原文 hash | 让宿主侧导出与作者侧 YAML 同源 |

边界：参数与返回值在 ABI 上是 `Config`，宿主看不到形状，所以方法面只能锁
“名字集合”（增删改名），锁不住参数类型变化。真正的 IO schema 锁需要给
`PluginDesc` 按方法增加 args/ret schema——这是另一项设计决策，不在本 ADR 与
阶段 1 内。`iface_spec_sha256` 只证明“两边看的是同一份规范”，不证明规范与
实现对得上。

导出面目前**不存在**：`StatusReport` 没有 capabilities/services，
`status_with_source` 在取完诊断后就 shutdown。阶段 1 需要两处加法——
`Store::services_of(&Arc<Fiber>)`（按 fiber 枚举服务与方法名，接收 fiber 而非
裸 uid；`uid` 只作行内字段，`UID_DISPOSED` 返回空并标记 disposed）与
`StatusReport.capabilities`（typed row，仿 `RowProvenance`；`StatusReport`
派生了 `Eq`，不能塞 `serde_json::Value`）。方法名必须在 shutdown 之前抓取。
两项都是加法，按 ADR 0006 登记 `api-inventory`，归“待验证”面。

契约比对由演化脚本或测试对两次导出做 diff，**不加** `validate --interface-lock`
一类 CLI flag。`attestation`（ADR 0005 §2.3）属于准入材料，不属于契约。

## 3. 不承诺

- **不承诺跨变体隔离**：T2 同进程、同地址空间；评估期隔离是外部循环的事。
- **不承诺金丝雀分数与容器评估分数可比**：同进程顺序热换仍受 dylib static
  污染（ADR 0005 §1.1 第 5 条）。基线定义为“同一 App、热换前、同一用例集”；
  硬判据不是分数，而是“同一用例由 Ok 变 Err”或“出现
  `callee_plugin=<变体名>` 的新 Err”，回滚后要求同一用例回到 Ok；
  跨容器/跨进程的分数不做比较。
- **不承诺记录自带用例粒度**：`TraceRecord` 没有 case id。对齐靠两条约定：
  宿主在 `report` 每次变化时落一份 mounts 快照（`label` / `uid` /
  `sha256` / UTC 毫秒），evaluator 落每个用例的起止时间戳；两者一起才能把
  记录归到“哪一代、哪个用例”。
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
- **ADR 0006（公开 API 稳定）**：`TraceSink` 与 `set_trace_sink` 是**加法**，
  属"待验证"面，须登记 `api-inventory`；不构成 0.2 迁移。
- **ADR 0007（版本只做诊断）**：`sha256` 与 `uid` 是身份与诊断，不升级为
  行为兼容的证明。
- **ADR 0008（来源准入作用域）**：变体准入沿用来源对象自带的结论
  （`PolicyPluginSource::strict`），不使用进程级开发放行。

## 5. 后果

**正面**

- 演化循环与宿主解耦：宿主换实现语言或换选择策略都不影响部署路径；
- 归因记录由宿主产生，插件无法经公开 API 自报或删除（恶意插件仍可篡改宿主
  内存，见 ADR 0005 §1.1 第 1 条）；
- 复用既有回滚与 volatile 语义，不新增 ABI、状态码或公开结构。

**负面**

- 归因覆盖面窄（iface-only），事件密集型插件的调用链仍不可见；
- 契约锁只到方法名集合，参数类型漂移检测不了；
- `uid` 只在单个 Store 生命周期内有效，跨热换的 trace 需要消费方切分时间窗；
- 单实例宿主意味着金丝雀期间新旧变体不同时在线，回滚是“换回上一版”而非
  “按比例切流”；
- 若没有真实消费者（见 roadmap 的接入门），本 ADR 的实施投入会沉没。

## 6. 验收与证据

本 ADR 是设计决策，验收落在 roadmap 的对应任务条目：

- 调用链归因：A→B→C 中 C 失败时产出两条 Err 记录，且按 `callee_uid` 聚合
  能把失败归到 C；
- 挂起结算：PENDING 50ms 后 `elapsed ≥ 50ms`；
- 丢弃路径：超时 → 恰好一条 `host_timeout`；取消 → `host_cancel`；panic →
  一条 `host_drop`；未接 outcome cell 时前两条用例必须红（防止“任何 drop
  都记超时”）；
- callee panic：宿主仍产出 `Err` 记录（“记录由宿主写”的硬证据）；
- 在飞热换：PENDING 期间换实例，记录仍带发起时的 `callee_uid` 与稳定的
  `callee_plugin`，`callee_uid != u64::MAX`；
- 变体身份：同内容不同 mtime 的 `sha256` 相同，改一字节则不同；热换后同
  label 的 `uid` 变化；
- 契约：增删或改名一个 `services[].methods` 条目时比对结果不一致，重编实现
  而声明与方法名不变时一致；
- ABI 形状不变：`crates/abi/tests/v1_shape.rs` 与 `baseline.txt` 不动。

## 7. 复评触发条件

出现任一情况，必须重开本 ADR：

- 需要事件级或流式接口的归因；
- 需要并行 episode 或按比例切流（单实例假设不成立）；
- 需要 RPC 控制面（文件驱动不足以满足消费方）；
- 需要在评估期直接运行未评估产物（触发 T3，见 ADR 0001）；
- 契约锁被发现不足以拦住真实的跨代破坏。
