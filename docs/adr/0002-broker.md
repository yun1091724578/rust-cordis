# ADR 0002：broker（服务多路复用 / 多提供者选择 / drain）

- **状态**：已决定（**不实现**；本 ADR 把"消费者问题"定案，并登记退化形态、表达路径与触发条件）
- **日期**：2026-10-16
- **依据**：论文 §6.2（*The service broker*，pp. 71-72）、§4.2.1（disjointness 与两条路，p.33）；
  《SDK方案.md》§0.3 取代清单、§1 G7、§4 M7（验收 A7）；《原理.md》§5.2；
  `docs/adr/0001-second-carrier.md`（跨进程）；
  **真实下游制品的一手证据**：`E:\code\新架构\deepseek-harness`（TS，vendored cordis
  `vendor/loader 1.0.6-alpha.1`，本仓的对齐对象）——§3 的每一行判定都引它
- **决定**：① 骨架内**不做 broker**；② 骨架里已落地的 `components::registry`（M7）就是它的
  **N = 1 退化形态**，而"换实现不重挂依赖者"这条真需求**已经被它覆盖**；③ "多实现"的三条
  表达路径写进 `SDK指南.md`；④ 负载均衡 / 滚动更新 / 跨进程三块只在 §8 的触发条件下回头做。

---

## 1. 论文里的 broker 是一个**模式**，不是机制

论文 §6.2（p.71）把"一个服务键有多个实现"分成两种形态：

> (1) *Exclusive binding*: several implementations share one interface but **at most one is bound
> at a time** … switching between them requires unloading one provider and loading another,
> momentarily perturbing every consumer's dependency.
> (2) *Service broker*: a central service that acts as the entrypoint for the interface is injected
> by both the backing providers and the consumers, so that **multiple providers coexist and the
> broker dispatches each request among them**.
> "Compared to exclusive binding, **the broker absorbs this perturbation**: updating a backing
> provider leaves the broker in place, so consumers see no change to their dependency and no reload
> is triggered."

论文给 broker 派生的三个能力（p.71-72）：**负载均衡**（策略如 round-robin / least-loaded /
latency-weighted，或消费者点名）、**滚动更新**（新实现在另一个 fiber 上装载，ACTIVE 后调权重
逐步转移流量，旧实现"不再带在飞请求"后卸载）、**跨进程调用**。

**关键在这句话**（§4.2.1, p.33）：

> a system that wants several providers of one key keeps two routes: realms, and the broker of
> Section 6.2, **one fiber providing the key and dispatching among implementations registered with
> it**.

⇒ broker 的落点是**一个 fiber（普通组件）**，不在内核里；注册走
"a revertible effect"（p.71-72），也就是本仓的 `Disposable`。
**论文本身没有要求任何新的内核机制。**

---

## 2. 本仓今天站在哪：exclusive binding + 一个共享 realm

- 键读在**一个共享 realm**（论文 §4.2.1 的口径），"隔离"只用来把**同一个键的第二个实现**
  挪进独立域（`crates/core/src/context.rs:265-267` 的 `isolate` ⇒ 每个域各自一个提供者、
  **互不可见**）。这正是上面的形态 (1)。
- 本仓**也没有走 realm 表那条路**：M4 的 `ScopeAnchor` 轴管的是**可见性**（谁看得见这个注册），
  不是**选择**（在多个里挑一个）。
- ⇒ 在本仓，"同一键两个实现"**不是一个运行时选择问题，而是一个组合问题**：两个实现 ⇒
  两个域、或两次装载，由组合者决定装谁。这与"有 provision 的组件至多一个 fiber"的纪律一致。

---

## 3. 消费者问题（本 ADR 的核心）：逐条判定

判定口径三条：**(a)** 今天有真实消费者吗（证据来自真实制品，不是推测）；**(b)** 能不能用
既有机制表达（键纪律 + 组合 + 实现内部转发）；**(c)** 会不会破坏不变量（epoch 含提供者 uid
⇒ 提供者重启必然级联其消费者；`方案.md:518`）。

下列路径均相对 `E:\code\新架构\deepseek-harness\`。

| # | 候选消费者 | 判定 | 一手证据 |
|---|---|---|---|
| 1 | 多提供者按权重/轮询/最闲**选择** | **无，且被制度性否决** | `.agents/notes/implemented/architecture/2026-07-14-provider-routed-llm-adapters.md:25`："A provider has exactly one adapter owner in a Cordis context … loading both owners is a configuration error **rather than an ordering rule or fallback**"；`packages/llm/llm/src/index.ts:383,431`（`DUPLICATE_ADAPTER`） |
| 2 | fallback（失败换一家实现） | **无，明确否决** | 同笔记 `:77`："mounting two adapters for one provider **fails at load instead of creating fallback order**"；重试只在**同一条路由**上做（`packages/llm/llm-retry/README.md:12,54,132`：配额失败"continue until success, cancellation, or disposal"） |
| 3 | 多 key / 多 endpoint 轮询、限流换 key | **无**；需求由"每次操作重解析"满足 | `docs/subsystems/credentials.md:5`："consumers resolve a reference **once per operation** … a rotated credential reaches the very next request **without any restart**" |
| 4 | least-loaded / 最闲调度 | **未发现证据**（全仓零命中，命中项均为测试命名/UI 比例等无关物） | — |
| 5 | 同键多实现的**原地替换**（不是选择） | **有**，但只到"单键 + all-or-nothing swap" | `.agents/notes/implemented/architecture/2026-07-30-credential-boundaries-and-atomic-registration.md:23`："Route replacement is a **registry operation, not a caller sequence** … validated in full first … then **swapped in one synchronous section**"；接口 `AdapterRegistrationHandle.replace(providers)`（`packages/llm/llm/src/index.ts:297-315`） |
| 6 | 同名工具 / MCP 服务池（合并或选择） | **无**；**命名空间共存** + 同键**硬失败** | `packages/mcp/mcp-client/README.md:75`："Two servers can both offer a tool named `search` and **coexist** as `mcp__github__search` and `mcp__web__search`"；`:78`："Two entries using the **same server name**: the later one fails to load"；`:110`："**Full generation or none** … swap generations atomically"；同名工具注册直接抛错（`packages/core/tools/src/index.ts:747-750`） |
| 7 | drain：等在飞请求跑完再换实现 | **服务层无需求**；只有域内单点 | `docs/subsystems/storage.md:39`（"Drain in-flight writes … and release the medium"）、`packages/mcp/mcp-client/README.md:127`（单 server 的 generation quiesce）、`docs/subsystems/llm-streaming.md:310`（**逐调用快照**："later route disposal or replacement **cannot change an in-flight failure's recovery policy**"——不是 drain） |
| 8 | 滚动更新 / 灰度 / canary / A/B 双跑 | **未发现证据**（命中全是测试命名、npm dist-tag、术语表、fixture 字面量） | — |
| 9 | HMR 源码替换（**drain 的唯一真实消费场景**） | **正在被主动删除** | `.agents/notes/proposed/simplification/2026-09-19-config-only-hmr.md:9`："other shipped profiles **disable or omit HMR**"；`:15`："**Require a process restart** for source-module … changes"；`:23`："That capability is useful; **this proposal deliberately gives it up**" |
| 10 | **"换实现要重启所有依赖者"（stable handle）** | ✅ **成立，且是全仓唯一一处**——但诉求是 **stable handle**，不是加权/均衡/drain | `.agents/notes/proposed/feature/2026-10-03-pi-plugins-compatibility.md:98`："Provider replacement \| Chord: **A consumer holds a handle that resolves the current provider on each access**; a reload swaps the implementation behind it. \| DSH: **A fiber whose injected service changes is unloaded and re-run.**"；`:106`："Four Chord mechanisms … **Each needs its own proposal; this note only records them.**"；`:108`："**Stable service handles.** … so a plugin reload does not restart its dependents and lose their state." |

**净判定**：10 条里 8 条是"无 / 被否决 / 正在删除"，1 条（#5）是**单键原地换值**，1 条（#10）是
**稳定句柄**。**没有任何一条要求"在 N 个候选之间选择"**——而"路由集 > 1 + 选择策略"正是 broker
相对既有机制的全部增量。

---

## 4. 决定

### D-a 不做 broker（含负载均衡 / 滚动更新 / 跨进程三块）

理由：§3 无消费者；且**选择策略是策略**——一旦进 SDK，它就从"组合事实"变成"框架承诺"
（论文自己把 broker 描述成"可配置策略"，p.71）。本仓的验收目标是**骨架**，不是调度器。

**诚实记账**：我们放弃的是论文 §6.2 的演示——"把蓝绿部署从基础设施操作变成应用级组合模式"。
今天没有任何制品需要它；将来需要时按 §6 做，不需要内核改动。

### D-b "多实现"仍然是**可表达**的，且不需要新机制（三条路径，见 §5）

论文的形态 (1)/(2) 在本仓都能直接写出来，只是**没有框架层的策略**：
同一个键、两个候选实现、由**组合**二选一（(1)）；一个键 + 实现内部路由（(2) 的实质）；
或两个域各一个（同键互不可见）。

### D-c 骨架里已落地的**退化形态**：`components::registry`（M7）

- 它是 broker 吗？**是 N = 1 的 broker**：路由集里恰好一个条目 ⇒ "选谁"退化成"就它"，
  于是只剩 broker 的**另一半价值**——§1 引文的 "the broker **absorbs this perturbation**"。
- 实现 = **槽位由注册表自己 provide**（`crates/components/src/registry.rs`）⇒ `replace` 是
  **同一个提供者**的一次 `Store::set` ⇒ 依赖者 epoch 不变、不重挂、**状态不丢**；槽位注册走
  `put` 返回的 `Disposable`——正是论文的 "revertible effect" 形态。
- 覆盖度：§3 的 #5（单键原地替换）与 #10（stable handle）**都是它**。证据 =
  `crates/components/tests/registry.rs::consumer_epoch_is_unchanged_when_a_named_impl_is_replaced`（验收 A7）。
- **与 Pi 笔记的精确差异**（写下来免得日后误读）：Pi 的 stable handle 是**消费者每访问解析一次**
  的活视图；我们给的是"注册表持有槽位 + 消费者**按需** `get` 当下值"
  （`RegistryHandle::get` 走注册表自己的提交视图，不是注册时的快照）。语义等价（都读到当下值），
  **API 形态不同**：要"拿一次、永远跟随"，今天得自己重取或订阅。

### D-d drain 不做，而且**换值本来就不需要 drain**

- 在飞调用持有的是**当时解析出来的那个值**（`Arc` 语义）⇒ 换值**不打断**它、也不会让它读到
  半成品；旧实现的资源在**最后一个引用落下**时释放（不需要新机制；要观测就自己
  `Arc::downgrade` / `Drop` 信号）。
- 真正的"排水"是**卸载**的语义，不是换值的语义；而卸载排水早就在宿主层落地
  （M2：注销**先结算在飞续延**再 `OP_DISPOSE`；`SDK方案.md` §3 的表）。
- ⇒ drain 作为**独立能力**没有落点。将来若出现"等最后一个在飞请求结束再退场"的需求，
  它属于**那个实现自己**，不属于容器。

---

## 5. 今天怎么写（`SDK指南.md` §8.1 的展开依据）

按推荐顺序：

1. **同一个键 + 两个候选实现 + 组合二选一**（= 论文形态 (1)，harness 的实际做法）：
   `dsh-llm-deepseek` 与 `dsh-llm-pi-ai` **都声明**提供 `deepseek`，**部署只装一个**
   （同时装两个在装载期就被拒：`DUPLICATE_ADAPTER`）；生产里"换实现"= 改组合
   （HMR 只做 volatile config，源码改动走重启）。
2. **一个键 + 实现内部路由**（= 形态 (2) 的实质，**不需要新类**）：该实现（T1 转发，见 D2）
   自己持多个下游 `Iface` / `Dyn`，在方法体内选；注册走 `Disposable`。
   论文那句"one fiber providing the key and dispatching among implementations **registered with
   it**"就是这个形状——今天用 `components::registry` 起槽位、在实现里记账即可。
3. **两个域各一个**（`Ctx::isolate`）：给"同键、互不可见"的场景，**不是选择**。

---

## 6. 落地路径（将来真出现消费者时照此做）

- **位置**：`components` 里新增 `router`（**普通插件**，零内核改动），而不是改 `registry`——
  `registry@1` 的"一个槽位一个值"语义已被 A7 钉住，加"多值"会同时动语义与既有测试。
- **形态**：`router@1` typed 面 + 原生句柄；`register(name, weight, Dyn) -> Disposable`
  （revertible effect，卸载自动出集）；选择策略（round-robin / least-loaded / 显式点名）；
  滚动更新 = 两条目权重过渡 + 旧条目"在飞计数归零再摘"。
- **验收口径（将来照此写）**：路由集 2 个条目；策略命中分布可断言；摘掉一个条目 ⇒ 其后不再
  被选中、而**消费者 epoch 不变**（A7 的扩展）；卸载 router ⇒ 槽位与条目全部回收。
- **跨进程那一块不单独做**：论文自己把"跨进程服务访问"**建立在 broker 之上**（§6.2, p.72）
  ⇒ 它与 ADR 0001 的 T3 是同一批工作，T3 出现时再评估是否需要先有 `router`。

---

## 7. 后果

- 骨架的组件清单不变（`registry` + 领域组件）；`SDK方案.md` §0.3 的取代清单行与 §1 的 G7 指向本文，
  `crates/components/src/lib.rs` 的模块注释也指向本文——"无消费者"从此**有出处**，不必再论证。
- **T1 承担多实现的一切**（组合二选一、或实现内部路由）；SDK 不提供策略，也不承诺均衡。
- 论文不因这个决定被违背：它把 broker 定位成一个**可组合的模式**，而本仓保留了这个模式的全部
  原料——一个键一个提供者、`Disposable` 注册、`Iface` 转发（D2）、原地换值（M7）。

## 8. 何时重新审视

① 出现"**同一个键、多个后端、按流量分配**"的真实需求（多账号配额分摊、多后端容量池、
多模型并跑对比）；
② 出现"**不停机换实现、且要按流量过渡**"的真实 SLA（滚动更新的前置：流量要能被"调走"）；
③ ADR 0001 的 T3 落地（跨进程块天然要它）。

任一成立 ⇒ 按 §6 新开里程碑；判定标准仍然是那条：**内核与 `cordis-abi` 一行不改**。
