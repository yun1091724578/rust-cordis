//! 五种事件调度。
//!
//! 与 cordis 逐一对齐：`emit` / `parallel` / `serial` / `bail` / `waterfall`。
//! hook 表是**全局单表**（对应 JS 里 `EventsService` 只挂在 root context 上），
//! 隔离域的过滤在派发时按调用者传入的 `filter` 施加。

use crate::dyn_value::{BoxFuture, Dyn};
use crate::Ctx;
use std::sync::{Arc, Weak};

pub type Args = Vec<Dyn>;
pub type Ret = Option<Dyn>;

/// `internal/update`：配置更新瀑布的事件名（M3-1）。行内钩子与全局钩子都挂它。
pub const INTERNAL_UPDATE: &str = "internal/update";

/// waterfall 的续延。
///
/// JS 需要运行期检测 `next() called multiple times`（因为 `next` 是普通函数）。
/// Rust 里 `call` **消费 `self`**，同一续延调两次根本写不出来——整类 bug 消失。
///
/// `call(args)` 接受**新实参**（M2b）：剩余链是**惰性**构造的——不调用就永远不会
/// 为下游建计划、也不会跑一个 handler。这正是 JS `await next(newArgs)` 的语义。
pub struct Next(Option<Box<dyn FnOnce(Args) -> BoxFuture<Ret> + Send>>);

impl Next {
    pub fn none() -> Self {
        Self(None)
    }

    pub fn some(f: impl FnOnce(Args) -> BoxFuture<Ret> + Send + 'static) -> Self {
        Self(Some(Box::new(f)))
    }

    /// 是否**还存在下游**（M2b）：宿主据此判断这次派发是不是 waterfall
    /// （只有 waterfall 模式会给 handler 一个有下游的 `next`）。
    pub fn is_linked(&self) -> bool {
        self.0.is_some()
    }

    pub async fn call(self, args: Args) -> Ret {
        match self.0 {
            Some(f) => f(args).await,
            None => None,
        }
    }
}

/// 一次派发调用。非 waterfall 模式下 `next` 恒为 `Next::none()`。
pub struct Call {
    pub ctx: Ctx,
    pub args: Args,
    pub next: Next,
    /// M2c：这次派发随行的取消令牌（调用方持有；`None` = 不可取消）。
    ///
    /// 内核**不因取消而中断链**——令牌只是随 handler 走的事实；
    /// 谁结算在飞的挂起续延是载体层的事（宿主侧的取消桥）。
    pub cancel: Option<crate::CancelToken>,
}

pub type Handler = Arc<dyn Fn(Call) -> BoxFuture<Ret> + Send + Sync>;

/// 一条 hook 的注册者上下文。
///
/// 普通 hook 强持 ctx：hook 由该 fiber 的 effect 拥有，卸载时一起消失。
/// **内置 hook** 用弱引用：它不挂在任何 fiber 上，若强持根 ctx 就会形成
/// `Store → Hook → root ctx → Store` 环（空 App 丢弃后仍不释放）。
#[derive(Clone)]
pub enum HookCtx {
    Strong(Ctx),
    Weak(Weak<crate::Context>),
}

impl HookCtx {
    pub(crate) fn upgrade(&self) -> Option<Ctx> {
        match self {
            Self::Strong(ctx) => Some(ctx.clone()),
            Self::Weak(ctx) => ctx.upgrade(),
        }
    }
}

#[derive(Clone)]
pub struct Hook {
    /// 注册者上下文。普通 hook 是强引用；内置 hook 是弱引用（见 [`HookCtx`]）。
    pub ctx: HookCtx,
    pub handler: Handler,
    /// `true` 时无视隔离域过滤。
    pub global: bool,
    /// 每个事件名的注册序号，用于原地摘除。
    pub sn: u64,
    /// 注册时所在的作用域（M4，来自注册 ctx）。`None` = 没有作用域，
    /// 于是它对**任何**派发点都参与（本仓的有意偏离，见 [`crate::hook_scope`] 模块文档）。
    pub scope: Option<Arc<crate::ScopeAnchor>>,
}

/// 隔离域过滤谓词：`hook.ctx` 与调用者是否属于同一域。
pub type Filter = Arc<dyn Fn(&Ctx) -> bool + Send + Sync>;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Emit,
    Parallel,
    Serial,
    Bail,
    Waterfall,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Emit => "emit",
            Mode::Parallel => "parallel",
            Mode::Serial => "serial",
            Mode::Bail => "bail",
            Mode::Waterfall => "waterfall",
        }
    }
}

/// `Ret` 是 JS `null | false | undefined | any` 的收敛表示：`None` 覆盖前两者。
///
/// 判定逐条对齐 `events.ts:6-8`：
/// ```ts
/// value !== null && value !== false && value !== undefined
/// ```
/// 所以**显式返回 `false` 也不是 bail 值**——那是"我不表态/否决但不短路"的写法，
/// 派发必须继续往后走。把 `false` 也算成 bail 是实现里最容易漏的一条：
/// `Ret = Option<Dyn>` 会诱使人写成 `ret.is_some()`，那样一个返回 `false`
/// 的权限钩子就会静默截断整条链。
///
/// 跨 ABI 的值统一包在 [`ExternValue`](crate::ExternValue) 里（JSON），
/// 所以 JSON `false` / `null` 必须与原生 `false` / `None` 同义（M1-3）；
/// 其余 JSON 值（含 `0`、空串）都是 bail 值——JS 只排除 `null/false/undefined`。
pub fn is_bailed(ret: &Ret) -> bool {
    let Some(value) = ret else {
        return false; // null / undefined
    };
    if let Some(flag) = value.downcast::<bool>() {
        return *flag; // false → 不短路；true → 短路
    }
    if let Some(extern_value) = value.downcast::<crate::ExternValue>() {
        return match &extern_value.0 {
            serde_json::Value::Bool(flag) => *flag,
            serde_json::Value::Null => false,
            _ => true,
        };
    }
    true // 其余任何值都是 bail 值
}

/// 派发到一串 handler。
///
/// # 有意偏离
/// JS 的 handler 可以 throw，`parallel` 把失败聚合成 `AggregateError`。
/// Rust 的 handler 签名**不可失败**（`BoxFuture<Ret>`），失败由 handler 自己记日志。
/// 理由：给每一种调度模式都塞一条 `Result` 通道，会给 99% 的监听器增加噪声，
/// 而 `AggregateError` 在实践中从未被任何调用点消费过。
pub async fn dispatch(
    mode: Mode,
    handlers: Vec<Handler>,
    ctx: Ctx,
    args: Args,
    tail: Option<BoxFuture<Ret>>,
    cancel: Option<crate::CancelToken>,
) -> Ret {
    match mode {
        // 顺序观察、不短路、丢弃返回值。
        //
        // JS 的 `emit` 是**同步**的（`events.ts:100-103`，连 await 都没有），
        // async Rust 做不到字面意义的同步派发，await 是必然选择。
        // 需要"发了就不等"时，把整次 dispatch 扔进任务里即可
        // ——内核自己就是这么做的（见 `Fiber::emit_status` / `Store::notify`）。
        Mode::Emit => {
            for handler in handlers {
                let call = Call {
                    ctx: ctx.clone(),
                    args: args.clone(),
                    next: Next::none(),
                    cancel: cancel.clone(),
                };
                let _ = handler(call).await;
            }
            None
        }
        // **并发**观察、等全部完成、丢弃返回值。
        //
        // 这就是 `parallel` 与 `emit` 的唯一区别：JS 用 `Promise.allSettled`
        // （`events.ts:95`），于是 N 个各耗时 T 的 handler 总耗时约 T 而非 N×T。
        // 写成串行 await 会让这个调度模式**名不副实**。
        //
        // 失败照旧由 handler 自己记账（偏离清单 #3）：`allSettled` 的 rejected 分支
        // 在这里表现为"任务里的错误不会带出来"。
        Mode::Parallel => {
            let mut tasks = tokio::task::JoinSet::new();
            for handler in handlers {
                let ctx = ctx.clone();
                let args = args.clone();
                let cancel = cancel.clone();
                tasks.spawn(async move {
                    let call = Call {
                        ctx,
                        args,
                        next: Next::none(),
                        cancel,
                    };
                    let _ = handler(call).await;
                });
            }
            // 等**全部**跑完，包括最慢的那一个（`allSettled` 的语义）。
            while tasks.join_next().await.is_some() {}
            None
        }
        // 顺序观察，遇到第一个 bail 值即返回。
        Mode::Serial | Mode::Bail => {
            for handler in handlers {
                let call = Call {
                    ctx: ctx.clone(),
                    args: args.clone(),
                    next: Next::none(),
                    cancel: cancel.clone(),
                };
                let ret = handler(call).await;
                if is_bailed(&ret) {
                    return ret;
                }
            }
            None
        }
        Mode::Waterfall => {
            let tail = tail.unwrap_or_else(|| Box::pin(async { None }));
            run_waterfall(handlers, ctx, args, tail, cancel).await
        }
    }
}

/// 把一串 handler 组合成 waterfall：`handlers[0]` 最先跑，`tail` 是最后一个下游。
///
/// M3-1 起也服务于"行内钩子拼接到全局链首"（`App::new` 里的内置 prepend 钩子），
/// 因此从私有改为 `pub(crate)`。
///
/// **惰性下游**（M2b）：给每个 handler 的 `next` 是一个**闭包**——只有真的调用它
/// （`next.call(args)`）才会为下游建计划。于是"直接返回"的短路不只是语义上的，
/// 下游的 handler 与 tail **一行都不会跑**；同时新实参得以替换下游实参。
pub(crate) fn run_waterfall(
    mut handlers: Vec<Handler>,
    ctx: Ctx,
    args: Args,
    tail: BoxFuture<Ret>,
    cancel: Option<crate::CancelToken>,
) -> BoxFuture<Ret> {
    // 从头部剥一层，余下的递归成"下游"。返回 boxed future，避免无限类型。
    if handlers.is_empty() {
        return tail;
    }
    let head = handlers.remove(0);
    let rest = handlers;
    let rest_ctx = ctx.clone();
    let rest_cancel = cancel.clone();
    Box::pin(async move {
        let next = Next::some(move |new_args: Args| {
            run_waterfall(rest, rest_ctx.clone(), new_args, tail, rest_cancel)
        });
        let call = Call {
            ctx,
            args,
            next,
            cancel,
        };
        head(call).await
    })
}

/// 构造一条 hook 记录（**唯一**的构造点，M4 的 `scope` 在这里落章）。
///
/// 根 ctx 上的 hook 只弱持 ctx：根 ctx 不是任何 fiber 的子 ctx，若强持就会形成
/// `Store → Hook → root ctx → Store`。普通（有 fiber 的）注册仍强持，派生作用域
/// ctx 的生命周期因此与 hook 一致。
pub fn owned(ctx: &Ctx, handler: Handler, global: bool, sn: u64) -> Hook {
    // 只弱持**真正的根 ctx**；`root.filter_subtree()` 这类派生 ctx 仍强持，
    // 因为作用域身份可能只由 hook 本身持有（调用方常常丢掉临时 ctx）。
    let is_root_ctx = ctx.parent.is_none()
        && ctx
            .fiber_opt()
            .is_some_and(|fiber| fiber.uid() == crate::fiber::UID_ROOT);
    let owner = if is_root_ctx {
        HookCtx::Weak(Arc::downgrade(ctx))
    } else {
        HookCtx::Strong(ctx.clone())
    };
    Hook {
        ctx: owner,
        handler,
        global,
        sn,
        scope: ctx.scope(),
    }
}

/// 构造由内核自己持有、不挂 fiber 的内置 hook：注册者 ctx 只持弱引用（防根引用环）。
pub(crate) fn owned_weak(ctx: &Ctx, handler: Handler, global: bool, sn: u64) -> Hook {
    Hook {
        ctx: HookCtx::Weak(Arc::downgrade(ctx)),
        handler,
        global,
        sn,
        scope: ctx.scope(),
    }
}
