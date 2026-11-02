//! Context：服务的容器。
//!
//! # 为什么不复刻 JS 的 Proxy
//! JS 里 `ctx.foo` 靠 Proxy 拦截 + `createTraceable` 影子对象自动做两件事：
//! ① 读属性 → 解析服务；② 服务方法体内的 `this.ctx` 自动重绑到**调用点**上下文，
//! 使 `ctx.tools.register(...)` 的注册归属调用者的 fiber。
//!
//! 这两件事在 Rust 里分别是：
//! ① `ctx.require_as::<T>("foo")`——显式，且用 `Result` 把"没声明 inject 就读服务"
//!    变成可处理的错误，不再是运行期 panic；
//! ② 服务方法首参传 `&Ctx`（use-site）——注册天然归属调用者。
//!
//! 于是 `ReflectService` 的 283 行 Proxy 逻辑、`createTraceable` 的影子机制、
//! `mixin` / `accessor` / `createCallable` 全部消失：**这是"短小精悍"的最大红利**。
//!
//! 本文件零 `unsafe`：所有需要顺父链向上走的方法都收 `self: &Ctx`（即 `&Arc<Context>`），
//! 而不是靠指针回转把 `&Context` 变回 `Arc<Context>`。

use crate::dyn_value::{BoxFuture, Config, Disposable, Dyn, Result};
use crate::event::{Args, Filter, Mode, Ret};
use crate::fiber::{Fiber, FiberState};
use crate::scope::Scope;
use crate::store::{Inject, Level, RealmId, ResolveError, Store};
use crate::Plugin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

pub type Ctx = Arc<Context>;

/// 把一层配置并入累积结果（M3 的合并规则，`resolve_intercept` / `resolve_caller_meta` 共用）。
fn merge_layer(acc: &mut Option<Config>, layer: Config) {
    if layer.is_null() {
        return;
    }
    if let Config::Object(head) = &layer {
        if let Some(Config::Object(base)) = acc {
            for (key, value) in head {
                base.insert(key.clone(), value.clone());
            }
            return;
        }
    }
    *acc = Some(layer);
}

/// 引擎便利通道（M3 通道 ②，**明确不是** Def 26 的 d(k)）。
///
/// 「本插件实际拿到的配置」+「每个已声明依赖在调用点上的合并结果」，一次给全。
///
/// 与论文的分工：[`ExternService::call`](crate::ExternService::call) 的 `caller_meta`
/// 是**提供者侧**在被调用时读到的那一个实参（论文的形态）；这里是**消费侧自省面**，
/// 供诊断、审计与"插件想知道自己声明的服务配置最终长什么样"这类需要。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ResolvedConfig {
    /// 本插件（fiber）**已解析**的配置（`internal/config` 与插件校验器之后）。
    pub plugin: Config,
    /// 已声明的依赖名 → 调用点合并结果（`声明 ⊕ intercept 链`，右偏）。
    ///
    /// 只有**声明过**的名字会出现（`inject` 列表）；没声明过的服务名不会凭空出现，
    /// 哪怕链上恰好有同名层。
    pub services: std::collections::BTreeMap<String, Config>,
}

pub struct Context {
    pub store: Arc<Store>,
    pub parent: Option<Ctx>,
    /// 反向绑定：ctx → fiber。**只持 `Weak`**（M4）——派生 ctx（作用域/隔离域/intercept）
    /// 会把它带下去，强引用会让"派生一个 ctx"变成"给 fiber 加一次计数"，
    /// 而作用域按定义要比 fiber 活得久（见 [`crate::ScopeAnchor`]）。
    /// 根 fiber 的强引用由 [`crate::App`] 持有；插件 fiber 的由装载表/父 fiber 的 effect 持有。
    fiber: Mutex<Option<Weak<Fiber>>>,
    /// 隔离域表：`name -> 域 id`，沿父链冒泡。
    pub isolate: Arc<Scope<Box<str>, RealmId>>,
    /// intercept 配置链，沿父链冒泡、子遮蔽父。
    pub intercept: Arc<Scope<Box<str>, Config>>,
    /// **组件声明链**（论文的 `d`，M3）：`inject: { <svc>: <config> }` 里那些
    /// 非 null 的声明，挂在**声明者自己**的 ctx 上并沿父链冒泡。
    ///
    /// 与 `intercept`（论文的 `ι`）分开两条链是 M3 的全部要害：
    /// 提供者侧读到的合并结果**右偏**——context 携带的 `ι` 覆盖组件声明的 `d`
    /// （论文 p.897-902），所以声明必须能被排到最低优先级，
    /// 而"最近 intercept 层 > 祖先层"这一半只讲 `ι` 自己的链。
    /// 声明与 `ι` 一起冒泡，是"子插件调用时也带着父的声明"这条既有语义。
    declare: Arc<Scope<Box<str>, Config>>,
    /// **作用域**（M4）：`Ctx::filter` 派生的注册期作用域；派生 ctx 继承它。
    ///
    /// 与 `isolate`（服务域）/`intercept`（元数据）都不同：它是"注册**在哪里**"的
    /// 身份，派发期由调用点自己的谓词裁决（见 [`crate::hook_scope`]）。
    hook_scope: Option<Arc<crate::ScopeAnchor>>,
    pub base_url: Option<String>,
    /// **仅供根 ctx**：强持根 fiber，保证 `App::new().root` 这种只留下 root 的用法
    /// 仍保有 fiber 身份。根 fiber 对 ctx 只持弱引用（`ctx_weak`），所以这里不构成环；
    /// 它 drop 时顺带清空事件表，打断 `Store → Hook → ctx → Store`。
    root_keepalive: Mutex<Option<RootKeepalive>>,
}

/// 根 ctx 的保活字段：根 fiber 的强引用 + 关闭时清 hook。
struct RootKeepalive {
    _fiber: Arc<Fiber>,
    store: Arc<Store>,
}

impl Drop for RootKeepalive {
    fn drop(&mut self) {
        self.store.clear_hooks();
    }
}

impl Context {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn assemble(
        store: Arc<Store>,
        parent: Option<Ctx>,
        isolate: Arc<Scope<Box<str>, RealmId>>,
        intercept: Arc<Scope<Box<str>, Config>>,
        declare: Arc<Scope<Box<str>, Config>>,
        hook_scope: Option<Arc<crate::ScopeAnchor>>,
        fiber: Option<Weak<Fiber>>,
        base_url: Option<String>,
    ) -> Ctx {
        Arc::new(Context {
            store,
            parent,
            fiber: Mutex::new(fiber),
            isolate,
            intercept,
            declare,
            hook_scope,
            base_url,
            root_keepalive: Mutex::new(None),
        })
    }

    pub(crate) fn new_root(store: Arc<Store>) -> Ctx {
        Self::assemble(
            store,
            None,
            Scope::root(),
            Scope::root(),
            Scope::root(),
            None,
            None,
            None,
        )
    }

    /// 声明链的当前快照（`Fiber::new` 往父链上追加自己的 `inject` 时用）。
    pub(crate) fn declarations(&self) -> Arc<Scope<Box<str>, Config>> {
        self.declare.clone()
    }

    pub(crate) fn attach_fiber(&self, fiber: &Arc<Fiber>) {
        *self.fiber.lock().unwrap() = Some(Arc::downgrade(fiber));
    }

    /// 给**根 ctx** 装上根 fiber 的保活强引用（由 [`crate::Fiber::root`] 调用）。
    pub(crate) fn attach_root_fiber(&self, fiber: &Arc<Fiber>) {
        *self.root_keepalive.lock().unwrap() = Some(RootKeepalive {
            _fiber: fiber.clone(),
            store: self.store.clone(),
        });
    }

    pub fn fiber(&self) -> Arc<Fiber> {
        self.fiber_opt()
            .expect("context has no fiber（没有绑定过，或它的 fiber 已经走了）")
    }

    pub fn fiber_opt(&self) -> Option<Arc<Fiber>> {
        self.fiber.lock().unwrap().as_ref().and_then(Weak::upgrade)
    }

    /// 弱引用版本。后台任务（如热重载监听）应该持有 `Weak` 而不是 `Arc`，
    /// 否则它们会在插件卸载后继续把整棵上下文树吊住——
    /// 这与内核里"dispose 时清空 `Fiber.ctx` 以打破引用环"的意图直接冲突。
    pub fn downgrade(self: &Ctx) -> std::sync::Weak<Context> {
        Arc::downgrade(self)
    }

    /// **本 ctx 自己**绑的 fiber 的弱引用（不冒泡）。派生 ctx 把它带下去：
    /// 只有父 ctx 自己绑过 fiber 时才传（`Fiber::new` 的挂载 ctx 因此照旧
    /// "无 fiber"，M1 的语义不被牵连），而且传的是 `Weak`——
    /// 派生 ctx 不增加 fiber 的强引用计数（M4 的"作用域比 fiber 活得久"）。
    pub(crate) fn fiber_weak(&self) -> Option<Weak<Fiber>> {
        self.fiber.lock().unwrap().clone()
    }

    // ---------------------------------------------------------------- 作用域（M4）

    /// 派生一个**作用域子 context**（M4，论文之外的"上游对齐项"，G4）：
    /// - 它在父 ctx 之上叠一个**新的作用域身份**（[`crate::ScopeAnchor`]），
    ///   谓词 `predicate` 在派发期逐个裁决"某个注册参不参与"（入参是**注册者**的 ctx，
    ///   对齐上游 `events.ts:76-85` 的 `filter.call(thisArg, hook.ctx)`）；
    /// - 它**带着调用者的 fiber**（G18）：在它上面注册事件是合法操作，
    ///   注册出来的东西照旧由那条 fiber 拥有、随它卸载；
    /// - 服务解析、隔离域、intercept 链、声明链都照常继承。
    ///
    /// 身份与父键关系见 [`crate::ScopeAnchor`]：作用域比 fiber 活得久。
    pub fn filter(self: &Ctx, predicate: crate::hook_scope::ScopePredicate) -> Ctx {
        let anchor = crate::ScopeAnchor::new(self.fiber_weak(), predicate);
        Self::assemble(
            self.store.clone(),
            Some(self.clone()),
            self.isolate.clone(),
            self.intercept.clone(),
            self.declare.clone(),
            Some(anchor),
            self.fiber_weak(),
            self.base_url.clone(),
        )
    }

    /// [`Context::filter`] 的常用组合：只放行**自己这一支**的注册
    /// （自己 + 经 [`Context::bind_scope_parent`] 挂在它下面的后代）。
    ///
    /// 多 agent / 多会话 / 子代理树的标准写法（M11 的 spike 就用它）：
    /// 每个 agent 一个作用域 ctx，各自的拦截器只对自己这一支可见。
    pub fn filter_subtree(self: &Ctx) -> Ctx {
        // 谓词需要锚点本身，而锚点又由 `filter` 造出来——用一个"先建、再回填"的槽。
        // 槽里放锚点的 **Weak**：谓词由锚点持有，若槽再强持锚点就会形成
        // `anchor -> predicate -> slot -> anchor` 的环，丢弃作用域 ctx 后锚点仍不释放。
        let slot: Arc<Mutex<Option<Weak<crate::ScopeAnchor>>>> = Arc::new(Mutex::new(None));
        let slot_in = slot.clone();
        let scoped = self.filter(Arc::new(move |hook_ctx: &Ctx| {
            let Some(anchor) = slot_in.lock().unwrap().as_ref().and_then(Weak::upgrade) else {
                return false;
            };
            hook_ctx
                .scope()
                .map(|hook_anchor| anchor.is_ancestor_or_self_of(&hook_anchor))
                .unwrap_or(false)
        }));
        *slot.lock().unwrap() = scoped.scope().map(|anchor| Arc::downgrade(&anchor));
        scoped
    }

    /// 本 ctx 的作用域（没有就是 `None`；派生 ctx 继承父的）。
    pub fn scope(&self) -> Option<Arc<crate::ScopeAnchor>> {
        self.hook_scope.clone()
    }

    /// 本 ctx 是不是在 `other` 的作用域**家族**里（自己、或沿父键链能到达它）。
    ///
    /// 作用域之间**默认互不可见**（这对多 agent 隔离是要害），
    /// 于是"我这一支"要显式绑定：见 [`Context::bind_scope_parent`] 与
    /// [`Context::filter_subtree`]。
    pub fn in_scope_of(self: &Ctx, other: &Ctx) -> bool {
        match (self.scope(), other.scope()) {
            (Some(mine), Some(theirs)) => theirs.is_ancestor_or_self_of(&mine),
            _ => false,
        }
    }

    /// 把本 ctx 的作用域挂到 `parent` 的下面（**可重绑**）：之后
    /// `self.in_scope_of(parent)` 成立，子代理树的关系也就成了链。
    ///
    /// 成环（父是自己的后代，或就是自己）会被**拒**，且关系原封不动——
    /// 作用域链是要参与可见性裁决的，环会让它变成"人人可见我"。
    pub fn bind_scope_parent(self: &Ctx, parent: &Ctx) -> std::result::Result<(), String> {
        let (Some(mine), Some(theirs)) = (self.scope(), parent.scope()) else {
            return Err(
                "绑定作用域父键要求两边都是作用域 ctx（用 `filter`/`filter_subtree` 派生）"
                    .to_string(),
            );
        };
        mine.bind_parent(&theirs)
    }

    /// 派生一个子上下文，等价 JS 的 `Object.create(this)`。
    ///
    /// `intercept`/`declare` 是**两张表各取一份快照**（M3）：调用方通常只改其中一张。
    /// 作用域与 fiber **原样继承**（M4 / G18）：子 context 上的注册因此照样归属
    /// 调用者的 fiber，`hook_scope` 也照样带下去。
    pub(crate) fn extend_with(
        self: &Ctx,
        intercept: Arc<Scope<Box<str>, Config>>,
        declare: Arc<Scope<Box<str>, Config>>,
    ) -> Ctx {
        Self::assemble(
            self.store.clone(),
            Some(self.clone()),
            self.isolate.clone(),
            intercept,
            declare,
            self.hook_scope.clone(),
            self.fiber_weak(),
            self.base_url.clone(),
        )
    }

    /// 派生一个子上下文（两张表都原样继承）。
    pub fn extend(self: &Ctx) -> Ctx {
        self.extend_with(self.intercept.clone(), self.declare.clone())
    }

    /// 隔离域：`name` 从此走独立的服务域。传入相同 `label` 即共享同一域
    /// （等价 JS 的 `isolate(name, label)`）。
    pub fn isolate(self: &Ctx, name: &str, label: Option<RealmId>) -> Ctx {
        let realm = label.unwrap_or_else(|| self.store.alloc_realm());
        let isolate = Scope::child(&self.isolate, name.into(), realm);
        Self::assemble(
            self.store.clone(),
            Some(self.clone()),
            isolate,
            self.intercept.clone(),
            self.declare.clone(),
            self.hook_scope.clone(),
            self.fiber_weak(),
            self.base_url.clone(),
        )
    }

    /// 为下层插件叠加一条 intercept 配置（论文的 `ι`）。
    pub fn intercept(self: &Ctx, name: &str, config: Config) -> Ctx {
        let intercept = Scope::child(&self.intercept, name.into(), config);
        self.extend_with(intercept, self.declare.clone())
    }

    /// 沿一条链收集某个名字的全部层，**祖先优先、后代覆盖**（M3 的合并规则）。
    ///
    /// 两层都是对象 ⇒ 浅合并（后者覆盖同键）；否则**后来者整个替换**——
    /// "右偏"在非对象层上同样成立（JS 的 `Object.assign` 也是如此）。
    /// `Config::Null` 表示"这一层没有配置"，直接跳过（与 `Fiber::new` 的跳过一致）。
    fn merge_chain(chain: &Arc<Scope<Box<str>, Config>>, name: &str) -> Option<Config> {
        let key: Box<str> = name.into();
        let mut layers = Vec::new();
        let mut cursor = Some(chain.clone());
        while let Some(scope) = cursor {
            if let Some(found) = scope.own(&key) {
                layers.push(found.clone());
            }
            cursor = scope.parent_arc();
        }
        let mut merged: Option<Config> = None;
        for layer in layers.into_iter().rev() {
            merge_layer(&mut merged, layer);
        }
        merged
    }

    /// 沿 intercept 链收集某个服务的全部配置，**祖先优先、后代覆盖**
    /// （对应 `service.ts:51-67` 的 `resolveConfig`）。
    pub fn resolve_intercept(&self, name: &str) -> Option<Config> {
        Self::merge_chain(&self.intercept, name)
    }

    /// 提供者侧读到的那个实参（M3 通道 ①）：`merge(组件声明 d, context 携带的 ι)`，**右偏**。
    ///
    /// 优先级（高 → 低）：最近 intercept 层 > 祖先 intercept 层 > 组件声明。
    /// 声明来自调用者 fiber 的 `inject: { <svc>: <config> }` 条目，`ctx.intercept` 链是 `ι`；
    /// 两者都没有 ⇒ `None`（**不**编造空对象：调用方据此决定要不要下发这个字段）。
    pub fn resolve_caller_meta(&self, name: &str) -> Option<Config> {
        let mut merged = Self::merge_chain(&self.declare, name);
        if let Some(intercept) = self.resolve_intercept(name) {
            merge_layer(&mut merged, intercept);
        }
        merged
    }

    // ---------------------------------------------------------------- 服务

    /// 读一个必需服务。未声明注入、未就绪、跨域都会给出与 cordis 一致的可读错误。
    pub fn require(self: &Ctx, name: &str) -> std::result::Result<Dyn, ResolveError> {
        self.store.resolve(self, name)
    }

    /// 读一个可选服务：不做注入检查，`None` 表示不可用。
    pub fn get(self: &Ctx, name: &str) -> Option<Dyn> {
        self.store.try_get(self, name, true)
    }

    /// 类型化读：`ctx.require_as::<Tools>("tools")?`
    ///
    /// M6：**类型不匹配**有独立变体 [`ResolveError::TypeMismatch`]（原来伪装成
    /// [`ResolveError::NoInject`]——"没声明"与"类型不对"两件事在日志里分不开）。
    pub fn require_as<T: Send + Sync + 'static>(
        self: &Ctx,
        name: &str,
    ) -> std::result::Result<Arc<T>, ResolveError> {
        let value = self.require(name)?;
        value
            .downcast::<T>()
            .ok_or_else(|| ResolveError::TypeMismatch {
                name: name.into(),
                expected: std::any::type_name::<T>(),
            })
    }

    /// 读一个 **T1 typed 服务**（M6，G6）：值形态是 [`crate::Iface<T>`]，
    /// 取回的是**同一份** `Arc<T>`（不加引用层）。
    ///
    /// `#[cordis::service]` 生成的访问器（`<Trait>Service::of`）落在它上面；
    /// 手写插件也可以直接用：`ctx.iface::<dyn Greeter>("greeter@1")?`。
    /// 键的命名规则是 `<name>@<major>`（§6.6 路线 1）。
    pub fn iface<T: ?Sized + Send + Sync + 'static>(
        self: &Ctx,
        name: &str,
    ) -> std::result::Result<Arc<T>, ResolveError> {
        let value = self.require(name)?;
        value
            .downcast::<crate::Iface<T>>()
            .map(|wrapped| wrapped.0.clone())
            .ok_or_else(|| ResolveError::TypeMismatch {
                name: name.into(),
                expected: std::any::type_name::<crate::Iface<T>>(),
            })
    }

    pub fn get_as<T: Send + Sync + 'static>(self: &Ctx, name: &str) -> Option<Arc<T>> {
        self.get(name).and_then(|value| value.downcast::<T>())
    }

    /// 读本 fiber 声明的 **volatile 引用**（M4-5；`path` 是点号路径，与
    /// [`Plugin::volatile_paths`](crate::Plugin::volatile_paths) 里的写法一致）。
    ///
    /// 引用在 fiber 的整个生命周期内**身份稳定**：只差 volatile 字段的配置更新
    /// 会改写它的快照，而不会重挂插件——插件因此可以放心缓存它（连接池、目录句柄……）。
    /// 卸载/重挂会换一个新的引用（`apply` 会再跑一次，插件重新取即可）。
    ///
    /// 没有声明过这个路径 ⇒ `None`（**不**猜、不给默认值：静默给个空引用会让
    /// "配置没生效"变成几小时后才发现的问题）。
    pub fn volatile(self: &Ctx, path: &str) -> Option<Arc<crate::VolatileRef>> {
        self.fiber_opt()?.volatile_ref(path)
    }

    /// 注册服务。返回的 disposer 会先等依赖者卸载完，再摘除自己的条目。
    pub fn provide(self: &Ctx, name: &str, value: Dyn) -> Result<Disposable> {
        self.store.provide(self, name, value, None)
    }

    /// 带就绪谓词的注册（对应 JS 的 `Service[check]`）。
    pub fn provide_checked(
        self: &Ctx,
        name: &str,
        value: Dyn,
        check: crate::store::Check,
    ) -> Result<Disposable> {
        self.store.provide(self, name, value, Some(check))
    }

    pub fn set(self: &Ctx, name: &str, value: Dyn) -> Result<()> {
        self.store.set(self, name, value)
    }

    /// 注册一个**带方法的服务**（跨 ABI 可调用的接口）。
    ///
    /// `version` 是接口版本：接口只能**追加方法**，所以请求方用 ≤ 的版本总能工作。
    pub fn provide_iface(
        self: &Ctx,
        name: &str,
        iface: Arc<dyn crate::ExternService>,
    ) -> Result<Disposable> {
        self.store
            .provide(self, name, Dyn::new(crate::IfaceRef(iface)), None)
    }

    /// 调用一个带方法的服务。
    ///
    /// 逐层校验，且每层的失败原因都可区分（**可降级** vs **不可降级**）：
    /// 1. 服务不存在 → `ResolveError`（能力缺失，可降级）；
    /// 2. 服务存在但**没有可调用面孔**（既不是接口，也没挂 JSON 面）→ 明确报错（配置/契约错误）；
    /// 3. 请求版本高于提供版本 → [`crate::IfaceVersionMismatch`]（契约错误）；
    /// 4. 方法不存在 → 明确报错并**列出可用方法**（可执行的诊断）。
    ///
    /// 「有没有可调用面孔」的唯一判据是 [`crate::Dyn::adapter`]（M6）——于是
    /// `ctx.provide_iface`（T2 提供者）与 `#[cordis::service]` 的 typed 服务
    /// 走的是**同一条**调用路径。
    pub fn call_iface(
        self: &Ctx,
        service: &str,
        version: u32,
        method: &str,
        args: Config,
    ) -> BoxFuture<Result<Config>> {
        // 解析与元数据在**调用时**算；校验与真正的调用留在 future 里
        // （await 之前不产生副作用——既有语义，逐字保留）。
        let (iface, caller_meta) = match self.iface_target(service) {
            Ok(pair) => pair,
            Err(err) => return Box::pin(async move { Err(err) }),
        };
        let (service, method) = (service.to_string(), method.to_string());
        Box::pin(async move {
            Self::check_iface_call(&iface, &service, version, &method)?;
            iface.call(caller_meta, method, args).await
        })
    }

    /// 按**流**调用一个带方法的服务（ADR 0001 §5）。
    ///
    /// 与 [`Context::call_iface`] 同一套解析与逐层校验（缺服务 / 没有可调用面孔 /
    /// 版本过高 / 未知方法收敛到同一条可读错误），区别只在结果形态：这里**立刻**
    /// 返回 [`crate::AsyncStream`]，块随后到达（提供者推、消费者逐块取）。
    ///
    /// 提供者不必写流式代码：默认实现把它的返回值当成"长度 1 的流"。
    pub fn call_iface_stream(
        self: &Ctx,
        service: &str,
        version: u32,
        method: &str,
        args: Config,
    ) -> Result<crate::AsyncStream> {
        let (iface, caller_meta) = self.iface_target(service)?;
        Self::check_iface_call(&iface, service, version, method)?;
        let (sink, mut stream) = crate::StreamSink::new(crate::STREAM_WINDOW);
        let mut future = iface.call_stream(caller_meta, method.to_string(), args, sink.clone());
        // 提供者的前半段要**现在就跑到第一个挂起点**：同步实现的提供者（含一切跨 ABI 的）
        // 在这一步就把窗口填满；真异步的提供者交给运行时继续跑——消费者于是可以立刻
        // 开始取块（生产与消费并行，而不是串行等待整条流算完）。
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);
        match future.as_mut().poll(&mut cx) {
            std::task::Poll::Ready(Ok(())) => Ok(stream),
            // 同步失败：当场报错（流本身随 `stream` 的丢弃一起收场）。
            std::task::Poll::Ready(Err(err)) => Err(err),
            std::task::Poll::Pending => {
                let Some(fiber) = self.fiber_opt() else {
                    sink.fail(
                        "提供者的流式实现还没就绪，而当前上下文没有绑定 fiber：\
                         无法把后台任务纳入生命周期",
                    );
                    return Ok(stream);
                };
                let cancel = crate::CancelToken::new();
                let task_cancel = cancel.clone();
                let task_sink = sink.clone();
                // 提供者任务归属于**调用点 fiber**：
                // - 丢弃流 ⇒ `AsyncStream::drop` 发取消，不再让它跑副作用；
                // - `App::close` / 卸载 ⇒ `Fiber::dispose_native_tasks` 取消并等待。
                if let Err(err) = fiber.spawn_native_task(cancel.clone(), async move {
                    // 取消是**已经发生的事实**时必须先结算：`select!` 默认随机轮询，
                    // 取消与结果同时就绪时可能先跑业务 future，让已经丢弃的流继续
                    // 执行副作用。先同步检查一次，再让 select 的取消分支固定优先。
                    if task_cancel.is_cancelled() {
                        task_sink.fail("流式调用已取消：调用方或生命周期已关闭");
                        return;
                    }
                    tokio::select! {
                        biased;
                        _ = task_cancel.cancelled() => {
                            task_sink.fail("流式调用已取消：调用方或生命周期已关闭");
                        }
                        result = future => {
                            if let Err(err) = result {
                                task_sink.fail(err.to_string());
                            }
                        }
                    }
                }) {
                    sink.fail(format!("流式调用无法绑定到生命周期: {err}"));
                    return Ok(stream);
                }
                stream.attach_cancel(cancel);
                Ok(stream)
            }
        }
    }

    /// 解析一个可调用服务，并算出**调用点**的合并元数据。
    ///
    /// `call_iface` 与 `call_iface_stream` 共用这一条解析链，于是两者的失败面
    /// 完全一致（同一句话解释"为什么调用不了"）。
    fn iface_target(
        self: &Ctx,
        service: &str,
    ) -> Result<(Arc<dyn crate::ExternService>, Option<Config>)> {
        let value = self.require(service)?;
        let Some(iface) = value.adapter() else {
            return Err(format!(
                "service \"{service}\" 不是可调用接口：它既不是 ctx.provide_iface() 注册的接口，\
                 也没有 JSON 面。提供方需要用其中之一把它暴露成可调用服务\
                 （typed 服务由 `#[cordis::service]` 的 provide 自动挂上 JSON 面）"
            )
            .into());
        };
        // M3（通道 ①）：提供者侧读到的那个实参在**调用时**算——`merge(声明 d, ι)`，右偏。
        // 在调用点上算（而不是注册时）是语义的一部分：同一条链上的两次调用
        // 可以带不同的 use-site 元数据，谁也不需要被重启。
        let caller_meta = self.resolve_caller_meta(service);
        Ok((iface, caller_meta))
    }

    /// 版本与方法的逐层校验（`call_iface` / `call_iface_stream` 共用一条判据）。
    fn check_iface_call(
        iface: &Arc<dyn crate::ExternService>,
        service: &str,
        version: u32,
        method: &str,
    ) -> Result<()> {
        let offered = iface.version();
        if version > offered {
            return Err(crate::IfaceVersionMismatch {
                service: service.to_string(),
                requested: version,
                offered,
            }
            .into());
        }
        let methods = iface.methods();
        if !methods.iter().any(|known| known == method) {
            return Err(format!(
                "service \"{service}\" 没有方法 \"{method}\"；可用方法: {methods:?}"
            )
            .into());
        }
        Ok(())
    }

    /// 见 [`ResolvedConfig`]：引擎便利通道（M3 通道 ②）。
    ///
    /// 根 ctx（没有 fiber）给空值——它不是任何插件的调用点。
    pub fn resolved_config(&self) -> ResolvedConfig {
        let Some(fiber) = self.fiber_opt() else {
            return ResolvedConfig::default();
        };
        let services = fiber
            .inject_names()
            .into_iter()
            .map(|name| {
                let merged = self.resolve_caller_meta(&name).unwrap_or(Config::Null);
                (name, merged)
            })
            .collect();
        ResolvedConfig {
            plugin: fiber.resolved(),
            services,
        }
    }

    // ---------------------------------------------------------------- 事件

    /// 注册事件监听器。监听器由**调用者 fiber** 拥有（卸载即摘除）。
    ///
    /// 注意：注册本身可能被**同步监听器钩子链**接管（M3-1）——比如
    /// `ctx.on("internal/update", …)` 会被路由成 fiber 的行内钩子（只对该 fiber 生效）。
    /// 见 [`Store::hook_listener`](crate::Store::hook_listener)。
    pub fn on<F>(self: &Ctx, name: &str, handler: F) -> Disposable
    where
        F: Fn(crate::event::Call) -> BoxFuture<Ret> + Send + Sync + 'static,
    {
        self.store.on(self, name, Arc::new(handler), false, false)
    }

    pub fn on_with<F>(self: &Ctx, name: &str, handler: F, prepend: bool, global: bool) -> Disposable
    where
        F: Fn(crate::event::Call) -> BoxFuture<Ret> + Send + Sync + 'static,
    {
        self.store
            .on(self, name, Arc::new(handler), prepend, global)
    }

    /// 只触发一次的监听器：触发后**自动摘除**（对应 `events.ts:173-179`——
    /// 包装器先 `dispose()` 再调用原监听器）。
    ///
    /// 返回的句柄可以提前摘除；提前摘除后再触发不会跑，
    /// 触发之后再手动摘除是**安全空操作**（幂等）。
    pub fn once<F>(self: &Ctx, name: &str, handler: F) -> Disposable
    where
        F: Fn(crate::event::Call) -> BoxFuture<Ret> + Send + Sync + 'static,
    {
        // 句柄与被包装的监听器互相需要（注册完才有句柄），用一个共享槽解这个环。
        let handler: Arc<F> = Arc::new(handler);
        let slot: Arc<Mutex<Option<Disposable>>> = Arc::new(Mutex::new(None));
        let fired = Arc::new(AtomicBool::new(false));
        let slot_in = slot.clone();
        let fired_in = fired.clone();
        let handler_in = handler.clone();
        let inner = self.on(name, move |call: crate::event::Call| {
            let slot = slot_in.clone();
            let fired = fired_in.clone();
            let handler = handler_in.clone();
            Box::pin(async move {
                // `once` 的"恰好一次"不能只靠 `slot.take()`：注册完成前到达的派发
                // 会看到 slot 还是 None，随后 hook 仍在表里，下一次派发就会再跑一次。
                // `fired` 覆盖这个窗口；slot 为空时直接不表态。
                if fired.swap(true, Ordering::SeqCst) {
                    return None;
                }
                let taken = slot.lock().unwrap().take();
                if let Some(disposable) = taken {
                    disposable.run().await;
                }
                (handler)(call).await
            })
        });
        *slot.lock().unwrap() = Some(inner);

        let slot_out = slot;
        Disposable::new(move || {
            Box::pin(async move {
                let taken = slot_out.lock().unwrap().take();
                if let Some(disposable) = taken {
                    disposable.run().await;
                }
            })
        })
    }

    pub fn emit(self: &Ctx, name: &str, args: Args) -> BoxFuture<()> {
        let ctx = self.clone();
        let name = name.to_string();
        Box::pin(async move {
            ctx.store
                .dispatch(Mode::Emit, &name, &ctx, args, None, None)
                .await;
        })
    }

    /// 五种调度都返回 `'static` 的未来（内部持有事件名），
    /// 这样它们可以被塞进任何地方（跨 ABI 的兜底路径、后台任务、组合器），
    /// 而不会把生命周期绑在调用点的字符串上。
    pub fn parallel(self: &Ctx, name: &str, args: Args) -> BoxFuture<Ret> {
        let ctx = self.clone();
        let name = name.to_string();
        Box::pin(async move {
            ctx.store
                .dispatch(Mode::Parallel, &name, &ctx, args, None, None)
                .await
        })
    }

    pub fn serial(self: &Ctx, name: &str, args: Args) -> BoxFuture<Ret> {
        let ctx = self.clone();
        let name = name.to_string();
        Box::pin(async move {
            ctx.store
                .dispatch(Mode::Serial, &name, &ctx, args, None, None)
                .await
        })
    }

    pub fn bail(self: &Ctx, name: &str, args: Args) -> BoxFuture<Ret> {
        let ctx = self.clone();
        let name = name.to_string();
        Box::pin(async move {
            ctx.store
                .dispatch(Mode::Bail, &name, &ctx, args, None, None)
                .await
        })
    }

    /// 环绕中间件。`tail` 是链尾，不调用 `next` 即短路。
    pub fn waterfall(
        self: &Ctx,
        name: &str,
        args: Args,
        tail: Option<BoxFuture<Ret>>,
        filter: Option<Filter>,
    ) -> BoxFuture<Ret> {
        let ctx = self.clone();
        let name = name.to_string();
        self.store
            .dispatch(Mode::Waterfall, &name, &ctx, args, tail, filter.as_ref())
    }

    pub fn hook_count(&self, name: &str) -> usize {
        self.store.hook_count(name)
    }

    // ---------------------------------------------------------------- fiber

    pub fn effect<F>(self: &Ctx, setup: F) -> Disposable
    where
        F: FnOnce(Ctx) -> Disposable + Send + 'static,
    {
        self.fiber().effect(setup)
    }

    /// 挂一个插件实例；已处置/脱离的上下文返回可读错误，而不是 panic。
    ///
    /// 这是 [`Ctx::plugin`] 的可检查版本。竞态窗口里父 fiber 可能刚好进入
    /// `UNLOADING`：此时返回的 fiber 会以 `FAILED` 收敛，`settle()` 能看到错误。
    pub fn try_plugin(
        self: &Ctx,
        plugin: Arc<dyn Plugin>,
        config: Config,
    ) -> std::result::Result<Arc<Fiber>, String> {
        match self.fiber_opt() {
            Some(fiber)
                if !fiber.is_disposed()
                    && !matches!(fiber.state(), FiberState::Disposed | FiberState::Unloading) => {}
            _ => {
                return Err(
                    "cannot mount plugin on a disposed context（上下文已经关闭或父 fiber 已离开）"
                        .into(),
                );
            }
        }
        let inject = plugin.inject();
        Ok(self.store.plugin(self, plugin, config, inject))
    }

    /// 挂一个插件实例。**不等待它激活**——`await` 返回的 future 即可等它收敛。
    ///
    /// 对已处置上下文会 panic；需要可恢复错误时用 [`Ctx::try_plugin`]。
    pub fn plugin(self: &Ctx, plugin: Arc<dyn Plugin>, config: Config) -> Arc<Fiber> {
        self.try_plugin(plugin, config)
            .unwrap_or_else(|err| panic!("{err}"))
    }

    /// 挂一个插件并等它稳定；失败会返回错误。
    pub fn plugin_and_await(
        self: &Ctx,
        plugin: Arc<dyn Plugin>,
        config: Config,
    ) -> BoxFuture<Result<Arc<Fiber>>> {
        let fiber = self.plugin(plugin, config);
        Box::pin(async move {
            fiber.settle().await;
            match fiber.error() {
                Some(err) => Err(err.into()),
                None => Ok(fiber),
            }
        })
    }

    /// 迟绑定依赖：依赖齐了就执行 `callback`，依赖消失则自动卸载、回来再重跑。
    /// 这是表达"可选能力"的标准手法（对应 `ctx.inject(['attachments'], ...)`）。
    pub fn inject<F>(self: &Ctx, inject: Inject, callback: F) -> Arc<Fiber>
    where
        F: Fn(Ctx) -> BoxFuture<Result<()>> + Send + Sync + 'static,
    {
        let plugin = Arc::new(crate::FnPlugin {
            name: "ctx.inject",
            key: crate::store::unique_key("ctx.inject"),
            inject,
            validator: None,
            f: Arc::new(move |ctx, _config| callback(ctx)),
        });
        self.plugin(plugin, Config::Null)
    }

    pub fn log(&self, level: Level, message: &str) {
        let name = self
            .fiber_opt()
            .map(|f| f.name())
            .unwrap_or_else(|| "root".to_string());
        self.store.log(level, &name, message);
    }

    pub fn is_active(&self) -> bool {
        self.fiber_opt()
            .map(|f| f.state() == FiberState::Active)
            .unwrap_or(false)
    }
}
