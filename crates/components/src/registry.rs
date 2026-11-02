//! `registry`：**具名槽位注册表**（M7，G7 的最小版）。
//!
//! # 它解决什么
//! "换一个适配器"不该等于"重启所有依赖它的插件"。上游的原料是
//! `AdapterRegistrationHandle.replace`；本仓的原料是
//! [`Store::set`](cordis_core::Store::set)：**原地改写实现的值，身份不动**
//! ⇒ 依赖者的 epoch 不变、不重挂，而它在自己的提交视图上读到的是**当下**的值
//! （提交视图冻结的是身份，不是值）。
//!
//! # 关键设计：槽位的提供者身份是**注册表自己**
//! [`Store::set`](cordis_core::Store::set) 只允许**提供者 fiber 自己**写。
//! 如果槽位由注册它的插件提供，"替它换值"会被守卫拒绝；让每个插件自己 `ctx.set`
//! 则绕开了注册表、也绕开了"名字是唯一的真相"。所以 `put` 一律由**注册表自己的 ctx**
//! 执行：注册表是提供者，换值因此只需要一次同步写。
//!
//! # 两个面，一张表
//! - `registry@1`：typed / JSON 面（[`Registry`] trait），T2 调用者与 JSON 值走它；
//! - [`HANDLE_KEY`]：原生句柄（[`RegistryHandle`]），T1 插件用它持有任意 [`Dyn`] 值
//!   （含 `#[cordis::service]` 的 typed 服务值）。
//!
//! 为什么要两个键：一个键在实现域内只有一个值，而一个值只有一种形态——
//! `registry@1` 上挂的是 `Iface<dyn Registry>`（带 JSON 面），原生句柄装不进去。
//!
//! # 生命周期
//! - 槽位由**注册表的 fiber** 拥有：注册表卸载 ⇒ 全部回收；
//! - `put` 返回的 disposer 可以提前摘掉一个槽位（跑它 = 摘条目 + 摘名字，
//!   依赖者跟着回到未激活）；
//! - [`RegistryHandle`] 只持 `Weak<Context>`（M4 的 Weak 纪律：句柄通常被别的插件
//!   拿在手里活得更久，不能把上下文树吊住）；注册表卸载后所有入口**优雅降级**。

use std::sync::{Arc, Mutex, Weak};

use cordis_core::{
    plugin_fn, with_provision, Config, Context, Ctx, Disposable, Dyn, Error, ExternValue, Inject,
    Plugin,
};

/// 原生句柄所在的键（用 [`handle`] 取，不用手写这个字符串）。
pub const HANDLE_KEY: &str = "registry.local";

/// 注册表的 **typed / JSON 面**：值的形态是 JSON（`Config`）。
///
/// T2 调用者（动态库插件）与"值本来就是 JSON"的场景走它；T1 插件想放任意
/// 进程内对象（typed 服务、句柄）请用 [`RegistryHandle`]。
#[cordis::service(name = "registry", version = 1)]
pub trait Registry {
    /// 注册一个具名槽位（首次）。同名已存在 ⇒ 错误（换值用 [`Registry::replace`]）。
    async fn put(&self, name: String, value: Config) -> Result<(), String>;

    /// **原地换值**：一次同步写，槽位的身份不变 ⇒ 依赖者的 epoch 不变。
    async fn replace(&self, name: String, value: Config) -> Result<(), String>;

    /// 读当前值（没有这个槽位 ⇒ `None`）。
    async fn get(&self, name: String) -> Result<Option<Config>, String>;

    /// 当前槽位名（按注册顺序；诊断用）。
    async fn names(&self) -> Result<Vec<String>, String>;
}

/// 注册表的**原生句柄**（T1）：槽位值可以是任意 [`Dyn`]。
///
/// 它只持注册表 ctx 的**弱引用**，因此可以被别的插件长期持有：
/// 注册表卸载之后每个入口都会给出可读的降级结果，而不是 panic 或悬空。
pub struct RegistryHandle {
    ctx: Weak<Context>,
    /// 槽位名（按注册顺序）。**值本身只有一份真相**——在服务表里。
    names: Mutex<Vec<String>>,
}

impl RegistryHandle {
    /// 注册一个槽位：值由**注册表自己的 ctx** 提供（见模块头部的"关键设计"）。
    ///
    /// 返回的 disposer 先摘服务表条目、再摘表里的名字；不跑它也不泄漏
    /// （注册表卸载时连同槽位一起回收）。
    pub fn put(self: &Arc<Self>, name: &str, value: Dyn) -> Result<Disposable, Error> {
        let ctx = self.live_ctx()?;
        {
            let names = self.names.lock().unwrap();
            if names.iter().any(|existing| existing == name) {
                return Err(format!("槽位 \"{name}\" 已经注册过：原地换值请用 replace").into());
            }
        }
        let provided = ctx.provide(name, value)?;
        self.names.lock().unwrap().push(name.to_string());
        let table = Arc::downgrade(self);
        let name_owned = name.to_string();
        Ok(Disposable::new(move || {
            Box::pin(async move {
                provided.run().await;
                if let Some(table) = table.upgrade() {
                    table
                        .names
                        .lock()
                        .unwrap()
                        .retain(|existing| existing != &name_owned);
                }
            })
        }))
    }

    /// **原地换值**：一次同步写（[`Store::set`](cordis_core::Store::set)），
    /// 槽位在服务表里的身份不变 ⇒ 依赖者 epoch 不变，但它们读到的是新值。
    pub fn replace(&self, name: &str, value: Dyn) -> Result<(), Error> {
        let ctx = self.live_ctx()?;
        if !self
            .names
            .lock()
            .unwrap()
            .iter()
            .any(|existing| existing == name)
        {
            return Err(format!("槽位 \"{name}\" 还没注册过：先 put 再 replace").into());
        }
        ctx.set(name, value)
    }

    /// 读**当下**的值（走注册表自己的提交视图，不是注册时的快照）。没有 ⇒ `None`。
    pub fn get(&self, name: &str) -> Option<Dyn> {
        let ctx = self.ctx.upgrade()?;
        if !self
            .names
            .lock()
            .unwrap()
            .iter()
            .any(|existing| existing == name)
        {
            return None;
        }
        ctx.get(name)
    }

    /// 当前槽位名（按注册顺序）。注册表已卸载 ⇒ 空表（优雅降级）。
    pub fn names(&self) -> Vec<String> {
        if self.ctx.upgrade().is_none() {
            return Vec::new();
        }
        self.names.lock().unwrap().clone()
    }

    /// 注册表自己的 ctx，且必须**在运行**：`provide` 在未激活/正在卸载的状态上会
    /// `assert`（那是内核护栏），组件不该去撞它——这里先给出可读错误。
    fn live_ctx(&self) -> Result<Ctx, Error> {
        let ctx = self.ctx.upgrade().ok_or_else(|| -> Error {
            "注册表已卸载：请重新挂载 cordis_components::registry::plugin()".into()
        })?;
        if !ctx.is_active() {
            return Err(
                "注册表不在运行（未激活或正在卸载）：槽位只能在它 ACTIVE 时注册或换值".into(),
            );
        }
        Ok(ctx)
    }
}

/// 从 ctx 取原生句柄（注册表必须先挂上，否则 `None`）。
///
/// 走的是宽容读（`Ctx::get`，不做注入检查）：句柄是基础设施对象，
/// 调用方不需要为了拿它而声明 `inject`。
pub fn handle(ctx: &Ctx) -> Option<Arc<RegistryHandle>> {
    ctx.get(HANDLE_KEY)
        .and_then(|value| value.downcast::<RegistryHandle>())
}

/// 挂成插件：提供 `registry@1`（typed + JSON 面）与 [`HANDLE_KEY`]（原生句柄）。
///
/// 两个 key 都写进 provision 声明（M5 的"声明即能力"）：CLI 的 `inspect`
/// 与 `Store::provision_report` 因此能看见这个组件会装什么。
pub fn plugin() -> Arc<dyn Plugin> {
    with_provision(
        plugin_fn(
            "registry",
            Inject::default(),
            |ctx: Ctx, _config: Config| {
                Box::pin(async move {
                    let handle = Arc::new(RegistryHandle {
                        ctx: ctx.downgrade(),
                        names: Mutex::new(Vec::new()),
                    });
                    ctx.provide(HANDLE_KEY, Dyn::shared(handle.clone()))?;
                    RegistryService::provide(&ctx, Arc::new(TypedFace { handle }))?;
                    Ok(())
                })
            },
        ),
        vec![RegistryService::KEY.to_string(), HANDLE_KEY.to_string()],
    )
}

/// typed / JSON 面的实现：把 JSON 值包成 [`ExternValue`] 之后落到同一张表上。
struct TypedFace {
    handle: Arc<RegistryHandle>,
}

#[cordis::service_impl]
impl Registry for TypedFace {
    async fn put(&self, name: String, value: Config) -> Result<(), String> {
        // JSON 面没有 disposer 可还：这类槽位随注册表卸载回收
        // （要提前摘除请用原生句柄的 `put`）。
        self.handle
            .put(&name, Dyn::new(ExternValue(value)))
            .map(|_| ())
            .map_err(|err| err.to_string())
    }

    async fn replace(&self, name: String, value: Config) -> Result<(), String> {
        self.handle
            .replace(&name, Dyn::new(ExternValue(value)))
            .map_err(|err| err.to_string())
    }

    async fn get(&self, name: String) -> Result<Option<Config>, String> {
        Ok(self
            .handle
            .get(&name)
            // 只认 `ExternValue`：typed 值的槽位对 JSON 面不可见（它没有 JSON 表示）。
            .and_then(|value| {
                value
                    .downcast::<ExternValue>()
                    .map(|exposed| exposed.0.clone())
            }))
    }

    async fn names(&self) -> Result<Vec<String>, String> {
        Ok(self.handle.names())
    }
}
