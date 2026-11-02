//! 插件侧 SDK：把 C ABI 的样板全部藏起来，让插件作者写的是普通 Rust。
//!
//! 用法（插件 crate 的 `lib.rs`）：
//! ```ignore
//! cordis_abi::cordis_plugin! {
//!     name: "hello",
//!     inject: ["greeting"],
//!     apply: |ctx, config| {
//!         ctx.provide_iface("hello.gate", 1, vec![("greet", |args, meta| {
//!             // `meta` = 调用点上的 `d(k) ⊕ ι(k)`（M3，右偏）；没有元数据时是 `None`。
//!             let _ = (args, meta);
//!             Ok(serde_json::json!({ "message": "hi" }))
//!         })])?;
//!         ctx.call_iface("greeting", 1, "text", serde_json::json!({}), |result| {
//!             eprintln!("结果是 {result:?}");
//!         })?;
//!         ctx.effect(|| { /* 释放资源 */ })?;
//!         Ok(())
//!     },
//! }
//! ```
//!
//! # 三条必须知道的约束
//! 1. **回调必须是同步函数**（返回 `Result<Json, String>`，不能返回 future）。
//!    这是 C ABI 与异步内核之间唯一的语义收窄；需要异步就用 `ctx.dispatch`。
//! 2. **跨 ABI 的值统一是 JSON**。Rust 进程内插件之间可以直接传 `Arc<T>`；
//!    越过 ABI 的一律降级。
//! 3. **一切注册都由宿主 fiber 拥有**，插件不需要记账：插件卸载时宿主会自动回收，
//!    并回调你注册的 `ctx.effect` 清理动作。

// clippy 1.98 会把已经是 `const { … }` 的初始化器继续报成“可改为 const”。
#![allow(clippy::missing_const_for_thread_local)]

use crate::{
    Blob, CtxAbortFn, CtxCallIfaceFn, CtxDispatchFn, CtxDisposeFn, CtxDisposeSelfFn, CtxEffectFn,
    CtxEmitFn, CtxFulfillFn, CtxGetFn, CtxInterceptFn, CtxIsolateFn, CtxNextFn, CtxOnFn,
    CtxProvideFn, CtxProvideIfaceFn, CtxRequireFn, CtxSetFn, CtxStreamCloseFn, CtxStreamFailFn,
    CtxStreamPushFn, CtxUseFn, HostApi, LogFn, Out, PluginDesc, ENTRY_SYMBOL, KIND_DISPATCH,
    KIND_IFACE, OP_APPLY, OP_CALLBACK, OP_CLEANUP, OP_DISPOSE, OP_PING, OP_VOLATILE_APPLY,
    OP_VOLATILE_PATHS, STATUS_ERR, STATUS_OK, STATUS_PENDING, STATUS_STREAM_GONE, STATUS_TOO_SMALL,
    STATUS_UNSUPPORTED,
};
use serde_json::Value;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

pub type Json = Value;
pub type PluginResult = Result<(), String>;

/// 宿主调用在 `STATUS_TOO_SMALL` 时能否安全重放。
///
/// 协议允许宿主回填所需长度后重试，但**只有只读/幂等操作**能重试；
/// 写操作（注册、派发、provide、apply）在回写结果之前已经执行，重放会重复副作用。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Retry {
    Safe,
    Never,
}

/// 事件监听器 / 接口方法 / 派发续延的统一形态。
pub type Handler = Arc<dyn Fn(CallbackInput) -> Result<Json, String> + Send + Sync>;
/// **可挂起**的监听器（M2）：`Fn(入参, dispatch_id) -> Result<AsyncReply, String>`。
pub type AsyncHandler = Arc<dyn Fn(Vec<Json>, u64) -> Result<AsyncReply, String> + Send + Sync>;

/// 异步监听器的返回形态（M2，D3）。
#[derive(Debug, Clone, PartialEq)]
pub enum AsyncReply {
    /// 同步返回一个值：与 [`Ctx::on`] 的返回同义。
    Reply(Json),
    /// 挂起：稍后用 [`Ctx::fulfill`] 结算（每次派发**恰好结算一次**）。
    Pending,
}

/// 一次流式推送的结局（插件侧视角，ADR 0001 §5）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StreamPush {
    /// 块已被接受（进了窗口）。
    Sent,
    /// 窗口满了：**块没有被接受**，等 `on_credit` 给的额度再重推这一块。
    Pending,
    /// 这条流已经不在了（消费者离场 / 已收尾）：停手。
    Gone,
}

/// 宿主对"等着推"的回应（[`Ctx::stream_push`] 的 `on_credit` 参数）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StreamCredit {
    /// 腾出了 `window` 个位置：继续推。
    Window(usize),
    /// 这条流已经不在了：停手，别再推（与推送拿到 [`StreamPush::Gone`] 同义）。
    Gone,
}

/// 接口方法体：入参是 JSON，出参是 JSON。
///
/// 第二个参数是**调用点上的合并元数据** `d(k) ⊕ ι(k)`（M3，右偏；没有元数据时为 `None`）。
/// 内核只传递它、不裁决——访问控制、审计、按调用方切换行为都由提供者自己决定。
pub type IfaceFn = Arc<dyn Fn(Json, Option<Json>) -> Result<Json, String> + Send + Sync>;
/// **可挂起**的接口方法体（ADR 0001 §4）：第三个参数是这次入站回调的 `dispatch_id`。
///
/// 返回 `AsyncReply::Pending` ⇒ 宿主把这次调用挂起，直到 `ctx.fulfill(dispatch_id, value)`
/// 结算（与 `on_async` 的监听器是**同一套**机制：先登记后调用、卸载先排水）。
/// 于是"服务"和"事件"在异步这件事上没有差别——harness 的模型适配器/工具/日志
/// 都可以写成服务方法，而不必为了挂起能力改写成事件监听器。
pub type AsyncIfaceFn =
    Arc<dyn Fn(Json, Option<Json>, u64) -> Result<AsyncReply, String> + Send + Sync>;
pub type Cleanup = Box<dyn FnOnce() + Send>;
pub type ApplyFn = dyn Fn(&Ctx, Json) -> PluginResult + Send + Sync;

/// 宿主回调进来的载荷。
pub struct CallbackInput {
    pub kind: CallbackKind,
    /// 事件名，或（`kind == Iface` 时）服务名。
    pub event: String,
    /// 方法名（`kind == Iface` 时有效）。
    pub method: Option<String>,
    pub args: Vec<Json>,
    /// 本次派发的 id（M2）：挂起后用它 [`Ctx::fulfill`] 结算。
    /// 旧宿主不下发这个字段时为 0（挂起因此不可用——但那本来也不支持）。
    pub dispatch_id: u64,
    /// **调用点上的合并元数据** `d(k) ⊕ ι(k)`（M3，右偏）——只对接口方法派发有意义。
    ///
    /// 调用方（或宿主）没带元数据时是 `None`：**不编造空对象**，
    /// 让提供者能区分"没有元数据"与"元数据是 `{}`"。
    pub meta: Option<Json>,
}

impl CallbackInput {
    fn parse(payload: &Json) -> Self {
        let kind = match payload.get("kind").and_then(|v| v.as_str()) {
            Some(KIND_IFACE) => CallbackKind::Iface,
            Some(KIND_DISPATCH) => CallbackKind::Dispatch,
            _ => CallbackKind::Event,
        };
        Self {
            kind,
            event: payload
                .get("event")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            method: payload
                .get("method")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            args: payload
                .get("args")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default(),
            dispatch_id: payload
                .get("dispatch_id")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            meta: payload
                .get("meta")
                .cloned()
                .filter(|value| !value.is_null()),
        }
    }

    /// 接口方法的入参（单个 JSON）。
    pub fn arg(&self) -> Json {
        self.args.first().cloned().unwrap_or(Json::Null)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CallbackKind {
    Event,
    Iface,
    Dispatch,
}

/// 插件侧持有的宿主函数表 + 本次调用的上下文句柄。
#[derive(Clone, Copy)]
pub struct Ctx {
    host: &'static HostApi,
    handle: u64,
}

/// 读一个尾追加函数前的存在性判断。
///
/// `attach` 已经把宿主表按 `host.size` **拷**进插件自有副本（copy-on-attach，
/// G15），所以这里的 `host.field_at` 比较的是**副本里**的 `size`——语义上它区分的
/// 不再是「能不能安全读」（副本永远是全尺寸、零初始化的），而是「宿主当场的
/// 版本到底提供没提供这个函数」。越界读在这个结构下**无从发生**，但保留
/// size 比较能让"宿主不支持"与"宿主支持但关闭"继续区分下去。
macro_rules! api {
    ($host:expr, $field:ident, $ty:ty) => {
        unsafe {
            $host
                .field_at::<Option<$ty>>(std::mem::offset_of!(HostApi, $field))
                .flatten()
        }
    };
}

impl Ctx {
    pub fn handle(&self) -> u64 {
        self.handle
    }

    fn get_fn(&self) -> Option<CtxGetFn> {
        api!(self.host, ctx_get, CtxGetFn)
    }
    fn require_fn(&self) -> Option<CtxRequireFn> {
        api!(self.host, ctx_require, CtxRequireFn)
    }
    fn set_fn(&self) -> Option<CtxSetFn> {
        api!(self.host, ctx_set, CtxSetFn)
    }
    fn provide_fn(&self) -> Option<CtxProvideFn> {
        api!(self.host, ctx_provide, CtxProvideFn)
    }
    fn on_fn(&self) -> Option<CtxOnFn> {
        api!(self.host, ctx_on, CtxOnFn)
    }
    fn effect_fn(&self) -> Option<CtxEffectFn> {
        api!(self.host, ctx_effect, CtxEffectFn)
    }
    fn emit_fn(&self) -> Option<CtxEmitFn> {
        api!(self.host, ctx_emit, CtxEmitFn)
    }
    fn log_fn(&self) -> Option<LogFn> {
        api!(self.host, log, LogFn)
    }
    fn provide_iface_fn(&self) -> Option<CtxProvideIfaceFn> {
        api!(self.host, ctx_provide_iface, CtxProvideIfaceFn)
    }
    fn call_iface_fn(&self) -> Option<CtxCallIfaceFn> {
        api!(self.host, ctx_call_iface, CtxCallIfaceFn)
    }
    fn dispatch_fn(&self) -> Option<CtxDispatchFn> {
        api!(self.host, ctx_dispatch, CtxDispatchFn)
    }
    fn isolate_fn(&self) -> Option<CtxIsolateFn> {
        api!(self.host, ctx_isolate, CtxIsolateFn)
    }
    fn intercept_fn(&self) -> Option<CtxInterceptFn> {
        api!(self.host, ctx_intercept, CtxInterceptFn)
    }
    fn use_fn(&self) -> Option<CtxUseFn> {
        api!(self.host, ctx_use, CtxUseFn)
    }
    fn dispose_fn(&self) -> Option<CtxDisposeFn> {
        api!(self.host, ctx_dispose, CtxDisposeFn)
    }
    fn dispose_self_fn(&self) -> Option<CtxDisposeSelfFn> {
        api!(self.host, ctx_dispose_self, CtxDisposeSelfFn)
    }

    /// M2：结算一个挂起的入站续延（#17）。
    fn fulfill_fn(&self) -> Option<CtxFulfillFn> {
        api!(self.host, ctx_fulfill, CtxFulfillFn)
    }

    /// M2b：续跑 waterfall 的剩余链（#18）。
    fn next_fn(&self) -> Option<CtxNextFn> {
        api!(self.host, ctx_next, CtxNextFn)
    }

    /// M2c：取消一次在飞派发（#19）。
    fn abort_fn(&self) -> Option<CtxAbortFn> {
        api!(self.host, ctx_abort, CtxAbortFn)
    }

    /// 流：往这次派发对应的流里推一块（#20）。
    fn stream_push_fn(&self) -> Option<CtxStreamPushFn> {
        api!(self.host, ctx_stream_push, CtxStreamPushFn)
    }

    /// 流：正常收尾（#21）。
    fn stream_close_fn(&self) -> Option<CtxStreamCloseFn> {
        api!(self.host, ctx_stream_close, CtxStreamCloseFn)
    }

    /// 流：失败收尾（#22）。
    fn stream_fail_fn(&self) -> Option<CtxStreamFailFn> {
        api!(self.host, ctx_stream_fail, CtxStreamFailFn)
    }

    /// 读服务。`None` 表示不存在或提供者未就绪——**不是错误**
    /// （对应 `ctx.get(name)` 的可选探测语义）。
    pub fn get(&self, name: &str) -> Option<Json> {
        let api = self.get_fn()?;
        let raw = self
            .invoke_read(|out| unsafe { api(self.handle, Blob::new(name.as_bytes()), out) })
            .ok()?;
        let parsed: Json = serde_json::from_slice(&raw).ok()?;
        if parsed
            .get("missing")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            return None;
        }
        Some(parsed.get("value").cloned().unwrap_or(Json::Null))
    }

    /// 读一个**必需**服务：未声明 `inject`、提供者未就绪、跨隔离域都会给出可读错误。
    ///
    /// 与 [`Ctx::get`] 的分工是"断言 vs 探测"：可选能力用 `get`（`None` = 不可用），
    /// 缺了就干不成事的依赖用 `require`——错在哪、为什么，都在消息里。
    /// 另外 `require` 读的是 committed 快照：**组件在自己的拆除过程中也能读到依赖**，
    /// 这对"卸载时要回调依赖做收尾"的插件是必需的。
    pub fn require(&self, name: &str) -> Result<Json, String> {
        self.require_detailed(name).map_err(|payload| payload.error)
    }

    /// [`Ctx::require`] 的详细版本：保留宿主返回的 `code/category/certainty/retry/detail`。
    #[allow(clippy::result_large_err)]
    pub fn require_detailed(&self, name: &str) -> Result<Json, crate::ErrorPayload> {
        let api = self.require_fn().ok_or_else(|| {
            protocol_error(
                "宿主不支持 ctx_require：请升级宿主，或改用 ctx.get 做可选探测",
                "host_unsupported",
            )
        })?;
        let raw = self.invoke_read_detail(|out| unsafe {
            api(self.handle, Blob::new(name.as_bytes()), out)
        })?;
        let parsed: Json = serde_json::from_slice(&raw).map_err(|err| {
            protocol_error(
                format!("宿主 ctx_require 回复不是合法 JSON: {err}"),
                "host_bad_reply",
            )
        })?;
        Ok(parsed.get("value").cloned().unwrap_or(Json::Null))
    }

    /// 覆盖**自己提供的**服务值（对应 JS 的 `ctx.set(name, value)`）。
    ///
    /// 两条硬约束与内核一致：名字没 `provide` 过 → `without provide`；
    /// 实现者不是自己 → `in multiple fibers`。换的是**值**，不是身份——
    /// 所以不会触发依赖者重载（上游语义同样如此）。
    pub fn set(&self, name: &str, value: Json) -> PluginResult {
        self.set_detailed(name, value)
            .map_err(|payload| payload.error)
    }

    /// [`Ctx::set`] 的详细版本：保留宿主返回的结构化错误。
    #[allow(clippy::result_large_err)]
    pub fn set_detailed(&self, name: &str, value: Json) -> Result<(), crate::ErrorPayload> {
        let api = self
            .set_fn()
            .ok_or_else(|| protocol_error("宿主不支持 ctx_set：请升级宿主", "host_unsupported"))?;
        let payload = value.to_string();
        self.invoke_detail(|out| unsafe {
            api(
                self.handle,
                Blob::new(name.as_bytes()),
                Blob::new(payload.as_bytes()),
                out,
            )
        })
        .map(|_| ())
    }

    /// 派生一个 realm 隔离的子 context（#12）。`key` 是隔离键，`label`
    /// 为空时是调用方私有域，非空时是具名共享域。返回**派生句柄**
    /// （`bit63=1`）：通过它做 `get`/`require` 读的是隔离域内的值；
    /// 随调用方实例注销一并回收。
    pub fn isolate(&self, key: &str, label: &str) -> Result<u64, String> {
        let api = self
            .isolate_fn()
            .ok_or("宿主不支持 ctx_isolate：请升级宿主，或改用宿主配置的 isolate 声明")?;
        let raw = self.invoke(|out| unsafe {
            api(
                self.handle,
                Blob::new(key.as_bytes()),
                Blob::new(label.as_bytes()),
                out,
            )
        })?;
        parse_handle(&raw, "isolate")
    }

    /// 派生一个 intercept 子 context（#13）。`metadata` 由宿主存住，
    /// 供子 context 读取；真正的拦截消费点在 M3。
    pub fn intercept(&self, key: &str, metadata: Json) -> Result<u64, String> {
        let api = self
            .intercept_fn()
            .ok_or("宿主不支持 ctx_intercept：请升级宿主，或改用宿主配置的 intercept 声明")?;
        let payload = metadata.to_string();
        let raw = self.invoke(|out| unsafe {
            api(
                self.handle,
                Blob::new(key.as_bytes()),
                Blob::new(payload.as_bytes()),
                out,
            )
        })?;
        parse_handle(&raw, "intercept")
    }

    /// 同 [`Ctx::intercept`]，但**把派生句柄包成一个可用的 `Ctx`**（M3）：
    /// 从这个 ctx 发起的服务调用会带上这条链上的元数据（提供者侧读到的
    /// `d(k) ⊕ ι(k)` 因此含它）。要回收句柄就用 [`Ctx::dispose`]。
    pub fn intercept_ctx(&self, key: &str, metadata: Json) -> Result<Ctx, String> {
        let handle = self.intercept(key, metadata)?;
        Ok(Ctx {
            host: self.host,
            handle,
        })
    }

    /// 挂一个子组件（#14）。`url` 交给宿主的 `PluginSource` 解析；
    /// `"@native:<name>"` 走宿主进程级 `NativeRegistry`。新 fiber 是
    /// 调用方 fiber 的一个 effect：**父 dispose ⇒ 子 dispose**。
    pub fn use_child(&self, url: &str, config: Json) -> Result<u64, String> {
        let api = self
            .use_fn()
            .ok_or("宿主不支持 ctx_use：请升级宿主，或改用宿主配置的子组件声明")?;
        let payload = config.to_string();
        let raw = self.invoke(|out| unsafe {
            api(
                self.handle,
                Blob::new(url.as_bytes()),
                Blob::new(payload.as_bytes()),
                out,
            )
        })?;
        parse_handle(&raw, "use")
    }

    /// 回收一个派生 ctx 句柄或子 fiber 句柄（#15）。子 fiber 会真的被
    /// dispose（走既有的 loader 行状态机制）；isolate/intercept 派生句柄
    /// 只是撤销。卸载自己请用 [`Ctx::dispose_self`]。
    pub fn dispose(&self, target: u64) -> PluginResult {
        let api = self
            .dispose_fn()
            .ok_or("宿主不支持 ctx_dispose：请升级宿主")?;
        let payload = serde_json::json!({ "handle": target }).to_string();
        self.invoke(|out| unsafe { api(self.handle, Blob::new(payload.as_bytes()), out) })
            .map(|_| ())
    }

    /// 卸载调用者自己（#16）：宿主把本插件所在 loader 行标记 disabled
    /// 并**持久化**——绝不会静默消失。
    pub fn dispose_self(&self) -> PluginResult {
        let api = self
            .dispose_self_fn()
            .ok_or("宿主不支持 ctx_dispose_self：请升级宿主")?;
        self.invoke(|out| unsafe { api(self.handle, out) })
            .map(|_| ())
    }

    /// 结算一个**挂起**的入站续延（#17，M2）：在监听器里返回
    /// [`AsyncReply::Pending`] 之后，用本次回调的 [`CallbackInput::dispatch_id`]
    /// 把返回值送回来。
    ///
    /// `value` = 这次回调的返回值本身（与同步路径 `{"ret": …}` 的内层同物）。
    /// 每次挂起**恰好结算一次**：重复结算是一条可读错误，绝不静默覆盖。
    pub fn fulfill(&self, dispatch_id: u64, value: Json) -> PluginResult {
        let api = self
            .fulfill_fn()
            .ok_or("宿主不支持 ctx_fulfill：请升级宿主")?;
        let payload = value.to_string();
        self.invoke(|out| unsafe {
            api(self.handle, dispatch_id, Blob::new(payload.as_bytes()), out)
        })
        .map(|_| ())
    }

    /// 往**这次派发对应的流**里推一块（#20，ADR 0001 §5）。
    ///
    /// `dispatch_id` 来自 [`Ctx::provide_iface_async`] 的方法体（第三个参数）或
    /// [`Ctx::on_async`] 的监听器（第二个参数）——同步的 `provide_iface` / `on`
    /// 拿不到它，因此**提供流的能力要求异步形态的方法体**。
    ///
    /// 窗口满时返回 [`StreamPush::Pending`]：**块没有被接受**，提供者要等
    /// `on_credit` 里给出的额度再重推这一块（额度一次一个位置）。
    /// 返回其它结局时 `on_credit` 不被使用——登记的那条回调会被撤销，不会漏。
    pub fn stream_push<F>(
        &self,
        dispatch_id: u64,
        chunk: Json,
        on_credit: F,
    ) -> Result<StreamPush, String>
    where
        F: FnOnce(StreamCredit) + Send + 'static,
    {
        let api = self
            .stream_push_fn()
            .ok_or("宿主不支持 ctx_stream_push：请升级宿主（流式是 ABI v2 的尾追加字段）")?;
        let continuation = registry().register_text_continuation_for(
            self.handle,
            Box::new(move |result| match result {
                Ok(payload) => {
                    let window = payload.get("window").and_then(Json::as_u64).unwrap_or(0) as usize;
                    on_credit(StreamCredit::Window(window));
                }
                // 宿主驳回（`args:[{"error": …}]`）：这条流已经不在了。
                Err(_) => on_credit(StreamCredit::Gone),
            }),
        );
        let payload = chunk.to_string();
        let (status, _) = self
            .invoke_statused(|out| unsafe {
                api(
                    self.handle,
                    dispatch_id,
                    Blob::new(payload.as_bytes()),
                    continuation,
                    out,
                )
            })
            .inspect_err(|_| {
                registry().forget(continuation);
            })?;
        match status {
            STATUS_OK => {
                registry().forget(continuation);
                Ok(StreamPush::Sent)
            }
            STATUS_PENDING => Ok(StreamPush::Pending),
            STATUS_STREAM_GONE => {
                registry().forget(continuation);
                Ok(StreamPush::Gone)
            }
            other => {
                registry().forget(continuation);
                Err(format!("ctx_stream_push 返回了意料之外的状态码 {other}"))
            }
        }
    }

    /// 正常收尾这条流（#21）：消费者读完已缓冲的块之后拿到 `Ok(None)`。
    ///
    /// **幂等**：流已经收场（或消费者已经离场）时同样返回 `Ok(())`——
    /// 提供者不需要先问一句"还在不在"。
    pub fn stream_close(&self, dispatch_id: u64) -> PluginResult {
        let api = self
            .stream_close_fn()
            .ok_or("宿主不支持 ctx_stream_close：请升级宿主")?;
        match self.invoke_statused_safe(|out| unsafe { api(self.handle, dispatch_id, out) }) {
            Ok((STATUS_OK, _)) | Ok((STATUS_STREAM_GONE, _)) => Ok(()),
            Ok((other, _)) => Err(format!("ctx_stream_close 返回了意料之外的状态码 {other}")),
            Err(message) => Err(message),
        }
    }

    /// 失败收尾这条流（#22）：消费者**立刻**读到 `reason`（缓冲丢弃）。
    ///
    /// 与 [`Ctx::stream_close`] 同样幂等。
    pub fn stream_fail(&self, dispatch_id: u64, reason: &str) -> PluginResult {
        let api = self
            .stream_fail_fn()
            .ok_or("宿主不支持 ctx_stream_fail：请升级宿主")?;
        match self.invoke_statused_safe(|out| unsafe {
            api(self.handle, dispatch_id, Blob::new(reason.as_bytes()), out)
        }) {
            Ok((STATUS_OK, _)) | Ok((STATUS_STREAM_GONE, _)) => Ok(()),
            Ok((other, _)) => Err(format!("ctx_stream_fail 返回了意料之外的状态码 {other}")),
            Err(message) => Err(message),
        }
    }

    /// 续跑 waterfall 的**剩余链**（#18，M2b）：在 `mode:"waterfall"` 的回调**内部**调用。
    ///
    /// 语义与 JS `await next(args)` 对应：`args` **替换**下游实参；下游完成后
    /// `on_done` 收到结果（`Ok(下游值)` / `Err(可读消息)`），随后用
    /// [`Ctx::fulfill`] 把**终值**送回派发方。宿主保证 `on_done` 恰好被调用一次
    /// （剩余链同步完成时就地调用，否则交给运行时）。
    pub fn next<F>(&self, dispatch_id: u64, args: Vec<Json>, on_done: F) -> PluginResult
    where
        F: FnOnce(Result<Json, String>) + Send + 'static,
    {
        let api = self.next_fn().ok_or("宿主不支持 ctx_next：请升级宿主")?;
        let cb = registry().register_text_continuation_for(self.handle, Box::new(on_done));
        let payload = serde_json::json!({ "args": args }).to_string();
        match self.invoke(|out| unsafe {
            api(
                self.handle,
                dispatch_id,
                Blob::new(payload.as_bytes()),
                cb,
                out,
            )
        }) {
            Ok(_) => Ok(()),
            Err(err) => {
                registry().forget(cb);
                Err(err)
            }
        }
    }

    /// [`Ctx::next`] 的详细版本：`on_done` 收到完整 `ErrorPayload`（旧字符串面保持
    /// 在 [`Ctx::next`]）。宿主必须带结构化兄弟字段；缺失时按 `unknown` 处理。
    #[allow(clippy::result_large_err)]
    pub fn next_detailed<F>(&self, dispatch_id: u64, args: Vec<Json>, on_done: F) -> PluginResult
    where
        F: FnOnce(Result<Json, crate::ErrorPayload>) + Send + 'static,
    {
        let api = self.next_fn().ok_or("宿主不支持 ctx_next：请升级宿主")?;
        let cb = registry().register_detailed_continuation_for(self.handle, Box::new(on_done));
        let payload = serde_json::json!({ "args": args }).to_string();
        match self.invoke(|out| unsafe {
            api(
                self.handle,
                dispatch_id,
                Blob::new(payload.as_bytes()),
                cb,
                out,
            )
        }) {
            Ok(_) => Ok(()),
            Err(err) => {
                registry().forget(cb);
                Err(err)
            }
        }
    }

    /// 取消一次在飞派发（#19，M2c）：该派发**及其后代**的挂起续延全部以「已取消」结算。
    ///
    /// `dispatch_id` 来自 [`CallbackInput::dispatch_id`]（取消自己正在处理的那次派发）
    /// 或 [`Ctx::dispatch_abortable`]（取消自己发起的那次派发）。
    pub fn abort(&self, dispatch_id: u64) -> PluginResult {
        let api = self.abort_fn().ok_or("宿主不支持 ctx_abort：请升级宿主")?;
        self.invoke(|out| unsafe { api(self.handle, dispatch_id, out) })
            .map(|_| ())
    }

    /// 用一份宿主函数表 + 实例句柄构造上下文（M2b）。
    ///
    /// 给**进程内的宿主/夹具/spike** 用：真实插件从入口拿到 `Ctx`（`apply` 的入参），
    /// 而"不经动态库的 ABI 形状插件"（`apply_via` + 手写 `CallFn`）需要一个构造点。
    /// 插件作者在真实插件里**不需要**它。
    pub fn from_host(host: &'static HostApi, handle: u64) -> Ctx {
        Ctx { host, handle }
    }

    /// 注册一个纯数据服务。
    pub fn provide(&self, name: &str, value: Json) -> PluginResult {
        let api = self.provide_fn().ok_or("宿主不支持 ctx_provide")?;
        let payload = value.to_string();
        self.invoke(|out| unsafe {
            api(
                self.handle,
                Blob::new(name.as_bytes()),
                Blob::new(payload.as_bytes()),
                out,
            )
        })
        .map(|_| ())
    }

    /// 注册一个**带方法的服务**。
    ///
    /// `version` 是接口版本。**接口只能追加方法**：这样用 1 版编译的调用方
    /// 在 2 版的提供方上照常工作，而用 2 版编译的调用方在 1 版提供方上会拿到
    /// 一条明确的版本不匹配错误（宿主侧拒绝），而不是调用一个不存在的方法。
    pub fn provide_iface(
        &self,
        name: &str,
        version: u32,
        methods: Vec<(&'static str, IfaceFn)>,
    ) -> PluginResult {
        let api = self
            .provide_iface_fn()
            .ok_or("宿主不支持 ctx_provide_iface：请升级宿主，或改用 ctx.provide 暴露纯数据")?;
        let names: Vec<Json> = methods
            .iter()
            .map(|(name, _)| Json::String((*name).into()))
            .collect();
        let table: HashMap<String, IfaceFn> = methods
            .into_iter()
            .map(|(name, f)| (name.to_string(), f))
            .collect();

        // 一个接口只占一个回调 id：宿主把方法名带进来，这里做二次分派。
        // 于是跨 ABI 的调用是"（服务名, 方法名, JSON）"三元组，
        // 而不是函数指针表——**方法名成为契约，指针不是**。
        let dispatcher: Handler = Arc::new(move |input: CallbackInput| {
            let method = input.method.clone().unwrap_or_default();
            let Some(handler) = table.get(&method) else {
                return Err(format!(
                    "service \"{}\" 没有方法 \"{}\"；可用: {:?}",
                    input.event,
                    method,
                    table.keys().collect::<Vec<_>>()
                ));
            };
            handler(input.arg(), input.meta.clone())
        });
        let cb = registry().register_handler_for(self.handle, dispatcher);
        let names_json = serde_json::to_vec(&names).unwrap_or_else(|_| b"[]".to_vec());
        match self.invoke(|out| unsafe {
            api(
                self.handle,
                Blob::new(name.as_bytes()),
                version,
                Blob::new(&names_json),
                cb,
                out,
            )
        }) {
            Ok(_) => Ok(()),
            Err(err) => {
                registry().forget(cb);
                Err(err)
            }
        }
    }

    /// 注册一个**带方法的服务**，且方法体**可以挂起**（ADR 0001 §4）。
    ///
    /// 与 [`Ctx::provide_iface`] 逐条同构（同一个宿主 op、同一条线上形状：接口只占一个
    /// 回调 id、方法名是契约），区别只在方法体的形态：
    ///
    /// ```ignore
    /// ctx.provide_iface_async("llm", 1, vec![(
    ///     "generate",
    ///     Arc::new(|args: Json, _meta: Option<Json>, dispatch_id: u64| {
    ///         // 记下 dispatch_id（比如塞进请求队列），稍后：
    ///         //   ctx.fulfill(dispatch_id, answer)
    ///         Ok(sdk::AsyncReply::Pending)
    ///     }),
    /// )])?;
    /// ```
    ///
    /// 于是 harness 的模型适配器 / 工具 / 会话日志都能写成**服务**（它们天然是异步的），
    /// 而不必为了挂起能力改写成事件监听器。返回 [`AsyncReply::Reply`] 就是"同步给答案"，
    /// 两条路径共用同一条通道，调用方不需要区分。
    pub fn provide_iface_async(
        &self,
        name: &str,
        version: u32,
        methods: Vec<(&'static str, AsyncIfaceFn)>,
    ) -> PluginResult {
        let api = self
            .provide_iface_fn()
            .ok_or("宿主不支持 ctx_provide_iface：请升级宿主，或改用 ctx.provide 暴露纯数据")?;
        let names: Vec<Json> = methods
            .iter()
            .map(|(name, _)| Json::String((*name).into()))
            .collect();
        let table: HashMap<String, AsyncIfaceFn> = methods
            .into_iter()
            .map(|(name, f)| (name.to_string(), f))
            .collect();
        let cb = registry().register_async_iface_for(self.handle, table);
        let names_json = serde_json::to_vec(&names).unwrap_or_else(|_| b"[]".to_vec());
        match self.invoke(|out| unsafe {
            api(
                self.handle,
                Blob::new(name.as_bytes()),
                version,
                Blob::new(&names_json),
                cb,
                out,
            )
        }) {
            Ok(_) => Ok(()),
            Err(err) => {
                registry().forget(cb);
                Err(err)
            }
        }
    }

    /// 调用另一个插件的服务方法。结果通过 `on_done` 返回。
    ///
    /// **宿主可能内联回调**（调用还没返回 `on_done` 就已经跑完了），
    /// 也可能稍后回调。调用方不需要区分这两种情况——这正是把
    /// "同步快速路径 + 异步兜底"藏在宿主里的好处。
    pub fn call_iface<F>(
        &self,
        service: &str,
        version: u32,
        method: &str,
        args: Json,
        on_done: F,
    ) -> PluginResult
    where
        F: FnOnce(Result<Json, String>) + Send + 'static,
    {
        let api = self.call_iface_fn().ok_or("宿主不支持 ctx_call_iface")?;
        let cb = registry().register_text_continuation_for(self.handle, Box::new(on_done));
        let payload = args.to_string();
        let response = match self.invoke(|out| unsafe {
            api(
                self.handle,
                Blob::new(service.as_bytes()),
                version,
                Blob::new(method.as_bytes()),
                Blob::new(payload.as_bytes()),
                cb,
                out,
            )
        }) {
            Ok(response) => response,
            Err(err) => {
                // 调用没起来：已经登记的续延必须撤销，否则永远等不到 on_done。
                registry().forget(cb);
                return Err(err);
            }
        };
        settle_inline(cb, &response);
        Ok(())
    }

    /// [`Ctx::call_iface`] 的详细版本：`on_done` 收到完整 `ErrorPayload`，旧字符串
    /// 面保持 [`Ctx::call_iface`]。宿主会在 `args[0]` 里同时保留 `error` 字符串与
    /// `code/category/certainty/retry/detail` 兄弟字段。
    #[allow(clippy::result_large_err)]
    pub fn call_iface_detailed<F>(
        &self,
        service: &str,
        version: u32,
        method: &str,
        args: Json,
        on_done: F,
    ) -> PluginResult
    where
        F: FnOnce(Result<Json, crate::ErrorPayload>) + Send + 'static,
    {
        let api = self.call_iface_fn().ok_or("宿主不支持 ctx_call_iface")?;
        let cb = registry().register_detailed_continuation_for(self.handle, Box::new(on_done));
        let payload = args.to_string();
        let response = match self.invoke(|out| unsafe {
            api(
                self.handle,
                Blob::new(service.as_bytes()),
                version,
                Blob::new(method.as_bytes()),
                Blob::new(payload.as_bytes()),
                cb,
                out,
            )
        }) {
            Ok(response) => response,
            Err(err) => {
                registry().forget(cb);
                return Err(err);
            }
        };
        settle_inline(cb, &response);
        Ok(())
    }

    /// 派发一个有返回值的事件（`mode` 取 `"bail"` / `"serial"` / `"parallel"`）。
    /// 结果通过 `on_done` 返回，语义同 [`Ctx::call_iface`]。
    pub fn dispatch<F>(&self, event: &str, mode: &str, args: Vec<Json>, on_done: F) -> PluginResult
    where
        F: FnOnce(Result<Json, String>) + Send + 'static,
    {
        // 普通 dispatch 不要求 `dispatch_id`：v1 宿主没有这个字段，
        // 但派发本身仍然成功，不能把成功报成失败。
        self.dispatch_inner(event, mode, args, on_done, false)
            .map(|_| ())
    }

    /// 派发一个**可取消**的有返回值事件（M2c）：与 [`Ctx::dispatch`] 同义，
    /// 但把这次派发的 `dispatch_id` 还给调用方——[`Ctx::abort`] 用它撤销
    /// （连带撤销它的后代）。
    pub fn dispatch_abortable<F>(
        &self,
        event: &str,
        mode: &str,
        args: Vec<Json>,
        on_done: F,
    ) -> Result<u64, String>
    where
        F: FnOnce(Result<Json, String>) + Send + 'static,
    {
        self.dispatch_inner(event, mode, args, on_done, true)?
            .ok_or_else(|| {
                "宿主没有返回 dispatch_id（它是为更老的宿主编译的）：`ctx.abort` 不可用".to_string()
            })
    }

    fn dispatch_inner<F>(
        &self,
        event: &str,
        mode: &str,
        args: Vec<Json>,
        on_done: F,
        require_id: bool,
    ) -> Result<Option<u64>, String>
    where
        F: FnOnce(Result<Json, String>) + Send + 'static,
    {
        let api = self.dispatch_fn().ok_or("宿主不支持 ctx_dispatch")?;
        let cb = registry().register_text_continuation_for(self.handle, Box::new(on_done));
        let payload = serde_json::json!({ "mode": mode, "args": args }).to_string();
        let response = match self.invoke(|out| unsafe {
            api(
                self.handle,
                Blob::new(event.as_bytes()),
                Blob::new(payload.as_bytes()),
                cb,
                out,
            )
        }) {
            Ok(response) => response,
            Err(err) => {
                registry().forget(cb);
                return Err(err);
            }
        };
        let parsed: Json = match serde_json::from_slice::<Json>(&response) {
            Ok(parsed) if parsed.is_object() => parsed,
            Ok(_) => {
                registry().fail(cb, "宿主返回的内联响应不是 JSON 对象".to_string());
                return Err("宿主返回的内联响应不是 JSON 对象".to_string());
            }
            Err(err) => {
                // 派发已经发生；续延必须收到错误，不能只 `forget` 后让 `on_done`
                // 永远不调用。`settle_inline` 也是同一条失败路径。
                registry().fail(cb, format!("宿主返回的内联响应不是合法 JSON: {err}"));
                return Err(err.to_string());
            }
        };
        // 先结算内联回调，再判断 dispatch_id：v1 宿主没有 id，但派发已经成功。
        settle_inline(cb, &response);
        let id = parsed.get("dispatch_id").and_then(Json::as_u64);
        if require_id && id.is_none() {
            return Err(
                "宿主没有返回 dispatch_id（它是为更老宿主编译的）：`ctx.abort` 不可用".to_string(),
            );
        }
        Ok(id)
    }

    /// [`Ctx::dispatch`] 的详细版本：`on_done` 收到完整 `ErrorPayload`。
    /// 普通 dispatch 不要求 `dispatch_id`，与旧面一致。
    #[allow(clippy::result_large_err)]
    pub fn dispatch_detailed<F>(
        &self,
        event: &str,
        mode: &str,
        args: Vec<Json>,
        on_done: F,
    ) -> PluginResult
    where
        F: FnOnce(Result<Json, crate::ErrorPayload>) + Send + 'static,
    {
        self.dispatch_inner_detailed(event, mode, args, on_done, false)
            .map(|_| ())
    }

    fn dispatch_inner_detailed<F>(
        &self,
        event: &str,
        mode: &str,
        args: Vec<Json>,
        on_done: F,
        require_id: bool,
    ) -> Result<Option<u64>, String>
    where
        F: FnOnce(Result<Json, crate::ErrorPayload>) + Send + 'static,
    {
        let api = self.dispatch_fn().ok_or("宿主不支持 ctx_dispatch")?;
        let cb = registry().register_detailed_continuation_for(self.handle, Box::new(on_done));
        let payload = serde_json::json!({ "mode": mode, "args": args }).to_string();
        let response = match self.invoke(|out| unsafe {
            api(
                self.handle,
                Blob::new(event.as_bytes()),
                Blob::new(payload.as_bytes()),
                cb,
                out,
            )
        }) {
            Ok(response) => response,
            Err(err) => {
                registry().forget(cb);
                return Err(err);
            }
        };
        let parsed: Json = match serde_json::from_slice::<Json>(&response) {
            Ok(parsed) if parsed.is_object() => parsed,
            Ok(_) => {
                registry().fail(cb, "宿主返回的内联响应不是 JSON 对象".to_string());
                return Err("宿主返回的内联响应不是 JSON 对象".to_string());
            }
            Err(err) => {
                registry().fail(cb, format!("宿主返回的内联响应不是合法 JSON: {err}"));
                return Err(err.to_string());
            }
        };
        settle_inline(cb, &response);
        let id = parsed.get("dispatch_id").and_then(Json::as_u64);
        if require_id && id.is_none() {
            return Err(
                "宿主没有返回 dispatch_id（它是为更老宿主编译的）：`ctx.abort` 不可用".to_string(),
            );
        }
        Ok(id)
    }

    /// 注册事件监听器。监听器由宿主在派发时**同步**回调进来。
    pub fn on<F>(&self, event: &str, handler: F) -> PluginResult
    where
        F: Fn(Vec<Json>) -> Result<Json, String> + Send + Sync + 'static,
    {
        let api = self.on_fn().ok_or("宿主不支持 ctx_on")?;
        let dispatcher: Handler = Arc::new(move |input: CallbackInput| handler(input.args));
        let cb = registry().register_handler_for(self.handle, dispatcher);
        self.emit_raw(api, event, cb)
    }

    /// 注册一个**可以挂起**的事件监听器（M2，D3）。
    ///
    /// 与 [`Ctx::on`] 的唯一区别：监听器可以返回 [`AsyncReply::Pending`] 表示
    /// "结果稍后经 [`Ctx::fulfill`] 结算"——异步的存在感只跨越**这一次回调**。
    /// 宿主侧行为：同步返回 ⇒ 与 `on` 逐字相同；挂起 ⇒ 宿主 await 结算后再继续。
    pub fn on_async<F>(&self, event: &str, handler: F) -> PluginResult
    where
        F: Fn(Vec<Json>, u64) -> Result<AsyncReply, String> + Send + Sync + 'static,
    {
        let api = self.on_fn().ok_or("宿主不支持 ctx_on")?;
        let dispatcher: AsyncHandler = Arc::new(handler);
        let cb = registry().register_async_handler_for(self.handle, dispatcher);
        self.emit_raw(api, event, cb)
    }

    /// 只管发不管收（`emit` 在 cordis 里本来就不 await、不取返回值）。
    pub fn emit(&self, event: &str, args: Vec<Json>) -> PluginResult {
        let api = self.emit_fn().ok_or("宿主不支持 ctx_emit")?;
        let payload = serde_json::json!({ "mode": "emit", "args": args }).to_string();
        self.invoke(|out| unsafe {
            api(
                self.handle,
                Blob::new(event.as_bytes()),
                Blob::new(payload.as_bytes()),
                0,
                out,
            )
        })
        .map(|_| ())
    }

    /// 注册一个"卸载时回调"的清理动作。
    pub fn effect<F>(&self, cleanup: F) -> PluginResult
    where
        F: FnOnce() + Send + 'static,
    {
        let api = self.effect_fn().ok_or("宿主不支持 ctx_effect")?;
        let id = registry().register_cleanup_for(self.handle, Box::new(cleanup));
        match self.invoke(|out| unsafe { api(self.handle, id, out) }) {
            Ok(_) => Ok(()),
            Err(err) => {
                // 注册没落地 ⇒ 清理闭包永远不会被宿主调用，必须现在就撤销。
                registry().forget(id);
                Err(err)
            }
        }
    }

    pub fn log(&self, level: u32, message: &str) {
        if let Some(api) = self.log_fn() {
            unsafe {
                api(self.handle, level, Blob::new(message.as_bytes()));
            }
        }
    }

    fn emit_raw(&self, api: CtxOnFn, event: &str, cb: u64) -> PluginResult {
        match self.invoke(|out| unsafe { api(self.handle, Blob::new(event.as_bytes()), cb, out) }) {
            Ok(_) => Ok(()),
            Err(err) => {
                // 宿主拒绝注册（实例已卸载 / 已关闭）时，刚放进全局回调表的
                // 闭包必须一起撤销；否则失效实例再也不会收到 OP_DISPOSE，
                // 这些闭包捕获的对象就永远回收不了。
                registry().forget(cb);
                Err(err)
            }
        }
    }

    /// 所有宿主调用的统一通道：处理 `TooSmall` 重试与错误解码。
    ///
    /// 这是"全部内存归宿主所有"的红利——**跨边界零分配**，插件侧不需要任何
    /// ownership 记账，也不存在跨分配器释放的崩溃面。
    fn invoke<F>(&self, call: F) -> Result<Vec<u8>, String>
    where
        F: Fn(*mut Out) -> i32,
    {
        // 默认不重试：写操作已经在宿主侧执行，重放会重复副作用。
        self.invoke_with(Retry::Never, call)
    }

    /// 只读/幂等操作的调用通道：允许在 `STATUS_TOO_SMALL` 时扩容重试。
    fn invoke_read<F>(&self, call: F) -> Result<Vec<u8>, String>
    where
        F: Fn(*mut Out) -> i32,
    {
        self.invoke_with(Retry::Safe, call)
    }

    #[allow(clippy::result_large_err)]
    fn invoke_detail<F>(&self, call: F) -> Result<Vec<u8>, crate::ErrorPayload>
    where
        F: Fn(*mut Out) -> i32,
    {
        self.invoke_with_detail(Retry::Never, call)
    }

    #[allow(clippy::result_large_err)]
    fn invoke_read_detail<F>(&self, call: F) -> Result<Vec<u8>, crate::ErrorPayload>
    where
        F: Fn(*mut Out) -> i32,
    {
        self.invoke_with_detail(Retry::Safe, call)
    }

    fn invoke_with<F>(&self, retry: Retry, call: F) -> Result<Vec<u8>, String>
    where
        F: Fn(*mut Out) -> i32,
    {
        self.invoke_with_detail(retry, call)
            .map_err(|payload| payload.error)
    }

    /// 保留完整 `ErrorPayload` 的调用通道；旧 `invoke_with` 只是它的 `.error` 兼容边界。
    #[allow(clippy::result_large_err)]
    fn invoke_with_detail<F>(&self, retry: Retry, call: F) -> Result<Vec<u8>, crate::ErrorPayload>
    where
        F: Fn(*mut Out) -> i32,
    {
        // 16 KiB 起步，尽量让大返回值走正常路径；超过时写操作宁可报错也不重放。
        let mut cap = 16 * 1024usize;
        for _ in 0..8 {
            let mut buffer = vec![0u8; cap];
            let mut out = Out {
                ptr: buffer.as_mut_ptr(),
                cap,
                len: 0,
            };
            let status = call(&mut out);
            match status {
                STATUS_OK => {
                    if out.len > buffer.len() {
                        return Err(protocol_error(
                            format!(
                                "宿主响应声称 {} 字节，超过插件缓冲 {cap} 字节；消息未完整写入",
                                out.len
                            ),
                            "host_protocol",
                        ));
                    }
                    buffer.truncate(out.len);
                    return Ok(buffer);
                }
                STATUS_TOO_SMALL => {
                    if retry == Retry::Never {
                        return Err(protocol_error(
                            format!(
                                "宿主响应需要 {} 字节，超过初始缓冲 {cap}；该操作不可重试（重放会重复执行副作用）",
                                out.len
                            ),
                            "host_too_small",
                        ));
                    }
                    cap = out.len.max(cap * 2);
                    continue;
                }
                STATUS_ERR => {
                    if out.len > buffer.len() {
                        return Err(protocol_error(
                            format!(
                                "宿主响应声称 {} 字节，超过插件缓冲 {cap} 字节；消息未完整写入",
                                out.len
                            ),
                            "host_protocol",
                        ));
                    }
                    buffer.truncate(out.len);
                    return Err(decode_host_error(self.host, &buffer));
                }
                STATUS_UNSUPPORTED => {
                    return Err(protocol_error(
                        "宿主不支持这个操作（版本过旧？）",
                        "host_unsupported",
                    ))
                }
                other => {
                    return Err(protocol_error(
                        format!("宿主返回了意料之外的状态码 {other}"),
                        "host_bad_status",
                    ))
                }
            }
        }
        Err(protocol_error(
            "响应在多次重试后仍然放不下",
            "host_bad_reply",
        ))
    }

    /// [`Ctx::invoke`] 的"状态码可判别"版本：`STATUS_OK` / `STATUS_PENDING` /
    /// `STATUS_STREAM_GONE` 这些**不是错误**的结局原样交给调用方决定，
    /// 只有 `STATUS_ERR` 与 `STATUS_UNSUPPORTED` 变成可读错误。
    ///
    /// 为什么需要它：流式推送的"窗口满了"是一个**正常结局**，把它压成错误
    /// 会让插件无法区分"没推成但流还活着"与"流没了"（两者要采取不同动作）。
    fn invoke_statused<F>(&self, call: F) -> Result<(i32, Vec<u8>), String>
    where
        F: Fn(*mut Out) -> i32,
    {
        self.invoke_statused_with(Retry::Never, call)
    }

    /// 幂等状态码通道（`stream_close`/`stream_fail`）：允许扩容重试。
    fn invoke_statused_safe<F>(&self, call: F) -> Result<(i32, Vec<u8>), String>
    where
        F: Fn(*mut Out) -> i32,
    {
        self.invoke_statused_with(Retry::Safe, call)
    }

    fn invoke_statused_with<F>(&self, retry: Retry, call: F) -> Result<(i32, Vec<u8>), String>
    where
        F: Fn(*mut Out) -> i32,
    {
        let mut cap = 16 * 1024usize;
        for _ in 0..8 {
            let mut buffer = vec![0u8; cap];
            let mut out = Out {
                ptr: buffer.as_mut_ptr(),
                cap,
                len: 0,
            };
            let status = call(&mut out);
            match status {
                STATUS_TOO_SMALL => {
                    if retry == Retry::Never {
                        return Err(format!(
                            "宿主响应需要 {} 字节，超过初始缓冲 {cap}；该操作不可重试（重放会重复执行副作用）",
                            out.len
                        ));
                    }
                    cap = out.len.max(cap * 2);
                    continue;
                }
                STATUS_ERR => {
                    if out.len > buffer.len() {
                        return Err(protocol_error(
                            format!(
                                "宿主响应声称 {} 字节，超过插件缓冲 {cap} 字节；消息未完整写入",
                                out.len
                            ),
                            "host_protocol",
                        )
                        .error);
                    }
                    buffer.truncate(out.len);
                    return Err(decode_host_error(self.host, &buffer).error);
                }
                STATUS_UNSUPPORTED => return Err("宿主不支持这个操作（版本过旧？）".into()),
                other => {
                    if out.len > buffer.len() {
                        return Err(protocol_error(
                            format!(
                                "宿主响应声称 {} 字节，超过插件缓冲 {cap} 字节；消息未完整写入",
                                out.len
                            ),
                            "host_protocol",
                        )
                        .error);
                    }
                    buffer.truncate(out.len);
                    return Ok((other, buffer));
                }
            }
        }
        Err("响应在多次重试后仍然放不下".into())
    }
}

// ------------------------------------------------------------------ 回调表

/// 三种回调分层存放，语义各不相同：
/// - `handlers`：**长期有效**（事件监听、接口方法分发），随插件卸载而失效；
/// - `continuations`：**一次性**（`call_iface` / `dispatch` 的结果回调）；
/// - `cleanups`：**一次性**，卸载时执行。
///
/// 把续延做成一次性，是对"宿主可能内联回调也可能稍后回调"这一事实的直接编码：
/// 无论走哪条路径，续延都恰好被执行一次——**不需要调用方自己去重**。
#[derive(Default)]
struct Registry {
    handlers: Mutex<HashMap<u64, Handler>>,
    /// **可挂起**的监听器（M2）：与 `handlers` 同 id 空间，分表存放。
    async_handlers: Mutex<HashMap<u64, AsyncHandler>>,
    /// **可挂起的接口方法**（ADR 0001 §4）：按"方法名 → 方法体"分表，同样按 id 索引。
    ///
    /// 与 `handlers` 的区别只在"能不能挂起"；与 `async_handlers` 的区别在
    /// `AsyncHandler` 只拿到 `(args, dispatch_id)`，而这里要按 `method` 二次分派
    /// （方法名是契约，见 `provide_iface` 的说明）。
    async_ifaces: Mutex<HashMap<u64, HashMap<String, AsyncIfaceFn>>>,
    continuations: Mutex<HashMap<u64, Continuation>>,
    cleanups: Mutex<HashMap<u64, Cleanup>>,
    /// 回调 id → 注册它的实例句柄。`OP_DISPOSE` 只清这个实例的登记，
    /// 同一个 cdylib 挂多个实例时不会互相拆台。
    owners: Mutex<HashMap<u64, u64>>,
    next: AtomicU64,
}

/// 回调的返回形态（内部）：普通值，或"挂起，稍后经 `ctx_fulfill` 结算"。
enum Dispatched {
    Value(Json),
    Pending,
}

type TextContinuation = Box<dyn FnOnce(Result<Json, String>) + Send>;
type DetailedContinuation = Box<dyn FnOnce(Result<Json, crate::ErrorPayload>) + Send>;

/// 续延的两种读取面：旧字符串面保持原签名，详细面接收完整 [`crate::ErrorPayload`]。
/// 两种都只在宿主交付时消费一次；旧面只取 `.error`，不改变调用结果。
enum Continuation {
    Text(TextContinuation),
    Detailed(DetailedContinuation),
}

impl Registry {
    fn next_id(&self) -> u64 {
        self.next.fetch_add(1, Ordering::SeqCst) + 1
    }

    fn remember(&self, id: u64, owner: u64) {
        self.owners.lock().unwrap().insert(id, owner);
    }

    fn register_handler_for(&self, owner: u64, handler: Handler) -> u64 {
        let id = self.next_id();
        self.handlers.lock().unwrap().insert(id, handler);
        self.remember(id, owner);
        id
    }

    fn register_async_handler_for(&self, owner: u64, handler: AsyncHandler) -> u64 {
        let id = self.next_id();
        self.async_handlers.lock().unwrap().insert(id, handler);
        self.remember(id, owner);
        id
    }

    fn register_async_iface_for(&self, owner: u64, table: HashMap<String, AsyncIfaceFn>) -> u64 {
        let id = self.next_id();
        self.async_ifaces.lock().unwrap().insert(id, table);
        self.remember(id, owner);
        id
    }

    fn register_continuation_for(&self, owner: u64, continuation: Continuation) -> u64 {
        let id = self.next_id();
        self.continuations.lock().unwrap().insert(id, continuation);
        self.remember(id, owner);
        id
    }

    fn register_text_continuation_for(&self, owner: u64, continuation: TextContinuation) -> u64 {
        self.register_continuation_for(owner, Continuation::Text(continuation))
    }

    fn register_detailed_continuation_for(
        &self,
        owner: u64,
        continuation: DetailedContinuation,
    ) -> u64 {
        self.register_continuation_for(owner, Continuation::Detailed(continuation))
    }

    fn register_cleanup_for(&self, owner: u64, cleanup: Cleanup) -> u64 {
        let id = self.next_id();
        self.cleanups.lock().unwrap().insert(id, cleanup);
        self.remember(id, owner);
        id
    }

    /// 清掉某个实例注册的全部回调（`OP_DISPOSE` 的收尾）。
    fn dispose_owner(&self, owner: u64) {
        let ids: Vec<u64> = self
            .owners
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, registered)| **registered == owner)
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            self.forget(id);
        }
    }

    /// 先查一次性续延，再查长期 handler。取出来后**立即放锁再执行**，
    /// 因此回调里可以安全地再注册 / 再派发（宿主会重入到这里）。
    fn dispatch(&self, id: u64, input: CallbackInput) -> Result<Dispatched, String> {
        // 作用域守卫必须覆盖整个回调（包括一次性续延）：`volatile(path)` 靠它找到当前实例。
        let owner = self.owners.lock().unwrap().get(&id).copied();
        let _owner = VolatileOwner::enter(owner);
        // 续延路径：宿主异步完成时回调同一个 id，载荷里带 `result` 或 `error`。
        if self.continuations.lock().unwrap().contains_key(&id) {
            self.complete(id, &input.args.first().cloned().unwrap_or(Json::Null));
            return Ok(Dispatched::Value(Json::Null));
        }
        let handler = { self.handlers.lock().unwrap().get(&id).cloned() };
        if let Some(handler) = handler {
            return handler(input).map(Dispatched::Value);
        }
        // **可挂起的接口方法**（ADR 0001 §4）：接口只占一个回调 id，这里按方法名二次分派。
        let table = { self.async_ifaces.lock().unwrap().get(&id).cloned() };
        if let Some(table) = table {
            let method = input.method.clone().unwrap_or_default();
            let Some(handler) = table.get(&method).cloned() else {
                return Err(format!(
                    "service \"{}\" 没有方法 \"{}\"；可用: {:?}",
                    input.event,
                    method,
                    table.keys().collect::<Vec<_>>()
                ));
            };
            let dispatch_id = input.dispatch_id;
            return handler(input.arg(), input.meta.clone(), dispatch_id).map(
                |reply| match reply {
                    AsyncReply::Reply(value) => Dispatched::Value(value),
                    AsyncReply::Pending => Dispatched::Pending,
                },
            );
        }
        // **可挂起**的监听器（M2）：`Pending` 会变成线上状态码 `STATUS_PENDING`。
        let handler = { self.async_handlers.lock().unwrap().get(&id).cloned() };
        if let Some(handler) = handler {
            let dispatch_id = input.dispatch_id;
            return handler(input.args, dispatch_id).map(|reply| match reply {
                AsyncReply::Reply(value) => Dispatched::Value(value),
                AsyncReply::Pending => Dispatched::Pending,
            });
        }
        Err(format!("未知的回调 id {id}"))
    }

    /// 消费一个续延。**取出即删**，所以内联路径与兜底路径合计恰好执行一次。
    fn complete(&self, id: u64, payload: &Json) {
        let result = if !payload.is_object() {
            Err(crate::ErrorPayload::new("宿主回调载荷不是 JSON 对象"))
        } else {
            match payload.get("error") {
                Some(Json::String(message)) => {
                    Err(crate::ErrorPayload::from_json(&payload.to_string())
                        .unwrap_or_else(|| crate::ErrorPayload::new(message.clone())))
                }
                Some(_) => Err(crate::ErrorPayload::new(
                    "宿主错误载荷里的 error 字段不是字符串",
                )),
                None => Ok(payload.get("result").cloned().unwrap_or(Json::Null)),
            }
        };
        self.finish(id, result);
    }

    /// 把一个续延结算成失败；用于宿主返回了无法解释的成功信封时，绝不降级成
    /// `Ok(null)`。
    fn fail(&self, id: u64, message: String) {
        self.finish(id, Err(crate::ErrorPayload::new(message)));
    }

    fn finish(&self, id: u64, result: Result<Json, crate::ErrorPayload>) {
        let Some((continuation, owner)) = self.take_continuation(id) else {
            return;
        };
        let _owner = VolatileOwner::enter(owner);
        match continuation {
            Continuation::Text(callback) => callback(result.map_err(|payload| payload.error)),
            Continuation::Detailed(callback) => callback(result),
        }
    }

    fn take_continuation(&self, id: u64) -> Option<(Continuation, Option<u64>)> {
        let continuation = self.continuations.lock().unwrap().remove(&id);
        let owner = self.owners.lock().unwrap().remove(&id);
        continuation.map(|continuation| (continuation, owner))
    }

    /// 撤销一个**从未被使用**的续延（流式推送在窗口内成功时，那次登记的回调
    /// 不会再被触发）。取出即删，重复撤销无害。
    fn forget(&self, id: u64) {
        // id 空间全局唯一；把可能落在任一张表里的登记一起清掉。
        //
        // 注意**先取出、后析构**：回调闭包捕获的对象可能在自己的 `Drop` 里
        // 重新进入 SDK（注销其它回调、注册新监听器）。如果在 `MutexGuard`
        // 存活期间丢弃 `Handler`/`Continuation`，析构就会在同一张表上重入死锁。
        // 绑到局部变量后，所有锁都在语句结束时释放，真正的析构发生在函数末尾。
        let handler = self.handlers.lock().unwrap().remove(&id);
        let async_handler = self.async_handlers.lock().unwrap().remove(&id);
        let async_iface = self.async_ifaces.lock().unwrap().remove(&id);
        let continuation = self.continuations.lock().unwrap().remove(&id);
        let cleanup = self.cleanups.lock().unwrap().remove(&id);
        let owner = self.owners.lock().unwrap().remove(&id);
        drop((
            handler,
            async_handler,
            async_iface,
            continuation,
            cleanup,
            owner,
        ));
    }

    fn run_cleanup(&self, id: u64) {
        let cleanup = self.cleanups.lock().unwrap().remove(&id);
        let owner = self.owners.lock().unwrap().remove(&id);
        if let Some(cleanup) = cleanup {
            let _owner = VolatileOwner::enter(owner);
            cleanup();
        }
    }
}

/// 内联路径：宿主已经把结果带回来了，插件自己把续延消费掉。
///
/// `pending: true` 表示结果会通过 `OP_CALLBACK` 送到同一个续延 id 上，
/// 此时**什么也不做**——这正是"插件不需要区分两条路径"的落点。
fn settle_inline(continuation: u64, response: &[u8]) {
    let parsed: Json = match serde_json::from_slice::<Json>(response) {
        Ok(value) if value.is_object() => value,
        Ok(_) => {
            registry().fail(continuation, "宿主返回的内联响应不是 JSON 对象".to_string());
            return;
        }
        Err(err) => {
            registry().fail(
                continuation,
                format!("宿主返回的内联响应不是合法 JSON: {err}"),
            );
            return;
        }
    };
    match parsed.get("pending") {
        Some(value) if value.as_bool() == Some(true) => return,
        Some(value) if value.as_bool() == Some(false) => {}
        Some(_) => {
            registry().fail(
                continuation,
                "宿主内联响应的 pending 字段不是布尔值".to_string(),
            );
            return;
        }
        None => {}
    }
    registry().complete(continuation, &parsed);
}

/// 从宿主的 `{"handle": <u64>}` 响应里解出派生句柄。宿主侧的布局
/// （bit63=1）是宿主的实现细节，插件侧只把它当**不透明 u64** 记账。
fn parse_handle(raw: &[u8], op: &str) -> Result<u64, String> {
    let parsed: Json = serde_json::from_slice(raw).map_err(|err| err.to_string())?;
    parsed
        .get("handle")
        .and_then(Json::as_u64)
        .ok_or_else(|| format!("{op} 的宿主响应里没有 handle 字段"))
}

fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(Registry::default)
}

/// 宿主函数表的**自有副本**（copy-on-attach，G15）。一个 cdylib 一份（即"一个插件一份"）。
static HOST: OnceLock<HostApi> = OnceLock::new();
static APPLY: OnceLock<&'static ApplyFn> = OnceLock::new();

/// 这个插件声明的 volatile 路径（编译期给定，见 [`build_plugin_with_volatile`]）。
static VOLATILE_PATHS: OnceLock<&'static [&'static str]> = OnceLock::new();
/// volatile 值的**当前副本**（M4-5）：与原生插件的 `ctx.volatile(path)` 对应——
/// 插件随取随读，不自己缓存；宿主通过 `OP_VOLATILE_APPLY` 原地更新它。
///
/// 键是实例句柄：同一个 cdylib 可以挂多个实例，每个实例必须读到自己那份配置。
static VOLATILE_VALUES: OnceLock<Mutex<HashMap<u64, HashMap<String, Json>>>> = OnceLock::new();

thread_local! {
    /// 当前线程正在为哪个实例执行插件代码。`volatile(path)` 没有实例参数，
    /// 只能靠这个作用域把"实例 1 的回调"和"实例 2 的回调"分开。
    static VOLATILE_OWNER: RefCell<Option<u64>> = const { RefCell::new(None) };
}

/// 进入一个实例的插件代码作用域；离开时恢复外层实例（回调可以重入）。
struct VolatileOwner(Option<u64>);

impl VolatileOwner {
    fn enter(owner: Option<u64>) -> Self {
        let previous = VOLATILE_OWNER.with(|current| current.replace(owner));
        Self(previous)
    }
}

impl Drop for VolatileOwner {
    fn drop(&mut self) {
        VOLATILE_OWNER.with(|current| *current.borrow_mut() = self.0);
    }
}

fn volatile_values() -> &'static Mutex<HashMap<u64, HashMap<String, Json>>> {
    VOLATILE_VALUES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 读一个声明的 volatile 值（M4-5）。
///
/// 与原生插件的 `ctx.volatile("data")` 同一个语义：**路径声明过就有值**，
/// 值会随 volatile-only 的配置更新在**不重挂**的前提下变化。
/// 没声明过 / 值还没到 / 不在该实例的插件代码里 ⇒ `None`（不猜、不给默认值）。
pub fn volatile(path: &str) -> Option<Json> {
    let owner = VOLATILE_OWNER.with(|current| *current.borrow())?;
    volatile_values()
        .lock()
        .unwrap()
        .get(&owner)?
        .get(path)
        .cloned()
}

/// 用**初始**配置播种某个实例的 volatile 值（`OP_APPLY` 里调用）。
fn seed_volatile(owner: u64, config: &Json) {
    let mut store = volatile_values().lock().unwrap();
    let values = store.entry(owner).or_default();
    values.clear();
    for path in VOLATILE_PATHS.get().copied().unwrap_or(&[]) {
        if let Some(value) = path_value(config, path) {
            values.insert((*path).to_string(), value);
        }
    }
}

/// 按点号路径取值（与内核 `cordis_core::path_value` 同一规则；插件侧只认对象键）。
fn path_value(config: &Json, path: &str) -> Option<Json> {
    let mut cursor = config;
    for segment in path.split('.').filter(|segment| !segment.is_empty()) {
        cursor = cursor.get(segment)?;
    }
    Some(cursor.clone())
}

/// 应用一次宿主推送给**某个实例**的 volatile 新值（`OP_VOLATILE_APPLY`）。
fn apply_volatile(owner: u64, payload: &Json) -> Result<(), String> {
    let Some(list) = payload.get("paths").and_then(|value| value.as_array()) else {
        return Err("OP_VOLATILE_APPLY 的载荷缺少 paths".to_string());
    };
    let mut store = volatile_values().lock().unwrap();
    let values = store.entry(owner).or_default();
    for item in list {
        let Some(path) = item.get("path").and_then(|value| value.as_str()) else {
            return Err("OP_VOLATILE_APPLY 的 path 项缺少 path".to_string());
        };
        let value = item.get("value").cloned().unwrap_or(Json::Null);
        values.insert(path.to_string(), value);
    }
    Ok(())
}

/// 实例卸载时丢掉它的 volatile 副本。
fn forget_volatile(owner: u64) {
    volatile_values().lock().unwrap().remove(&owner);
}

// ------------------------------------------------------------------ 装配

pub struct DescSlot {
    slot: OnceLock<usize>,
}

impl DescSlot {
    pub const fn new() -> Self {
        Self {
            slot: OnceLock::new(),
        }
    }

    pub fn get_or_init(&self, build: impl FnOnce() -> *const PluginDesc) -> *const PluginDesc {
        *self.slot.get_or_init(|| build() as usize) as *const PluginDesc
    }

    /// 与 [`DescSlot::get_or_init`] 相同，但把构造期的 panic 截在入口内。
    ///
    /// `cordis_plugin_v1` 是 `extern "C"`；从 Rust 1.81 起，panic 越过 C ABI
    /// 边界会直接 abort。非法 schema 是插件作者的声明错误，应该让宿主得到
    /// “描述表为空”的可读装载错误，而不是把整个宿主进程带走。
    pub fn get_or_init_catching(
        &self,
        build: impl FnOnce() -> *const PluginDesc,
    ) -> *const PluginDesc {
        *self.slot.get_or_init(|| {
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(build)) {
                Ok(desc) => desc as usize,
                Err(payload) => {
                    eprintln!(
                        "[cordis] 插件描述表构造失败: {}",
                        panic_message(payload.as_ref())
                    );
                    0
                }
            }
        }) as *const PluginDesc
    }
}

impl Default for DescSlot {
    fn default() -> Self {
        Self::new()
    }
}

/// 构造插件描述表。由 `cordis_plugin!` 宏调用，通常不需要手写。
pub fn build_plugin<A>(name: &str, inject: &[&str], apply: A) -> *const PluginDesc
where
    A: Fn(&Ctx, Json) -> PluginResult + Send + Sync + 'static,
{
    build_plugin_with_volatile(name, inject, &[], apply)
}

/// 构造**带 volatile 声明**的插件描述表（M4-5）。
///
/// `volatile` 里的点号路径就是"配置只差它们时不要重挂"的那些位置；
/// 插件用 [`volatile`] 读当前值。声明是静态能力，所以走编译期参数而不是运行期注册。
pub fn build_plugin_with_volatile<A>(
    name: &str,
    inject: &[&str],
    volatile: &[&'static str],
    apply: A,
) -> *const PluginDesc
where
    A: Fn(&Ctx, Json) -> PluginResult + Send + Sync + 'static,
{
    build_plugin_with_provision(name, inject, volatile, &[], apply)
}

/// 构造**带 volatile 与 provision 声明**的插件描述表（M5）。
///
/// `provision` 是"我能安装哪些 key"的声明，写进 `PluginDesc` 的**尾字段**：
/// 宿主在挂载之前就能读到它（`Capabilities`/`probe_capabilities`/`inspect`），
/// v1 宿主根本不看这个字段（它们只读冻结前缀）。命名空间与 `impls_owned_by` 同一口径
/// （今天都是裸名；M6 换 `<name>@<major>` 时两边一起换）。
///
/// # 一个进程只该有一个（重要）
/// 这是"**不经动态库**造一个 ABI 形状插件"的装配入口（宿主夹具、spike、示例用）。
/// 它的 `apply` 槽是**进程级**的：同一进程里第二次调用本函数会用新描述表覆盖入口，
/// 但 apply 槽只认第一个安装的闭包（真插件走 [`cordis_plugin!`]，每个宏调用生成
/// 自己的入口函数，**不受这条限制**）。所以一次进程里只造一个；要造多个，请用
/// `cordis_plugin!`（或把它们放进各自的动态库）。
pub fn build_plugin_with_schema<A>(
    name: &str,
    inject: &[&str],
    volatile: &[&'static str],
    provision: &[&str],
    schema: Option<&str>,
    apply: A,
) -> *const PluginDesc
where
    A: Fn(&Ctx, Json) -> PluginResult + Send + Sync + 'static,
{
    let schema_json: Option<&'static [u8]> = match schema {
        Some(text) => {
            assert!(
                text.len() <= crate::MAX_CONFIG_SCHEMA_BYTES,
                "插件 schema 超过 {} 字节上限",
                crate::MAX_CONFIG_SCHEMA_BYTES
            );
            let value: Json = serde_json::from_str(text).expect("插件 schema 必须是合法 JSON");
            assert!(
                value.is_object(),
                "插件 schema 必须是 JSON 对象（当前是 {}）",
                value
            );
            Some(Box::leak(text.as_bytes().to_vec().into_boxed_slice()))
        }
        None => None,
    };
    // 描述表与其内容是 `'static`：`Box::leak` 一次，进程内只发生一次。
    let id: &'static [u8] = Box::leak(name.as_bytes().to_vec().into_boxed_slice());
    let inject_list: Vec<Json> = inject.iter().map(|s| Json::String((*s).into())).collect();
    let inject_json: &'static [u8] =
        Box::leak(serde_json::to_vec(&inject_list).unwrap().into_boxed_slice());
    let provision_list: Vec<Json> = provision
        .iter()
        .map(|s| Json::String((*s).into()))
        .collect();
    let provision_json: &'static [u8] = Box::leak(
        serde_json::to_vec(&provision_list)
            .unwrap()
            .into_boxed_slice(),
    );
    let boxed: Box<ApplyFn> = Box::new(apply);
    let apply: &'static ApplyFn = Box::leak(boxed);
    let _ = APPLY.set(apply);
    // 声明的路径表也要 `'static`（与 id/inject 同样的 `Box::leak` 处理）。
    let volatile: &'static [&'static str] = Box::leak(volatile.to_vec().into_boxed_slice());
    let _ = VOLATILE_PATHS.set(volatile);

    let desc: &'static PluginDesc = Box::leak(Box::new(PluginDesc {
        abi_version: crate::ABI_VERSION,
        size: std::mem::size_of::<PluginDesc>() as u32,
        id: Blob {
            ptr: id.as_ptr(),
            len: id.len(),
        },
        inject: Blob {
            ptr: inject_json.as_ptr(),
            len: inject_json.len(),
        },
        schema: schema_json.map_or(crate::Blob::EMPTY, crate::Blob::new),
        attach,
        call,
        provision: Blob {
            ptr: provision_json.as_ptr(),
            len: provision_json.len(),
        },
        error_format: crate::ERROR_FORMAT_V1,
        tail_reserved: 0,
    }));
    desc
}

/// 旧公开构造器：不带 schema，保持既有调用方源码兼容。
pub fn build_plugin_with_provision<A>(
    name: &str,
    inject: &[&str],
    volatile: &[&'static str],
    provision: &[&str],
    apply: A,
) -> *const PluginDesc
where
    A: Fn(&Ctx, Json) -> PluginResult + Send + Sync + 'static,
{
    build_plugin_with_schema(name, inject, volatile, provision, None, apply)
}

/// 登记**宿主函数表**（copy-on-attach，G15）：按 `host.size` 拷进插件自有的
/// 零初始化缓冲，此后插件的一切读都走这份副本。
///
/// `pub` 只是测试 seam（D8）；插件正常经由 `PluginDesc.attach` 间接触达它。
///
/// # Safety
///
/// `host` 必须指向一个按 [`HostApi`] 布局起始、且至少可读 `size` 字节的宿主函数表；
/// 可以为空指针（此时直接返回）。
pub unsafe extern "C" fn attach(host: *const HostApi) {
    if host.is_null() {
        return;
    }
    // 版本闸门就在这里，而不是在每次 call 里：一次检查，之后零成本。
    // 主版本不等就不予登记，后续 call 返回"版本不匹配"，宿主会把它标成 FAILED
    // ——失败是**显式**的，而不是以未定义行为的形式出现。
    if (*host).abi_version != crate::ABI_VERSION {
        return;
    }
    // copy-on-attach（G15）：宿主表可能比本插件认识的布局**短**（旧宿主只分配
    // `size` 字节），所以只拷 `min(host.size, sizeof::<HostApi>())` 进零初始化的
    // 自有副本——`size` 之外的字段保持零（`Option<fn>` 的零表示就是 `None`，
    // 正是"宿主不支持"的语义）。此后一切读都在副本之内，越界读无从发生。
    //
    // SAFETY：`HostApi::default()` 只含 u32 与 `Option<fn>`，是合法的全零语义；
    // `copy_nonoverlapping` 只读宿主表前 `min(size, sizeof)` 字节（`size` 由宿主
    // 声明，必在其分配之内），目的端是本进程自有的栈上副本，两者不重叠。
    let copy: HostApi = unsafe {
        let mut copy = HostApi::default();
        let bytes = (*host).size as usize;
        let bytes = bytes.min(std::mem::size_of::<HostApi>());
        std::ptr::copy_nonoverlapping(
            host as *const u8,
            &mut copy as *mut HostApi as *mut u8,
            bytes,
        );
        // 副本只真正拥有 `bytes` 字节；把声明尺寸收回到实际拷贝量，
        // 后续 `field_at` 的边界才与这块内存的真实尺寸一致。
        copy.size = bytes as u32;
        copy
    };
    let _ = HOST.set(copy);
}

/// 测试 seam（D8）：`attach` 登记的那份宿主表副本（未登记 / 版本不符 ⇒ `None`）。
///
/// 宿主侧读它只是为了断言插件登记了什么；插件代码不该用它。
pub fn attached_host() -> Option<&'static HostApi> {
    HOST.get()
}

unsafe extern "C" fn call(ctx: u64, op: u32, a: Blob, b: Blob, out: *mut Out) -> i32 {
    let target = &mut *out;
    let Some(host) = HOST.get() else {
        target.write(b"plugin abi_version mismatch: rebuild this plugin against cordis-abi v1");
        return STATUS_ERR;
    };
    let _ = b;
    // **M5-1 护栏**：下面跑的是插件的**用户代码**（apply / 监听器 / cleanup），
    // 而 `call` 是 `extern "C"` 边界——"C" ABI 不可展开，panic 一旦试图跨过它，
    // 进程直接 abort（宿主连一条可读诊断都拿不到）。`catch_unwind` 放在这里，
    // panic 就在边界**之内**被截住：插件侧的表现是"这次调用失败了"。
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        dispatch(host, ctx, op, a, target)
    })) {
        Ok(status) => {
            // 初始化失败（含 panic）时宿主不会给这个实例发 OP_DISPOSE（它从未成功
            // apply 过），但 apply 里已经注册的回调必须在这里回收，否则它们会
            // 永远持有插件捕获的对象。volatile 副本同理。
            if op == OP_APPLY && status != STATUS_OK {
                registry().dispose_owner(ctx);
                forget_volatile(ctx);
            }
            status
        }
        Err(payload) => {
            if op == OP_APPLY {
                registry().dispose_owner(ctx);
                forget_volatile(ctx);
            }
            let message = format!(
                "插件在 {} 中 panic: {}",
                phase_of(op),
                panic_message(&*payload)
            );
            write_error(target, host.error_format(), &message, "panic", "lifecycle")
        }
    }
}

/// 写一条 `STATUS_ERR`。双方能力都支持时写结构化载荷；结构化放不下就退回纯文本，
/// **至少保留人类可读消息**（ADR 0004 §4.5）。
///
/// # Safety
/// `target` 必须指向宿主提供的有效输出缓冲。
unsafe fn write_error(
    target: &mut Out,
    error_format: u32,
    message: &str,
    code: &str,
    category: &str,
) -> i32 {
    if error_format >= crate::ERROR_FORMAT_V1 {
        let payload = crate::ErrorPayload::new(message)
            .with_code(code)
            .with_category(category)
            .to_json();
        if target.write(payload.as_bytes()) == STATUS_OK {
            return STATUS_ERR;
        }
    }
    target.write(truncate_utf8(message, target.cap).as_bytes());
    STATUS_ERR
}

/// 截到 `max` 字节以内，且不切断 UTF-8 字符边界；有空间时带 `…` 截断标记。
fn truncate_utf8(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    const MARKER: &str = "…";
    if max <= MARKER.len() {
        return if max >= MARKER.len() {
            MARKER.to_string()
        } else {
            String::new()
        };
    }
    let mut end = max - MARKER.len();
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = String::with_capacity(end + MARKER.len());
    out.push_str(&text[..end]);
    out.push_str(MARKER);
    out
}

/// 宿主→插件：只有宿主声明了能力位时才尝试解析；解析失败保留原文。
/// 旧宿主或无法解析的文本按 `unknown` 处理，绝不解释成成功。
/// 旧字符串 API 只取 `.error`。
fn decode_host_error(host: &HostApi, bytes: &[u8]) -> crate::ErrorPayload {
    let text = String::from_utf8_lossy(bytes);
    if host.error_format() >= crate::ERROR_FORMAT_V1 {
        if let Some(payload) = crate::ErrorPayload::from_json(&text) {
            return payload;
        }
    }
    crate::ErrorPayload::new(text.into_owned())
}

fn protocol_error(message: impl Into<String>, code: &str) -> crate::ErrorPayload {
    crate::ErrorPayload::new(message)
        .with_code(code)
        .with_category("protocol")
}

/// 一次 op 的分派（`call` 的实体）。
///
/// 单独抽出来只是为了让上面那个 `catch_unwind` 能**包住整段用户代码**：
/// 面板在哪里，护栏就画在哪里。
///
/// # Safety
/// 与 [`call`] 相同的契约：`target` 必须指向宿主提供的有效输出缓冲。
unsafe fn dispatch(host: &'static HostApi, ctx: u64, op: u32, a: Blob, target: &mut Out) -> i32 {
    match op {
        OP_PING => target.write(format!("ok:{}", crate::ABI_VERSION).as_bytes()),
        OP_VOLATILE_PATHS => {
            let paths: Vec<&str> = VOLATILE_PATHS.get().copied().unwrap_or(&[]).to_vec();
            target.write(serde_json::json!({ "ret": paths }).to_string().as_bytes())
        }
        OP_VOLATILE_APPLY => {
            let payload: Json = serde_json::from_slice(a.as_bytes()).unwrap_or(Json::Null);
            match apply_volatile(ctx, &payload) {
                Ok(()) => target.write(b""),
                Err(message) => write_error(
                    target,
                    host.error_format(),
                    &message,
                    "volatile_apply",
                    "business",
                ),
            }
        }
        OP_APPLY => {
            let config: Json = serde_json::from_slice(a.as_bytes()).unwrap_or(Json::Null);
            let ctx = Ctx { host, handle: ctx };
            let Some(apply) = APPLY.get() else {
                return write_error(
                    target,
                    host.error_format(),
                    "plugin was not initialized",
                    "not_initialized",
                    "lifecycle",
                );
            };
            // 先播种这个实例的 volatile 副本再跑 apply：插件在 apply 里就能读到值。
            let _owner = VolatileOwner::enter(Some(ctx.handle));
            seed_volatile(ctx.handle, &config);
            match apply(&ctx, config) {
                Ok(()) => target.write(b""),
                Err(message) => write_error(
                    target,
                    host.error_format(),
                    &message,
                    "apply_failed",
                    "business",
                ),
            }
        }
        OP_DISPOSE => {
            // 只清这个实例的回调与 volatile 副本；同一 cdylib 的其它实例不受影响。
            registry().dispose_owner(ctx);
            forget_volatile(ctx);
            target.write(b"")
        }
        OP_CALLBACK => {
            let payload: Json = serde_json::from_slice(a.as_bytes()).unwrap_or(Json::Null);
            let id = payload.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let input = CallbackInput::parse(&payload);
            match registry().dispatch(id, input) {
                Ok(Dispatched::Value(ret)) => {
                    target.write(serde_json::json!({ "ret": ret }).to_string().as_bytes())
                }
                // M2（D3）：挂起走**状态码**——载荷里没有任何"我在挂起"的标记，
                // 因此"合法返回值恰好长得像标记"结构上不可能被误解。
                Ok(Dispatched::Pending) => STATUS_PENDING,
                Err(message) => write_error(
                    target,
                    host.error_format(),
                    &message,
                    "callback_failed",
                    "business",
                ),
            }
        }
        OP_CLEANUP => {
            let payload: Json = serde_json::from_slice(a.as_bytes()).unwrap_or(Json::Null);
            let id = payload.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            registry().run_cleanup(id);
            target.write(b"")
        }
        // **未知 op 必须原样返回 Unsupported**：这是 ABI 前向兼容的关键——
        // 新宿主 + 旧插件 = 优雅降级，而不是崩溃。
        _ => STATUS_UNSUPPORTED,
    }
}

/// panic 消息里的"死在哪一步"（诊断用；用户代码只有三个入口）。
fn phase_of(op: u32) -> &'static str {
    match op {
        OP_APPLY => "apply",
        OP_CALLBACK => "回调",
        OP_CLEANUP => "清理",
        OP_VOLATILE_APPLY => "volatile 更新",
        _ => "调用",
    }
}

/// 把 `catch_unwind` 截获的 panic 载荷转成一条可读消息。
///
/// 与 `cordis_core::panic_message` 同一实现，但**不能共用**：`cordis-abi`
/// 是独立 crate（插件只依赖它，不依赖内核），多十几行远比多一条依赖便宜。
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_string()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "未知的 panic 载荷".to_string()
    }
}

/// 声明一个插件并导出入口符号。
#[macro_export]
macro_rules! cordis_plugin {
    (
        name: $name:expr,
        $( inject: $inject:expr, )?
        $( volatile: $volatile:expr, )?
        $( provision: $provision:expr, )?
        $( schema: $schema:expr, )?
        apply: $apply:expr $(,)?
    ) => {
        static __CORDIS_DESC: $crate::sdk::DescSlot = $crate::sdk::DescSlot::new();

        #[no_mangle]
        pub extern "C" fn cordis_plugin_v1() -> *const $crate::PluginDesc {
            __CORDIS_DESC.get_or_init_catching(|| {
                $crate::sdk::build_plugin_with_schema(
                    $name,
                    $crate::__inject!($($inject)?),
                    $crate::__volatile!($($volatile)?),
                    $crate::__provision!($($provision)?),
                    $crate::__schema!($($schema)?),
                    $apply,
                )
            })
        }
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __schema {
    () => {
        None
    };
    ($text:expr) => {
        Some($text)
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __provision {
    () => {
        &[]
    };
    ($list:expr) => {
        &$list
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __volatile {
    () => {
        &[]
    };
    ($list:expr) => {
        &$list
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __inject {
    () => {
        &[]
    };
    ($list:expr) => {
        &$list
    };
}

/// 入口符号名（与 `ENTRY_SYMBOL` 一致，插件作者一般不需要直接引用）。
pub const PLUGIN_ENTRY: &[u8] = ENTRY_SYMBOL;

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一个"宿主函数表"，`size` 与尾追加字段可调，用来模拟不同新旧的宿主。
    fn host_with(
        size: u32,
        require: Option<CtxRequireFn>,
        set: Option<CtxSetFn>,
    ) -> &'static HostApi {
        Box::leak(Box::new(HostApi {
            abi_version: crate::ABI_VERSION,
            size,
            ctx_require: require,
            ctx_set: set,
            error_format: crate::ERROR_FORMAT_V1,
            ..HostApi::default()
        }))
    }

    #[test]
    fn desc_slot_catches_builder_panics_instead_of_crossing_the_c_abi() {
        let slot = DescSlot::new();
        let desc = slot.get_or_init_catching(|| panic!("invalid schema"));
        assert!(desc.is_null(), "构造 panic 必须降级为空描述表");
    }

    unsafe extern "C" fn dummy_require(_: u64, _: Blob, out: *mut Out) -> i32 {
        (*out).write(b"{\"value\":42}")
    }

    unsafe extern "C" fn dummy_set(_: u64, _: Blob, _: Blob, out: *mut Out) -> i32 {
        (*out).write(b"")
    }

    /// M1/M2：ABI v2 的尾追加字段（#12–#17）在**截断宿主**（size 只到 v1
    /// 边界，即 `ctx_isolate` 之前）上必须逐字段降级成「宿主不支持」的可读错误
    /// ——copy-on-attach（M0）已保证截断之外的字段是零初始化 `None`，
    /// 这里的职责只是把 `None` 变成一条人能读懂的消息，而不是 unwrap 或静默。
    #[test]
    fn abi_isolate_degrades_on_a_truncated_host_api() {
        // 截断到 v1 边界：ctx_isolate 起的 6 个字段全部探测不到。
        let ctx = Ctx {
            host: host_with(
                std::mem::offset_of!(HostApi, ctx_isolate) as u32,
                None,
                None,
            ),
            handle: 1,
        };
        let error = ctx
            .isolate("svc", "")
            .expect_err("截断宿主上 isolate 必须报不支持");
        assert!(error.contains("宿主不支持 ctx_isolate"), "{error}");
        let error = ctx
            .intercept("svc", Json::Null)
            .expect_err("截断宿主上 intercept 必须报不支持");
        assert!(error.contains("宿主不支持 ctx_intercept"), "{error}");
        let error = ctx
            .use_child("child", Json::Null)
            .expect_err("截断宿主上 use 必须报不支持");
        assert!(error.contains("宿主不支持 ctx_use"), "{error}");
        let error = ctx.dispose(1).expect_err("截断宿主上 dispose 必须报不支持");
        assert!(error.contains("宿主不支持 ctx_dispose"), "{error}");
        let error = ctx
            .dispose_self()
            .expect_err("截断宿主上 dispose_self 必须报不支持");
        assert!(error.contains("宿主不支持 ctx_dispose_self"), "{error}");
        // M2（#17）：fulfill 在截断之外 ⇒ 同一条降级路径。
        let error = ctx
            .fulfill(7, Json::Null)
            .expect_err("截断宿主上 fulfill 必须报不支持");
        assert!(error.contains("宿主不支持 ctx_fulfill"), "{error}");
        // M2b/M2c（#18/#19）：next / abort 同理。
        let error = ctx
            .next(7, Vec::new(), |_| {})
            .expect_err("截断宿主上 next 必须报不支持");
        assert!(error.contains("宿主不支持 ctx_next"), "{error}");
        let error = ctx.abort(7).expect_err("截断宿主上 abort 必须报不支持");
        assert!(error.contains("宿主不支持 ctx_abort"), "{error}");

        // 全量 size 但字段为 None（宿主支持位关闭）：同一条可读错误——
        // "字段存在"与"宿主提供"是两件事，但插件侧的可观察行为一致。
        let ctx = Ctx {
            host: host_with(std::mem::size_of::<HostApi>() as u32, None, None),
            handle: 1,
        };
        let error = ctx
            .isolate("svc", "")
            .expect_err("None 字段同样必须报不支持");
        assert!(error.contains("宿主不支持 ctx_isolate"), "{error}");
    }

    /// M2-1/M2-2：`ctx_require` 与 `ctx_set` 都是 `HostApi` 的**尾追加**字段——
    /// 旧宿主（`size` 不含对应字段）必须逐字段优雅降级成"宿主不支持"，
    /// 而不是越界读到垃圾。**"中间代"宿主（有 require、没 set）也必须工作**，
    /// 这正是逐字段探测与"整体版本号"的区别。
    #[test]
    fn tail_appended_apis_degrade_gracefully_on_older_hosts() {
        // ① size 止步于 ctx_require 之前：两个都不可用。
        let ctx = Ctx {
            host: host_with(
                std::mem::offset_of!(HostApi, ctx_require) as u32,
                Some(dummy_require),
                Some(dummy_set),
            ),
            handle: 1,
        };
        let error = ctx.require("svc").expect_err("旧宿主上必须报不支持");
        assert!(error.contains("宿主不支持 ctx_require"), "{error}");
        let error = ctx
            .set("svc", Json::Null)
            .expect_err("旧宿主上必须报不支持");
        assert!(error.contains("宿主不支持 ctx_set"), "{error}");

        // ② "中间代"宿主：size 只覆盖到 ctx_require。
        let ctx = Ctx {
            host: host_with(
                std::mem::offset_of!(HostApi, ctx_set) as u32,
                Some(dummy_require),
                Some(dummy_set),
            ),
            handle: 1,
        };
        assert!(ctx.require("svc").is_ok(), "require 在中间代宿主上必须可用");
        let error = ctx
            .set("svc", Json::Null)
            .expect_err("set 在中间代宿主上必须报不支持");
        assert!(error.contains("宿主不支持 ctx_set"), "{error}");

        // ③ 完整 size 的宿主：两个字段都可探测。
        let ctx = Ctx {
            host: host_with(
                std::mem::size_of::<HostApi>() as u32,
                Some(dummy_require),
                Some(dummy_set),
            ),
            handle: 1,
        };
        assert!(
            ctx.require_fn().is_some(),
            "完整 size 的宿主必须能探测到 ctx_require"
        );
        assert!(ctx.require("svc").is_ok(), "探针应当拿到 dummy 应答");
        assert!(
            ctx.set_fn().is_some(),
            "完整 size 的宿主必须能探测到 ctx_set"
        );
        assert!(ctx.set("svc", Json::Null).is_ok(), "set 应当成功");
    }

    /// 调一次 `call`，把 `(状态码, 正文)` 取出来。
    unsafe fn invoke(
        call: unsafe extern "C" fn(u64, u32, Blob, Blob, *mut Out) -> i32,
        op: u32,
        payload: &str,
    ) -> (i32, String) {
        let mut buffer = [0u8; 512];
        let mut out = Out {
            ptr: buffer.as_mut_ptr(),
            cap: buffer.len(),
            len: 0,
        };
        let status = call(1, op, Blob::new(payload.as_bytes()), Blob::EMPTY, &mut out);
        (
            status,
            String::from_utf8_lossy(&buffer[..out.len]).into_owned(),
        )
    }

    fn callback_input() -> CallbackInput {
        CallbackInput {
            kind: CallbackKind::Event,
            event: "test".into(),
            method: None,
            args: Vec::new(),
            dispatch_id: 1,
            meta: None,
        }
    }

    fn callback_value(id: u64) -> Json {
        match registry()
            .dispatch(id, callback_input())
            .expect("回调应当成功")
        {
            Dispatched::Value(value) => value,
            Dispatched::Pending => panic!("测试回调不应挂起"),
        }
    }

    /// 回调表不能在执行用户代码时被持有：监听器里 `ctx.on(...)` 会再次登记
    /// 同一张表。用**本测试私有的 Registry** 做判据，避免其他并行测试的短暂
    /// 占锁被误判成实现回归。
    #[test]
    fn callback_tables_are_released_before_user_code_runs() {
        let table = Arc::new(Registry::default());

        let weak = Arc::downgrade(&table);
        let handler: Handler = Arc::new(move |_input: CallbackInput| {
            let table = weak.upgrade().ok_or("registry 已释放")?;
            if table.handlers.try_lock().is_err() {
                return Err("handlers 表在回调执行期间仍被持有".into());
            }
            let nested: Handler = Arc::new(|_input: CallbackInput| Ok(Json::Null));
            table.register_handler_for(0xA1, nested);
            Ok(Json::Null)
        });
        let id = table.register_handler_for(0xA1, handler);
        assert!(matches!(
            table.dispatch(id, callback_input()),
            Ok(Dispatched::Value(Json::Null))
        ));

        let weak = Arc::downgrade(&table);
        let mut iface: HashMap<String, AsyncIfaceFn> = HashMap::new();
        iface.insert(
            "go".into(),
            Arc::new(move |_args: Json, _meta: Option<Json>, _dispatch_id: u64| {
                let table = weak.upgrade().ok_or("registry 已释放")?;
                if table.async_ifaces.try_lock().is_err() {
                    return Err("async_ifaces 表在方法执行期间仍被持有".into());
                }
                Ok(AsyncReply::Reply(Json::Null))
            }),
        );
        let id = table.register_async_iface_for(0xA2, iface);
        let input = CallbackInput {
            kind: CallbackKind::Iface,
            event: "test".into(),
            method: Some("go".into()),
            ..callback_input()
        };
        assert!(matches!(
            table.dispatch(id, input),
            Ok(Dispatched::Value(Json::Null))
        ));

        let weak = Arc::downgrade(&table);
        let handler: AsyncHandler = Arc::new(move |_args, _dispatch_id| {
            let table = weak.upgrade().ok_or("registry 已释放")?;
            if table.async_handlers.try_lock().is_err() {
                return Err("async_handlers 表在回调执行期间仍被持有".into());
            }
            Ok(AsyncReply::Reply(Json::Null))
        });
        let id = table.register_async_handler_for(0xA3, handler);
        assert!(matches!(
            table.dispatch(id, callback_input()),
            Ok(Dispatched::Value(Json::Null))
        ));
    }

    /// volatile 副本按**实例句柄**隔离：挂载第二个实例不能改掉第一个实例读到的值。
    #[test]
    fn volatile_values_are_scoped_by_instance() {
        apply_volatile(
            0xB1,
            &serde_json::json!({ "paths": [{ "path": "data", "value": 1 }] }),
        )
        .expect("实例 1 播种");
        apply_volatile(
            0xB2,
            &serde_json::json!({ "paths": [{ "path": "data", "value": 2 }] }),
        )
        .expect("实例 2 播种");

        let first: Handler = Arc::new(|_input| Ok(volatile("data").unwrap_or(Json::Null)));
        let second: Handler = Arc::new(|_input| Ok(volatile("data").unwrap_or(Json::Null)));
        let first = registry().register_handler_for(0xB1, first);
        let second = registry().register_handler_for(0xB2, second);

        assert_eq!(
            callback_value(first),
            serde_json::json!(1),
            "实例 1 必须读到自己那份值"
        );
        assert_eq!(
            callback_value(second),
            serde_json::json!(2),
            "实例 2 读到自己那份值"
        );
        assert_eq!(
            callback_value(first),
            serde_json::json!(1),
            "实例 2 的初始化不得覆盖实例 1"
        );

        // 同步快速路径的续延不经过 `dispatch`，也必须处在同一实例作用域里。
        apply_volatile(
            0xB3,
            &serde_json::json!({ "paths": [{ "path": "data", "value": 3 }] }),
        )
        .expect("实例 3 播种");
        let seen = Arc::new(Mutex::new(None));
        let seen_in = seen.clone();
        let continuation = registry().register_text_continuation_for(
            0xB3,
            Box::new(move |_result| {
                *seen_in.lock().unwrap() = volatile("data");
            }),
        );
        registry().complete(continuation, &serde_json::json!({ "result": null }));
        assert_eq!(
            *seen.lock().unwrap(),
            Some(serde_json::json!(3)),
            "内联续延也要读到实例 3 的值"
        );
    }

    /// **M5-1**：插件用户代码的 panic 必须在 `call` **之内**被截住。
    ///
    /// `call` 是 `extern "C"` 边界，而 Rust 的 "C" ABI 是**不可展开**的：
    /// panic 一旦试图跨过它，进程直接 abort——宿主连一条可读诊断都拿不到。
    /// 这正是"任意第三方插件都能拖垮进程"的后半条路（前半条是延迟回调里的裸指针）。
    ///
    /// 三条 panic 路径（apply / 回调 / 清理）必须放进**同一个** `#[test]`：
    /// `HOST` 与 `APPLY` 都是 `OnceLock`，一个动态库进程内只装一次。
    #[test]
    fn plugin_panics_become_status_err_at_the_abi_boundary() {
        unsafe { attach(host_with(std::mem::size_of::<HostApi>() as u32, None, None)) };

        let desc = build_plugin("panicky", &[], |_ctx: &Ctx, config: Json| {
            if config
                .get("boom")
                .and_then(|value| value.as_bool())
                .unwrap_or(false)
            {
                panic!("apply 内爆了");
            }
            Ok(())
        });
        let call = unsafe { (*desc).call };

        // ① apply：插件体里 panic。
        let payload = serde_json::json!({ "boom": true }).to_string();
        let (status, text) = unsafe { invoke(call, OP_APPLY, &payload) };
        assert_eq!(
            status, STATUS_ERR,
            "apply 的 panic 必须转成 STATUS_ERR，而不是 abort：{text}"
        );
        assert!(text.contains("apply"), "消息要能定位到阶段：{text}");
        assert!(text.contains("panic"), "消息要说明发生了什么：{text}");
        let structured = crate::ErrorPayload::from_json(&text).expect("panic 应产生结构化载荷");
        assert_eq!(structured.code, "panic");
        assert_eq!(structured.category, "lifecycle");

        // ② 回调（OP_CALLBACK）：监听器体里 panic。
        let handler: Handler = Arc::new(|_input: CallbackInput| panic!("监听器内爆了"));
        let id = registry().register_handler_for(0, handler);
        let payload = serde_json::json!({
            "kind": "event",
            "id": id,
            "event": "boom",
            "args": [],
        })
        .to_string();
        let (status, text) = unsafe { invoke(call, OP_CALLBACK, &payload) };
        assert_eq!(
            status, STATUS_ERR,
            "回调的 panic 必须转成 STATUS_ERR，而不是 abort：{text}"
        );
        assert!(text.contains("回调"), "消息要能定位到阶段：{text}");
        let structured =
            crate::ErrorPayload::from_json(&text).expect("回调 panic 应产生结构化载荷");
        assert_eq!(structured.code, "panic");

        // ③ 清理（OP_CLEANUP）：cleanup 体里 panic。
        let id = registry().register_cleanup_for(0, Box::new(|| panic!("清理内爆了")));
        let payload = serde_json::json!({ "id": id }).to_string();
        let (status, text) = unsafe { invoke(call, OP_CLEANUP, &payload) };
        assert_eq!(
            status, STATUS_ERR,
            "清理的 panic 必须转成 STATUS_ERR，而不是 abort：{text}"
        );
        assert!(text.contains("清理"), "消息要能定位到阶段：{text}");
        let structured =
            crate::ErrorPayload::from_json(&text).expect("清理 panic 应产生结构化载荷");
        assert_eq!(structured.code, "panic");
    }

    unsafe extern "C" fn reject_on(_: u64, _: Blob, _: u64, out: *mut Out) -> i32 {
        (*out).write(b"instance is closed");
        STATUS_ERR
    }

    unsafe extern "C" fn reject_effect(_: u64, _: u64, out: *mut Out) -> i32 {
        (*out).write(b"instance is closed");
        STATUS_ERR
    }

    unsafe extern "C" fn reject_iface(
        _: u64,
        _: Blob,
        _: u32,
        _: Blob,
        _: u64,
        out: *mut Out,
    ) -> i32 {
        (*out).write(b"instance is closed");
        STATUS_ERR
    }

    /// 一个"实例已卸载"的宿主：所有注册函数都返回 `STATUS_ERR`。
    fn rejecting_host() -> &'static HostApi {
        Box::leak(Box::new(HostApi {
            abi_version: crate::ABI_VERSION,
            size: std::mem::size_of::<HostApi>() as u32,
            ctx_on: Some(reject_on),
            ctx_effect: Some(reject_effect),
            ctx_provide_iface: Some(reject_iface),
            error_format: crate::ERROR_FORMAT_V1,
            ..HostApi::default()
        }))
    }

    /// 回调析构可能重入 SDK；回收时不能还持有任何回调表锁。
    ///
    /// 用 `try_lock` 而不是真的阻塞：旧实现会在 `forget` 的锁内丢弃 `Handler`，
    /// 这个析构探针会看到 `WouldBlock`，测试直接失败而不是挂死 CI。
    #[test]
    fn callback_cleanup_drops_outside_registry_locks() {
        use std::sync::atomic::AtomicBool;

        struct ReenterOnDrop {
            table: Arc<Registry>,
            saw_locked: Arc<AtomicBool>,
        }

        impl Drop for ReenterOnDrop {
            fn drop(&mut self) {
                if self.table.handlers.try_lock().is_err() {
                    self.saw_locked.store(true, Ordering::SeqCst);
                }
            }
        }

        let table = Arc::new(Registry::default());
        let saw_locked = Arc::new(AtomicBool::new(false));
        let probe = Arc::new(ReenterOnDrop {
            table: table.clone(),
            saw_locked: saw_locked.clone(),
        });
        let handler: Handler = Arc::new(move |_input: CallbackInput| {
            let _ = &probe;
            Ok(Json::Null)
        });
        table.register_handler_for(0xCC, handler);

        table.dispose_owner(0xCC);

        assert!(
            !saw_locked.load(Ordering::SeqCst),
            "回调析构发生在 Registry 锁内：重入注册/注销会死锁"
        );
        assert!(table.handlers.lock().unwrap().is_empty());
        assert!(table.owners.lock().unwrap().is_empty());
    }

    /// 注册失败必须回滚**已经放进全局回调表**的闭包。
    ///
    /// 失效实例不会再收到 `OP_DISPOSE`；不回滚的话，每次失败的
    /// `ctx.on`/`on_async`/`provide_iface`/`effect` 都会永久保留一份捕获对象。
    #[test]
    fn failed_registrations_release_the_callbacks_they_registered() {
        let ctx = Ctx::from_host(rejecting_host(), 0xDEAD_BEEF);

        let retained = Arc::new(());
        let weak = Arc::downgrade(&retained);
        for _ in 0..100 {
            let captured = retained.clone();
            let result = ctx.on("event", move |_args| {
                let _ = &captured;
                Ok(Json::Null)
            });
            assert!(result.is_err(), "宿主拒绝时 ctx.on 必须报错");
        }
        drop(retained);
        assert!(weak.upgrade().is_none(), "失败的 ctx.on 保留了捕获对象");

        let retained = Arc::new(());
        let weak = Arc::downgrade(&retained);
        for _ in 0..100 {
            let captured = retained.clone();
            let result = ctx.on_async("event", move |_args, _dispatch| {
                let _ = &captured;
                Ok(AsyncReply::Reply(Json::Null))
            });
            assert!(result.is_err(), "宿主拒绝时 ctx.on_async 必须报错");
        }
        drop(retained);
        assert!(
            weak.upgrade().is_none(),
            "失败的 ctx.on_async 保留了捕获对象"
        );

        let retained = Arc::new(());
        let weak = Arc::downgrade(&retained);
        for _ in 0..100 {
            let captured = retained.clone();
            let method: IfaceFn = Arc::new(move |_args, _meta| {
                let _ = &captured;
                Ok(Json::Null)
            });
            let result = ctx.provide_iface("svc", 1, vec![("go", method)]);
            assert!(result.is_err(), "宿主拒绝时 provide_iface 必须报错");
        }
        drop(retained);
        assert!(
            weak.upgrade().is_none(),
            "失败的 provide_iface 保留了方法表"
        );

        let retained = Arc::new(());
        let weak = Arc::downgrade(&retained);
        for _ in 0..100 {
            let captured = retained.clone();
            let result = ctx.effect(move || {
                let _ = &captured;
            });
            assert!(result.is_err(), "宿主拒绝时 ctx.effect 必须报错");
        }
        drop(retained);
        assert!(weak.upgrade().is_none(), "失败的 ctx.effect 保留了清理闭包");
    }

    #[test]
    fn structured_error_is_used_when_the_host_supports_it() {
        let mut buffer = vec![0u8; 4096];
        let mut out = Out {
            ptr: buffer.as_mut_ptr(),
            cap: buffer.len(),
            len: 0,
        };
        let status = unsafe {
            write_error(
                &mut out,
                crate::ERROR_FORMAT_V1,
                "boom",
                "E_BOOM",
                "business",
            )
        };
        assert_eq!(status, STATUS_ERR);
        buffer.truncate(out.len);
        let decoded = crate::ErrorPayload::from_json(&String::from_utf8(buffer).unwrap())
            .expect("支持能力的宿主必须收到结构化载荷");
        assert_eq!(decoded.error, "boom");
        assert_eq!(decoded.code, "E_BOOM");
        assert_eq!(decoded.category, "business");
    }

    #[test]
    fn text_only_host_gets_plain_text() {
        let mut buffer = vec![0u8; 4096];
        let mut out = Out {
            ptr: buffer.as_mut_ptr(),
            cap: buffer.len(),
            len: 0,
        };
        let status = unsafe { write_error(&mut out, 0, "boom", "E_BOOM", "business") };
        assert_eq!(status, STATUS_ERR);
        buffer.truncate(out.len);
        assert_eq!(String::from_utf8(buffer).unwrap(), "boom");
    }

    #[test]
    fn structured_error_falls_back_to_text_when_it_does_not_fit() {
        let mut buffer = vec![0u8; 64];
        let mut out = Out {
            ptr: buffer.as_mut_ptr(),
            cap: buffer.len(),
            len: 0,
        };
        let status = unsafe {
            write_error(
                &mut out,
                crate::ERROR_FORMAT_V1,
                "boom",
                "E_BOOM",
                "business",
            )
        };
        assert_eq!(status, STATUS_ERR, "结构放不下也必须保留消息");
        buffer.truncate(out.len);
        assert_eq!(String::from_utf8(buffer).unwrap(), "boom");
    }

    #[test]
    fn message_larger_than_out_is_truncated() {
        let mut buffer = vec![0u8; 8];
        let mut out = Out {
            ptr: buffer.as_mut_ptr(),
            cap: buffer.len(),
            len: 0,
        };
        let status = unsafe {
            write_error(
                &mut out,
                crate::ERROR_FORMAT_V1,
                "abcdefghij",
                "E_BOOM",
                "business",
            )
        };
        assert_eq!(status, STATUS_ERR);
        assert!(out.len <= 8, "out.len 不得超过 cap：{}", out.len);
        buffer.truncate(out.len);
        assert_eq!(String::from_utf8(buffer).unwrap(), "abcde…");
    }

    #[test]
    fn chinese_message_truncates_on_a_char_boundary_with_marker() {
        let mut buffer = vec![0u8; 8];
        let mut out = Out {
            ptr: buffer.as_mut_ptr(),
            cap: buffer.len(),
            len: 0,
        };
        let status = unsafe {
            write_error(
                &mut out,
                crate::ERROR_FORMAT_V1,
                "中文错误",
                "E_BOOM",
                "business",
            )
        };
        assert_eq!(status, STATUS_ERR);
        assert!(out.len <= 8, "out.len 不得超过 cap：{}", out.len);
        buffer.truncate(out.len);
        let text = String::from_utf8(buffer).expect("截断后必须仍是合法 UTF-8");
        assert!(text.ends_with('…'), "必须有截断标记：{text}");
        assert!(text.len() <= 8, "截断后长度：{}", text.len());
    }

    #[test]
    fn tiny_buffer_is_bounded_and_not_a_nul_string() {
        let mut buffer = vec![0u8; 2];
        let mut out = Out {
            ptr: buffer.as_mut_ptr(),
            cap: buffer.len(),
            len: 0,
        };
        let status = unsafe {
            write_error(
                &mut out,
                crate::ERROR_FORMAT_V1,
                "中文",
                "E_BOOM",
                "business",
            )
        };
        assert_eq!(status, STATUS_ERR);
        assert!(out.len <= 2, "out.len 不得超过 cap：{}", out.len);
        buffer.truncate(out.len);
        assert_eq!(String::from_utf8(buffer).unwrap(), "");
    }

    #[test]
    fn twenty_k_message_is_truncated_to_the_incoming_cap() {
        let message = "x".repeat(20_000);
        let mut buffer = vec![0u8; 16 * 1024];
        let mut out = Out {
            ptr: buffer.as_mut_ptr(),
            cap: buffer.len(),
            len: 0,
        };
        let status = unsafe {
            write_error(
                &mut out,
                crate::ERROR_FORMAT_V1,
                &message,
                "E_BOOM",
                "business",
            )
        };
        assert_eq!(status, STATUS_ERR);
        assert!(out.len <= 16 * 1024, "out.len 不得超过 cap：{}", out.len);
        buffer.truncate(out.len);
        let text = String::from_utf8(buffer).unwrap();
        assert!(text.ends_with('…'), "必须带截断标记");
        assert!(!text.contains('\0'), "不得出现 NUL 串");
    }

    static REQ_CODE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    unsafe extern "C" fn coded_require(_: u64, _: Blob, out: *mut Out) -> i32 {
        use std::sync::atomic::Ordering;
        let code = if REQ_CODE.fetch_add(1, Ordering::SeqCst) == 0 {
            "E_ONE"
        } else {
            "E_TWO"
        };
        let payload = crate::ErrorPayload::new("same message")
            .with_code(code)
            .to_json();
        (*out).write(payload.as_bytes());
        STATUS_ERR
    }

    #[test]
    fn require_detailed_distinguishes_codes_with_the_same_message() {
        use std::sync::atomic::Ordering;
        REQ_CODE.store(0, Ordering::SeqCst);
        let host = host_with(
            std::mem::size_of::<HostApi>() as u32,
            Some(coded_require),
            None,
        );
        let ctx = Ctx { host, handle: 0 };

        let first = ctx.require_detailed("svc").unwrap_err();
        let second = ctx.require_detailed("svc").unwrap_err();
        assert_eq!(first.error, second.error, "消息必须保持相同");
        assert_ne!(first.code, second.code, "调用者必须能靠 code 区分");
        assert_eq!(ctx.require("svc").unwrap_err(), "same message");
    }

    static SET_CODE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    unsafe extern "C" fn coded_set(_: u64, _: Blob, _: Blob, out: *mut Out) -> i32 {
        use std::sync::atomic::Ordering;
        let code = if SET_CODE.fetch_add(1, Ordering::SeqCst) == 0 {
            "E_SET_ONE"
        } else {
            "E_SET_TWO"
        };
        let payload = crate::ErrorPayload::new("same set message")
            .with_code(code)
            .to_json();
        (*out).write(payload.as_bytes());
        STATUS_ERR
    }

    #[test]
    fn set_detailed_distinguishes_codes_with_the_same_message() {
        use std::sync::atomic::Ordering;
        SET_CODE.store(0, Ordering::SeqCst);
        let host = host_with(std::mem::size_of::<HostApi>() as u32, None, Some(coded_set));
        let ctx = Ctx { host, handle: 0 };

        let first = ctx.set_detailed("svc", Json::Null).unwrap_err();
        let second = ctx.set_detailed("svc", Json::Null).unwrap_err();
        assert_eq!(first.error, second.error, "消息必须保持相同");
        assert_ne!(first.code, second.code, "调用者必须能靠 code 区分");
        assert_eq!(ctx.set("svc", Json::Null).unwrap_err(), "same set message");
    }

    unsafe extern "C" fn over_cap_require(_: u64, _: Blob, out: *mut Out) -> i32 {
        (*out).len = 20_000;
        STATUS_ERR
    }

    #[test]
    fn over_cap_host_output_is_bounded_and_readable() {
        let host = host_with(
            std::mem::size_of::<HostApi>() as u32,
            Some(over_cap_require),
            None,
        );
        let ctx = Ctx { host, handle: 0 };
        let error = ctx.require_detailed("svc").unwrap_err();
        assert_eq!(error.code, "host_protocol");
        assert!(
            error.error.contains("超过插件缓冲"),
            "必须给出有界诊断：{}",
            error.error
        );
    }

    unsafe extern "C" fn structured_require(_: u64, _: Blob, out: *mut Out) -> i32 {
        let payload = crate::ErrorPayload::new("clean")
            .with_code("E_HOST")
            .to_json();
        (*out).write(payload.as_bytes());
        STATUS_ERR
    }

    unsafe extern "C" fn plain_require(_: u64, _: Blob, out: *mut Out) -> i32 {
        (*out).write(b"clean");
        STATUS_ERR
    }

    static TOO_SMALL_READ_CALLS: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);
    static TOO_SMALL_WRITE_CALLS: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);

    unsafe extern "C" fn too_small_once(_: u64, _: Blob, out: *mut Out) -> i32 {
        let call = TOO_SMALL_READ_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if call == 0 {
            (*out).len = 32 * 1024;
            STATUS_TOO_SMALL
        } else {
            (*out).write(b"{\"value\":42}")
        }
    }

    unsafe extern "C" fn too_small_never(_: u64, _: Blob, _: Blob, out: *mut Out) -> i32 {
        TOO_SMALL_WRITE_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        (*out).len = 32 * 1024;
        STATUS_TOO_SMALL
    }

    #[test]
    fn structured_host_errors_are_decoded() {
        let host = host_with(
            std::mem::size_of::<HostApi>() as u32,
            Some(structured_require),
            None,
        );
        assert_eq!(
            host.error_format(),
            crate::ERROR_FORMAT_V1,
            "测试宿主必须探测到能力位"
        );
        let ctx = Ctx { host, handle: 0 };
        assert_eq!(ctx.require("svc").unwrap_err(), "clean");
    }

    #[test]
    fn text_host_errors_survive_without_capability() {
        // 旧宿主桩：截断 HostApi.size 到 error_format 之前，模拟能力位 0；
        // 这不是真实旧宿主进程，四格文档中必须标注为“桩”。
        let cap = std::mem::offset_of!(HostApi, error_format) as u32;
        let host = host_with(cap, Some(structured_require), None);
        let ctx = Ctx { host, handle: 0 };
        let err = ctx.require("svc").unwrap_err();
        assert!(
            err.contains("\"error\":\"clean\""),
            "没有能力位时不得私自解析结构：{err}"
        );
    }

    #[test]
    fn too_small_retries_only_the_read_path() {
        use std::sync::atomic::Ordering;

        TOO_SMALL_READ_CALLS.store(0, Ordering::SeqCst);
        TOO_SMALL_WRITE_CALLS.store(0, Ordering::SeqCst);
        let host = host_with(
            std::mem::size_of::<HostApi>() as u32,
            Some(too_small_once),
            Some(too_small_never),
        );
        let ctx = Ctx { host, handle: 0 };

        assert_eq!(ctx.require("svc").unwrap(), serde_json::json!(42));
        assert_eq!(
            TOO_SMALL_READ_CALLS.load(Ordering::SeqCst),
            2,
            "只读路径应在 TOO_SMALL 后扩容重试一次"
        );

        let err = ctx.set_detailed("svc", Json::Null).unwrap_err();
        assert_eq!(err.code, "host_too_small");
        assert_eq!(
            TOO_SMALL_WRITE_CALLS.load(Ordering::SeqCst),
            1,
            "写路径不得重放"
        );
    }

    #[test]
    fn text_host_errors_use_unknown_code_and_category() {
        let cap = std::mem::offset_of!(HostApi, error_format) as u32;
        let host = host_with(cap, Some(plain_require), None);
        let ctx = Ctx { host, handle: 0 };
        let err = ctx.require_detailed("svc").unwrap_err();
        assert_eq!(err.error, "clean");
        assert_eq!(err.code, "unknown");
        assert_eq!(err.category, "unknown");
    }

    #[test]
    fn malformed_inline_response_is_not_a_success() {
        let (tx, rx) = std::sync::mpsc::channel::<Result<Json, String>>();
        let id = registry().register_text_continuation_for(
            0xBAD,
            Box::new(move |result| {
                let _ = tx.send(result);
            }),
        );
        settle_inline(id, b"not-json");
        let message = rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("续延必须被结算")
            .expect_err("非法内联响应不能被当成成功");
        assert!(message.contains("不是合法 JSON"), "{message}");
    }

    #[test]
    fn non_object_inline_response_is_not_a_success() {
        let (tx, rx) = std::sync::mpsc::channel::<Result<Json, String>>();
        let id = registry().register_text_continuation_for(
            0xBAE,
            Box::new(move |result| {
                let _ = tx.send(result);
            }),
        );
        settle_inline(id, b"[]");
        let message = rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("续延必须被结算")
            .expect_err("非对象内联响应不能被当成成功");
        assert!(message.contains("不是 JSON 对象"), "{message}");
    }

    #[test]
    fn non_string_error_field_is_not_a_success() {
        let (tx, rx) = std::sync::mpsc::channel::<Result<Json, String>>();
        let id = registry().register_text_continuation_for(
            0xBAF,
            Box::new(move |result| {
                let _ = tx.send(result);
            }),
        );
        registry().complete(id, &serde_json::json!({ "error": 42 }));
        let message = rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("续延必须被结算")
            .expect_err("非字符串 error 不能被当成成功");
        assert!(message.contains("不是字符串"), "{message}");
    }

    #[test]
    fn non_object_callback_payload_is_not_a_success() {
        let (tx, rx) = std::sync::mpsc::channel::<Result<Json, String>>();
        let id = registry().register_text_continuation_for(
            0xBB1,
            Box::new(move |result| {
                let _ = tx.send(result);
            }),
        );
        registry().complete(id, &Json::Null);
        let message = rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("续延必须被结算")
            .expect_err("非对象回调载荷不能被当成成功");
        assert!(message.contains("不是 JSON 对象"), "{message}");
    }

    #[test]
    fn detailed_continuation_preserves_error_siblings() {
        let seen = Arc::new(Mutex::new(None));
        let seen_in = seen.clone();
        let id = registry().register_detailed_continuation_for(
            0xBB2,
            Box::new(move |result| {
                *seen_in.lock().unwrap() = Some(result);
            }),
        );
        registry().complete(
            id,
            &serde_json::json!({
                "error": "clean",
                "code": "E_CLEAN",
                "category": "business",
                "certainty": "reported",
                "retry": "never",
                "detail": { "field": "x" }
            }),
        );
        let error = seen
            .lock()
            .unwrap()
            .take()
            .expect("续延必须被结算")
            .expect_err("错误载荷必须保留为 Err");
        assert_eq!(error.error, "clean");
        assert_eq!(error.code, "E_CLEAN");
        assert_eq!(error.category, "business");
        assert_eq!(error.certainty, "reported");
        assert_eq!(error.retry, "never");
        assert_eq!(error.detail, serde_json::json!({ "field": "x" }));
    }

    #[test]
    fn text_continuation_still_reads_only_the_error_string() {
        let seen = Arc::new(Mutex::new(None));
        let seen_in = seen.clone();
        let id = registry().register_text_continuation_for(
            0xBB3,
            Box::new(move |result| {
                *seen_in.lock().unwrap() = Some(result);
            }),
        );
        registry().complete(
            id,
            &serde_json::json!({
                "error": "clean",
                "code": "E_CLEAN",
                "category": "business"
            }),
        );
        assert_eq!(
            seen.lock().unwrap().take().expect("续延必须被结算"),
            Err("clean".to_string()),
            "旧字符串入口保持兼容，只读取 error"
        );
    }

    unsafe extern "C" fn malformed_dispatch(
        _: u64,
        _: Blob,
        _: Blob,
        _: u64,
        out: *mut Out,
    ) -> i32 {
        (*out).write(b"not-json");
        STATUS_OK
    }

    fn dispatch_host() -> &'static HostApi {
        Box::leak(Box::new(HostApi {
            abi_version: crate::ABI_VERSION,
            size: std::mem::size_of::<HostApi>() as u32,
            ctx_dispatch: Some(malformed_dispatch),
            error_format: crate::ERROR_FORMAT_V1,
            ..HostApi::default()
        }))
    }

    #[test]
    fn dispatch_settles_text_continuation_on_malformed_inline_json() {
        let ctx = Ctx {
            host: dispatch_host(),
            handle: 0xD15,
        };
        let (tx, rx) = std::sync::mpsc::channel::<Result<Json, String>>();
        let error = ctx
            .dispatch("evt", "emit", Vec::new(), move |result| {
                let _ = tx.send(result);
            })
            .expect_err("非法内联响应必须让 dispatch 返回错误");
        assert!(
            error.contains("invalid") || error.contains("expected"),
            "{error}"
        );
        let callback = rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("续延必须被结算，不能只 forget");
        let message = callback.expect_err("on_done 必须收到错误");
        assert!(message.contains("不是合法 JSON"), "{message}");
    }

    #[test]
    fn dispatch_settles_detailed_continuation_on_malformed_inline_json() {
        let ctx = Ctx {
            host: dispatch_host(),
            handle: 0xD16,
        };
        let seen = Arc::new(Mutex::new(None));
        let seen_in = seen.clone();
        ctx.dispatch_detailed("evt", "emit", Vec::new(), move |result| {
            *seen_in.lock().unwrap() = Some(result);
        })
        .expect_err("非法内联响应必须让 dispatch_detailed 返回错误");
        let error = seen
            .lock()
            .unwrap()
            .take()
            .expect("detailed 续延必须被结算")
            .expect_err("必须交付错误载荷");
        assert!(error.error.contains("不是合法 JSON"), "{error:?}");
    }

    static NEXT_CB: AtomicU64 = AtomicU64::new(0);

    unsafe extern "C" fn capture_next(_: u64, _: u64, _: Blob, on_done: u64, out: *mut Out) -> i32 {
        NEXT_CB.store(on_done, Ordering::SeqCst);
        (*out).write(b"");
        STATUS_OK
    }

    #[test]
    fn next_detailed_delivers_a_structured_error_payload() {
        NEXT_CB.store(0, Ordering::SeqCst);
        let host = Box::leak(Box::new(HostApi {
            abi_version: crate::ABI_VERSION,
            size: std::mem::size_of::<HostApi>() as u32,
            ctx_next: Some(capture_next),
            error_format: crate::ERROR_FORMAT_V1,
            ..HostApi::default()
        }));
        let ctx = Ctx {
            host,
            handle: 0xD17,
        };
        let seen = Arc::new(Mutex::new(None));
        let seen_in = seen.clone();
        ctx.next_detailed(7, Vec::new(), move |result| {
            *seen_in.lock().unwrap() = Some(result);
        })
        .expect("宿主接受 next 后应返回 Ok");
        let cb = NEXT_CB.load(Ordering::SeqCst);
        assert_ne!(cb, 0, "fake 宿主必须拿到续延 id");
        registry().complete(
            cb,
            &serde_json::json!({
                "error": "downstream failed",
                "code": "E_DOWNSTREAM",
                "category": "business",
                "certainty": "reported"
            }),
        );
        let error = seen
            .lock()
            .unwrap()
            .take()
            .expect("detailed 续延必须被结算")
            .expect_err("必须交付错误载荷");
        assert_eq!(error.code, "E_DOWNSTREAM");
        assert_eq!(error.category, "business");
        assert_eq!(error.certainty, "reported");
    }

    #[test]
    fn pending_inline_response_keeps_the_continuation() {
        let (tx, rx) = std::sync::mpsc::channel::<Result<Json, String>>();
        let id = registry().register_text_continuation_for(
            0xBB0,
            Box::new(move |result| {
                let _ = tx.send(result);
            }),
        );
        settle_inline(id, br#"{"pending":true}"#);
        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(20))
                .is_err(),
            "pending 响应必须等 OP_CALLBACK，而不是提前结算"
        );
        registry().forget(id);
    }
}
