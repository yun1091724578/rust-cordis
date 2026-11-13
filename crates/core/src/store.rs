//! 服务注册表 + 插件注册表 + 事件表 + 隔离域表。
//!
//! # 为什么是"一张表"
//! JS 里 `ReflectService / RegistryService / EventsService / LoggerService` 四个对象
//! 各只有一份，而且**都挂在 root context 上**——子 context 只通过原型链继承到同一个实例。
//! 换句话说它们本来就是"全局单表"。Rust 里直接合成一个 `Store`，由所有 `Context`
//! 共享 `Arc<Store>`：语义完全一致，少了一层间接。
//!
//! # 服务的两种身份
//! - `Impl`：**一个已注册的实现**，键是「隔离域」（不是名字）——
//!   所以同一名字在不同域里可以是不同实现，同域内天然互斥。
//! - `realms`：默认域表，等价 JS 的 `ctx.root[isolate][name] ??= Symbol(name)`。
//!
//! JS 那边还有一张 `props` 表（名字 → 属性种类：service / accessor / mixin），
//! 本实现**刻意没有它**：accessor 与 mixin 已被整体删除（见方案.md 偏离清单 #5），
//! 于是"名字已被声明为服务"这个标记不再携带任何信息——
//! `provide` 的冲突检查直接看 `impls`（与 JS 的 `reflect.ts:189-192` 一致），
//! `set` 的"没 provide 过就不能写"也直接看 `impls`（与 `reflect.ts:164-169` 一致）。

use crate::dyn_value::{BoxFuture, Config, Disposable, Dyn, Result};
use crate::event::{self, Args, Filter, Handler, Hook, Mode, Ret};
use crate::fiber::Fiber;
use crate::{Context, Ctx};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

pub type RealmId = u32;

/// 服务的就绪谓词：返回 `false` 视为"服务未就绪"，依赖方保持 PENDING。
/// 对应 JS 的 `Service[check]`（`loader` 用它表达"还有 pending 任务"）。
pub type Check = Arc<dyn Fn(&Dyn) -> bool + Send + Sync>;

/// **同步监听器钩子**：`ctx.on` 每次注册前先咨询它们（`internal/listener` 的 Rust 形态，
/// M3-1）。实参 = `(注册者 ctx, 事件名, handler, prepend, global)`；
/// 返回 `Some(disposable)` ⇒ 接管这次注册。
pub type ListenerHook =
    Arc<dyn Fn(&Ctx, &str, &Handler, bool, bool) -> Option<Disposable> + Send + Sync>;

/// **同步读钩子**（`internal/get` 的 Rust 形态，M3-3）：实参 `(读的 ctx, 服务名)`；
/// 返回 `Some(值)` ⇒ 接管这次读取（不再查表）；`None` ⇒ 落回正常解析。
///
/// 上游是 Proxy 陷阱里的 waterfall（`reflect.ts:82`）；Rust 没有 Proxy，
/// 映射到注入检查的读（`Ctx::require` → [`Store::resolve`]）。
pub type GetHook = Arc<dyn Fn(&Ctx, &str) -> Option<Dyn> + Send + Sync>;

/// **同步写钩子**（`internal/set` 的 Rust 形态，M3-3）：实参 `(写的 ctx, 服务名, 新值)`；
/// 返回 `Some(result)` ⇒ 接管这次写入（`Some(Err(..))` 即否决）；`None` ⇒ 走内核的 `set`。
pub type SetHook = Arc<dyn Fn(&Ctx, &str, &Dyn) -> Option<Result<()>> + Send + Sync>;

pub struct Impl {
    pub name: Box<str>,
    /// 内部可变：`ctx.set()` 允许提供者原地换值，而所有依赖者共享同一个 `Impl`，
    /// 所以不能靠替换 `Arc` 来实现（那样依赖者会拿到旧值）。
    pub value: Mutex<Dyn>,
    pub fiber: Weak<Fiber>,
    pub check: Option<Check>,
    /// 注册时实际落入的隔离域。`notify` 必须按它比较，不能再用提供者的
    /// **base ctx** 重新算——在派生 `isolate` ctx 上 `provide` 时两者会不同。
    pub realm: RealmId,
}

impl Impl {
    pub fn new(
        name: Box<str>,
        value: Dyn,
        fiber: Weak<Fiber>,
        check: Option<Check>,
        realm: RealmId,
    ) -> Self {
        Self {
            name,
            value: Mutex::new(value),
            fiber,
            check,
            realm,
        }
    }

    pub fn get(&self) -> Dyn {
        self.value.lock().unwrap().clone()
    }

    /// 提供者 fiber 的 uid，参与依赖方的 epoch 计算。
    /// 提供者一旦重启（uid 变化），所有依赖方的 epoch 变化 → 自动 reload。
    pub fn provider_uid(&self) -> u64 {
        self.fiber.upgrade().map(|f| f.uid()).unwrap_or(u64::MAX)
    }
}

/// 插件依赖声明：`name -> intercept 配置`（`Config::Null` 表示无配置）。
#[derive(Clone, Debug, Default)]
pub struct Inject(pub Vec<(Box<str>, Config)>);

impl Inject {
    pub fn of(names: &[&str]) -> Self {
        Self(names.iter().map(|n| ((*n).into(), Config::Null)).collect())
    }
    pub fn contains(&self, name: &str) -> bool {
        self.0.iter().any(|(n, _)| &**n == name)
    }
    pub fn with(mut self, name: &str, config: Config) -> Self {
        if let Some(slot) = self.0.iter_mut().find(|(n, _)| &**n == name) {
            slot.1 = config;
        } else {
            self.0.push((name.into(), config));
        }
        self
    }
}

/// 插件的注册表身份键。
///
/// 必须区分**显示名**和**身份**：`name` 用于日志与 `fiber.name()`（同名很正常，
/// 比如两行配置都用 `@x/provider`）；`key` 决定"是否共享同一条 Runtime 记录"。
/// JS 用 callback **函数对象**本身当 Map 键，天然带身份；Rust 里必须显式给出来。
/// 绑错这一层会出现最难查的 bug：两个不同的插件塌缩成一条 Runtime，
/// 后挂的实例跑的是先挂实例的 `apply`。
pub type PluginKey = Arc<str>;

/// 生成一个进程内唯一的插件身份键。
///
/// **同一个 `Arc<dyn Plugin>` 的 clone 共享 key**（所以"同一插件挂多个实例"仍然
/// 共享一条 Runtime 记录）；两个独立构造的同名插件各得一把新 key。
pub fn unique_key(name: &str) -> PluginKey {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    let sn = COUNTER.fetch_add(1, Ordering::SeqCst);
    Arc::from(format!("{name}#{sn}").as_str())
}

/// 插件。
///
/// 对应 JS 的 `Plugin.Function | Plugin.Constructor | Plugin.Object` 三形态——
/// Rust 里没有"函数即插件对象"的原型魔法，三形态统一收敛为一个 trait，
/// 用 `FnPlugin` 适配器表达。
pub trait Plugin: Send + Sync + 'static {
    /// 诊断用名字（日志、`fiber.name()`）。
    fn name(&self) -> &str;

    /// 注册表身份键。默认实现是"按名字共享"，手写插件通常直接给一个稳定常量
    /// （包名），这样同一包挂多行配置共享一条 Runtime 记录。
    fn key(&self) -> PluginKey {
        Arc::from(self.name())
    }

    /// 可选的插件版本（诊断/身份用，不是 ABI 授权）。
    ///
    /// 默认 `None` = 未声明；loader 与 CLI 只能把它当展示信息，不能据此替代
    /// `abi_version` 的精确匹配或来源策略。
    fn version(&self) -> Option<&str> {
        None
    }

    fn inject(&self) -> Inject {
        Inject::default()
    }

    /// **provision 声明**（M5）：这个插件声明自己能安装哪些 key。
    ///
    /// 命名空间与 [`Store::impls_owned_by`] **同一口径**（今天都是裸名；
    /// M6 启用 `<name>@<major>` 时两边一起换，绝不允许只换一边）。
    ///
    /// **默认空 = 不透明**（跳过一切 provision 检查）：既有插件都没有声明，
    /// 把空读成"声明了空集"会让 Def 48（装了未声明的 key）对每个插件报警——
    /// 规则就写在 [`Store::provision_report`] 的构造器旁边，改这里就毁了它。
    /// ABI 插件的声明来自 `PluginDesc.provision`（v1 插件读作"未声明"）。
    fn provision(&self) -> Vec<String> {
        Vec::new()
    }

    /// **配置 schema 声明**（阶段 3）：插件自述的 JSON Schema，供装载前审阅。
    ///
    /// 默认 `None` = 未声明。它只是**展示用元数据**：内核不据此验证配置，
    /// 也不把“声明了 schema”当成批准或授权；真正的配置校验仍走 [`Plugin::validate`]
    /// 或插件自己的 apply 逻辑。
    fn config_schema(&self) -> Option<String> {
        None
    }

    /// 同步的配置校验（JS 的 Standard Schema，且明确拒绝异步校验）。
    fn validate(&self, raw: Config) -> std::result::Result<Config, ValidationError> {
        Ok(raw)
    }

    /// **volatile 声明**（M4-5）：这些路径上的值由内核持有**稳定引用**，
    /// 只差它们的配置变更不会重挂插件。
    ///
    /// 对应上游 schema 里的 `.volatile()` 元数据（`schemastery/src/index.ts:480-483`）：
    /// 路径是**点号对象路径**（`"data"`、`"cache.dir"`），不允许数组下标与动态键
    /// （`validateVolatileSchema` 的理由：那样的路径不固定，"哪些引用要更新"就没有唯一答案）。
    ///
    /// 默认空 ⇒ 这个插件不参与 volatile：配置一变就重挂（安全的那一侧）。
    fn volatile_paths(&self) -> Vec<String> {
        Vec::new()
    }

    /// 把新的 volatile 值推给**正在运行**的实例（M4-5）。
    ///
    /// 原生插件的默认实现是空操作：引用在插件手里，内核直接写引用就够了
    /// （[`VolatileRef::commit`](crate::VolatileRef::commit)）。
    /// **ABI 插件必须覆盖它**：跨边界没有共享引用，只能把新值推过去
    /// （`OP_VOLATILE_APPLY`），由插件更新自己那份副本。
    ///
    /// 返回 `Err` ⇒ 这次提交作废：引用不写、事件不发，装载器把它记成
    /// "实例落后于配置"（与"候选无效"同一处置）。
    fn apply_volatile(&self, ctx: &Ctx, values: &[(String, Config)]) -> Result<()> {
        let _ = (ctx, values);
        Ok(())
    }

    fn apply(&self, ctx: Ctx, config: Config) -> BoxFuture<Result<()>>;
}

/// 给任意插件**加上 provision 声明**（M5）。
///
/// 原生插件没有 ABI 的 `PluginDesc` 字段，声明走这个包装器：
/// ```ignore
/// let plugin = with_provision(plugin_fn("tools", Inject::default(), apply), vec!["tools".into()]);
/// ```
/// ABI 插件不需要它——声明在 desc 里，宿主读进 [`Plugin::provision`]。
pub fn with_provision(plugin: Arc<dyn Plugin>, provision: Vec<String>) -> Arc<dyn Plugin> {
    Arc::new(Provisioned {
        inner: plugin,
        provision,
    })
}

struct Provisioned {
    inner: Arc<dyn Plugin>,
    provision: Vec<String>,
}

impl Plugin for Provisioned {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn version(&self) -> Option<&str> {
        self.inner.version()
    }

    fn key(&self) -> PluginKey {
        self.inner.key()
    }

    fn inject(&self) -> Inject {
        self.inner.inject()
    }

    fn provision(&self) -> Vec<String> {
        self.provision.clone()
    }

    fn validate(&self, raw: Config) -> std::result::Result<Config, ValidationError> {
        self.inner.validate(raw)
    }

    fn volatile_paths(&self) -> Vec<String> {
        self.inner.volatile_paths()
    }

    fn apply_volatile(&self, ctx: &Ctx, values: &[(String, Config)]) -> Result<()> {
        self.inner.apply_volatile(ctx, values)
    }

    fn apply(&self, ctx: Ctx, config: Config) -> BoxFuture<Result<()>> {
        self.inner.apply(ctx, config)
    }
}

/// 一个 fiber 的 provision 账（M5）：声明 / 实际安装 / 两边对不上的地方。
///
/// **规则 1（紧挨报告构造器）**：`declared` 为空 = **不透明** = 跳过检查。
/// 既有的原生插件都不声明 provision；若把空读成"声明了空集"，
/// Def 48（装了未声明的 key）会对每一个插件报警，demo 的断言会全红。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProvisionReport {
    /// 插件显示名。
    pub plugin: String,
    /// fiber 身份（同一插件挂多行时用来区分）。
    pub uid: u64,
    /// 声明原文序（作者写的顺序有意义）。
    pub declared: Vec<String>,
    /// 实际安装的 key（排序，`impls_owned_by` 来自哈希表）。
    pub installed: Vec<String>,
    /// 可读问题（Def 48 / Def 76）；对得上就是空。
    pub issues: Vec<String>,
}

impl ProvisionReport {
    /// 声明为空 ⇒ 不透明：一切检查都跳过（规则 1 的唯一落点）。
    pub fn is_opaque(&self) -> bool {
        self.declared.is_empty()
    }
}

/// 消息格式与 JS 的 `ValidationError` 对齐：
/// `invalid config:\n  - <msg> (at <path>)`
#[derive(Debug, Clone)]
pub struct ValidationError {
    pub issues: Vec<String>,
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid config:")?;
        for issue in &self.issues {
            write!(f, "\n  - {issue}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ValidationError {}

/// 同步配置校验器（cordis 明确拒绝异步 schema）。
pub type Validator =
    Arc<dyn Fn(Config) -> std::result::Result<Config, ValidationError> + Send + Sync>;
type PluginObserver = Arc<dyn Fn(&Arc<Fiber>) + Send + Sync>;
type CommitObserver = Arc<dyn Fn(&Arc<Fiber>, &Config, bool) + Send + Sync>;
type NamedRealms = HashMap<(Box<str>, Box<str>), RealmId>;

/// 函数式插件：`fn(ctx, config) -> BoxFuture<Result<()>>`
pub struct FnPlugin {
    pub name: &'static str,
    pub key: PluginKey,
    pub inject: Inject,
    pub validator: Option<Validator>,
    pub f: Arc<dyn Fn(Ctx, Config) -> BoxFuture<Result<()>> + Send + Sync>,
}

impl Plugin for FnPlugin {
    fn name(&self) -> &str {
        self.name
    }
    fn key(&self) -> PluginKey {
        self.key.clone()
    }
    fn inject(&self) -> Inject {
        self.inject.clone()
    }
    fn validate(&self, raw: Config) -> std::result::Result<Config, ValidationError> {
        match &self.validator {
            Some(v) => v(raw),
            None => Ok(raw),
        }
    }
    fn apply(&self, ctx: Ctx, config: Config) -> BoxFuture<Result<()>> {
        (self.f)(ctx, config)
    }
}

/// 一条插件的注册记录：同一 key 的所有实例共享它。
pub struct Runtime {
    pub key: PluginKey,
    pub name: String,
    pub plugin: Arc<dyn Plugin>,
    pub fibers: Mutex<Vec<Arc<Fiber>>>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Level {
    Debug,
    Info,
    Warn,
    Error,
}

pub type LogSink = Arc<dyn Fn(Level, &str, &str) + Send + Sync>;

pub struct Store {
    impls: Mutex<HashMap<RealmId, Arc<Impl>>>,
    realms: Mutex<HashMap<Box<str>, RealmId>>,
    runtimes: Mutex<Vec<Arc<Runtime>>>,
    /// 依赖图变更的全局串行化：插入依赖前先做可达性检查，避免并发插入
    /// 各自看不到对方而形成环。
    pub(crate) dependency_lock: Mutex<()>,
    hooks: Mutex<HashMap<Box<str>, Vec<Arc<Hook>>>>,
    /// **根 fiber 的弱引用**：`ctx → fiber` 也持 `Weak`，根 fiber 的强引用放在
    /// [`crate::App`] 上。这里若持 `Arc` 就会形成 `Store → root_fiber → Store`
    /// 强引用环——空 App 丢弃后整棵对象图仍被吊住。
    root_fiber: Mutex<Option<Weak<Fiber>>>,
    /// `internal/plugin` / `internal/commit` 的观察者。
    ///
    /// # 为什么这两个不走事件总线
    /// `internal/plugin` 必须是**同步**的：loader 要在它里面按配置行追加 `inject`，
    /// 而依赖解析紧接着就要发生（`vendor/loader/src/index.ts:129-135`）。
    /// 把这条链路做成 async 会引入一个"依赖还没追加就算完依赖状态"的竞态窗口。
    /// `internal/commit` 同理——它是"配置被改了就落盘"的同步通知。
    plugin_observers: Mutex<Vec<(u64, PluginObserver)>>,
    commit_observers: Mutex<Vec<(u64, CommitObserver)>>,
    /// **同步监听器钩子**：`ctx.on` 每次注册前先咨询它们（`internal/listener` 的 Rust 形态）。
    ///
    /// 与事件表不同，这条链是**同步**的：`ctx.on` 本身是同步函数，返回 `Disposable`；
    /// 把它做成 async 事件会让"注册"这个动作无法在同步帧里完成（见方案 §7 M3-1 的 v4 修正）。
    listener_hooks: Mutex<Vec<(u64, ListenerHook)>>,
    /// **同步读/写钩子**（M3-3）：`Ctx::require` / `Ctx::set` 的接管点。
    get_hooks: Mutex<Vec<(u64, GetHook)>>,
    set_hooks: Mutex<Vec<(u64, SetHook)>>,
    /// **具名共享域**表（M4-1）：`(服务名, 标签)` → realm id。只增不减。
    named_realms: Mutex<NamedRealms>,
    next_realm: AtomicU32,
    hook_sn: AtomicU64,
    observer_sn: AtomicU64,
    fiber_sn: AtomicU64,
    log_sink: Mutex<LogSink>,
}

impl Default for Store {
    fn default() -> Self {
        Self::new()
    }
}

impl Store {
    pub fn new() -> Self {
        let store = Self {
            impls: Mutex::new(HashMap::new()),
            realms: Mutex::new(HashMap::new()),
            runtimes: Mutex::new(Vec::new()),
            dependency_lock: Mutex::new(()),
            hooks: Mutex::new(HashMap::new()),
            root_fiber: Mutex::new(None),
            plugin_observers: Mutex::new(Vec::new()),
            commit_observers: Mutex::new(Vec::new()),
            listener_hooks: Mutex::new(Vec::new()),
            get_hooks: Mutex::new(Vec::new()),
            set_hooks: Mutex::new(Vec::new()),
            named_realms: Mutex::new(HashMap::new()),
            next_realm: AtomicU32::new(1),
            hook_sn: AtomicU64::new(1),
            observer_sn: AtomicU64::new(1),
            fiber_sn: AtomicU64::new(1),
            log_sink: Mutex::new(Arc::new(|level, name, msg| {
                eprintln!("[{level:?}] {name}: {msg}");
            })),
        };
        // 内置监听器钩子（上游 `events.ts:54-58` 的那个 `internal/listener` 监听器）：
        // 把**非全局**的 `internal/update` 注册路由进 fiber 自己的钩子链——
        // "行内钩子只对本 fiber 生效、随 fiber 卸载自动摘除"就是从这一条来的。
        store.listener_hooks.lock().unwrap().push((
            0,
            Arc::new(
                |ctx: &Ctx, name: &str, handler: &Handler, prepend: bool, global: bool| {
                    if name == crate::event::INTERNAL_UPDATE && !global {
                        return Some(ctx.fiber().push_hook(name, handler.clone(), prepend));
                    }
                    None
                },
            ),
        ));
        store
    }

    pub fn set_log_sink(&self, sink: LogSink) {
        *self.log_sink.lock().unwrap() = sink;
    }

    pub fn log(&self, level: Level, name: &str, message: &str) {
        let sink = self.log_sink.lock().unwrap().clone();
        sink(level, name, message);
    }

    pub fn next_fiber_uid(&self) -> u64 {
        self.fiber_sn.fetch_add(1, Ordering::SeqCst)
    }

    /// 分配一个新的隔离域标识。
    pub fn alloc_realm(&self) -> RealmId {
        self.next_realm.fetch_add(1, Ordering::SeqCst)
    }

    /// **具名共享域**：`(服务名, 标签)` → 稳定的 realm id（M4-1）。
    ///
    /// `isolate: {svc: "<label>"}` 的两行因此共享同一个域，能互相看见对方的 `svc`。
    ///
    /// **不回收**（与默认域一致，P2-14 的 v4 决策）：域是"服务地址"的一部分，
    /// 回收它会让两代插件意外共享或失去隔离。增长用 [`Store::named_realm_count`] 观测，
    /// 装载器把它放进 `loader.report`。
    pub fn named_realm(&self, name: &str, label: &str) -> RealmId {
        let mut realms = self.named_realms.lock().unwrap();
        if let Some(realm) = realms.get(&(name.into(), label.into())) {
            return *realm;
        }
        let realm = self.next_realm.fetch_add(1, Ordering::SeqCst);
        realms.insert((name.into(), label.into()), realm);
        realm
    }

    /// 具名共享域的数量（只增不减）。
    pub fn named_realm_count(&self) -> usize {
        self.named_realms.lock().unwrap().len()
    }

    /// **默认域**表的条数（M4-6）：`name → 默认域 id` 的映射，同样只增不减。
    ///
    /// 与 [`Store::named_realm_count`] 一起构成"域在长吗"的完整观测面
    /// （P2-14 的 v4 决策：两张表都**不回收**，增长本身就是信号）。
    /// 默认域是"没声明 isolate 的服务地址"——每个名字第一次被用到时就落进这张表。
    pub fn default_realm_count(&self) -> usize {
        self.realms.lock().unwrap().len()
    }

    /// 按域取实现。域由 `realm_of` 从名字 + 上下文算出。
    pub fn lookup(&self, realm: RealmId) -> Option<Arc<Impl>> {
        self.impls.lock().unwrap().get(&realm).cloned()
    }

    /// 只读地查一个 key 在当前上下文里的实现；**不分配 realm**。
    ///
    /// 诊断专用：`realm_of` 会为未知名字插入一条默认域映射，不适合只读路径。
    pub(crate) fn lookup_visible(&self, ctx: &Ctx, name: &str) -> Option<Arc<Impl>> {
        let realm = ctx
            .isolate
            .get(&name.into())
            .copied()
            .or_else(|| self.realms.lock().unwrap().get(name).copied())?;
        self.lookup(realm)
    }

    /// 全局注册表里与 `missing` 同 base 的 key（升序去重）。
    ///
    /// 只用于 pending 诊断的“是不是版本写错了”提示；不检查 isolate 可见性。
    pub(crate) fn related_service_keys(&self, missing: &str) -> Vec<String> {
        let base = missing.split('@').next().unwrap_or(missing);
        let mut keys: Vec<String> = self
            .impls
            .lock()
            .unwrap()
            .values()
            .map(|impl_| impl_.name.to_string())
            .filter(|key| key.as_str() == missing || key.split('@').next() == Some(base))
            .collect();
        keys.sort();
        keys.dedup();
        keys
    }

    /// 从全局候选里筛出当前上下文**可见**的 key（保持候选顺序）。
    ///
    /// 调用方已经拿到 [`Store::related_service_keys`] 的结果时用它，避免
    /// 为每个缺失键重复扫描全局注册表。
    pub(crate) fn visible_keys(&self, ctx: &Ctx, candidates: &[String]) -> Vec<String> {
        candidates
            .iter()
            .filter(|key| self.lookup_visible(ctx, key).is_some())
            .cloned()
            .collect()
    }

    /// 同步观察"新 fiber 诞生 / fiber 被处置"。
    pub fn on_plugin<F>(self: &Arc<Self>, ctx: &Ctx, observer: F) -> crate::Disposable
    where
        F: Fn(&Arc<Fiber>) + Send + Sync + 'static,
    {
        let store = self.clone();
        let observer: PluginObserver = Arc::new(observer);
        ctx.fiber().effect(move |_| {
            let sn = store.observer_sn.fetch_add(1, Ordering::SeqCst);
            store.plugin_observers.lock().unwrap().push((sn, observer));
            let store2 = store.clone();
            crate::Disposable::sync(move || {
                store2
                    .plugin_observers
                    .lock()
                    .unwrap()
                    .retain(|(s, _)| *s != sn);
            })
        })
    }

    /// 同步观察"插件配置被提交"（loader 靠它把改动写回配置文件）。
    ///
    /// `no_save` 是 M3-1 起新增的载荷：`internal/commit` **总是**发出（含
    /// `update_with(config, true)`），观察者自己决定"只推进内存记录"还是"也写盘"。
    /// 上游是干脆不发 commit（`fiber.ts:491` 的 `!noSave`）——那是两通道做法，
    /// 这里是单通道 + 载荷（附录 A 记录）。
    pub fn on_commit<F>(self: &Arc<Self>, ctx: &Ctx, observer: F) -> crate::Disposable
    where
        F: Fn(&Arc<Fiber>, &Config, bool) + Send + Sync + 'static,
    {
        let store = self.clone();
        let observer: CommitObserver = Arc::new(observer);
        ctx.fiber().effect(move |_| {
            let sn = store.observer_sn.fetch_add(1, Ordering::SeqCst);
            store.commit_observers.lock().unwrap().push((sn, observer));
            let store2 = store.clone();
            crate::Disposable::sync(move || {
                store2
                    .commit_observers
                    .lock()
                    .unwrap()
                    .retain(|(s, _)| *s != sn);
            })
        })
    }

    pub(crate) fn fire_plugin(&self, fiber: &Arc<Fiber>) {
        let observers: Vec<_> = self
            .plugin_observers
            .lock()
            .unwrap()
            .iter()
            .map(|(_, f)| f.clone())
            .collect();
        for observer in observers {
            observer(fiber);
        }
    }

    pub(crate) fn fire_commit(&self, fiber: &Arc<Fiber>, config: &Config, no_save: bool) {
        let observers: Vec<_> = self
            .commit_observers
            .lock()
            .unwrap()
            .iter()
            .map(|(_, f)| f.clone())
            .collect();
        for observer in observers {
            observer(fiber, config, no_save);
        }
    }

    /// 注册一个由**内核自己**拥有的全局钩子（不挂 fiber、不可摘除）。
    ///
    /// 只有内置监听器用它（`App::new` 里那条把行内钩子拼到链首的 prepend 钩子）。
    /// 普通代码请用 `Ctx::on_with`。
    pub(crate) fn builtin_hook(
        &self,
        ctx: &Ctx,
        name: &str,
        handler: Handler,
        global: bool,
        prepend: bool,
    ) {
        let sn = self.hook_sn.fetch_add(1, Ordering::SeqCst);
        let hook = Arc::new(event::owned_weak(ctx, handler, global, sn));
        let mut hooks = self.hooks.lock().unwrap();
        let list = hooks.entry(name.into()).or_default();
        if prepend {
            list.insert(0, hook);
        } else {
            list.push(hook);
        }
    }

    // ---------------------------------------------------------------- 隔离域

    /// 等价 JS 的 `ctx[symbols.isolate][name]` 沿原型链查表，查不到则落到
    /// `root[isolate][name] ??= Symbol(name)` 建立的全域默认域。
    pub fn realm_of(&self, ctx: &Context, name: &str) -> RealmId {
        if let Some(realm) = ctx.isolate.get(&name.into()) {
            return *realm;
        }
        let mut realms = self.realms.lock().unwrap();
        if let Some(realm) = realms.get(name) {
            return *realm;
        }
        let realm = self.next_realm.fetch_add(1, Ordering::SeqCst);
        realms.insert(name.into(), realm);
        realm
    }

    // ------------------------------------------------------------ 服务解析

    /// 服务解析：**从请求者 fiber 沿父链向上冒泡**，这是 cordis 依赖注入的全部算法。
    ///
    /// 逐条对应 `core/src/reflect.ts:79-96`：
    /// 0. **无 fiber 的定义点**（root、或宿主侧的基础设施上下文）直接读全局表，
    ///    不做注入检查——它根本没有 `inject` 可声明（`reflect.ts:79-81` 的原话是
    ///    "a fiber-less def site cannot declare `inject` at all, so it keeps the
    ///    unchecked root access"）。少了这一条，`root.get('loader.report')`
    ///    这种写法就会莫名其妙地失败。
    /// 1. 本 fiber 已解析到该实现 → 返回；
    /// 2. 该名字在自己的 `inject` 里但没解析到 → "不能读未就绪的必需服务"；
    /// 3. 走到无 fiber 的根 → "没有声明 inject 就不能读属性"；
    /// 4. 跨了隔离域边界 → 拒绝。
    pub fn resolve(&self, ctx: &Ctx, name: &str) -> std::result::Result<Dyn, ResolveError> {
        let fiberless = ctx
            .fiber_opt()
            .map(|fiber| fiber.runtime.is_none())
            .unwrap_or(true);
        if fiberless {
            return self
                .try_get(ctx, name, false)
                .ok_or_else(|| ResolveError::NoInject(name.into()));
        }
        // M3-3：`internal/get` 的 Rust 形态——**查表之前**先咨询读钩子链。
        // 任一钩子返回 `Some` 即接管这次读取（含跳过注入检查——与上游一致：
        // waterfall 包住了做检查的那段 tail，`reflect.ts:82`）。
        let get_hooks: Vec<GetHook> = self
            .get_hooks
            .lock()
            .unwrap()
            .iter()
            .map(|(_, h)| h.clone())
            .collect();
        for hook in get_hooks {
            if let Some(value) = hook(ctx, name) {
                return Ok(value);
            }
        }
        // 可选依赖不进入 committed 快照（不参与激活判定），因此不能按必需依赖
        // 的“声明处停住”逻辑处理：直接按全局服务表探测，缺席时回报 Inactive。
        if let Some(fiber) = ctx.fiber_opt() {
            if fiber.declares_optional_inject(name) && !fiber.declares_inject(name) {
                return self
                    .try_get(ctx, name, true)
                    .ok_or_else(|| ResolveError::Inactive(name.into()));
            }
        }
        let realm = self.realm_of(ctx, name);
        let mut fiber = ctx.fiber();
        loop {
            if let Some(impl_) = fiber.committed(name, realm) {
                return Ok(impl_.get());
            }
            if fiber.declares_inject(name) {
                return Err(ResolveError::Inactive(name.into()));
            }
            if fiber.runtime.is_none() {
                return Err(ResolveError::NoInject(name.into()));
            }
            let Some(parent) = fiber.parent_ctx() else {
                return Err(ResolveError::NoInject(name.into()));
            };
            if self.realm_of(&parent, name) != realm {
                return Err(ResolveError::OutOfRealm(name.into()));
            }
            match parent.fiber_opt() {
                Some(next) => fiber = next,
                None => return Err(ResolveError::NoInject(name.into())),
            }
        }
    }

    /// 宽容读：等价 JS 的 `ctx.get(name)`（`strict=false` 时不要求提供者 ACTIVE）。
    pub fn try_get(&self, ctx: &Ctx, name: &str, strict: bool) -> Option<Dyn> {
        let realm = self.realm_of(ctx, name);
        let impl_ = self.impls.lock().unwrap().get(&realm).cloned()?;
        if strict {
            let active = impl_
                .fiber
                .upgrade()
                .map(|f| f.state() == crate::fiber::FiberState::Active)
                .unwrap_or(false);
            if !active {
                return None;
            }
        }
        Some(impl_.get())
    }

    /// 注册一个服务实现。返回的 disposer 会**先等所有依赖者卸载完，再删自己的条目**
    /// （`reflect.ts:197-203`）。整件事是提供者 fiber 的一个 effect，提供者卸载即回收。
    pub fn provide(
        self: &Arc<Self>,
        ctx: &Ctx,
        name: &str,
        value: Dyn,
        check: Option<Check>,
    ) -> Result<Disposable> {
        let store = Arc::clone(self);
        let ctx = ctx.clone();
        let name_owned: Box<str> = name.into();
        // setup 是同步执行的，所以冲突结果立刻可见；失败时直接返回 `Err`，
        // **不产生恢复栈登记**（`effect_result` 的语义）——曾经这里先登记一条
        // 空壳、再在外层返回错误，调用方拿不到句柄，于是每次撞名都欠一笔永久条目。
        ctx.fiber().effect_result(move |_| {
            let realm = store.realm_of(&ctx, &name_owned);
            let impl_ = Arc::new(Impl::new(
                name_owned.clone(),
                value,
                Arc::downgrade(&ctx.fiber()),
                check,
                realm,
            ));
            {
                let mut impls = store.impls.lock().unwrap();
                if let Some(prev) = impls.get(&realm) {
                    // 冲突信息带上提供者的 fiber uid：这样才能区分
                    // "同一个 fiber 重复 provide"（= 卸载没清干净）与
                    // "两个 fiber 抢同一个名字"（= 真正的配置冲突）。
                    let provider = prev
                        .fiber
                        .upgrade()
                        .map(|fiber| format!("<{}#{}>", fiber.name(), fiber.uid()))
                        .unwrap_or_else(|| "<已卸载的提供者>".to_string());
                    return Err(format!("service \"{name_owned}\" 已被 {provider} 注册").into());
                }
                impls.insert(realm, impl_.clone());
            }
            // 提供者自己的依赖表也要有它，否则它自己读不到自己的服务。
            ctx.fiber().set_dep(name_owned.clone(), impl_.clone());
            // 提交视图同样要登记——这是 `reflect.ts:193` 那一行：
            // `this.ctx.fiber.store![name] = impl`。
            ctx.fiber()
                .committed_insert(name_owned.clone(), impl_.clone());
            if ctx.fiber().state() == crate::fiber::FiberState::Active {
                store.notify(&[&name_owned], &ctx, true);
            }
            let store2 = Arc::clone(&store);
            let ctx2 = ctx.clone();
            let name2 = name_owned.clone();
            Ok(Disposable::new(move || {
                Box::pin(async move {
                    // 顺序与 `reflect.ts:197-203` 逐行一致：
                    //   ① 先摘全局表条目 → ② 通知依赖者 → ③ 等它们拆完 → ④ 才摘自己的提交视图。
                    //（早先这里的注释写成"先等依赖者收敛、再摘除自己"，与代码相反；
                    //  代码一直是对的。两种顺序其实都安全——`check_impl` 要求提供者处于
                    //  ACTIVE，已进入 UNLOADING 的提供者本来就不再"提供"，见原理.md §6.3。）
                    store2.impls.lock().unwrap().remove(&realm);
                    let dependents = store2.notify(&[&name2], &ctx2, true);
                    let provider_fiber = ctx2.fiber_opt();
                    for fiber in dependents {
                        // 依赖者如果正在拆自己（尤其它是 provider 的祖先），等它 settle
                        // 会回头抢父 fiber 已持有的 driver：父 unload 等子 dispose、
                        // 子 provide disposer 又等父 settle，形成死锁。
                        let ancestor_consumer = provider_fiber
                            .as_ref()
                            .is_some_and(|provider| fiber.is_ancestor_of(provider));
                        if fiber.is_disposed()
                            || fiber.state() == crate::fiber::FiberState::Unloading
                            || fiber.is_cycle_blocked(&name2)
                            || ancestor_consumer
                        {
                            if ancestor_consumer {
                                if let Some(provider) = &provider_fiber {
                                    provider.warn_ancestor_consumer(&fiber);
                                }
                            }
                            continue;
                        }
                        fiber.settle().await;
                    }
                    // 最后才从**自己的提交视图**里摘掉自己那条（`reflect.ts:202`）。
                    // 顺序是要害：放到这里，提供者的 disposer 在整个拆除过程中都还能
                    // 读到自己提供的服务——JS 那一行的注释就是
                    // *"ensure self access before dependencies cleanup"*。
                    ctx2.fiber().remove_dep(&name2);
                    ctx2.fiber().committed_remove(&name2, realm);
                })
            }))
        })
    }

    /// 覆盖已注册服务的值。只有提供者 fiber 自己可以写（`reflect.ts:164-175`）。
    ///
    /// M3-3：**写之前**先咨询同步写钩子链（`internal/set` 的 Rust 形态）——
    /// 任一钩子返回 `Some(result)` 即接管这次写入（`Some(Err(..))` 就是否决）。
    pub fn set(&self, ctx: &Ctx, name: &str, value: Dyn) -> Result<()> {
        let set_hooks: Vec<SetHook> = self
            .set_hooks
            .lock()
            .unwrap()
            .iter()
            .map(|(_, h)| h.clone())
            .collect();
        for hook in set_hooks {
            if let Some(result) = hook(ctx, name, &value) {
                return result;
            }
        }
        let realm = self.realm_of(ctx, name);
        let impls = self.impls.lock().unwrap();
        let impl_ = impls
            .get(&realm)
            .ok_or_else(|| format!("cannot set property \"{name}\" without provide"))?;
        let owner_uid = impl_.fiber.upgrade().map(|f| f.uid());
        if owner_uid != Some(ctx.fiber().uid()) {
            return Err(format!("cannot set property \"{name}\" in multiple fibers").into());
        }
        *impl_.value.lock().unwrap() = value;
        Ok(())
    }

    /// 依赖失效传播：这是整个框架**唯一**的"依赖图"实现，比想象中短得多。
    /// 没有拓扑排序——只有"重算 epoch"。
    ///
    /// M3-4：`broadcast_events` 为 true 时**同时**逐个服务名广播 `internal/service`
    /// （值取"当下"的实现，刚被移除 ⇒ `None`）。只有 provide 与移除两条路径广播
    /// （上游 `reflect.ts:223-227` 亦然）；fiber 状态迁移引发的重算**不广播**
    /// ——否则每次激活/卸载都会多出一串事件，且卸载开始那一刻的值是陈旧的。
    pub fn notify(
        self: &Arc<Self>,
        names: &[&str],
        provider_ctx: &Ctx,
        broadcast_events: bool,
    ) -> Vec<Arc<Fiber>> {
        self.notify_inner(names, provider_ctx, None, broadcast_events)
    }

    /// 与 [`Store::notify`] 相同，但用**注册时记录的 realm** 做过滤。
    ///
    /// 加载期在派生 `isolate` ctx 上 `provide` 时，provider 仍是 Loading，不会立刻通知；
    /// 激活时用 base ctx 重算 realm 会漏掉同域 consumer。这里由 `Impl.realm` 给出真值。
    pub fn notify_realm(
        self: &Arc<Self>,
        names: &[&str],
        provider_ctx: &Ctx,
        realm: RealmId,
        broadcast_events: bool,
    ) -> Vec<Arc<Fiber>> {
        self.notify_inner(names, provider_ctx, Some(realm), broadcast_events)
    }

    fn notify_inner(
        self: &Arc<Self>,
        names: &[&str],
        provider_ctx: &Ctx,
        realm_override: Option<RealmId>,
        broadcast_events: bool,
    ) -> Vec<Arc<Fiber>> {
        let mut touched: Vec<Arc<Fiber>> = Vec::new();
        let runtimes: Vec<Arc<Runtime>> = self.runtimes.lock().unwrap().clone();
        for runtime in runtimes {
            let fibers: Vec<Arc<Fiber>> = runtime.fibers.lock().unwrap().clone();
            for fiber in fibers {
                let mut hit = false;
                for name in names {
                    if !fiber.declares_inject(name) {
                        continue;
                    }
                    if let Some(fctx) = fiber.ctx_opt() {
                        let provider_realm =
                            realm_override.unwrap_or_else(|| self.realm_of(provider_ctx, name));
                        if self.realm_of(&fctx, name) != provider_realm {
                            continue;
                        }
                    }
                    hit = true;
                    fiber.check_impl(name);
                }
                if !hit {
                    continue;
                }
                fiber.refresh_desired();
                touched.push(fiber);
            }
        }
        // 浮空驱动：对应 JS 里 `_setEpoch` 里那个不被 await 的 promise。
        if tokio::runtime::Handle::try_current().is_ok() {
            for fiber in &touched {
                let fiber = fiber.clone();
                tokio::spawn(async move { fiber.settle().await });
            }
            // M3-4：逐个名字广播 `internal/service`。
            // - 值取**当下**的实现：provide 路径拿得到值；移除路径 impls 已经摘掉 ⇒ `None`
            //   （对齐上游 `this._getImpl(name, false)?.value` 的 undefined）。
            // - 带 `realm_filter`：只有与提供者同域的注册者收得到（P2-4 的消费点）。
            // - spawn 而不是 await：`notify` 在 provide/disposer 的同步路径上被调用。
            if broadcast_events {
                for name in names {
                    let store = Arc::clone(self);
                    let ctx = provider_ctx.clone();
                    let name = (*name).to_string();
                    let value = self.try_get(provider_ctx, &name, false);
                    let filter = self.realm_filter(provider_ctx, &name);
                    tokio::spawn(async move {
                        let args: Args = vec![Dyn::new(name), Dyn::new(value)];
                        store
                            .dispatch(
                                Mode::Emit,
                                "internal/service",
                                &ctx,
                                args,
                                None,
                                filter.as_ref(),
                            )
                            .await;
                    });
                }
            }
        }
        touched
    }

    /// 本 fiber 提供出去的所有服务名（用于"提供者状态变化 → 通知依赖者"）。
    pub fn impls_owned_by(&self, fiber: &Arc<Fiber>) -> Vec<Box<str>> {
        self.impls
            .lock()
            .unwrap()
            .values()
            .filter(|impl_| {
                impl_
                    .fiber
                    .upgrade()
                    .map(|f| Arc::ptr_eq(&f, fiber))
                    .unwrap_or(false)
            })
            .map(|impl_| impl_.name.clone())
            .collect()
    }

    /// 与 [`Store::impls_owned_by`] 相同，但带上注册时的隔离域。
    ///
    /// 激活通知必须按这对 `(name, realm)` 发：同一个 fiber 可能在派生 isolate
    /// ctx 上提供多个域里的同名服务。
    pub fn impls_owned_by_with_realm(&self, fiber: &Arc<Fiber>) -> Vec<(Box<str>, RealmId)> {
        self.impls
            .lock()
            .unwrap()
            .values()
            .filter(|impl_| {
                impl_
                    .fiber
                    .upgrade()
                    .map(|f| Arc::ptr_eq(&f, fiber))
                    .unwrap_or(false)
            })
            .map(|impl_| (impl_.name.clone(), impl_.realm))
            .collect()
    }

    // ------------------------------------------------------------- provision（M5）

    /// 单个 fiber 的 provision 账（Def 48 / Def 76）。
    ///
    /// **规则 1 的落点**（见 [`ProvisionReport`] 的文档）：`declared` 为空 =
    /// **不透明** ⇒ 直接返回一条没有任何问题的记录，一条检查都不做。
    pub fn check_provision(&self, plugin: &Arc<dyn Plugin>, fiber: &Arc<Fiber>) -> ProvisionReport {
        let declared = plugin.provision();
        let mut installed: Vec<String> = self
            .impls_owned_by(fiber)
            .iter()
            .map(|name| name.to_string())
            .collect();
        installed.sort();
        let mut issues = Vec::new();
        if !declared.is_empty() {
            // Def 48：装了**声明之外**的 key（越界安装）。
            for key in &installed {
                if !declared.iter().any(|allowed| allowed == key) {
                    issues.push(format!(
                        "key \"{key}\" 装上了但**不在声明里**（Def 48）：声明 = {declared:?}"
                    ));
                }
            }
            // Def 76：激活**结束**了却还没装上声明里的 key。
            // 只查已经 ACTIVE 的 fiber——没跑过 apply 的（PENDING）"还没装"不是问题。
            if fiber.state() == crate::FiberState::Active {
                for key in &declared {
                    if !installed.iter().any(|name| name == key) {
                        issues.push(format!(
                            "key \"{key}\" **声明了但激活结束时没有安装**（Def 76）"
                        ));
                    }
                }
            }
        }
        ProvisionReport {
            plugin: plugin.name().to_string(),
            uid: fiber.uid(),
            declared,
            installed,
            issues,
        }
    }

    /// 全部存活 fiber 的 provision 账（诊断面；顺序 = 装载表顺序）。
    ///
    /// 只报告、不裁决：装了越界的 key 照旧可用（附 `issues`），
    /// 因为"能不能装"是策略问题，内核只把**事实**摊开（论文 §6.5 的能力口吻）。
    pub fn provision_report(&self) -> Vec<ProvisionReport> {
        let mut out = Vec::new();
        for runtime in self.runtimes.lock().unwrap().clone() {
            for fiber in runtime.fibers.lock().unwrap().clone() {
                out.push(self.check_provision(&runtime.plugin, &fiber));
            }
        }
        out
    }

    // ------------------------------------------------------------------ 事件

    pub fn on(
        self: &Arc<Self>,
        ctx: &Ctx,
        name: &str,
        handler: Handler,
        prepend: bool,
        global: bool,
    ) -> Disposable {
        // M3-1：先咨询**同步监听器钩子链**（`internal/listener` 的 Rust 形态）。
        // 钩子返回 `Some` ⇒ 这次注册被接管（对应上游 `events.ts:166-167` 的 bail 短路），
        // core 不再把 handler 放进事件表。
        let listener_hooks: Vec<ListenerHook> = self
            .listener_hooks
            .lock()
            .unwrap()
            .iter()
            .map(|(_, hook)| hook.clone())
            .collect();
        for hook in listener_hooks {
            if let Some(taken) = hook(ctx, name, &handler, prepend, global) {
                return taken;
            }
        }
        let store = Arc::clone(self);
        let ctx2 = ctx.clone();
        let name_owned: Box<str> = name.into();
        ctx.fiber().effect(move |_| {
            let sn = store.hook_sn.fetch_add(1, Ordering::SeqCst);
            let hook = Arc::new(event::owned(&ctx2, handler, global, sn));
            {
                let mut hooks = store.hooks.lock().unwrap();
                let list = hooks.entry(name_owned.clone()).or_default();
                if prepend {
                    list.insert(0, hook);
                } else {
                    list.push(hook);
                }
            }
            let store2 = store.clone();
            let name2 = name_owned.clone();
            crate::Disposable::sync(move || {
                let mut hooks = store2.hooks.lock().unwrap();
                if let Some(list) = hooks.get_mut(&name2) {
                    list.retain(|h| h.sn != sn);
                }
            })
        })
    }

    pub fn hook_count(&self, name: &str) -> usize {
        self.hooks
            .lock()
            .unwrap()
            .get(name)
            .map(|l| l.len())
            .unwrap_or(0)
    }

    /// 应用关闭时清空事件表：`Hook` 持注册者 ctx，而 ctx 持 `Store`。
    /// 不清空的话，根级 hook 会让 `Store → Hook → ctx → Store` 留在堆上。
    pub(crate) fn clear_hooks(&self) {
        self.hooks.lock().unwrap().clear();
    }

    /// 注册一个**同步监听器钩子**：`ctx.on` 每次注册前都会先咨询它。
    ///
    /// 钩子返回 `Some(disposable)` ⇒ **接管**这次注册：core 不把 handler 放进事件表，
    /// 生命周期完全由返回的 `Disposable` 负责；返回 `None` ⇒ 交给下一个钩子 / 自动注册。
    ///
    /// 注册本身由调用者 fiber 拥有（effect）：插件卸载时钩子自动摘除。
    pub fn hook_listener<F>(self: &Arc<Self>, ctx: &Ctx, hook: F) -> crate::Disposable
    where
        F: Fn(&Ctx, &str, &Handler, bool, bool) -> Option<Disposable> + Send + Sync + 'static,
    {
        let store = self.clone();
        let hook: ListenerHook = Arc::new(hook);
        ctx.fiber().effect(move |_| {
            let sn = store.observer_sn.fetch_add(1, Ordering::SeqCst);
            store.listener_hooks.lock().unwrap().push((sn, hook));
            let store2 = store.clone();
            crate::Disposable::sync(move || {
                store2
                    .listener_hooks
                    .lock()
                    .unwrap()
                    .retain(|(s, _)| *s != sn);
            })
        })
    }

    /// 注册一个**同步读钩子**（M3-3）：`Ctx::require` 查表之前先咨询它。
    ///
    /// 返回 `Some(值)` ⇒ 接管这次读取；`None` ⇒ 落回正常解析。
    /// 钩子由注册它的 fiber 拥有（卸载即摘除）。
    pub fn hook_get<F>(self: &Arc<Self>, ctx: &Ctx, hook: F) -> crate::Disposable
    where
        F: Fn(&Ctx, &str) -> Option<Dyn> + Send + Sync + 'static,
    {
        let store = self.clone();
        let hook: GetHook = Arc::new(hook);
        ctx.fiber().effect(move |_| {
            let sn = store.observer_sn.fetch_add(1, Ordering::SeqCst);
            store.get_hooks.lock().unwrap().push((sn, hook));
            let store2 = store.clone();
            crate::Disposable::sync(move || {
                store2.get_hooks.lock().unwrap().retain(|(s, _)| *s != sn);
            })
        })
    }

    /// 注册一个**同步写钩子**（M3-3）：`Ctx::set` 落内核之前先咨询它。
    ///
    /// 返回 `Some(Ok(()))` ⇒ 接管且报成功；`Some(Err(..))` ⇒ 否决；
    /// `None` ⇒ 交给下一个钩子 / 内核的 `set`。钩子由注册它的 fiber 拥有。
    pub fn hook_set<F>(self: &Arc<Self>, ctx: &Ctx, hook: F) -> crate::Disposable
    where
        F: Fn(&Ctx, &str, &Dyn) -> Option<Result<()>> + Send + Sync + 'static,
    {
        let store = self.clone();
        let hook: SetHook = Arc::new(hook);
        ctx.fiber().effect(move |_| {
            let sn = store.observer_sn.fetch_add(1, Ordering::SeqCst);
            store.set_hooks.lock().unwrap().push((sn, hook));
            let store2 = store.clone();
            crate::Disposable::sync(move || {
                store2.set_hooks.lock().unwrap().retain(|(s, _)| *s != sn);
            })
        })
    }

    /// 取一次派发要跑的 handler 列表：**既有两轴**（realm/fiber 的 `filter`）
    /// 与 **M4 的 scope 轴**都在这一个点上裁决。
    ///
    /// `from` 是**派发点**的 ctx：scope 轴由它自己的谓词说了算（规则见
    /// [`crate::hook_scope`]）——没有 scope 时这一轴不参与，既有行为逐字不变。
    fn handlers(&self, name: &str, filter: Option<&Filter>, from: &Ctx) -> Vec<Handler> {
        let dispatch_scope = from.scope();
        // 先拍快照，**释放表锁**后再调用 filter/scope 谓词。谓词是用户代码，
        // 它可能再次查询 hook_count（或注册/摘除 hook）；持锁调用会自死锁。
        let snapshot = {
            let hooks = self.hooks.lock().unwrap();
            hooks.get(name).cloned().unwrap_or_default()
        };
        let mut handlers = Vec::with_capacity(snapshot.len());
        for hook in &snapshot {
            // 普通 hook 的 ctx 是强引用；内置 hook 是弱引用。升级失败说明根已关闭，
            // 这条内置 hook 不该再参与。
            let Some(hook_ctx) = hook.ctx.upgrade() else {
                continue;
            };
            if let Some(filter) = filter {
                if !hook.global && !filter(&hook_ctx) {
                    continue;
                }
            }
            if crate::hook_scope::allows(
                dispatch_scope.as_ref(),
                hook.global,
                hook.scope.as_ref(),
                &hook_ctx,
            ) {
                handlers.push(hook.handler.clone());
            }
        }
        handlers
    }

    /// `name` 上**带作用域**的注册都落在哪些 scope 里（M4 的诊断面）。
    ///
    /// 升序去重：一条注册一个 scope，同一个 scope 上多条也只算一次。
    /// 无 scope 的注册**不出现**（它们不属于任何作用域，对谁都参与）。
    pub fn hook_scopes(&self, name: &str) -> Vec<u64> {
        let hooks = self.hooks.lock().unwrap();
        let mut uids: Vec<u64> = hooks
            .get(name)
            .map(|list| {
                list.iter()
                    .filter_map(|hook| hook.scope.as_ref().map(|s| s.uid()))
                    .collect()
            })
            .unwrap_or_default();
        uids.sort_unstable();
        uids.dedup();
        uids
    }

    pub fn dispatch(
        self: &Arc<Self>,
        mode: Mode,
        name: &str,
        ctx: &Ctx,
        args: Args,
        tail: Option<BoxFuture<Ret>>,
        filter: Option<&Filter>,
    ) -> BoxFuture<Ret> {
        // 旧入口：语义逐字不变（不可取消）。M2c 的令牌走 `dispatch_cancellable`。
        self.dispatch_cancellable(mode, name, ctx, args, tail, filter, None)
    }

    /// **带取消令牌**的派发（M2c，D3）：令牌随 `Call.cancel` 到每个 handler。
    ///
    /// 内核本身不因取消而中断链——它只负责把"事实"交给 handler；
    /// 把在飞的挂起续延结算掉是载体层的职责（宿主侧的取消桥）。
    /// 旧入口 `dispatch` 等价于 `cancel = None`，因此**既有测试零改动**。
    #[allow(clippy::too_many_arguments)]
    pub fn dispatch_cancellable(
        self: &Arc<Self>,
        mode: Mode,
        name: &str,
        ctx: &Ctx,
        args: Args,
        tail: Option<BoxFuture<Ret>>,
        filter: Option<&Filter>,
        cancel: Option<crate::CancelToken>,
    ) -> BoxFuture<Ret> {
        let handlers = self.handlers(name, filter, ctx);
        let store = self.clone();
        let ctx = ctx.clone();
        let name_owned = name.to_string();
        Box::pin(async move {
            // 对应 JS：非 internal/* 事件先广播一条 internal/dispatch 供观测/调试。
            if !name_owned.starts_with("internal/") && store.hook_count("internal/dispatch") > 0 {
                let label = name_owned.clone();
                let argc = args.len();
                store
                    .dispatch(
                        Mode::Emit,
                        "internal/dispatch",
                        &ctx,
                        vec![Dyn::new(mode.as_str()), Dyn::new(label), Dyn::new(argc)],
                        None,
                        None,
                    )
                    .await;
            }
            event::dispatch(mode, handlers, ctx, args, tail, cancel).await
        })
    }

    /// 按名字建立隔离域过滤谓词：服务方法被调用时，只有同域内的监听者会被触发。
    pub fn realm_filter(self: &Arc<Self>, provider: &Ctx, name: &str) -> Option<Filter> {
        let store = self.clone();
        let provider = provider.clone();
        let name: Box<str> = name.into();
        Some(Arc::new(move |hook_ctx: &Ctx| {
            store.realm_of(hook_ctx, &name) == store.realm_of(&provider, &name)
        }))
    }

    /// 按 **fiber 子树**过滤监听器：放行"注册者 fiber 就是 `fiber`，或是它的后代"。
    ///
    /// 这是 harness `createScope`（`session-controller/src/client/scope.ts:44-57`）的通用化：
    /// 那边按 session tag 放行（"untagged 全局放行，tagged 只认同代"），这里按
    /// "注册是不是发生在这棵生命周期子树里"放行。
    /// 与 [`Store::realm_filter`] 是两条正交的轴（服务域 vs 生命周期子树）。
    pub fn fiber_filter(self: &Arc<Self>, fiber: &Arc<Fiber>) -> Option<Filter> {
        let target = fiber.uid();
        Some(Arc::new(move |hook_ctx: &Ctx| {
            let mut cursor = hook_ctx.fiber_opt();
            while let Some(fiber) = cursor {
                if fiber.uid() == target {
                    return true;
                }
                cursor = fiber.parent_ctx().and_then(|ctx| ctx.fiber_opt());
            }
            false
        }))
    }

    // ---------------------------------------------------------------- 注册表

    /// 创建一个插件实例。返回的 fiber 已经**开始收敛**（等价 JS 里那个不被 await 的
    /// 浮空 promise），调用方可以 `settle()` 等它稳定。
    pub fn plugin(
        self: &Arc<Self>,
        parent: &Ctx,
        plugin: Arc<dyn Plugin>,
        config: Config,
        inject: Inject,
    ) -> Arc<Fiber> {
        self.plugin_with_optional(parent, plugin, config, inject, &[])
    }

    /// 与 [`Store::plugin`] 相同，但额外登记一组**可选**依赖。
    ///
    /// 可选依赖在 fiber 开始收敛**之前**写入，因此插件 apply 里的
    /// `ctx.require` 一定能看到它们；它们不参与激活判定。
    pub fn plugin_with_optional(
        self: &Arc<Self>,
        parent: &Ctx,
        plugin: Arc<dyn Plugin>,
        config: Config,
        inject: Inject,
        optional_inject: &[String],
    ) -> Arc<Fiber> {
        let runtime = self.runtime_for(&plugin);
        let fiber = Fiber::new(parent, config, inject, Some(runtime), self);
        for name in optional_inject {
            fiber.add_optional_inject(name);
        }
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let fiber = fiber.clone();
            handle.spawn(async move { fiber.settle().await });
        }
        fiber
    }

    fn runtime_for(self: &Arc<Self>, plugin: &Arc<dyn Plugin>) -> Arc<Runtime> {
        let key = plugin.key();
        let mut runtimes = self.runtimes.lock().unwrap();
        if let Some(existing) = runtimes.iter().find(|r| r.key == key) {
            return existing.clone();
        }
        let runtime = Arc::new(Runtime {
            key,
            name: plugin.name().to_string(),
            plugin: plugin.clone(),
            fibers: Mutex::new(Vec::new()),
        });
        runtimes.push(runtime.clone());
        runtime
    }

    pub fn runtime_count(&self) -> usize {
        self.runtimes.lock().unwrap().len()
    }

    /// 按插件**显示名**（不是身份键）取出所有存活的 fiber。
    /// 诊断与运维用：HMR、启动审计、以及"停用某一行的所有实例"。
    pub fn fibers_by_plugin_name(&self, name: &str) -> Vec<Arc<Fiber>> {
        self.runtimes
            .lock()
            .unwrap()
            .iter()
            .filter(|runtime| runtime.name == name)
            .flat_map(|runtime| runtime.fibers.lock().unwrap().clone())
            .collect()
    }

    /// 挂上**根 fiber** 的弱引用（由 [`crate::Fiber::root`] 调用；强引用在 `App` 上）。
    pub(crate) fn bind_root_fiber(&self, fiber: &Arc<Fiber>) {
        *self.root_fiber.lock().unwrap() = Some(Arc::downgrade(fiber));
    }

    /// 根 fiber（诊断用；`App` 还活着时必定可升级）。
    pub fn root_fiber(&self) -> Option<Arc<Fiber>> {
        self.root_fiber
            .lock()
            .unwrap()
            .as_ref()
            .and_then(Weak::upgrade)
    }

    /// 所有存活的 runtime 快照：`(显示名, fiber 数, 身份键)`。
    pub fn inventory(&self) -> Vec<(String, usize, String)> {
        self.runtimes
            .lock()
            .unwrap()
            .iter()
            .map(|runtime| {
                (
                    runtime.name.clone(),
                    runtime.fibers.lock().unwrap().len(),
                    runtime.key.to_string(),
                )
            })
            .collect()
    }

    /// 一次性卸载某插件的所有实例（HMR / 卸载插件用）。
    pub fn delete_plugin(&self, key: &str) -> Vec<Arc<Fiber>> {
        let removed = {
            let mut runtimes = self.runtimes.lock().unwrap();
            let idx = runtimes.iter().position(|r| &*r.key == key);
            idx.map(|i| runtimes.remove(i))
        };
        match removed {
            Some(runtime) => runtime.fibers.lock().unwrap().clone(),
            None => Vec::new(),
        }
    }

    pub(crate) fn register_fiber(&self, runtime: &Arc<Runtime>, fiber: &Arc<Fiber>) {
        runtime.fibers.lock().unwrap().push(fiber.clone());
    }

    pub(crate) fn unregister_fiber(&self, runtime: &Arc<Runtime>, fiber: &Arc<Fiber>) {
        {
            let mut fibers = runtime.fibers.lock().unwrap();
            fibers.retain(|f| !Arc::ptr_eq(f, fiber));
            if !fibers.is_empty() {
                return;
            }
        }
        let mut runtimes = self.runtimes.lock().unwrap();
        runtimes.retain(|r| !Arc::ptr_eq(r, runtime));
    }

    /// 配置解析：先过 `internal/config` 瀑布（loader 在这里做 `!!js` 插值），
    /// 最后交给插件自己的**同步**校验器（cordis 明确拒绝异步 schema）。
    ///
    /// M3-2：派发实参 `[fiber, raw]`——首实参是**正在解析的这个 fiber**
    /// （上游 `fiber.ts:230` 的 `waterfall('internal/config', this, …)`），
    /// 于是监听者能做 per-fiber 的插值/改写。
    ///
    /// M4-4：**瀑布被中断（没有监听者给出配置值）= 错误**。原来这里会静默回落 `raw`，
    /// 而 `!!js` 的插值监听者正是"算不出来就中断"的那一个——静默回落的后果是
    /// **原始标记流进插件的配置**（看起来像"配了个对象"），那是最难查的一类静默错误。
    pub fn resolve_config(
        self: &Arc<Self>,
        ctx: &Ctx,
        runtime: &Arc<Runtime>,
        raw: Config,
    ) -> BoxFuture<Result<Config>> {
        let plugin = runtime.plugin.clone();
        let handlers = self.handlers("internal/config", None, ctx);
        let ctx = ctx.clone();
        let fiber = ctx.fiber();
        Box::pin(async move {
            let tail_raw = raw.clone();
            let tail: BoxFuture<Ret> = Box::pin(async move { Some(Dyn::new(tail_raw)) });
            let result = event::dispatch(
                Mode::Waterfall,
                handlers,
                ctx,
                vec![Dyn::shared(fiber), Dyn::new(raw.clone())],
                Some(tail),
                None,
            )
            .await;
            let final_raw = match result {
                Some(value) => match value.downcast::<Config>() {
                    Some(config) => (*config).clone(),
                    None => {
                        return Err(
                            "internal/config 的监听者返回了非配置值（应当原样透传或替换配置）"
                                .to_string()
                                .into(),
                        )
                    }
                },
                None => {
                    return Err(
                        "internal/config 解析被监听者中断（没有任何监听者给出配置值）——\
                                最常见的原因是某个 `!!js` 监听者求值失败，它的错误日志里带具体路径"
                            .to_string()
                            .into(),
                    )
                }
            };
            plugin.validate(final_raw).map_err(|err| err.into())
        })
    }
}

#[derive(Debug, Clone)]
pub enum ResolveError {
    /// 声明了必需依赖但提供者未就绪。
    Inactive(String),
    /// 没声明 `inject` 就访问该服务。
    NoInject(String),
    /// 跨越了隔离域边界。
    OutOfRealm(String),
    /// 服务在，但它的**值形态**不是被请求的那个类型（M6）。
    ///
    /// 这条变体是 G6 的下半截：过去类型不匹配被伪装成 [`ResolveError::NoInject`]，
    /// 于是"没声明 inject"（配置问题）与"类型对不上"（契约问题）在日志里长得一模一样。
    /// `expected` 是 `std::any::type_name` 的结果（`Iface<dyn Greeter>` 这类），
    /// 只在诊断里用，不参与任何判断。
    TypeMismatch {
        /// 读的那个键。
        name: String,
        /// 期望的类型。
        expected: &'static str,
    },
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResolveError::Inactive(name) => {
                write!(
                    f,
                    "cannot get required service \"{name}\" in inactive context"
                )
            }
            ResolveError::NoInject(name) => {
                write!(f, "cannot get property \"{name}\" without inject")
            }
            ResolveError::OutOfRealm(name) => {
                write!(f, "cannot get property \"{name}\" across isolate boundary")
            }
            ResolveError::TypeMismatch { name, expected } => {
                write!(
                    f,
                    "cannot use property \"{name}\" as {expected}: value has another type"
                )
            }
        }
    }
}

impl std::error::Error for ResolveError {}
