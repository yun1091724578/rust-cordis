//! Fiber：插件的生命周期单元。
//!
//! # 核心改写：Promise 接力链 → 收敛循环
//! JS 的实现（`core/src/fiber.ts:402-468`）是 `_setEpoch` → `_reload` → `_unload` → `_reload` …
//! 两个 async 函数用 `inertia` 这个 Promise 字段互相接力，再加"epoch 变了就提前卸"的检查。
//! 它之所以必须这么写，是因为 JS 里一次状态迁移就是一个"飞出去的 promise"，没有办法等待它从
//! 外部收敛。
//!
//! Rust 里有更直接的表达：一个 **desired/running 二值的收敛循环**。
//! ```text
//! loop {
//!     if failed { break }                 // 失败只能靠 update() 恢复
//!     if desired == running { break }     // 已收敛
//!     if running.is_some() { unload() }   // 早于 load 判定"先卸后装"
//!     else { load(desired) }
//! }
//! ```
//! 等价性可以逐条论证：
//! - **合并中间态**：JS 用 `await Promise.resolve()` 让出一拍来合并同 tick 的多次通知；
//!   循环模型天然合并——中途 desired 变回原值就一次都不跑。
//! - **被抢占**：JS 在装载中途发现 epoch 变了会立刻 `_unload`；循环模型在下一轮迭代
//!   看到 `desired != running` 就走卸载分支，效果一致（差别见下）。
//! - **不丢唤醒**：`driver` 是一把普通异步互斥锁，每次 `settle()` 都排队，排到后重新
//!   读 desired，所以不存在 JS 那种"`if (this.inertia) return` 丢掉一次唤醒"的窗口。
//!
//! # 唯一的故意偏离
//! JS 在装载中途被抢占时**中止**当前 `apply`（靠异步生成器每次 yield 前检查 epoch）。
//! Rust 里做不到抢占一个正在 await 的异步函数，所以当前这次 `apply` 会跑完、副作用会被
//! 完整注册，然后立刻被下一轮卸载回收：
//! - **tracked effect 等价**（最终没有残留注册，代价是中间态多存在一个微任务周期）；
//! - **emission 不等价**：抢占点之后发出的 `emit` 依然会到达监听者，JS 里不会。
//!   这条差别的可执行断言在 `core/tests/failure_modes.rs`（P2-13）。
#![allow(clippy::missing_const_for_thread_local)]

use crate::dyn_value::{BoxFuture, Config, Disposable, Dyn, Error, Result};
use crate::event::{Args, Handler, Mode, Ret, INTERNAL_UPDATE};
use crate::store::{Impl, Inject, Level, RealmId, Runtime, Store};
use crate::{CancelToken, Context, Ctx};
use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::task::Poll;

/// `uid` 的两个哨兵值。
pub const UID_ROOT: u64 = 0;
pub const UID_DISPOSED: u64 = u64::MAX;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FiberState {
    /// 有 `inject` 未满足（不是"正在排队"，是"依赖没齐"）。
    Pending = 0,
    Loading = 1,
    Active = 2,
    Failed = 3,
    Unloading = 4,
    Disposed = 5,
}

/// 一条未满足的必需 inject 的诊断原因。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PendingReason {
    /// 当前上下文没有**可见**的同名提供者。
    Missing,
    /// 当前上下文没有可见的精确 key，但同 base 的其它版本可见
    /// （如缺 `tools@2`，当前域有 `tools@1`）。
    VersionMismatch,
    /// 当前上下文可见的精确 key 已注册，但提供者未 Active 或就绪谓词为 false。
    ///
    /// 提供者尚未 `apply` / `provide` 时还没有注册实现，仍报 [`Missing`](Self::Missing)。
    ProviderNotReady,
    /// 全局注册表有精确 key，但当前上下文不可见（例如在另一个 isolate 域）。
    RealmMismatch,
    /// 这条依赖边会形成运行时环，已拒绝。
    ///
    /// 标记服务于卸载等待图，可能保留到 fiber 卸载；提供者消失后诊断可能短暂
    /// 仍报 `cycle`，这是当前已知的边界，不据此单独推断“环仍然存在”。
    Cycle,
}

impl PendingReason {
    /// 稳定的机器可读名称（CLI/JSON 诊断用）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::VersionMismatch => "version_mismatch",
            Self::ProviderNotReady => "provider_not_ready",
            Self::RealmMismatch => "realm_mismatch",
            Self::Cycle => "cycle",
        }
    }
}

/// 一条未满足的必需 inject。
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct PendingInject {
    /// 精确的 inject key。
    pub key: String,
    pub reason: PendingReason,
    /// 同 base 的已注册 key（升序去重；可能为空）。
    ///
    /// 这是**全局注册表**的候选，不证明它在调用方的 isolate 域里可见。
    pub related: Vec<String>,
}

impl FiberState {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => FiberState::Loading,
            2 => FiberState::Active,
            3 => FiberState::Failed,
            4 => FiberState::Unloading,
            5 => FiberState::Disposed,
            _ => FiberState::Pending,
        }
    }
}

/// `internal/update` 的结果：重启、被钩子否决，或被更晚的 update 超越。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateOutcome {
    /// 重启已执行（revision 已递增、已收敛）。
    Restarted,
    /// 被 `internal/update` 钩子否决：fiber 原地不动，也不是错误。
    Vetoed,
    /// 这次 update 在解析/提交阶段被更晚的 update 超越：没有提交、没有重启。
    ///
    /// 与 `Vetoed` 不同，`Superseded` 不是插件做出的决定，而是并发 update 的
    /// 收敛规则：旧配置不能覆盖新配置。调用方可以忽略它，或据此记录诊断。
    Superseded,
}

/// effect 登记闸门。只在闸门内做计数/状态变更，**绝不**在持锁时执行用户 setup。
struct EffectGate {
    closed: bool,
    in_flight: usize,
}

/// 恢复栈里的一条登记。`id` 让手动 `run` 的句柄能把槽位摘掉，而不是留下空壳。
struct EffectEntry {
    id: u64,
    cell: Arc<Mutex<Option<Disposable>>>,
}

/// 内部 effect 句柄：公开面仍然是 [`Disposable`]，另外允许 fiber 生命周期
/// **只摘除登记而不执行恢复动作**（子 fiber 清理完成时用；递归 dispose 会自锁）。
struct EffectHandle {
    disposable: Disposable,
    owner: Weak<Fiber>,
    id: u64,
}

/// [`Ctx::spawn_scoped`](crate::Context::spawn_scoped) / [`Fiber::spawn_scoped`] 拒绝启动后台任务的原因。
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SpawnScopedError {
    /// 当前线程未处于 Tokio 运行时内。
    NoRuntime,
    /// 上下文没有绑定 fiber，或所属 fiber 已经离开 / 正在卸载 / 已失败 / 已处置。
    InactiveFiber,
}

impl std::fmt::Display for SpawnScopedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoRuntime => f.write_str("受管后台任务必须在 Tokio 运行时内启动"),
            Self::InactiveFiber => {
                f.write_str("无法在未绑定或已关闭/正在卸载的 fiber 上启动受管后台任务")
            }
        }
    }
}

impl std::error::Error for SpawnScopedError {}

/// 受管后台任务重入调用 [`Fiber::restart`] / [`Fiber::update`] / [`Fiber::update_with`] 时，
/// 重载请求已被受理并移交后台任务异步执行，但当前任务不能同步等待卸载/重载完成，因此无法
/// 报告新一代 `apply()` 的最终成败。
///
/// 为什么不能返回 `Ok(())` 或 [`UpdateOutcome::Restarted`]：
/// `unload()` 必须等待当前受管任务退出后才能进入新一代 `load()`；在当前任务退出前读取
/// `fiber.error()` 只会看到旧一代状态，新一代 `apply()` 随后失败进入 [`FiberState::Failed`]
/// 时会误报重载成功。调用方可通过 `err.downcast_ref::<ReloadDeferred>()` 明确区分
/// “重载请求已受理（推迟执行）”与“重载已完成”或“装载已失败”，并在受管任务外部通过
/// [`Fiber::settle`] / [`Fiber::state`] / [`Fiber::error`] 观测最终收敛结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReloadDeferred {
    fiber_name: String,
    scheduled: bool,
    waiting_for_cleanup: bool,
}

impl ReloadDeferred {
    fn scheduled(fiber_name: String) -> Self {
        Self {
            fiber_name,
            scheduled: true,
            waiting_for_cleanup: false,
        }
    }

    fn unscheduled(fiber_name: String) -> Self {
        Self {
            fiber_name,
            scheduled: false,
            waiting_for_cleanup: false,
        }
    }

    fn waiting_for_cleanup(fiber_name: String, scheduled: bool) -> Self {
        Self {
            fiber_name,
            scheduled,
            waiting_for_cleanup: true,
        }
    }

    /// 目标 fiber 名称。
    pub fn fiber_name(&self) -> &str {
        &self.fiber_name
    }

    /// 后台收敛任务是否已成功调度到当前 Tokio 运行时。
    ///
    /// 在受管任务正常运行的 Tokio 上下文内为 `true`；若当前线程无活跃运行时则为 `false`
    /// （此时 `revision`/配置已更新，需外部调用 [`Fiber::settle`] 推进收敛）。
    pub fn is_scheduled(&self) -> bool {
        self.scheduled
    }

    /// 推迟原因是否为“上一代后台清理（超出 `LifecyclePolicy` 预算后移交后台）尚未完成”。
    ///
    /// 为 `true` 时 fiber 停在 [`FiberState::Pending`]，新一代 `apply()` 会在
    /// [`Fiber::wait_background_cleanups`] 归零后自动重试；调用方可等待它后再 [`Fiber::settle`]
    /// 观测最终结果。
    pub fn is_waiting_for_cleanup(&self) -> bool {
        self.waiting_for_cleanup
    }
}

impl std::fmt::Display for ReloadDeferred {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.waiting_for_cleanup {
            write!(
                f,
                "fiber `{}` 的上一代后台清理尚未完成，重载已受理并推迟到清理归零后自动执行",
                self.fiber_name
            )
        } else if self.scheduled {
            write!(
                f,
                "fiber `{}` 的重入重载请求已受理并移交后台异步执行，无法在当前受管任务内等待新一代 apply() 结果",
                self.fiber_name
            )
        } else {
            write!(
                f,
                "fiber `{}` 的重入重载请求已更新目标状态，但当前线程无 Tokio 运行时，未能调度后台收敛",
                self.fiber_name
            )
        }
    }
}

impl std::error::Error for ReloadDeferred {}

/// 生命周期三段等待预算（按 App / `Store` 配置，缺省全部 `None` = 不设上限）。
///
/// 三段预算分别约束不同阶段，绝不混用：
/// - `apply_timeout_ms`：单次插件 `apply`（装载激活）的等待预算；
/// - `drain_timeout_ms`：卸载时等待在途原生调用、受管后台任务与非祖先消费者排空的等待预算；
/// - `dispose_timeout_ms`：`Fiber::dispose_with_report` / `App::close_with_report`
///   （调用方未显式传超时参数时）及热重载前卸载的等待预算。
///
/// **同进程保活红线（ADR 0005 §4）**：超时只代表宿主停止等待并上报故障；
/// 同步阻塞中的原生线程不会被强杀，动态库保活仍须等到最后一次原生调用退出后才能卸载。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LifecyclePolicy {
    pub apply_timeout_ms: Option<u64>,
    pub drain_timeout_ms: Option<u64>,
    pub dispose_timeout_ms: Option<u64>,
}

impl LifecyclePolicy {
    /// 带校验的构造入口：`Some(0)` 被拒绝（0 不是“无限”，`None` 才是不设上限）。
    pub fn new(
        apply_timeout_ms: Option<u64>,
        drain_timeout_ms: Option<u64>,
        dispose_timeout_ms: Option<u64>,
    ) -> std::result::Result<Self, String> {
        let policy = Self {
            apply_timeout_ms,
            drain_timeout_ms,
            dispose_timeout_ms,
        };
        policy.validate()?;
        Ok(policy)
    }

    /// 校验三段等待预算：任一项为 `Some(0)` 时返回可读错误。
    pub(crate) fn validate(self) -> std::result::Result<(), String> {
        let fields = [
            ("apply_timeout_ms", self.apply_timeout_ms),
            ("drain_timeout_ms", self.drain_timeout_ms),
            ("dispose_timeout_ms", self.dispose_timeout_ms),
        ];
        for (name, value) in fields {
            if value == Some(0) {
                return Err(format!(
                    "LifecyclePolicy.{name} 不能为 0；不设上限请传 None，否则请给出正整数毫秒"
                ));
            }
        }
        Ok(())
    }
}

/// 清理错误来源分类。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CleanupErrorKind {
    /// 原生 `Disposable` 恢复回调 panic 或被取消。
    DisposerPanic,
    /// 宿主/载体登记的清理回调失败（例如 T2 `OP_CLEANUP` 返回错误）。
    HostCleanup,
    /// 宿主/载体卸载通知失败（例如 T2 `OP_DISPOSE` 返回错误）。
    HostDispose,
    /// 生命周期内部清理任务异常或状态迁移 panic。
    InternalCleanup,
    /// 清理阶段（排水 / 卸载）超过等待预算，后台清理仍在进行。
    Timeout,
}

impl CleanupErrorKind {
    /// 稳定的机器可读类别名。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DisposerPanic => "disposer_panic",
            Self::HostCleanup => "host_cleanup",
            Self::HostDispose => "host_dispose",
            Self::InternalCleanup => "internal_cleanup",
            Self::Timeout => "timeout",
        }
    }
}

/// 一条结构化的清理错误记录。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CleanupError {
    /// 产生该错误的 fiber 名称。
    pub fiber: String,
    /// 产生该错误的 fiber 唯一编号（创建时固定，不会在处置后坍缩为 `UID_DISPOSED`）。
    pub uid: u64,
    /// 错误来源分类。
    pub kind: CleanupErrorKind,
    /// 可读错误详情。
    pub message: String,
}

/// 一次结构化关闭 / 处置的结果分类。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CloseStatus {
    /// 清理已完成，且未发生任何清理错误。
    Completed,
    /// 清理已完成，但过程中产生了一项或多项清理错误。
    CompletedWithErrors,
    /// 等待超出预算，后台清理任务仍在进行中（动态库与在途调用保活不受破坏）。
    TimedOut,
    /// 在受管后台任务内部重入发起关闭：关闭已启动并移交后台继续执行。
    Deferred,
}

impl CloseStatus {
    /// 稳定的机器可读状态名。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::CompletedWithErrors => "completed_with_errors",
            Self::TimedOut => "timed_out",
            Self::Deferred => "deferred",
        }
    }
}

/// 一次 `Fiber::dispose_with_report` 或 `App::close_with_report` 的结构化关闭报告。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CloseReport {
    /// 关停结果的终态分类。
    pub status: CloseStatus,
    /// 本次关停期间收集到的全部结构化清理错误。
    pub errors: Vec<CleanupError>,
    /// 后台清理是否仍在进行（`TimedOut` / `Deferred` 时为 `true`）。
    pub still_running: bool,
    /// 本次关停等待耗时。
    pub elapsed: std::time::Duration,
}

/// 后台延后清理的 RAII 凭证：持有它表示仍有清理工作在后台收尾，Drop 时销账并唤醒等待者。
pub struct BackgroundCleanupGuard {
    store: Arc<Store>,
    id: u64,
}

impl BackgroundCleanupGuard {
    pub(crate) fn new(store: Arc<Store>, id: u64) -> Self {
        Self { store, id }
    }

    pub(crate) fn id(&self) -> u64 {
        self.id
    }
}

impl Drop for BackgroundCleanupGuard {
    fn drop(&mut self) {
        self.store.finish_background_cleanup(self.id);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SettleDisposition {
    /// 本次调用已同步等到状态机收敛。
    Completed,
    /// 当前任务会被本 fiber 卸载/清理等待退出，收敛已移交后台任务。
    Deferred,
    /// 当前任务会被本 fiber 卸载/清理等待退出，且没有可用的 Tokio 运行时推进后台收敛。
    DeferredNoRuntime,
    /// 上一代后台清理尚未完成：停在 Pending，已（或无法）调度清理归零后的自动重试。
    WaitingForCleanup { scheduled: bool },
}

/// 一条由 fiber 拥有的原生后台任务（`call_iface_stream` 提供者任务与 `Ctx::spawn_scoped` 任务）。
///
/// effect 栈能保证"卸载时执行 disposer"，但它不适合承载**可提前完成**的任务：
/// 每条流都往恢复栈里留一条空壳，长期存活的 fiber 会线性增长。这里用独立表：
/// 任务完成时自己摘除；卸载时统一取消并等待完成。
struct NativeTaskSlot {
    cancel: CancelToken,
    done: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
}

/// 任务结束时（包括 panic / 被丢弃）摘除登记并发送完成信号。
struct NativeTaskDone {
    fiber: Weak<Fiber>,
    id: u64,
    done: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Drop for NativeTaskDone {
    fn drop(&mut self) {
        if let Some(fiber) = self.fiber.upgrade() {
            fiber.native_tasks.lock().unwrap().remove(&self.id);
        }
        if let Some(done) = self.done.take() {
            let _ = done.send(());
        }
    }
}

thread_local! {
    /// 当前 poll 窗口内正在执行的受管原生后台任务所属 fiber（`spawn_scoped` / `spawn_native_task`）。
    ///
    /// 公开生命周期入口靠它识别“任务正在等待自己的 fiber（或其祖先/提供者）收敛/关闭”，
    /// 避免清理等任务、任务等清理的自锁。
    static CURRENT_NATIVE_TASK_FIBER: std::cell::RefCell<Option<Weak<Fiber>>> =
        const { std::cell::RefCell::new(None) };
}

struct NativeTaskGuard {
    previous: Option<Weak<Fiber>>,
}

impl Drop for NativeTaskGuard {
    fn drop(&mut self) {
        let previous = self.previous.take();
        CURRENT_NATIVE_TASK_FIBER.with(|cell| {
            *cell.borrow_mut() = previous;
        });
    }
}

fn enter_native_task(owner: Weak<Fiber>) -> NativeTaskGuard {
    let previous = CURRENT_NATIVE_TASK_FIBER.with(|cell| cell.borrow_mut().replace(owner));
    NativeTaskGuard { previous }
}

fn current_native_task_fiber() -> Option<Arc<Fiber>> {
    CURRENT_NATIVE_TASK_FIBER.with(|cell| cell.borrow().as_ref().and_then(Weak::upgrade))
}

/// 当前线程是否正在执行一条由 `target` 卸载/处置时会等待退出的受管原生任务。
///
/// 只要当前任务所属 fiber 沿「父链（祖先 dispose 递归 dispose 子 fiber）」或
/// 「依赖边（提供者 unload/dispose 等待依赖者卸载）」可达 `target`，
/// `target` 的清理/卸载就必须等当前任务退出；当前任务绝不能反向等待 `target` 收敛。
fn current_task_waited_by(target: &Arc<Fiber>) -> bool {
    let Some(owner) = current_native_task_fiber() else {
        return false;
    };
    let target_ptr = Arc::as_ptr(target);
    let mut stack = vec![owner];
    let mut seen = HashSet::new();
    while let Some(fiber) = stack.pop() {
        let ptr = Arc::as_ptr(&fiber);
        if ptr == target_ptr {
            return true;
        }
        if !seen.insert(ptr as usize) {
            continue;
        }
        if let Some(parent) = fiber.parent_ctx().and_then(|ctx| ctx.fiber_opt()) {
            stack.push(parent);
        }
        let dep_impls: Vec<(Box<str>, Arc<Impl>)> = fiber
            .deps
            .lock()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        for (name, dep) in dep_impls {
            if let Some(provider) = dep.fiber.upgrade() {
                if !fiber.is_cycle_blocked(&name) && !fiber.is_ancestor_of(&provider) {
                    stack.push(provider);
                }
            }
        }
        let committed_impls: Vec<(Box<str>, Arc<Impl>)> = fiber
            .committed
            .lock()
            .unwrap()
            .as_ref()
            .map(|view| {
                view.values()
                    .flat_map(|by_name| by_name.iter().map(|(k, v)| (k.clone(), v.clone())))
                    .collect()
            })
            .unwrap_or_default();
        for (name, dep) in committed_impls {
            if let Some(provider) = dep.fiber.upgrade() {
                if !Arc::ptr_eq(&provider, &fiber)
                    && !fiber.is_cycle_blocked(&name)
                    && !fiber.is_ancestor_of(&provider)
                {
                    stack.push(provider);
                }
            }
        }
    }
    false
}

/// 受管原生后台任务的 Future 包装（零 `unsafe`）。
///
/// 两个硬不变量：
/// 1. **析构顺序不变量**：`future` 声明在 `_done` 之前，Rust 保证按字段声明顺序析构；
///    无论正常完成、panic 展开、还是**尚未首次 `poll` 即被运行时丢弃**，`future` 捕获的
///    全部资源都严格先于 `NativeTaskDone`（摘除槽位 + 发送完成信号）释放。
/// 2. **poll 级身份追踪**：每次 `poll` 压入所属 fiber（含可选 `correlation_id`），
///    返回或 unwind 自动恢复，供生命周期入口识别重入自锁。
struct ScopedTaskFuture {
    future: Option<BoxFuture<()>>,
    owner: Weak<Fiber>,
    correlation_id: Option<Arc<str>>,
    /// 必须保持为最后一个字段。
    _done: NativeTaskDone,
}

impl std::future::Future for ScopedTaskFuture {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let Some(fut) = this.future.as_mut() else {
            return Poll::Ready(());
        };
        let _task_guard = enter_native_task(this.owner.clone());
        let _corr_guard = this
            .correlation_id
            .as_ref()
            .map(|cid| crate::trace::enter_correlation(Some(cid.clone())));
        match fut.as_mut().poll(cx) {
            Poll::Ready(()) => {
                // 在 `_done` 析构前先显式释放内层 future 及其捕获资源。
                this.future = None;
                Poll::Ready(())
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl EffectHandle {
    fn detach(self) {
        if let Some(owner) = self.owner.upgrade() {
            owner.remove_effect(self.id);
        }
    }
}

/// 行内钩子表：事件名 → `(序号, handler)`，按注册顺序排列。
type HookTable = HashMap<Box<str>, Vec<(u64, Handler)>>;

/// 提交视图：隔离域 → 服务名 → 实现。按域分组是跨域读取不串线的关键。
type CommittedView = HashMap<RealmId, HashMap<Box<str>, Arc<Impl>>>;

/// 一次正在进行的 effect 登记。Drop 时递减 `in_flight` 并唤醒卸载等待者；
/// 即使 setup panic，也不会把闸门永久卡住。
struct EffectRegistration<'a> {
    fiber: &'a Fiber,
}

impl Drop for EffectRegistration<'_> {
    fn drop(&mut self) {
        let mut gate = self.fiber.effect_gate.lock().unwrap();
        gate.in_flight = gate.in_flight.saturating_sub(1);
        if gate.in_flight == 0 {
            drop(gate);
            self.fiber.effect_notify.notify_waiters();
        }
    }
}

/// 一条正在执行的 `internal/update` chain。
///
/// Drop 时递减在飞计数；如果这条 chain 已被更晚的 update 超越，标记需要
/// quiescence replay。真正的 replay 只在最后一个 chain 结束时调度。
struct UpdateChain {
    fiber: Arc<Fiber>,
    rev: u64,
}

impl Drop for UpdateChain {
    fn drop(&mut self) {
        self.fiber.finish_update_chain(self.rev);
    }
}

pub struct Fiber {
    uid: AtomicU64,
    /// 创建时固定的唯一编号（不会被 `dispose()` 覆写成 `UID_DISPOSED`），用于错误归因与收集器路由。
    id: u64,
    /// 宿主/装载器在挂载时绑定的不可变行标（如配置行 `label`）；非装载器挂载的 fiber 为 `None`。
    /// 不可变且不向下传递给插件再创建的子 fiber，防止子插件冒用父行身份。
    host_label: Option<Arc<str>>,
    /// 从自身到根 fiber 的不可变祖先链 `[self.id, parent.id, ..., UID_ROOT]`。
    ancestor_ids: Arc<[u64]>,
    pub store: Arc<Store>,
    /// 创建它的上下文（对应 JS 构造函数里的 `parent: Context`）。
    parent: Option<Ctx>,
    /// 它自己的上下文。`dispose` 结束时置 `None`，用来打破 `Context <-> Fiber` 的强引用环。
    /// 根 fiber 这里恒为 `None`——根 ctx 由 `ctx_weak` 反向弱持，强引用在根 ctx 的
    /// `root_keepalive` 上，于是 `App::new().root` 单独留下 root 也不会丢 fiber。
    ctx: Mutex<Option<Ctx>>,
    /// 根 fiber 专用：根 ctx 的弱引用。plugin fiber 用上面的强 `ctx`。
    ctx_weak: Mutex<Option<Weak<Context>>>,
    /// 原始配置（校验前的值），对应 JS 的 `fiber.config`。
    pub config: Mutex<Config>,
    /// **已解析**的配置（`internal/config` 之后、插件校验器之后）。
    ///
    /// 与 `config` 的分工：`config` 是"用户/文件说的"，这里是"插件实际拿到的"。
    /// volatile 的候选比较（M4-5）必须比这个——`!!js` 与默认值都在这一层才成形。
    resolved: Mutex<Config>,
    /// **volatile 引用表**（M4-5）：点号路径 → 稳定引用。
    ///
    /// 每次 `load` 重建（对应上游"校验期创建引用"）；volatile-only 更新**不重建**，
    /// 只写引用里的值——引用身份稳定正是这个机制的全部意义。
    volatile: Mutex<crate::volatile::VolatileTable>,
    inject: Mutex<Inject>,
    /// 行级 `inject: { optional: [...] }`：不参与激活判定，但声明了访问意图。
    ///
    /// `ctx.require` 在服务缺席时因此报 `Inactive`（已声明、未就绪），而不是
    /// `NoInject`（从未声明）；`ctx.get` 本来就允许探测，无需声明。
    optional_inject: Mutex<HashSet<Box<str>>>,
    pub runtime: Option<Arc<Runtime>>,
    /// 已解析的依赖实现（JS 的 `_store`）。**实时表**，epoch 由它 + `revision` 算出。
    deps: Mutex<HashMap<Box<str>, Arc<Impl>>>,
    /// **提交视图**（论文 Table 2 的 ω，JS 的 `fiber.store`）。
    ///
    /// 与 `deps` 的区别是全部要害：`deps` 被 `check_impl` 随时改写、只服务 epoch 计算；
    /// 而这里是**激活那一刻冻结**的快照，**属性访问读的是它**
    /// （`reflect.ts:86` `const impl = fiber.store?.[prop]`）。
    ///
    /// **按域索引**：同一个 fiber 可以在默认域与派生 `isolate` 域提供同名服务
    /// （`provide` 的冲突检查也是按 `(name, realm)` 做的）。只按名字存会让后注册的
    /// 那条覆盖先注册的，`require()` 于是在默认域读到派生域的实现。
    ///
    /// 两处写入、两处清除，与参考实现逐条对应：
    /// - 写入①：`load` 开头按域分组 `= deps.clone()`（`fiber.ts:421`）
    /// - 写入②：`provide` 登记自己提供的服务（`reflect.ts:193`）
    /// - 清除①：`unload` / `fail` 在**所有 disposer 跑完之后**置 `None`（`fiber.ts:459`）
    /// - 清除②：`provide` 的 disposer **末尾**摘掉自己那条（`reflect.ts:202`，
    ///   紧邻注释是 *"ensure self access before dependencies cleanup"*）
    ///
    /// 合起来的规格：**组件在整个生命周期内（含拆除过程）都能读到它依赖的东西。**
    /// 论文 p.61：*"Reading the view rather than the store is what Theorem 63 relies on."*
    committed: Mutex<Option<CommittedView>>,
    /// 本 fiber 拥有的一切可逆副作用，卸载时**逆序**释放。
    ///
    /// 列表始终归 fiber 所有：`dispose_effects` 每次只 `pop` 一个，调用者的
    /// Future 被丢弃不会把尚未执行的恢复栈一起带走。手动 `run` 句柄会立即
    /// 摘掉对应槽位（`EffectEntry::id`），长期存活的 fiber 不会线性累积死条目。
    disposables: Mutex<Vec<EffectEntry>>,
    /// effect 登记序号：句柄手动释放时用它定位槽位。
    effect_sn: AtomicU64,
    /// 子 fiber 在父 fiber 恢复栈里的登记句柄。子 fiber 清理完成时只**摘除**
    /// 这条登记（不递归 dispose），父 fiber 因此不用等到自己卸载才放掉子槽位。
    parent_effect: Mutex<Option<EffectHandle>>,
    /// effect 登记闸门：关闭后不再接受新 effect；`in_flight` 让卸载等待
    /// “已经通过检查、但 setup 尚未返回”的登记完成，避免迟到的 disposer
    /// 被追加到已处置的 fiber。
    effect_gate: Mutex<EffectGate>,
    /// 异步唤醒关闭闸门的等待者；**不用 Condvar**，避免在 async worker 上阻塞。
    effect_notify: tokio::sync::Notify,
    /// 原生后台任务：完成即摘除，卸载时取消并等待。
    ///
    /// 登记闸门复用 `effect_gate`：`close_effects()` 之后不会再出现新任务，
    /// 于是"取走任务表 + 取消 + 等待"看到的是一份完整快照。
    native_tasks: Mutex<HashMap<u64, Arc<NativeTaskSlot>>>,
    native_task_sn: AtomicU64,
    /// 处置完成通知：重复 `dispose()` 都等同一次清理，而不是看到终态就早退。
    dispose_tx: tokio::sync::watch::Sender<bool>,
    /// **行内钩子**（上游 `Fiber._hooks`，`fiber.ts:111`）。
    ///
    /// `ctx.on('internal/update', …)` 的非全局注册会被内置监听器路由到这里
    /// （`events.ts:54-58`），于是它**只对本 fiber 生效**、且随 fiber 卸载自动摘除。
    hooks: Mutex<HookTable>,
    hook_sn: AtomicU64,
    desired: Mutex<Option<u64>>,
    running: Mutex<Option<u64>>,
    error: Mutex<Option<Error>>,
    /// 清理/卸载阶段收集到的结构化错误（带单调序号用于并发收集器去重）。
    cleanup_errors: Mutex<Vec<(u64, CleanupError)>>,
    /// 本子树是否启用了带等待预算的关闭路径（供宿主决定是否把同步 OP_CLEANUP / OP_DISPOSE 移交 blocking 池）。
    budgeted_dispose: AtomicBool,
    /// 每次 `load()` 递增的装载代次，防止上一代失败后台清理误清下一代的 `committed` 视图。
    load_gen: AtomicU64,
    /// 前置 `dispose_effects_bounded` 超时移交后台的恢复栈计数（仅限恢复栈，不含 drain/close 观察者）。
    effect_bg_cleanups: AtomicUsize,
    effect_bg_notify: tokio::sync::Notify,
    /// 是否已调度“后台清理归零后自动重试收敛”的任务（避免重复调度）。
    cleanup_retry_scheduled: AtomicBool,
    /// 支持范围外拓扑只告警一次，避免卸载链上重复刷屏。
    unsupported_ancestor_logged: AtomicBool,
    /// 因会形成运行时环而被拒绝的服务名；卸载等待时必须跳过这些声明，
    /// 否则“拒绝建边”仍然会在等待图上留下环。
    cycle_blocked: Mutex<HashSet<Box<str>>>,
    /// `update()` / `restart()` 递增；参与 epoch，使依赖集合不变时也能强制重载。
    revision: AtomicU64,
    /// 每次 `update_with` 调用同步取号；用来判定提交是否已被更新版本超越。
    update_rev: AtomicU64,
    /// 最后一个真正提交 config 的 `update_rev`。
    committed_rev: Mutex<u64>,
    /// 提交闸门：把“比较 update_rev + fire_commit + 赋值 + adopt”串行化。
    /// 不覆盖 `internal/update`，因此 handler 重入 update 不会死锁。
    commit_gate: tokio::sync::Mutex<()>,
    /// 正在执行的 `internal/update` chain 数（不含 replay）。
    active_chains: AtomicUsize,
    /// 有被超越的 chain 需要在所有 chain 静默后对最新配置 replay 一次。
    replay_owed: AtomicBool,
    /// 是否已经有一个 replay 任务在飞，避免重复调度。
    replay_scheduled: AtomicBool,
    /// replay 单飞锁：即使调度竞争，也只允许一个 replay 循环。
    replay_gate: tokio::sync::Mutex<()>,
    /// update 收敛通知：`when_updates_settled` 等 active/replay 全部结束。
    updates_settled: tokio::sync::Notify,
    state: AtomicU8,
    /// 收敛循环的单飞锁。
    driver: tokio::sync::Mutex<()>,
}

impl Fiber {
    /// 建根 fiber：`uid = 0`，无 runtime，永远 ACTIVE，只用于承载根上下文。
    ///
    /// 根 fiber 的强引用挂在**根 ctx 的 `root_keepalive`** 上（不是 `Store`）：
    /// `Store` 只记弱引用，因此 `Store → root_fiber → Store` 不成立；而 `App::new().root`
    /// 这种只留下 root 的用法仍能通过 `root_keepalive` 保住 fiber 身份。
    pub fn root(store: &Arc<Store>) -> Ctx {
        let ctx = Context::new_root(store.clone());
        let (dispose_tx, _) = tokio::sync::watch::channel(false);
        let fiber = Arc::new(Self {
            uid: AtomicU64::new(UID_ROOT),
            id: UID_ROOT,
            host_label: None,
            ancestor_ids: Arc::from([UID_ROOT]),
            store: store.clone(),
            parent: None,
            ctx: Mutex::new(None),
            ctx_weak: Mutex::new(Some(Arc::downgrade(&ctx))),
            config: Mutex::new(Config::Null),
            resolved: Mutex::new(Config::Null),
            volatile: Mutex::new(crate::volatile::VolatileTable::new()),
            inject: Mutex::new(Inject::default()),
            optional_inject: Mutex::new(HashSet::new()),
            runtime: None,
            deps: Mutex::new(HashMap::new()),
            // 根 fiber 的提交视图是**空表而不是 `None`**（`fiber.ts:204`
            // `this.store = Object.create(null)`）——它永远"已激活"。
            committed: Mutex::new(Some(HashMap::new())),
            disposables: Mutex::new(Vec::new()),
            effect_sn: AtomicU64::new(1),
            parent_effect: Mutex::new(None),
            effect_gate: Mutex::new(EffectGate {
                closed: false,
                in_flight: 0,
            }),
            effect_notify: tokio::sync::Notify::new(),
            native_tasks: Mutex::new(HashMap::new()),
            native_task_sn: AtomicU64::new(1),
            dispose_tx,
            hooks: Mutex::new(HashMap::new()),
            hook_sn: AtomicU64::new(1),
            desired: Mutex::new(None),
            running: Mutex::new(None),
            error: Mutex::new(None),
            cleanup_errors: Mutex::new(Vec::new()),
            budgeted_dispose: AtomicBool::new(false),
            load_gen: AtomicU64::new(0),
            effect_bg_cleanups: AtomicUsize::new(0),
            effect_bg_notify: tokio::sync::Notify::new(),
            cleanup_retry_scheduled: AtomicBool::new(false),
            unsupported_ancestor_logged: AtomicBool::new(false),
            cycle_blocked: Mutex::new(HashSet::new()),
            revision: AtomicU64::new(0),
            update_rev: AtomicU64::new(0),
            committed_rev: Mutex::new(0),
            commit_gate: tokio::sync::Mutex::new(()),
            active_chains: AtomicUsize::new(0),
            replay_owed: AtomicBool::new(false),
            replay_scheduled: AtomicBool::new(false),
            replay_gate: tokio::sync::Mutex::new(()),
            updates_settled: tokio::sync::Notify::new(),
            state: AtomicU8::new(FiberState::Active as u8),
            driver: tokio::sync::Mutex::new(()),
        });
        ctx.attach_fiber(&fiber);
        ctx.attach_root_fiber(&fiber);
        // 装载表只持**弱引用**（防 `Store → Fiber → Store` 环）。
        store.bind_root_fiber(&fiber);
        ctx
    }

    /// 建插件 fiber。注意：**这里不做依赖解析**——按 cordis 的约定，
    /// `internal/plugin` 必须先派发（loader 会在这时按配置行追加 `inject`），
    /// 之后才允许计算依赖状态。
    pub(crate) fn new(
        parent: &Ctx,
        config: Config,
        inject: Inject,
        runtime: Option<Arc<Runtime>>,
        store: &Arc<Store>,
        host_label: Option<Arc<str>>,
    ) -> Arc<Self> {
        // inject 里的非 null 值进本 fiber 自己的**声明链**（论文的 `d`，M3）：
        // 祖先优先、后代覆盖，但**排在 intercept 链（ι）之下**——提供者侧读到的
        // 合并结果右偏（context 携带的覆盖组件声明）。空值（`Config::Null`）不进链：
        // 它表示"声明了这个依赖、但没有配置"。
        let mut declare = parent.declarations();
        for (name, entry) in inject.0.iter() {
            if !entry.is_null() {
                declare = crate::scope::Scope::child(&declare, name.clone(), entry.clone());
            }
        }
        let ctx = parent.extend_with(parent.intercept.clone(), declare);
        let (dispose_tx, _) = tokio::sync::watch::channel(false);
        let id = store.next_fiber_uid();
        let mut ancestors = Vec::with_capacity(4);
        ancestors.push(id);
        if let Some(parent_fiber) = parent.fiber_opt() {
            ancestors.extend_from_slice(&parent_fiber.ancestor_ids);
        } else {
            ancestors.push(UID_ROOT);
        }
        let fiber = Arc::new(Self {
            uid: AtomicU64::new(id),
            id,
            host_label,
            ancestor_ids: Arc::from(ancestors),
            store: store.clone(),
            parent: Some(parent.clone()),
            ctx: Mutex::new(None),
            ctx_weak: Mutex::new(None),
            config: Mutex::new(config),
            resolved: Mutex::new(Config::Null),
            volatile: Mutex::new(crate::volatile::VolatileTable::new()),
            inject: Mutex::new(inject),
            optional_inject: Mutex::new(HashSet::new()),
            runtime: runtime.clone(),
            deps: Mutex::new(HashMap::new()),
            // 新 fiber 尚未激活：`load` 时才重拍（`fiber.ts:421` 的等价物）。
            committed: Mutex::new(None),
            disposables: Mutex::new(Vec::new()),
            effect_sn: AtomicU64::new(1),
            parent_effect: Mutex::new(None),
            effect_gate: Mutex::new(EffectGate {
                closed: false,
                in_flight: 0,
            }),
            effect_notify: tokio::sync::Notify::new(),
            native_tasks: Mutex::new(HashMap::new()),
            native_task_sn: AtomicU64::new(1),
            dispose_tx,
            hooks: Mutex::new(HashMap::new()),
            hook_sn: AtomicU64::new(1),
            desired: Mutex::new(None),
            running: Mutex::new(None),
            error: Mutex::new(None),
            cleanup_errors: Mutex::new(Vec::new()),
            budgeted_dispose: AtomicBool::new(false),
            load_gen: AtomicU64::new(0),
            effect_bg_cleanups: AtomicUsize::new(0),
            effect_bg_notify: tokio::sync::Notify::new(),
            cleanup_retry_scheduled: AtomicBool::new(false),
            unsupported_ancestor_logged: AtomicBool::new(false),
            cycle_blocked: Mutex::new(HashSet::new()),
            revision: AtomicU64::new(0),
            update_rev: AtomicU64::new(0),
            committed_rev: Mutex::new(0),
            commit_gate: tokio::sync::Mutex::new(()),
            active_chains: AtomicUsize::new(0),
            replay_owed: AtomicBool::new(false),
            replay_scheduled: AtomicBool::new(false),
            replay_gate: tokio::sync::Mutex::new(()),
            updates_settled: tokio::sync::Notify::new(),
            state: AtomicU8::new(FiberState::Pending as u8),
            driver: tokio::sync::Mutex::new(()),
        });
        // 双向绑定：Context 指向 Fiber（语义上由 dispose 打破），Fiber 指向自己的 Context。
        ctx.attach_fiber(&fiber);
        *fiber.ctx.lock().unwrap() = Some(ctx.clone());

        if let Some(runtime) = &runtime {
            // 关键：**插件的生命周期本身就是父 fiber 的一个 effect**。
            // 这一行同时买到两件事：① 父卸载 → 子递归卸载；
            // ② 父子卸载顺序自动满足 LIFO（对应 JS 的 `fiber.ts:171-199`）。
            if let Some(parent_fiber) = parent.fiber_opt() {
                // 只持 **Weak**：子 fiber 的存活不应由父 effect 栈包办。运行时
                // 注册表在子 fiber 卸载前一直强持它，所以递归处置仍然成立；
                // 而子 fiber 已处置后，这里不会把对象继续钉在父 fiber 上。
                let this = Arc::downgrade(&fiber);
                let parent_weak = Arc::downgrade(&parent_fiber);
                // 竞态保护：`try_plugin` 检查之后父 fiber 仍可能进入 UNLOADING。
                // 此时不要 panic（那会跨 ABI 边界 abort 进程），而是让返回的
                // fiber 以 FAILED 收敛，调用方 settle() 时拿到可读错误。
                match parent_fiber.try_effect_registered(move |_| {
                    Disposable::new(move || {
                        let this = this.upgrade();
                        let parent_budgeted = parent_weak
                            .upgrade()
                            .is_some_and(|parent| parent.budgeted_dispose.load(Ordering::SeqCst));
                        Box::pin(async move {
                            if let Some(this) = this {
                                if parent_budgeted {
                                    this.mark_budgeted_dispose();
                                }
                                this.dispose().await;
                            }
                        })
                    })
                }) {
                    Ok(registered) => {
                        *fiber.parent_effect.lock().unwrap() = Some(registered);
                    }
                    Err(err) => {
                        fiber.mark_effects_closed();
                        *fiber.error.lock().unwrap() = Some(err);
                        fiber.set_state(FiberState::Failed);
                        return fiber;
                    }
                }
            }
            store.register_fiber(runtime, &fiber);
            store.fire_plugin(&fiber);
            // ↑ 派发之后 inject 可能已被 loader 追加过，这时候才能算依赖。
            for name in fiber.inject_names() {
                fiber.check_impl(&name);
            }
            fiber.refresh_desired();
        }
        fiber
    }

    // ------------------------------------------------------------- 基本访问

    pub fn uid(&self) -> u64 {
        self.uid.load(Ordering::SeqCst)
    }

    /// 宿主/装载器在挂载时为该 fiber 绑定的不可变行标（非装载器挂载时为 `None`）。
    pub fn host_label(&self) -> Option<&str> {
        self.host_label.as_deref()
    }

    pub fn state(&self) -> FiberState {
        FiberState::from_u8(self.state.load(Ordering::SeqCst))
    }

    pub fn is_disposed(&self) -> bool {
        self.uid() == UID_DISPOSED
    }

    pub fn error(&self) -> Option<String> {
        self.error.lock().unwrap().as_ref().map(|e| e.to_string())
    }

    /// 清理/卸载阶段摘要错误的可读消息（兼容 0.1 既有入口；全量结构化错误见 [`Fiber::cleanup_errors`]）。
    ///
    /// 若发生清理监督者自身异常（[`CleanupErrorKind::InternalCleanup`]），优先返回该异常，
    /// 否则返回首条清理错误。
    pub fn cleanup_error(&self) -> Option<String> {
        let errors = self.cleanup_errors.lock().unwrap();
        errors
            .iter()
            .find(|(_, err)| err.kind == CleanupErrorKind::InternalCleanup)
            .or_else(|| errors.first())
            .map(|(_, err)| err.message.clone())
    }

    /// 清理/卸载阶段记录到的全部结构化错误。
    pub fn cleanup_errors(&self) -> Vec<CleanupError> {
        self.cleanup_errors
            .lock()
            .unwrap()
            .iter()
            .map(|(_, err)| err.clone())
            .collect()
    }

    fn cleanup_errors_with_seq(&self) -> Vec<(u64, CleanupError)> {
        self.cleanup_errors.lock().unwrap().clone()
    }

    /// 记录一条结构化清理错误，并向覆盖本 fiber 的活动关闭报告收集器广播。
    ///
    /// 宿主载体（如 T2 `OP_CLEANUP` / `OP_DISPOSE`）可通过本入口把清理失败汇总到
    /// [`Fiber::cleanup_errors`] 与 [`CloseReport::errors`] 中，避免只打日志而丢给调用方。
    pub fn record_cleanup_error(&self, kind: CleanupErrorKind, message: impl Into<String>) {
        let _ = self.record_cleanup_error_with_seq(kind, message);
    }

    fn record_cleanup_error_with_seq(
        &self,
        kind: CleanupErrorKind,
        message: impl Into<String>,
    ) -> (u64, CleanupError) {
        let seq = self.store.next_cleanup_error_seq();
        let entry = CleanupError {
            fiber: self.name(),
            uid: self.id,
            kind,
            message: message.into(),
        };
        self.cleanup_errors
            .lock()
            .unwrap()
            .push((seq, entry.clone()));
        self.store
            .broadcast_cleanup_error(&self.ancestor_ids, seq, &entry);
        (seq, entry)
    }

    /// 登记一条与本 fiber 关联的后台延后清理任务（例如排水超时后继续等待原生调用退出并保活动态库）。
    pub fn defer_background_cleanup(&self) -> BackgroundCleanupGuard {
        let latest_timeout = self
            .cleanup_errors
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|(_, err)| err.kind == CleanupErrorKind::Timeout)
            .cloned();
        self.store.register_background_cleanup(
            self.ancestor_ids.clone(),
            self.name(),
            self.id,
            latest_timeout,
        )
    }

    pub(crate) fn defer_background_cleanup_without_error(&self) -> BackgroundCleanupGuard {
        self.store.register_background_cleanup(
            self.ancestor_ids.clone(),
            self.name(),
            self.id,
            None,
        )
    }

    /// 本 fiber 及其后代子树当前仍在后台进行的延后清理任务数。
    pub fn pending_background_cleanups(&self) -> usize {
        self.store.pending_background_cleanups_under(self.id)
    }

    /// 等待本 fiber 及其后代子树全部后台延后清理任务完成。
    pub async fn wait_background_cleanups(&self) {
        self.store.wait_background_cleanups_under(self.id).await;
    }

    /// 标记本 fiber 正在按带等待预算的路径处置（供宿主把同步 `OP_CLEANUP`/`OP_DISPOSE` 移交阻塞池）。
    pub(crate) fn mark_budgeted_dispose(&self) {
        self.budgeted_dispose.store(true, Ordering::SeqCst);
    }

    /// 宿主是否应把同步卸载/清理调用（`OP_CLEANUP`/`OP_DISPOSE`）移交 blocking 线程池以保证等待预算可触发。
    pub fn should_offload_dispose(&self) -> bool {
        if self.store.lifecycle_policy().dispose_timeout_ms.is_some()
            || self.budgeted_dispose.load(Ordering::SeqCst)
        {
            return true;
        }
        let mut cursor = self.parent_ctx();
        while let Some(ctx) = cursor {
            if let Some(parent) = ctx.fiber_opt() {
                if parent.budgeted_dispose.load(Ordering::SeqCst) {
                    return true;
                }
                cursor = parent.parent_ctx();
            } else {
                cursor = ctx.parent.clone();
            }
        }
        false
    }

    /// 当前**raw** 配置（校验前的那份）。
    ///
    /// `update_with` 会在派发 `internal/update` **之前**把它换成新值
    /// （上游 `fiber.ts:492`），所以钩子在派发期间读到的已经是新配置。
    pub fn config(&self) -> Config {
        self.config.lock().unwrap().clone()
    }

    /// 当前**已解析**的配置（`internal/config` + 校验器之后的那份）。
    ///
    /// 诊断价值：它是插件"实际拿到"的东西，与 [`Fiber::config`] 的差别就是
    /// `!!js`、默认值与 `internal/config` 监听者改动的总和。
    pub fn resolved(&self) -> Config {
        self.resolved.lock().unwrap().clone()
    }

    /// 这个 fiber 的插件声明的 volatile 路径（M4-5；空 = 不参与 volatile）。
    pub fn volatile_paths(&self) -> Vec<String> {
        self.runtime
            .as_ref()
            .map(|runtime| runtime.plugin.volatile_paths())
            .unwrap_or_default()
    }

    /// 取一条 volatile 引用（没有这个路径就返回 `None`）。
    ///
    /// 插件侧通过 [`Ctx::volatile`] 读它；装载器侧用它做提交。
    pub fn volatile_ref(&self, path: &str) -> Option<Arc<crate::VolatileRef>> {
        self.volatile.lock().unwrap().get(path).cloned()
    }

    /// 把新 raw 换上去（**不发** commit/update 事件）。
    ///
    /// volatile 提交用它推进"下次激活用的 raw"（上游 `fiber._config = …`）：
    /// 这不是一次更新，是一次原地提交。
    pub fn set_raw_config(&self, config: Config) {
        *self.config.lock().unwrap() = config;
    }

    /// 一次 **volatile 原地提交**（M4-5）：已解析配置推进到候选，逐路径写引用。
    ///
    /// 返回真正**变化**的路径（值没变的路径不进事件载荷，上游同样只报变化的）。
    /// 调用方（装载器）负责：候选的合法性、前置条件、以及把变化推给 ABI 侧。
    pub fn commit_volatile(self: &Arc<Self>, candidate: Config) -> Vec<String> {
        *self.resolved.lock().unwrap() = candidate.clone();
        let table: Vec<(String, Arc<crate::VolatileRef>)> = self
            .volatile
            .lock()
            .unwrap()
            .iter()
            .map(|(path, reference)| (path.to_string(), reference.clone()))
            .collect();
        let mut changed = Vec::new();
        for (path, reference) in table {
            let Some(value) = crate::path_value(&candidate, &crate::split_path(&path)) else {
                // 候选里没有这个路径 ⇒ 视作"没有新值可提交"（上游也只收集已存在的引用）。
                continue;
            };
            if reference.get() != *value {
                reference.commit(value.clone());
                changed.push(path);
            }
        }
        changed
    }

    /// 把 raw 解析成**候选**（不改动任何状态；volatile 提交前的那一步）。
    ///
    /// 对应上游 `_commitVolatile` 里的
    /// `resolveConfig(fiber.runtime, fiber.ctx.waterfall(fiber, 'internal/config', raw))`：
    /// 走一遍完整的解析链（`!!js` 会跟着**当前上下文**再算一次），失败即"候选无效"。
    pub async fn resolve_raw(self: &Arc<Self>, raw: &Config) -> Result<Config> {
        let Some(runtime) = self.runtime.clone() else {
            return Err("fiber 没有 runtime：无法解析配置".into());
        };
        self.store
            .resolve_config(&self.ctx(), &runtime, raw.clone())
            .await
    }

    /// 记账：已解析配置 + volatile 引用表（每次装载重建，对应上游"校验期创建引用"）。
    fn adopt_resolved(self: &Arc<Self>, resolved: &Config, runtime: &Arc<Runtime>) {
        *self.resolved.lock().unwrap() = resolved.clone();
        let paths = runtime.plugin.volatile_paths();
        if let Err(issue) = crate::validate_volatile_paths(&paths) {
            // 声明本身不合法：不建任何引用 ⇒ 这个插件退化成"配置一变就重挂"。
            // 之所以是日志而不是装载失败：它是**能力声明**的问题，不是这一行配置的问题。
            self.store.log(Level::Error, &self.name(), &issue);
            self.volatile.lock().unwrap().clear();
            return;
        }
        let mut table = crate::volatile::VolatileTable::new();
        for (path, value) in crate::values_at(resolved, &paths) {
            table.insert(path.into_boxed_str(), crate::VolatileRef::new(value));
        }
        *self.volatile.lock().unwrap() = table;
    }

    /// 当前**正在运行**的那个 epoch。
    ///
    /// 诊断价值很高：它把"提供者重启 → 依赖者重载"这条看不见的因果链变成可断言的值。
    /// 因为 epoch 里混入了每个依赖实现的提供者 uid，所以提供者换了一代，
    /// 所有依赖者的 `running_epoch` 必然变化。
    pub fn running_epoch(&self) -> Option<u64> {
        *self.running.lock().unwrap()
    }

    /// 期望的 epoch（`None` = 依赖未齐，处于未激活）。
    pub fn desired_epoch(&self) -> Option<u64> {
        *self.desired.lock().unwrap()
    }

    pub fn ctx(&self) -> Ctx {
        self.ctx_opt()
            .expect("fiber is disposed: its context is gone")
    }

    /// 当前未满足的**必需** inject 及原因。
    ///
    /// 这是只读诊断：不建依赖边、不分配 realm、不触发插件代码。`related` 来自
    /// 全局注册表，只用于提示“可能是不是版本写错了”，不证明它在当前 isolate 域可见。
    ///
    /// 它不是原子快照：`inject` / `deps` 与注册表查询之间没有全局锁。并发
    /// `provide` / 卸载时，返回值只代表查询过程中观察到的状态。
    pub fn pending_injects(&self) -> Vec<PendingInject> {
        let Some(ctx) = self.ctx_opt() else {
            return Vec::new();
        };
        // 先在锁内收集缺失 key，再放锁做注册表查询：`Store` 的锁与
        // `inject`/`deps` 没有固定顺序，不能持着后者去拿前者。
        let mut missing: Vec<String> = {
            let inject = self.inject.lock().unwrap();
            let deps = self.deps.lock().unwrap();
            inject
                .0
                .iter()
                .filter(|(name, _)| !deps.contains_key(name.as_ref()))
                .map(|(name, _)| name.to_string())
                .collect()
        };
        // `Inject` 是 Vec，允许重复声明；诊断按 key 一条即可，避免同一行刷两遍。
        missing.sort();
        missing.dedup();
        let mut pending = Vec::new();
        for name in missing {
            let related = self.store.related_service_keys(&name);
            let visible = self.store.visible_keys(&ctx, &related);
            let reason = if self.is_cycle_blocked(&name) {
                PendingReason::Cycle
            } else if self.store.lookup_visible(&ctx, &name).is_some() {
                // 精确 key 可见，但依赖边还没建立：提供者未 Active 或 check 为 false。
                // 两者对调用方的结论相同，统一报“提供者未就绪”。
                PendingReason::ProviderNotReady
            } else if related.iter().any(|key| key == &name) {
                // 全局有精确 key，但当前 isolate 看不见它；这比“完全没有”更具体。
                PendingReason::RealmMismatch
            } else if visible.iter().any(|key| key != &name) {
                // 只有**当前域可见**的同 base 版本才算版本不匹配；别的域里的候选
                // 只能作为 `related` 提示，不能拿来推断原因。
                PendingReason::VersionMismatch
            } else {
                PendingReason::Missing
            };
            pending.push(PendingInject {
                key: name,
                reason,
                related,
            });
        }
        pending
    }

    pub fn ctx_opt(&self) -> Option<Ctx> {
        let strong = self.ctx.lock().unwrap().clone();
        if strong.is_some() {
            return strong;
        }
        self.ctx_weak
            .lock()
            .unwrap()
            .as_ref()
            .and_then(Weak::upgrade)
    }

    pub fn parent_ctx(&self) -> Option<Ctx> {
        self.parent.clone()
    }

    /// `self` 是否是 `other` 的祖先（含父链上的任意一层）。
    ///
    /// provide 的 disposer 用它避免"等待一个正在卸载自己的祖先"：祖先已经
    /// 持有 driver 在等子 fiber 拆除，再回头等祖先的 `settle()` 就是死锁。
    pub fn is_ancestor_of(&self, other: &Arc<Fiber>) -> bool {
        let mut cursor = other.parent_ctx().and_then(|ctx| ctx.fiber_opt());
        while let Some(fiber) = cursor {
            if std::ptr::eq(fiber.as_ref(), self) {
                return true;
            }
            cursor = fiber.parent_ctx().and_then(|ctx| ctx.fiber_opt());
        }
        false
    }

    /// 明确记录“祖先消费者”这一支持范围外拓扑；每个提供者只告警一次。
    pub(crate) fn warn_ancestor_consumer(&self, consumer: &Arc<Fiber>) {
        if self
            .unsupported_ancestor_logged
            .swap(true, Ordering::SeqCst)
        {
            return;
        }
        self.store.log(
            Level::Warn,
            &self.name(),
            &format!(
                "生命周期支持范围外：提供者 <{}#{}> 的消费者 <{}#{}> 是它的祖先，跳过等待；\
                 提供者资源可能早于该消费者剩余的恢复栈关闭",
                self.name(),
                self.uid(),
                consumer.name(),
                consumer.uid()
            ),
        );
    }

    /// 最近的具名祖先，否则 `"root"`（对应 JS 的 `get name()`）。
    pub fn name(&self) -> String {
        if let Some(runtime) = &self.runtime {
            return runtime.name.to_string();
        }
        let mut cursor = self.parent_ctx();
        while let Some(ctx) = cursor {
            match ctx.fiber_opt() {
                Some(fiber) => {
                    if let Some(runtime) = &fiber.runtime {
                        return runtime.name.to_string();
                    }
                    cursor = fiber.parent_ctx();
                }
                None => cursor = ctx.parent.clone(),
            }
        }
        "root".to_string()
    }

    pub fn assert_active(&self) {
        assert!(
            !self.is_disposed(),
            "cannot create effect on inactive context"
        );
    }

    // --------------------------------------------------------------- 依赖表

    pub fn dep(&self, name: &str) -> Option<Arc<Impl>> {
        self.deps.lock().unwrap().get(name).cloned()
    }

    pub(crate) fn set_dep(&self, name: Box<str>, impl_: Arc<Impl>) {
        let _graph = self.store.dependency_lock.lock().unwrap();
        self.deps.lock().unwrap().insert(name, impl_);
    }

    pub(crate) fn remove_dep(&self, name: &str) {
        self.remove_dependency(name);
        self.refresh_desired();
    }

    fn remove_dependency(&self, name: &str) {
        let _graph = self.store.dependency_lock.lock().unwrap();
        self.deps.lock().unwrap().remove(name);
    }

    /// `self` 是否可以沿已解析的依赖边走到 `target`。
    ///
    /// 调用方必须持有 `Store::dependency_lock`，否则并发插入会让两边都看不到对方。
    fn dependency_reaches(self: &Arc<Self>, target: *const Fiber) -> bool {
        let mut stack = vec![self.clone()];
        let mut seen = HashSet::new();
        while let Some(fiber) = stack.pop() {
            if Arc::as_ptr(&fiber) == target {
                return true;
            }
            if !seen.insert(Arc::as_ptr(&fiber) as usize) {
                continue;
            }
            let deps: Vec<Arc<Impl>> = fiber.deps.lock().unwrap().values().cloned().collect();
            for dep in deps {
                if let Some(owner) = dep.fiber.upgrade() {
                    stack.push(owner);
                }
            }
        }
        false
    }

    pub(crate) fn is_cycle_blocked(&self, name: &str) -> bool {
        self.cycle_blocked.lock().unwrap().contains(name)
    }

    /// 记录一条因成环而被拒绝的依赖；返回 `true` 表示这是第一次记录。
    fn block_cycle(&self, name: &str) -> bool {
        self.cycle_blocked.lock().unwrap().insert(name.into())
    }

    fn clear_cycle_blocked(&self, name: &str) {
        self.cycle_blocked.lock().unwrap().remove(name);
    }

    /// 在全局依赖图锁内插入一条依赖边；若会形成环则拒绝。
    pub(crate) fn insert_dependency(&self, name: &str, impl_: Arc<Impl>) -> bool {
        let _graph = self.store.dependency_lock.lock().unwrap();
        if let Some(existing) = self.deps.lock().unwrap().get(name) {
            if Arc::ptr_eq(existing, &impl_) {
                return true;
            }
        }
        if let Some(provider) = impl_.fiber.upgrade() {
            if !std::ptr::eq(provider.as_ref(), self)
                && provider.dependency_reaches(std::ptr::from_ref(self))
            {
                self.deps.lock().unwrap().remove(name);
                return false;
            }
        }
        self.deps.lock().unwrap().insert(name.into(), impl_);
        true
    }

    /// 从**提交视图**里按 `(name, realm)` 读一个依赖。属性访问走这条路，而不是 `deps`。
    ///
    /// 这是论文 §5.1.4 Algorithm 6 的第一条分支（`若 key ∈ fiber.committed → 返回`），
    /// 也是 `reflect.ts:86` 那一行。**读视图而不是读表，正是 Theorem 63 成立的前提**：
    /// 它让"因为某个依赖消失而触发拆除"的组件，在拆完之前仍能读到那个依赖。
    ///
    /// 域必须参与查找：同名服务可以在不同 isolate 域各有一份实现，取错域等于
    /// 把别的 agent 的实现交给调用方。
    pub fn committed(&self, name: &str, realm: RealmId) -> Option<Arc<Impl>> {
        self.committed
            .lock()
            .unwrap()
            .as_ref()?
            .get(&realm)?
            .get(name)
            .cloned()
    }

    /// 把自己刚提供的服务写进提交视图（`reflect.ts:193`）。
    ///
    /// `committed` 为 `None`（未激活）时是空操作——与 JS 在 `store` 未定义时不写同理。
    pub(crate) fn committed_insert(&self, name: Box<str>, impl_: Arc<Impl>) {
        if let Some(map) = self.committed.lock().unwrap().as_mut() {
            map.entry(impl_.realm).or_default().insert(name, impl_);
        }
    }

    /// 从提交视图里摘掉一条（`reflect.ts:202`，在依赖者全部拆完之后才调用）。
    pub(crate) fn committed_remove(&self, name: &str, realm: RealmId) {
        if let Some(map) = self.committed.lock().unwrap().as_mut() {
            if let Some(realm_map) = map.get_mut(&realm) {
                realm_map.remove(name);
                if realm_map.is_empty() {
                    map.remove(&realm);
                }
            }
        }
    }

    /// 重拍提交视图：把当前的实时解析表按域分组冻结一份（`fiber.ts:421`）。
    ///
    /// 两把锁**不嵌套**（花括号让 `deps` 的守卫在下一句之前就释放），所以不存在锁序问题。
    fn resnapshot(&self) {
        let snapshot = { self.deps.lock().unwrap().clone() };
        let mut view: CommittedView = HashMap::new();
        for (name, impl_) in snapshot {
            view.entry(impl_.realm).or_default().insert(name, impl_);
        }
        *self.committed.lock().unwrap() = Some(view);
    }

    /// 是否声明了**必需**依赖。依赖解析、通知与激活判定只认这一种。
    pub fn declares_inject(&self, name: &str) -> bool {
        self.inject.lock().unwrap().contains(name)
    }

    /// 是否声明了可选依赖。
    pub fn declares_optional_inject(&self, name: &str) -> bool {
        self.optional_inject.lock().unwrap().contains(name)
    }

    /// 追加一条**必需**依赖声明。
    ///
    /// **只允许在 `internal/plugin` 观察者里调用**：依赖解析在那之后立刻发生，
    /// 之后再加不会重算。这正是 loader 行级 `inject` 的落点
    /// （`vendor/loader/src/index.ts:129-135`），也是为什么不把 `internal/plugin`
    /// 做成异步事件。
    pub fn add_inject(&self, name: &str) {
        let mut inject = self.inject.lock().unwrap();
        if !inject.contains(name) {
            inject.0.push((name.into(), Config::Null));
        }
    }

    pub fn inject_names(&self) -> Vec<String> {
        self.inject
            .lock()
            .unwrap()
            .0
            .iter()
            .map(|(n, _)| n.to_string())
            .collect()
    }

    /// 追加一条**可选**依赖声明；不参与激活判定。
    pub fn add_optional_inject(&self, name: &str) {
        self.optional_inject.lock().unwrap().insert(name.into());
    }

    pub fn optional_inject_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .optional_inject
            .lock()
            .unwrap()
            .iter()
            .map(|name| name.to_string())
            .collect();
        names.sort();
        names
    }

    /// 按当前注册表重新求值一个依赖。服务消失或 `check` 谓词变假都会把它从依赖表里摘掉。
    pub fn check_impl(&self, name: &str) {
        let Some(ctx) = self.ctx_opt() else { return };
        let realm = self.store.realm_of(&ctx, name);
        let impl_ = self.store.lookup(realm);
        let keep = match &impl_ {
            Some(impl_) => {
                let ready = impl_
                    .fiber
                    .upgrade()
                    .map(|f| f.state() == FiberState::Active)
                    .unwrap_or(false);
                if !ready {
                    false
                } else {
                    match impl_.check.clone() {
                        Some(check) => {
                            let value = impl_.get();
                            check(&value)
                        }
                        None => true,
                    }
                }
            }
            None => false,
        };
        if keep {
            let impl_ = impl_.expect("keep implies a resolved implementation");
            if !self.insert_dependency(name, impl_.clone()) {
                if self.block_cycle(name) {
                    let provider = impl_
                        .fiber
                        .upgrade()
                        .map(|fiber| format!("<{}#{}>", fiber.name(), fiber.uid()))
                        .unwrap_or_else(|| "<已卸载的提供者>".to_string());
                    self.store.log(
                        Level::Error,
                        &self.name(),
                        &format!(
                            "检测到运行时服务依赖环：消费者 <{}#{}> 依赖 \"{name}\"（提供者 {provider}）\
                             会形成环，已拒绝这条依赖；消费者保持未满足",
                            self.name(),
                            self.uid()
                        ),
                    );
                }
            } else {
                self.clear_cycle_blocked(name);
            }
        } else {
            self.remove_dependency(name);
            // 注意：这里**不能**清 `cycle_blocked`。卸载等待图仍要跳过被拒绝的
            // 声明，否则会回到“拒绝建边但等待链仍成环”的死锁（见 runtime_cycles）。
            // 代价是提供者消失后诊断可能短暂保留 `cycle`，属于已知的诊断边界。
        }
    }

    /// 重算期望的 epoch。
    ///
    /// epoch 由 `(revision, 每个依赖实现的提供者 uid)` 混合而成——**关键点在这里**：
    /// 提供者一旦重启，uid 变化 → 所有依赖方 epoch 变化 → 自动卸载重载。
    /// 这就是"加载顺序由服务可用性驱动，而不是手动编排"的全部实现。
    ///
    /// # 不变量：处置是终态
    /// 已处置的 fiber 永远不能再次变成"期望装载"。JS 里这条不变量是**隐式**成立的
    /// ——dispose 会把自己从 `runtime.fibers` 摘除，于是 `notify()` 再也碰不到它，
    /// 它的 `_refresh()` 不会被调用。Rust 这里在清理完成前仍保留登记（提供者卸载
    /// 要能找到"已处置但恢复栈还在跑"的消费者），所以必须**显式**挡住：服务提供者
    /// 的 disposer 末尾会调用自己的 `remove_dep()`，那会触发一次 `refresh_desired()`，
    /// 如果不挡住，就会出现"已 dispose 的提供者被复活、把服务重新注册回来、
    /// 依赖者跟着复活"的幽灵重载。
    pub fn refresh_desired(&self) {
        if self.is_disposed() {
            *self.desired.lock().unwrap() = None;
            return;
        }
        let inject = self.inject.lock().unwrap();
        let deps = self.deps.lock().unwrap();
        let mut hash = self
            .revision
            .load(Ordering::SeqCst)
            .wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let mut complete = true;
        for (name, _) in inject.0.iter() {
            match deps.get(name) {
                Some(impl_) => {
                    hash = crate::mix(hash, impl_.provider_uid());
                    hash = crate::mix(hash, crate::fnv(name.as_bytes()));
                }
                None => {
                    complete = false;
                    break;
                }
            }
        }
        *self.desired.lock().unwrap() = if complete { Some(hash) } else { None };
    }

    // ------------------------------------------------------------- effect

    /// 注册一个可逆副作用。返回值是**幂等句柄**：手动调用即提前释放，
    /// fiber 卸载时也会自动逆序释放；谁先跑到谁生效（共享同一个 cell）。
    pub fn effect<F>(self: &Arc<Self>, setup: F) -> Disposable
    where
        F: FnOnce(Ctx) -> Disposable + Send + 'static,
    {
        // `UNLOADING` 期间拒绝创建 effect（上游 vendor 补丁 #6 加固过这一点）。
        let state = self.state();
        assert!(
            state != FiberState::Disposed && state != FiberState::Unloading,
            "cannot create effect on inactive context"
        );
        // 闸门与状态检查/登记同步：卸载会先关闭闸门并等待所有 in-flight setup
        // 返回，迟到的 disposer 因此不可能被追加到已经取走的恢复栈。
        let _registration = self.begin_effect().unwrap_or_else(|err| panic!("{err}"));
        let ctx = self
            .ctx_opt()
            .expect("fiber is disposed: its context is gone");
        self.track_effect(setup(ctx)).disposable
    }

    /// [`Fiber::effect`] 的**可失败**版本：`setup` 返回 `Err` 时**不留任何登记**。
    ///
    /// 用于"先登记、再校验"的路径（[`crate::Store::provide`]）：校验失败时恢复栈
    /// 必须原样不变，否则每次失败都会留下一条永远不会执行的空槽，而调用方拿不到
    /// 句柄也就无从撤销——长期运行里重复注册/重试会让恢复栈线性增长。
    ///
    /// 与 [`Fiber::try_effect`] 的分工：那个把 setup 的 panic 收敛成 `Err`（跨 ABI
    /// 边界用），这个让 setup 自己报告业务失败（原生代码用，panic 照旧放出去）。
    pub(crate) fn effect_result<F>(self: &Arc<Self>, setup: F) -> Result<Disposable>
    where
        F: FnOnce(Ctx) -> Result<Disposable> + Send + 'static,
    {
        let state = self.state();
        assert!(
            state != FiberState::Disposed && state != FiberState::Unloading,
            "cannot create effect on inactive context"
        );
        let _registration = self.begin_effect().unwrap_or_else(|err| panic!("{err}"));
        let ctx = self
            .ctx_opt()
            .expect("fiber is disposed: its context is gone");
        Ok(self.track_effect(setup(ctx)?).disposable)
    }

    /// [`Fiber::effect`] 的**可失败**版本：ABI 边界专用（M5-1）。
    ///
    /// 与 `effect` 的两点差别都是"不能 panic 的边界"逼出来的：
    /// 1. 状态不对 / 上下文已释放 ⇒ `Err`，而不是 assert；
    /// 2. `setup` 自己 panic ⇒ `Err`，而不是把 panic 放出去——
    ///    在 `extern "C"` 宿主函数里，逃出去的 panic 等于 abort 整个进程。
    ///
    /// 这不是 `effect` 的替代品：原生插件该 panic 还是 panic（错误早暴露、
    /// 栈里有调用者），只有跨越 ABI 边界的那一侧必须收敛成一条可读错误。
    pub fn try_effect<F>(self: &Arc<Self>, setup: F) -> Result<Disposable>
    where
        F: FnOnce(Ctx) -> Disposable + Send + 'static,
    {
        self.try_effect_registered(setup)
            .map(|handle| handle.disposable)
    }

    /// [`Fiber::try_effect`] 的登记版本：除公开句柄外还返回可摘除的内部登记。
    fn try_effect_registered<F>(self: &Arc<Self>, setup: F) -> Result<EffectHandle>
    where
        F: FnOnce(Ctx) -> Disposable + Send + 'static,
    {
        if matches!(self.state(), FiberState::Disposed | FiberState::Unloading) {
            return Err("cannot create effect on inactive context".into());
        }
        let _registration = self.begin_effect()?;
        let Some(ctx) = self.ctx_opt() else {
            return Err("fiber is disposed: its context is gone".into());
        };
        let inner = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| setup(ctx))).map_err(
            |payload| {
                format!(
                    "插件在 effect 建立期 panic: {}",
                    crate::panic_message(&*payload)
                )
            },
        )?;
        Ok(self.track_effect(inner))
    }

    fn begin_effect(&self) -> Result<EffectRegistration<'_>> {
        let mut gate = self.effect_gate.lock().unwrap();
        if gate.closed {
            return Err("cannot create effect on inactive context".into());
        }
        gate.in_flight += 1;
        Ok(EffectRegistration { fiber: self })
    }

    /// 同步关闭 effect 闸门；关闭后 `begin_effect` 一律拒绝新登记。
    fn mark_effects_closed(&self) {
        self.effect_gate.lock().unwrap().closed = true;
    }

    /// 关闭 effect 闸门并等待所有**已经通过闸门**的 setup 完成。
    ///
    /// 调用返回后，`disposables` 里不会再有迟到的登记；接下来的 drain 是完整的。
    /// 等待走 [`tokio::sync::Notify`]，不会阻塞 async worker——同步 setup 本身
    /// 仍按公开契约在调用线程上执行。
    async fn close_effects(&self) {
        self.mark_effects_closed();
        if self.effect_gate.lock().unwrap().in_flight == 0 {
            return;
        }
        loop {
            // 先注册通知再复查，避免在“检查为 0”和“开始等待”之间丢唤醒。
            let notified = self.effect_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.effect_gate.lock().unwrap().in_flight == 0 {
                return;
            }
            notified.await;
        }
    }

    fn open_effects(&self) -> bool {
        let mut gate = self.effect_gate.lock().unwrap();
        if self.is_disposed() {
            return false;
        }
        gate.closed = false;
        true
    }

    /// 把建好的 disposer 挂进本 fiber 的**逆序**清单（`effect`/`try_effect` 的共同落点）。
    fn track_effect(self: &Arc<Self>, inner: Disposable) -> EffectHandle {
        let cell: Arc<Mutex<Option<Disposable>>> = Arc::new(Mutex::new(Some(inner)));
        let id = self.effect_sn.fetch_add(1, Ordering::SeqCst);
        self.disposables.lock().unwrap().push(EffectEntry {
            id,
            cell: cell.clone(),
        });
        EffectHandle {
            disposable: effect_handle(Arc::downgrade(self), id, cell),
            owner: Arc::downgrade(self),
            id,
        }
    }

    /// 从恢复栈里摘掉一条登记。手动释放的常见形态是"最后注册、先释放"，
    /// 因此先试 O(1) 的队尾弹出；乱序释放才退化为线性扫描。
    fn remove_effect(&self, id: u64) {
        let mut list = self.disposables.lock().unwrap();
        if list.last().is_some_and(|entry| entry.id == id) {
            list.pop();
        } else {
            list.retain(|entry| entry.id != id);
        }
    }

    pub fn effect_count(&self) -> usize {
        self.disposables.lock().unwrap().len()
    }

    /// 当前仍由本 fiber 跟踪的在飞受管后台任务数（完成即自动摘除）。
    pub fn scoped_task_count(&self) -> usize {
        self.native_tasks.lock().unwrap().len()
    }

    /// 在本 fiber 的生命周期内启动一条受管后台任务。
    ///
    /// - fiber 卸载、装载失败、重载或 `App::close()` 时会先关闭闸门，再取消全部任务并等待它们退出；
    /// - 任务结束（含 panic 或未首次 poll 即被丢弃）时自行从表里摘除，长期运行的 fiber 不会线性累积；
    /// - 遵循 ADR 0011：自动继承当前宿主调用作用域的 `current_correlation_id()`；若还需优先
    ///   继承某个 `Ctx` 上绑定的 `with_correlation`，请使用 [`Context::spawn_scoped`](crate::Context::spawn_scoped)。
    pub fn spawn_scoped<F, Fut>(
        self: &Arc<Self>,
        task: F,
    ) -> std::result::Result<CancelToken, SpawnScopedError>
    where
        F: FnOnce(CancelToken) -> Fut,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        self.spawn_scoped_inner(crate::trace::current_correlation_id(), task)
    }

    pub(crate) fn spawn_scoped_inner<F, Fut>(
        self: &Arc<Self>,
        correlation_id: Option<Arc<str>>,
        task: F,
    ) -> std::result::Result<CancelToken, SpawnScopedError>
    where
        F: FnOnce(CancelToken) -> Fut,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let handle =
            tokio::runtime::Handle::try_current().map_err(|_| SpawnScopedError::NoRuntime)?;
        let _registration = self
            .begin_effect()
            .map_err(|_| SpawnScopedError::InactiveFiber)?;
        if self.is_disposed()
            || matches!(
                self.state(),
                FiberState::Disposed | FiberState::Unloading | FiberState::Failed
            )
        {
            return Err(SpawnScopedError::InactiveFiber);
        }
        let cancel = CancelToken::new();
        // 先在未持 `native_tasks` 锁的情况下构造 future：允许同步阶段重入登记，
        // 且若闭包 panic 不会留下空槽位，`_registration` Drop 也会唤醒卸载等待者。
        let future = {
            let _guard = correlation_id
                .as_ref()
                .map(|cid| crate::trace::enter_correlation(Some(cid.clone())));
            task(cancel.clone())
        };
        let future: BoxFuture<()> = Box::pin(future);
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let id = self.native_task_sn.fetch_add(1, Ordering::SeqCst);
        self.native_tasks.lock().unwrap().insert(
            id,
            Arc::new(NativeTaskSlot {
                cancel: cancel.clone(),
                done: Mutex::new(Some(done_rx)),
            }),
        );
        // 由 `ScopedTaskFuture` 显式约束字段析构顺序（`future` 先于 `_done`）：
        // 即使任务尚未首次 poll 即被运行时丢弃，也保证先释放任务捕获的资源，
        // 再摘除槽位并通知 `dispose()` / `App::close()` 完成。
        let done = NativeTaskDone {
            fiber: Arc::downgrade(self),
            id,
            done: Some(done_tx),
        };
        handle.spawn(ScopedTaskFuture {
            future: Some(future),
            owner: Arc::downgrade(self),
            correlation_id,
            _done: done,
        });
        Ok(cancel)
    }

    /// 登记一条**原生后台任务**：fiber 卸载时取消并等待，任务完成时自行摘除。
    ///
    /// 用 `begin_effect` 的闸门而不是另起一个：`close_effects()` 返回后
    /// 不会再有新任务插入，随后的 [`Fiber::dispose_native_tasks`] 才能
    /// 看到完整快照（否则"检查空表 → 新任务插入 → 关闭返回"会漏掉它）。
    pub(crate) fn spawn_native_task<F>(
        self: &Arc<Self>,
        cancel: CancelToken,
        future: F,
    ) -> Result<()>
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let handle = tokio::runtime::Handle::try_current()
            .map_err(|_| "原生后台任务必须在 tokio 运行时里启动".to_string())?;
        let _registration = self.begin_effect()?;
        if self.is_disposed()
            || matches!(
                self.state(),
                FiberState::Disposed | FiberState::Unloading | FiberState::Failed
            )
        {
            return Err("cannot create effect on inactive context".into());
        }
        let future: BoxFuture<()> = Box::pin(future);
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let id = self.native_task_sn.fetch_add(1, Ordering::SeqCst);
        self.native_tasks.lock().unwrap().insert(
            id,
            Arc::new(NativeTaskSlot {
                cancel: cancel.clone(),
                done: Mutex::new(Some(done_rx)),
            }),
        );
        let done = NativeTaskDone {
            fiber: Arc::downgrade(self),
            id,
            done: Some(done_tx),
        };
        handle.spawn(ScopedTaskFuture {
            future: Some(future),
            owner: Arc::downgrade(self),
            correlation_id: None,
            _done: done,
        });
        Ok(())
    }

    /// 取消并等待本 fiber 拥有的一切原生后台任务。
    ///
    /// 必须在 `close_effects()` 之后调用：闸门已关，表里不会再有迟到者。
    /// 若配置了 `drain` 预算且超时，会把剩余未退出任务的等待移交后台（登记
    /// [`BackgroundCleanupGuard`]），并记录 [`CleanupErrorKind::Timeout`]，不强杀也不丢跟踪。
    async fn dispose_native_tasks(
        self: &Arc<Self>,
        budget: Option<std::time::Duration>,
        deadline: Option<tokio::time::Instant>,
    ) {
        let tasks: Vec<Arc<NativeTaskSlot>> = {
            let mut table = self.native_tasks.lock().unwrap();
            table.drain().map(|(_, slot)| slot).collect()
        };
        for task in &tasks {
            task.cancel.cancel();
        }
        let mut pending: Vec<tokio::sync::oneshot::Receiver<()>> = tasks
            .into_iter()
            .filter_map(|task| task.done.lock().unwrap().take())
            .collect();
        if pending.is_empty() {
            return;
        }
        let Some(deadline) = deadline.filter(|_| tokio::runtime::Handle::try_current().is_ok())
        else {
            for done in pending {
                // 取消是协作式的：任务可能在取消前已经结束（`Ok`），也可能
                // 因 panic 丢掉发送端（`Err`）。两种都表示"不会再跑了"。
                let _ = done.await;
            }
            return;
        };
        let mut idx = 0usize;
        while idx < pending.len() {
            if tokio::time::timeout_at(deadline, &mut pending[idx])
                .await
                .is_err()
            {
                let leftover: Vec<_> = pending.drain(idx..).collect();
                let budget_ms = budget.map(|d| d.as_millis()).unwrap_or(0);
                let reason = format!(
                    "等待受管后台任务退出超时（预算 {budget_ms} ms，仍有 {} 条任务在后台运行）",
                    leftover.len()
                );
                self.record_cleanup_error(CleanupErrorKind::Timeout, reason.clone());
                self.store.log(Level::Error, &self.name(), &reason);
                if let Ok(handle) = tokio::runtime::Handle::try_current() {
                    let guard = self.defer_background_cleanup();
                    handle.spawn(async move {
                        let _guard = guard;
                        for done in leftover {
                            let _ = done.await;
                        }
                    });
                }
                return;
            }
            idx += 1;
        }
    }

    // ------------------------------------------------------------- 行内钩子

    /// 注册一个**行内钩子**（上游 `Fiber._hooks`）：目前只有 `internal/update` 用它。
    ///
    /// 上游 `events.ts:54-58` 用内置的 `internal/listener` 把**非全局**的
    /// `ctx.on('internal/update', …)` 路由到这里，于是：
    /// - 它只对**这个 fiber** 的更新生效（别的 fiber 更新时不会跑）；
    /// - 它由 fiber 的 effect 拥有，**卸载即摘除**。
    pub(crate) fn push_hook(
        self: &Arc<Self>,
        name: &str,
        handler: Handler,
        prepend: bool,
    ) -> Disposable {
        let fiber = self.clone();
        let name_owned: Box<str> = name.into();
        self.effect(move |_| {
            let sn = fiber.hook_sn.fetch_add(1, Ordering::SeqCst);
            {
                let mut hooks = fiber.hooks.lock().unwrap();
                let list = hooks.entry(name_owned.clone()).or_default();
                if prepend {
                    list.insert(0, (sn, handler));
                } else {
                    list.push((sn, handler));
                }
            }
            let fiber2 = fiber.clone();
            let name2 = name_owned.clone();
            Disposable::sync(move || {
                let mut hooks = fiber2.hooks.lock().unwrap();
                if let Some(list) = hooks.get_mut(&name2) {
                    list.retain(|(s, _)| *s != sn);
                    if list.is_empty() {
                        hooks.remove(&name2);
                    }
                }
            })
        })
    }

    /// 本 fiber 的行内钩子（按注册顺序）。`internal/update` 的派发先跑它们。
    pub(crate) fn own_hooks(&self, name: &str) -> Vec<Handler> {
        self.hooks
            .lock()
            .unwrap()
            .get(name)
            .map(|list| list.iter().map(|(_, handler)| handler.clone()).collect())
            .unwrap_or_default()
    }

    // ------------------------------------------------------------- 状态迁移

    fn set_state(self: &Arc<Self>, next: FiberState) {
        // 单一入口状态机不变量：fiber 一旦置为已处置（`UID_DISPOSED`），绝不允许再迁回
        // `Pending` / `Loading` / `Active` / `Failed`（仅允许清理期 `Unloading` 与终态 `Disposed`）。
        let mut cur = self.state.load(Ordering::SeqCst);
        let prev = loop {
            let prev_state = FiberState::from_u8(cur);
            if prev_state == next {
                return;
            }
            if self.is_disposed() && !matches!(next, FiberState::Disposed | FiberState::Unloading) {
                return;
            }
            match self
                .state
                .compare_exchange(cur, next as u8, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => break prev_state,
                Err(actual) => cur = actual,
            }
        };
        self.emit_status(prev);
        // 只有 ACTIVE ↔ 非 ACTIVE 的跨界才需要通知依赖者。
        if (prev == FiberState::Active) == (next == FiberState::Active) {
            return;
        }
        if let Some(ctx) = self.ctx_opt() {
            for (name, realm) in self.store.impls_owned_by_with_realm(self) {
                // 状态迁移只做依赖唤醒，**不广播** `internal/service`（M3-4）——
                // 上游的广播点只有 provide 与移除两条路径；`notify_realm` 已为每个受影响依赖者 spawn `settle()`。
                let _ = self.store.notify_realm(&[&name], &ctx, realm, false);
            }
        }
    }

    fn emit_status(self: &Arc<Self>, prev: FiberState) {
        let Some(ctx) = self.ctx_opt() else { return };
        let store = self.store.clone();
        let fiber = self.clone();
        // 上游 `internal/status` 的实参是 `(fiber, oldValue)`；fiber 必须用
        // `Dyn::shared` 才能被 `downcast::<Fiber>()` 取回，oldValue 直接给状态枚举。
        let args: Args = vec![Dyn::shared(fiber), Dyn::new(prev)];
        if tokio::runtime::Handle::try_current().is_ok() {
            tokio::spawn(async move {
                store
                    .dispatch(Mode::Emit, "internal/status", &ctx, args, None, None)
                    .await;
            });
        }
    }

    /// 收敛：把状态推向 desired 与 running 一致的那个不动点。
    ///
    /// 若在“本 fiber 卸载/清理必须等待退出”的受管后台任务内部调用，收敛会移交到独立
    /// 后台任务并立即返回，避免与 `dispose_native_tasks` 自锁；需要确认重载最终结果
    /// 的调用方应在受管任务外部等待 [`Fiber::settle`] 或检查 [`ReloadDeferred`]。
    pub fn settle(self: &Arc<Self>) -> crate::dyn_value::BoxFuture<()> {
        let fut = self.settle_inner();
        Box::pin(async move {
            let _ = fut.await;
        })
    }

    fn settle_inner(self: &Arc<Self>) -> crate::dyn_value::BoxFuture<SettleDisposition> {
        let this = self.clone();
        Box::pin(async move {
            // 若当前线程正在运行一条会被本 fiber 卸载/清理等待退出的受管后台任务，
            // 不能在当前任务内等待 `driver`/卸载完成（会与 `dispose_native_tasks` 自锁）；
            // 改为在独立后台任务推进收敛并显式向调用方报告“已推迟/未完成”。
            if current_task_waited_by(&this) {
                return if let Ok(handle) = tokio::runtime::Handle::try_current() {
                    handle.spawn(async move {
                        this.settle().await;
                    });
                    SettleDisposition::Deferred
                } else {
                    SettleDisposition::DeferredNoRuntime
                };
            }
            let _guard = this.driver.lock().await;
            loop {
                // 处置是终态：已处置的 fiber 一律收敛到"卸载"。
                // 不能只看 `running`：根 fiber（`running == None`）与"装载失败但
                // 已注册子 fiber/effect"的 fiber 同样必须走 `unload`，
                // 否则 `dispose` 只改状态、不释放任何资源。
                if this.is_disposed() {
                    if let Err(payload) = CatchPanic::new(this.unload()).await {
                        let reason = format!(
                            "生命周期清理任务 panic: {}",
                            crate::panic_message(&*payload)
                        );
                        this.record_cleanup_error(
                            CleanupErrorKind::InternalCleanup,
                            reason.clone(),
                        );
                        this.store.log(Level::Error, &this.name(), &reason);
                        this.recover_cleanup().await;
                    }
                    break;
                }
                if this.error.lock().unwrap().is_some() {
                    break;
                }
                let desired = *this.desired.lock().unwrap();
                let running = *this.running.lock().unwrap();
                if desired == running {
                    break;
                }
                if running.is_some() {
                    if let Err(payload) = CatchPanic::new(this.unload()).await {
                        let reason = format!(
                            "生命周期卸载任务 panic: {}",
                            crate::panic_message(&*payload)
                        );
                        this.record_cleanup_error(
                            CleanupErrorKind::InternalCleanup,
                            reason.clone(),
                        );
                        this.store.log(Level::Error, &this.name(), &reason);
                        *this.error.lock().unwrap() = Some(reason.into());
                        *this.running.lock().unwrap() = None;
                        this.close_effects().await;
                        this.dispose_native_tasks(None, None).await;
                        this.dispose_effects().await;
                        *this.committed.lock().unwrap() = None;
                        this.set_state(FiberState::Failed);
                    }
                } else {
                    // 上一代（失败回滚 / 重载卸载 / 排水 / 等待依赖者）超出预算移交后台的清理
                    // 尚未归零：绝不并发装载新一代（避免新旧代次争用同一外部资源/服务名），
                    // 但这只是**暂态**——停在 Pending 并在清理归零后自动重试，而不是记成
                    // 只能靠 `update()` 恢复的 `error`（否则依赖驱动的重载会永久卡死）。
                    if this.pending_background_cleanups() > 0 {
                        let scheduled = this.schedule_cleanup_retry();
                        this.set_state(FiberState::Pending);
                        return SettleDisposition::WaitingForCleanup { scheduled };
                    }
                    this.load(desired.expect("desired is Some when running is None"))
                        .await;
                }
            }
            SettleDisposition::Completed
        })
    }

    /// 调度一次“等待本子树后台清理归零后重新收敛”的任务；已调度过则复用。
    fn schedule_cleanup_retry(self: &Arc<Self>) -> bool {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return false;
        };
        if self.cleanup_retry_scheduled.swap(true, Ordering::SeqCst) {
            return true;
        }
        let this = self.clone();
        handle.spawn(async move {
            this.wait_background_cleanups().await;
            // 先复位再收敛：若收敛时又出现新的后台清理，可以重新调度。
            this.cleanup_retry_scheduled.store(false, Ordering::SeqCst);
            if !this.is_disposed() {
                this.settle().await;
            }
        });
        true
    }

    async fn load(self: &Arc<Self>, epoch: u64) {
        let gen = self.load_gen.fetch_add(1, Ordering::SeqCst) + 1;
        // 上一轮卸载/失败会关闭闸门；新装载重新开放 effect 登记（已处置则直接退出）。
        if !self.open_effects() {
            return;
        }
        self.set_state(FiberState::Loading);
        // 重拍提交视图（`fiber.ts:421`）。**必须在 `apply` 之前**：
        // 插件在 apply 里 `provide` 的东西会由 `committed_insert` 追加进来，
        // 但那一刻的 `deps` 快照才是它"激活时解析到的依赖"。
        self.resnapshot();
        // 让出一拍，合并同一 tick 内到达的多次通知（对应 JS 的 `await Promise.resolve()`）。
        tokio::task::yield_now().await;
        if self.is_disposed() {
            return;
        }
        let Some(ctx) = self.ctx_opt() else { return };
        let Some(runtime) = self.runtime.clone() else {
            return;
        };
        let raw = self.config.lock().unwrap().clone();
        let resolved = match self.store.resolve_config(&ctx, &runtime, raw).await {
            Ok(config) => config,
            Err(err) => {
                self.fail(&ctx, err, gen).await;
                return;
            }
        };
        if self.is_disposed() {
            return;
        }
        // 已解析配置与 volatile 引用都是"装载的一部分"（M4-5）：引用在**插件跑之前**建好，
        // 插件的 `apply` 里就能 `ctx.volatile("data")` 取到——对应上游"校验期创建引用"。
        self.adopt_resolved(&resolved, &runtime);
        // **M5-1 护栏**：插件的 `apply` 是用户代码，它的 panic 不该让 fiber 无声地
        // 卡在 LOADING（`Store::plugin` 用 `tokio::spawn` 推状态机，panic 会被吞进
        // `JoinError`，之后再也没有人推进它）。截住 panic ⇒ 记成一次普通的装载失败。
        // 同步段 panic 也在护栏内：`plugin.apply(...)` 这个**调用本身**可能 panic
        //（`FnPlugin` 直接调用闭包，自定义实现也可能在返回 future 前失败）。
        // 只包 `CatchPanic` 的 `poll` 不够——panic 会在 `CatchPanic::new` 之前逃出去。
        let apply = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.plugin.apply(ctx.clone(), resolved)
        })) {
            Ok(future) => future,
            Err(payload) => {
                let reason = format!("插件 apply panic: {}", crate::panic_message(&*payload));
                self.fail(&ctx, reason.into(), gen).await;
                return;
            }
        };
        let apply_timeout = self
            .store
            .lifecycle_policy()
            .apply_timeout_ms
            .map(std::time::Duration::from_millis);
        let outcome = match apply_timeout.filter(|_| tokio::runtime::Handle::try_current().is_ok())
        {
            Some(limit) => match tokio::time::timeout(limit, CatchPanic::new(apply)).await {
                Ok(res) => res,
                Err(_) => {
                    let reason = format!("插件 apply 超时（预算 {} ms）", limit.as_millis());
                    self.fail(&ctx, reason.into(), gen).await;
                    return;
                }
            },
            None => CatchPanic::new(apply).await,
        };
        if self.is_disposed() {
            // apply 期间并发触发了 dispose：禁止把已处置 fiber 推回 Active/Failed，
            // 留给外层 `settle_inner` 下一轮执行 `unload()` 统一回收。
            return;
        }
        match outcome {
            Ok(Ok(())) => {
                *self.error.lock().unwrap() = None;
                *self.running.lock().unwrap() = Some(epoch);
                self.set_state(FiberState::Active);
            }
            Ok(Err(err)) => self.fail(&ctx, err, gen).await,
            Err(payload) => {
                let reason = format!("插件 apply panic: {}", crate::panic_message(&*payload));
                self.fail(&ctx, reason.into(), gen).await;
            }
        }
    }

    /// 装载失败：记日志 → 进入 FAILED → **立刻回收半途注册的副作用**。
    /// 后半步对应 JS `_reload` 的 catch 把 epoch 置 INACTIVE 从而触发紧随的 `_unload`。
    async fn fail(self: &Arc<Self>, _ctx: &Ctx, err: Error, gen: u64) {
        self.store.log(Level::Error, &self.name(), &err.to_string());
        *self.error.lock().unwrap() = Some(err);
        *self.running.lock().unwrap() = None;
        let drain_budget = self
            .store
            .lifecycle_policy()
            .drain_timeout_ms
            .map(std::time::Duration::from_millis);
        let drain_deadline = drain_budget.map(|limit| tokio::time::Instant::now() + limit);
        self.close_effects().await;
        self.dispose_native_tasks(drain_budget, drain_deadline)
            .await;
        self.dispose_effects_for_fail(gen).await;
        if !self.is_disposed() {
            self.set_state(FiberState::Failed);
        }
    }

    /// 装载失败或非处置重载卸载时回滚恢复栈：受 `LifecyclePolicy::dispose_timeout_ms` 约束。
    ///
    /// 返回 `Ok(())` 表示恢复栈已在预算内跑完；`Err(reason)` 表示超时并已整体移交后台按 LIFO
    /// 继续排空（由 `BackgroundCleanupGuard` 记账，`settle` 会在其归零前阻止装载新一代）。
    async fn dispose_effects_bounded(
        self: &Arc<Self>,
        gen: u64,
        timeout_prefix: &str,
    ) -> std::result::Result<(), String> {
        let dispose_budget = self
            .store
            .lifecycle_policy()
            .dispose_timeout_ms
            .map(std::time::Duration::from_millis);
        let Some((limit, rt)) = dispose_budget.and_then(|limit| {
            tokio::runtime::Handle::try_current()
                .ok()
                .map(|rt| (limit, rt))
        }) else {
            self.dispose_effects().await;
            if self.load_gen.load(Ordering::SeqCst) == gen {
                *self.committed.lock().unwrap() = None;
            }
            return Ok(());
        };
        let mut entries = std::mem::take(&mut *self.disposables.lock().unwrap());
        if entries.is_empty() {
            if self.load_gen.load(Ordering::SeqCst) == gen {
                *self.committed.lock().unwrap() = None;
            }
            return Ok(());
        }
        // `close_effects()` 已关闸，整体移交给单条任务按严格 LIFO 执行，
        // 避免随后 `dispose()` 与本后台任务并发弹出剩余槽位破坏 LIFO 逆序。
        entries.reverse();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let bg_guard = self.defer_background_cleanup_without_error();
        let bg_id = bg_guard.id();
        self.effect_bg_cleanups.fetch_add(1, Ordering::SeqCst);
        struct EffectBgGuard(Arc<Fiber>);
        impl Drop for EffectBgGuard {
            fn drop(&mut self) {
                if self.0.effect_bg_cleanups.fetch_sub(1, Ordering::SeqCst) == 1 {
                    self.0.effect_bg_notify.notify_waiters();
                }
            }
        }
        let effect_bg_guard = EffectBgGuard(self.clone());
        let this = self.clone();
        rt.spawn(async move {
            let _effect_bg = effect_bg_guard;
            this.run_disposable_entries(entries).await;
            if this.load_gen.load(Ordering::SeqCst) == gen {
                *this.committed.lock().unwrap() = None;
            }
            // 先销账再唤醒 `done_rx`，消除未超时路径上 `pending_background_cleanups() > 0` 的竞态窗。
            drop(bg_guard);
            let _ = done_tx.send(());
        });
        if tokio::time::timeout(limit, done_rx).await.is_err() {
            let reason = format!(
                "{timeout_prefix}（预算 {} ms），剩余恢复栈已移交后台继续执行",
                limit.as_millis()
            );
            let (seq, entry) =
                self.record_cleanup_error_with_seq(CleanupErrorKind::Timeout, reason.clone());
            self.store.set_background_cleanup_error(bg_id, seq, entry);
            self.store.log(Level::Error, &self.name(), &reason);
            return Err(reason);
        }
        Ok(())
    }

    async fn dispose_effects_for_fail(self: &Arc<Self>, gen: u64) {
        let _ = self
            .dispose_effects_bounded(gen, "装载失败回滚清理超时")
            .await;
    }

    /// 判定已处置 fiber 是否已处于彻底静默的终态（所有资源已关闭并排空）。
    ///
    /// 满足该条件时 `unload()` 自身即幂等空操作，无需调用者额外维护布尔护栏，
    /// 也不会把终态 `Disposed` 反弹回 `Unloading`。
    fn is_disposed_quiescent(self: &Arc<Self>) -> bool {
        if !self.is_disposed() || self.state() != FiberState::Disposed {
            return false;
        }
        {
            let gate = self.effect_gate.lock().unwrap();
            if !gate.closed || gate.in_flight != 0 {
                return false;
            }
        }
        if self.effect_bg_cleanups.load(Ordering::SeqCst) != 0 {
            return false;
        }
        if !self.disposables.lock().unwrap().is_empty() {
            return false;
        }
        if !self.native_tasks.lock().unwrap().is_empty() {
            return false;
        }
        if self.committed.lock().unwrap().is_some() || self.running.lock().unwrap().is_some() {
            return false;
        }
        self.store.impls_owned_by_with_realm(self).is_empty()
    }

    async fn unload(self: &Arc<Self>) {
        if self.is_disposed_quiescent() {
            return;
        }
        self.set_state(FiberState::Unloading);
        // 先关门，确保没有迟到的 effect / 原生任务登记；再取消并等待本 fiber
        // 自己的后台任务（它们可能正调用即将拆除的依赖），然后等依赖者拆完，
        // 最后才动提供者的恢复栈（论文 Algorithm 5 第 25 行）。
        let drain_budget = self
            .store
            .lifecycle_policy()
            .drain_timeout_ms
            .map(std::time::Duration::from_millis);
        let drain_deadline = drain_budget.map(|limit| tokio::time::Instant::now() + limit);
        self.close_effects().await;
        self.dispose_native_tasks(drain_budget, drain_deadline)
            .await;
        self.wait_for_dependents(drain_budget, drain_deadline).await;
        if self.is_disposed() {
            self.dispose_effects().await;
            self.wait_effect_background_cleanups().await;
            // 提交视图在**所有 disposer（含前置超时移交后台的恢复栈）都跑完之后**才清空（`fiber.ts:459`）。
            *self.committed.lock().unwrap() = None;
            *self.running.lock().unwrap() = None;
            self.set_state(FiberState::Disposed);
            return;
        }
        let gen = self.load_gen.load(Ordering::SeqCst);
        if self
            .dispose_effects_bounded(gen, "重载卸载旧代次清理超时")
            .await
            .is_err()
        {
            // 超时只是“暂态未完成”：不记 `error`（否则只能靠 `update()` 恢复，依赖驱动的
            // 重新激活会永久卡死）。保持闸门关闭停在 Pending，`settle` 会在后台清理归零后
            // 自动重试装载；超时本身已作为 `CleanupErrorKind::Timeout` 记账可观测。
            *self.running.lock().unwrap() = None;
            let next = if self.is_disposed() {
                FiberState::Disposed
            } else {
                FiberState::Pending
            };
            self.set_state(next);
            return;
        }
        *self.running.lock().unwrap() = None;
        // 处置是终态：若在清理期间被并发置为 Disposed，仍按 Disposed 收敛。
        let next = if self.is_disposed() {
            FiberState::Disposed
        } else {
            // 普通重载还会再次 load；重开闸门，让下一轮 apply 能登记 effect。
            self.open_effects();
            if self.error.lock().unwrap().is_some() {
                FiberState::Failed
            } else {
                FiberState::Pending
            }
        };
        self.set_state(next);
    }

    /// 等待直接依赖本 fiber 服务的消费者完成拆除。
    ///
    /// `set_state(Unloading)` 已经让 `check_impl` 把依赖者摘掉并触发 settle；
    /// 这里再取一次通知结果并**逐个 await**，把"提供者资源关闭"推迟到消费者
    /// 恢复栈跑完之后。若配置了 `drain` 预算且超时，剩余等待移交后台并记账。
    async fn wait_for_dependents(
        self: &Arc<Self>,
        budget: Option<std::time::Duration>,
        deadline: Option<tokio::time::Instant>,
    ) {
        let Some(ctx) = self.ctx_opt() else { return };
        let mut to_wait: Vec<Arc<Fiber>> = Vec::new();
        let mut seen: HashSet<u64> = HashSet::new();
        for (name, realm) in self.store.impls_owned_by_with_realm(self) {
            let dependents = self.store.notify_realm(&[&name], &ctx, realm, false);
            for fiber in dependents {
                if Arc::ptr_eq(&fiber, self) || fiber.is_cycle_blocked(name.as_ref()) {
                    continue;
                }
                if fiber.is_ancestor_of(self) {
                    self.warn_ancestor_consumer(&fiber);
                    continue;
                }
                if seen.insert(fiber.id) {
                    to_wait.push(fiber);
                }
            }
        }
        if to_wait.is_empty() {
            return;
        }
        let Some(deadline) = deadline.filter(|_| tokio::runtime::Handle::try_current().is_ok())
        else {
            for fiber in to_wait {
                if fiber.is_disposed() {
                    // uid 已经是 DISPOSED 不代表恢复栈跑完了：消费者可能正卡在
                    // 自己的 disposer 里。等它真正完成，而不是看到终态就跳过——
                    // 否则提供者会先关闭资源，消费者随后读到已关闭的值。
                    fiber.wait_disposed().await;
                } else {
                    // `UNLOADING` 也要等：那正是消费者已经被本提供者唤醒、正在
                    // 执行恢复栈的时刻。
                    fiber.settle().await;
                }
            }
            return;
        };
        let mut iter = to_wait.into_iter();
        while let Some(fiber) = iter.next() {
            let mut wait_fut: BoxFuture<()> = if fiber.is_disposed() {
                let target = fiber.clone();
                Box::pin(async move { target.wait_disposed().await })
            } else {
                fiber.settle()
            };
            if tokio::time::timeout_at(deadline, &mut wait_fut)
                .await
                .is_err()
            {
                let rest: Vec<Arc<Fiber>> = iter.collect();
                let budget_ms = budget.map(|d| d.as_millis()).unwrap_or(0);
                let reason = format!(
                    "等待依赖者 <{}#{}> 卸载超时（预算 {budget_ms} ms，仍有 {} 个依赖者在后台清理）",
                    fiber.name(),
                    fiber.id,
                    rest.len() + 1
                );
                self.record_cleanup_error(CleanupErrorKind::Timeout, reason.clone());
                self.store.log(Level::Error, &self.name(), &reason);
                if let Ok(handle) = tokio::runtime::Handle::try_current() {
                    let guard = self.defer_background_cleanup();
                    handle.spawn(async move {
                        let _guard = guard;
                        wait_fut.await;
                        for other in rest {
                            if other.is_disposed() {
                                other.wait_disposed().await;
                            } else {
                                other.settle().await;
                            }
                        }
                    });
                }
                return;
            }
        }
    }

    /// 逆序释放全部副作用。顺序语义与 cordis 一致：**LIFO**。
    ///
    /// # panic 隔离（第三方插件的必备护栏）
    /// 一个行为不端的插件在 disposer 里 panic，不该让**整个宿主**的卸载流程中断
    /// ——否则"任意第三方插件"就变成了"任意第三方插件都能拖垮进程"。
    /// 这里把每个 disposer 放进独立任务执行：`tokio::spawn` 会捕获 panic 并把它
    /// 变成 `JoinError`，于是宿主能记账并继续卸载剩下的。
    ///
    /// 注意这只隔离 panic 的**传播**，不修复它造成的**状态破坏**：
    /// 如果 panting 的 disposer 当时持有一把 `Mutex`，那把锁会中毒，
    /// 之后任何 `.unwrap()` 都会再次 panic。真正的隔离需要在插件边界做进程隔离。
    async fn wait_effect_background_cleanups(&self) {
        loop {
            let notified = self.effect_bg_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.effect_bg_cleanups.load(Ordering::SeqCst) == 0 {
                return;
            }
            notified.await;
        }
    }

    async fn dispose_effects(self: &Arc<Self>) {
        let name = self.name();
        // 每次只把队尾（最后注册）移出 fiber；调用者 Future 中途被丢弃时，
        // 尚未执行的恢复栈仍留在 self.disposables 里，下一次 dispose 可继续。
        loop {
            let Some(entry) = self.disposables.lock().unwrap().pop() else {
                break;
            };
            self.run_disposable_entry(&name, entry).await;
        }
    }

    async fn run_disposable_entries(self: &Arc<Self>, entries: Vec<EffectEntry>) {
        let name = self.name();
        for entry in entries {
            self.run_disposable_entry(&name, entry).await;
        }
    }

    async fn run_disposable_entry(self: &Arc<Self>, name: &str, entry: EffectEntry) {
        let Some(item) = entry.cell.lock().unwrap().take() else {
            // 句柄已被手动执行（登记随后会被摘除）；这里只可能是极窄的竞态窗口。
            return;
        };
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                if let Err(err) = handle.spawn(async move { item.run().await }).await {
                    let reason = if err.is_panic() {
                        format!(
                            "disposer panicked: {}",
                            crate::panic_message(&*err.into_panic())
                        )
                    } else {
                        "disposer task was cancelled".to_string()
                    };
                    self.record_cleanup_error(CleanupErrorKind::DisposerPanic, reason.clone());
                    self.store.log(Level::Error, name, &reason);
                }
            }
            // 没有运行时（同步上下文）：仍捕获 panic 并记账，不让单条 disposer 中断整条恢复栈。
            Err(_) => {
                if let Err(payload) = CatchPanic::new(item.run()).await {
                    let reason = format!("disposer panicked: {}", crate::panic_message(&*payload));
                    self.record_cleanup_error(CleanupErrorKind::DisposerPanic, reason.clone());
                    self.store.log(Level::Error, name, &reason);
                }
            }
        }
    }

    // --------------------------------------------------------------- 公开操作

    /// 强制重载：递增 revision 让 epoch 必变，然后收敛。
    ///
    /// 若在“本 fiber 卸载必须等待退出”的受管后台任务内部重入调用，重载请求会被受理并
    /// 移交后台任务执行，但本调用返回 `Err`（具体类型为 [`ReloadDeferred`]），绝不把
    /// 尚未执行的新一代 `apply()` 误报为 `Ok(())`。
    pub async fn restart(self: &Arc<Self>) -> Result<()> {
        if self.is_disposed() {
            return Err("cannot restart disposed fiber".into());
        }
        self.revision.fetch_add(1, Ordering::SeqCst);
        self.refresh_desired();
        match self.settle_inner().await {
            SettleDisposition::Completed => match self.error() {
                Some(err) => Err(err.into()),
                None => Ok(()),
            },
            SettleDisposition::Deferred => Err(Box::new(ReloadDeferred::scheduled(self.name()))),
            SettleDisposition::DeferredNoRuntime => {
                Err(Box::new(ReloadDeferred::unscheduled(self.name())))
            }
            SettleDisposition::WaitingForCleanup { scheduled } => Err(Box::new(
                ReloadDeferred::waiting_for_cleanup(self.name(), scheduled),
            )),
        }
    }

    /// 更新配置（历史入口：不关心是否被否决，被否决也算成功）。
    /// 需要区分"重启了"与"被否决"时用 [`Fiber::update_with`]。
    pub async fn update(self: &Arc<Self>, config: Config) -> Result<()> {
        self.update_with(config, false).await.map(|_| ())
    }

    /// 更新配置的完整形态（`internal/update`，M3-1）。步骤逐条对齐上游
    /// `fiber.ts:486-506`：
    ///
    /// 1. **先解析**（`internal/config` 瀑布 + 插件校验器）。解析失败 ⇒ 直接返回 Err，
    ///    fiber 原地不动（旧配置继续跑）——**不再是**"重启 → apply 失败 → fiber FAILED"；
    /// 2. `internal/commit` **总是**发出，载荷 `(fiber, raw, no_save)`。这是有意偏离
    ///    上游的 `!noSave`：单一通道让 loader 自己区分"推进内存记录"与"写盘"（附录 A）；
    /// 3. `fiber.config = raw` 在派发之前赋值（上游 `fiber.ts:492`）；
    /// 4. `internal/update` 瀑布，实参 `[fiber, resolved, no_save]`；
    ///    顺序 = **自身行内钩子 → 全局钩子 → tail**（上游用 global+prepend 内置钩子拼接）；
    /// 5. 否决（整体返回 `None`）⇒ 不重启、不报错，返回 [`UpdateOutcome::Vetoed`]。
    ///
    /// 已知偏差（附录 A）：`Ret = Option<Dyn>` 把 `null` 与 `undefined` 合并，
    /// 所以"调了 next 但显式返回 None"也会被当作否决（上游只把 `undefined` 当否决）。
    pub async fn update_with(
        self: &Arc<Self>,
        config: Config,
        no_save: bool,
    ) -> Result<UpdateOutcome> {
        if self.is_disposed() {
            return Err("cannot update disposed fiber".into());
        }
        // 同步取号：并发 update 的“新旧”由此决定，而不是由解析/派发完成顺序决定。
        let my_rev = self.update_rev.fetch_add(1, Ordering::SeqCst) + 1;
        let loaded = self.running.lock().unwrap().is_some();
        let resolved = if loaded {
            // 解析失败 ⇒ 什么都不动（上游把 `_resolve` 放在赋值之前）。
            let Some(runtime) = self.runtime.clone() else {
                return Err("fiber 没有 runtime：无法解析配置".into());
            };
            let Some(ctx) = self.ctx_opt() else {
                return Err("cannot update disposed fiber".into());
            };
            self.store
                .resolve_config(&ctx, &runtime, config.clone())
                .await?
        } else {
            config.clone()
        };

        // 提交闸门只保护“比较 + 赋值 + adopt”这一小段；不跨 await，也不包住
        // `internal/update`，否则 handler 重入 update 会死锁。
        //
        // 先解析、后比较：两个 update 的 resolve 可以并发/乱序完成，但只有
        // `update_rev` 更大的那一份能落地。被超越的调用返回 Superseded，不派发
        // handler，避免它把旧配置写进插件状态。
        let mut did_commit = false;
        let mut previous = None;
        {
            // 串行化提交，避免两个 resolve 乱序完成后“旧值后写”。committed_rev
            // 只在这一小段内持有；fire_commit/adopt 不持 committed_rev 锁，
            // 防止提交观察者重入 update 时自锁。
            let _commit = self.commit_gate.lock().await;
            let latest = {
                let mut committed_rev = self.committed_rev.lock().unwrap();
                if my_rev > *committed_rev {
                    *committed_rev = my_rev;
                    true
                } else {
                    false
                }
            };
            if latest {
                // 赋值在派发之前（上游 `fiber.ts:492`），避免 `fire_commit` 观察者 panic 导致
                // `committed_rev` 已推进但 `config`/`resolved`/`error` 仍停在旧值。
                *self.config.lock().unwrap() = config.clone();
                // `adopt_resolved` 会重建 volatile 引用表；若 `internal/update` 随后否决，
                // 必须把**旧表原样**放回去——重建会换 `Arc`，插件缓存的引用就再也写不到了。
                previous = loaded.then(|| {
                    (
                        self.resolved.lock().unwrap().clone(),
                        self.volatile.lock().unwrap().clone(),
                    )
                });
                // 已解析的那一份同样推进（volatile 的候选比较拿它当基准，M4-5）。
                if loaded {
                    let runtime = self.runtime.clone();
                    if let Some(runtime) = runtime {
                        self.adopt_resolved(&resolved, &runtime);
                    }
                }
                *self.error.lock().unwrap() = None; // FAILED 的唯一恢复路径
                did_commit = true;
                // commit **总是**发出（含 no_save）：loader 靠它同时推进记录与决定是否落盘。
                self.store.fire_commit(self, &config, no_save);
                self.store.fire_commit_async(self, &config, no_save).await;
            }
        }
        if !did_commit {
            return Ok(UpdateOutcome::Superseded);
        }

        if loaded {
            // RAII：无论 handler 正常返回、否决还是 panic 展开，都会递减在飞计数
            // 并在被超越时标记 replay。
            self.active_chains.fetch_add(1, Ordering::SeqCst);
            let _chain = UpdateChain {
                fiber: self.clone(),
                rev: my_rev,
            };
            let ctx = self.ctx();
            let tail_resolved = resolved.clone();
            let tail: BoxFuture<Ret> = Box::pin(async move { Some(Dyn::new(tail_resolved)) });
            let args: Args = vec![
                Dyn::shared(self.clone()),
                Dyn::new(resolved),
                Dyn::new(no_save),
            ];
            let result = self
                .store
                .dispatch(
                    Mode::Waterfall,
                    INTERNAL_UPDATE,
                    &ctx,
                    args,
                    Some(tail),
                    None,
                )
                .await;
            if result.is_none() {
                // 被 update 钩子否决：不重启，也不报错（上游 `fiber.ts:499`）。
                // 只有这次提交仍是最新提交时才回滚；否则旧否决会把新 update
                // 已经 adopt 的 resolved/volatile 一起打回去。
                if *self.committed_rev.lock().unwrap() == my_rev {
                    if let Some((old_resolved, old_volatile)) = previous {
                        *self.resolved.lock().unwrap() = old_resolved;
                        *self.volatile.lock().unwrap() = old_volatile;
                    }
                }
                return Ok(UpdateOutcome::Vetoed);
            }
        }

        self.revision.fetch_add(1, Ordering::SeqCst);
        self.refresh_desired();
        match self.settle_inner().await {
            SettleDisposition::Completed => match self.error() {
                Some(err) => Err(err.into()),
                None => Ok(UpdateOutcome::Restarted),
            },
            SettleDisposition::Deferred => Err(Box::new(ReloadDeferred::scheduled(self.name()))),
            SettleDisposition::DeferredNoRuntime => {
                Err(Box::new(ReloadDeferred::unscheduled(self.name())))
            }
            SettleDisposition::WaitingForCleanup { scheduled } => Err(Box::new(
                ReloadDeferred::waiting_for_cleanup(self.name(), scheduled),
            )),
        }
    }

    /// 等所有并发 update 及其 replay 结束。
    ///
    /// `await update_with(...)` 只保证本次调用返回；若它被更晚的 update 超越，
    /// 真正把插件状态修正到最新配置的 replay 可能在返回之后才跑。需要“状态已收敛”
    /// 的调用方（测试、配置提交后的强同步点）应 await 这个 future。
    pub async fn when_updates_settled(&self) {
        loop {
            // `notify_waiters` 不会为尚未 poll 的 `Notified` 保存许可；必须先
            // enable，再检查条件，否则 replay 在检查与 await 之间完成时会丢唤醒。
            let notified = self.updates_settled.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.active_chains.load(Ordering::SeqCst) == 0
                && !self.replay_owed.load(Ordering::SeqCst)
                && !self.replay_scheduled.load(Ordering::SeqCst)
            {
                return;
            }
            notified.await;
        }
    }

    /// 一条 update chain 结束：递减在飞计数，必要时调度 quiescence replay。
    fn finish_update_chain(self: &Arc<Self>, rev: u64) {
        let remaining = self
            .active_chains
            .fetch_sub(1, Ordering::SeqCst)
            .saturating_sub(1);
        if rev < self.update_rev.load(Ordering::SeqCst) {
            self.replay_owed.store(true, Ordering::SeqCst);
        }
        if remaining == 0 {
            if self.replay_owed.load(Ordering::SeqCst) && !self.is_disposed() {
                self.schedule_replay();
            }
            self.updates_settled.notify_waiters();
        }
    }

    fn schedule_replay(self: &Arc<Self>) {
        if self.replay_scheduled.swap(true, Ordering::SeqCst) {
            return;
        }
        let this = self.clone();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    this.run_replay().await;
                });
            }
            Err(_) => {
                // 没有运行时就没有 replay 的驱动器；不能把 when_updates_settled
                // 永久挂住，清掉 owed 并通知等待者。
                self.replay_owed.store(false, Ordering::SeqCst);
                self.replay_scheduled.store(false, Ordering::SeqCst);
                self.updates_settled.notify_waiters();
            }
        }
    }

    /// 对**最新** resolved 配置重跑 `internal/update`。
    ///
    /// 关键约束（对齐上游 #202）：只跑 handler，不递增 revision、不 settle、不重启；
    /// 参数 `no_save = true`，replay 不会再次落盘。若 replay 期间又有 update 提交，
    /// 循环到 revision 稳定为止。
    async fn run_replay(self: &Arc<Self>) {
        {
            let _gate = self.replay_gate.lock().await;
            loop {
                // 还有在飞 chain：让最后一个 chain 结束时再调度，避免与它并发写入。
                if self.active_chains.load(Ordering::SeqCst) != 0 {
                    break;
                }
                if !self.replay_owed.swap(false, Ordering::SeqCst) {
                    break;
                }
                let Some(ctx) = self.ctx_opt() else { break };
                // 未装载或已处置：没有可修正的实例；下一次 update 会重新解析。
                if self.running.lock().unwrap().is_none() || self.is_disposed() {
                    break;
                }
                let rev = self.update_rev.load(Ordering::SeqCst);
                let resolved = self.resolved.lock().unwrap().clone();
                let tail_resolved = resolved.clone();
                let tail: BoxFuture<Ret> = Box::pin(async move { Some(Dyn::new(tail_resolved)) });
                let args: Args = vec![
                    Dyn::shared(self.clone()),
                    Dyn::new(resolved),
                    Dyn::new(true),
                ];
                let _ = self
                    .store
                    .dispatch(
                        Mode::Waterfall,
                        INTERNAL_UPDATE,
                        &ctx,
                        args,
                        Some(tail),
                        None,
                    )
                    .await;
                if self.update_rev.load(Ordering::SeqCst) == rev {
                    break;
                }
                self.replay_owed.store(true, Ordering::SeqCst);
            }
        }
        self.replay_scheduled.store(false, Ordering::SeqCst);
        if self.replay_owed.load(Ordering::SeqCst)
            && self.active_chains.load(Ordering::SeqCst) == 0
            && !self.is_disposed()
        {
            self.schedule_replay();
        }
        self.updates_settled.notify_waiters();
    }

    /// 卸载并释放这个 fiber 的一切。父 fiber 卸载时通过 effect 自动递归调用。
    ///
    /// 清理本身由一条独立的共享任务推进：调用者的 Future 被 timeout 丢弃不会
    /// 取消恢复栈；重复调用则等待同一次清理完成，而不是看到终态就提前返回。
    ///
    /// 若在“本 fiber 清理必须等待退出”的受管后台任务（`spawn_scoped` / 原生流任务）
    /// 内部调用，则只发起关闭而不阻塞等待清理结束，避免清理等任务、任务等清理的自锁。
    pub fn dispose(self: &Arc<Self>) -> crate::dyn_value::BoxFuture<()> {
        let this = self.clone();
        Box::pin(async move {
            let self_reentrant = current_task_waited_by(&this);
            let first = this.uid.swap(UID_DISPOSED, Ordering::SeqCst) != UID_DISPOSED;
            // 清理任务必须在**任何 await 之前**启动：否则调用者在 `close_effects()`
            // 等待在途 setup 时丢弃 Future，就会留下“uid 已置 DISPOSED、却没有
            // 清理任务”的终态，后续 dispose 只能永久等待 dispose_tx。
            if first && !this.begin_dispose() {
                // 没有运行时就没有可用的 panic 隔离；至少保证等待者不会永久挂起。
                let _done = DisposeDone(this.clone());
                this.finish_dispose().await;
            }
            if !self_reentrant {
                this.wait_disposed().await;
            }
        })
    }

    /// 带结构化报告与可选等待预算的关闭入口。
    ///
    /// - `timeout` 为 `Some(d)` 时优先使用本次调用的等待预算；为 `None` 时回退到
    ///   [`LifecyclePolicy::dispose_timeout_ms`]（同样为 `None` 表示不设上限）；
    /// - 汇总本 fiber 及其全部后代子树上的清理错误（原生 disposer panic、宿主
    ///   `OP_CLEANUP` / `OP_DISPOSE` 失败、内部清理异常与超时）；若此前已由无报告的
    ///   [`Fiber::dispose`] 完成处置，再次调用仅能返回本 fiber 自身记录到的清理错误；
    /// - 等待超时时立刻返回 [`CloseStatus::TimedOut`]（`still_running == true`），
    ///   共享清理任务与后台延后清理任务仍继续运行，不破坏同进程动态库与在途调用保活；
    /// - 在受管后台任务内部重入调用时返回 [`CloseStatus::Deferred`]（`still_running == true`）。
    pub fn dispose_with_report(
        self: &Arc<Self>,
        timeout: Option<std::time::Duration>,
    ) -> crate::dyn_value::BoxFuture<CloseReport> {
        let this = self.clone();
        Box::pin(async move {
            let started = std::time::Instant::now();
            let effective_timeout = timeout.or_else(|| {
                this.store
                    .lifecycle_policy()
                    .dispose_timeout_ms
                    .map(std::time::Duration::from_millis)
            });
            if effective_timeout.is_some() {
                this.mark_budgeted_dispose();
            }
            // 先挂收集器、再读已有错误：配合全局单调序号去重，并发关闭既不漏记也不重复记。
            let collector = Arc::new(Mutex::new(Vec::<(u64, CleanupError)>::new()));
            this.store
                .attach_cleanup_collector(this.id, Arc::downgrade(&collector));

            let self_reentrant = current_task_waited_by(&this);
            let first = this.uid.swap(UID_DISPOSED, Ordering::SeqCst) != UID_DISPOSED;
            let pre_existing = this.cleanup_errors_with_seq();
            if first && !this.begin_dispose() {
                let _done = DisposeDone(this.clone());
                this.finish_dispose().await;
            }
            if self_reentrant {
                if let Ok(handle) = tokio::runtime::Handle::try_current() {
                    let guard = this.defer_background_cleanup_without_error();
                    let target = this.clone();
                    handle.spawn(async move {
                        let _guard = guard;
                        target.wait_disposed().await;
                    });
                }
                let errors = merge_cleanup_errors(pre_existing, Vec::new(), &collector, false);
                return CloseReport {
                    status: CloseStatus::Deferred,
                    errors,
                    still_running: true,
                    elapsed: started.elapsed(),
                };
            }

            let wait_timed_out =
                match effective_timeout.filter(|_| tokio::runtime::Handle::try_current().is_ok()) {
                    Some(limit) => tokio::time::timeout(limit, this.wait_disposed())
                        .await
                        .is_err(),
                    None => {
                        this.wait_disposed().await;
                        false
                    }
                };
            if wait_timed_out {
                let limit_ms = effective_timeout.map(|d| d.as_millis()).unwrap_or(0);
                let reason = format!(
                    "等待 fiber `{}` 关闭超时（预算 {limit_ms} ms），后台清理仍在进行",
                    this.name()
                );
                this.record_cleanup_error(CleanupErrorKind::Timeout, reason.clone());
                this.store.log(Level::Error, &this.name(), &reason);
                if let Ok(handle) = tokio::runtime::Handle::try_current() {
                    let guard = this.defer_background_cleanup();
                    let target = this.clone();
                    handle.spawn(async move {
                        let _guard = guard;
                        target.wait_disposed().await;
                    });
                }
            }
            let (bg_count, bg_errors) = this.store.background_cleanups_snapshot_under(this.id);
            let still_running = wait_timed_out || bg_count > 0;
            let errors = merge_cleanup_errors(pre_existing, bg_errors, &collector, still_running);
            let status = if still_running {
                CloseStatus::TimedOut
            } else if !errors.is_empty() {
                CloseStatus::CompletedWithErrors
            } else {
                CloseStatus::Completed
            };
            CloseReport {
                status,
                errors,
                still_running,
                elapsed: started.elapsed(),
            }
        })
    }

    pub(crate) fn is_current_task_waited_by(self: &Arc<Self>) -> bool {
        current_task_waited_by(self)
    }

    /// 同步完成处置记账并启动共享清理任务；返回是否成功 spawn。
    fn begin_dispose(self: &Arc<Self>) -> bool {
        // 先同步关门，避免 uid 已置空、但清理任务尚未被 poll 的窗口里
        // 又有 effect 通过闸门。
        self.mark_effects_closed();
        // uid 已置空后才派发 internal/plugin，loader 靠这一点识别"用户手动停用了这一行"。
        // 截住用户 `on_plugin` 观察者 panic，确保后续清理任务仍会被启动，避免 dispose 永久挂起。
        if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.store.fire_plugin(self);
        })) {
            let reason = format!(
                "dispose 阶段 on_plugin 观察者 panic: {}",
                crate::panic_message(&*payload)
            );
            self.record_cleanup_error(CleanupErrorKind::InternalCleanup, reason.clone());
            self.store.log(Level::Error, &self.name(), &reason);
        }
        *self.desired.lock().unwrap() = None;
        *self.error.lock().unwrap() = None;
        // 状态迁移会通知依赖者，用户提供的 `Check` 谓词可能 panic。
        // 这里截住它，确保清理任务仍然会被启动；否则 dispose 会半途而废。
        if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.set_state(FiberState::Disposed);
        })) {
            let reason = format!(
                "生命周期状态迁移 panic: {}",
                crate::panic_message(&*payload)
            );
            self.record_cleanup_error(CleanupErrorKind::InternalCleanup, reason.clone());
            self.store.log(Level::Error, &self.name(), &reason);
        }

        let cleanup = self.clone();
        if tokio::runtime::Handle::try_current().is_ok() {
            tokio::spawn(async move {
                cleanup.supervise_cleanup().await;
            });
            true
        } else {
            false
        }
    }

    /// 清理任务的监督者：内层任务 panic 时不提前置完成位，而是先跑兜底排水。
    async fn supervise_cleanup(self: &Arc<Self>) {
        let _done = DisposeDone(self.clone());
        let cleanup = self.clone();
        if let Err(err) = tokio::spawn(async move { cleanup.finish_dispose().await }).await {
            let reason = if err.is_panic() {
                "生命周期清理任务 panic".to_string()
            } else {
                "生命周期清理任务被取消".to_string()
            };
            self.record_cleanup_error(CleanupErrorKind::InternalCleanup, reason.clone());
            self.store.log(Level::Error, &self.name(), &reason);
            self.recover_cleanup().await;
        }
    }

    async fn finish_dispose(self: &Arc<Self>) {
        self.settle().await;
        // `unload` 已按终态收敛；这里再确认一次，避免未来改动把终态次序改坏。
        self.set_state(FiberState::Disposed);
        self.clear_context();
    }

    /// 清理任务异常后的兜底：尽力关门、排空恢复栈，并留下可诊断的错误。
    async fn recover_cleanup(self: &Arc<Self>) {
        if tokio::runtime::Handle::try_current().is_ok() {
            if tokio::time::timeout(std::time::Duration::from_secs(1), self.close_effects())
                .await
                .is_err()
            {
                let reason = "清理兜底等待 effect setup 超时".to_string();
                self.record_cleanup_error(CleanupErrorKind::Timeout, reason.clone());
                self.store.log(Level::Error, &self.name(), &reason);
            }
        } else {
            self.close_effects().await;
        }
        self.dispose_native_tasks(None, None).await;
        let drain = self.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            if let Err(err) = handle
                .spawn(async move { drain.dispose_effects().await })
                .await
            {
                let reason = if err.is_panic() {
                    "清理兜底排水 panic".to_string()
                } else {
                    "清理兜底排水被取消".to_string()
                };
                self.record_cleanup_error(CleanupErrorKind::InternalCleanup, reason.clone());
                self.store.log(Level::Error, &self.name(), &reason);
            }
        } else {
            drain.dispose_effects().await;
        }
        *self.committed.lock().unwrap() = None;
        *self.running.lock().unwrap() = None;
        if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.set_state(FiberState::Disposed);
        })) {
            let reason = format!(
                "清理兜底状态迁移 panic: {}",
                crate::panic_message(&*payload)
            );
            self.record_cleanup_error(CleanupErrorKind::InternalCleanup, reason.clone());
            self.store.log(Level::Error, &self.name(), &reason);
        }
        self.clear_context();
    }

    fn clear_context(&self) {
        // 打破 Context <-> Fiber 的强引用环，避免 HMR 反复重载时泄漏。
        *self.ctx.lock().unwrap() = None;
        *self.ctx_weak.lock().unwrap() = None;
    }

    pub(crate) async fn wait_disposed(self: &Arc<Self>) {
        let mut rx = self.dispose_tx.subscribe();
        while !*rx.borrow() {
            if rx.changed().await.is_err() {
                break;
            }
        }
    }
}

/// 清理任务结束（或崩溃）时置位 `dispose_tx`，让所有等待者收敛。
struct DisposeDone(Arc<Fiber>);

impl Drop for DisposeDone {
    fn drop(&mut self) {
        // 子 fiber 的恢复栈已经跑完，父 fiber 里那条“递归 dispose”登记不再需要。
        // 只摘登记、不执行（在清理里再调 dispose 会等自己的完成位，必然自锁）。
        if let Some(handle) = self.0.parent_effect.lock().unwrap().take() {
            handle.detach();
        }
        // 清理完成前必须仍留在 runtime 表里：提供者卸载时要能通过 notify 找到
        // “已置 DISPOSED、但恢复栈还在跑”的消费者，并等它真正拆完。
        // 先摘除再置完成位，保证被唤醒的等待者看不到半拆除的登记。
        if let Some(runtime) = &self.0.runtime {
            self.0.store.unregister_fiber(runtime, &self.0);
        }
        self.0.dispose_tx.send_replace(true);
    }
}

fn merge_cleanup_errors(
    pre_existing: Vec<(u64, CleanupError)>,
    bg_errors: Vec<(u64, CleanupError)>,
    collector: &Arc<Mutex<Vec<(u64, CleanupError)>>>,
    include_synthetic_bg: bool,
) -> Vec<CleanupError> {
    let collected = collector.lock().unwrap().clone();
    let mut numbered: Vec<(u64, CleanupError)> = Vec::new();
    let mut synthetic: Vec<CleanupError> = Vec::new();
    for (seq, err) in pre_existing.into_iter().chain(collected).chain(bg_errors) {
        if seq == 0 {
            synthetic.push(err);
        } else {
            numbered.push((seq, err));
        }
    }
    numbered.sort_by_key(|(seq, _)| *seq);
    numbered.dedup_by_key(|(seq, _)| *seq);
    let mut out: Vec<CleanupError> = numbered.into_iter().map(|(_, err)| err).collect();
    if include_synthetic_bg {
        for err in synthetic {
            let covered = out.iter().any(|existing| {
                existing.uid == err.uid && existing.kind == CleanupErrorKind::Timeout
            });
            if !covered {
                out.push(err);
            }
        }
    }
    out
}

fn effect_handle(owner: Weak<Fiber>, id: u64, cell: Arc<Mutex<Option<Disposable>>>) -> Disposable {
    Disposable::new(move || {
        let taken = cell.lock().unwrap().take();
        // 手动释放立即摘除登记：恢复栈只保留“仍然欠着”的 effect。
        if let Some(owner) = owner.upgrade() {
            owner.remove_effect(id);
        }
        Box::pin(async move {
            if let Some(disposable) = taken {
                disposable.run().await;
            }
        })
    })
}

/// 把"某次 poll 里的 panic"变成 `Err(payload)` 的 future 包装（M5-1）。
///
/// 为什么不直接用 `catch_unwind`：插件的 `apply` 是**异步**的，panic 发生在某次
/// `poll` 里，而 `catch_unwind` 包不住 `.await`。也不用 `futures` 的
/// `FutureExt::catch_unwind`：本仓刻意不引入 `futures` 依赖（见 `dyn_value` 开头）。
///
/// 内部是 `Pin<Box<F>>` 而不是裸 `F`：`Pin<Box<_>>` 恒为 `Unpin`，于是
/// `poll` 里能安全地拿回 `&mut`，**不需要任何 `unsafe`**（内核整仓 `forbid(unsafe_code)`）。
/// 代价是每次装载多一次装箱——`apply` 是低频操作，这个价格可以忽略。
///
/// 截住之后这个 future 会被丢弃：panic 中途的 future 状态"不确定"，继续 poll
/// 它没有任何语义可依。
struct CatchPanic<F>(Pin<Box<F>>);

impl<F> CatchPanic<F> {
    fn new(inner: F) -> Self {
        Self(Box::pin(inner))
    }
}

impl<F: std::future::Future> std::future::Future for CatchPanic<F> {
    type Output = std::result::Result<F::Output, Box<dyn Any + Send>>;

    fn poll(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        let slot = &mut self.get_mut().0;
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| slot.as_mut().poll(cx))) {
            Ok(Poll::Ready(value)) => Poll::Ready(Ok(value)),
            Ok(Poll::Pending) => Poll::Pending,
            Err(payload) => Poll::Ready(Err(payload)),
        }
    }
}
