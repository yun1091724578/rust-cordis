//! cordis 内核的最小 Rust 实现。
//!
//! 目标不是"翻译 TypeScript"，而是**提炼出 cordis 的语义内核**，并用 Rust 的表达力
//! 把 JS 时代为了绕过语言限制而存在的部分（Proxy / 原型链 / 生成器 / Promise 接力链）
//! 换成更短的等价物。逐条论证见各模块头部注释，结论汇总在 `docs/` 的方案文档里。
//!
//! 内核只做四件事：
//! 1. **服务容器**：服务按 `(名字, 隔离域)` 注册，按"从请求者 fiber 沿父链向上冒泡"解析。
//! 2. **epoch 驱动的生命周命期**：依赖集合的"版本指纹"一变就自动卸载/重载，
//!    没有拓扑排序、没有手动编排启动顺序。
//! 3. **可逆 effect**：一切注册都是 fiber 拥有的 effect，卸载时逆序释放。
//! 4. **五种事件调度**：`emit` / `parallel` / `serial` / `bail` / `waterfall`。
//!
//! 其余（配置文件装载、隔离域编排、热更新、二进制插件 ABI）都在内核**之外**，
//! 通过内核暴露的扩展点接入——这正是"任意插件可安装"的前提。

#![forbid(unsafe_code)]

mod cancel;
mod context;
mod dyn_value;
mod event;
mod fiber;
mod hook_scope;
mod iface;
mod scope;
mod store;
mod stream;
mod volatile;

pub use cancel::CancelToken;
pub use context::{Context, Ctx, ResolvedConfig};
pub use dyn_value::{
    group, panic_message, BoxFuture, Config, Disposable, Dyn, Error, ExternService, ExternValue,
    IfaceRef, IfaceVersionMismatch, Result, ServiceFuture,
};
pub use event::{is_bailed, Args, Call, Filter, Handler, Mode, Next, Ret};
pub use fiber::{
    Fiber, FiberState, PendingInject, PendingReason, UpdateOutcome, UID_DISPOSED, UID_ROOT,
};
pub use hook_scope::{ScopeAnchor, ScopePredicate};
pub use iface::Iface;
pub use scope::Scope;
pub use store::{
    unique_key, with_provision, Check, FnPlugin, GetHook, Impl, Inject, Level, ListenerHook,
    LogSink, Plugin, PluginKey, ProvisionReport, RealmId, ResolveError, Runtime, SetHook, Store,
    ValidationError,
};
pub use stream::{
    AsyncStream, PushOutcome, StreamObserver, StreamOutcome, StreamSink, STREAM_WINDOW,
};
pub use volatile::{
    declare_volatile, equal_except_volatile, path_value, split_path, validate_volatile_paths,
    values_at, without_volatile, VolatileRef, VolatileValues,
};

use std::sync::Arc;

/// 64 位混合函数（splitmix64 的常数），用于把"依赖指纹"折叠成一个 epoch。
pub fn mix(hash: u64, value: u64) -> u64 {
    let mut h = hash ^ value.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    h ^= h >> 30;
    h = h.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    h ^= h >> 27;
    h = h.wrapping_mul(0x94D0_49BB_1331_11EB);
    h ^ (h >> 31)
}

/// FNV-1a，用于把服务名混进 epoch（保证不同名字的依赖集合不撞车）。
pub fn fnv(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xCBF2_9CE4_8422_2325;
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
    }
    hash
}

/// 一个应用：root 上下文 + 全局注册表。
///
/// 对应 JS 的 `new Context()`。注意 `root` 不是进程级单例——每棵 Context 树各有一份
/// （`context.ts:36-49` 的注释：`root` 是"当前这棵树的根"）。
pub struct App {
    pub store: Arc<Store>,
    pub root: Ctx,
}

impl App {
    pub fn new() -> Self {
        let store = Arc::new(Store::new());
        let root = crate::Fiber::root(&store);
        // 内置监听器（上游 `events.ts:62-69`）：`internal/update` 的**全局 prepend** 钩子，
        // 把"正在更新的那个 fiber 自己的行内钩子"接到全局链最前面。
        // 于是顺序恒为：**行内钩子 → 全局钩子 → tail**。
        let splice: event::Handler = Arc::new(|call: event::Call| {
            let own = call.ctx.fiber().own_hooks(event::INTERNAL_UPDATE);
            let ctx = call.ctx.clone();
            let args = call.args.clone();
            let next_args = call.args.clone();
            let cancel = call.cancel.clone();
            let downstream: BoxFuture<event::Ret> =
                Box::pin(async move { call.next.call(next_args).await });
            event::run_waterfall(own, ctx, args, downstream, cancel)
        });
        store.builtin_hook(&root, event::INTERNAL_UPDATE, splice, true, true);
        Self { store, root }
    }

    /// 根 fiber（与 `root.fiber()` 等价；根 ctx 的保活字段持有它）。
    pub fn root_fiber(&self) -> Arc<crate::Fiber> {
        self.root.fiber()
    }

    /// 挂一个插件并等它稳定。
    pub async fn plugin(&self, plugin: Arc<dyn Plugin>, config: Config) -> Result<Arc<Fiber>> {
        self.root.plugin_and_await(plugin, config).await
    }

    /// 统一关闭入口：递归处置根树，并清空根级 hook。
    ///
    /// 消费 `App`，关闭后不能再挂插件；外层若还持有 `Ctx`/`Store` 克隆，
    /// 只保证这棵 root 树拥有的资源无残留。
    pub async fn close(self) {
        self.root_fiber().dispose().await;
        self.store.clear_hooks();
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

/// 便捷构造一个函数式插件。
///
/// ```ignore
/// let hello = plugin_fn("hello", Inject::of(&["tools"]), |ctx: Ctx, _cfg| Box::pin(async move {
///     ctx.on("x", |call| Box::pin(async move { None }))?;
///     Ok(())
/// }));
/// ```
pub fn plugin_fn<F>(name: &'static str, inject: Inject, apply: F) -> Arc<dyn Plugin>
where
    F: Fn(Ctx, Config) -> BoxFuture<Result<()>> + Send + Sync + 'static,
{
    Arc::new(FnPlugin {
        name,
        key: store::unique_key(name),
        inject,
        validator: None,
        f: Arc::new(apply),
    })
}

/// 便捷构造带同步配置校验的函数式插件。
///
/// 校验失败的消息格式与 JS 完全一致（`invalid config:\n  - msg (at path)`），
/// 因为插件作者写的断言与测试都在依赖这个格式。
pub fn plugin_fn_validated<F, V>(
    name: &'static str,
    inject: Inject,
    validate: V,
    apply: F,
) -> Arc<dyn Plugin>
where
    F: Fn(Ctx, Config) -> BoxFuture<Result<()>> + Send + Sync + 'static,
    V: Fn(Config) -> std::result::Result<Config, ValidationError> + Send + Sync + 'static,
{
    Arc::new(FnPlugin {
        name,
        key: store::unique_key(name),
        inject,
        validator: Some(Arc::new(validate)),
        f: Arc::new(apply),
    })
}
