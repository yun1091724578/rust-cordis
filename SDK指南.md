# cordis-rs SDK 指南（M10 产物）

> 面向**写插件的人**与**接载体的人**。这里只讲"用的时候会踩到什么"，
> 权威实现以代码与测试为准——每个结论后面都指了可执行的断点。
> 配套文档：[`SDK方案.md`](SDK方案.md)（设计与里程碑）、[`兼容矩阵.md`](兼容矩阵.md)（自动生成）、
> [`原理.md`](原理.md)（论文对照）。

---

## 1. 五分钟：一个插件、一个服务、一个事件

依赖只有一条（包名是 `rust-cordis-sdk`，库名是 `cordis`，宏展开里的路径全是 `::cordis::…`）：

```toml
[dependencies]
cordis = { package = "rust-cordis-sdk", path = "…/crates/sdk" }
tokio = { version = "1", features = ["rt", "rt-multi-thread", "macros"] }
```

```rust
use cordis::core::{BoxFuture, Config, Ctx, Dyn, ExternValue, Result};

// ① 服务：trait + 实现（宏脱糖 async fn、补 Send + Sync、生成伴生类型）
#[cordis::service(name = "greeter", version = 1)]
pub trait Greeter {
    async fn greet(&self, who: String) -> std::result::Result<String, String>;
}

#[derive(Clone)]
pub struct GreeterImpl;

#[cordis::service_impl]
impl Greeter for GreeterImpl {
    async fn greet(&self, who: String) -> std::result::Result<String, String> {
        Ok(format!("hello, {who}"))
    }
}

// ② 事件：名字**逐字**，模式写在属性上
#[cordis::events]
pub enum Tools {
    #[event("tools/before-invoke", mode = "waterfall")]
    BeforeInvoke(Invoke),
}

// ③ 插件：声明进语法树，apply 是普通 async fn
#[cordis::config]
#[derive(Clone, Debug)]
pub struct LoopConfig { pub concurrency: u32 }

#[cordis::plugin(inject("greeter@1"), provision("agentLoop@1"), volatile("cache.dir"), config(LoopConfig))]
#[derive(Clone)]
pub struct AgentLoop;

impl AgentLoop {
    async fn apply(&self, ctx: Ctx, config: Config) -> Result<()> {
        let cfg = LoopConfig::parse(config)?;                 // 挂载期已校验过一遍，这里拿类型化的
        let greeter = GreeterService::of(&ctx)?;              // typed 面（T1）
        let _greeting = greeter.greet("world".into()).await.map_err(to_err)?;
        GreeterService::provide(&ctx, std::sync::Arc::new(GreeterImpl))?; // 提供服务（typed + JSON 两面）
        let _ = cfg.concurrency;
        Ok(())
    }
}

fn to_err<E: std::fmt::Display>(err: E) -> cordis::core::Error { err.to_string().into() }
```

挂载与观测：

```rust
let app = cordis::core::App::new();
let fiber = app.plugin(std::sync::Arc::new(AgentLoop), serde_json::json!({ "concurrency": 2 })).await?;
// 装载器（配置驱动）：cordis_host::loader::loader() 挂成插件，报告在 `loader.report`
```

跑起来的口径（与仓库的测试纪律一致）：

```powershell
.\cargo.cmd test --workspace --offline          # 全量（连跑必须同结果）
.\cargo.cmd run -p demo --offline               # 端到端；examples/demo/compare.ps1 逐字节比对
```

---

## 2. 三档 tier 与信任模型（D1）

| 档 | 载体 | 装卸 | 能力面 | 信任 | 典型用途 |
|---|---|---|---|---|---|
| **T1** in-process native | Rust crate，链进宿主 | 无（编译期） | **全量**（含 typed 服务、流式） | 完全信任 | 领域骨架、性能热点、流式提供者 |
| **T2** in-process dylib | `dlopen` / `LoadLibrary` | 丢文件即装 | ABI v2（单入口 + 7 个 op 码 + 22 个宿主函数：v1 的 11 + v2 的 11） | **完全信任**（同地址空间） | 第三方组件（受审计） |
| **T3** out-of-process / Wasm | 计划中（ADR，M8） | 丢文件即装 | 同一套 op 协议 | **不提供沙箱** | 模型自生成组件（**需 OS 边界，SDK 不承诺**） |

**两条硬话**：

1. **模型自生成组件永远不得是 T2**——同地址空间里它可以直接拿到底层对象，任何"能力检查"都失效；
2. **T3 是"载体可换"的证明，不是信任边界**。本仓能说清的是**接口透明性**与受限导入：
   ABI 的形状（单入口 + op 码 + 全部内存归宿主 + JSON）让换载体只需要重写适配器，
   内核一行不改（`LoadedPlugin` 是唯一适配器；`rust-cordis-abi` 零平台依赖）。

**T3 必须异步**：跨进程的调用点只能 await，所以 M2（入站挂起/结算）是 T3 的前置条件。

---

## 3. 键、名字与声明

| 东西 | 规则 | 为什么 |
|---|---|---|
| **服务键** | `<name>@<major>`（`GreeterService::KEY` = `"greeter@1"`） | §6.6 路线 1：同名不同 major 的服务**可以共存**；版本不匹配表现为**停在 Pending**，而不是"读到别的版本" |
| **inject / provision** | 同一口径写字面量（`inject("greeter@1")`）；裸标识符当裸名（`inject(tools)` ⇒ `"tools"`） | 宏**不替你猜** major（那是 `#[cordis::service]` 的 `KEY` 的职责） |
| **provision 空 = 不透明** | 不声明 = **跳过一切检查**（Def 48/76 不报警）；声明了才是承诺 | 既有插件大多不声明；把"空"读成"声明了空集"会对每个插件报警 |
| **事件名** | **逐字**，绝不加前缀/命名空间 | ABI 侧的拦截名是硬编码的（`tools/before-invoke`）；改名即静默失配 |

---

## 4. 两张"同名不同物"的 interception

论文 **Def 26/27** 的 interception（"访问控制/元数据"）与 §6.4 的 "interception primitive"
（**访问中介**：一个把类型化声明与访问器一起产出的机制）**同名不同物**。本仓的对应物：

| 论文 | 本仓 | 用法 |
|---|---|---|
| Def 26/27：`σ(k)( d(k) ⊕_k ι(k) )`，**提供者侧** | `Ctx::intercept(key, config)` + 声明链 `inject` 的配置 | 调用点上算 merge（**右偏**：最近 intercept 层 > 祖先 > 组件声明），经 ABI 载荷的 `meta` 字段下发；**策略留给提供者** |
| §6.4：访问中介 | `#[cordis::service]` 宏（声明与访问器一起产出） | typed 方法调用**不是**反射回退，没有"无类型兜底"路径 |

M3 的红测试钉住了第一条的全部语义（`crates/core/tests/intercept_meta.rs`）；
第二条的边界见下一节。

---

## 5. T1/T2 typed 边界规则（**最容易踩的一条**）

- **`TypeId` 不跨编译单元稳定** ⇒ typed 访问器（`GreeterService::of`）**只在 T1 有效**：
  同一个二进制（同一 crate 图）里，提供者与消费者的 `dyn Trait` 是同一个 `TypeId`。
- **T2/T3 一律走 JSON 面**：`ctx.call_iface("greeter@1", 1, "greet", json!(["world"]))`。
  typed 服务自带 JSON 面（`adapter`），两条通道**指向同一份实现**——
  所以你不需要为 T2 调用者额外写一个适配器。
- **同一个 key 不允许同时有 T1 与 T2 两个提供者**（内核不变量：一个键一个实现）。
  需要同时服务两档时（例如 `tools` 既要在进程内被 typed 调用、又要收动态库的注册）：

  > **规则**：T1 侧的实现持一个 `IfaceRef`（`ctx.require_as::<IfaceRef>(…)` 或构造时注入），
  > 由它用 `ctx.call_iface` 把调用**转发**给 T2 提供者。

  不这么做的话，spike 会在运行期撞 `ResolveError::TypeMismatch`——
  这条错误就是为"看得见类型不对"而加的（M6 前它伪装成 `NoInject`）。

---

## 6. 语义陷阱清单（每条都有测试）

| 现象 | 为什么 | 怎么办 |
|---|---|---|
| `Dyn::new("x")` 取不回 `String` | 装箱的是 `&'static str` | 传自有类型：`Dyn::new("x".to_string())` |
| `Dyn::new(arc)` 取回 `Arc<Arc<T>>` | `Any` 要求 `Sized` | 用 `Dyn::shared(arc)`；typed 服务用 `Iface::<dyn T>(arc)` |
| 监听器返回 `false` 却不短路 | JS 语义：`false`/`null`/`undefined` **不是** bail 值 | 要表态就返回真值；`Ok(None)` 是"不表态" |
| waterfall 的尾巴收不到载荷 | 内核的 tail 是**零参** future（`Option<BoxFuture<Ret>>`） | 真正的"链尾"是最后一个监听器；tail 只用来给链一个默认答案 |
| 插件类型不实现 `Clone` 编译不过 | `Plugin::apply` 返回 `'static` future，借不了 `&self` | `#[derive(Clone)]`；共享状态放 `Arc` |
| 配置里写了 `!!js` 但没生效 | `interpolation` 模式 / 表达式算不出来（fail loud，不降级） | 看 `loader.report.interpolation{mode,failed}`；`injected` 模式必须注入引擎 |
| `!!js` 表达式的结果是旧值 | 表达式**不缓存**（每次决策现算） | 依赖 ctx 的表达式本来就应该跟着上下文走 |
| `!!js` 的 `?.` 用在**调用**上报错 | 内置子集只支持成员访问上的 `?.` | 注入真引擎（`js_evaluator` 挂点）或改写表达式 |
| 一个插件装了别的 key 却不声明 | Def 48：装了没声明的 | `provision("…")` 声明，或保持"不透明"（不声明即跳过检查） |
| 配置校验只报 `invalid type: …`，看不出哪个字段 | `serde_json::from_value` 的消息里**没有字段路径** | `#[cordis::config]` 的 `parse` 会先做一遍**顶层字段体检**，把字段名放进 `issues[0]`（`invalid config:\n  - concurrency: …`）；带 `#[serde(deserialize_with)]` 的字段可能被误点名，此时以 serde 的原话为准 |
| `ctx.set` 报 `in multiple fibers` | 只有**提供者 fiber** 能写自己的实现 | 由提供者写；要允许"别人替我换值"就用 `components::registry`（它代持提供者身份） |
| 插件自己装载完了，别人 `ctx.get` 它刚 `provide_iface` 的接口却是 `None` | `Ctx::get` 是**严格读**（要求提供者 `ACTIVE`），而"边装载边登记"发生在 `apply` 里（那时还在 `LOADING`） | 基础设施读用宽容读 `ctx.store.try_get(&ctx, name, false)`（`harness-spike` 的 `tools@1` 就这么解析 T2 的 iface；证据：`examples/harness-spike/src/lib.rs` 的 `ToolsImpl::register`） |
| 插件重载后新实例报"这个名字已经登记过" | 目录是**另一个插件的状态**：内核只回收你自己 `provide` 的东西，你登记到别人表里的条目不会跟着消失 | 在自己的 cleanup `effect` 里显式撤销（`unregister`）；证据：`harness-tool` 的 ⑦ 与 spike 第 9 步 |
| 放行一个挂起的插件调用，结果它一直不结算 | 宿主**先登记**挂起条目、插件随后才记下 `dispatch_id`（D3 的先登记后调用）⇒ 只看 `pending_len` 会"放空炮" | 等**两侧都到位**（`pending_len == 1` 且插件侧的"已武装"计数 ≥ 1）；证据：spike 的 `run_session` |
| 行 fiber（装载器挂出来的插件）卸载后，暂存副本还在磁盘上 | 行 fiber 是**父 fiber 的一个 effect**；父若是 `App::root`（永不 dispose），那些闭包就永远强持行 fiber ⇒ `Fiber → Runtime → Runtime.plugin` ⇒ `Arc<LoadedPlugin>` 活着 | **装载器自己挂成一个插件 fiber**（`loader()` / spike 的 `harness-loader`），收尾拆它；证据：spike 第 10 步的真删除断言（附录 E 缺陷 6） |
| 卸载一个"依赖装载器行服务的原生 fiber"之后，那一行的暂存副本不回收（附录 E 缺陷 5，**已闭环**） | 旧实现在 `ctx → fiber` 上是**强引用**：被留下的 ctx（依赖者提交视图里的 `Impl`、宿主的 `Instance.ctx`、派生 intercept ctx）会把提供者的 fiber 一起吊住 ⇒ `Fiber → Runtime → Runtime.plugin` | M4 的 G18 把它改成 `Weak<Fiber>`（根 fiber 由 `Store.root_fiber` 强持）⇒ 链断。回归：`crates/host/tests/loader_release.rs` 四变体（判据 = 插件对象是否被 drop）；二进制层的等价观测 = demo 的 6.7 现在**真卸载**调用者 + spike 第 10 步真删除 |
| 用 `sdk::build_plugin_*` 在一个进程里造**第二个** ABI 形状插件，第二个的 `apply` 静默不生效 | 它的 apply 槽是**进程级**的（真插件走 `cordis_plugin!` 才每个插件一个入口） | 一个进程只造一个（宿主夹具/spike/示例都按这条写）；要多个就用 `cordis_plugin!` 或各放一个动态库；证据：`crates/host/tests/sdk_async_iface.rs` 的"为什么这里只有一个测试" |
| T2 插件的服务方法要等网络/子进程，却只能答同步值 | `ctx.provide_iface` 只登记**同步**方法体（`Handler`）；异步入站（挂起 + `ctx_fulfill`）当时只有事件监听器那条路 | **已补**：`ctx.provide_iface_async`（`AsyncIfaceFn`，方法体拿到 `dispatch_id`）——返回 `Pending` 就挂起，返回 `Reply` 就立即答复，与 `on_async` 共用同一套机制；证据：`crates/host/tests/sdk_async_iface.rs`（七条断言，含大结果上限与卸载排水） |
| 流式推送返回 `Pending` 却被当成"推成功了" | `Pending` 的准确含义是**块没有被接受**（窗口 4 满了） | 重推同一块（重复推会得到重复数据）；`StreamPush::{Sent, Pending, Gone}` 三态必须各自处理；证据：`crates/host/tests/abi_stream.rs` 的 100 块验收（插件观测的未归还峰值恰为 4） |
| 新加的接口/键在装载报告里没有、或 `require` 报 `NoInject` | ① `provision:` 是 M5 的"声明即能力"（A5 的预挂载清单来源），只 provide 不声明会被记为 Def 48；② 更常见的真凶是**产物没更新**（构建失败/旧副本） | 把键补进 `cordis_plugin!` 的 `provision:`；`NoInject` 先查"提供者到底在不在"（构建日志 + 产物里搜字符串）；证据：本轮 M8-后续② |

---

## 7. MSRV 与特性矩阵

| 项 | 值 | 依据 |
|---|---|---|
| MSRV | **1.85** | `Waker::noop()`（G19）比 `offset_of!`（1.77）更紧；每个成员 crate 都写 `rust-version.workspace = true` |
| 内核 | **零 unsafe**（`#![forbid(unsafe_code)]`） | A8 的判据 |
| `rust-cordis-abi` | **零平台依赖**（只依赖 serde_json） | 载体可换的前提 |
| `rust-cordis-host` | `serde` / `serde_json` / `serde_yaml` / `tokio` / `libloading`（**已删**，见 M0） | |
| tokio 特性 | `rt`、`rt-multi-thread`、`macros`、`sync`、`time`、`test-util` | `test-util` 供 `pause/advance` 类测试 |
| 过程宏 | `syn = 3`（`full`）/ `quote` / `proc-macro2` | 仅 `rust-cordis-macros` 依赖 |
| 构建 | **离线**：`.\cargo.cmd test --workspace --offline`（`CARGO_HOME` 指到工作区自带的缓存） | 见 `cargo.cmd` / `run-tests.cmd` |
| ABI 面 | 单入口 + **7 个 op 码**（`OP_APPLY/DISPOSE/CALLBACK/CLEANUP/PING/VOLATILE_PATHS/VOLATILE_APPLY`）+ **6 个状态码**（`OK/ERR/TOO_SMALL/UNSUPPORTED/PENDING/STREAM_GONE`）+ **22 个宿主函数**（v1 的 11 个 + v2 追加 11 个） | 冻结值与矩阵见 [`兼容矩阵.md`](兼容矩阵.md) |

---

## 8. TS 移植对照（cordis → cordis-rs）

| 上游（TS） | 本仓 | 备注 |
|---|---|---|
| `ctx.plugin(plugin, config)` | `App::plugin(&self, Arc<dyn Plugin>, Config)` | 订阅式挂载：依赖齐了才激活 |
| `ctx.on(name, listener)` | `Ctx::on` / `#[cordis::events]` 的 `on_<variant>` | typed 监听器与动态库监听器看到**同一份 JSON** |
| `await next(args)` | `Next::call(args)` / `TypedNext::call(payload)` | 惰性下游；不调 = 短路 |
| `ctx.provide(name, value)` | `Ctx::provide` / `<Trait>Service::provide` | M6 起 typed 服务两条通道同源 |
| `ctx.set`（原地换值） | `Ctx::set` + `components::registry` | 消费者 epoch **不变**（A7） |
| `ctx.isolate` / `ctx.intercept` | 同名 API（M1 起 ABI 也有） | ABI v2 |
| `ctx.filter` 一类的 thisArg/carrier | `Ctx::filter` / `filter_subtree` / `in_scope_of` | 只做作用域轴；per-service thisArg 全量语义**不做** |
| `ctx.inject(['x'], cb)` | `Ctx::inject(Inject, cb)` | 迟绑定可选能力 |
| `AbortSignal` | `CancelToken` + `Call.cancel` + `Ctx::abort` | 级联取消（含后代派发） |
| `declare module Events` | `#[cordis::events]` | 名字**逐字** |
| `Service[check]` | `Ctx::provide_checked` + `Plugin::provision` | 就绪谓词 / 「声明即能力」 |
| 配置里的 `!!js` | 内置子集 或注入 `js_evaluator` | `interpolation: "builtin" \| "injected"` |
| `Schema` 校验 | `#[cordis::config]` + `Plugin::validate` | 挂载期报 `invalid config:` |

### 8.1 同键多实现与换实现（ADR 0002）

论文的 broker（一个键、多个后端、按策略派发）在本仓**不进骨架**：负载均衡 / 滚动更新 /
跨进程三块都还没有消费者，而它的实质今天就能写出来，不需要新机制（逐条判定与一手证据见
`docs/adr/0002-broker.md`）。按推荐顺序：

1. **一个键、两个实现、组合二选一**（论文形态 (1) "exclusive binding"）：两个实现都**声明**
   同一个键，**部署只装一个**——同时装两个会被拒（一个键在一个域里容不下两个提供者）；
   "换实现"= 改组合，不是改代码。
2. **一个键 + 实现内部路由**（broker 的实质，**不需要新类**）：路由实现自己提供该键，每个
   后端用 `components::registry` 的**具名槽位**登记（`put` 返回 `Disposable` ⇒ 卸载自动出集，
   这正是论文的 "implementations registered with it" + "revertible effect"），在方法体里
   按策略选一个。
3. **`Ctx::isolate` 各域一个**：给"同键、互不可见"的场景——这是**隔离**，不是**选择**。

**换实现**用 `components::registry` 的 `replace`（原地换值：消费者 epoch 不变、状态不丢，A7）；
**不要**"卸载再挂"——那会重启所有依赖者。**drain 不需要**：在飞调用持有的是当时解析出来的
那个值（`Arc` 语义），换值不打断它，旧值在最后一个引用落下时释放（真正的排水是**卸载**的
语义，宿主层已有）。

---

## 9. 流：一次调用一串值（ADR 0001 §5）

LLM 的 token 流、长输出的分块返回——"一次调用产出一串值"在这里是一等公民：调用方拿到
`AsyncStream`，提供者往里推块，中间隔着 **4 块**的窗口（背压的全部）。

```rust
// 消费侧（T1）：开流、逐块取。Ok(None) = 正常收尾；Err = 失败 / 被宿主收回
let mut stream = ctx.call_iface_stream("llm@1", 1, "generate", json!({"prompt": "hi"}))?;
while let Some(chunk) = stream.next().await? {
    print!("{chunk}");
}
```

提供者有两种写法：

1. **不写流式代码**（默认实现）：普通方法返回一件值 ⇒ 调用方看到"**长度 1 的流**"。
   于是同一个接口方法既能按值调、也能按流调，调用方不必分叉。
2. **真流式**（T2 侧必须用 `provide_iface_async`——同步方法体拿不到 `dispatch_id`）：

```rust
ctx.provide_iface_async("llm@1", 1, vec![("generate", Arc::new(move |args, _meta, dispatch_id| {
    // 推一块：窗口内 Sent；满了 Pending（**块没被接受**，等 credit 重推）；Gone = 流没了。
    match ctx.stream_push(dispatch_id, json!("tok"), move |credit| match credit {
        StreamCredit::Window(_) => { /* 又有位置了：接着推（递归 pump） */ }
        StreamCredit::Gone => { /* 消费者收场 / 宿主卸载：停手 */ }
    })? {
        StreamPush::Sent => {}
        StreamPush::Pending => return Ok(AsyncReply::Pending),      // 挂起，等 credit
        StreamPush::Gone => return Ok(AsyncReply::Reply(json!(null))),
    }
    // 收尾二选一：stream_close（消费者读完缓冲拿到 Ok(None)）
    //             stream_fail（消费者**立刻**拿到 Err，缓冲丢弃）
    ctx.stream_close(dispatch_id)?;
    Ok(AsyncReply::Pending)
}))])?;
```

"推完一块再推下一块"的完整状态机（含递归 `pump()` 与 `Arc<Self>` 写法）见
`examples/plugins/harness-tool/src/lib.rs` 的 `Streamer`——**真 DLL 的流式提供者**就是这么写的。

**同进程（T1）的提供者**更简单：直接实现 `ExternService::call_stream`，拿 `StreamSink` 用
`sink.send(chunk).await` 自己等窗口（零 ABI 成本），最后 `sink.close()`。

四条必须记住的语义：

- `Pending` = **块没有被接受**，提供者必须重推这一块（把 `Pending` 当成功 = 丢数据）。
- **窗口 4 是背压的全部**：`streams_len` / `pending_len` / `waterfall_len` 归零 = 流收干净了。
- 收尾**恰好一次**：`close` / `fail` / 消费者 `drop` 谁先到谁生效；`drop` 就是取消
  （提供者下一次推送拿到 `Gone`，等待中的 credit 回调收到 `StreamCredit::Gone`）。
- 卸载 / 取消 / 超时 / 消费者离场都**先排水**：等待中的 credit 先被驳回、流以**错误**结束，
  然后才 `OP_DISPOSE`——"没读完"永远不会被静默当成"读完了"。

**边界**（ADR 0001 的 D7）：跨 ABI 的**提供**流已支持；跨 ABI 的**消费**仍留给 T3——
T2 插件拿到的 `sdk::Ctx` 只有提供侧的三个方法。

---

## 10. 迁移清单（从 TS 搬到本仓时按顺序核）

1. 把 `inject` 的键与服务的 `name@major` 对齐（版本不匹配只表现为"停在 Pending"）；
2. 事件的**名字**逐字抄过来（一个字符都别改）；
3. `!!js` 表达式过一遍：算不出来的形态要么改写、要么注入引擎；
4. 服务方法写成 `async fn`、参数用自有类型（可 serde）、返回 `Result<T, E: Display>`；
5. 需要原地换实现的适配器：用 `components::registry` 起槽位，别用"卸载再挂"（那会重启所有消费者）；
6. 动态库插件：`provision` / `volatile` 声明写进 `cordis_plugin!`；`ctx.call_iface` 走 JSON 面；
7. 卸载路径核一遍：注册都是 fiber 的 effect（`Disposable`），跑不跑都幂等；
8. 最后跑 `.\cargo.cmd test --workspace --offline` 与 `examples/demo/compare.ps1`。

---

## 11. 仓外消费工程（R2：首发低层 ABI）

首发支持路径已定为**低层 ABI + 固定 revision 的源码依赖**。标准 typed 门面
`rust-cordis-sdk` 仍是 `publish = false`，不在首发范围；下面是已经实际跑通的仓外宿主
写法，证据见仓外工程 `../desolate/R1-REPORT.md`。

### 11.1 独立 workspace，不继承本仓

消费工程自己声明 workspace，并直接依赖固定 revision 的源码：

```toml
[workspace]
resolver = "2"
members = ["host", "plugins/note-v1"]

# host/Cargo.toml
[dependencies]
cordis-core = { package = "rust-cordis-core", path = "../../rust-cordis/crates/core" }
cordis-host = { package = "rust-cordis-host", path = "../../rust-cordis/crates/host" }
serde_json = "1"
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }

# plugins/note-v1/Cargo.toml
[lib]
crate-type = ["cdylib"]

[dependencies]
cordis-abi = { package = "rust-cordis-abi", path = "../../../rust-cordis/crates/abi" }
serde_json = "1"
```

不要把消费工程放进 `rust-cordis` 的 workspace，也不要用 `workspace = true`
继承本仓依赖；那会掩盖“仓外工程是否能独立编译”的问题。用脚本固定 revision，
例如 `git -C ../rust-cordis rev-parse HEAD` 与 `git status --porcelain -- crates/`
一起检查。

### 11.2 构建顺序与加载

1. 先构建 cdylib 插件；`cargo run -p host` 不会自动构建插件：
   `cargo build -p desolate-note-v1 -p desolate-note-v2`
2. 宿主从自己的可执行文件目录定位插件：
   `DLL_PREFIX + name + "." + DLL_EXTENSION`（Windows 是 `name.dll`，
   Linux 是 `libname.so`）。
3. 加载与调用：

```rust
let plugin = std::sync::Arc::new(cordis_host::LoadedPlugin::open(&plugin_path)?);
let fiber = app.plugin(plugin, serde_json::json!({ "root": root })).await?;
let value = app.root
    .call_iface("desolate.index", 1, "scan", serde_json::json!({}))
    .await?;
fiber.update(serde_json::json!({ "root": root, "label": "reload" })).await?;
fiber.dispose().await;
app.close().await;
```

真实宿主的失败路径也要断言：空配置在挂载期失败、缺失 root 返回可读错误、
卸载后 `call_iface` 找不到服务、`loaded_instances()` 回到 0。

### 11.3 已验证范围与边界

- 已验证：独立编译、低层 ABI 宏展开、真 DLL 装载、调用成功 / 失败、卸载、
  `Fiber::update` 重载、v1→v2 顺序升级、`App::close`；31 篇自造样例完成索引、
  搜索、重载、升级、标签过滤与报告写出；`detailed-probe` 覆盖
  `call_iface_detailed` / `dispatch_detailed` / `next_detailed` 的正常结果、
  同步启动错误、provider 回调错误、`HOST_TIMEOUT`、卸载/取消结算与卸载后不重复
  投递；窄策略消费者按 `category` / `certainty` 选择
  `accept` / `fail_fast` / `degrade_unknown` / `unavailable`，不解析文本、不自动重试。
- 未验证：registry typed SDK、第三方真实数据与长期维护反馈、完整重试/补偿策略
  （还需副作用类别、幂等是否生效、截止时间与预算）、原地 HMR、历史产物跨版本
  兼容。第三方复现是额外证据，不是必选门禁。
- 本机 Windows GNU 需要 `rust-cordis/cargo.cmd` 的 self-contained MinGW 与 ASCII
  target 目录；普通环境可以直接用 `cargo`，但必须显式设置插件路径和构建顺序。

### 11.4 真实 agent 首链路

`../desolate` 的 `agent` 任务是一个可运行的最小消费链，不是新的框架层：

```powershell
.\scripts\agent.ps1 -Mode ok
.\scripts\agent.ps1 -Mode stream_fail
.\scripts\agent.ps1 -Mode tool_timeout
.\scripts\agent.ps1 -Mode normal_timeout
.\scripts\agent.ps1 -Mode tool_error
.\scripts\agent.ps1 -Mode model_start_error
```

它覆盖“请求 → 模型 token 流 → 归档搜索工具 → 结果记录 → 关闭”。正常路径收到
3 个 token 和 8 条归档命中；`stream_fail` 在部分输出后失败；`tool_timeout` 让
工具先写出搜索结果、再不返回，宿主用自己的 100ms 截止时间记录
`timeout_unknown` 和 `side_effect_observed=true`；`normal_timeout` 证明普通
mode 下的真实超时仍按 `Err(elapsed)` 记为 `timed_out/unknown`，不是按 mode
解释；`tool_error` / `model_start_error` 验证普通失败也会形成唯一终止并释放
资源。每次运行还写
`reports/*.events.jsonl`，终止事件带按消息保存的部分快照和独立的
`status` / `certainty`。报告见 `../desolate/AGENT-REPORT.md`，事件协议见
[`docs/agent-stream-protocol.md`](docs/agent-stream-protocol.md)。

接入结论：失败流只带文本原因，已经消费的 token 可见，未读缓冲会被丢弃；需要
保留全部部分输出时要单独改流错误协议。`DispatchPolicy` 不是宿主调用的通用截止
时间，agent 宿主必须自己设 deadline，不能把看门狗当执行隔离。当前 JSONL sink
仍是内存聚合后一次写入，不能证明在线增量交付；链路没有自动重试、账本、broker
或进程外载体。
