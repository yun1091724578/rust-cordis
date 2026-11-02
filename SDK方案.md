# rust-cordis SDK 骨架演进方案

> 状态：**v4（终稿）**。已完成 3 轮多 agent 评审（9 名独立评审员），评审记录见附录 C。
> 依据：论文 arXiv:2608.25512（92 页版）、《原理.md》、《方案.md》、《补全方案.md》、
> 审计结论（本仓 112 测试 / demo 全绿，已实测复核）。
> 交付方式：**先写失败测试（红）→ 最小实现（绿）→ 全量回归**；里程碑 M0→M11。
>
> **取代清单（本方案生效后，以下内容作废或改写）**：
> ①《方案.md》§10「演进路线」整节；
> ②《方案.md》§7 **#8**（"插件回调必须同步"）与 **#9**（"waterfall 无法映射"）——由 D3/D6 修订；
> ③《方案.md》§4.2 的 `desc.size >= sizeof(PluginDesc)` 门禁描述、以及 §4.3 的
>    "代价：插件回调不能是 async 函数"——由 D4/M2 修订；
> ④《补全方案.md》§0 与附录 A 的「**scope 体系不做**」——由 M4 修订
>    （改为：scope 原语做，per-service 的 thisArg/carrier 全量语义仍不做）；
> ⑤《补全方案.md》§6 M2-3 的「二期草图」三项——由 M1/M2/M2b 落地并扩充；
> ⑥《补全方案.md》§0 的「**ABI 暴露 waterfall 不做**」——由 D6/M2b 修订；
> ⑦《补全方案.md》§0 的「**key namespacing 不做**」——由 M6 修订。
>    （原文给出的理由"《方案.md》§10 已列"**不成立**：§10 从未提到 key namespacing。）

---

## 0. 目标与验收总纲

### 0.1 什么叫「完整的 SDK 骨架」，以及它的判据**有多强**

论文 §5 把 Cordis 分三层：**core library / component loader / application frameworks**，
并称前者为 meta-framework——*"prescribes no concrete scenario; its sole responsibility is to
supply universal dynamic composition semantics"*（p.57）。§5.3 用 Koishi 证明分层成立：
应用框架只贡献领域词汇，机制全在下面两层。

| 层 | crate | 现状 | 目标形态 |
|---|---|---|---|
| 内核 | `cordis-core` | ✅ 完整（零 unsafe） | 补 6 处小增量（intercept 消费点 / scope / 类型化读 / 取消令牌 / provision 声明 / 检查器） |
| 插件契约 | `cordis-abi` | ⚠️ 单入口 + 7 op + 11 host 函数 | **ABI v2**：**+8 个新增 host 函数** + 1 处载荷变更 + 1 个新状态码 |
| 插件作者面 | `cordis-abi::sdk` | ⚠️ 手写、JSON 化、仅服务 | 拆出 `cordis-sdk`（**保留 `cordis_abi::sdk` 路径可用**）；新增 `cordis-macros` |
| 宿主 | `cordis-host` | ✅ loader 完整 | + 能力清单 / 环诊断 / 原生注册表 / 派发策略 / 预挂载探测 |
| 电池 | — | ❌ | `cordis-components`：provider 原地替换（最小版） |
| 入口 | — | ❌ | `cordis-cli`：`inspect` / `validate` / `run` |

**判据（有界断言，且**逐条对应验收**）**：一个领域框架（harness）的领域服务、事件管道、
作用域、工具执行，可以只用文档化的内核/SDK 面表达，除了
**{M1 子组件挂载/隔离域/intercept、M2 入站异步、M2b waterfall、M2c 取消、
M4 scope、M5 provision+能力清单、M7 registry}** 与 **`cordis-cli`** 之外
**不需要新增内核机制**。清单里每一项都必须在 M11 的 spike 里被用到。
（v3 的清单漏了 M5 与 CLI——第 3 轮评审指出 spike 的 `provision`/`inspect` 行必用它们。）

### 0.2 验收总纲（可判定，11 条）

| # | 判据 | 判定方式 |
|---|---|---|
| A1 | `cargo test --workspace --offline` 全绿且**确定性**（连跑 3 次同结果） | `run-tests.cmd`；**断言一律自带隔离**，见 D10 |
| A2 | 既有 demo 断言**集合与输出快照**（`examples/demo/expected.txt`）逐字不变；demo/spike **新增 6 组** | `cargo run -p demo` + `cargo run -p harness-spike` |
| A3 | ABI 兼容矩阵：v1-SDK 插件 × v2 宿主 = 可加载且功能正常；v2-SDK × v1 宿主 = 可读降级且**结构上不可能越界** | `crates/host/tests/abi_matrix.rs`（宿主侧）+ `crates/abi` 单测（截断侧） |
| A4 | 一个**动态库**插件能：挂子插件、派生隔离域、设 intercept、自我卸载、做异步工具 | `cargo run -p harness-spike`（**不是** cargo test，见 D9） |
| A5 | 能力清单可在 **mount 之前**得到：`cordis-cli inspect` 打印每行的 inject/provision/volatile，且断言此时 `mounted == 0` | `crates/cli/tests/inspect.rs`（用 M5 的预挂载探测 seam） |
| A6 | 依赖环在装载期被**诊断**（带环路径），且**默认不拒挂**、不改变现有配置行为 | `loader_cycles.rs`（**手动驱动** `reconcile_once`） |
| A7 | 原地替换服务实现时**消费者 epoch 不变**，**且新值可见** | `crates/components/tests/registry.rs` + spike 断言 |
| A8 | 内核仍**零 unsafe**；`cordis-abi` 仍零平台依赖 | `#![forbid(unsafe_code)]` + grep |
| A9 | **typed 事件 + waterfall 拦截**：一个动态库插件能拦截 `tools/before-invoke` 并改写结果 | spike 二进制 |
| A10 | **取消**：调用方取消一次在飞派发 ⇒ 挂起的续延以错误结算；**且不残留**——`pending_len(handle) == 0` | `crates/host/tests/abi_m2c.rs`（取消桥）+ spike 二进制第 7 步（真 DLL 的挂起） |
| A11 | **有序持久日志**：spike 的 `sessions` 是 append-only、seq 连续；**一次 DLL 重载**后 seq 仍连续、可从日志恢复 | spike 二进制（载体钉死为 T2，见 M11） |

### 0.3 明确不做（含理由）

| 项 | 理由 |
|---|---|
| **流式（token stream）跨 ABI** | 骨架内流式提供者必须是 T1（D7） |
| WASM 全量载体 | 移出骨架（M8 = ADR） |
| DSU 式前向状态迁移 | 论文自认 future work（p.80） |
| 补偿（compensation）的元理论 | 论文 §6.1：必须对着比 `≃` 更粗的等价重证 |
| 结构化子类型兼容 | 论文 §6.6 自认 open；只做 key 命名空间 |
| 包管理器 / OCI registry | 只留 `resolve_plugin_path` + `NativeRegistry` 挂点 |
| 全量 `process.getBuiltinModule(...)` 等 JS 形态 | 保持 fail loud；`!!js` 能力化降为 P2 |
| **per-service 的 thisArg / carrier 全量 scope 语义** | 只做 `Filter` + `realm_filter` + `fiber_filter` + **父键关系**（M4） |
| 会话日志 / 子代理树 / MCP / 客户端插件图 / 凭据库 | 领域词汇，不进 SDK；A11 要求 spike 证明 SDK 不阻碍它 |
| 加权 broker（round-robin / least-loaded / drain） | 无消费者（M7 砍到最小）；消费者问题**已定案** → **ADR 0002** |
| 事件名命名空间化 | 事件名必须**逐字**（M6）；否则 ABI 侧硬编码的拦截名会静默失配 |

### 0.4 与《方案.md》§10 的对应

| 原 §10 条目 | 落点 |
|---|---|
| 1 真包管理器 | M1（挂点） |
| 2 热路径 vtable 化 | **不做**：与「方法名才是契约」相悖 |
| 3 配置 schema 校验与目录生成 | **P2**（M9b） |
| 4 `stage_plugin` 产物回收 | M0b |
| 5 `Weak` 传播审计 | M10b |
| 6 WASM 载体 | **移出骨架** → M8（ADR） |
| 7 ABI 兼容矩阵自动化 | A3（M0 夹具）+ M10c |

---

## 1. 差距清单

| # | 差距 | 证据（本仓，**已逐条核对**） | 论文位置（已校正） | 里程碑 |
|---|---|---|---|---|
| G1 | 动态插件不能做异步工具：入站签名同步，无「稍后结算」的 host 函数 | `sdk.rs:42-46`；`host/lib.rs:579` | §6.2（*"must be designed against an asynchronous contract"*）、§7 | **M2** |
| G2 | 动态插件是二等公民：不能挂子插件（Def 52）、不能 isolate/intercept、不能自我卸载 | `abi/lib.rs:43-69` 只有 7 op | §4.2.1（Def 52）、§5.1.2 | **M1** |
| G3 | **interception 是死机制** | `context.rs:119-141` 仅被测试引用 | §3.2.3（Def 26/27）、§6.3 | **M3** |
| G4 | 无 `Context.filter` ⇒ 多 agent / 会话 / 子代理树作用域缺失 | 只有**派发期**的 `realm_filter`/`fiber_filter` | **不是论文构造**；最接近的是 §3.2.3 隔离域。标为**上游对齐项** | **M4** |
| G5 | 无声明式 provision ⇒ 无法做环诊断 / 「声明即能力」/ Def 76 检查 | `store.rs:125-171` 无 `provision` | §6.5（*"a runtime **can** report it"*——能力非义务）、Def 48/72/76 | **M5** |
| G6 | 类型化依赖靠运行时 downcast；**值形态本身有缺陷** | `context.rs:156-161`；`dyn_value.rs:53/61` | §6.4 | **M6** |
| G7 | 无 provider 原地替换组件 | `store.rs:607-625` 是原料，无组件 | §6.2（broker 是*提议*；**消费者问题 → ADR 0002**） | **M7** |
| G8 | 数据服务跨 ABI 不校验形状 | `host/lib.rs:394` | §6.6 | M6 |
| G9 | `!!js` 内置子集是默认，配置可达 `process.env.*` / `ctx.get(...)` | `js.rs:9-25` | §6.3 | **M9（P2）** |
| G10 | 文档引用的论文版本偏移（且《原理.md》自身**内部不一致**：`:816` 已按新编号引 Thm 64、`:494/683` 又按 §4.4 引） | 《原理.md》 | — | M10a |
| G11 | `stage_plugin` 产物不回收；`libloading` 死声明 | `host/lib.rs:974`；`Cargo.toml:25` | — | M0b |
| G12 | 无 typed 事件面；**waterfall 不跨 ABI** | `sdk.rs:286-294` | §5.1.4 | **M2b + M6** |
| G13 | 无取消原语 | `event.rs:41`；`host/lib.rs:202-222` | §4.4、§6.2 | **M2c** |
| G14 | **尾追加破坏兼容**：`open()` 用 `size < size_of::<PluginDesc>()` 拒绝 | `host/lib.rs:777-783`；`sdk.rs:572` | — | **M0** |
| G15 | **`api!` 挨饿读**：`$host.$field` 作实参先求值，`field` 只挡返回值 ⇒ v2 插件在**真** v1 宿主上**越界读**；现有夹具 leak 全尺寸结构体、只改 `size`，测不出布局不匹配 | `sdk.rs:103-107` + `abi/lib.rs:239-245`；`sdk.rs:99-102` 的注释「不存在越界读」是**错的** | — | **M0** |
| G16 | `Instance` 是 `#[derive(Clone)]`，加裸 `Mutex` **编译不过** | `host/lib.rs:60-64`，`with_instance` 克隆（`:261`） | — | **M2** |
| G17 | **派发 id 与注册 id 混同**：`OP_CALLBACK` 的 `id` 是**注册时**的 cb（跨派发不变）⇒ 并发挂起会互相覆盖 | `sdk.rs:391-395` + `host/lib.rs:588-596` | — | **M2** |
| G18 | `Ctx::filter` 的子 context **无 fiber**，而 `Store::on` 要 `ctx.fiber()`（panic） | `store.rs:717`；`context.rs:66-68` | — | **M4** |
| G19 | 声明的最小 Rust 版本 **早已被违反**，且**不止一处**：`offset_of!`（1.77）、**`Waker::noop()`（1.85）** | `Cargo.toml:15` vs `sdk.rs:105/802/815`、`host/lib.rs:204` | — | **M0** |
| G20 | `ctx_dispatch` 没有 waterfall 模式；无「派发父子关系」⇒ 取消无法级联 | `host/lib.rs:638-715` | §4.4 | **M2c** |

---

## 2. 架构决策

### D1：三档 tier 与诚实的信任模型

| 档 | 载体 | 装卸 | 能力面 | 信任 | 用途 |
|---|---|---|---|---|---|
| T1 in-process native | Rust crate，链接进宿主 | 无 | **全量** | 完全信任 | 领域骨架组件、性能热点、**流式提供者**（D7） |
| T2 in-process dylib | `dlopen`/`LoadLibrary` | 丢文件即装 | ABI v2（附录 A） | **完全信任**（同地址空间） | 第三方组件（受审计） |
| T3 out-of-process / Wasm | 计划中（M8 = ADR） | 丢文件即装 | 同一套 op 协议 | **不提供沙箱** | 模型自生成组件（需 OS 边界，**SDK 不承诺**） |

**两条硬话**：① **模型自生成组件永远不得是 T2**；② **T3 是「载体可换」的证明，不是信任边界**
（参考实现自己也是这个立场）。
**载体可换性的依据归位**：论文的依据是**接口透明性**（§6.2/§6.3）与 §6.7 的**受限导入**；
**本仓自己的理由**（不是论文结论）是 ABI 的形状（单入口 + op 码 + 全部内存归宿主 + JSON）。
这条理由**只在 op 协议层面成立**。**T3 必须异步**（§6.2）⇒ M2 是 T3 的前置条件。

### D2：类型化 = 过程宏 + 能存进 `Dyn` 的值形态

**四个必须先修的缺陷（已核对）**：`async fn` 非 dyn ⇒ 宏**脱糖**；`dyn Trait` 需
`Send + Sync` ⇒ 宏**重写 trait 头**；`Dyn::shared` 收不了 `?Sized`、`Dyn::new(Arc<dyn T>)`
会得到 `Arc<Arc<dyn T>>` ⇒ 内核新增 `Iface<T: ?Sized + Send + Sync + 'static>(pub Arc<T>)`
（`Iface<dyn A>` 与 `Iface<dyn B>` 是不同 `TypeId`）；类型不匹配被伪装成 `NoInject`
（`context.rs:160`）⇒ 新增 `ResolveError::TypeMismatch`。

**宏是一对**：`#[cordis::service]`（trait）+ `#[cordis::service_impl]`（impl，**同样脱糖**）——
否则作者写的 `async fn` impl 与脱糖后的 trait 类型不符，「插件作者写普通 Rust」不成立。

**宏不能做的事（编译期报错，不静默）**：非 serde 参数/返回；返回不是 `Result<T, E: Display>`；
泛型方法 / `impl Trait` 返回（使 trait 非 dyn）。**参数信封**：JSON 数组 `[a1, a2, …]`，
方法名做键（`ExternService::call` 只传一个 JSON 值）。
**`'_` 与 `'static`**：`BoxFuture<T>` 是 `'static`（`dyn_value.rs:15`），脱糖方法借 `&self`
⇒ 适配器需一次 `Arc<Self>` 提升把 future 变成可持有形态。

**`TypeId` 边界（必须写）**：`TypeId` **不跨编译单元稳定** ⇒ typed 访问器**只在 T1 有效**；
T2/T3 走 JSON。**T1/T2 同名服务不可共存**（内核不变量：一个 key 一个实现域内唯一）⇒
**规则**：需要同时服务 T1 与 T2 的 key（如 `tools`），其 T1 侧实现持 `IfaceRef`
（`dyn_value.rs:112-114`）并由 T1 服务用 `call_iface` 转发给 T2 提供者。
**这条规则必须写进 SDK 指南**，否则 spike 会在运行期撞 `TypeMismatch`。

**对 §6.4 的正确读法**：该段论点是**访问中介**（宏把类型化声明与访问器一起产出、省掉一个
通用拦截原语），Rust 过程宏只是诸选项之一；`require_as::<Arc<dyn X>>` 是一次**类型化 downcast**，
**不是**论文说的无类型反射回退。措辞：「我们满足 §6.4 对宿主语言的要求」。
§6.4 的 "interception primitive"（访问中介）与 Def 26/27 的 interception（访问控制元数据）
**同名不同物**——写进 SDK 指南。

### D3：异步入站 = 「挂起 + 结算」，键是**每次派发的 id**

**决策**：入站回调可返回「挂起」，之后由 `ctx_fulfill` 结算。**同步路径零行为变更。**

| 问题 | 证据 | 修法 |
|---|---|---|
| 用**注册 id** 当挂起键 ⇒ 并发派发互相覆盖 | `sdk.rs:391-395` + `host/lib.rs:588-596` | **`dispatch_id`**：宿主在每次入站回调前铸造，随载荷下发；挂起表键为 **`(handle, dispatch_id)`**（所有清理路径也按这个键，**不许混用裸 id**） |
| `Instance` 加裸 `Mutex` 破坏 `Clone` | `host/lib.rs:60-64` | `pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Json>>>>` |
| `ctx_fulfill` 绕过代际校验 | `Slab::get`（`lib.rs:99-103`） | **必须经 `with_instance`**（`:253`）。已核对：闭包内**不持** slab 锁 ⇒ 在闭包里排 `pending` 安全 |
| `{"pending":true}` 与合法返回值冲突 | `host/lib.rs:600` | SDK 用 `AsyncReply::{Reply, Pending}`；**ABI 线上形态 = 新状态码 `STATUS_PENDING = 4`** |
| **插入时序竞态**：先调 `OP_CALLBACK` 再 `insert`，则插件在回调内部就 `fulfill`/`ctx_next` 会看不到条目 | 第 3 轮评审 | **先 `insert` 再调用**（`dispatch_id` 在调用前已铸造并随载荷下发） |

**清理路径的所有权表（必须严格实现；四条路径一律 `remove`-then-settle）**：

| 路径 | 拥有 | 动作 |
|---|---|---|
| `ctx_fulfill(handle, dispatch_id, payload)` | 一个键 | `remove` ⇒ `send`；二次调用返回可读错误 |
| 实例注销（`apply_via` 的 dispose effect，`host/lib.rs:909-924`） | 该实例全部 pending **+ 其 waterfall 帧** | **先 drain**（以「插件已卸载」结算）**再** `OP_DISPOSE` + `unregister` |
| `ctx_abort(handle, dispatch_id)`（M2c） | 该派发**及其后代派发**的全部续延 | `remove` 后以错误结算（必须移出，否则并发 `fulfill` 双结算） |
| 派发超时（可选） | 同上 | 同上 |

**取消桥（v3 缺失，第 3 轮评审的 P0）**：内核侧的 `CancelToken` 与宿主侧的 `pending` 是两张表，
必须显式接上——宿主在 await 挂起结果时用 **`tokio::select!`** 同时等
`rx` 与 `cancel_token.cancelled()`；取消先到 ⇒ `remove` + 以错误结算。
**光丢 future 不够**（drop `rx` 不会移出表项 ⇒ 泄漏）。
**父子级联**：`DispatchId` 带 `parent: Option<DispatchId>`；`ctx_abort` 递归到后代。
（`internal/*` 这类**原生发起**的派发也必须铸造 `dispatch_id`——统一规则，见 §5 risk 2。）

**接缝修正**：接口方法那条路的挂起点在 **`PluginIface::call`**（`host/lib.rs:477-497`），
不是 `host_ctx_provide_iface`（那是注册函数）。

**为什么忠实**：JS `events.ts` 对 `bail`/`serial` 是 `await listener(...)`——异步监听器**本来就被 await**。
**可选看门狗**：`DispatchPolicy { timeout_ms: Option<u64> }`，
归属 = `LoaderConfig` 字段（宿主级默认）+ `ctx_dispatch` 载荷按次覆盖；默认 `None`。

### D4：ABI v1 → v2 的纪律（**三处修正**）

**决策**：只**尾追加 `HostApi` 函数**（8 个），`abi_version` 仍为 1。

**修正一（G14）**：门禁改为**固定前缀**：
`PLUGIN_DESC_PREFIX_SIZE := offset_of!(PluginDesc, call) + size_of::<unsafe extern "C" fn(u64,u32,Blob,Blob,*mut Out)>()`
（写进 `crates/abi`；**不能用 `host::CallFn` 别名**——那是 host 侧类型，abi 里编不过）。
该常量写成常量 + 一条断言测试（`#[repr(C)]` 固定字段序 ⇒ 往 `call` 之后追加永不移动它）。

**修正二（G15，最严重）**：**改「惰性探测」为「挂载期拷贝」（copy-on-attach）**——
`attach` 时把宿主函数表**按 `host.size` 拷贝进插件自有缓冲并先零初始化**：
```rust
let mut local: HostApi = unsafe { std::mem::zeroed() };
let n = (host.size as usize).min(size_of::<HostApi>());
unsafe { ptr::copy_nonoverlapping(host as *const u8, &mut local as *mut _ as *mut u8, n) };
```
**为什么这一条比惰性探测更好**：① 拷贝后**任何**读都在自己的缓冲内 ⇒
「越界读」在**结构上不可能**（不是"恰好没崩"）；② `Option<fn>` 的空指针 niche 保证
零初始化 = `None` ⇒ 缺席字段天然是「宿主不支持」；③ 一次性，无每次调用的分支。
`field()` 仍保留 `size` 比较，用作**语义**区分（「有能力但本次不提供」的显式 `None`
vs 「宿主没有」）。**测试 oracle**：夹具用**真截断**分配（`MaybeUninit` 按 `size` 字节），
并在其后放**金丝雀**（断言未被改写）；`cargo +nightly miri test -p cordis-abi`
作为**附加**门禁（进 `run-tests.cmd` 的 `miri` 子命令，**不是** A1 的必要条件，见 D10）。
删掉 `sdk.rs:99-102` 的错误注释。
`PluginDesc::field_at` 用**同一形状**（只在 size 足够的分支里 `ptr::read_unaligned`）。

**修正三（G19）**：`rust-version = "1.85"`（`Waker::noop()` 的下限，比 `offset_of!` 的 1.77 更紧），
每个成员 crate 加 `rust-version.workspace = true`。

**其余规则**：`PluginDesc` 的字段是 `Blob` 不是 `Option<Blob>`；「有字段但没提供」= `Blob::EMPTY`；
`id`/`inject`/`provision` 都是插件侧 `Box::leak` 的静态内存，`Blob::as_str` 无界 ⇒
**只能在库存活期立刻拷贝**（`open` 现在就是这么做的）。`PluginDesc.provision` 的
**生产路径**（`build_plugin_with_provision` + `cordis_plugin!` 的一个 `provision:` 分支）
**必须在 M5 落地**——否则 M5 的 ABI 侧测试无对象可测（第 3 轮评审）。

### D5：`!!js` 能力化（**P2**）

静态提取 `process.env.<NAME>` / `process.platform` / `ctx.get('<name>')` 进 M5 的能力清单；
`LoaderConfig.interpolation: "builtin" | "injected"`（默认 `builtin`）。降 P2 因为它是
vendored TS loader 的兼容产物；保留因为它在参考实现里是 `new Function + with(ctx)`，**读得到 ctx**。
**能力清单里不得出现凭据值**（只带引用）。

### D6：waterfall 跨 ABI（**协议已按第 3 轮评审更正**）

```
宿主派发 waterfall：把「剩余链」按 dispatch_id 存在宿主侧
  call(OP_CALLBACK, {kind:"event", mode:"waterfall", dispatch_id, args})
        ┌─ 插件在**这次 OP_CALLBACK 内部**决定：直接返回 ⇒ 链到此为止（同步返回终值）
        └─ 插件要 next ⇒ 调 ctx_next(handle, dispatch_id, new_args, on_done)
                          （宿主立刻用一个内部任务续跑剩余链，**不阻塞本次回调**）
                        然后返回 STATUS_PENDING
          宿主续跑完成 ⇒ 回调插件 kind:"next"（带 down_stack 的值）
          插件拿到后 ctx_fulfill(dispatch_id, 终值)
```
**v3 的协议自相矛盾（第 3 轮评审 P0）**：v3 写「`ctx_next` 只允许在原派发返回
`STATUS_PENDING` 之后调用」——但插件在 `call` 返回后就没有代码在跑了，规则使机制不可达。
**正确规则**：`ctx_next` **在 `OP_CALLBACK` 内部调用**；`STATUS_PENDING` 只表示
「下游结果还没到，我会 fulfill」。宿主侧用一个内部任务推进剩余链（**同步快速路径**：
若剩余链全同步完成，则把结果**在本次回调返回前**交给 `on_done`，此时插件已经返回了
`PENDING`，仍需 `fulfill`——这一点必须在测试里钉死，避免"同步链也要一次往返"）。

三种形态都成立：**直接返回** / **原样转发** / **调 next 再改写**；与 JS `await next(args)` 对应。
键同样是 `dispatch_id`；**帧的清理并入 D3 的四条清理路径**（含取消与级联，v3 漏了这一点）。

### D7：骨架内**流式提供者只允许 T1**

harness 的 LLM 适配器返回 `AsyncIterable<StreamChunk>`；ABI v2 的异步入站是「一次挂起 + 一次结算」，
**表达不了流**。M11 的「换 LLM 适配器」= **原地替换一个 T1 适配器**。
**出路**：`ctx_next` 已跑通「反转控制流」，流式 = 推广成 N 次回调 + 背压窗口 → M8 的 ADR。

### D8：`pub` seam 纪律（沿用《补全方案.md》§3.2）

先落 seam 再写测试；凡 `crates/*/tests/*.rs` 要用的项一律 `pub`。

| seam | 现状 | 归属 |
|---|---|---|
| `host::CallFn`、`host::apply_via`、`host::host_api()` | ✅ 已有 | host |
| **`host::desc_gate(abi_version, size, expected_prefix) -> Result<()>`**（从 `open` 里抽出，纯函数） | ❌ 新增 | **M0** |
| **`host::set_native_registry` + `NativeRegistry` trait**（**host 进程级全局 + pub setter**——`App` 在 `crates/core`，host 类型不能做它的字段） | ❌ 新增 | M1 |
| **派生句柄位布局常量** | ❌ 新增 | **M1** |
| `host::DispatchPolicy` | ❌ 新增 | M2 |
| **`host::pending_len(handle) -> usize`**（A10 的观测口） | ❌ 新增 | **M2** |
| `abi::{STATUS_PENDING, PluginDesc::field_at, PLUGIN_DESC_PREFIX_SIZE}` | ❌ 新增 | M0/M2 |
| `abi::sdk::{AsyncReply, Ctx::{on_async, fulfill, next}}` | ❌ 新增 | M2/M2b |
| `core::Iface<T>`、`ResolveError::TypeMismatch` | ❌ 新增 | M6 |
| `core::Plugin::provision`、`Store::check_provision` | ❌ 新增 | M5 |
| **`host::probe_capabilities(path) -> Capabilities`**（**load-without-mount**，A5 的机制） | ❌ 新增 | **M5** |
| `core::Ctx::{filter, in_scope_of, bind_scope_parent}`、`Store::hook_scopes`（+ `Hook.scope` 字段，经 `event::owned` 构造） | ❌ 新增 | M4 |
| `core::CancelToken`、`Call.cancel` | ❌ 新增 | M2c |
| `loader::report.{cycles, capabilities, stagedLeaked}` | 部分已有 → 补字段 | M0b/M5 |
| `components::registry` 服务 | ❌ 新增 | M7 |
| `host::Slab`/`Instance`/`with_instance` | **保持私有**（双表 + `pending_len` 让它不必暴露） | — |

**夹具铁律**：fake `CallFn` 内**禁止 `assert!`**；`instances()`/`HOST_API` 是**进程级**静态 ⇒
同一二进制内测试共享句柄表，断言计数前必须隔离（D10）；`thread_local!` 夹具**跨线程读不到**。

### D9：真 DLL / HMR 断言**只放 demo 与 spike 二进制**

《补全方案.md》§3.2 的规矩。A4/A9/A11、M0b 的**实际删除**、M11 的 HMR 重载 ⇒ 二进制断言。
`cargo test` 只保留**无 DLL** 的路径（fake `CallFn` + `apply_via` + 假 `PluginSource`）。

### D10：测试基建（**v3 缺失，第 3 轮评审的横切项**）

| 项 | 问题 | 做法 |
|---|---|---|
| **MSRV** | `rust-version` 与实际不符（G19） | M0 改 `1.85`；`rust-version.workspace = true` |
| **tokio `test-util`** | `Cargo.toml:21` 的 features 没有它，M2 t10 的 `time::pause()` 用不了 | M0 给 workspace 依赖加 `test-util` |
| **越界读 oracle** | 光靠"没崩"不可判定 | D4 的 copy-on-attach（结构上不可能）+ 金丝雀 + **可选** `miri` 子命令 |
| **句柄计数竞态** | `instances()` 是进程级；同一 `tests/*.rs` 二进制的多线程测试会互相干扰 | 规则：**不断言全局绝对计数**；改用 `pending_len(handle)`/`loaded_instances()` 的**差值**且在同一 `#[tokio::test]` 内串行完成；需要绝对计数时用 `serial` 语义（单测试文件 + `--test-threads=1` 的专用命令） |
| **A2 基线** | "逐字不变"无基线 | M0 产出 `examples/demo/expected.txt` 快照 + 比较脚本 |
| **spike 前置构建** | `cargo run -p harness-spike` **不会**构建 cdylib（`demo` 也不依赖插件 crate，见 `方案.md:577-579`） | M11 文档写死：先 `cargo build -p plugin-harness-tool`；`[lib] name = "harness_tool"`（避免 `方案.md:522` 记录的 `plugin_greeting.dll` 陷阱）；插件目录取自 exe 目录而非 cwd |

---

## 3. 里程碑总览

```
M0（基线固化 + 闸门 + copy-on-attach + MSRV + 测试基建 + workspace 成员）
 ├─ M0b（StagedFile 回收 + 死声明清理）
 └─ M1（ABI 能力五项 + NativeRegistry + 派生句柄位布局）
      ├─ M2（入站异步：dispatch_id + STATUS_PENDING + ctx_fulfill + pending_len）
      │    ├─ M2b（waterfall：ctx_next，协议见 D6）
      │    └─ M2c（取消：ctx_abort + Call.cancel + 父子级联 + 取消桥）
      ├─ M3（intercept 消费点；`ExternService::call` 在此一次定稿）
      ├─ M4（scope/Filter：新原语 + 父键关系 + 子 context 带 fiber）
      ├─ M5（provision 声明 + ABI 生产路径 + 能力清单 + 预挂载探测 + 环诊断）
      │    └─ M6（cordis-macros：typed 服务/事件/配置 + service_impl）
      │         └─ M7（provider 原地替换，最小版）
      ├─ M9（配置层能力化，**P2**）
      ├─ M10（文档重整 + Weak 审计 + 兼容矩阵 + MSRV/特性矩阵）
      └─ M11（harness spike：总验收；DLL/HMR 断言在 spike 二进制）
M8（第二载体）→ 移出骨架，产出 ADR（含 D7 的流式推广路径）
```

**执行顺序**：`M0 → M0b → M1 → M2 → M2b → M2c → M3 → M4 → M5 → M6 → M7 → M9 → M10 → M11`。

**M0 落 `Cargo.toml` 的 workspace 成员**：`crates/sdk`、`crates/macros`、`crates/components`、
`crates/cli`、`examples/harness-spike`、`examples/plugins/harness-tool`。
**路径统一**：cdylib 目录 `examples/plugins/harness-tool`；二进制 `examples/harness-spike`。

**规模估计**：生产 ~4.8k / 测试 ~4.2k（净新增）。

---

## 4. 里程碑明细

### M0：基线固化 + 闸门 + copy-on-attach + 测试基建（前置）

**八件事**（前 4 项是"修既有缺陷"，后 4 项是"基线"）：
1. **G19**：`rust-version = "1.85"`；成员 crate 加 `rust-version.workspace = true`；
   workspace 依赖给 tokio 加 `test-util`。
2. **G14**：`open()` 的门禁抽成纯函数 **`host::desc_gate(abi_version, size, expected_prefix)`**（D8），
   门禁用 `PLUGIN_DESC_PREFIX_SIZE`；**同时**在 `PluginDesc` 尾追加 `provision: Blob`，
   并在 `sdk.rs` 的 `build_plugin_with_volatile` 字面量里写 `provision: Blob::EMPTY`
   （否则 `#[repr(C)]` 字面量编译不过）。**这一步让 M0 的红测试真的能红。**
3. **G15**：`attach` 改为 **copy-on-attach**（D4）+ 删错误注释；`crates/abi` 夹具改真截断 + 金丝雀。
4. **D10**：`examples/demo/expected.txt` 快照 + 比较脚本；`run-tests.cmd` 增加**可选** `miri` 子命令。
5. v1 夹具：固化 11 字段 + `PluginDesc` 字段序 + 手写 `call` 的 v1 插件。
6. demo 断言抽成 `assertions.rs`。
7. workspace 成员登记（见 §3）。
8. 记录基线：用例清单与计数进 `crates/abi/tests/baseline.txt`（断言**只增不减**）。

**红测试（3 条，全部可红、全部无 DLL）**：
1. `desc_gate_rejects_a_prefix_only_desc_under_the_old_rule_and_accepts_it_under_the_new`
   （纯函数，直接喂 `(1, 72, 72)`；**现在红**——第 2 项落地后旧门禁会拒它）
2. `truncated_host_api_reads_only_inside_its_own_copy`（真截断 + 金丝雀；**现在红**）
3. `plugin_desc_prefix_size_is_frozen`（断言常量 == 手算值）

### M0b：产物回收与死声明清理

- **落地**：**不要在 `LoadedPlugin::drop` 里删文件**——有 `Drop` 的类型，`drop(&mut self)` **先跑**，
  字段才按声明序 drop ⇒ 写在 `LoadedPlugin::drop` 里的删除会**早于** `FreeLibrary`。
  正确做法：`struct StagedFile(PathBuf)`（**自己**实现 `Drop` 做 `remove_file`，失败只记日志），
  作为字段声明在 **`_library` 之后**。`LoadedFile.staged` 的类型**不变**（`PathBuf`，纯记账）
  ⇒ 4 个外部夹具 + 1 个 crate 内字面量**都不需要改**（v3 说"要同步 4 个"是**过渡说法**，§4 M0b 以本行为准）。
- **红测试（1 红 + 1 守卫）**：
  1. `staged_leak_counter_is_exposed_and_reaches_zero_on_success`（红，纯记账，无 DLL）
  2. `staged_file_survives_while_the_rollback_record_holds_the_plugin`（**守卫**，本就绿）
  3. 实际删除 ⇒ **demo 断言**（D9）
- **附**：删 `Cargo.toml:25` 的 `libloading`。

### M1：ABI 能力五项（G2）

- **ABI（尾追加 5 个）**：`ctx_isolate` / `ctx_intercept` / `ctx_use` / `ctx_dispose` / `ctx_dispose_self`。
- **句柄位布局（精确定义）**：实例 = `bit63=0 | bits[62:32]=index（insert 里 assert index < 2^31）| bits[31:0]=generation`；
  派生 = `bit63=1 | bits[62:0]=单调 seq（从 1 起，永不复用）`；`split()` 先掩 bit63；
  解析顺序 = 先实例表（bit63=1 必然 miss），后派生表；`unregister_instance(root)` 排水 `owner == root`。
- **`ctx_use`**：`url` 交 `PluginSource`；`NativeRegistry` = **host 进程级全局 + pub setter**（D8）；
  `url = "@native:<name>"`；新 fiber 是调用方 fiber 的一个 effect。
- **红测试（8 条 host + 1 条 abi）**：`abi_isolate_derives_a_realm_and_returns_a_usable_handle`、
  `derived_handle_is_revoked_when_the_root_instance_unregisters`、
  `derived_handle_does_not_receive_op_dispose`、`a_derived_handle_never_resolves_as_an_instance`、
  `abi_use_mounts_a_child_component_and_disposes_it_with_the_parent`（假 `PluginSource`，**无 DLL**）、
  `abi_use_resolves_native_components_by_at_name`、
  `abi_use_from_inside_a_callback_does_not_deadlock`、
  `abi_dispose_self_marks_the_loader_row_disabled_and_persists`；
  `abi_isolate_degrades_on_a_truncated_host_api`（**归 `crates/abi` 单测**）。

### M2：入站异步（G1/G16/G17）

- **ABI**：`ctx_fulfill(handle, dispatch_id, payload, out)`；**`STATUS_PENDING = 4`**。
- **落地**：D3 的五条修法 + 清理路径所有权表（四条路径一律 `remove`-then-settle，
  键一律 `(handle, dispatch_id)`，**统一大小写**：表里所有 `remove` 都按该键）。
  **先 insert 再调用**。接缝 = `host_ctx_on`（事件）+ **`PluginIface::call`**（接口方法）。
  `dispatch_id` **可以在 host 侧铸造**（两个入站回调都在 host）⇒ core 不需要改。
- **红测试（8 + 2 守卫）**：`pending_inbound_continuation_is_settled_exactly_once`、
  `two_concurrent_dispatches_of_the_same_handler_settle_independently`（G17）、
  `ids_do_not_collide_across_two_plugin_libraries`、
  `unregistering_an_instance_fails_its_pending_continuations`（断言 **`pending_len` 归零**）、
  `fulfill_on_a_stale_handle_is_rejected`、
  `fulfill_goes_through_with_instance_and_respects_generation`（守卫，**计入守卫列**）、
  `status_pending_is_not_confused_with_a_json_return_value`、
  `pending_is_observed_before_the_dispatch_returns`（**确定性**：手动 oneshot + channel 记序，**不比墙钟**）、
  `sync_handler_path_is_unchanged`（守卫）、
  `dispatch_policy_timeout_settles_a_never_fulfilled_continuation_as_timeout`（**`tokio::time::pause()`**）。
- **夹具注记**：跨线程测试用 `Arc<Mutex>`/channel，不用 `thread_local!`。

### M2b：waterfall 跨 ABI（G12，D6）

- **ABI（尾追加 1 个）**：`ctx_next(handle, dispatch_id, args, on_done, out)`。
- **协议**：见 D6（**在 `OP_CALLBACK` 内部调用**；`PENDING` 表示"结果稍后到"）。
- **红测试（7 + 1 守卫）**：`abi_handler_can_return_a_final_value_and_stop_the_chain`、
  `abi_handler_can_delegate_to_next_verbatim`、`abi_handler_can_wrap_the_next_result`、
  `next_arguments_replace_the_downstream_arguments`、`chain_tail_runs_when_the_last_handler_delegates`、
  `a_fully_synchronous_chain_still_requires_exactly_one_pending_round_trip`（钉死 D6 的同步路径语义）、
  `unfinished_waterfall_frame_is_cleaned_on_unregister`、
  `plain_native_waterfall_is_unchanged`（守卫）。

### M2c：取消（G13/G20）

- **ABI**：`ctx_dispatch` 的 `out` 增 `dispatch_id`；`ctx_abort(handle, dispatch_id, out)`。
- **内核接缝（第 3 轮评审纠正）**：令牌放 **`Call`**（`event.rs:41`）新增字段 `cancel: Option<CancelToken>`。
  **不改 `Store::dispatch` 的签名**——另加一个入口
  （`Store::dispatch_cancellable(ctx, mode, args, cancel)`），旧入口语义不变 ⇒
  **0 个测试文件需要改**（若改签名，`event_semantics.rs` 5 处 + `extension_points.rs` 1 处要改）。
- **取消桥 + 级联**：见 D3（`select!` + `DispatchId.parent`）。
- **红测试（6 条）**：`aborting_a_dispatch_settles_its_pending_continuations_as_failure`、
  `fulfill_after_abort_is_rejected`（靠 `remove`）、
  `cancel_token_is_visible_to_native_handlers_via_call`、
  `aborting_a_parent_dispatch_cancels_its_child_dispatches`（**G20**）、
  `aborting_an_unknown_dispatch_id_is_a_readable_error`、`abort_does_not_affect_other_dispatch_ids`。

### M3：intercept 消费点（G3）——语义按论文校正

- **论文形态**：提供者侧一个实参 `σ(k)( d(k) ⊕_k ι(k) )`，**右偏**（context 携带的 `ι` 覆盖
  组件声明的 `d`，p.897-902）；§6.3 说**提供者**在被调用时查阅它（p.3484-3485）；
  链条优先级 = 最近 intercept 层 > 祖先层 > 组件声明。
- **两条通道**：① 提供者侧（论文的）——`Context::call_iface`（`context.rs:208`，
  **不存在 `Store::call_iface`**）派发前算 `merge(d_caller, ι_caller)`，经 ABI 载荷可选字段下发；
  ② 引擎便利（**不是 Def 26 的 d(k)**）——`ResolvedConfig { plugin, services }`。
- **`ExternService::call` 一次定稿**：`call(&self, caller_meta: Option<Config>, method: String, args: Config)`。
  **波及面（第 3 轮评审要求点名）**：`PluginIface`（`host/lib.rs:464-497`）、
  测试实现 `Greeter`（`core/tests/kernel.rs:434-444`）、调用点 `context.rs:245`。
- **红测试（7 条）**：`providers_see_the_merged_metadata_of_declaration_and_context`、
  `context_carried_metadata_overrides_the_component_declaration`、
  `nearest_intercept_layer_wins_over_an_ancestor`、`use_site_metadata_reaches_the_provider_at_call_time`、
  `changing_intercept_does_not_restart_the_fiber`、`service_config_reaches_the_plugin_through_resolved_config`、
  `intercept_metadata_survives_a_provider_replacement`。

### M4：scope / Filter（G4/G18）——新原语，不是「统一既有两轴」

- **既有两轴一律不动**；`Ctx::filter` 派生的子 context **必须继承/附加调用者的 fiber**（G18）。
- **`Hook.scope` 字段的构造点必须点名**：`event::owned`（`event.rs:200`），
  两个调用点 `store.rs:426`（`builtin_hook`）与 `store.rs:719`（`Store::on`）——三处同改。
- **接口**：`filter` / `in_scope_of` / `bind_scope_parent` + `Store::hook_scopes(name)`；
  `ScopeAnchor` 只持 `Weak<Fiber>`/uid（防环）。
- **红测试（5 + 1 守卫）**：`filtered_context_can_register_events_and_carries_the_fiber`（G18）、
  `service_method_can_observe_the_callers_scope`、`scope_parent_chain_is_queryable_and_rebindable`、
  `scope_parent_cycle_is_rejected`、`realm_and_fiber_dispatch_filters_are_unaffected`（守卫）、
  `scope_survives_the_anchor_fiber_going_away`。

### M5：声明式 provision + 能力清单 + 预挂载探测 + 环诊断（G5）

- **引用校正**：§6.5 是 *"a runtime **can** report it"*（能力非义务）；Thm 73 的 acyclicity 是**前提**
  （并经 Lemma 77 进入 Thm 80），不是报告义务的来源；`strict` 拒挂是本仓扩展（默认关）。
- **落地**：`Plugin::provision`（默认空）+ `PluginDesc.provision` **的 ABI 生产路径**
  （`build_plugin_with_provision` + `cordis_plugin!` 的 `provision:` 分支——**必须在本里程碑落地**）
  + `Store::check_provision` + **`host::probe_capabilities(path)`（load-without-mount，A5 的机制）**
  + 环诊断（SCC ⇒ `report.cycles`，**带环路径**；默认只报告）+ `Capabilities`。
- **两条必须写死的规则**：
  1. **「声明为空 = 不透明 = 跳过检查」紧挨报告构造器写**——现有两个示例插件都不声明 provision；
     若读成「声明了空集」，Def 48 会对每个既有插件报警，demo 断言全红。
  2. **key 命名空间的调和**：M6 之后 key 是 `<name>@<major>`，而 `provision`/`impls_owned_by`
     今天是**裸名** ⇒ 必须同时规定 provision 用**同一命名规则**，或在命名空间启用前把检查置为跳过。
- **红测试（9 + 1 守卫）**：`declared_provision_is_reported_per_fiber`、
  `installing_a_key_outside_the_declaration_is_reported`（Def 48）、
  `finishing_activation_without_installing_a_declared_key_is_reported`（Def 76）、
  `dependency_cycle_is_reported_with_its_path`、
  `cycle_does_not_prevent_the_rest_of_the_tree_from_mounting`（**手动驱动 `reconcile_once`**）、
  `strict_mode_turns_a_cycle_into_a_load_failure`、`acyclic_precedence_is_never_reported`（守卫）、
  `capabilities_report_aggregates_inject_provision_and_volatile`、
  `abi_plugin_can_declare_provision_through_the_build_path`、
  `probe_capabilities_reads_provision_without_mounting`（**A5**）。
- **与第 1 轮评审的分歧记录**：R3-P1-6 主张砍掉环检测；裁决为保留 `provision` 声明 + 预挂载探测
  （R3 亦认可能力清单是真实需求），**环检测降为默认只报告**（零回归）。

### M6：`cordis-macros`（G6/G8/G12）

- **crate**：`crates/macros` + `crates/sdk`。
  **`cordis-sdk` 是**新增**crate，纯增量**——`cordis-abi` 的 `sdk` 模块**保留不动**
  （示例插件 `use cordis_abi::sdk` 不受影响）。v3 说"必须 re-export 否则编译失败"的**理由是错的**。
- **四个宏**：`#[cordis::service]`（trait）+ `#[cordis::service_impl]`（impl）+ `#[cordis::events]`
  （**事件名逐字，不命名空间化**）+ `#[cordis::plugin(inject, provision, volatile)]` + `#[cordis::config]`。
- **内核新增**：`Iface<T>`、`ResolveError::TypeMismatch`。
- **红测试（8 条）**：`service_macro_generates_key_and_typed_accessor`、
  `async_service_methods_are_dyn_compatible`（编译即断言）、
  `service_impl_macro_lets_authors_write_async_fn`、
  `typed_accessor_and_json_adapter_dispatch_to_the_same_impl`、
  `typed_accessor_reports_type_mismatch_not_noinject`、
  `plugin_macro_emits_inject_provision_and_volatile`、
  `events_macro_generates_verbatim_names_and_dispatch_modes`、
  `key_namespacing_prevents_collision_between_two_tools_at_v1`。
- **注**：`crates/abi` 的 `HOST`/`APPLY`/`VOLATILE_PATHS` 是进程级 `OnceLock`
  ⇒ 新增 SDK 测试若调用 `build_plugin` 会与既有测试**同进程冲突**（既有测试已有一条注释警告）。
  规则：需要 `build_plugin` 的测试集中到**一个**测试文件里串行跑。

### M7：provider 原地替换组件（G7，最小版）

- **接口**：`#[cordis::service(name = "registry", version = 1)] trait Registry { put/replace/get }`。
- **必须建立在 `Store::set` 语义上**（原地换值、`Impl::provider_uid` 不变）⇒ epoch 才不变。
- **红测试（4 条）**：`consumer_epoch_is_unchanged_when_a_named_impl_is_replaced`（A7，
  **同时断言新值可见**，否则空实现也能过）、`replacing_is_a_single_synchronous_write`、
  `disposer_removes_the_entry`、`empty_registry_degrades_gracefully`。

### M8：第二载体 —— 移出骨架，产出 ADR

**✅ 已产出**：`docs/adr/0001-second-carrier.md`（① `LoadedPlugin` 是唯一适配器、`cordis-abi`
零平台依赖，四个 seam 逐条点名；② D1 的诚实边界；③ **D7 的流式推广路径**（N 次回调 +
背压窗口，含将来实现的验收口径）；④ T3 的接口契约必须异步（含待补的 `AsyncIfaceFn`）。

### M9：配置层（**P2**）

`!!js` 能力化（D5）。**红测试 3 + 1 守卫（具名）**：`js_expression_capabilities_are_extracted_and_reported`、
`interpolation_injected_mode_rejects_an_unevaluated_expression`、
`interpolation_builtin_mode_is_unchanged`（守卫）、
`credential_values_never_appear_in_the_capability_snapshot`。M9b（schema）保留为 P2 尾项。

### M10：文档 + Weak 审计 + 兼容矩阵

- **M10a（纯文档，不计测试）**：论文编号对照表（59→64 / 61→68 / 63→70 / 66→73 / 73→80；页号 −3；
  Metatheory §4.4→§4.3；DSU future work 在 p.80），逐条修正《原理.md》的 `(p.NN)`，
  **并处理《原理.md》自身的内部不一致**（`:816` 已用新编号、`:494/683` 又按 §4.4 引）；
  把 §0 取代清单同步进《方案.md》/《补全方案.md》。
- **M10b（1 条）**：`ctx_is_not_leaked_after_fiber_dispose`（`debug_assertions` + `Weak::strong_count == 0`）。
- **M10c（1 条）**：`abi_matrix.rs`（宿主侧）+ 截断侧在 `crates/abi`。
- **产物**：`SDK指南.md`（三档 tier、两张同名不同物的 interception、**T1/T2 typed 边界规则**、
  语义陷阱、TS 移植对照、**MSRV/特性矩阵**）、`兼容矩阵.md`（自动生成）。

### M11：harness spike（**总验收**）

- **两层落地（D9 + D10）**：
  - **`cargo test`（无 DLL）**：`examples/harness-spike/tests/e2e.rs`——scope 隔离、
    typed in-process 调用、typed 事件的类型检查、replay 适配器的确定性、registry 的 epoch 不变。
  - **`cargo run -p harness-spike`（真 DLL + HMR）**：A4 / A9 / A11。
    **前置**：`cargo build -p plugin-harness-tool`（文档写死）+ `[lib] name = "harness_tool"`。
- **能力表**：

| spike 里的写法 | 依赖 | 验收 |
|---|---|---|
| `#[cordis::plugin(inject(tools, llm, registry), provision(agentLoop))]` | M6/M5 | — |
| 三个服务用 `#[cordis::service]`；in-process typed 调用 | M6 | — |
| `#[cordis::events]` 定义 `agent/pre-step`、`tools/before-invoke`（waterfall，**名逐字**） | M6/M2b | A9 |
| 一个**动态库**插件拦截 `tools/before-invoke`（调 next 后改写） | M2b | **A9** |
| 每 agent 一个 scope；两 agent 同名工具互不可见；子代理 `bind_scope_parent` | M4 | — |
| 一个动态库插件提供**异步**工具（挂起后 fulfill） | M2 | A4 |
| 调用方**取消**一次在飞工具调用 ⇒ 挂起续延以错误结算，`pending_len == 0` | M2c | **A10** |
| **原地**换 `llm` 适配器（T1）⇒ agent loop 的 epoch 不变 | M7 | **A7** |
| 动态库插件挂一个 `@native:` 子组件 | M1 | A4 |
| 工具的 T1 服务持 `IfaceRef` 并把 T2 提供者的注册转发进来（**D2 的 T1/T2 规则**） | M6 | — |
| `cordis-cli inspect` 在 mount 前输出能力清单；未声明 provision 的行**被列出** | M5(+CLI) | **A5** |
| `sessions`（**T2/DLL**）：append-only、seq 连续、flush 屏障（**做成一个 effect**，不是 spawn） | 领域 | **A11** |
| 一个 replay 适配器（读录制的 chunk 序列）驱动 agent loop 的确定性测试 | 领域/测试接缝 | A11 |

- **A11 的载体必须钉死为 T2**（DLL 重载会换出新的插件对象）：若 `sessions` 是 T1，
  loader 的重载复用同一个 `Arc<dyn Plugin>`（`loader.rs:175/1785`），
  内存里的 `next_seq` 天然连续 ⇒ **假通过**。触发方式也必须确定（触碰插件文件 + 手动驱动 `reload_one`，
  不走墙钟 HMR）。
- **注**：`sessions`/`replay` 是领域词汇（不进 SDK）；A11 断言它们在 SDK 上可表达，
  且一次 **DLL 重载**后 seq 仍连续。「未声明 provision 的行**被列出**」而非「被拒」——
  与 M5 / §5-risk-5 的默认关一致。

---

## 5. 风险与开放问题

1. **挂起无超时**（默认）⇒ 行为不端的插件可占住派发。缓解：`DispatchPolicy` + 卸载/取消排水。
   **开放**：看门狗是否默认开。
2. **`internal/*` 等原生发起派发的 `dispatch_id` 铸造**：D3 规定"统一铸造"，
   但 `Store::notify` 里的 `internal/service` 是 spawn 的 Emit ⇒ 需要一个统一入口。
   **开放**：是否所有入站回调都走同一铸造点。
3. **`ctx_use` 的重入**：只建 fiber 不 await。**开放**：是否需要「挂载点」概念。
4. **派生句柄位空间**：bit63 + 63 位单调 seq；`index < 2^31` 断言。
   **开放**：派生句柄是否也应带代际号。
5. **`provision` 是自愿的**。**开放**：harness 是否有权**拒绝**未声明 provision 的第三方组件
   （开关默认关）。
6. **key 带 major 版本** ⇒ 版本不匹配表现为「停在 Pending」。**开放**：是否加历史 key 诊断。
7. **proc-macro 的编译期成本**；不回退手写双轨。
8. **T3 不是信任边界**（D1）。
9. **Confluence 的前提被显式化**（M5）：默认只报告 ⇒ 零回归；`strict` 引入新失败模式（默认关）。
10. **waterfall 帧泄漏**：缓解 = 帧与 `dispatch_id` 同生命周期 + 四条清理路径 + 超时。
    **开放**：是否需要帧上限。
11. **`Call.cancel` 的公开再导出**（`core/lib.rs:32` 导出 `Call`）必须源码兼容；
    新入口 `dispatch_cancellable` 保证旧入口与**全部既有测试**不改。
12. **T1/T2 同名服务的边界规则**（D2）：需要同时服务两档时的转发模式必须写进 SDK 指南，
    否则会在运行期撞 `TypeMismatch`。

---

## 6. 验收与回归矩阵

| 里程碑 | 红测试 | 其中守卫 | demo/spike 新增组 | 兼容性关键点 | 验收 |
|---|---|---|---|---|---|
| M0 | 3 | 0 | 0 | v1 插件在 v2 宿主可加载；越界读**结构上不可能** | A1/A3 |
| M0b | 1 | 1 | 0 | — | A2 |
| M1 | 9 | 0 | +1 | 旧宿主 ⇒ 可读降级 | A2/A4 |
| M2 | 8 | 2 | +1 | 同步路径零变更 | A2/A4/A10 |
| M2b | 7 | 1 | +1 | 原生 waterfall 不变 | A2/A9 |
| M2c | 6 | 0 | +1 | 旧入口与既有测试零改动 | A2/A10 |
| M3 | 7 | 0 | 0 | `ctx_call_iface` 载荷向前兼容 | — |
| M4 | 5 | 1 | +1 | 派发期过滤零变更 | A2 |
| M5 | 9 | 1 | 0 | **默认零回归**（只报告） | A5/A6 |
| M6 | 8 | 0 | 0 | `cordis-abi::sdk` 路径保留 | — |
| M7 | 4 | 0 | 0 | — | A7 |
| M9 | 3 | 1 | 0 | `builtin` 模式零变更 | — |
| M10 | 2 | 0 | 0 | 自动矩阵（M10a 纯文档） | A3 |
| M11 | 5 | 0 | +1（spike 二进制：A4/A9/A11） | — | A2/A4/A5/A7/A9/A10/A11 |
| **合计** | **77** | **7** | **+6 组** | | |

> 「红」= 引入该测试的提交里**必须先失败**（编译期红亦可）。
> 「守卫」= 本就绿、防回归，**单独计数、不冒充红**。

**回归纪律**：先写会失败的红测试；红证据可复现；同一提交内改写既有测试必须**具名列出**；
**真 DLL/HMR 断言只进 demo/spike**（D9）；**时序断言一律 `tokio::time::pause()`/手动 oneshot**；
**不断言全局绝对句柄计数**（D10）。

---

## 附录 A：ABI v2 能力表（HostApi 尾追加 8 个 + 1 处载荷变更，`abi_version` 仍为 1）

| # | 字段 | 签名 | 语义 | 旧宿主（**copy-on-attach 后**） | 旧插件 |
|---|---|---|---|---|---|
| 12 | `ctx_isolate` | `(ctx, key, label, out) -> i32` | 派生 realm 子 context | 拷贝按 `host.size` 截断 ⇒ 零初始化 ⇒ `None` ⇒ SDK 报「宿主不支持」；**不触宿主内存** | 从不读 ⇒ 无影响 |
| 13 | `ctx_intercept` | `(ctx, key, metadata, out) -> i32` | 派生 intercept 子 context | 同上 | 同上 |
| 14 | `ctx_use` | `(ctx, url, config, out) -> i32` | 挂子组件（`PluginSource`/`@native:`） | 同上 | 同上 |
| 15 | `ctx_dispose` | `(ctx, target, out) -> i32` | 回收派生 ctx 句柄或子 fiber 句柄 | 同上 | 同上 |
| 16 | `ctx_dispose_self` | `(ctx, out) -> i32` | 卸载调用者自己 | 同上 | 同上 |
| 17 | `ctx_fulfill` | `(ctx, dispatch_id, payload, out) -> i32` | 结算一个返回过 `STATUS_PENDING` 的入站续延 | 同上 | 同上 |
| 18 | `ctx_next` | `(ctx, dispatch_id, args, on_done, out) -> i32` | 续跑 waterfall 剩余链 | 同上 | 同上 |
| 19 | `ctx_abort` | `(ctx, dispatch_id, out) -> i32` | 取消一次在飞派发（含后代） | 同上 | 同上 |
| — | `ctx_dispatch`（既有，**载荷变更**） | `(…) -> i32` | `out` 载荷**新增** `dispatch_id` | 插件忽略未知字段 | **旧宿主不返回 `dispatch_id` ⇒ SDK 必须禁用 `ctx_abort`** 并给可读错误，不得假装配到 id |

**新状态码**：`STATUS_OK=0` / `STATUS_ERR=1` / `STATUS_TOO_SMALL=2` / `STATUS_UNSUPPORTED=3` /
**`STATUS_PENDING=4`**（仅 `OP_CALLBACK` 的返回码使用）。

`PluginDesc` 尾追加（**M0 落地字段，M5 落地生产路径**）：

| 字段 | 类型 | 语义 | 读法 |
|---|---|---|---|
| `provision` | `Blob`（JSON 数组） | 组件声明自己能提供的 key | `PluginDesc::field_at`：`size` 不足 ⇒ `None`（未声明）；`Blob::EMPTY` ⇒ 「有字段但没提供」；只在 size 足够的分支里 `ptr::read_unaligned`；**只能在库存活期立刻拷贝** |

**前缀常量**：`PLUGIN_DESC_PREFIX_SIZE := offset_of!(PluginDesc, call) + size_of::<unsafe extern "C" fn(u64,u32,Blob,Blob,*mut Out)>()`（冻结；不引用 host 侧别名）。

---

## 附录 B：三态语义对照（新增项）

| 语义点 | 上游 v4 / vendor | rust 现状 | rust 目标 |
|---|---|---|---|
| 入站回调可异步 | 是（`await listener(...)`） | **否** | `AsyncReply` + `STATUS_PENDING` + `ctx_fulfill(dispatch_id)`；同步路径零变更 |
| **waterfall 跨 ABI** | 是 | **否**（v1 误判不可行） | `ctx_next`（D6） |
| **取消（含级联）** | `AbortSignal` 贯穿 | **无** | `ctx_abort` + `Call.cancel` + `DispatchId.parent`（M2c） |
| **typed 事件** | `declare module Events` | 无 | `#[cordis::events]`（M6，**名逐字**） |
| `ctx.isolate` / `ctx.intercept` | 公开 | 内核有、ABI 无 | ABI v2（M1） |
| `ctx.use`（Def 52） | `ctx.plugin` | 内核有、ABI 无 | ABI v2 + `@native:`（M1） |
| 自我卸载/禁用回写 | loader 处理 | 仅原生 | ABI 补齐（M1） |
| per-consumer service config | `inject: { svc: {...} }` | **死机制** | 引擎便利通道（M3，**明确非 Def 26 的 d(k)**） |
| 访问控制元数据 | 提供者读 `d(k) ⊕ ι(k)`（右偏） | 无 | 内核**传递合并结果**，策略留给提供者（M3） |
| `Context.filter` | core/scope（**上游实现，非论文构造**） | 只有派发期两轴 | 注册期 scope + **父键关系**（M4） |
| 声明式 provision | schema/Service | 无 | `Plugin::provision` + `PluginDesc.provision` + **预挂载探测**（M0/M5） |
| 依赖环 | 运行时自然 inactive | 无 | 装载期**诊断**（默认不拒挂）；`strict` 才拒（**本仓扩展**） |
| 能力清单（装载前审阅） | `auditStartupEntries` | 无能力面 | `report.capabilities` + `probe_capabilities` + CLI（M5） |
| 服务原地替换 | `AdapterRegistrationHandle.replace` | 原料有（`Store::set`） | `components::registry`（M7） |
| **流式提供者** | `AsyncIterable` | 无 | **骨架内只允许 T1**（D7） |
| 跨进程/沙箱载体 | 无 | 无 | **移出骨架**（M8 = ADR）；**不是信任边界** |
| typed 服务 | TS 模块增强 | 运行时 downcast（且有双重 `Arc` 缺陷） | `Iface<T>` + 过程宏（M6）；**仅 T1 有效**，T1/T2 同名需转发（D2） |
| key 命名空间（§6.6 路线 1） | peer deps（路线 2） | 无 | key = `<name>@<major>`（M6）+ provision 同步用同一规则（M5） |

---

## 附录 C：评审记录

### 第 1 轮（v1 → v2）：论文忠实度 / Rust 工程 / harness 范围

| key | 级别 | 发现 | 处置 |
|---|---|---|---|
| R1-M3-INTERCEPT | P1 | M3 把两个来源错配到 d(k)/ι(k)；论文里**两者都是提供者侧**、合并右偏 | **采纳**：M3 重写 |
| R1-D2-FRAMING | P2 | §6.4 讲的是**访问中介**，不是推荐宏替代 downcast | **采纳** |
| R1-D1-ATTR | P2 | 载体中立性依据应归 §6.2/§6.3/§6.7 | **采纳** |
| R1-M5-CITE | P2 | 「Thm 73 ⇒ 必须报告环」误归因；§6.5 是*能力* | **采纳** |
| R1-G1/G4-CITE、R1-P80 | P2 | 引用位置不准 | **采纳** |
| R2-M6-DYN | **P0** | M6 字面示例编不过（async 非 dyn / 缺 Send+Sync / `Dyn::shared` 收不了 `?Sized` / 类型不匹配伪装成 `NoInject`） | **采纳** |
| R2-DESC-SIZE | **P0** | `PluginDesc` 尾追加被 `size < size_of` 门禁**拒掉所有 v1 插件** | **采纳**（→ G14） |
| R2-M1-SLAB | P1 | `Slab`→enum 波及九处热路径 | **采纳**：双表 |
| R2-M2-KEY | P1 | 裸 `id` 跨库碰撞 / 排水无落点 / 绕过代际校验 / `{"pending":true}` 冲突 | **采纳**（第 2 轮进一步修正） |
| R2-M4-FILTER | P1 | 「统一两轴」做不到（形状不同 + 造环） | **采纳** |
| R2-M3-SEAM | P1/P2 | `Store::call_iface` 不存在；metadata 未定义 | **采纳** |
| R2-M0B-DROP | P1 | `Drop` 归属错 | **采纳**（第 2 轮给出正确写法） |
| R3-P0-1-EVENTS | **P0** | 无 typed 事件、waterfall 被排除——harness 主扩展点 | **采纳**（→ D6/M2b） |
| R3-P0-2-DURABILITY | **P0** | 验收未覆盖有序/持久/可重放日志 | **部分采纳**（→ A11） |
| R3-P0-3-CANCEL | **P0** | 无取消原语 | **采纳**（→ M2c） |
| R3-P1-1..7、R3-P2-1..4 | P1/P2 | T3 信任、流式、顺序、broker、M8、环检测、replay、M9b、凭据、边界 | **采纳**（环检测为部分采纳+分歧记录） |

### 第 2 轮（v2 → v3）：ABI 考古 / TDD 可达性 / 契约一致性

| key | 级别 | 发现 | 处置 |
|---|---|---|---|
| R2.1-F1 | **P0** | **`api!` 挨饿读** ⇒ 真 v1 宿主上越界读；夹具测不出 | **采纳**（→ G15；第 3 轮升级为 copy-on-attach） |
| R2.3-F3 | **P0** | M0b 的 drop 顺序推理**是反的** | **采纳**：`StagedFile` newtype 声明在 `_library` 之后 |
| R2.3-F4 | **P0** | `Instance` 是 `#[derive(Clone)]` ⇒ 加裸 `Mutex` 编译不过 | **采纳**：`Arc<Mutex<..>>` |
| R2.3-F5 | **P0** | **挂起键用的是注册 cb id**（跨派发不变）⇒ 并发覆盖 | **采纳**（→ G17；引入 `dispatch_id`） |
| R2.2-P0-1 | **P0** | M11 的 A9/A11/HMR 断言不能是 cargo test | **采纳**（→ D9） |
| R2.1-F2 | P1 | 前缀未定义；MSRV 已被违反 | **采纳**（第 3 轮更正下限） |
| R2.1-F4 | P1 | 句柄 64 位吃满，「高位标签」未定义 | **采纳**：精确位布局 |
| R2.2-P1-1 | P1 | M0 头号红测试不会红 | **采纳**：M0 就落 `provision` 字段 |
| R2.2-P1-2/7 | P1 | M0b 删除测试需真 DLL；断言受 OS 锁影响 | **采纳** |
| R2.2-P1-3 | P1 | **缺 `pub` seam 清单** | **采纳**（→ D8） |
| R2.2-P1-4 | P1 | Pending 的线上形态未定义 | **采纳**：`STATUS_PENDING` |
| R2.2-P1-5/F20 | P1/P2 | A2「+8 组」与表不符 | **采纳**：统一 +6 |
| R2.2-P1-6 | P1 | 4 条时序测试不确定 | **采纳**：`time::pause()` 等 |
| R2.3-F1 | P1 | M4 scope 与《补全方案.md》§0/附录 A 冲突未声明 | **采纳**：取代清单④ |
| R2.3-F6 | P1 | 令牌放 `tail` 不可实现；波及面隐藏 | **采纳**：令牌放 `Call` |
| R2.3-F11/F12/F14 | P1/P2 | impl 也须脱糖；`'static` vs `'_`；`Ctx::filter` 子 ctx 无 fiber | **采纳**（→ G18） |
| R2.3-F7/F8/F9、F13 | P1/P2 | 接缝应是 `PluginIface::call`；`DispatchPolicy` 无归属；abort 必须 `remove`；`ExternService::call` 只改一次 | **采纳** |
| R2.3-F2/F3、F15..F23 | P2 | 取代清单不全；计数；命名空间调和；措辞 | **采纳** |

### 第 3 轮（v3 → v4）：实施模拟 / 溯源一致性 / 端到端验收

| key | 级别 | 发现 | 处置 |
|---|---|---|---|
| R3.3-P0-1 | **P0** | **M2b 协议自相矛盾**（`ctx_next` 与 `PENDING` 的先后）⇒ A9 不可实现 | **采纳**：D6 重写协议 |
| R3.3-P0-2 | **P0** | **取消无桥**：内核 `CancelToken` 与宿主 `pending` 两张表没接上 ⇒ A10 挂死或泄漏 | **采纳**：D3 取消桥（`select!`） |
| R3.3-P0-3 | **P0** | 无**父子级联**，且 `ctx_dispatch` 无 waterfall 模式 ⇒ 取消到不了嵌套工具派发 | **采纳**：G20 + `DispatchId.parent` |
| R3.1-P0-1 | **P0** | M0 头号红测试**没有可运行的载体**（门禁只在 `open` 里，需真 cdylib） | **采纳**：抽出纯函数 `host::desc_gate` |
| R3.3-P0-4/R3.1-P1-2 | **P0/P1** | A10 的「pending 表为空」**不可观测**（D8 把 `Instance` 保持私有） | **采纳**：`host::pending_len(handle)` |
| R3.3-P0-5 | **P0** | §0.1 的机制集合**漏了 M5**（`Plugin::provision` 是内核改动） | **采纳**：集合补 M5 + CLI |
| R3.3-P0-6 | **P0** | A5 的**预挂载能力来源未指定**（读 provision 必须 load，而 load 只发生在 `mount_row`） | **采纳**：`host::probe_capabilities` |
| R3.1-P1-1/R3.2 | **P1** | MSRV 下限**不是 1.77 而是 1.85**（`Waker::noop()`）——两名评审独立发现 | **采纳**：`rust-version = "1.85"` |
| R3.1-P1-3 | P1 | `Store::dispatch` 改签名会连坐 6 处测试 | **采纳**：新增 `dispatch_cancellable` 入口，旧入口不变 |
| R3.1-P1-4 | P1 | M5 声称消费 `PluginDesc.provision`，但**没有生产者** | **采纳**：M5 落 ABI 生产路径 |
| R3.1-P1-5/R3.3 | P1 | spike 找不到 cdylib；前置构建未写；`[lib] name` 陷阱 | **采纳**：D10 + M11 |
| R3.3 P1 诸条 | P1 | tokio `test-util` 未开；`instances()` 竞态污染 A1/A10；A11 载体依赖导致**假通过**；A3 的越界半边缺 oracle；A2 无基线 | **采纳**：D10 五项 |
| R3.2-P1 ×2 | P1 | 取代清单漏《补全方案.md》§0 的 waterfall 行与 key-namespacing 行；且后者的理由陈述本身不成立 | **采纳**：取代清单⑥⑦ |
| R3.2/R3.3 P2 诸条 | P2 | 「§5-risk-4」应为 risk 5；D8 把派生句柄布局归给 M2/M2c（实为 M1）；§3 的 workspace 成员登记未进 M0 正文；A4 的「见 §3.2」悬空；M2 t6 守卫未计入；`PLUGIN_DESC_PREFIX_SIZE` 类型；M3 波及面点名；M6 re-export 理由反了；D3 键大小写不统一；事件名不得命名空间化；`.cargo-home` 与 `cargo.cmd` 的 `CARGO_HOME` 不同 | **采纳**：全部落实 |

**三轮小结**：共 **17 条 P0**（第 1 轮 5、第 2 轮 5、第 3 轮 7；其中一行含两条相互印证的发现），
**全部采纳**；P1 全部采纳；两处分歧记录在案（环检测默认只报告；不回退手写宏双轨）。
`api!` 越界读与挂起键两处**升级处理**（分别升级为 copy-on-attach 与引入 `dispatch_id`）。

---

## 附录 D：不做清单

见 §0.3。补充：

| 项 | 理由 |
|---|---|
| 加权 broker / 负载均衡 / drain | 无消费者（R3-P1-4） |
| 第二载体的实现（含 WASM） | 移出骨架 → ADR（R3-P1-5） |
| 跨 ABI 流式 | 骨架内只允许 T1（D7） |
| 手写宏回退双轨 | 评审建议不做 |
| 事件名命名空间化 | 会使 ABI 侧硬编码的拦截名静默失配 |
| `waterfall` 曾列入不做 | **已撤销**：M2b 证明可映射（D6） |
| 把 `realm_filter`/`fiber_filter` 统一进 `Filter` | **已撤销**：形状不同且会造环 |
| 改 `Store::dispatch` 的签名 | 改用新增入口，保住全部既有测试 |
| 惰性探测作为主方案 | **已升级**为 copy-on-attach（结构上不可能越界） |

---

## 附录 E：执行记录（TDD，按里程碑滚动更新）

> 证据口径：每个里程碑收口时 `cargo test --workspace --offline` 全绿 +
> `examples/demo/compare.ps1` 逐字节通过；红测试与守卫计数见
> `crates/abi/tests/baseline.txt`（断言只增不减）。

| 里程碑 | 状态 | 全仓测试 | demo | 备注 |
|---|---|---|---|---|
| M0 | ✅ | 112 → 120 | 快照 3349B + `compare.ps1` 落地 | 红档完整（3 红：编译红/断言红，后实现） |
| M0b | ✅ | （并入 120） | 第 8 组（真删除断言） | `StagedFile` 声明在 `_library` **之后** |
| M1 | ✅ | 120 → 129 | +5 行（7.5 组） | ⚠ 红档缺失：执行通道被配额中断；以**非空验证**替代——5 个新宿主编织置 None ⇒ 8 条宿主测试全红 |
| M2 | ✅ | 129 → 139 | +2 行（7.6 组） | 非空验证：`ctx_fulfill` 置 None ⇒ 3 条挂起/结算测试可读超时红 |
| M2b | ✅ | 139 → 147 | +3 行（7.7 组） | 红档完整（E0432/E0609/E0599/E0061 编译红 + 实现中 3 条断言红→修断言）；**核心 API 升级**：`Next::call(args)` |
| M2c | ✅ | 147 → 154 | +3 行（7.8 组） | 红档完整；父链用**派发作用域**（线程本地 + `Scoped` future）记录，插件零传参 |
| M3 | ✅ | 154 → 161 | +3 行（6.7 组：真 DLL 提供者经 SDK 读 `meta`） | 红档完整（E0432/E0050/E0599 编译红 + `abi_m3` 运行期红）；**`ExternService::call` 一次定稿**（+`caller_meta`）；⚠ 执行期发现一条**与 M3 无关的**既有产物回收缺陷（见下 5），M3 未修、只记录 |
| M4 | ✅ | 161 → 167 | 不变（4782B 逐字节） | 红档完整（E0432/E0599 编译红 + 实现中间态 2 条断言红→改测试；另有一条测试夹具纠错：`Bail` 会短路、父 effect 强持 fiber）；**`Context.fiber` 改 Weak**（派生不再增加强引用计数）+ `Store.root_fiber` 强持根；非空验证：`allows` 短路 ⇒ 2 条红 |
| M5 | ✅ | 167 → 178 | **单行具名变更**（4782 → 4982B） | 红档完整（E0432/E0407/E0425/E0433 编译红 + 实现中间态 3 条断言红）；落地 `Plugin::provision` + `with_provision` + `PluginDesc.provision` 生产路径（`build_plugin_with_provision` + 宏 `provision:`）+ `Store::provision_report` + `Capabilities`/`probe_capabilities` + 迭代 Tarjan 环诊断（`report.cycles`）+ `cordis-cli inspect`（A5）；非空验证：`cycle_paths` 短路 ⇒ 3 条红 |
| M6 | ✅ | 178 → 189 | 不变（4982B 逐字节） | 红档完整（E0432/E0433/E0599/E0405/E0107 编译红 + 实现中间态 2 条断言红）；落地五个宏（`service`/`service_impl`/`events`/`plugin`/`config`）+ `cordis-sdk` 门面（库名 `cordis`）+ 内核 `Iface<T>`/`Dyn::adapter`/`ResolveError::TypeMismatch`/`Ctx::iface`/`ServiceFuture` + 宿主 `encode_service` 同口径；非空验证：`Dyn::adapter` 短路 ⇒ 8 条红、事件名前缀 ⇒ 2 条红 |
| M7 | ✅ | 189 → 194 | 不变（4982B 逐字节） | 红档：E0432（`cordis_components::registry` 不存在）→ 首实现即绿；`components::registry`（具名槽位）落地：槽位**由注册表自己提供** ⇒ `replace` 过 `Store::set` 的提供者守卫、身份不变 ⇒ 依赖者 epoch 不动（A7）而读到当下值；**零内核改动**（`refresh_desired` 只算声明过的 inject）；非空验证：`replace` 改成重新 provide ⇒ 3 条红、`get` 空实现 ⇒ 3 条红 |
| M9 | ✅ | 194 → 198 | **单行具名变更**（4982 → 5079B） | 红档：编译红（E0432 `InterpolationMode` / E0560 新字段）+ 1 条运行期红；`js::JsCapabilities` + `capabilities_of`（词法抽取，**只有引用没有值**）+ `RejectInjected` + `LoaderConfig.interpolation{builtin(默认)|injected}` + `patch::collect_js_capabilities`（与问题清单共用**唯一**遍历器）+ 报告 `capabilities[].js` / `interpolation.mode`；既有测试 9 处具名改动（1 处整行 json 断言 + 8 处 `LoaderConfig` 字面量）；非空验证：`capabilities_of` 短路 ⇒ 2 条红、`injected` 回落内置子集 ⇒ 1 条红 |
| M10 | ✅ | 198 → 200 | 不变（5079B 逐字节） | M10b：`ctx_is_not_leaked_after_fiber_dispose`（子 ctx 与派生作用域 ctx 都回收 + **root 仍活着**的对照断言）；M10c：`abi_compatibility_matrix`（每格**真读一次** `field_at` 并与判界规则逐格对照 + 门禁矩阵 + **渲染 `兼容矩阵.md`**，默认防漂移、`CORDIS_WRITE_MATRIX=1` 生成）；M10a 文档：`原理.md` 页号 −3（括号内 64 + 括号外 15）+ 五条定理重编号 + `§4.4→§4.3`、`方案.md`/`补全方案.md` 镜像取代清单、**新增 `SDK指南.md`**（tier/两张 interception/T1-T2 typed 边界/语义陷阱/MSRV 矩阵/TS 对照）；非空验证：判界 `>=`→`>` ⇒ 3 条红、故意泄漏 ctx ⇒ 1 条红 |
| M11 | ✅ | 200 → 205 | 不变（5079B 逐字节；**spike 二进制**另有一套断言：11 步、连跑 3 次 stdout SHA256 全同，3221 字节） | 两层落地：**无 DLL 层** 5 条（`examples/harness-spike/tests/e2e.rs`：作用域隔离 / typed 服务两面 / typed 事件类型检查 / replay 确定性 / 原地换适配器）+ **真 DLL 层**（`examples/plugins/harness-tool` 的 A4/A9/A11 + `src/main.rs` 的 M5 预挂载探测、**cordis-cli 库面**、D2 目录转发、M3 use-site 元数据、A7、A10，共 11 步断言）。红档：E0432（缺 lib 目标）编译红 + 实现中间态 2 条断言红。非空验证三处：`agent_scope` 谓词恒真 ⇒ 1 条红、DLL 不恢复 `seq` ⇒ A11 红、拦截器不改写入参 ⇒ A9 红（详见 `crates/abi/tests/baseline.txt` M11 段）。具名修改 4 处：`loader::reload_one` 私有 → `pub`（确定性重载驱动）、根 `Cargo.toml` 加 `cordis-cli` 工作区依赖、两个示例的 `Cargo.toml` |
| M8（ADR） | ✅ | — | — | 交付 `docs/adr/0001-second-carrier.md`：**不实现**第二载体（成本 × 无消费者），但把四件事写死——① `LoadedPlugin` 是唯一适配器 + `cordis-abi` 零平台依赖（四个 seam 逐条点名）② D1 的两条硬话（T3 不是信任边界；模型自生成组件永远不得是 T2）③ **D7 的流式推广路径**（N 次回调 + 背压窗口，复用现有挂起机制，含将来实现的验收口径）④ **T3 的接口契约必须异步**（含待补的 `provide_iface` 异步形态 `AsyncIfaceFn`、取消"本地绝不等待远端确认"） |
| 缺陷 5 闭环 | ✅ | 205 → 209 | 逐字节不变（5079B，demo 6.7 恢复真卸载后仍然逐字） | 机制定位：`Arc<dyn Plugin>` 的两条持有面都以"fiber 活着"为前提，而 **`ctx → fiber` 曾经是强引用** ⇒ 被留下的 ctx（依赖者的 `Impl`、`Instance.ctx`、派生 intercept ctx）吊住提供者 fiber；M4 的 G18 已把它改成 `Weak`（原记录里"Mounted.plugin"的猜测不成立）。回归 `crates/host/tests/loader_release.rs`（4 条：判据 = 插件对象是否被 drop；夹具把装载器挂成**插件 fiber**，否则量到的是缺陷 6；含"故意留着 ⇒ 判据必须为假"的反空转对照）。非空验证：`Context::attach_fiber` 另存一份强引用（模拟 G18 之前）⇒ **3 条红** |
| M8-后续①（ADR §4 第一块拼图） | ✅ | 209 → 210 | 不变（5079B 逐字节） | `ctx.provide_iface_async` + `AsyncIfaceFn`：**T2 插件的服务方法也能挂起**（`Pending` 挂起 / `Reply` 立即答复，与 `on_async` 共用同一套机制）。宿主侧**零改动**（`PluginIface::call` 早就在等结算）。回归 `crates/host/tests/sdk_async_iface.rs`（六条断言，含卸载排水）。红档：E0599 编译红 + 实现中间态红（apply 槽进程级 ⇒ `handle == 0`，已写进 `sdk.rs` 文档与指南陷阱清单） |
| ADR 0002（broker 的消费者问题） | ✅ | 210（不变） | 不变（5079B 逐字节） | 交付 `docs/adr/0002-broker.md`，**零代码改动**。① 论文里的 broker 是**模式不是机制**（§6.2 pp.71-72："one fiber providing the key and dispatching among implementations registered with it"；注册走 "a revertible effect"）+ 三能力（负载均衡 / 滚动更新 / 跨进程）；② 判定口径从"没有消费者"这句口号升级为**一手证据**：真实下游制品 `deepseek-harness` 的源码与 `.agents/notes`，10 条候选逐条引用 file:line；③ 净判定：8 条无需求/被制度性否决/正在删除，剩下两条只到"**单键原子换值**"与"**稳定句柄**"——**没有任何一条要求"在 N 个候选之间选择"**，而 M7 的 `components::registry` 正是 broker 的 **N=1 退化形态**（覆盖 A7 的"换实现不重挂依赖者"）；④ 三条表达路径（组合二选一 / 实现内部路由 + 具名槽位 / `isolate` 各域一个）写进 `SDK指南.md` §8.1；⑤ **drain 不做**——换值走 `Arc` 语义本就不需要排水（排水是**卸载**的语义，M2 已有），将来要"等在飞归零再退场"属于实现自己；⑥ 跨进程块并入 ADR 0001 的 T3（论文自己把跨进程建立在 broker 之上）；落地路径 = `components::router`（普通插件，**零内核改动**）+ 三条触发条件 |
| M8-后续②（流式 op，ADR 0001 §5） | ✅ | 210 → **224** | 不变（5079B 逐字节；**spike 3221 → 3432 字节**，连跑 3 次 SHA256 全同） | 落地 **`Context::call_iface_stream` → `AsyncStream`** + ABI **#20–#22**（`ctx_stream_push` / `ctx_stream_close` / `ctx_stream_fail`，尾追加）+ `KIND_STREAM_CREDIT` + `STATUS_STREAM_GONE`。core 新增 `stream.rs`（`StreamSink`/`AsyncStream`/`StreamObserver`/`PushOutcome`/`StreamOutcome`；窗口 4 = tokio mpsc 容量；credit 队列与"登记/投递同一把锁"防丢唤醒；收尾恰好一次）+ `ExternService::call_stream` 默认实现（**返回值 = 长度 1 的流** ⇒ 不写流式代码的提供者也能被按流调用）；宿主：`DispatchRecord.stream`（挂在既有记录上 ⇒ "条目即生命周期"继续成立）+ 流观察者 + credit 投递（`kind:"stream-credit"`，**先取 call、放锁、后调用**）+ **四条收口**（`fulfill` 补最后一件并收尾 / 取消 / 超时 / 卸载排水：等待中的 credit 先被驳回、流以错误结束，然后才 `OP_DISPOSE`）+ `streams_len` 观测口。真 DLL 层：spike 第 **8.5** 步（`harness.stream@1` 推 20 块、被窗口挡 16 次）。红档：编译红（core E0432/E0407；host E0432/E0609/E0599）+ 实现中间态红（E0308/E0063/E0408/E0596）。**非空验证 3 条**：满窗口不登记 credit ⇒ 5 红（含验收用例撞 10s 护栏）、消费者离场不通知 ⇒ 2 红、卸载不排水 ⇒ 1 红。执行期发现 2 条（`cargo build` 失败被误判为通过 ⇒ 旧 DLL + `NoInject`；新键要同步进 `provision:`）。ADR §5 的**三处偏离已注记**：无 `open-stream`（流 = 那次派发本身）、`stream_fail` 取代 ADR 的 "abort"、额度由 `next()` 隐式归还 |

**执行期间发现并修复的既有缺陷（不在原方案清单内）**：

1. **A2 确定性竞态**（M2 期间暴露）：`[rust-side]` 激活打印与装载器「已挂载」日志
   无 happens-before，约 1/4 运行会翻转两行顺序（字节级基线因此会偶发失配）。
   修复：激活消息**入队**、由 main 在两个可观测收敛点
   （① `report.reconcile.applied == 2`；② 消息入队）flush——输出内容与顺序和既有
   快照完全一致；连跑 8 次 SHA256 全同。
2. **夹具纪律**：M2 测试的 fake `CallFn` 里原先把缺失能力写成 `.expect()`
   ——panic 跨 `extern "C"` 会 abort 整个测试进程；改为返回可读状态码（D10 铁律），
   并给所有"等待结算"的派发加 10s 超时护栏（失败可读、绝不挂死）。
3. **持锁回调插件 ⇒ 同线程死锁**（M2b 首跑即暴露）：`deliver_next` / `deliver_abort`
   在持有 `instances()` 锁的 `if let` 里调用插件，而插件在回调里 `fulfill` 需要
   同一把锁。修复＝**先取 `call`、再放锁、后调用**（全仓同模式的第三处，M1 已修
   过两处：`host_ctx_use` 重入、回调内 `mount`）。
4. **测试夹具的锁中毒放大成进程 abort**（M2c 首跑暴露）：`id_for` 在**持锁期间**
   `expect`——断言失败 ⇒ 锁中毒 ⇒ 后续任何 `extern "C"` 回调里 `unwrap()` 中毒锁
   = panic 跨边界 = abort 整个测试二进制。修复＝每测试独立夹具静态 + 助手把
   `unwrap` 移出持锁期（`unwrap_or_else(|poisoned| poisoned.into_inner())`）。
6. **装载器以 root ctx 装载 ⇒ 行 fiber 永远不回收**（M11 定位并规避；与下面"尚未修复"的
   缺陷 5 同族，但触发条件不同）：`load_with_source` 挂出来的行 fiber 是**父 fiber 的一个
   effect**（`Fiber::new` 的"父卸载 ⇒ 子递归卸载"）。父若是 **root fiber**（`App::root`，
   永不 dispose），那些 effect 闭包就永远强持行 fiber ⇒ `Fiber → Runtime →
   Runtime.plugin = Arc<LoadedPlugin>` ⇒ 暂存副本到进程结束都不删。现场特征：
   `loaded_instances() == 0`、`stagedLeaked == 0`、`StagedFile::drop` **没有**打
   "删除失败"日志，而 `.r1`/`.r2` 都还在磁盘上；
   处置：**装载器自己挂成一个插件 fiber**（demo 的 `loader()` 就是这个形态；spike 的
   `harness-loader` 为了拿到 `LoaderState` 自己包了一层），收尾时拆它 ⇒ 行 fiber 与
   装载器一起回收（spike 第 10 步的真删除断言因此成立）。

**执行期间发现的既有缺陷（曾记录为"待修"，现已闭环）**：

5. **卸载"依赖装载器插件服务的原生 fiber"之后，该插件的暂存副本不再被回收**（M3
   期间为 demo 组选位置时暴露；G11 面，与 M3 的实现无关）→ **M11/ADR 这一轮定位并闭环**。
   原始记录（最小复现，demo 内）：原生 fiber 声明 `inject: ["greeting"]`（greeting 是
   装载器挂的真 DLL）→ 调用它的服务方法 → `fiber.dispose().await` ⇒ demo 尾部的
   "M0b 真删除断言"失败：`greeting.r1.dll` 活到进程结束。
   二分矩阵（每一行一次完整 demo 运行；"泄漏"= 该断言失败）：
   | 变体 | 依赖声明 | 派生 intercept ctx | 调用 iface | 卸载调用者 | 结果 |
   |---|---|---|---|---|---|
   | 基线（M2c，无此组） | — | — | — | — | 干净 |
   | (c) 只挂载/卸载 | 无 | 无 | 无 | 是 | 干净 |
   | (h) 依赖 + 调用 + 卸载 | 有 | 无 | 有 | 是 | **泄漏** |
   | (b) 依赖 + 派生 + 卸载 | 有 | 有 | 无 | 是 | **泄漏** |
   | (e) 依赖 + 派生 + 调用 | 有 | 有 | 有 | 否 | 干净 |
   观测（失败现场在断言消息里直接可见）：`loaded_instances() == 0`（宿主全局实例表
   已空）、`staged_leaked() == 0`（副本已被 `StagedFile` 认领）、`StagedFile::drop`
   没打出"删除失败"日志 ⇒ 结论是**某个进程级结构仍持有 `Arc<dyn Plugin>`**，
   而不是文件被锁或台账漏认领。

   ### 闭环（本轮）
   - **机制**：`Arc<dyn Plugin>` 的长期持有面只有 `Store.runtimes[..].plugin` 与
     `Fiber.runtime → Runtime.plugin` 两条，**两条都以"fiber 活着"为前提**。
     于是问题等价于"插件卸载之后谁还握着那条 fiber"——答案是
     **`ctx → fiber` 曾经是强引用**：依赖者被卸载后，只要还有人留着它的 ctx
     （依赖者提交视图里的 `Impl`、宿主的 `Instance.ctx`、派生 intercept ctx），
     提供者的 fiber 就被一起吊住。**M4 的 G18 已把它改成 `Weak<Fiber>`**
     （根 fiber 由 `Store.root_fiber` 强持）⇒ 链断。原始记录里"最可能是
     `Mounted.plugin`"的猜测**不成立**（`Mounted` 在卸载时被摘除，watcher 也不是原因）。
   - **回归**（新文件 `crates/host/tests/loader_release.rs`，4 条）：判据是**插件对象是否
     被 drop**（`SpyPlugin` 的 `Drop` 打标）——它就是二进制层"暂存副本能否删除"的等价
     观测，且**不依赖动态库**。夹具把装载器挂成一个**插件 fiber**（否则量到的是缺陷 6）：
     (h)/(b)/(e) 三个变体 + 一条"故意留着插件对象 ⇒ 判据必须为假"的反空转对照。
   - **非空验证**：把 `Context::attach_fiber` 改回"另存一份 `Arc<Fiber>`"（模拟 G18 之前的
     强引用）⇒ **3 条红**，报错正是"装载器拆完之后 `Arc<dyn Plugin>` 必须被回收"。
   - **端到端**：demo 的 6.7 组恢复**真卸载**调用者（该组当初正因本条而省略这一步），
     输出仍逐字节不变（5079 字节）、尾部真删除断言照旧通过；spike 第 10 步的真删除断言
     同样覆盖（那条路径还叠加了缺陷 6 的规避纪律）。

**TDD 纪律的诚实记录**：M0/M0b 按"先红后绿"执行且留有红证据；M1 因执行通道中断
未留红档（以非空验证替代）；M2 同理在实现后补测，用非空验证与超时护栏保证
"测试不空转、失败不挂死"。**M2b/M2c 起恢复完整红档**：先写 15 条测试（8 + 7）并
记录编译红（E0432/E0609/E0599/E0061）与实现中间态的断言红，再修到全绿。
**M3 同样先红后绿**：先落 7 条测试（core 6 + host 1），记录编译红
（E0432 `ResolvedConfig` 未导出 / E0050 `ExternService::call` 形参不匹配 /
E0599 `resolved_config` 不存在）与 `abi_m3` 的运行期红（派生句柄当时还不能用于
`ctx_call_iface` ⇒ 提供者收不到调用），再实现到 161 全绿。
**M4/M5/M6 继续先红后绿**：M4 记 2 条断言红（实现中间态）、M5 记 3 条（实现中间态）、
M6 先写 11 条测试并记录编译红（E0432 门面未导出宏 / E0433 `typed` 不存在 /
E0599 `LoopConfig::parse` / E0405+E0107 宏展开里的路径笔误）与实现中间态 2 条断言红
（`calls()` 期望值应为 2；`serde_json` 错误里没有字段名 ⇒ 配置宏改为「字段级体检优先」），
再修到 189 全绿。
**M7 先写 5 条测试、记编译红（E0432）**，但**首实现即绿**——按 M5 起的纪律，
首跑即绿的里程碑必须用 **mutation** 交代"断言有牙"：`replace` 改成重新 provide ⇒ 3 条红、
`get` 改成空实现 ⇒ 3 条红（详见 `baseline.txt` 的 M7 段）。
**M9 先写 4 条测试**，记编译红（E0432 `InterpolationMode` 未导出 / E0560 新字段 /
测试自身两处类型笔误）与首跑时的 1 条运行期红，再修到 198 全绿；同样补了 2 条 mutation
（`capabilities_of` 短路 ⇒ 2 条红、`injected` 回落内置子集 ⇒ 1 条红）。
**M10 是"文档 + 两条结构性断言"**：两条新测试（Weak 审计 / 兼容矩阵）都首跑即绿——
这正是要用 **mutation** 交代的地方：判界 `>=` 改成 `>` ⇒ 3 条红、故意把一个 ctx 存进
静态（模拟"忘了用 Weak"）⇒ 1 条红；矩阵那条测试另外自带**防漂移**判据
（生成的文档与提交的文档逐字比对），所以它的"红"不依赖人的记性。

**M6 执行期新发现并已处理的两条（已在宏文档里写死，M10 的 SDK 指南要收进去）**：

6. **`Plugin::apply` 的 `'static` future 与 `&self` 相互排斥**：D2 只说了"适配器需一次
   `Arc<Self>` 提升"，落到**插件类型本身**时是另一回事——`#[cordis::plugin]` 生成的
   `impl Plugin` 拿到的只有 `&self`，没法升成 `Arc<Self>`（trait 的接收者不是 `Arc<Self>`）。
   处置：宏在调用前**克隆一份 `self`**，于是插件的类型必须 `Clone`（共享状态放 `Arc`），
   这条契约写进了 `#[cordis::plugin]` 的文档头。若要改 trait 接收者，属于 ABI 级改动，
   M6 不做。
7. **`serde_json::from_value` 的错误里没有字段路径**：`invalid type: string "x", expected u32`
   ——配置诊断最要紧的"哪个字段"缺失。处置：`#[cordis::config]` 的 `parse` 在 serde 失败后
   做一遍**顶层字段体检**（逐字段 `from_value::<FieldTy>`），把第一个解不出来的字段名
   放进 `ValidationError.issues[0]`；字段级扫描说不出话时（如"缺字段"）才用 serde 原话。
   代价：错误路径上多一轮浅解析（可忽略）；已知偏差：带 `#[serde(deserialize_with)]` 的
   字段可能被误点名（serde 的原话仍可作对照，且仅在整体解析**已经失败**时才会出现）。

8. **内置子集的 `?.` 只覆盖成员访问，不覆盖调用**（M9 写测试时暴露）：`ctx.get('x')?.name`
   可以（M4-4a 的表里有它），但 `ctx?.get('x')` 会被判成"不支持调用 成员访问 上的 `?`"。
   处置：**不改求值器**（改它要动 M4-4a 那张"子集即规格"的表，而 M9 的题面是能力面）；
   改成把两件事分开断言——**能力抽取是词法的**（`?.` 等价于 `.`，照样认），能不能求值是
   求值器的事。这条记录在这里，是因为它同时是"往 `with(ctx)` 全量语义再走一步"的入口
   （真引擎走 `LoaderConfig.js_evaluator` 挂点，M9 的 `injection` 模式让"只用注入的引擎"
   成为可配置的纪律）。

---

## 附录 F：终验收（A1..A11 复跑，2026-10-16）

> 口径与 §0.2 一致；每条给**现场证据**（命令 + 可复算的数字）。
> 全量测试 **225 passed / 0 failed / 0 warnings**，连跑 3 次同结果。

| # | 判据 | 结论 | 证据 |
|---|---|---|---|
| A1 | 全绿且确定性 | ✅ | `cargo test --workspace --offline --no-fail-fast` 连跑 3 次：**225 / 0 / 0** 每次相同 |
| A2 | demo 快照逐字 + 新增组 | ✅ | `compare.ps1` 逐字节（5079B）；**20 次连跑 0 次抖动**、8 次 stdout SHA256 全同；demo 8 → **15 组**（新增 6.5/6.6/6.7/7.5/7.6/7.7/7.8 共 7 组）；spike **3432B**、5 次 SHA256 全同（`5F8F38…`）——**本轮修掉一条收尾期竞态**（缺陷 7） |
| A3 | 兼容矩阵 | ✅ | `abi_matrix`（3 条：逐格真读 `field_at` + 门禁 + 渲染防漂移）、`v1_shape`（3 条，尾字段 8 → 11）、`copy_on_attach`（1 条截断金丝雀）；`兼容矩阵.md` 与 M10 逐字节相同（`6BC5144E…`；本轮只动 `HostApi` 尾追加，`PluginDesc` 未动） |
| A4 | 动态库插件的一等公民能力 | ✅ | spike 第 4（派生隔离域 + `@native:` 子组件）、6（异步工具）、**8.5（流式）**、9（自我卸载）步 |
| A5 | mount 之前的能力清单 | ✅ | `cli/inspect`（1 条，断言 `mounted == 0`）+ spike 第 0 步现场输出（`mounted = 0`；harness 行的 inject/provision/volatile 全列出，含新增的 `harness.stream@1`） |
| A6 | 环诊断、默认不拒挂 | ✅ | `loader_cycles` 4 条（默认只报告 + WARN；`strict` 才升级成装载失败，附环路径） |
| A7 | 原地换实现的 epoch 不变 | ✅ | `components/registry` 5 条 + spike 第 5 步（换 llm 适配器后 epoch 打印不变） |
| A8 | 内核零 unsafe、abi 零平台依赖 | ✅ | `crates/core/src/*.rs` 里 `unsafe` 只出现在 `#![forbid(unsafe_code)]`（lib.rs:17）与两处文档句（context.rs:16、fiber.rs:940）；`crates/abi/Cargo.toml` 依赖只有 `serde_json`，源码里唯一的 std 外部符号是 `std::os::raw::c_char`（ADR 0001 §1 的原话） |
| A9 | typed 事件 + waterfall 拦截 | ✅ | spike 第 6 步（DLL 拦截 `tools/before-invoke` 并改写结果；`stages == ["harness-tool"]`） |
| A10 | 取消不残留 | ✅ | `abi_m2c` 6 条 + spike 第 7 步（`pending_len 1 → 0`、日志写明「已取消」）；**流式面同一口径**：`abi_stream` 五条结束时 `pending_len` / `waterfall_len` / `dispatches_len` / `streams_len` 全零 |
| A11 | 有序持久日志 | ✅ | spike 第 8 步（一次真 DLL 重载：`boot 1 → 2`、seq `[10..14]`/`[15..19]` 连续、19 行 seq 1..19、旧文件是新文件前缀）+ 第 9 步（插件走了，盘上 19 行一条不少） |

### 验收期发现的缺陷 7（已闭环）

**现象**：demo 快照在收尾期有约 **1/20** 的概率多出 107 字节——装载器的 `publish_report`
在一轮"晚到的对账"里对**已经拆掉**的 `loader.report` 写值 ⇒ `ctx.set` 失败 ⇒ 打出一条 WARN
混进 demo 的 stdout（A2 因此失败）。现场特征：`length mismatch: expected 5079 bytes,
actual 5186 bytes`，多出来的正是
`[WARN ] cordis:include: 刷新 loader.report 失败: cannot set property "loader.report" without provide`。

**判定**：这不是异常而是**正常时序**——`loader.report` 的提供者**就是装载器自己**
（`load_with_source` 在 `provide` 之后才 `mark_report_published`），装载器拆到一半
（`Unloading`/`Disposed`）或依赖没齐（`Pending`）时这一写必然失败。

**修复**：`publish_report` 在**装载器自己不在 `FiberState::Active`** 时直接返回——与
`attach_self_dispose_observer` 的第 2 条判定同源（"装载器自己不在 Active，整棵树就在关"）；
只有"还 ACTIVE 却写不进自己的报告"才继续报警。

**回归**：`crates/host/tests/loader_reconcile.rs::a_reconcile_after_the_loader_fiber_is_disposed_does_not_publish`
（把装载器挂在一个自己的插件 fiber 下 ⇒ 拆掉 ⇒ 再来一轮 `sync()` ⇒ 断言日志里没有那句话）。
非空验证：去掉守卫 ⇒ 该条红（225 = 224 + 1）。
**修后**：demo 连跑 20 次 0 次抖动（修前 20 次里 1 次）。

