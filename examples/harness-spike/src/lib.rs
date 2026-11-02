//! harness-spike 的**领域层**（M11）：一个够小的 agent harness，两层落地共用同一份。
//!
//! # 它要证明什么
//! 论文里的 harness（agent 循环、工具、会话日志、子代理树、拦截器）**不需要新内核机制**：
//! 这里用到的面全部是文档化的（`SDK指南.md` 的第一/第二档），一行内核代码都没有为了
//! spike 改过。三样东西是领域词汇，**不进 SDK**：`tools` / `sessions` / `llm`。
//!
//! # 两层怎么分工（D9）
//! - **`cargo test`（无 DLL）**：本模块 + `tests/e2e.rs`。作用域隔离、typed 服务与事件、
//!   replay 适配器的确定性、原地换适配器的 epoch —— 全部走进程内路径（内存版 `sessions`）。
//! - **`cargo run -p harness-spike`（真 DLL）**：`src/main.rs`。`sessions` 换成
//!   `harness_tool.dll` 提供的 T2 实现（**载体钉死为 T2**，否则重载复用同一个
//!   `Arc<dyn Plugin>` ⇒ seq 连续是假通过），再加 A4/A9/A10/A11 的断言。
//!
//! # 三个服务（都是 `#[cordis::service]`，键 = `<name>@<major>`）
//! | 键 | 是什么 | 谁提供 |
//! |---|---|---|
//! | `tools@1` | 工具目录（名字的真相） | `tools_plugin()`（T1） |
//! | `llm@1` | LLM 适配器 | **registry 槽位**（⇒ 可原地换，A7） |
//! | `sessions@1` | append-only 会话日志（`seq` 连续） | 测试层 = `memory_sessions_plugin()`；二进制层 = DLL |
//! | `agent@1` | agent 循环本身 | `AgentLoop`（T1，`#[cordis::plugin]`） |
//!
//! **执行的真相在钩子上**：工具的实现是 `tools/invoke`（bail）上的监听器，
//! 目录只负责"名字与可见性"（`names(owners)` 按作用域链过滤）。
//! 于是**同名工具互不可见**是内核的作用域轴给的，而不是目录里手写的 if 判断。
//!
//! # 事件（`#[cordis::events]`，名字逐字）
//! - `agent/pre-step`（emit）：每轮开始，只有观察；
//! - `tools/before-invoke`（waterfall）：**调用前**的拦截链，链尾把载荷原样交回；
//!   动态库拦截器在 `next` 回来后改写它（A9）；
//! - `tools/invoke`（bail）：真正的执行，第一个表态的监听器赢（同步的、挂起的都算）。
//!
//! # 作用域（M4）
//! [`agent_scope`] 给每个 agent 一个作用域 ctx：只放行"**自己这一支 + 祖先**"的注册。
//! 与 `Ctx::filter_subtree` 的差别只有继承方向——子代理 `bind_scope_parent` 之后要
//! 看得见**父**的工具，而 `filter_subtree` 只管自己这根子树。

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, Weak};

use cordis::core::{
    plugin_fn, with_provision, App, BoxFuture, Config, Context, Ctx, Dyn, Error, ExternValue,
    Fiber, FiberState, Iface, IfaceRef, Inject, Plugin, Ret, ScopeAnchor,
};
use cordis::typed::events::decode_ret;
use cordis::{config, events, plugin, service, service_impl};
use cordis_components::registry::{self, RegistryHandle};
use serde_json::{json, Value};

// ============================================================ 领域词汇

/// 一次工具调用在链上流动的形态：**两个事件共用同一个载荷类型**。
///
/// 拦截器改写 `input`（或压一个 `stages` 标记），执行者填 `output`——
/// 于是"调用前改写"与"结果改写"在同一个对象上就能观察（A9）。
#[derive(Clone, Debug, PartialEq, ::cordis::serde::Serialize, ::cordis::serde::Deserialize)]
#[serde(crate = "::cordis::serde")]
pub struct Invoke {
    pub agent: String,
    pub tool: String,
    pub input: Value,
    /// 执行结果；拦截链上还是 `Null`。
    pub output: Value,
    /// 经过的拦截链（每个拦截器压一个标记）。
    pub stages: Vec<String>,
}

impl Invoke {
    pub fn plan(agent: &str, tool: &str, input: Value) -> Self {
        Self {
            agent: agent.to_string(),
            tool: tool.to_string(),
            input,
            output: Value::Null,
            stages: Vec::new(),
        }
    }

    pub fn json(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

/// 每轮开始的载荷。
#[derive(Clone, Debug, PartialEq, ::cordis::serde::Serialize, ::cordis::serde::Deserialize)]
#[serde(crate = "::cordis::serde")]
pub struct Step {
    pub agent: String,
    pub round: u32,
}

/// LLM 适配器给出的一个计划步（"智能"的全部内容就是这个序列）。
#[derive(Clone, Debug, PartialEq, ::cordis::serde::Serialize, ::cordis::serde::Deserialize)]
#[serde(crate = "::cordis::serde")]
pub struct Plan {
    pub tool: String,
    pub input: Value,
}

/// spike 的事件面。**名字逐字**——ABI 侧的拦截名是硬编码的，改了就静默失配。
#[events]
#[derive(Clone, Debug)]
pub enum HarnessEvents {
    /// 每轮开始（`emit`：只有观察）。
    #[event("agent/pre-step")]
    PreStep(Step),
    /// 调用前（`waterfall`：下游可改写载荷，链尾的决定回流）。
    #[event("tools/before-invoke", mode = "waterfall")]
    BeforeInvoke(Invoke),
    /// 真正的执行（`bail`：第一个表态的赢）。
    #[event("tools/invoke", mode = "bail")]
    RunTool(Invoke),
}

/// 一次会长的结果（`Agent::run` 的返回值）。
#[derive(Clone, Debug, PartialEq, ::cordis::serde::Serialize, ::cordis::serde::Deserialize)]
#[serde(crate = "::cordis::serde")]
pub struct Report {
    pub agent: String,
    /// 这次用的是哪个适配器（A7：原地换过之后必须变）。
    pub adapter: String,
    pub rounds: u32,
    /// 真正执行到的工具（顺序）。
    pub tools: Vec<String>,
    /// 没有人应答的工具（**隔离的反证**：不是"跑成了别人的"）。
    pub unanswered: Vec<String>,
    /// 本次会长写进 `sessions` 的 `seq`（连续性的证据）。
    pub seqs: Vec<u64>,
    /// 拦截链上留下的标记（A9 的证据）。
    pub stages: Vec<String>,
}

// ============================================================ 三个 T1 服务

/// 工具目录：**名字的真相**在这里，**执行的真相**在 `tools/invoke` 的钩子上。
///
/// 线上形状（JSON 面，参数信封 `[a1…aN]`）与 typed 面逐字对应。
#[service(name = "tools", version = 1)]
pub trait Tools {
    /// 登记一个工具。`owner` 是登记者的作用域 uid（`0` = 全局；
    /// 作用域 ctx 上用 `ctx.scope().map(|a| a.uid())` 取）。
    ///
    /// `iface` 非空 ⇒ **T2 提供者**：T1 侧当场解析它并留下一个 [`IfaceRef`]
    /// （D2 的"工具的 T1 服务持引用把 T2 的注册转发进来"）；解析不到就报错。
    async fn register(
        &self,
        owner: u64,
        name: String,
        desc: String,
        iface: Option<String>,
    ) -> Result<(), String>;

    /// 撤销一条登记。**提供者卸载时必须走这一步**：目录是**另一个插件的状态**，
    /// 内核只回收"你自己 provide 的东西"——不显式撤销，重载后的新实例就会撞上
    /// 自己留下的那条重名登记。
    async fn unregister(&self, owner: u64, name: String) -> Result<(), String>;

    /// 按作用域链列出可见的工具名（`owners` 见 [`owner_chain`]）。
    async fn names(&self, owners: Vec<u64>) -> Result<Vec<String>, String>;

    /// 调一个**带 iface 的**工具（T2 转发）。`owners` 的用途与 [`Tools::names`] 相同：
    /// 同名工具分属不同作用域时，由调用方的链选定。目录里的 iface 约定只有一个方法
    /// `call`（入参是裸 JSON，与 `greeting` 那种写法一致，不走信封）。
    ///
    /// 钩子型工具（`iface = None`）不从这里走——它们由 `tools/invoke` 派发，
    /// **作用域在派发期裁决**。
    async fn call(&self, owners: Vec<u64>, name: String, input: Config) -> Result<Config, String>;
}

/// LLM 适配器。它由 **registry 槽位**提供 ⇒ 可以原地换掉而依赖者 epoch 不变（A7）。
#[service(name = "llm", version = 1)]
pub trait Llm {
    /// 适配器标识（脚本 / 录制 / 真实模型……）。
    async fn adapter(&self) -> Result<String, String>;

    /// 给出这次 prompt 的计划。
    async fn plan(&self, prompt: String) -> Result<Vec<Plan>, String>;
}

/// 会话日志：**append-only、`seq` 连续**（A11）。
///
/// 线上形状：`append` 的入参是 `[record]`，返回**分配的 `seq`**（一个数字），
/// 并且**返回即已落盘**——这就是 flush 屏障（二进制层的实现是写 + flush + sync）。
#[service(name = "sessions", version = 1)]
pub trait Sessions {
    async fn append(&self, record: Config) -> Result<u64, String>;
    async fn tail(&self, count: u32) -> Result<Vec<Config>, String>;
    /// 下一个待分配的 `seq`（**重载后必须能从日志里恢复出来**）。
    async fn next_seq(&self) -> Result<u64, String>;
}

/// agent 循环本身（每个 agent 一份实例）。
#[service(name = "agent", version = 1)]
pub trait Agent {
    /// 跑一次会长：llm 给几步就跑几步。
    async fn run(&self, prompt: String) -> Result<Report, String>;
    /// 当下读到的适配器标识（每轮现读 ⇒ A7 的新值可见）。
    async fn adapter(&self) -> Result<String, String>;
    /// 本 agent 作用域链上可见的工具名（目录面）。
    async fn tools(&self) -> Result<Vec<String>, String>;
}

// ============================================================ 作用域

/// 一个 agent 的作用域 ctx：只放行「**自己这一支 + 祖先**」的注册。
///
/// 谓词的入参是**注册者**的 ctx（对齐上游 `events.ts:76-85`）；`None`（没有作用域）
/// 的注册由内核的规则 2 兜底（永远参与）——全局拦截器就注册在无作用域的 ctx 上。
pub fn agent_scope(base: &Ctx) -> Ctx {
    // 谓词需要锚点本身，而锚点由 `filter` 造出来 ⇒ 先建槽、再回填（同 `filter_subtree`）。
    let slot: Arc<Mutex<Option<Arc<ScopeAnchor>>>> = Arc::new(Mutex::new(None));
    let slot_in = slot.clone();
    let scoped = base.filter(Arc::new(move |hook_ctx: &Ctx| {
        let Some(mine) = slot_in.lock().unwrap().clone() else {
            return false;
        };
        match hook_ctx.scope() {
            Some(hook_scope) => {
                hook_scope.uid() == mine.uid() || hook_scope.is_ancestor_or_self_of(&mine)
            }
            None => false,
        }
    }));
    *slot.lock().unwrap() = scoped.scope();
    scoped
}

/// 作用域链（`0`（全局）→ 自己 → 祖先……），供目录这类"按可见性过滤"的查询用。
pub fn owner_chain(ctx: &Ctx) -> Vec<u64> {
    let mut chain = vec![0u64];
    let mut cursor = ctx.scope();
    while let Some(anchor) = cursor {
        chain.push(anchor.uid());
        cursor = anchor.parent();
    }
    chain
}

// ============================================================ tools@1 的实现

#[derive(Clone)]
struct ToolEntry {
    name: String,
    desc: String,
    iface: Option<String>,
    /// T2 转发面（D2）：**T1 侧持引用**，T2 卸载后调用会得到可读错误。
    target: Option<IfaceRef>,
}

struct ToolsImpl {
    ctx: Weak<Context>,
    entries: Mutex<BTreeMap<(u64, String), ToolEntry>>,
}

impl ToolsImpl {
    fn live_ctx(&self) -> Result<Ctx, String> {
        let ctx = self
            .ctx
            .upgrade()
            .ok_or_else(|| "tools@1 的提供者已卸载".to_string())?;
        if !ctx.is_active() {
            return Err("tools@1 不在运行（未激活或正在卸载）".to_string());
        }
        Ok(ctx)
    }
}

#[service_impl]
impl Tools for ToolsImpl {
    async fn register(
        &self,
        owner: u64,
        name: String,
        desc: String,
        iface: Option<String>,
    ) -> Result<(), String> {
        // 解析 iface 必须在**取锁之前**：锁不跨 await，也不跨宿主回调。
        let target = match &iface {
            Some(key) => {
                let ctx = self.live_ctx()?;
                // **宽容读**（`strict = false`）：登记常常发生在**提供者自己的 apply 里**
                // （T2 插件边装载边登记它的 iface），那时提供者还在 LOADING——
                // 严格读要求提供者 ACTIVE，会在这里把刚 provide 好的接口判成"不存在"。
                let adapter = ctx
                    .store
                    .try_get(&ctx, key, false)
                    .and_then(|value| value.adapter())
                    .ok_or_else(|| {
                        format!("工具 \"{name}\" 的 iface {key:?} 还没有提供者（或它不是可调用的接口）：先 provide_iface 再登记")
                    })?;
                Some(IfaceRef(adapter))
            }
            None => None,
        };
        let mut entries = self.entries.lock().unwrap();
        if entries.contains_key(&(owner, name.clone())) {
            return Err(format!(
                "工具 \"{name}\" 在 owner {owner} 下已经登记过：目录不允许重名"
            ));
        }
        entries.insert(
            (owner, name.clone()),
            ToolEntry {
                name,
                desc,
                iface,
                target,
            },
        );
        Ok(())
    }

    async fn unregister(&self, owner: u64, name: String) -> Result<(), String> {
        let removed = self.entries.lock().unwrap().remove(&(owner, name.clone()));
        match removed {
            Some(_) => Ok(()),
            None => Err(format!("目录里没有 (owner {owner}, \"{name}\") 这条登记")),
        }
    }

    async fn names(&self, owners: Vec<u64>) -> Result<Vec<String>, String> {
        let entries = self.entries.lock().unwrap();
        let mut names: Vec<String> = entries
            .keys()
            .filter(|(owner, _)| owners.contains(owner))
            .map(|(_, name)| name.clone())
            .collect();
        names.sort();
        names.dedup();
        Ok(names)
    }

    async fn call(&self, owners: Vec<u64>, name: String, input: Config) -> Result<Config, String> {
        // 取锁期间只做"挑一个候选项"，调用发生在锁外（D10：锁不跨宿主回调）。
        let found: Option<ToolEntry> = {
            let entries = self.entries.lock().unwrap();
            entries
                .iter()
                .filter(|((owner, _), _)| owners.contains(owner))
                .find(|(_, entry)| entry.name == name)
                .map(|(_, entry)| (*entry).clone())
        };
        let Some(entry) = found else {
            let visible = self.names(owners).await?;
            return Err(format!(
                "这个作用域链上找不到工具 \"{name}\"；可见的是 {visible:?}"
            ));
        };
        let Some(target) = entry.target else {
            return Err(format!(
                "工具 \"{name}\"（{}）没有 iface 转发面（登记时 iface = {:?}）：它的实现是 `tools/invoke` 上的钩子，请走事件派发",
                entry.desc, entry.iface
            ));
        };
        target
            .0
            .call(None, "call".to_string(), input)
            .await
            .map_err(|err| err.to_string())
    }
}

/// 提供 `tools@1`（typed + JSON 两面）。
pub fn tools_plugin() -> Arc<dyn Plugin> {
    with_provision(
        plugin_fn("tools", Inject::default(), |ctx: Ctx, _config: Config| {
            Box::pin(async move {
                let table = Arc::new(ToolsImpl {
                    ctx: ctx.downgrade(),
                    entries: Mutex::new(BTreeMap::new()),
                });
                ToolsService::provide(&ctx, table)?;
                Ok(())
            })
        }),
        vec![ToolsService::KEY.to_string()],
    )
}

// ============================================================ sessions（内存版）

#[derive(Default)]
struct MemoryState {
    log: Vec<Config>,
    /// 已经分配出去的最大 `seq`（`seq` 从 1 起）。
    next: u64,
}

/// 测试层的 `sessions@1`：内存实现，**与 DLL 的线上形状逐字同形**。
#[derive(Default)]
pub struct MemorySessions {
    state: Mutex<MemoryState>,
}

impl MemorySessions {
    /// 日志快照（按写入顺序）。
    pub fn records(&self) -> Vec<Config> {
        self.state.lock().unwrap().log.clone()
    }
}

#[service_impl]
impl Sessions for MemorySessions {
    async fn append(&self, record: Config) -> Result<u64, String> {
        let mut state = self.state.lock().unwrap();
        let seq = state.next + 1;
        state.next = seq;
        let mut record = record;
        if let Value::Object(map) = &mut record {
            map.insert("seq".to_string(), json!(seq));
        }
        state.log.push(record);
        Ok(seq)
    }

    async fn tail(&self, count: u32) -> Result<Vec<Config>, String> {
        let state = self.state.lock().unwrap();
        let take = (count as usize).min(state.log.len());
        Ok(state.log[state.log.len() - take..].to_vec())
    }

    async fn next_seq(&self) -> Result<u64, String> {
        Ok(self.state.lock().unwrap().next + 1)
    }
}

/// 挂一个内存版 `sessions@1`；返回插件与它的句柄（测试要读日志）。
pub fn memory_sessions_plugin() -> (Arc<dyn Plugin>, Arc<MemorySessions>) {
    let sessions = Arc::new(MemorySessions::default());
    let for_plugin = sessions.clone();
    let plugin = with_provision(
        plugin_fn(
            "memory-sessions",
            Inject::default(),
            move |ctx: Ctx, _config: Config| {
                let sessions = for_plugin.clone();
                Box::pin(async move {
                    SessionsService::provide(&ctx, sessions)?;
                    Ok(())
                })
            },
        ),
        vec![SessionsService::KEY.to_string()],
    );
    (plugin, sessions)
}

// ============================================================ llm（两个适配器 + 槽位）

/// 脚本适配器：计划写死（二进制层用它驱动一场确定的会话）。
#[derive(Clone)]
pub struct ScriptedLlm {
    name: String,
    plans: Vec<Plan>,
}

#[service_impl]
impl Llm for ScriptedLlm {
    async fn adapter(&self) -> Result<String, String> {
        Ok(self.name.clone())
    }

    async fn plan(&self, _prompt: String) -> Result<Vec<Plan>, String> {
        Ok(self.plans.clone())
    }
}

/// replay 适配器：读**录制的 chunk 序列**，逐条翻成计划步。
///
/// 同一份录制跑两次必须逐字段相同——这就是"确定性测试"的全部要求
/// （D7：流式提供者超出 ABI v2 的表达力，spike 里只换 T1 适配器，不假装能流）。
#[derive(Clone)]
pub struct ReplayLlm {
    name: String,
    chunks: Vec<Value>,
}

impl ReplayLlm {
    pub fn new(name: &str, chunks: Vec<Value>) -> Self {
        Self {
            name: name.to_string(),
            chunks,
        }
    }
}

#[service_impl]
impl Llm for ReplayLlm {
    async fn adapter(&self) -> Result<String, String> {
        Ok(self.name.clone())
    }

    async fn plan(&self, _prompt: String) -> Result<Vec<Plan>, String> {
        let mut plans = Vec::new();
        for (index, chunk) in self.chunks.iter().enumerate() {
            let plan: Plan = serde_json::from_value(chunk.clone()).map_err(|err| {
                format!(
                    "录制的 chunk #{index} 解不出计划步（要 {{\"tool\": …, \"input\": …}}）: {err}"
                )
            })?;
            plans.push(plan);
        }
        Ok(plans)
    }
}

/// 适配器 → **registry 槽位值**：typed 面（`Iface<dyn Llm>`）与 JSON 面（adapter）
/// 指同一份实现（M6 的两张面 + M7 的槽位）。
pub fn llm_slot<T: Llm + 'static>(adapter: Arc<T>) -> Dyn {
    let typed: Arc<dyn Llm> = adapter.clone();
    let json_face = LlmService::adapter(typed.clone());
    Dyn::new(Iface::<dyn Llm>(typed)).with_adapter(json_face)
}

/// 脚本适配器的槽位值。
pub fn scripted_llm(name: &str, plans: Vec<Plan>) -> Dyn {
    llm_slot(Arc::new(ScriptedLlm {
        name: name.to_string(),
        plans,
    }))
}

/// replay 适配器的槽位值（`chunks` 是录制下来的转录）。
pub fn replay_llm(name: &str, chunks: Vec<Value>) -> Dyn {
    llm_slot(Arc::new(ReplayLlm::new(name, chunks)))
}

// ============================================================ 工具（作用域内的钩子）

/// 一个**作用域内**的工具：目录里登记名字，`tools/invoke` 上挂实现。
///
/// 关键在"挂在哪"：调用方从**带作用域的 ctx** `plugin(...)` 它，fiber 的 ctx 就继承
/// 那个作用域，于是 `ctx.on(...)` 注册出来的钩子带着作用域身份——判定留给派发期。
pub fn tool_plugin(agent: &str, tool: &str) -> Arc<dyn Plugin> {
    let owner_label = agent.to_string();
    let tool = tool.to_string();
    plugin_fn(
        "tool",
        Inject::of(&[ToolsService::KEY]),
        move |ctx: Ctx, _config: Config| {
            let owner_label = owner_label.clone();
            let tool = tool.clone();
            Box::pin(async move {
                let owner = ctx.scope().map(|anchor| anchor.uid()).unwrap_or(0);
                let tools = ToolsService::of(&ctx).map_err(|err| err.to_string())?;
                tools
                    .register(
                        owner,
                        tool.clone(),
                        format!("{owner_label} 的 {tool}（tools/invoke 上的钩子）"),
                        None,
                    )
                    .await?;

                let mine = tool.clone();
                HarnessEvents::on_run_tool(&ctx, move |payload: Invoke| {
                    let mine = mine.clone();
                    let owner_label = owner_label.clone();
                    Box::pin(async move {
                        if payload.tool != mine {
                            return Ok(None); // 不是我的工具 ⇒ 不表态（bail 继续往后走）
                        }
                        let input = payload.input.clone();
                        let mut done = payload;
                        done.output = json!({ "tool": done.tool.clone(), "owner": owner_label, "input": input });
                        Ok(Some(done))
                    })
                });
                Ok(())
            })
        },
    )
}

/// 从 `agent_ctx` 挂一个工具（作用域由挂载点继承）。
pub async fn mount_tool(agent_ctx: &Ctx, agent: &str, tool: &str) -> Arc<Fiber> {
    let fiber = agent_ctx.plugin(tool_plugin(agent, tool), Config::Null);
    fiber.settle().await;
    fiber
}

/// 直接派发一次 `tools/invoke`（不改写、不记账）——断言"某个作用域看得见什么"的最小动作。
pub async fn call_tool(ctx: &Ctx, tool: &str) -> Option<Invoke> {
    call_tool_with(ctx, tool, json!({})).await
}

/// 同上，带自定义入参。
pub async fn call_tool_with(ctx: &Ctx, tool: &str, input: Value) -> Option<Invoke> {
    let ret = HarnessEvents::run_tool(ctx, Invoke::plan("caller", tool, input)).await;
    decode_ret::<Invoke>(ret).ok().flatten()
}

// ============================================================ agent 循环

/// `#[cordis::config]`：字段级体检优先（serde 的默认报错不带字段名）。
#[config]
#[derive(Clone, Debug, PartialEq)]
pub struct AgentConfig {
    pub agent: String,
}

/// agent 循环：`#[cordis::plugin]` 声明 inject / provision / config，
/// `apply` 是普通 `async fn`（宏在调用前克隆一份 —— 状态放 `Arc`/`Weak`）。
#[plugin(
    name = "agent-loop",
    inject("llm@1", "sessions@1", "tools@1", "registry.local"),
    provision("agent@1"),
    config(AgentConfig)
)]
#[derive(Clone)]
pub struct AgentLoop;

impl AgentLoop {
    async fn apply(&self, ctx: Ctx, config: Config) -> cordis::core::Result<()> {
        let cfg = AgentConfig::parse(config)?;
        let driver = Arc::new(AgentDriver {
            agent: cfg.agent,
            ctx: ctx.downgrade(), // Weak 纪律（M10）：服务值活得可能比承载它的 fiber 久
        });
        AgentService::provide(&ctx, driver)?;
        Ok(())
    }
}

struct AgentDriver {
    agent: String,
    ctx: Weak<Context>,
}

impl AgentDriver {
    fn live(&self) -> Result<Ctx, String> {
        self.ctx
            .upgrade()
            .ok_or_else(|| format!("agent \"{}\" 的 agent-loop 已卸载", self.agent))
    }

    fn llm(&self) -> Result<Arc<dyn Llm>, String> {
        let ctx = self.live()?;
        LlmService::of(&ctx).map_err(|err| err.to_string())
    }

    /// 一条 `sessions` 记录：走 **JSON 面**（T2 的提供者是动态库，只有这条路）。
    /// 返回值是分配的 `seq`；**返回即已落盘**（flush 屏障在提供者那一侧）。
    async fn append(&self, ctx: &Ctx, record: Value) -> Result<u64, String> {
        let out = ctx
            .call_iface(SessionsService::KEY, 1, "append", json!([record]))
            .await
            .map_err(|err| format!("sessions.append 失败: {err}"))?;
        out.as_u64()
            .ok_or_else(|| format!("sessions.append 的应答不是 seq：{out}"))
    }

    /// 目录回退（D2）：`tools/invoke` 上没人应答的工具，问 T1 的目录要不要把它
    /// 转发给 T2 的 iface。目录找不到、或那条登记没有 iface 面 ⇒ 可读错误。
    async fn call_catalog(&self, ctx: &Ctx, tool: &str, input: Value) -> Result<Value, String> {
        let tools = ToolsService::of(ctx).map_err(|err| err.to_string())?;
        tools.call(owner_chain(ctx), tool.to_string(), input).await
    }
}

#[service_impl]
impl Agent for AgentDriver {
    async fn adapter(&self) -> Result<String, String> {
        self.llm()?.adapter().await
    }

    async fn tools(&self) -> Result<Vec<String>, String> {
        let ctx = self.live()?;
        let tools = ToolsService::of(&ctx).map_err(|err| err.to_string())?;
        tools.names(owner_chain(&ctx)).await
    }

    async fn run(&self, prompt: String) -> Result<Report, String> {
        let ctx = self.live()?;
        let llm = LlmService::of(&ctx).map_err(|err| err.to_string())?;
        let adapter = llm.adapter().await?;
        let plans = llm.plan(prompt.clone()).await?;

        let mut seqs: Vec<u64> = Vec::new();
        let mut tools: Vec<String> = Vec::new();
        let mut unanswered: Vec<String> = Vec::new();
        let mut stages: Vec<String> = Vec::new();

        seqs.push(
            self.append(
                &ctx,
                json!({ "kind": "session-start", "agent": self.agent, "adapter": adapter, "prompt": prompt }),
            )
            .await?,
        );

        for (index, plan) in plans.iter().enumerate() {
            let round = index as u32 + 1;
            // ① 每轮开始：观察面（emit 的返回值被丢弃）。
            HarnessEvents::pre_step(
                &ctx,
                Step {
                    agent: self.agent.clone(),
                    round,
                },
            )
            .await;

            // ② 拦截链（waterfall）。链尾把载荷**原样**交回 ⇒ "没人拦截"也拿得到载荷；
            //    拦截器要么改写它，要么不调 `next`（等于否决这一步）。
            let mut invoke = Invoke::plan(&self.agent, &plan.tool, plan.input.clone());
            let tail_value = invoke.json();
            let tail: Option<BoxFuture<Ret>> =
                Some(Box::pin(
                    async move { Some(Dyn::new(ExternValue(tail_value))) },
                ));
            let ret = HarnessEvents::before_invoke(&ctx, invoke.clone(), tail).await;
            match decode_ret::<Invoke>(ret).map_err(|err| err.to_string())? {
                Some(rewritten) => invoke = rewritten,
                None => {
                    seqs.push(
                        self.append(
                            &ctx,
                            json!({ "kind": "blocked", "agent": self.agent, "round": round, "tool": invoke.tool }),
                        )
                        .await?,
                    );
                    continue;
                }
            }
            collect(&mut stages, &invoke.stages);

            // ③ 执行（bail）：同步的监听器与挂起的监听器都算"表态"。
            let ret = HarnessEvents::run_tool(&ctx, invoke.clone()).await;
            match decode_ret::<Invoke>(ret).map_err(|err| err.to_string())? {
                Some(done) => {
                    collect(&mut stages, &done.stages);
                    tools.push(invoke.tool.clone());
                    let seq = self
                        .append(
                            &ctx,
                            json!({
                                "kind": "tool",
                                "agent": self.agent,
                                "round": round,
                                "tool": invoke.tool,
                                "input": invoke.input,
                                "output": done.output,
                                "stages": done.stages,
                            }),
                        )
                        .await?;
                    seqs.push(seq);
                }
                None => {
                    // 事件上没人应答 ⇒ 问**目录**：带 iface 的工具（T2 提供者）从这条路走
                    // （D2：T1 侧持引用、把 T2 的注册转发进来的那个面）。
                    let input = invoke.input.clone();
                    match self.call_catalog(&ctx, &invoke.tool, input).await {
                        Ok(output) => {
                            tools.push(invoke.tool.clone());
                            let seq = self
                                .append(
                                    &ctx,
                                    json!({
                                        "kind": "tool",
                                        "agent": self.agent,
                                        "round": round,
                                        "tool": invoke.tool,
                                        "input": invoke.input,
                                        "output": output,
                                        "stages": invoke.stages,
                                        "via": "catalog",
                                    }),
                                )
                                .await?;
                            seqs.push(seq);
                        }
                        Err(err) => {
                            unanswered.push(invoke.tool.clone());
                            let seq = self
                                .append(
                                    &ctx,
                                    json!({ "kind": "tool", "agent": self.agent, "round": round, "tool": invoke.tool, "unanswered": true, "catalog": err }),
                                )
                                .await?;
                            seqs.push(seq);
                        }
                    }
                }
            }
        }

        seqs.push(
            self.append(
                &ctx,
                json!({ "kind": "session-end", "agent": self.agent, "rounds": plans.len() }),
            )
            .await?,
        );

        Ok(Report {
            agent: self.agent.clone(),
            adapter,
            rounds: plans.len() as u32,
            tools,
            unanswered,
            seqs,
            stages,
        })
    }
}

/// 把新标记追加进 `stages`（保序、去重）。
fn collect(stages: &mut Vec<String>, fresh: &[String]) {
    for stage in fresh {
        if !stages.contains(stage) {
            stages.push(stage.clone());
        }
    }
}

// ============================================================ 装配

/// spike 的前半截：`registry`（M7 的槽位）+ `tools@1` + `llm` 槽位。
pub struct Core {
    pub app: App,
    pub registry: Arc<RegistryHandle>,
}

/// 一个 agent 的句柄：作用域 ctx + 它的 agent-loop fiber。
pub struct AgentHandle {
    pub name: String,
    pub ctx: Ctx,
    pub fiber: Arc<Fiber>,
}

/// 用**调用方给的** `App` 装配前半截（二进制层要自己装日志出口）。
pub async fn boot_core_in(app: App, llm: Dyn) -> Result<Core, Error> {
    app.plugin(registry::plugin(), Config::Null).await?;
    let registry = registry::handle(&app.root)
        .ok_or_else(|| -> Error { "registry.local 没挂上".into() })?;
    app.plugin(tools_plugin(), Config::Null).await?;
    registry.put(LlmService::KEY, llm)?;
    Ok(Core { app, registry })
}

/// 装配前半截（自带 `App`）。
pub async fn boot_core(llm: Dyn) -> Result<Core, Error> {
    boot_core_in(App::new(), llm).await
}

/// 在一个（可带作用域的）父 ctx 下挂一个 agent：**作用域由调用点决定**。
///
/// 父 ctx 已经带作用域 ⇒ 新的这一支自动**挂在它下面**（`bind_scope_parent`）：
/// 于是"子代理看得见父的工具"是链上的继承，而不是把父的工具拷一份。
pub async fn mount_agent(parent: &Ctx, name: &str) -> Result<AgentHandle, Error> {
    let scoped = agent_scope(parent);
    if parent.scope().is_some() {
        scoped
            .bind_scope_parent(parent)
            .map_err(|err| -> Error { err.into() })?;
    }
    // 每个 agent 一个**私有域**：`agent@1` 这个键在域内解析。
    // 内核的不变量是"一个键在实现域内只有一个实现"——多个 agent 各挂一个 agent-loop，
    // 分开它们的正是"域"这一层（M1 的派生隔离域），而不是名字上的前缀小技巧。
    let ctx = scoped.isolate(AgentService::KEY, None);
    let fiber = ctx.plugin(Arc::new(AgentLoop), json!({ "agent": name }));
    fiber.settle().await;
    if fiber.state() != FiberState::Active {
        return Err(format!(
            "agent-loop \"{name}\" 没有激活（{:?}）：llm@1 / sessions@1 / tools@1 / registry.local 都齐了吗？",
            fiber.state()
        )
        .into());
    }
    Ok(AgentHandle {
        name: name.to_string(),
        ctx,
        fiber,
    })
}

/// 在一个父 ctx 下挂一批 agent（每个一个作用域）：二进制层传 `&core.app.root`，
/// 想挂子代理就传一个**带作用域**的 ctx。
pub async fn boot_agents(
    parent: &Ctx,
    agents: &[&str],
) -> Result<BTreeMap<String, AgentHandle>, Error> {
    let mut handles = BTreeMap::new();
    for name in agents {
        handles.insert(name.to_string(), mount_agent(parent, name).await?);
    }
    Ok(handles)
}

/// 测试层的一站式装配：前半截 + 内存 `sessions@1` + agent 们。
pub struct Harness {
    pub app: App,
    pub registry: Arc<RegistryHandle>,
    pub sessions: Arc<MemorySessions>,
    pub agents: BTreeMap<String, AgentHandle>,
}

impl Harness {
    pub fn agent(&self, name: &str) -> &AgentHandle {
        self.agents
            .get(name)
            .unwrap_or_else(|| panic!("没有名为 \"{name}\" 的 agent"))
    }

    pub fn agent_ctx(&self, name: &str) -> Ctx {
        self.agent(name).ctx.clone()
    }

    pub fn session_records(&self) -> Vec<Config> {
        self.sessions.records()
    }

    /// 首次放进 `llm@1` 槽位。
    pub fn put_llm(&self, adapter: Dyn) -> Result<(), Error> {
        self.registry.put(LlmService::KEY, adapter).map(|_| ())
    }

    /// **原地换**适配器：一次同步写，槽位身份不变 ⇒ 依赖者 epoch 不变（A7）。
    pub fn swap_llm(&self, adapter: Dyn) -> Result<(), Error> {
        self.registry.replace(LlmService::KEY, adapter)
    }
}

pub async fn boot(agents: &[&str], llm: Dyn) -> Result<Harness, Error> {
    let Core { app, registry } = boot_core(llm).await?;
    let (sessions_plugin, sessions) = memory_sessions_plugin();
    app.plugin(sessions_plugin, Config::Null).await?;
    let agents = boot_agents(&app.root, agents).await?;
    Ok(Harness {
        app,
        registry,
        sessions,
        agents,
    })
}
