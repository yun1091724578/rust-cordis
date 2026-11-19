//! 服务调用的逐跳记录（ADR 0009 / ADR 0010）。
//!
//! 记录由宿主写入；`call_id` 复用入站派发 id，`parent_call_id` 由宿主在
//! iface 入站与续延交付两个窗口压栈推出。内核只负责在解析/校验失败时补一条
//! `call_id = 0` 的记录，并提供发起方 uid 的 poll 级作用域。
#![allow(clippy::missing_const_for_thread_local)]

use std::cell::Cell;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::{BoxFuture, Config, Result};

/// 一次调用的结局。`Err` 里是错误码（如 `HOST_TIMEOUT` / `host_panic`）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    Err(Box<str>),
}

/// 一条逐跳记录。
#[derive(Clone, Debug)]
pub struct TraceRecord {
    /// 完成时刻（Unix 毫秒）。
    pub ts_ms: u64,
    /// `service.method`；解析失败时仍写请求的名字。
    pub name: Box<str>,
    /// 入站派发 id；解析/校验失败、仅出站超时或宿主 poll 边界 panic 时为 0。
    pub call_id: u64,
    /// 发起时调用作用域的栈顶；0 = 根/未归因。
    pub parent_call_id: u64,
    /// 发起方 fiber 的 uid（根为 `UID_ROOT`）。
    pub caller_uid: u64,
    /// 实现方 fiber 的 uid；解析失败/仅出站超时/poll 边界 panic/NoRuntime 时为 0，
    /// 根实现也是 0（`UID_ROOT`）。
    pub callee_uid: u64,
    /// 实现方运行时名；未记到实现方时退化为请求的服务名，根实现为 `"root"`。
    pub callee_plugin: Box<str>,
    pub elapsed: Duration,
    pub outcome: Outcome,
}

/// 记录出口。实现必须同步、不阻塞调用线程、不重入宿主；内部可以用短锁保护
/// 自己的状态（宿主不对实现做无锁要求）。
pub trait TraceSink: Send + Sync {
    fn record(&self, record: TraceRecord);
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

thread_local! {
    /// 当前 poll 期间的发起方 uid（ADR 0010 §2.1：只覆盖“最近一次 iface 调用”）。
    static CALLER_UID: Cell<u64> = const { Cell::new(0) };
}

/// 当前调用方的 fiber uid；不在任何调用作用域内时为 0。
pub fn current_caller_uid() -> u64 {
    CALLER_UID.with(Cell::get)
}

/// 把 `uid` 放进 poll 级作用域：每次 poll 前设置、poll 后恢复。
///
/// 恢复用 RAII 守卫而不是 poll 后的直写：宿主会在 poll 边界 `catch_unwind`
/// 并继续运行，unwind 路径也必须把 thread-local 还原，否则 panic 之后
/// `current_caller_uid()` 会永久停在上一次发起方。
///
/// 用 `Unpin` 包装而不是 `unsafe` 投影——`BoxFuture` 本身就是 `Unpin`。
pub struct ScopedCaller<F> {
    uid: u64,
    inner: F,
}

impl<F> ScopedCaller<F> {
    pub fn new(uid: u64, inner: F) -> Self {
        Self { uid, inner }
    }
}

struct RestoreCaller(u64);

impl Drop for RestoreCaller {
    fn drop(&mut self) {
        CALLER_UID.with(|cell| cell.set(self.0));
    }
}

impl<F: Future + Unpin> Future for ScopedCaller<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let previous = CALLER_UID.with(|cell| cell.replace(this.uid));
        let _restore = RestoreCaller(previous);
        Pin::new(&mut this.inner).poll(cx)
    }
}

/// 给一个内核 iface 调用套上 caller 作用域。
pub(crate) fn scope_caller(
    future: BoxFuture<Result<Config>>,
    caller_uid: u64,
) -> BoxFuture<Result<Config>> {
    Box::pin(ScopedCaller::new(caller_uid, future))
}

/// 解析/校验失败时的记录（没有派发记录，`call_id = 0`）。
pub(crate) fn failed(
    name: String,
    caller_uid: u64,
    service: &str,
    code: &str,
    started: std::time::Instant,
) -> TraceRecord {
    TraceRecord {
        ts_ms: now_ms(),
        name: name.into_boxed_str(),
        call_id: 0,
        parent_call_id: 0,
        caller_uid,
        callee_uid: 0,
        callee_plugin: service.to_string().into_boxed_str(),
        elapsed: started.elapsed(),
        outcome: Outcome::Err(code.into()),
    }
}

#[cfg(test)]
#[path = "trace_tests.rs"]
mod tests;
