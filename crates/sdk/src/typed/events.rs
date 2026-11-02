//! typed 事件的载荷编解码与 typed `next`（M6）。
//!
//! 载荷的**线上形态**是 `Dyn::new(ExternValue(json))`——与宿主把 ABI 载荷翻成
//! `Args` 时用的 `json_to_args` 逐字相同。于是：
//! - typed 监听器（T1）与动态库监听器（T2）看到的是**同一份 JSON**（A9）；
//! - `next` 接力（含跨 ABI 的 `ctx_next`）传的也还是同一份 JSON。

use cordis_core::{Args, BoxFuture, Config, Ctx, Dyn, Error, ExternValue, Level, Next, Ret};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::marker::PhantomData;

/// 载荷 → 单个 `Args` 元素（JSON 信封）。
pub fn encode_payload<P: Serialize>(event: &str, payload: &P) -> Result<Dyn, Error> {
    let value = serde_json::to_value(payload).map_err(|err| -> Error {
        format!("事件 \"{event}\" 的载荷无法序列化: {err}").into()
    })?;
    Ok(Dyn::new(ExternValue(value)))
}

/// 载荷 → 一次派发要用的 `Args`；编不出来就**记日志并放弃这次派发**。
///
/// 返回 `None` 表示"这次派发没发生"——调用点据此返回一个空 future。
/// 为什么不是 panic：这是插件运行期路径，内核的纪律是"失败记日志、不炸宿主"。
pub fn args_or_log<P: Serialize>(ctx: &Ctx, event: &str, payload: &P) -> Option<Args> {
    match encode_payload(event, payload) {
        Ok(encoded) => Some(vec![encoded]),
        Err(err) => {
            ctx.log(Level::Error, &err.to_string());
            None
        }
    }
}

/// 派发参数 → typed 载荷（监听器侧）。
pub fn decode_payload<P: DeserializeOwned>(args: &Args, event: &str) -> Result<P, Error> {
    let raw = args.first().and_then(payload_of).ok_or_else(|| -> Error {
        format!("事件 \"{event}\" 的载荷不是 JSON 信封（`ExternValue`）").into()
    })?;
    serde_json::from_value(raw).map_err(|err| -> Error {
        format!("事件 \"{event}\" 的载荷解不回 typed 形态: {err}").into()
    })
}

/// 一次派发的返回值 → typed 载荷。`None` = 没人表态（或被丢弃）。
pub fn decode_ret<P: DeserializeOwned>(ret: Ret) -> Result<Option<P>, Error> {
    let Some(value) = ret else {
        return Ok(None);
    };
    let raw = payload_of(&value).unwrap_or(Config::Null);
    // `null` 在核心的 `Ret` 里就是"不表态"（与 `undefined` 合并）；
    // 不能把它当成一个必须解码成 `P` 的载荷，否则下游一句 `Reply(Null)`
    // 就会让 typed 上游收到假错误。
    if raw.is_null() {
        return Ok(None);
    }
    serde_json::from_value(raw)
        .map(Some)
        .map_err(|err| -> Error { format!("事件返回值解不回 typed 载荷: {err}").into() })
}

/// typed 版的 [`Next`]：把载荷编成 JSON 信封交给下游，再把下游的决定解回来。
///
/// `None` 有两层含义，都来自内核（`event::is_bailed` 与 waterfall 的短路语义）：
/// 下游**没有表态**，或者链被上游截断了。
pub struct TypedNext<P> {
    next: Next,
    event: &'static str,
    _marker: PhantomData<fn(P)>,
}

impl<P> TypedNext<P> {
    pub fn new(next: Next, event: &'static str) -> Self {
        Self {
            next,
            event,
            _marker: PhantomData,
        }
    }

    /// 事件名（诊断用）。
    pub fn event(&self) -> &'static str {
        self.event
    }
}

impl<P: Serialize + DeserializeOwned + Send + 'static> TypedNext<P> {
    /// 把（可能已改写的）载荷交给下游，并等它的决定回来。
    pub fn call(self, payload: P) -> BoxFuture<Result<Option<P>, Error>> {
        let event = self.event;
        let next = self.next;
        match encode_payload(event, &payload) {
            Ok(encoded) => Box::pin(async move { decode_ret::<P>(next.call(vec![encoded]).await) }),
            Err(err) => Box::pin(async move { Err(err) }),
        }
    }
}

/// 从 `Args` 元素里取 JSON：约定是 `ExternValue`（跨 ABI 的统一形态），
/// 原生监听器直接塞 `Config` 也认（否则一个手写的原生监听器会被静默忽略）。
fn payload_of(value: &Dyn) -> Option<Config> {
    if let Some(exposed) = value.downcast::<ExternValue>() {
        return Some(exposed.0.clone());
    }
    value.downcast::<Config>().map(|config| (*config).clone())
}
