//! 宿主：动态库加载 + C ABI ↔ 内核适配 + 配置驱动装载。
//!
//! 四块内容：
//! - [`dylib`]：`dlopen` 一个 cdylib（手写，零第三方依赖）；
//! - [`patch`]：`cordis.yml` / `cordis.patch.yml` 的纯函数兼容层；
//! - [`js`]：`!!js` 表达式的内置子集求值器（`process.*` / `ctx.get(...)` / 运算符）；
//! - [`loader`]：配置驱动的插件树装载器 + 热重载。
//!
//! # ⚠️ C ABI 与 async 内核的接法：同步快速路径 + 异步兜底
//! 这是整个载体层最需要想清楚的一件事。内核是异步的，C ABI 是同步的。
//! 朴素做法有两种，都不可接受：让宿主线程阻塞等结果（会在 async 运行时里死锁），
//! 或者把所有操作都改成"发起 + 回调"（把最常见的同步场景也变成状态机）。
//!
//! 本实现两个都要：**先同步试跑一次**（`complete_or_defer`），
//! - 立即就绪 → 直接把结果内联返回，插件侧就是普通的一次函数调用；
//! - 尚未就绪 → 把这次未来交给运行时，完成后通过 `OP_CALLBACK` 回调插件注册的续延。
//!
//! 插件侧因此**不需要区分**两条路径：它的续延恰好被执行一次。
//! 这条性质由三件事共同保证：
//! 1. 续延在插件侧注册为**一次性**（`Registry::continuations` 取出即删）；
//! 2. 内联路径下插件自己消费掉续延（`Registry::complete`）；
//! 3. 兜底路径下宿主回调同一个续延 id。
//!
//! # `unsafe` 清单
//! 本 crate 集中了工程里**全部**的裸 FFI，位置固定、可一次审完：
//! - [`dylib`]：`LoadLibraryW` / `GetProcAddress` / `FreeLibrary`（或 `dlopen`/`dlsym`/`dlclose`）；
//! - [`LoadedPlugin::open`]：读 `PluginDesc`、校验 ABI、把符号转成 `EntryFn`；
//! - `host_ctx_*`：把内核对象转成 ABI 的 `Blob` / `Out`。
//!
//! 边界约束收得很紧：句柄是**带代际的槽位号**（不含地址，且陈旧句柄会被拒绝）、
//! **跨边界零分配**（内存永远归宿主）、所有插件代码调用点都先「放锁再调用」
//! （回调可能重入宿主）。

// clippy 1.98 会把已经是 `const { … }` 的初始化器继续报成“可改为 const”。
#![allow(clippy::missing_const_for_thread_local)]

pub mod dylib;
mod ffi_buf;
pub mod js;
pub mod loader;
pub mod patch;
pub mod plan;
mod policy;
pub mod source;
mod staging;

pub use cordis_core::{CallerIdentity, MethodUnauthorized};
pub use ffi_buf::call_plugin_detailed;
pub use policy::{
    lifecycle_policy_for, method_allow_list_for, set_default_host_limits, set_dispatch_policy,
    set_dispatch_policy_for, set_host_limits_for, set_lifecycle_policy_for,
    set_method_allow_list_for, set_startup_policy_for, startup_policy_for, DispatchPolicy,
    FailureCategory, FailureDisposition, HostLimits, LifecyclePolicy, MethodAllowList, RowStartup,
    StartupPolicy,
};
pub(crate) use staging::mount_with_label;
pub use staging::{describe, mount, resolve_plugin_path, stage_plugin, staged_leaked, StagedFile};

#[cfg(test)]
use cordis_abi::ERROR_FORMAT_V1;
use ffi_buf::{
    args_to_json, call_op, call_op_read, call_plugin, decode_plugin_error, error_arg, fail,
    fail_payload, fail_to, invoke_raw, json_to_args, payload_of, reply, with_instance,
    with_instance_as, with_instance_payload, CallFailure, CallOutcome,
};
#[cfg(test)]
use ffi_buf::{read_retry_cap, CallBuffer, MAX_READ_RETRY_BYTES};
use policy::{dispatch_policy_for_ctx, host_limits_for_ctx};

use crate::source::PluginSourcePolicy;

use cordis_abi::{
    Blob, EntryFn, ErrorPayload, HostApi, Out, PluginDesc, ABI_VERSION, ENTRY_SYMBOL,
    KIND_DISPATCH, KIND_EVENT, KIND_IFACE, KIND_NEXT, KIND_STREAM_CREDIT, OP_APPLY, OP_CALLBACK,
    OP_DISPOSE, STATUS_ERR, STATUS_OK, STATUS_PENDING, STATUS_STREAM_GONE, STATUS_TOO_SMALL,
    STATUS_UNSUPPORTED,
};
use cordis_core::{
    BoxFuture, CancelToken, CleanupErrorKind, Config, Ctx, Disposable, Dyn, Error, ExternService,
    ExternValue, Fiber, Inject, Level, Next, Plugin, PluginKey, PushOutcome, Result, Ret,
    StreamObserver, StreamOutcome, StreamSink,
};
use serde_json::{json, Value};
use std::any::Any;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock, Weak};
use std::task::Poll;

// ------------------------------------------------------------ 实例句柄表（slab + 代际）

/// ABI 插件入口的函数指针类型。
///
/// 公开它是为了让**不经动态库的 ABI 形状插件**可被构造（M5-1 的边界测试夹具）：
/// 一个 `CallFn` + [`apply_via`] 等价于一个 `LoadedPlugin` 的装载路径——但
/// [`apply_via`] 是**文本错误**的 raw seam；若 `CallFn` 来自 `sdk::build_plugin`，
/// 用 [`apply_via_with_error_format`] 传 `PluginDesc::error_format_at(desc)`。
pub type CallFn = unsafe extern "C" fn(u64, u32, Blob, Blob, *mut Out) -> i32;

/// 派生句柄位布局（§4 M1，D8 的 seam 常量）：
/// - 实例句柄：`bit63=0 | bits[62:32]=index | bits[31:0]=generation`；
/// - 派生句柄：`bit63=1 | bits[62:0]=单调 seq（从 1 起，永不复用）`。
///
/// bit63 是两个句柄命名空间**不相交**的保证：实例表的查找对 bit63=1 必然
/// miss，派生表只收 bit63=1 的键——一个派生句柄永远不可能被解析成实例
/// （反之亦然），不存在"旧句柄复活成另一个上下文"的串线面。
pub const DERIVED_HANDLE_TAG: u64 = 1 << 63;

#[cfg(test)]
type ObserverGate = Arc<(std::sync::Barrier, std::sync::Barrier)>;

#[derive(Clone)]
struct Instance {
    ctx: Ctx,
    call: CallFn,
    /// 插件在 `PluginDesc.error_format` 里声明的结构化错误能力（0 = 纯文本）。
    error_format: u32,
    /// 在飞派发的记录表（M2/M2b/M2c）：`dispatch_id → DispatchRecord`。
    ///
    /// 键是**每次派发的 id**（不是注册时的回调 id，G17）——同一 handler 的
    /// 并发派发因此各自成条、互不覆盖。`Arc` 让 `Instance` 保持 `Clone`（G16）；
    /// 条目即生命周期：摘表 = 挂起续延/waterfall 帧/取消树一起释放
    /// （D3 的四条清理路径都落在这一张表上）。
    dispatches: Arc<Mutex<HashMap<u64, DispatchRecord>>>,
    /// **库生命周期保活**（弱引用）：事件快照和实际调用会 `upgrade()` 并持有
    /// 强引用。`Instance` 的普通克隆（服务句柄、续延等）不应拖住整个动态库——
    /// 否则一次未完成的 `PluginIface` 就会让暂存副本永远删不掉。
    ///
    /// **注册期内的强 owner** 是实例表槽位里的那一份（`Slab::slots` 的第三项），
    /// 随 `unregister_instance` 释放；`Instance` 克隆不延长它。
    ///
    /// 但"注册期内 `upgrade()` 必成功、注销后即失效"**只**在没有第三方持有该
    /// `Arc` 时成立。标准装载路径里 `LoadedPlugin` 自己还拿着一份
    /// `Arc<DynLib>`（`_library`），它可能比实例活得久：注销之后 `upgrade()`
    /// 仍可能成功。真正拦住对已注销实例的 FFI 的是 `closed` 与**句柄重查**
    /// （`with_instance` / `Slab::get` 查不到、或查到的是另一代实例），不是这条
    /// 弱引用失效——别把它当"注销后必 dead"的保证。
    /// `#[cfg(test)]` 的 `force_dead_weak` 会**故意**打破"注册期内 upgrade 必成功"，
    /// 只用于覆盖"dead Weak 时绝不碰裸 `CallFn`"那条纪律。
    keepalive: Option<Weak<dyn Any + Send + Sync>>,
    /// 实例是否已经进入关闭流程（不再接受新的入站回调）。
    closed: Arc<AtomicBool>,
    /// `OP_APPLY` 是否因 `LifecyclePolicy::apply_timeout_ms` 被宿主放弃等待。
    apply_timed_out: Arc<AtomicBool>,
    /// `OP_APPLY` 超时仍在运行时延后登记的 `OP_CLEANUP` 回调 id（保持 LIFO 顺序，待 `OP_APPLY` 退出后先于 `OP_DISPOSE` 执行）。
    deferred_cleanups: Arc<Mutex<Vec<u64>>>,
    /// 短锁登记调用，不跨 FFI 持锁；关闭与准入共用 dispatches 锁。
    lifecycle: Arc<Mutex<usize>>,
    calls_changed: Arc<tokio::sync::Notify>,
    /// Pending 推送按次数记债，不依赖业务记录是否已经摘除。
    credits: Arc<Mutex<HashMap<(u64, u64), usize>>>,
    #[cfg(test)]
    observer_gate: Arc<Mutex<Option<ObserverGate>>>,
    #[cfg(test)]
    outbound_gate: Arc<Mutex<Option<ObserverGate>>>,
    #[cfg(test)]
    cancel_gate: Arc<Mutex<Option<ObserverGate>>>,
    #[cfg(test)]
    close_drained: Arc<tokio::sync::Notify>,
    /// 独立于业务记录的任务退出凭证；锁序为 dispatches → tasks。
    tasks: Arc<Mutex<HashMap<u64, DispatchTask>>>,
    /// 注册时从所属 App 的 per-App 策略（或进程级回落）取下的**快照**。
    ///
    /// 快照而不是每次读注册表：已注册实例的策略不因后来调用
    /// [`set_dispatch_policy_for`] 而漂移；这也避免每次 settle / 出站派发都
    /// 去锁全局注册表。`Instance` 会随派生句柄/续延克隆，策略跟着一起走。
    dispatch_policy: DispatchPolicy,
    /// 注册时从所属 App 的容量准入表（或进程级回落）取下的**快照**（P0-3）。
    ///
    /// 与 [`DispatchPolicy`] 同一套纪律：运行期改表不追溯已注册实例。
    host_limits: HostLimits,
}

/// 一条挂起续延的结算通道：`Ok(值)` 或带结构化原因的错误。
type PendingSink = tokio::sync::oneshot::Sender<std::result::Result<Value, ErrorPayload>>;

/// 出站续延的类型：取消/卸载时必须回送调用方实际等待的那个 `kind`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ContinuationKind {
    Iface,
    Dispatch,
}

impl ContinuationKind {
    fn wire_name(self) -> &'static str {
        match self {
            Self::Iface => KIND_IFACE,
            Self::Dispatch => KIND_DISPATCH,
        }
    }
}

/// 一条出站续延：插件实例 + 回调 id + 它等待的回调类型。
struct Outbound {
    owner: u64,
    continuation: u64,
    kind: ContinuationKind,
    /// trace 父（ADR 0010 §2.2）：只有 iface 出站写它；续延交付时用它压 CALL_SCOPE。
    trace_parent: Option<u64>,
    /// 关联标识（ADR 0011）：续延交付时恢复，保证跨 `STATUS_PENDING` 的多会话归因不串线。
    trace_correlation: Option<Arc<str>>,
}

/// 一条在飞派发的记录（M2/M2b/M2c 共用）：
/// 挂起续延、waterfall 剩余链、出站节点与取消树都挂在它上面——
/// "条目在、东西就在；条目被摘，全部一起被丢"。
struct DispatchRecord {
    /// 入站挂起续延的结算通道（`ctx_fulfill` 用）。
    sink: Option<PendingSink>,
    /// waterfall 的**剩余链**（`ctx_next` 用；取走即"已经 next 过"）。
    frame: Option<Next>,
    /// 取消树的父节点：发起这次派发的那个在飞 dispatch（由宿主的**派发作用域**给出）。
    parent: Option<u64>,
    /// 出站派发登记（`ctx_dispatch`/`ctx_emit`/`ctx_call_iface` 发起）：
    /// 取消时把失败送回插件的续延。
    outbound: Option<Outbound>,
    /// 流（ADR 0001 §5）：这次派发对应的流（`call_iface_stream` 才有）。
    ///
    /// 挂在记录上而不是另立一张表，是为了让"条目即生命周期"这条既有纪律继续成立：
    /// 卸载排水 / 取消 / 超时 / 消费者离场这四条**同一条**摘表路径。
    stream: Option<StreamSink>,
}

/// 业务记录摘除之后，取消与退出凭证仍由实例持有。
struct DispatchTask {
    dispatch_id: u64,
    cancel: CancelToken,
    done: tokio::sync::oneshot::Receiver<()>,
}

struct TaskExit {
    registry: Weak<Mutex<HashMap<u64, DispatchTask>>>,
    id: u64,
    done: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Drop for TaskExit {
    fn drop(&mut self) {
        if let Some(registry) = self.registry.upgrade() {
            let mut tasks = registry.lock().unwrap();
            // 与 close 取走凭证互斥；close 已摘走时也必须发布退出。
            if let Some(done) = self.done.take() {
                let _ = done.send(());
            }
            tasks.remove(&self.id);
        }
    }
}

/// 调用者持有 dispatches 锁，且已确认 closed 与业务记录准入。
fn dispatch_task(instance: &Instance, dispatch_id: u64) -> (CancelToken, TaskExit) {
    static TASK_SEQ: AtomicU64 = AtomicU64::new(1);
    let id = TASK_SEQ.fetch_add(1, Ordering::Relaxed);
    let cancel = CancelToken::new();
    let (done, rx) = tokio::sync::oneshot::channel();
    instance.tasks.lock().unwrap().insert(
        id,
        DispatchTask {
            dispatch_id,
            cancel: cancel.clone(),
            done: rx,
        },
    );
    (
        cancel,
        TaskExit {
            registry: Arc::downgrade(&instance.tasks),
            id,
            done: Some(done),
        },
    )
}

#[cfg(test)]
#[allow(clippy::result_large_err)]
fn register_dispatch_task(
    instance: &Instance,
    dispatch_id: u64,
) -> std::result::Result<(CancelToken, TaskExit), ErrorPayload> {
    let table = instance.dispatches.lock().unwrap();
    if instance.closed.load(Ordering::SeqCst) {
        return Err(closed_admission_payload());
    }
    if !table.contains_key(&dispatch_id) {
        return Err(ErrorPayload::new("派发已结算：不再启动后台任务")
            .with_code("host_lifecycle")
            .with_category("lifecycle")
            .with_certainty("unknown"));
    }
    Ok(dispatch_task(instance, dispatch_id))
}

// 字段析构顺序是协议的一部分：即使首 poll panic 或任务从未被 poll，
// future 与交付闭包的捕获物也必须先于退出信号释放。
struct TaskWork<T, C> {
    future: BoxFuture<T>,
    on_done: Option<C>,
    _exit: TaskExit,
}

/// 句柄表：**槽位号 + 代际号**。
///
/// # 为什么不用裸指针或自增 id
/// - 裸指针会把宿主内存布局泄露给任意第三方二进制；
/// - 纯自增 id 在槽位复用后会**静默指向新实例**——旧句柄拿到的是别人的上下文，
///   这是最难查的一类 UAF 变体（尤其在有热重载的系统里：卸载后残留的句柄
///   可能在新插件装载后"复活"成一个合法但完全错误的调用）。
///
/// 把代际号编进句柄，陈旧句柄自然失效：`get` 会代际不匹配而拒绝。
#[derive(Default)]
struct Slab {
    /// `(代际号, 实例, 库保活强 owner)`。
    ///
    /// 保活强 owner 是**唯一**一份：`Instance::keepalive` 只是它的弱引用。
    /// 保存在这里（而不是 `Instance` 里）既让 `Instance` 保持廉价 `Clone`，
    /// 又让库的生命周期正好等于"注册期"——`unregister_instance` 是唯一释放点。
    slots: Vec<Option<(u32, Instance, Option<KeepaliveOwner>)>>,
    free: Vec<usize>,
    generation: u32,
}

/// 槽位里的库保活强 owner（真实装载路径下是 `Arc<DynLib>`，Drop 即 `FreeLibrary`）。
type KeepaliveOwner = Arc<dyn Any + Send + Sync>;

impl Slab {
    fn insert(&mut self, instance: Instance, owner: Option<KeepaliveOwner>) -> u64 {
        self.generation = self.generation.wrapping_add(1).max(1);
        let generation = self.generation;
        let index = match self.free.pop() {
            Some(index) => {
                self.slots[index] = Some((generation, instance, owner));
                index
            }
            None => {
                self.slots.push(Some((generation, instance, owner)));
                self.slots.len() - 1
            }
        };
        // 位布局契约：bits[62:32] 装 index，所以它必须小于 2^31——
        // 否则实例句柄会撞上 bit63（派生句柄命名空间）。2^31 个并发实例
        // 远超任何真实宿主，真撞上说明状态已经不可信，宁可 fail loud。
        assert!(
            index < (1usize << 31),
            "实例槽位号 {index} 溢出位布局（bits[62:32]）"
        );
        Self::handle(index, generation)
    }

    fn get(&self, handle: u64) -> Option<Instance> {
        // 解析顺序（位布局约定）：先实例表，且 bit63=1 的句柄**必然 miss**——
        // 派生句柄走派生表，绝不落进实例槽位。
        if handle & DERIVED_HANDLE_TAG != 0 {
            return None;
        }
        let (index, generation) = Self::split(handle);
        let (slot_generation, instance, _) = self.slots.get(index)?.as_ref()?;
        (*slot_generation == generation).then(|| instance.clone())
    }

    /// 取走槽位：`Instance` 与库保活强 owner 一起交给调用方在**锁外**析构。
    fn remove(&mut self, handle: u64) -> Option<(Instance, Option<KeepaliveOwner>)> {
        if handle & DERIVED_HANDLE_TAG != 0 {
            return None;
        }
        let (index, generation) = Self::split(handle);
        let (slot_generation, _, _) = self.slots.get(index)?.as_ref()?;
        if *slot_generation != generation {
            return None;
        }
        let taken = self.slots[index]
            .take()
            .map(|(_, instance, owner)| (instance, owner));
        if taken.is_some() {
            self.free.push(index);
        }
        taken
    }

    /// 测试探针用：拿到槽位里的实例与强 owner（仅 `#[cfg(test)]` 路径使用）。
    #[cfg(test)]
    fn slot_mut(&mut self, handle: u64) -> Option<(&mut Instance, &mut Option<KeepaliveOwner>)> {
        if handle & DERIVED_HANDLE_TAG != 0 {
            return None;
        }
        let (index, generation) = Self::split(handle);
        let (slot_generation, instance, owner) = self.slots.get_mut(index)?.as_mut()?;
        if *slot_generation != generation {
            return None;
        }
        Some((instance, owner))
    }

    fn live(&self) -> usize {
        self.slots.iter().filter(|slot| slot.is_some()).count()
    }

    /// 句柄 0 是**保留值**（表示"没有上下文"），代际号从 1 开始，二者永不冲突。
    fn handle(index: usize, generation: u32) -> u64 {
        ((index as u64) << 32) | generation as u64
    }

    /// 先掩掉 bit63 再拆位（位布局约定：bit63 区分实例/派生命名空间，
    /// 不参与 index/generation 的编码）。
    fn split(handle: u64) -> (usize, u32) {
        let handle = handle & !DERIVED_HANDLE_TAG;
        ((handle >> 32) as usize, handle as u32)
    }
}

fn instances() -> &'static RwLock<Slab> {
    static INSTANCES: OnceLock<RwLock<Slab>> = OnceLock::new();
    INSTANCES.get_or_init(|| RwLock::new(Slab::default()))
}

/// 注册一个 ABI 实例，并在**同一把实例表锁**内执行 per-App 容量准入。
///
/// 计数与插入必须原子：先数后插会在并发 apply 下越过 `maxInstancesPerApp`。
/// 返回 `Err` 时没有登记任何实例，调用方（`apply_via`）把它转成插件装载错误。
fn register_instance_with_keepalive(
    ctx: Ctx,
    call: CallFn,
    keepalive: Option<Arc<dyn Any + Send + Sync>>,
    error_format: u32,
) -> std::result::Result<u64, String> {
    let dispatch_policy = dispatch_policy_for_ctx(&ctx);
    let host_limits = host_limits_for_ctx(&ctx);
    let mut slab = instances().write().unwrap();
    let used = slab
        .slots
        .iter()
        .filter_map(|slot| slot.as_ref())
        .filter(|(_, instance, _)| Arc::ptr_eq(&instance.ctx.store, &ctx.store))
        .count();
    if used >= host_limits.max_instances_per_app {
        return Err(format!(
            "App 实例数已达上限 {}（maxInstancesPerApp），拒绝注册插件实例",
            host_limits.max_instances_per_app
        ));
    }
    // 强 owner 随注册**移入槽位**，实例里只留弱引用：注册期内 `upgrade()` 必成功，
    // `unregister_instance` 取走 owner 之后即失效。`apply_via`（keepalive=None）
    // 语义不变——none 仍然表示"没有库需要保活"。
    let weak = keepalive.as_ref().map(Arc::downgrade);
    let instance = Instance {
        ctx,
        call,
        error_format,
        dispatches: Arc::new(Mutex::new(HashMap::new())),
        keepalive: weak,
        closed: Arc::new(AtomicBool::new(false)),
        apply_timed_out: Arc::new(AtomicBool::new(false)),
        deferred_cleanups: Arc::new(Mutex::new(Vec::new())),
        lifecycle: Arc::new(Mutex::new(0)),
        calls_changed: Arc::new(tokio::sync::Notify::new()),
        credits: Arc::new(Mutex::new(HashMap::new())),
        #[cfg(test)]
        close_drained: Arc::new(tokio::sync::Notify::new()),
        #[cfg(test)]
        observer_gate: Arc::new(Mutex::new(None)),
        #[cfg(test)]
        outbound_gate: Arc::new(Mutex::new(None)),
        #[cfg(test)]
        cancel_gate: Arc::new(Mutex::new(None)),
        tasks: Arc::new(Mutex::new(HashMap::new())),
        dispatch_policy,
        host_limits,
    };
    Ok(slab.insert(instance, keepalive))
}

fn unregister_instance(handle: u64) {
    let removed = instances().write().unwrap().remove(handle);
    // 排水：`owner == root` 的派生句柄全部撤销。只撤句柄——不给派生句柄发
    // OP_DISPOSE（派生 ctx 没有自己的插件入口），子 fiber 的拆除由父 fiber 的
    // effect 链负责（`Fiber::new` 已把子 dispose 注册成父的 effect，且先于本
    // 函数跑完）。
    let entries = {
        let mut table = derived_table().lock().unwrap();
        let handles: Vec<_> = table
            .iter()
            .filter_map(|(&key, entry)| (entry.owner == handle).then_some(key))
            .collect();
        handles
            .into_iter()
            .filter_map(|key| table.remove(&key))
            .collect::<Vec<_>>()
    };
    // 最后强引用的析构可能重入宿主，两个表锁都必须已释放。
    drop(entries);
    // 槽位里取出的两样东西在这里才 drop：`Instance` 的析构可能重入宿主；
    // 库 owner 的析构就是 `FreeLibrary`（第三方二进制代码），同样不能带表锁跑。
    if let Some((instance, owner)) = removed {
        drop(instance);
        drop(owner);
    }
}

/// 测试探针：把实例的库保活强 owner 摘掉，并装进一个**已释放**的 `Weak`。
///
/// 前提随所有权模型改变：强 owner 现在住在实例表槽位里，注册期内弱引用不可能
/// 自然失效。想覆盖"dead Weak 时绝不碰裸 `CallFn`"这条既有纪律，只能显式构造。
/// 返回那个 dead Weak，供调用方同步到自己手里的 `Instance` 克隆（克隆不共享槽位）。
///
/// 摘下来的 owner 在锁外 drop：它就是"库"的析构。
#[cfg(test)]
fn force_dead_weak(handle: u64) -> Option<Weak<dyn Any + Send + Sync>> {
    let dead: Arc<dyn Any + Send + Sync> = Arc::new(());
    let weak = Arc::downgrade(&dead);
    drop(dead);
    let owner = {
        let mut slab = instances().write().unwrap();
        let (instance, owner) = slab.slot_mut(handle)?;
        instance.keepalive = Some(weak.clone());
        owner.take()
    };
    drop(owner);
    Some(weak)
}

/// 测试探针：把槽位里的库保活强 owner **摘走并交给调用方**（实例仍登记，
/// `Instance::keepalive` 的弱引用仍指向它——这里**不**装 dead weak）。
///
/// 用途：让测试重新模拟"强引用只由外部（测试侧）持有"的旧所有权模型。O4 把
/// owner 移进槽位之后，槽位那份强引用会替测试一直兜住库，于是"call lease /
/// close lease 是否真的生效"这类断言（`DropProbe` 的 `alive`）变成恒真。
/// 取出后由测试在指定时刻 drop，`alive` 才重新只由 lease 决定。
///
/// 与 [`force_dead_weak`] 同族，区别只在弱引用是否还有效。它同样会（仅在测试中）
/// 打破"注册期内 `upgrade()` 必成功"的不变量：owner 被摘到测试手里，槽位不再替它兜着。
#[cfg(test)]
fn take_slot_owner(handle: u64) -> Option<KeepaliveOwner> {
    let mut slab = instances().write().unwrap();
    let (_, owner) = slab.slot_mut(handle)?;
    owner.take()
}

/// 按**上下文身份**找回实例句柄（M4-5 的 volatile 推送要用它）。
///
/// 为什么不在 `LoadedPlugin` 上缓存句柄：同一个插件对象可以挂多行（多实例），
/// `&self` 里存不下"当前实例"；而句柄由 `apply_via` 在装载时注册、随 fiber 卸载注销，
/// 唯一稳定的关联就是 ctx 本身。表很小，线性查找足够（volatile 提交是低频操作）。
fn instance_handle_of(ctx: &Ctx) -> Option<u64> {
    let slab = instances().read().unwrap();
    for index in 0..slab.slots.len() {
        if let Some((generation, instance, _)) =
            slab.slots.get(index).and_then(|slot| slot.as_ref())
        {
            if Arc::ptr_eq(&instance.ctx, ctx) {
                return Some(Slab::handle(index, *generation));
            }
        }
    }
    None
}

/// 当前存活实例数（诊断用）。
pub fn loaded_instances() -> usize {
    instances().read().unwrap().live()
}

// ------------------------------------------------------------ 派生句柄表（M1）

/// 派生表的一条记录：isolate/intercept 派生的子 context，或 `ctx_use` 挂的子 fiber。
struct DerivedEntry {
    /// 创建者（根）实例句柄：`unregister_instance(root)` 时按它排水。
    /// 从派生句柄再派生时向上传播，保证排水总能追到根。
    owner: u64,
    /// 派生 context（isolate/intercept 子 context；`ctx_use` 则是子 fiber 的 ctx）。
    ctx: Ctx,
    /// 继承的调用方实例身份：回调 id 的派发仍走原实例（realm 隔离不隔离身份）。
    instance: Instance,
    /// `ctx_use` 挂的子 fiber（isolate/intercept 派生条目为 `None`）。
    fiber: Option<Weak<Fiber>>,
}

fn derived_table() -> &'static Mutex<HashMap<u64, DerivedEntry>> {
    static DERIVED: OnceLock<Mutex<HashMap<u64, DerivedEntry>>> = OnceLock::new();
    DERIVED.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 派生句柄 seq：进程内单调、从 1 起、**永不复用**——排水之后旧句柄不会
/// 被解析成新派生上下文（与实例表的代际号同一动机，实现更简单：只增不减）。
fn next_derived_seq() -> u64 {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    SEQ.fetch_add(1, Ordering::SeqCst) + 1
}

fn register_derived(owner: u64, ctx: Ctx, instance: Instance, fiber: Option<Weak<Fiber>>) -> u64 {
    let handle = DERIVED_HANDLE_TAG | next_derived_seq();
    derived_table().lock().unwrap().insert(
        handle,
        DerivedEntry {
            owner,
            ctx,
            instance,
            fiber,
        },
    );
    handle
}

/// 句柄解析的统一产物：派生句柄与实例句柄在这里汇成同一条路径。
struct Resolved {
    ctx: Ctx,
    instance: Instance,
    /// 根实例句柄（派生条目向上传播；实例句柄就是它自己）。
    owner: u64,
}

/// 统一解析：**先实例表**（bit63=1 必然 miss），**后派生表**（位布局约定）。
fn resolve_handle(handle: u64) -> Option<Resolved> {
    if handle & DERIVED_HANDLE_TAG != 0 {
        let table = derived_table().lock().ok()?;
        let entry = table.get(&handle)?;
        Some(Resolved {
            ctx: entry.ctx.clone(),
            instance: entry.instance.clone(),
            owner: entry.owner,
        })
    } else {
        let instance = instances().read().ok()?.get(handle)?;
        Some(Resolved {
            ctx: instance.ctx.clone(),
            instance,
            owner: handle,
        })
    }
}

/// [`with_instance`] 的统一解析版：`ctx_get` / `ctx_require` 与 M1 的五个
/// 新能力经它接受派生句柄；只在实例上成立的 op 仍走 [`with_instance`]
/// （派生句柄在那里自然落进"已失效"分支——这正是位布局要的性质）。
///
/// 与 `with_instance` 同一套纪律：克隆后才执行（执行期间不持锁）、
/// panic 兜底截在 ABI 边界之内。
unsafe fn with_resolved<T>(
    handle: u64,
    out: *mut Out,
    f: impl FnOnce(&Resolved) -> std::result::Result<T, String>,
) -> i32
where
    T: AsRef<[u8]>,
{
    with_resolved_as(handle, out, "host_call", "protocol", f)
}

/// 与 [`with_resolved`] 相同，但允许调用点给出更具体的错误 `code` / `category`。
#[allow(clippy::result_large_err)]
unsafe fn with_resolved_as<T>(
    handle: u64,
    out: *mut Out,
    code: &str,
    category: &str,
    f: impl FnOnce(&Resolved) -> std::result::Result<T, String>,
) -> i32
where
    T: AsRef<[u8]>,
{
    with_resolved_payload(handle, out, |resolved| {
        f(resolved).map_err(|message| {
            ErrorPayload::new(message)
                .with_code(code)
                .with_category(category)
        })
    })
}

/// [`with_resolved_as`] 的载荷版：调用点可以直接给出完整的 [`ErrorPayload`]。
///
/// 容量拒绝经它返回 `host_capacity` / `capacity` / `not_executed`，而不是被
/// 压平成一条只有消息的字符串错误。
#[allow(clippy::result_large_err)]
unsafe fn with_resolved_payload<T>(
    handle: u64,
    out: *mut Out,
    f: impl FnOnce(&Resolved) -> std::result::Result<T, ErrorPayload>,
) -> i32
where
    T: AsRef<[u8]>,
{
    let resolved = resolve_handle(handle);
    match resolved {
        None => fail(
            out,
            &format!(
                "ctx 句柄 {handle} 已失效（插件已卸载、派生句柄已回收，或句柄来自上一代实例）"
            ),
        ),
        Some(resolved) => {
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&resolved))) {
                Ok(Ok(bytes)) => reply(out, bytes.as_ref()),
                Ok(Err(payload)) => fail_payload(out, resolved.instance.error_format, &payload),
                Err(payload) => fail_to(
                    out,
                    resolved.instance.error_format,
                    &format!(
                        "宿主内部 panic 已在 ABI 边界截获（插件侧看到的就是这次失败）：{}",
                        cordis_core::panic_message(&*payload)
                    ),
                    "host_panic",
                    "unknown",
                ),
            }
        }
    }
}

// ------------------------------------------------------------ 宿主函数表

static HOST_API: HostApi = HostApi {
    abi_version: ABI_VERSION,
    size: std::mem::size_of::<HostApi>() as u32,
    ctx_get: Some(host_ctx_get),
    ctx_provide: Some(host_ctx_provide),
    ctx_on: Some(host_ctx_on),
    ctx_effect: Some(host_ctx_effect),
    ctx_emit: Some(host_ctx_emit),
    log: Some(host_log),
    ctx_provide_iface: Some(host_ctx_provide_iface),
    ctx_call_iface: Some(host_ctx_call_iface),
    ctx_dispatch: Some(host_ctx_dispatch),
    ctx_require: Some(host_ctx_require),
    ctx_set: Some(host_ctx_set),
    // ABI v2 尾追加（附录 A #12–#16，M1）。
    ctx_isolate: Some(host_ctx_isolate),
    ctx_intercept: Some(host_ctx_intercept),
    ctx_use: Some(host_ctx_use),
    ctx_dispose: Some(host_ctx_dispose),
    ctx_dispose_self: Some(host_ctx_dispose_self),
    ctx_fulfill: Some(host_ctx_fulfill),
    ctx_next: Some(host_ctx_next),
    ctx_abort: Some(host_ctx_abort),
    // 流（ADR 0001 §5）：`call_iface_stream` 的提供者侧三件套。
    ctx_stream_push: Some(host_stream_push),
    ctx_stream_close: Some(host_stream_close),
    ctx_stream_fail: Some(host_stream_fail),
    error_format: cordis_abi::ERROR_FORMAT_V1,
    reserved: 0,
    ctx_stream_fail_payload: Some(host_stream_fail_payload),
};

/// 宿主函数表（`HostApi`）：插件的 `attach` 收到的就是它。
///
/// 公开它是给**测试夹具**用：假插件入口（[`CallFn`]）可以直接调
/// `host_api().ctx_effect` 等函数，从而覆盖"插件侧调用宿主"的真实路径，
/// 而不是另造一条只有测试才走的旁路。
pub fn host_api() -> &'static HostApi {
    &HOST_API
}

// ------------------------------------------------------------ 流（ADR 0001 §5）

/// 宿主侧的流观察者：把 core 的流事件翻译成两件事——
/// (a) **送回插件的 credit 回调**（跨 ABI 的提供者只有这条路能知道"又能推了"/
/// "流没了"）；(b) **结算这次派发**（流结束 ⇒ 记录连同 sink 一起被摘）。
struct HostStreamObserver {
    handle: u64,
    dispatch_id: u64,
}

impl StreamObserver for HostStreamObserver {
    fn credit_available(&self, on_credit: u64, window: usize) {
        deliver_stream_credit(self.handle, self.dispatch_id, on_credit, window);
    }

    fn finished(&self, outcome: &StreamOutcome, waiting: Vec<u64>) {
        // ① 还在等额度的推送先被驳回：插件会一直等一个永远不来的位置，否则。
        for on_credit in waiting {
            deliver_stream_credit_gone(self.handle, self.dispatch_id, on_credit, outcome);
        }
        // ② 流结束 ⇒ 这次派发结束（`remove` 幂等：宿主自己收回的路径早已摘过）。
        let instance = instances().read().unwrap().get(self.handle);
        if let Some(instance) = instance {
            let removed = instance
                .dispatches
                .lock()
                .unwrap()
                .remove(&self.dispatch_id);
            drop(removed);
        }
    }
}

/// 流收尾形态的一句话说明（送回插件时用）。
fn stream_outcome_message(outcome: &StreamOutcome) -> String {
    match outcome {
        StreamOutcome::Closed => "流已经正常收尾".to_string(),
        StreamOutcome::Failed(reason) => format!("流已经失败收尾：{reason}"),
        StreamOutcome::Abandoned => "消费者已经不再读这条流".to_string(),
    }
}

/// 一次**单向投递**给插件（流的两条通知共用）。
///
/// 与 `deliver_next` 同一套纪律：把这次派发留在作用域里（插件在回调里再发派发时
/// 父节点仍是它）；**先取 call、放锁、后调用**（插件会重入宿主：推块、再登记额度）。
fn deliver_to_plugin(handle: u64, dispatch_id: u64, on_credit: u64, payload: &str, what: &str) {
    let _scope = scope_push(handle, dispatch_id);
    let target = instances().read().unwrap().get(handle);
    #[cfg(test)]
    if let Some(instance) = &target {
        let gate = instance.observer_gate.lock().unwrap().take();
        if let Some(gate) = gate {
            gate.0.wait();
            gate.1.wait();
        }
    }
    let Some(instance) = target else { return };
    let Some(_active) = admit_call(&instance) else {
        return;
    };
    {
        let mut credits = instance.credits.lock().unwrap();
        // 这张票不存在 ⇒ 这条续延已经被结算过（或从来没登记），不再投递。
        if !release_credit(&mut credits, dispatch_id, on_credit) {
            return;
        }
    }
    if let Err(message) = unsafe {
        call_plugin(
            instance.call,
            handle,
            OP_CALLBACK,
            payload,
            instance.error_format,
        )
    } {
        eprintln!("[cordis] {what} 投递失败: {message}");
    }
}

/// "又有位置了"：`window` = 当前可用额度，`on_credit` = 推送时登记的续延 id。
fn deliver_stream_credit(handle: u64, dispatch_id: u64, on_credit: u64, window: usize) {
    let payload = json!({
        "kind": KIND_STREAM_CREDIT,
        "id": on_credit,
        "args": [{ "result": { "window": window, "dispatch_id": dispatch_id } }],
    })
    .to_string();
    deliver_to_plugin(handle, dispatch_id, on_credit, &payload, "流额度");
}

/// "这条流已经不在了"：同一个续延 id，载荷里是 `error`（插件侧会走
/// `StreamCredit::Gone` 分支，然后停手）。
fn deliver_stream_credit_gone(
    handle: u64,
    dispatch_id: u64,
    on_credit: u64,
    outcome: &StreamOutcome,
) {
    let payload = json!({
        "kind": KIND_STREAM_CREDIT,
        "id": on_credit,
        "args": [{ "error": stream_outcome_message(outcome) }],
    })
    .to_string();
    deliver_to_plugin(handle, dispatch_id, on_credit, &payload, "流驳回");
}

/// 流式 op 的统一外壳：与 [`with_instance`] 同一套纪律（克隆后执行、panic 截在
/// ABI 边界内），但**状态码由这次调用自己决定**——推块有三态
/// （`STATUS_OK` / `STATUS_PENDING` / `STATUS_STREAM_GONE`），把它们压成
/// "成功/失败"会丢掉提供者必须区分的信息。
///
/// 错误走**载荷**通道（与 [`with_instance_payload`] 同一条）：宿主在这里写的是
/// 一个完整的 `ErrorPayload`（`code` / `category` / `certainty` / `detail`），
/// 而不是一句压平的话。
///
/// 但**结构化字段当前并不透出到插件侧**：`ctx.stream_push` 走
/// `invoke_statused`，它只保留 `ErrorPayload.error` 这段文本（`StreamPush`
/// 没有错误面）。所以插件今天看到的是文本；要按 `certainty` 决定重试，
/// 得等 SDK 把它暴露成结果类型里的字段。
#[allow(clippy::result_large_err)]
unsafe fn with_stream_instance(
    handle: u64,
    out: *mut Out,
    f: impl FnOnce(&Instance) -> std::result::Result<i32, ErrorPayload>,
) -> i32 {
    let instance = match instances().read() {
        Ok(table) => table.get(handle),
        Err(_) => return fail(out, "宿主实例表锁已中毒（已在 ABI 边界截获）"),
    };
    match instance {
        None => fail(
            out,
            &format!("ctx 句柄 {handle} 已失效（插件已卸载，或句柄来自上一代实例）"),
        ),
        Some(instance) => {
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&instance))) {
                Ok(Ok(status)) => status,
                Ok(Err(payload)) => fail_payload(out, instance.error_format, &payload),
                Err(payload) => fail_to(
                    out,
                    instance.error_format,
                    &format!(
                        "宿主内部 panic 已在 ABI 边界截获（插件侧看到的就是这次失败）：{}",
                        cordis_core::panic_message(&*payload)
                    ),
                    "host_panic",
                    "unknown",
                ),
            }
        }
    }
}

/// 流式 op 的协议错误：与 `fail_to(host_call/protocol)` 同形，只是走载荷通道。
fn stream_protocol_error(message: String) -> ErrorPayload {
    ErrorPayload::new(message)
        .with_code("host_call")
        .with_category("protocol")
}

/// 取一条派发记录上的流。三种结局都要能区分：
/// - `None`：**这次派发已经结束**（收尾/取消/卸载收回）⇒ 调用方报 `STREAM_GONE`；
/// - `Some(None)`：派发在，但**它不是流**（调用方走的是 `ctx_call_iface`）⇒ 可读错误；
/// - `Some(Some(sink))`：正常路径。
///
/// 锁只在这一条语句里持有：返回的是克隆，调用方在**放锁之后**才碰流。
fn stream_of(instance: &Instance, dispatch_id: u64) -> Option<Option<StreamSink>> {
    instance
        .dispatches
        .lock()
        .unwrap()
        .get(&dispatch_id)
        .map(|record| record.stream.clone())
}

/// 每条流最多挂多少条**等待窗口的推送**（credit 债）。
///
/// 它不是窗口大小（`STREAM_WINDOW` 是 core 的缓冲块数），也不是派发/流的数量
/// （那些在 `HostLimits` 里）。它挡的是"窗口满之后不断换 `on_credit` id 重推"：
/// 每推一次，core 的 `waiting` 队列、宿主的 `credits` 表与 SDK 的 continuation
/// 表各长一条，三处都按**推送次数**增长，`maxDispatchesPerInstance` /
/// `maxStreamsPerInstance` 一个都盖不住。
///
/// 64 的取法：正常背压只需要"每个窗口位置一个等待者"（窗口默认 4），64 留了一个
/// 数量级余量；再多就只可能是坏插件在循环。私有常量而不是 `HostLimits` 字段：
/// 这是宿主对**自己内存**的硬边界，不该变成可配置项，也不该改公开结构形状。
///
/// 为什么 64 是**结构性**上界而不只是"经验值"：真正保证它的是**检查与记账同在一个
/// `credits` 临界区**——`reserve_credit` 一次持锁做"看余额 + 记账"，两者之间没有
/// 别的推送插进来的窗口。`dispatches` 锁只让**同一实例的推送串行**，因此并发维度
/// 没有额外风险；上界本身不来自那把锁。也正因为如此，这里不写"多 producer 并发"
/// 用例：那样的用例只能证明锁存在，测不到上限（上限是临界区决定的，不是并发压出来的）。
///
/// 正常路径不漏票：`reserve_credit` 之后紧跟 `push_awaiting`，`Sent` 与 `Gone` 都
/// 当场回滚，只有 `Pending` 兑现。一旦 `push_awaiting` **内部** panic（记账之后唯一
/// 可能 panic 的位置），`credits` 与 `dispatches` 两把 std `Mutex` 都在作用域内，
/// 于是**两把锁都会中毒**：本实例此后的推送与卸载都会在 `.lock().unwrap()` 上 panic。
/// 这是既有的 fail-loud 形态（不承诺恢复），不是"随那次卸载一起消失"的可恢复泄漏。
const MAX_OUTSTANDING_CREDITS_PER_STREAM: usize = 64;

/// 一条流当前记了几张 credit 票（**按次数**求和：重复 id 不去重）。
fn outstanding_credits(credits: &HashMap<(u64, u64), usize>, dispatch_id: u64) -> usize {
    credits
        .iter()
        .filter(|((dispatch, _), _)| *dispatch == dispatch_id)
        .map(|(_, count)| *count)
        .sum()
}

/// 给这次挂起**预留**一张票：上限检查与记账在同一把 `credits` 锁里完成，
/// 因此"检查通过"与"记债"之间不存在第二个推送插进来的窗口（无 TOCTOU）。
///
/// 调用方必须先预留、再 `push_awaiting`：`Sent` 与 `Gone` 都回滚，只有 `Pending`
/// 兑现。拒绝时**不记账**，返回与容量准入同形的载荷（同 code / category /
/// certainty / detail），文案则说清被拒的是**这一块**而不是一次派发。
#[allow(clippy::result_large_err)]
fn reserve_credit(
    credits: &mut HashMap<(u64, u64), usize>,
    dispatch_id: u64,
    on_credit: u64,
) -> std::result::Result<(), ErrorPayload> {
    let outstanding = outstanding_credits(credits, dispatch_id);
    if outstanding >= MAX_OUTSTANDING_CREDITS_PER_STREAM {
        // 文案只针对**这一块**：派发本身没有问题，插件代码也正在执行。
        return Err(capacity_payload_with(
            format!(
                "宿主容量上限：超出每流挂起推送上限 maxOutstandingCreditsPerStream 已用 \
                 {outstanding}/{MAX_OUTSTANDING_CREDITS_PER_STREAM}，这块未被接受"
            ),
            "maxOutstandingCreditsPerStream",
            outstanding,
            MAX_OUTSTANDING_CREDITS_PER_STREAM,
        ));
    }
    *credits.entry((dispatch_id, on_credit)).or_default() += 1;
    Ok(())
}

/// 还一张票。返回 `false` = 这张票根本不存在（调用方据此决定"还要不要投递"）。
fn release_credit(
    credits: &mut HashMap<(u64, u64), usize>,
    dispatch_id: u64,
    on_credit: u64,
) -> bool {
    let key = (dispatch_id, on_credit);
    let Some(count) = credits.get_mut(&key) else {
        return false;
    };
    *count -= 1;
    if *count == 0 {
        credits.remove(&key);
    }
    true
}

/// #20（ADR 0001 §5）：往这次派发对应的流里推一块。
#[allow(clippy::result_large_err)]
unsafe extern "C" fn host_stream_push(
    handle: u64,
    dispatch_id: u64,
    chunk: Blob,
    on_credit: u64,
    out: *mut Out,
) -> i32 {
    let value: Value = match serde_json::from_slice(chunk.as_bytes()) {
        Ok(value) => value,
        Err(err) => return fail(out, &format!("流数据块不是合法 JSON: {err}")),
    };
    with_stream_instance(handle, out, move |instance| {
        let table = instance.dispatches.lock().unwrap();
        if instance.closed.load(Ordering::SeqCst) {
            return Ok(STATUS_STREAM_GONE);
        }
        match table.get(&dispatch_id).map(|record| record.stream.as_ref()) {
            None => Ok(STATUS_STREAM_GONE),
            Some(None) => Err(stream_protocol_error(format!(
                "dispatch {dispatch_id} 不是流：这次调用走的是值路径（ctx_call_iface）。\
             要按流调用请用 ctx.call_iface_stream"
            ))),
            Some(Some(sink)) => {
                // push_awaiting 不调用 observer；finish/credit 会先放 core 锁。
                // 先预留一张票再推：`Sent`（块直接被接受）与 `Gone`（流已不在）
                // 都回滚，只有 `Pending`（真的挂起了）兑现——拒绝路径绝不会 +1。
                let mut credits = instance.credits.lock().unwrap();
                reserve_credit(&mut credits, dispatch_id, on_credit)?;
                Ok(match sink.push_awaiting(value, on_credit) {
                    PushOutcome::Pending => STATUS_PENDING,
                    PushOutcome::Sent => {
                        release_credit(&mut credits, dispatch_id, on_credit);
                        STATUS_OK
                    }
                    PushOutcome::Gone => {
                        release_credit(&mut credits, dispatch_id, on_credit);
                        STATUS_STREAM_GONE
                    }
                })
            }
        }
    })
}

/// #21（ADR 0001 §5）：正常收尾——消费者读完已缓冲的块之后拿到 `Ok(None)`。
#[allow(clippy::result_large_err)]
unsafe extern "C" fn host_stream_close(handle: u64, dispatch_id: u64, out: *mut Out) -> i32 {
    with_stream_instance(handle, out, move |instance| {
        match stream_of(instance, dispatch_id) {
            None => Ok(STATUS_STREAM_GONE),
            Some(None) => Err(stream_protocol_error(format!(
                "dispatch {dispatch_id} 不是流：收尾只对 ctx.call_iface_stream 开的流有意义"
            ))),
            Some(Some(sink)) => Ok(match sink.close() {
                PushOutcome::Sent => STATUS_OK,
                _ => STATUS_STREAM_GONE,
            }),
        }
    })
}

/// #22（ADR 0001 §5）：纯文本失败收尾——`reason` 原样直达消费者（缓冲丢弃）。
///
/// 始终保持纯文本语义，即使插件声明 `ERROR_FORMAT_V1` 且错误文本恰好长得像 JSON
/// 也不重解释；需要传递结构化 [`ErrorPayload`] 请走独立尾追加入口
/// `ctx_stream_fail_payload`（#23）。
#[allow(clippy::result_large_err)]
unsafe extern "C" fn host_stream_fail(
    handle: u64,
    dispatch_id: u64,
    reason: Blob,
    out: *mut Out,
) -> i32 {
    let reason = String::from_utf8_lossy(reason.as_bytes()).into_owned();
    with_stream_instance(handle, out, move |instance| {
        match stream_of(instance, dispatch_id) {
            None => Ok(STATUS_STREAM_GONE),
            Some(None) => Err(stream_protocol_error(format!(
                "dispatch {dispatch_id} 不是流：失败收尾只对 ctx.call_iface_stream 开的流有意义"
            ))),
            Some(Some(sink)) => Ok(match sink.fail(reason) {
                PushOutcome::Sent => STATUS_OK,
                _ => STATUS_STREAM_GONE,
            }),
        }
    })
}

/// #23（ADR 0004 扩展）：结构化错误载荷失败收尾——按 [`ErrorPayload`] JSON 解码后
/// 经 [`StreamSink::fail_error`] 交付给消费者（缓冲丢弃）。
///
/// 与 `ctx_stream_fail`（纯文本）分离，保证：
/// - `0.1.5` 插件经 `ctx_stream_fail` 传入的普通 JSON 错误文本不被误解析；
/// - `0.1.6` 插件在 `0.1.5` 宿主（仅声明 `ERROR_FORMAT_V1`、无本字段）上自动降级为纯文本。
#[allow(clippy::result_large_err)]
unsafe extern "C" fn host_stream_fail_payload(
    handle: u64,
    dispatch_id: u64,
    payload: Blob,
    out: *mut Out,
) -> i32 {
    let parsed = payload.try_as_str().ok().and_then(ErrorPayload::from_json);
    with_stream_instance(handle, out, move |instance| {
        let Some(payload) = parsed else {
            return Err(stream_protocol_error(
                "ctx_stream_fail_payload 需要合法的 ErrorPayload JSON 载荷".to_string(),
            ));
        };
        match stream_of(instance, dispatch_id) {
            None => Ok(STATUS_STREAM_GONE),
            Some(None) => Err(stream_protocol_error(format!(
                "dispatch {dispatch_id} 不是流：失败收尾只对 ctx.call_iface_stream 开的流有意义"
            ))),
            Some(Some(sink)) => Ok(match sink.fail_error(payload) {
                PushOutcome::Sent => STATUS_OK,
                _ => STATUS_STREAM_GONE,
            }),
        }
    })
}

/// **宿主主动收回一次派发**：摘掉记录，并把它上面的流以失败收场（如果有）。
///
/// 这是"取消 / 超时 / 消费者离场"三条路径共用的收口——流必须以**错误**结束
/// （`Ok(None)` 只能来自提供者的正常收尾），否则调用方会把"被收回"读成"流讲完了"。
fn revoke_dispatch(instance: &Instance, dispatch_id: u64, payload: &ErrorPayload) {
    // 取消/超时也必须级联后代：父记录被摘掉后，子记录仍可能继续运行并
    // 在取消之后交付。`host_ctx_abort` 的递归收集逻辑是唯一真相。
    let _ = drain_dispatch_tree(instance, dispatch_id, payload);
}

/// 摘除 `root` 及其全部后代，并按统一的收尾顺序结算：
/// 流以失败结束、挂起续延收到错误、出站续延收到 abort。
///
/// 返回 `None` 表示 `root` 已终态，或关闭已认领余下记录。
fn drain_dispatch_tree(instance: &Instance, root: u64, payload: &ErrorPayload) -> Option<usize> {
    let _active;
    let mut removed = Vec::new();
    {
        let mut table = instance.dispatches.lock().unwrap();
        if instance.closed.load(Ordering::SeqCst) || !table.contains_key(&root) {
            return None;
        }
        // Host 撤销与 FFI 准入分离：dead Weak 仍须排水并让 close 等待，
        // 但 Err 明确禁止调用裸函数指针（不同于 fake target 的 Ok(None)）。
        let lease = call_lease(instance);
        *instance.lifecycle.lock().unwrap() += 1;
        _active = ActiveCall {
            active: instance.lifecycle.clone(),
            changed: instance.calls_changed.clone(),
            lease,
        };
        let mut victims = vec![root];
        let mut index = 0;
        while index < victims.len() {
            let current = victims[index];
            for (id, record) in table.iter() {
                if record.parent == Some(current) && !victims.contains(id) {
                    victims.push(*id);
                }
            }
            index += 1;
        }
        let cancellations: Vec<_> = instance
            .tasks
            .lock()
            .unwrap()
            .values()
            .filter(|task| victims.contains(&task.dispatch_id))
            .map(|task| task.cancel.clone())
            .collect();
        if _active.lease.is_err() {
            instance
                .credits
                .lock()
                .unwrap()
                .retain(|(dispatch_id, _), _| !victims.contains(dispatch_id));
        }
        for id in victims {
            if let Some(record) = table.remove(&id) {
                removed.push(record);
            }
        }
        drop(table);
        for cancel in cancellations {
            cancel.cancel();
        }
    }
    #[cfg(test)]
    {
        let gate = instance.cancel_gate.lock().unwrap().take();
        if let Some(gate) = gate {
            gate.0.wait();
            gate.1.wait();
        }
    }
    let count = removed.len();
    for mut record in removed {
        if let Some(sink) = &record.stream {
            sink.fail_error(payload.clone());
        }
        if let Some(tx) = record.sink.take() {
            let _ = tx.send(Err(payload.clone()));
        }
        if _active.lease.is_ok() {
            if let Some(outbound) = record.outbound {
                deliver_abort_direct(instance, outbound, payload);
            }
        }
        // `record.frame` 随记录一起被丢：剩余链的 future 在这里释放。
    }
    Some(count)
}

/// 一个实例当前还挂着的**流**数（ADR 0001 §5 的观测口）。
///
/// 与 [`pending_len`] / [`waterfall_len`] 同一口径：句柄已失效 ⇒ 0
/// （实例连同它的记录一起被排水）。
pub fn streams_len(handle: u64) -> usize {
    count_dispatches(handle, |record| record.stream.is_some())
}

// ------------------------------------------------------------ 入站异步（M2）

/// 铸造一次**派发**的 id（M2，D3）：单调、进程内全局。
///
/// 为什么不用注册时的回调 id 当键：那个 id 跨派发不变（G17），并发挂起会互相
/// 覆盖；`dispatch_id` 每次入站回调都是新的，且**跨插件库不碰撞**（全局计数器）。
static DISPATCH_SEQ: AtomicU64 = AtomicU64::new(1);

fn next_dispatch_id() -> u64 {
    DISPATCH_SEQ.fetch_add(1, Ordering::SeqCst)
}

/// 一条容量拒绝的结构化骨架：`host_capacity` / `capacity` / `not_executed`。
///
/// 文案由调用方给：派发准入拒的是**一次派发**，每流推送上限拒的是**一块数据**。
/// 复用同一句话会让插件读到"拒绝这次派发（插件代码未执行）"——对推送它不成立。
///
/// `not_executed` 的读法随 `scope` 走：派发准入是"插件代码不会执行"，而 credit 拒绝
/// （[`reserve_credit`]）是"**这块数据未入队**"——插件代码当前正在执行。别把它读成
/// "插件未执行"。
fn capacity_payload_with(message: String, scope: &str, used: usize, limit: usize) -> ErrorPayload {
    ErrorPayload::new(message)
        .with_code("host_capacity")
        .with_category("capacity")
        .with_certainty("not_executed")
        .with_detail(json!({ "scope": scope, "used": used, "limit": limit }))
}

/// 派发准入的容量拒绝（`maxDispatchesPerInstance` 等）。
fn capacity_payload(scope: &str, used: usize, limit: usize) -> ErrorPayload {
    capacity_payload_with(
        format!("宿主容量上限：{scope} 已用 {used}/{limit}，拒绝这次派发（插件代码未执行）"),
        scope,
        used,
        limit,
    )
}

fn closed_admission_payload() -> ErrorPayload {
    ErrorPayload::new("插件正在卸载：不再接受新工作")
        .with_code("host_lifecycle")
        .with_category("lifecycle")
        .with_certainty("not_executed")
}

/// 派发记录的唯一插入口：关闭、容量检查与插入在同一把表锁内完成。
///
/// 拒绝时返回可读的结构化载荷，**不插入**任何记录；调用方必须在这之后直接
/// 返回错误，绝不能继续调用插件代码。
#[allow(clippy::result_large_err)]
fn insert_dispatch(
    instance: &Instance,
    dispatch_id: u64,
    record: DispatchRecord,
) -> std::result::Result<(), (ErrorPayload, DispatchRecord)> {
    let mut table = instance.dispatches.lock().unwrap();
    insert_dispatch_locked(instance, &mut table, dispatch_id, record)
}

#[allow(clippy::result_large_err)]
fn insert_dispatch_task(
    instance: &Instance,
    dispatch_id: u64,
    record: DispatchRecord,
) -> std::result::Result<(CancelToken, TaskExit), (ErrorPayload, DispatchRecord)> {
    let mut table = instance.dispatches.lock().unwrap();
    insert_dispatch_locked(instance, &mut table, dispatch_id, record)?;
    Ok(dispatch_task(instance, dispatch_id))
}

#[allow(clippy::result_large_err)]
fn insert_dispatch_locked(
    instance: &Instance,
    table: &mut HashMap<u64, DispatchRecord>,
    dispatch_id: u64,
    record: DispatchRecord,
) -> std::result::Result<(), (ErrorPayload, DispatchRecord)> {
    let limits = instance.host_limits;
    if instance.closed.load(Ordering::SeqCst) {
        return Err((closed_admission_payload(), record));
    }
    if table.len() >= limits.max_dispatches_per_instance {
        return Err((
            capacity_payload(
                "maxDispatchesPerInstance",
                table.len(),
                limits.max_dispatches_per_instance,
            ),
            record,
        ));
    }
    if record.sink.is_some() {
        let pending = table
            .values()
            .filter(|record| record.sink.is_some())
            .count();
        if pending >= limits.max_pending_per_instance {
            return Err((
                capacity_payload(
                    "maxPendingPerInstance",
                    pending,
                    limits.max_pending_per_instance,
                ),
                record,
            ));
        }
    }
    if record.stream.is_some() {
        let streams = table
            .values()
            .filter(|record| record.stream.is_some())
            .count();
        if streams >= limits.max_streams_per_instance {
            return Err((
                capacity_payload(
                    "maxStreamsPerInstance",
                    streams,
                    limits.max_streams_per_instance,
                ),
                record,
            ));
        }
    }
    table.insert(dispatch_id, record);
    Ok(())
}

struct ActiveCall {
    active: Arc<Mutex<usize>>,
    changed: Arc<tokio::sync::Notify>,
    // Err = host-only cleanup, never FFI; Ok(None) = explicit fake target.
    lease: std::result::Result<Option<Arc<dyn Any + Send + Sync>>, ()>,
}

impl Drop for ActiveCall {
    fn drop(&mut self) {
        // 最后强引用在锁外释放，退出通知必须晚于调用保护期。
        drop(std::mem::replace(&mut self.lease, Err(())));
        *self.active.lock().unwrap() -= 1;
        self.changed.notify_waiters();
    }
}

fn call_lease(instance: &Instance) -> std::result::Result<Option<Arc<dyn Any + Send + Sync>>, ()> {
    match &instance.keepalive {
        None => Ok(None),
        Some(owner) => owner.upgrade().map(Some).ok_or(()),
    }
}

fn admit_call(instance: &Instance) -> Option<ActiveCall> {
    let _table = instance.dispatches.lock().unwrap();
    if instance.closed.load(Ordering::SeqCst) {
        return None;
    }
    let lease = call_lease(instance).ok()?;
    *instance.lifecycle.lock().unwrap() += 1;
    Some(ActiveCall {
        active: instance.lifecycle.clone(),
        changed: instance.calls_changed.clone(),
        lease: Ok(lease),
    })
}

async fn wait_lifecycle_exclusive(instance: &Instance) {
    loop {
        let changed = instance.calls_changed.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        if *instance.lifecycle.lock().unwrap() == 0 {
            return;
        }
        changed.await;
    }
}

/// 一个实例的**在飞派发记录**总数（诊断口径）：含挂起续延、waterfall 帧与出站节点。
/// 句柄已失效 ⇒ 0（实例连同它的记录一起被排水）。
pub fn dispatches_len(handle: u64) -> usize {
    count_dispatches(handle, |_| true)
}

/// 一个实例当前**挂起中**的入站续延数（D8 / A10 的观测口）。
pub fn pending_len(handle: u64) -> usize {
    count_dispatches(handle, |record| record.sink.is_some())
}

/// 一个实例当前悬置的 **waterfall 帧**数（M2b 的观测口）。
pub fn waterfall_len(handle: u64) -> usize {
    count_dispatches(handle, |record| record.frame.is_some())
}

fn count_dispatches(handle: u64, predicate: impl Fn(&DispatchRecord) -> bool) -> usize {
    instances()
        .read()
        .unwrap()
        .get(handle)
        .map(|instance| {
            instance
                .dispatches
                .lock()
                .unwrap()
                .values()
                .filter(|record| predicate(record))
                .count()
        })
        .unwrap_or(0)
}

// ------------------------------------------------------------ 派发作用域（M2c）
//
// "谁发起了这次派发"必须在**插件发起子派发的那一刻**可得：插件是在自己的回调里
// 调 `ctx_dispatch` 的，而那次回调是宿主同步调进去的。于是用线程本地的**派发栈**：
// - `call_inbound` / `next` 交付在调用插件期间推入"正在执行的那次派发"；
// - `Scoped` future 在**每次 poll** 期间推入"正在交付的这次出站派发"
//   （poll 返回时立刻弹出——**不跨 await**，不会污染别的任务）。
// 插件在作用域外发起派发时父节点为 `None`（合法：它不属于取消树）。

thread_local! {
    static DISPATCH_SCOPE: std::cell::RefCell<Vec<(u64, u64)>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// 取出当前线程派发栈顶的 `(实例, dispatch_id)`。
fn scope_top() -> Option<(u64, u64)> {
    DISPATCH_SCOPE.with(|scope| scope.borrow().last().copied())
}

struct ScopeGuard;

impl Drop for ScopeGuard {
    fn drop(&mut self) {
        DISPATCH_SCOPE.with(|scope| {
            scope.borrow_mut().pop();
        });
    }
}

fn scope_push(handle: u64, dispatch_id: u64) -> ScopeGuard {
    DISPATCH_SCOPE.with(|scope| scope.borrow_mut().push((handle, dispatch_id)));
    ScopeGuard
}

// 调用作用域（ADR 0010）：**只由有记录的 iface 跳维护**，与上面的取消作用域分开。
// 事件 / waterfall / 流只压 DISPATCH_SCOPE，不压这一条，避免父指向无记录的 id。
thread_local! {
    static CALL_SCOPE: std::cell::RefCell<Vec<u64>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn call_scope_top() -> Option<u64> {
    CALL_SCOPE.with(|scope| scope.borrow().last().copied())
}

struct CallScopeGuard;

impl Drop for CallScopeGuard {
    fn drop(&mut self) {
        CALL_SCOPE.with(|scope| {
            scope.borrow_mut().pop();
        });
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn call_scope_push(call_id: u64) -> CallScopeGuard {
    CALL_SCOPE.with(|scope| scope.borrow_mut().push(call_id));
    CallScopeGuard
}

/// 给一条**出站派发**的 future 打上作用域：每次 poll 期间栈顶就是它，于是这次
/// 派发**交付给插件**的入站回调能记下自己的父节点（`parent = scope_top()`）。
struct Scoped {
    inner: BoxFuture<Ret>,
    scope: (u64, u64),
}

impl std::future::Future for Scoped {
    type Output = Ret;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Ret> {
        let this = self.get_mut(); // `Scoped` 是 Unpin（字段全是）
        let _guard = scope_push(this.scope.0, this.scope.1);
        this.inner.as_mut().poll(cx)
    }
}

/// 一次入站回调（事件监听 / 接口方法）的结局（M2）。
enum Inbound {
    /// 插件同步返回（M2 之前的行为，逐字不变）。
    Ready(Value),
    /// 插件返回 `STATUS_PENDING`：结果经 `ctx_fulfill` 到达。
    Pending(Box<PendingContinuation>),
}

/// 一个挂起的入站续延：宿主在等它被结算。
struct PendingContinuation {
    instance: Instance,
    dispatch_id: u64,
    rx: tokio::sync::oneshot::Receiver<std::result::Result<Value, ErrorPayload>>,
    /// 取消桥（M2c）：调用方撤销 ⇒ 不等了，立刻以错误结算并移出条目。
    cancel: Option<CancelToken>,
    /// 调用 Future 被丢弃（超时/取消/调用方放弃）时撤销派发记录。
    _abandon: PendingDropGuard,
    /// trace 父（ADR 0010）：settle 时写记录用。
    trace_parent: Option<u64>,
    /// 这次入站是否要写记录（事件 / 流为 false）。
    traced: bool,
    /// 发起时快照：settle 可能晚于 provider 卸载，届时 uid 已被处置。
    callee: (u64, Box<str>),
    caller_uid: u64,
    correlation_id: Option<Arc<str>>,
    name: Box<str>,
    started: std::time::Instant,
}

/// 挂起续延的“兜底清理”：只要 `PendingContinuation` 没有走到正常收尾就被丢弃，
/// 就把记录移出（值路径），或交给流观察者继续持有（流路径）。
struct PendingDropGuard {
    instance: Instance,
    dispatch_id: u64,
}

impl Drop for PendingDropGuard {
    fn drop(&mut self) {
        let stream_owned = self
            .instance
            .dispatches
            .lock()
            .unwrap()
            .get(&self.dispatch_id)
            .is_some_and(|record| record.stream.is_some());
        if !stream_owned {
            revoke_dispatch(
                &self.instance,
                self.dispatch_id,
                &ErrorPayload::new("调用方已放弃等待（Future 被丢弃）")
                    .with_code("host_cancel")
                    .with_category("cancel")
                    .with_certainty("unknown"),
            );
        }
    }
}

/// `settle` 的三态结局（把"谁先到"显式化，而不是压成一个 Option）。
enum SettleOutcome {
    Settled(
        std::result::Result<
            std::result::Result<Value, ErrorPayload>,
            tokio::sync::oneshot::error::RecvError,
        >,
    ),
    Cancelled,
    TimedOut(u64),
}

impl PendingContinuation {
    /// 等结算。**取消桥**（`select!` 令牌）与**超时**两条路径都必须把条目
    /// **移出**（D3 的四条清理路径）——否则晚到的 `fulfill` 会"悄悄成功"，
    /// 而调用方早已走了失败路径。
    #[allow(clippy::result_large_err)]
    async fn settle(self, what: &str) -> std::result::Result<Value, CallFailure> {
        let PendingContinuation {
            instance,
            dispatch_id,
            rx,
            cancel,
            _abandon,
            trace_parent,
            traced,
            callee,
            caller_uid,
            correlation_id,
            name,
            started,
        } = self;
        let timeout_ms = instance.dispatch_policy.timeout_ms;
        let wait = async move {
            match cancel {
                None => SettleOutcome::Settled(rx.await),
                Some(token) => tokio::select! {
                    outcome = rx => SettleOutcome::Settled(outcome),
                    _ = token.cancelled() => SettleOutcome::Cancelled,
                },
            }
        };
        let outcome = match timeout_ms {
            None => wait.await,
            Some(ms) => {
                match tokio::time::timeout(std::time::Duration::from_millis(ms), wait).await {
                    Ok(outcome) => outcome,
                    Err(_) => SettleOutcome::TimedOut(ms),
                }
            }
        };
        let emit = |outcome: cordis_core::Outcome| {
            if !traced {
                return;
            }
            instance.ctx.store.record_trace(cordis_core::TraceRecord {
                ts_ms: now_ms(),
                name: name.clone(),
                call_id: dispatch_id,
                parent_call_id: trace_parent.unwrap_or(0),
                caller_uid,
                callee_uid: callee.0,
                callee_plugin: callee.1.clone(),
                correlation_id: correlation_id.clone(),
                elapsed: started.elapsed(),
                outcome,
            });
        };
        match outcome {
            SettleOutcome::Settled(Ok(Ok(value))) => {
                emit(cordis_core::Outcome::Ok);
                Ok(value)
            }
            SettleOutcome::Settled(Ok(Err(payload))) => {
                emit(cordis_core::Outcome::Err(
                    payload.code.clone().into_boxed_str(),
                ));
                Err(CallFailure::from_payload(payload))
            }
            SettleOutcome::Settled(Err(_)) => {
                emit(cordis_core::Outcome::Err("host_lifecycle".into()));
                Err(CallFailure::new(
                    format!("{what}：结算通道已关闭（插件已卸载）"),
                    "host_lifecycle",
                    "lifecycle",
                    "unknown",
                ))
            }
            SettleOutcome::Cancelled => {
                // 收回这条派发：记录连同它上面的流一起（流必须以**错误**结束）。
                let payload =
                    ErrorPayload::new(format!("{what}：已取消——等待中的结算被调用方撤销"))
                        .with_code("host_cancel")
                        .with_category("cancel")
                        .with_certainty("unknown");
                revoke_dispatch(&instance, dispatch_id, &payload);
                emit(cordis_core::Outcome::Err("host_cancel".into()));
                Err(CallFailure::from_payload(payload))
            }
            SettleOutcome::TimedOut(ms) => {
                let payload = ErrorPayload::new(format!("{what}：等待插件结算超时（{ms}ms）"))
                    .with_code("HOST_TIMEOUT")
                    .with_category("timeout")
                    .with_certainty("unknown");
                revoke_dispatch(&instance, dispatch_id, &payload);
                emit(cordis_core::Outcome::Err("HOST_TIMEOUT".into()));
                Err(CallFailure::from_payload(payload))
            }
        }
    }
}

/// 调用插件的**入站**回调入口（M2/M2b）：铸造 `dispatch_id`、**先登记后调用**、
/// 把 `STATUS_PENDING` 变成一个可 await 的续延；waterfall 派发同时把剩余链
/// 登记成帧（`ctx_next` 用它）。
///
/// 顺序是安全属性的一部分（D3）：插件在回调内部就可能 `fulfill` / `next`，
/// 登记晚一步这些调用会看到"不存在"。
#[allow(clippy::too_many_arguments, clippy::result_large_err)]
unsafe fn call_inbound(
    instance: &Instance,
    handle: u64,
    op: u32,
    mut payload: Value,
    parent: Option<u64>,
    cancel: Option<CancelToken>,
    frame: Option<Next>,
    stream: Option<StreamSink>,
    trace: bool,
    trace_parent: Option<u64>,
) -> std::result::Result<Inbound, CallFailure> {
    let started = std::time::Instant::now();
    // 没有安装记录出口时，连身份快照都不构造（事件/流路径最常见）。
    let traced = trace && instance.ctx.store.has_trace_sink();
    let (caller_uid, correlation_id, callee, name) = if traced {
        let caller_uid = cordis_core::current_caller_uid();
        let correlation_id = cordis_core::current_correlation_id();
        let service_hint = payload
            .get("event")
            .or_else(|| payload.get("service"))
            .and_then(Value::as_str)
            .unwrap_or("?");
        let callee = instance
            .ctx
            .fiber_opt()
            .map(|fiber| (fiber.uid(), fiber.name().into_boxed_str()))
            .unwrap_or_else(|| (0, Box::from(service_hint)));
        let name: Box<str> = match &payload {
            Value::Object(map) if map.contains_key("event") || map.contains_key("service") => {
                format!(
                    "{}.{}",
                    service_hint,
                    map.get("method").and_then(Value::as_str).unwrap_or("?")
                )
                .into_boxed_str()
            }
            _ => format!("op:{op}").into_boxed_str(),
        };
        (caller_uid, correlation_id, callee, name)
    } else {
        (0, None, (0, Box::from("")), Box::from(""))
    };
    let _lifecycle = admit_call(instance).ok_or_else(|| {
        CallFailure::new(
            format!("ctx 句柄 {handle} 已失效（插件正在卸载或库已释放）"),
            "host_lifecycle",
            "lifecycle",
            "unknown",
        )
    })?;
    {
        let live = instances().read().unwrap().get(handle);
        if !live
            .as_ref()
            .is_some_and(|live| Arc::ptr_eq(&live.ctx, &instance.ctx))
        {
            return Err(CallFailure::new(
                format!("ctx 句柄 {handle} 已失效（插件已卸载，或句柄来自上一代实例）"),
                "host_lifecycle",
                "lifecycle",
                "unknown",
            ));
        }
    }
    let dispatch_id = next_dispatch_id();
    if let Value::Object(map) = &mut payload {
        map.insert("dispatch_id".into(), json!(dispatch_id));
    }
    // 流（ADR 0001 §5）：观察者要在**调用之前**装好——插件在回调内部就可能推块，
    // 而"窗口腾出位置 / 流结束"这些事件必须有人接。
    if let Some(sink) = &stream {
        sink.observe(Arc::new(HostStreamObserver {
            handle,
            dispatch_id,
        }));
    }
    // **先 insert 再调用**：插件在回调内部就能看到自己的条目。
    // 容量拒绝在这里发生：记录不插入，插件代码也不会被调用。
    let (tx, rx) = tokio::sync::oneshot::channel();
    if let Err((payload, rejected)) = insert_dispatch(
        instance,
        dispatch_id,
        DispatchRecord {
            sink: Some(tx),
            frame,
            parent,
            outbound: None,
            stream,
        },
    ) {
        return Err(CallFailure::from_payload(payload).with_frame(rejected.frame));
    }
    let (status, text) = {
        // 调用期间把"正在执行的那次派发"压进作用域：插件在回调内部发起的
        // 出站派发因此能记下父节点（取消级联）。
        let _scope = scope_push(handle, dispatch_id);
        // 调用作用域（ADR 0010）：只有有记录的 iface 跳压它。
        let _call_scope = traced.then(|| call_scope_push(dispatch_id));
        invoke_raw(instance.call, handle, op, &payload.to_string())
    };
    let emit = |outcome: cordis_core::Outcome| {
        if !traced {
            return;
        }
        instance.ctx.store.record_trace(cordis_core::TraceRecord {
            ts_ms: now_ms(),
            name: name.clone(),
            call_id: dispatch_id,
            parent_call_id: trace_parent.unwrap_or(0),
            caller_uid,
            callee_uid: callee.0,
            callee_plugin: callee.1.clone(),
            correlation_id: correlation_id.clone(),
            elapsed: started.elapsed(),
            outcome,
        });
    };
    match status {
        STATUS_OK => {
            let removed = instance.dispatches.lock().unwrap().remove(&dispatch_id);
            drop(removed);
            if text.is_empty() {
                emit(cordis_core::Outcome::Ok);
                return Ok(Inbound::Ready(Value::Null));
            }
            let parsed: Value = match serde_json::from_str(&text) {
                Ok(parsed) => parsed,
                Err(err) => {
                    emit(cordis_core::Outcome::Err("plugin_protocol".into()));
                    return Err(CallFailure::new(
                        format!("插件回调回复不是合法 JSON: {err}"),
                        "plugin_protocol",
                        "protocol",
                        "unknown",
                    ));
                }
            };
            emit(cordis_core::Outcome::Ok);
            Ok(Inbound::Ready(
                parsed.get("ret").cloned().unwrap_or(Value::Null),
            ))
        }
        STATUS_PENDING => Ok(Inbound::Pending(Box::new(PendingContinuation {
            instance: instance.clone(),
            dispatch_id,
            rx,
            cancel,
            _abandon: PendingDropGuard {
                instance: instance.clone(),
                dispatch_id,
            },
            trace_parent,
            traced,
            callee,
            caller_uid,
            correlation_id,
            name,
            started,
        }))),
        STATUS_ERR => {
            let removed = instance.dispatches.lock().unwrap().remove(&dispatch_id);
            drop(removed);
            let payload = decode_plugin_error(instance.error_format, &text);
            emit(cordis_core::Outcome::Err(
                payload.code.clone().into_boxed_str(),
            ));
            Err(CallFailure::from_payload(payload))
        }
        STATUS_UNSUPPORTED => {
            let removed = instance.dispatches.lock().unwrap().remove(&dispatch_id);
            drop(removed);
            emit(cordis_core::Outcome::Err("plugin_unsupported".into()));
            Err(CallFailure::new(
                format!("插件不支持 op {op}（它可能是为更老的宿主编译的）"),
                "plugin_unsupported",
                "protocol",
                "unknown",
            ))
        }
        other => {
            let removed = instance.dispatches.lock().unwrap().remove(&dispatch_id);
            drop(removed);
            emit(cordis_core::Outcome::Err(
                format!("status_{other}").into_boxed_str(),
            ));
            Err(CallFailure::new(
                if text.is_empty() {
                    format!("插件回调返回了意料之外的状态码 {other}")
                } else {
                    text
                },
                "plugin_protocol",
                "protocol",
                "unknown",
            ))
        }
    }
}

// ------------------------------------------------------------ 同步快速路径 + 异步兜底

/// 一次出站 iface 调用的归因快照：超时与 panic 两类“入站没有结算”的
/// 异常都用它补写记录（ADR 0009 §2.2 第 3/4 点）。
///
/// `call_id=0`、`callee_uid=0`：这条记录证明“入站从未结算”，没有可复用的
/// 派发 id 或实现方身份；`callee_plugin` 退化为请求的服务名。
struct IfaceTrace {
    store: Arc<cordis_core::Store>,
    name: String,
    callee_plugin: String,
    caller_uid: u64,
    parent_call_id: u64,
    correlation_id: Option<Arc<str>>,
    started: std::time::Instant,
}

impl IfaceTrace {
    fn record(&self, code: &str) {
        self.store.record_trace(cordis_core::TraceRecord {
            ts_ms: now_ms(),
            name: self.name.clone().into_boxed_str(),
            call_id: 0,
            parent_call_id: self.parent_call_id,
            caller_uid: self.caller_uid,
            callee_uid: 0,
            callee_plugin: self.callee_plugin.clone().into_boxed_str(),
            correlation_id: self.correlation_id.clone(),
            elapsed: self.started.elapsed(),
            outcome: cordis_core::Outcome::Err(code.into()),
        });
    }
}

/// 给 `ctx_call_iface` 的 future 套上出站看门狗。
///
/// 只看“提供者结算”，不改变内联/异步两条路径：超时后 future 被丢弃，调用方
/// 收到一条可读错误，后续由 `complete_or_defer` 的 `on_done` 撤销派发记录。
///
/// 记录只写在超时分支：内层 future 与计时器在**同一个任务**里 poll，且
/// `tokio::time::timeout` 先 poll 内层——已结算的调用返回 `Ok(_)`，走到这里
/// 的调用一定没有可用的入站记录，与 `call_inbound`/`settle` 的写入互斥。
/// 更本质的不变量是：入站写入发生在**同一次 poll** 内且该 poll 必然返回
/// `Ready`；`timeout` 的 `Err` 分支只会在内层没有 Ready 时到达。
fn with_iface_timeout(
    future: BoxFuture<Result<Config>>,
    service: String,
    method: String,
    policy: DispatchPolicy,
    trace: Option<Arc<IfaceTrace>>,
) -> BoxFuture<Result<Config>> {
    let Some(ms) = policy.outbound_timeout_ms else {
        return future;
    };
    // 无 tokio 运行时的线程（如插件自建线程）无法构造 tokio 计时器，直接包装会让
    // 首 poll 在 `tokio::time::timeout` 里 panic 并被误记成 `host_panic`；跳过包装，
    // 交给 `complete_or_defer` 的同步快路径或 `DeferFailure::NoRuntime`（`host_call`）。
    if tokio::runtime::Handle::try_current().is_err() {
        return future;
    }
    Box::pin(async move {
        match tokio::time::timeout(std::time::Duration::from_millis(ms), future).await {
            Ok(result) => result,
            Err(_) => {
                if let Some(trace) = trace {
                    trace.record("HOST_TIMEOUT");
                }
                Err(CallFailure::new(
                    format!("HOST_TIMEOUT: iface {service}.{method} 调用超过 {ms}ms 未结算"),
                    "HOST_TIMEOUT",
                    "timeout",
                    "unknown",
                )
                .into())
            }
        }
    })
}

/// 给 `ctx_dispatch` / `ctx_emit` 的 future 套上出站看门狗。
///
/// 超时被收敛成 `Err(ErrorPayload)`；调用方把它投递成 `{ "error": ..., ... }`，
/// 而不是伪装成一个正常返回值。
fn with_dispatch_timeout(
    future: BoxFuture<Ret>,
    event: String,
    policy: DispatchPolicy,
) -> BoxFuture<std::result::Result<Ret, ErrorPayload>> {
    let Some(ms) = policy.outbound_timeout_ms else {
        return Box::pin(async move { Ok(future.await) });
    };
    if tokio::runtime::Handle::try_current().is_err() {
        return Box::pin(async move { Ok(future.await) });
    }
    Box::pin(async move {
        match tokio::time::timeout(std::time::Duration::from_millis(ms), future).await {
            Ok(ret) => Ok(ret),
            Err(_) => Err(ErrorPayload::new(format!(
                "HOST_TIMEOUT: dispatch \"{event}\" 超过 {ms}ms 未结算"
            ))
            .with_code("HOST_TIMEOUT")
            .with_category("timeout")
            .with_certainty("unknown")),
        }
    })
}

/// 给 waterfall 的剩余链套上同一个出站看门狗；错误经 `kind:"next"` 交付。
fn with_next_timeout(
    future: BoxFuture<Ret>,
    dispatch_id: u64,
    policy: DispatchPolicy,
) -> BoxFuture<std::result::Result<Ret, ErrorPayload>> {
    let Some(ms) = policy.outbound_timeout_ms else {
        return Box::pin(async move { Ok(future.await) });
    };
    if tokio::runtime::Handle::try_current().is_err() {
        return Box::pin(async move { Ok(future.await) });
    }
    Box::pin(async move {
        match tokio::time::timeout(std::time::Duration::from_millis(ms), future).await {
            Ok(ret) => Ok(ret),
            Err(_) => Err(ErrorPayload::new(format!(
                "HOST_TIMEOUT: waterfall {dispatch_id} 的剩余链超过 {ms}ms 未结算"
            ))
            .with_code("HOST_TIMEOUT")
            .with_category("timeout")
            .with_certainty("unknown")),
        }
    })
}

/// 一次 poll 边界 panic 的归档坐标：日志、测试登记与错误消息共用同一套口径。
#[derive(Clone, Copy)]
struct PanicScope {
    /// 场景名（`iface` / `dispatch` / `next` / `emit` / `on_done`）。
    scene: &'static str,
    handle: u64,
    dispatch_id: u64,
}

/// [`complete_or_defer`] 的失败形态：调用点**必须**能区分这两件事。
enum DeferFailure {
    /// 操作尚未就绪，但当前线程没有 tokio 运行时：结果无法交付。
    NoRuntime(String),
    /// **首 poll 就 panic**：调用方同步返回这条错误，绝不调用 `on_done`。
    Panic(ErrorPayload),
}

/// 一次 poll 边界 panic 的结构化载荷：`host_panic`，分类/认知/重试保持默认 `unknown`。
///
/// 绝不写 `not_executed`：宿主**无法**证明插件的副作用没有发生——那条字段是
/// 调用方据此决定"能不能重试"的事实，猜错了就是重复副作用。
fn panic_payload(scene: &str, payload: &(dyn Any + Send)) -> ErrorPayload {
    ErrorPayload::new(format!(
        "宿主在驱动异步操作时 panic（{scene}，已在 poll 边界截获）：{}",
        cordis_core::panic_message(payload)
    ))
    .with_code("host_panic")
}

/// 一条宿主 poll 边界 panic 的诊断记录（仅测试构建；按 handle/dispatch 过滤，
/// 避免并行测试互相串扰）。
#[cfg(test)]
#[derive(Clone, Debug)]
pub(crate) struct PanicNote {
    pub(crate) scene: &'static str,
    pub(crate) handle: u64,
    pub(crate) dispatch_id: u64,
    pub(crate) message: String,
}

#[cfg(test)]
pub(crate) fn panic_notes() -> &'static Mutex<Vec<PanicNote>> {
    static NOTES: OnceLock<Mutex<Vec<PanicNote>>> = OnceLock::new();
    NOTES.get_or_init(|| Mutex::new(Vec::new()))
}

/// 记录一次**宿主自己**在 poll 边界截获的 panic。
///
/// 范围只有"宿主进程内、future 被 poll 时的 Rust unwind"（`panic=unwind`）：
/// `extern "C"` 边界、OOM/abort、`panic=abort`、panic-in-Drop 都不在这里的承诺里。
/// 生产构建只打印既有风格的诊断；测试构建另外登记一条供断言。
fn record_async_panic(
    scene: &'static str,
    handle: u64,
    dispatch_id: u64,
    payload: &(dyn Any + Send),
) {
    let message = cordis_core::panic_message(payload);
    eprintln!(
        "[cordis] 宿主 poll 边界截获 panic（{scene}；handle {handle}；dispatch {dispatch_id}）：{message}"
    );
    #[cfg(test)]
    panic_notes().lock().unwrap().push(PanicNote {
        scene,
        handle,
        dispatch_id,
        message,
    });
}

/// 借用式适配器：只把 `poll` 的 unwind 收敛成 `Err`，**不动** future 的所有权。
///
/// 为什么不换掉 `TaskWork.future` 的类型：`future → on_done → _exit` 的字段析构序
/// 是 O1 的协议（首 poll panic 时捕获物也必须先于退出信号释放）。把 `&mut` 借出来
/// 包一层，析构序原样保留。
struct CatchUnwind<'a, T> {
    future: &'a mut BoxFuture<T>,
}

impl<'a, T> CatchUnwind<'a, T> {
    fn new(future: &'a mut BoxFuture<T>) -> Self {
        Self { future }
    }

    fn get_mut(&mut self) -> &mut BoxFuture<T> {
        self.future
    }
}

impl<T> std::future::Future for CatchUnwind<'_, T> {
    type Output = std::result::Result<T, Box<dyn Any + Send>>;

    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        // 适配器本身 Unpin：先解开 Pin，再借出内层 future 的 `&mut`。
        let this = std::pin::Pin::get_mut(self);
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            std::future::Future::poll(this.get_mut().as_mut(), cx)
        }));
        match caught {
            Ok(Poll::Ready(value)) => Poll::Ready(Ok(value)),
            Ok(Poll::Pending) => Poll::Pending,
            Err(payload) => Poll::Ready(Err(payload)),
        }
    }
}

/// 先同步试跑一次；不就绪就交给运行时，完成时调 `on_done`。
///
/// 用 `Waker::noop()` 轮询是安全的：若返回 `Pending`，我们把**同一个** future
/// 交给真实任务继续跑（届时会重新注册真实 waker），所以不会丢唤醒。
/// 若返回 `Ready`，调用方拿到值，插件侧就是普通的一次函数调用——
/// 这就是"同步 ABI 不为异步内核付出代价"的来源。
///
/// # panic 结算（O3）
/// - **首 poll** 就 panic：返回 [`DeferFailure::Panic`]，`on_done` / `on_panic`
///   一律不调用（调用方同步返回错误，记录由调用点摘）；
/// - **后台 poll** 才 panic：与正常完成走**同一条**交付路径，调 `on_panic(payload)`——
///   插件在等的是一条回调，而不是"永远没有下文"。
#[allow(clippy::result_large_err)]
fn complete_or_defer<T: Send + 'static>(
    future: BoxFuture<T>,
    on_done: impl FnOnce(T) + Send + 'static,
    on_panic: impl FnOnce(ErrorPayload) + Send + 'static,
    task: (CancelToken, TaskExit),
    scope: PanicScope,
) -> std::result::Result<Option<T>, DeferFailure> {
    let (cancel, exit) = task;
    let mut work = TaskWork {
        future,
        on_done: Some(on_done),
        _exit: exit,
    };
    let waker = std::task::Waker::noop();
    let mut cx = std::task::Context::from_waker(waker);
    let polled = {
        let mut caught = CatchUnwind::new(&mut work.future);
        std::future::Future::poll(std::pin::Pin::new(&mut caught), &mut cx)
    };
    match polled {
        Poll::Ready(Ok(value)) => Ok(Some(value)),
        Poll::Ready(Err(payload)) => {
            record_async_panic(scope.scene, scope.handle, scope.dispatch_id, &*payload);
            // `work` 在这里析构：future → on_done → 退出信号（O1 的析构序）。
            Err(DeferFailure::Panic(panic_payload(scope.scene, &*payload)))
        }
        Poll::Pending => {
            // 没有运行时说明调用发生在同步上下文里：既不能阻塞（会死锁），
            // 也无法驱动未来。必须把错误交还调用方，不能假装"稍后会有回调"。
            match tokio::runtime::Handle::try_current() {
                Ok(handle) => {
                    handle.spawn(async move {
                        let settled = {
                            let mut caught = CatchUnwind::new(&mut work.future);
                            tokio::select! {
                                biased;
                                _ = cancel.cancelled() => None,
                                value = &mut caught => Some(value),
                            }
                        };
                        match settled {
                            // 取消：交付属于取消/排水路径（"恰好一次"的既有纪律）。
                            None => {}
                            Some(Ok(value)) => {
                                if let Some(on_done) = work.on_done.take() {
                                    // `on_done` 跑在宿主派生的任务里：它自己的 panic 也要
                                    // 截获，否则就是一条无人观察的 panic（插件既等不到
                                    // 回调，也得不到任何诊断）。
                                    if let Err(payload) = std::panic::catch_unwind(
                                        std::panic::AssertUnwindSafe(move || on_done(value)),
                                    ) {
                                        record_async_panic(
                                            "on_done",
                                            scope.handle,
                                            scope.dispatch_id,
                                            &*payload,
                                        );
                                    }
                                }
                            }
                            Some(Err(payload)) => {
                                record_async_panic(
                                    scope.scene,
                                    scope.handle,
                                    scope.dispatch_id,
                                    &*payload,
                                );
                                on_panic(panic_payload(scope.scene, &*payload));
                            }
                        }
                        drop(work);
                    });
                    Ok(None)
                }
                Err(_) => Err(DeferFailure::NoRuntime("操作尚未就绪，但当前没有异步运行时；结果无法交付（请在有 tokio runtime 的线程调用）"
                    .to_string())),
            }
        }
    }
}

/// 只向**仍然存活**的实例投递回调：短锁准入、取得调用 lease，锁外执行。
///
/// waterfall next 和取消交付经此准入；普通出站完成由 `deliver_outbound`
/// 将摘表与准入合并。直接使用快照里的裸 `CallFn` 会打进已释放的库。
unsafe fn call_live(handle: u64, op: u32, payload: &str) -> std::result::Result<Value, String> {
    let instance =
        instances().read().unwrap().get(handle).ok_or_else(|| {
            format!("ctx 句柄 {handle} 已失效（插件已卸载，或句柄来自上一代实例）")
        })?;
    let _lifecycle = admit_call(&instance)
        .ok_or_else(|| format!("ctx 句柄 {handle} 已失效（插件正在卸载或库已释放）"))?;
    call_plugin(instance.call, handle, op, payload, instance.error_format)
}

mod ffi_exports;
use ffi_exports::*;

// ------------------------------------------------------------ 原生组件注册表（D8）

/// `@native:<name>` 的解析点（D8）：宿主把进程内已有的原生组件登记进来，
/// ABI 插件就能像挂普通组件一样挂它们。
///
/// 为什么是**进程级全局 + pub setter**：host 的类型不能进 core 的
/// `App` 字段（core 对 host 一无所知），而 `ctx_use` 的调用点在
/// `host_ctx_*` 的 C ABI 边界上——没有 ctx 可传，只剩进程级这一条路。
pub trait NativeRegistry: Send + Sync {
    /// 返回 `None` = 宿主没有注册这个原生组件（调用方拿到可读错误）。
    fn native(&self, name: &str) -> Option<Arc<dyn Plugin>>;
}

static NATIVE_REGISTRY: RwLock<Option<Arc<dyn NativeRegistry>>> = RwLock::new(None);

/// 登记进程级原生组件注册表（覆盖式；后设生效）。
pub fn set_native_registry(registry: Arc<dyn NativeRegistry>) {
    *NATIVE_REGISTRY.write().unwrap() = Some(registry);
}

fn native_registry() -> Option<Arc<dyn NativeRegistry>> {
    NATIVE_REGISTRY.read().unwrap().clone()
}

static PLUGIN_SOURCE: RwLock<Option<Arc<dyn loader::PluginSource>>> = RwLock::new(None);

/// `ctx_use` 的 `PluginSource` 注入口（D8 的对称 seam）：宿主注入后，
/// ABI 插件的 `ctx_use` 就能挂进程内组件；缺省回退到磁盘动态库解析。
pub fn set_plugin_source(source: Arc<dyn loader::PluginSource>) {
    *PLUGIN_SOURCE.write().unwrap() = Some(source);
}

fn plugin_source() -> Arc<dyn loader::PluginSource> {
    PLUGIN_SOURCE
        .read()
        .unwrap()
        .clone()
        .unwrap_or_else(|| Arc::new(loader::DiskPluginSource))
}

// ------------------------------------------------------------ 动态库加载

/// 动态库与暂存副本的联合保活壳。
///
/// **字段声明顺序即析构顺序**：必须先 drop `_library`（`FreeLibrary` / `dlclose` 解锁文件），
/// 再 drop `_staged`（`remove_file` 删除暂存副本，Windows 上反序必然删不掉）。
/// 两者合进同一个 `Arc` 后，任何持有保活 lease 的在飞调用或超时后台收尾都能同时把动态库与
/// 暂存文件保活到最后一次原生调用退出之后（ADR 0005 §4）。
struct LoadedLibrary {
    _library: dylib::DynLib,
    _staged: Option<StagedFile>,
}

/// 一个从动态库装载进来的插件。
pub struct LoadedPlugin {
    /// 动态库句柄 + 暂存副本的联合引用计数；drop 即按序卸库并删副本。
    /// **字段顺序有意义**：它必须活到最后，见 `raw_desc` 的说明。
    library: Arc<LoadedLibrary>,
    desc: *const PluginDesc,
    call: CallFn,
    name: String,
    source: PathBuf,
    inject: Inject,
    /// v2 尾追加：`provision` 声明（v1 插件 ⇒ 空 = 未声明）。
    provision: Vec<String>,
    /// 配置 schema（冻结前缀字段；非法/空 ⇒ `None`）。
    schema: Option<String>,
    /// 声明了 schema 但不可展示时的原因（`None` = 未声明或合法）。
    schema_error: Option<String>,
    /// ADR 0004 尾追加：插件声明的结构化错误能力（0 = 纯文本）。
    error_format: u32,
}

// 动态库句柄与描述表在插件的整个生命周期内有效；`call` 是函数指针。
// 这里的不安全承诺是：**动态库在 `LoadedPlugin` 存活期间不会被卸载**
// （由 `_library` 字段的 drop 顺序保证）。
unsafe impl Send for LoadedPlugin {}
unsafe impl Sync for LoadedPlugin {}

/// ABI 门禁（G14，纯函数 seam——D8）：主版本必须**相等**，desc 尺寸只需盖住
/// **冻结前缀**（`call` 之前含 `call` 的布局）。
///
/// 旧规则（`size >= sizeof(PluginDesc)`）会把"宿主侧尾追加字段"变成破坏性变更；
/// 新规则下 v1 插件（`size == expected_prefix`）照常加载，前缀之后的尾字段由
/// [`PluginDesc::field_at`] 按 `size` 逐个探测（读到 `None` = 未声明）。
pub fn desc_gate(abi_version: u32, size: usize, expected_prefix: usize) -> Result<()> {
    if abi_version != ABI_VERSION {
        return Err(format!(
            "ABI 主版本是 {abi_version}，宿主需要 {ABI_VERSION}；拒绝加载（而非勉强运行）"
        )
        .into());
    }
    if size < expected_prefix {
        return Err(format!(
            "PluginDesc 尺寸 {size} 小于冻结前缀 {expected_prefix}；拒绝加载（宿主连前缀内的字段都读不全）"
        )
        .into());
    }
    Ok(())
}

impl LoadedPlugin {
    /// 打开一个动态库并校验 ABI。
    pub fn open(path: impl AsRef<Path>) -> Result<LoadedPlugin> {
        Self::open_classified(path).map_err(|err| err.message.into())
    }

    pub(crate) fn open_classified(
        path: impl AsRef<Path>,
    ) -> std::result::Result<LoadedPlugin, crate::source::ClassifiedVerifyError> {
        let path = path.as_ref().to_path_buf();
        let library =
            dylib::DynLib::open(&path).map_err(|err| crate::source::ClassifiedVerifyError {
                category: FailureCategory::RuntimeUnavailable,
                message: err.to_string(),
            })?;
        let entry: EntryFn = unsafe {
            std::mem::transmute(library.symbol(ENTRY_SYMBOL).map_err(|err| {
                crate::source::ClassifiedVerifyError {
                    category: FailureCategory::Integrity,
                    message: err.to_string(),
                }
            })?)
        };
        let desc = unsafe { entry() };
        if desc.is_null() {
            return Err(crate::source::ClassifiedVerifyError {
                category: FailureCategory::Integrity,
                message: format!("{} 的 cordis_plugin_v1 返回了空指针", path.display()),
            });
        }
        // **先只读冻结头**：`id`/`inject` 的 offset 在 `size` 覆盖范围之外时，
        // 直接读 `(*desc).id` 会越界。门禁必须发生在任何尾字段读取之前。
        let (abi_version, size) = unsafe { ((*desc).abi_version, (*desc).size as usize) };
        desc_gate(abi_version, size, cordis_abi::PLUGIN_DESC_PREFIX_SIZE).map_err(|error| {
            crate::source::ClassifiedVerifyError {
                category: FailureCategory::Integrity,
                message: format!("插件文件 {}：{error}", path.display()),
            }
        })?;
        let (id, inject_json, provision_json, schema, schema_error, error_format) = unsafe {
            let schema = schema_from_blob((*desc).schema);
            let id = blob_to_string((*desc).id, "PluginDesc.id").map_err(|error| {
                crate::source::ClassifiedVerifyError {
                    category: FailureCategory::Integrity,
                    message: format!("插件文件 {}：{error}", path.display()),
                }
            })?;
            let inject_json =
                blob_to_string((*desc).inject, "PluginDesc.inject").map_err(|error| {
                    crate::source::ClassifiedVerifyError {
                        category: FailureCategory::Integrity,
                        message: format!("插件文件 {}：{error}", path.display()),
                    }
                })?;
            // provision 是 v2 尾追加字段：v1 插件（size 只到冻结前缀）经
            // `field_at` 读作「未声明」；Blob 指向插件内存，必须立刻拷成 String
            // （附录 A）。
            let provision_json = match PluginDesc::field_at::<Blob>(
                desc,
                std::mem::offset_of!(PluginDesc, provision),
            ) {
                Some(blob) => blob_to_string(blob, "PluginDesc.provision").map_err(|error| {
                    crate::source::ClassifiedVerifyError {
                        category: FailureCategory::Integrity,
                        message: format!("插件文件 {}：{error}", path.display()),
                    }
                })?,
                None => String::new(),
            };
            (
                id,
                inject_json,
                provision_json,
                schema.text,
                schema.error,
                PluginDesc::error_format_at(desc),
            )
        };
        // 把宿主函数表交给插件。**这是版本闸门的唯一入口**：插件在这里校验，
        // 不匹配就不登记，之后每次 call 都会显式失败。
        unsafe { ((*desc).attach)(&HOST_API) };
        let call = unsafe { (*desc).call };

        Ok(LoadedPlugin {
            library: Arc::new(LoadedLibrary {
                _library: library,
                _staged: None,
            }),
            desc,
            call,
            name: id,
            source: path,
            inject: parse_inject(&inject_json),
            provision: parse_provision(&provision_json),
            schema,
            schema_error,
            error_format,
        })
    }

    /// 打开一个**暂存副本**并认领它（M0b，G11）：装载成功后，副本文件
    /// 的生命周期归本插件——drop 时先卸库、后删文件（字段声明序）。
    /// 装载失败则不认领：文件留在泄漏台账里，`stagedLeaked` 会暴露它。
    pub fn open_staged(path: impl AsRef<Path>) -> Result<LoadedPlugin> {
        Self::open_staged_classified(path).map_err(|err| err.message.into())
    }

    pub(crate) fn open_staged_classified(
        path: impl AsRef<Path>,
    ) -> std::result::Result<LoadedPlugin, crate::source::ClassifiedVerifyError> {
        let mut plugin = Self::open_classified(path.as_ref())?;
        Arc::get_mut(&mut plugin.library)
            .expect("新打开的 LoadedPlugin 独占 library")
            ._staged = Some(StagedFile::new(path.as_ref()));
        Ok(plugin)
    }

    /// 先校验 sidecar manifest 与来源 allow-list，再加载动态库。
    pub fn open_verified(
        path: impl AsRef<Path>,
        policy: &PluginSourcePolicy,
    ) -> Result<LoadedPlugin> {
        let path = path.as_ref();
        let verified = policy.verify(path)?;
        let plugin = Self::open(&verified.path)?;
        if plugin.name != verified.manifest.id {
            return Err(format!(
                "插件 id 与 manifest 不匹配: manifest={} plugin={}",
                verified.manifest.id, plugin.name
            )
            .into());
        }
        Ok(plugin)
    }

    /// 校验原始 artifact，加载并认领一个**不同的**暂存副本。
    ///
    /// `original` 与 `staged` 必须是两个不同文件；相同路径会被拒绝，避免把
    /// 部署目录里的原始插件当成暂存副本删除。
    pub fn open_staged_verified(
        original: impl AsRef<Path>,
        staged: impl AsRef<Path>,
        policy: &PluginSourcePolicy,
    ) -> Result<LoadedPlugin> {
        Self::open_staged_verified_classified(original, staged, policy)
            .map_err(|err| err.message.into())
    }

    pub(crate) fn open_staged_verified_classified(
        original: impl AsRef<Path>,
        staged: impl AsRef<Path>,
        policy: &PluginSourcePolicy,
    ) -> std::result::Result<LoadedPlugin, crate::source::ClassifiedVerifyError> {
        let original = original.as_ref();
        let staged = staged.as_ref();
        let original_abs =
            std::path::absolute(original).map_err(|err| crate::source::ClassifiedVerifyError {
                category: FailureCategory::Integrity,
                message: format!("规范化插件 artifact {} 失败: {err}", original.display()),
            })?;
        let staged_abs =
            std::path::absolute(staged).map_err(|err| crate::source::ClassifiedVerifyError {
                category: FailureCategory::Integrity,
                message: format!("规范化暂存副本 {} 失败: {err}", staged.display()),
            })?;
        let same = match (
            std::fs::canonicalize(&original_abs),
            std::fs::canonicalize(&staged_abs),
        ) {
            (Ok(original), Ok(staged)) => original == staged,
            _ => original_abs == staged_abs,
        };
        if same {
            return Err(crate::source::ClassifiedVerifyError {
                category: FailureCategory::Integrity,
                message: "open_staged_verified 拒绝把原始 artifact 当作暂存副本".into(),
            });
        }
        let discard_staged = || {
            drop(StagedFile::new(staged));
        };
        let verified = match policy.verify_classified(&original_abs) {
            Ok(verified) => verified,
            Err(error) => {
                discard_staged();
                return Err(error);
            }
        };
        let staged_hash = match crate::source::sha256_file(&staged_abs) {
            Ok(hash) => hash,
            Err(error) => {
                discard_staged();
                return Err(crate::source::ClassifiedVerifyError {
                    category: FailureCategory::Integrity,
                    message: error,
                });
            }
        };
        if staged_hash != verified.sha256 {
            discard_staged();
            return Err(crate::source::ClassifiedVerifyError {
                category: FailureCategory::Integrity,
                message: format!(
                    "暂存副本 SHA256 与已校验 artifact 不匹配: staged={staged_hash} verified={}",
                    verified.sha256
                ),
            });
        }
        let mut plugin = match Self::open_classified(&staged_abs) {
            Ok(plugin) => plugin,
            Err(error) => {
                discard_staged();
                return Err(error);
            }
        };
        if plugin.name != verified.manifest.id {
            let plugin_id = plugin.name.clone();
            drop(plugin);
            discard_staged();
            return Err(crate::source::ClassifiedVerifyError {
                category: FailureCategory::Integrity,
                message: format!(
                    "插件 id 与 manifest 不匹配: manifest={} plugin={plugin_id}",
                    verified.manifest.id
                ),
            });
        }
        Arc::get_mut(&mut plugin.library)
            .expect("新打开的 LoadedPlugin 独占 library")
            ._staged = Some(StagedFile::new(staged));
        Ok(plugin)
    }

    pub fn source(&self) -> &Path {
        &self.source
    }

    /// v2 尾追加：插件的 `provision` 声明（v1 插件 ⇒ 空 = 未声明）。
    pub fn provision(&self) -> &[String] {
        &self.provision
    }

    /// 插件自述的配置 schema（合法 JSON 对象；未声明/非法 ⇒ `None`）。
    pub fn schema(&self) -> Option<&str> {
        self.schema.as_deref()
    }

    /// 声明了 schema 但无法展示的原因（`None` = 未声明或合法）。
    pub fn schema_error(&self) -> Option<&str> {
        self.schema_error.as_deref()
    }

    /// 插件描述表的原始指针。保留它是为了诊断（比如打印完整 ABI 版本信息），
    /// 生命周期由本 struct 持有动态库句柄保证。
    pub fn raw_desc(&self) -> *const PluginDesc {
        self.desc
    }

    /// 一次 `OP_PING`：确认插件真的可调用（也用于加载期体检）。
    ///
    /// 传 `handle = 0`：`OP_PING` 按约定不使用上下文，所以**不需要注册假实例**。
    pub fn ping(&self) -> bool {
        let mut buffer = vec![0u8; 256];
        let mut out = Out {
            ptr: buffer.as_mut_ptr(),
            cap: buffer.len(),
            len: 0,
        };
        let status =
            unsafe { (self.call)(0, cordis_abi::OP_PING, Blob::EMPTY, Blob::EMPTY, &mut out) };
        status == STATUS_OK
    }
}

impl Plugin for LoadedPlugin {
    fn name(&self) -> &str {
        &self.name
    }

    /// 每个动态库一把身份键：不同文件即使同名也不会塌缩成同一条 Runtime 记录。
    fn key(&self) -> PluginKey {
        Arc::from(format!("{}::{}", self.name, self.source.display()).as_str())
    }

    fn inject(&self) -> Inject {
        self.inject.clone()
    }

    /// ABI 的 provision 声明（M5）：来自 `PluginDesc.provision` 尾字段
    /// （v1 插件读作"未声明" ⇒ 空 ⇒ 不透明，规则见 `Store::provision_report`）。
    fn provision(&self) -> Vec<String> {
        self.provision.clone()
    }

    fn config_schema(&self) -> Option<String> {
        self.schema.clone()
    }

    fn apply(&self, ctx: Ctx, config: Config) -> BoxFuture<Result<()>> {
        // 动态库句柄与暂存副本以**弱引用**登记为实例保活：`Instance` 克隆不延长 owner——
        // owner 由实例表槽位持有到注销，在飞调用与超时后台收尾持有 lease，最后 drop 在表锁之外
        // （那一下才先 `FreeLibrary`、再删暂存副本）。
        let keepalive: Arc<dyn Any + Send + Sync> = self.library.clone();
        apply_via_with_keepalive_and_format(
            self.call,
            ctx,
            config,
            self.name.as_str(),
            Some(keepalive),
            self.error_format,
        )
    }

    /// ABI 的 volatile 声明：跨边界问一次（`OP_VOLATILE_PATHS`）。
    ///
    /// 句柄传 0：这个 op 按约定**不使用上下文**（与 `OP_PING` 同类）——声明是插件的
    /// 静态能力，`apply` 之前就该答得出来（内核在装载时建引用要用它）。
    ///
    /// **旧插件返回 `UNSUPPORTED` ⇒ 空声明**（它不认识这个 op）：那一行退化成
    /// "配置一变就重挂"——这正是 ABI 前向兼容要的降级，而不是崩溃或静默走错路。
    fn volatile_paths(&self) -> Vec<String> {
        match unsafe {
            call_op_read(
                self.call,
                0,
                cordis_abi::OP_VOLATILE_PATHS,
                "",
                self.error_format,
            )
        } {
            Ok(CallOutcome::Ok(value)) => value
                .as_array()
                .map(|list| {
                    list.iter()
                        .filter_map(|item| item.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
            Ok(CallOutcome::Unsupported) => Vec::new(),
            Ok(CallOutcome::Err(payload)) => {
                eprintln!(
                    "[cordis] {} 的 volatile 声明查询失败: {}",
                    self.name, payload.error
                );
                Vec::new()
            }
            Err(message) => {
                eprintln!("[cordis] {} 的 volatile 声明查询出错: {message}", self.name);
                Vec::new()
            }
        }
    }

    /// 把新的 volatile 值推给**正在跑的实例**（`OP_VOLATILE_APPLY`）。
    ///
    /// 跨边界没有共享引用：进程内插件共享一个 `VolatileRef`，ABI 插件只能收**值**，
    /// 由插件更新自己那份副本（SDK 的 `sdk::volatile(path)` 读的就是它）。
    fn apply_volatile(&self, ctx: &Ctx, values: &[(String, Config)]) -> Result<()> {
        let Some(handle) = instance_handle_of(ctx) else {
            return Err("推送 volatile 值时找不到该实例的句柄（实例可能已经卸载）".into());
        };
        let Some(instance) = instances().read().unwrap().get(handle) else {
            return Err("推送 volatile 值时实例已注销".into());
        };
        let Some(_active) = admit_call(&instance) else {
            return Err("推送 volatile 值时实例已进入关闭流程".into());
        };
        let paths: Vec<Value> = values
            .iter()
            .map(|(path, value)| json!({ "path": path, "value": value }))
            .collect();
        let payload = json!({ "paths": paths }).to_string();
        match unsafe {
            call_op(
                self.call,
                handle,
                cordis_abi::OP_VOLATILE_APPLY,
                &payload,
                self.error_format,
            )
        } {
            Ok(CallOutcome::Ok(_)) => Ok(()),
            Ok(CallOutcome::Unsupported) => Err("插件不支持 OP_VOLATILE_APPLY".into()),
            Ok(CallOutcome::Err(payload)) => Err(payload.error.into()),
            Err(message) => Err(message.into()),
        }
    }
}

/// ABI 装载的实现体：`LoadedPlugin::apply` 与测试夹具共用（M1-1 的 seam）。
///
/// 生命周期契约（M1-1 修正后）：
/// 1. 句柄注销与 OP_DISPOSE 通知是**同一个 effect**，注册在 `OP_APPLY` **之前**——
///    LIFO 下它最后跑：插件在 apply 里注册的 cleanup（OP_CLEANUP）先执行，
///    期间句柄仍然有效；
/// 2. `OP_DISPOSE` **只投给成功 apply 过的实例**（`applied` 门控）：apply 失败时
///    只注销句柄，不向插件投递"即将回收"的信号；
/// 3. 失败路径**不手工注销**句柄——注销是上面那个 effect 的单点职责
///    （fiber 的失败清理会跑到它），避免两条路径互相漏。
pub fn apply_via(call: CallFn, ctx: Ctx, config: Config, name: &str) -> BoxFuture<Result<()>> {
    apply_via_with_keepalive_and_format(call, ctx, config, name, None, 0)
}

/// 知道插件能力位的无 DLL 装载入口。
///
/// `CallFn` 本身不带 `PluginDesc`；当它来自 [`cordis_abi::sdk::build_plugin`]
/// 时，调用方应传 `PluginDesc::error_format_at(desc)`，否则宿主会按文本读取，
/// 插件的结构化 `STATUS_ERR` 会以 JSON 文本出现在用户可见消息里。
pub fn apply_via_with_error_format(
    call: CallFn,
    ctx: Ctx,
    config: Config,
    name: &str,
    error_format: u32,
) -> BoxFuture<Result<()>> {
    apply_via_with_keepalive_and_format(call, ctx, config, name, None, error_format)
}

/// 同时带库保活与插件错误能力位的无 DLL 装载入口。
pub fn apply_via_with_keepalive_and_error_format(
    call: CallFn,
    ctx: Ctx,
    config: Config,
    name: &str,
    keepalive: Option<Arc<dyn Any + Send + Sync>>,
    error_format: u32,
) -> BoxFuture<Result<()>> {
    apply_via_with_keepalive_and_format(call, ctx, config, name, keepalive, error_format)
}

/// [`apply_via`] 的带库保活版本：`LoadedPlugin::apply` 传入 `Arc<DynLib>`。
///
/// 保活窗口 = 注册期（槽位强 owner）+ 在飞调用 lease + 关闭交付 lease；
/// 注销后由 `closed`、句柄重查、lease 失效共同保证不进入 FFI。
pub fn apply_via_with_keepalive(
    call: CallFn,
    ctx: Ctx,
    config: Config,
    name: &str,
    keepalive: Option<Arc<dyn Any + Send + Sync>>,
) -> BoxFuture<Result<()>> {
    apply_via_with_keepalive_and_format(call, ctx, config, name, keepalive, 0)
}

struct ApplyInFlightGuard {
    completed: bool,
    apply_timed_out: Arc<AtomicBool>,
}

impl Drop for ApplyInFlightGuard {
    fn drop(&mut self) {
        if !self.completed {
            self.apply_timed_out.store(true, Ordering::SeqCst);
        }
    }
}

fn invoke_apply_op(
    call: CallFn,
    handle: u64,
    name: &str,
    payload: &str,
    error_format: u32,
) -> Result<()> {
    // apply 是不可重放的（它可能已经注册了一半 effect）；TOO_SMALL 绝不重试。
    let mut buffer = vec![0u8; 16 * 1024];
    let mut out = Out {
        ptr: buffer.as_mut_ptr(),
        cap: buffer.len(),
        len: 0,
    };
    let status = unsafe {
        call(
            handle,
            OP_APPLY,
            Blob::new(payload.as_bytes()),
            Blob::EMPTY,
            &mut out,
        )
    };
    if status == STATUS_TOO_SMALL {
        return Err(format!(
            "插件 {name} 装载失败: apply 输出需要 {} 字节，超过 16 KiB 初始缓冲；\
             写 op 不允许自动重试（会重复执行插件的 apply）",
            out.len
        )
        .into());
    }
    if status != STATUS_OK {
        if out.len > buffer.len() {
            return Err(format!(
                "插件 {name} 装载失败: 插件回调输出声称 {} 字节，超过宿主缓冲 {} 字节；消息未完整写入",
                out.len,
                buffer.len()
            )
            .into());
        }
        buffer.truncate(out.len);
        let message = if status == STATUS_ERR {
            let text = String::from_utf8_lossy(&buffer);
            decode_plugin_error(error_format, &text).error
        } else {
            format!("插件回调返回了意料之外的状态码 {status}")
        };
        return Err(format!("插件 {name} 装载失败: {message}").into());
    }
    Ok(())
}

fn invoke_dispose_op(
    call: CallFn,
    handle: u64,
    error_format: u32,
) -> std::result::Result<(), String> {
    let mut buffer = vec![0u8; 16 * 1024];
    let mut out = Out {
        ptr: buffer.as_mut_ptr(),
        cap: buffer.len(),
        len: 0,
    };
    let status = unsafe { call(handle, OP_DISPOSE, Blob::EMPTY, Blob::EMPTY, &mut out) };
    if status == STATUS_OK {
        return Ok(());
    }
    if out.len > buffer.len() {
        Err(format!(
            "插件卸载通知输出声称 {} 字节，超过宿主缓冲 {} 字节",
            out.len,
            buffer.len()
        ))
    } else {
        buffer.truncate(out.len);
        let text = String::from_utf8_lossy(&buffer);
        let error = decode_plugin_error(error_format, &text);
        Err(format!("插件卸载通知失败: {}", error.error))
    }
}

enum TeardownStatus {
    Completed,
    DeferredToBackground,
}

#[allow(clippy::too_many_arguments)]
fn run_teardown_ffi(
    handle: u64,
    instance: &Instance,
    close_lease: std::result::Result<Option<Arc<dyn Any + Send + Sync>>, ()>,
    credits: HashMap<(u64, u64), usize>,
    outbound: Vec<Outbound>,
    deferred_cleanups: Vec<u64>,
    run_dispose: bool,
    error_format: u32,
) -> Vec<(CleanupErrorKind, String)> {
    let mut errors = Vec::new();
    if close_lease.is_err() {
        return errors;
    }
    let unload_error = ErrorPayload::new("插件已卸载：等待中的调用被结算")
        .with_code("host_lifecycle")
        .with_category("lifecycle")
        .with_certainty("unknown");
    for ((dispatch_id, id), count) in credits {
        let payload = json!({
            "kind": KIND_STREAM_CREDIT,
            "id": id,
            "args": [{ "error": "插件已卸载：这条流已被收回" }],
        })
        .to_string();
        for _ in 0..count {
            let _scope = scope_push(handle, dispatch_id);
            let _ = unsafe {
                call_plugin(
                    instance.call,
                    handle,
                    OP_CALLBACK,
                    &payload,
                    instance.error_format,
                )
            };
        }
    }
    for outbound in outbound {
        deliver_abort_direct(instance, outbound, &unload_error);
    }
    for cb in deferred_cleanups {
        let payload = json!({ "id": cb }).to_string();
        if let Err(message) = unsafe {
            call_plugin(
                instance.call,
                handle,
                cordis_abi::OP_CLEANUP,
                &payload,
                error_format,
            )
        } {
            let detail = format!("插件清理回调失败: {message}");
            eprintln!("[cordis] {detail}");
            errors.push((CleanupErrorKind::HostCleanup, detail));
        }
    }
    if run_dispose {
        if let Err(message) = invoke_dispose_op(instance.call, handle, error_format) {
            eprintln!("[cordis] {message}");
            errors.push((CleanupErrorKind::HostDispose, message));
        }
    }
    drop(close_lease);
    errors
}

#[allow(clippy::too_many_arguments)]
async fn finish_instance_teardown_unbounded(
    handle: u64,
    instance: &Instance,
    close_lease: std::result::Result<Option<Arc<dyn Any + Send + Sync>>, ()>,
    applied_flag: &AtomicBool,
    error_format: u32,
    fiber: Option<&Arc<Fiber>>,
    offload_dispose: bool,
    mut outbound: Vec<Outbound>,
) {
    loop {
        let (drained, tasks) = {
            let mut table = instance.dispatches.lock().unwrap();
            let drained: Vec<DispatchRecord> = table.drain().map(|(_, record)| record).collect();
            let tasks: Vec<_> = instance
                .tasks
                .lock()
                .unwrap()
                .drain()
                .map(|(_, task)| task)
                .collect();
            (drained, tasks)
        };
        #[cfg(test)]
        instance.close_drained.notify_one();
        if drained.is_empty() && tasks.is_empty() {
            break;
        }
        for mut record in drained {
            if let Some(sink) = &record.stream {
                sink.fail_error(
                    ErrorPayload::new("插件已卸载：这条流没有收尾就被收回")
                        .with_code("host_lifecycle")
                        .with_category("lifecycle")
                        .with_certainty("unknown"),
                );
            }
            if let Some(tx) = record.sink.take() {
                let _ = tx.send(Err(ErrorPayload::new("插件已卸载：挂起的入站续延被结算")
                    .with_code("host_lifecycle")
                    .with_category("lifecycle")
                    .with_certainty("unknown")));
            }
            if let Some(entry) = record.outbound {
                outbound.push(entry);
            }
        }
        for task in &tasks {
            task.cancel.cancel();
        }
        for rx in tasks.into_iter().map(|task| task.done) {
            let _ = rx.await;
        }
    }
    finish_teardown_ffi_phase(
        handle,
        instance,
        close_lease,
        applied_flag,
        error_format,
        fiber,
        offload_dispose,
        outbound,
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
async fn finish_teardown_ffi_phase(
    handle: u64,
    instance: &Instance,
    close_lease: std::result::Result<Option<Arc<dyn Any + Send + Sync>>, ()>,
    applied_flag: &AtomicBool,
    error_format: u32,
    fiber: Option<&Arc<Fiber>>,
    offload_dispose: bool,
    outbound: Vec<Outbound>,
) {
    // 出站续延、credit 驳回、延迟 OP_CLEANUP 与 OP_DISPOSE 均在持有 close_lease 下按序执行；
    // 开启 dispose 等待预算时整体移交 blocking 线程池，避免任意插件同步阻塞拖死 async worker。
    let credits = std::mem::take(&mut *instance.credits.lock().unwrap());
    let deferred_cleanups = std::mem::take(&mut *instance.deferred_cleanups.lock().unwrap());
    let run_dispose = applied_flag.load(Ordering::SeqCst);
    let errors = if offload_dispose {
        match tokio::runtime::Handle::try_current() {
            Ok(rt) => {
                let instance_clone = instance.clone();
                match rt
                    .spawn_blocking(move || {
                        run_teardown_ffi(
                            handle,
                            &instance_clone,
                            close_lease,
                            credits,
                            outbound,
                            deferred_cleanups,
                            run_dispose,
                            error_format,
                        )
                    })
                    .await
                {
                    Ok(errs) => errs,
                    Err(err) => {
                        let detail = if err.is_panic() {
                            format!(
                                "阻塞线程执行插件卸载收尾 panic: {}",
                                cordis_core::panic_message(&*err.into_panic())
                            )
                        } else {
                            "插件卸载收尾阻塞任务被取消".to_string()
                        };
                        eprintln!("[cordis] {detail}");
                        vec![(CleanupErrorKind::HostDispose, detail)]
                    }
                }
            }
            Err(_) => run_teardown_ffi(
                handle,
                instance,
                close_lease,
                credits,
                outbound,
                deferred_cleanups,
                run_dispose,
                error_format,
            ),
        }
    } else {
        run_teardown_ffi(
            handle,
            instance,
            close_lease,
            credits,
            outbound,
            deferred_cleanups,
            run_dispose,
            error_format,
        )
    };
    if let Some(fiber) = fiber {
        for (kind, message) in errors {
            fiber.record_cleanup_error(kind, message);
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn finish_instance_teardown(
    handle: u64,
    plugin_name: &str,
    instance: &Instance,
    close_lease: std::result::Result<Option<Arc<dyn Any + Send + Sync>>, ()>,
    applied_flag: &Arc<AtomicBool>,
    error_format: u32,
    fiber: Option<&Arc<Fiber>>,
    offload_dispose: bool,
    drain_budget: Option<std::time::Duration>,
    drain_deadline: Option<tokio::time::Instant>,
    mut outbound: Vec<Outbound>,
) -> TeardownStatus {
    let Some(deadline) = drain_deadline.filter(|_| tokio::runtime::Handle::try_current().is_ok())
    else {
        finish_instance_teardown_unbounded(
            handle,
            instance,
            close_lease,
            applied_flag,
            error_format,
            fiber,
            offload_dispose,
            outbound,
        )
        .await;
        return TeardownStatus::Completed;
    };
    // closed 与登记共用 dispatches 锁；任务凭证不随业务摘表消失。
    loop {
        let (drained, tasks) = {
            let mut table = instance.dispatches.lock().unwrap();
            let drained: Vec<DispatchRecord> = table.drain().map(|(_, record)| record).collect();
            let tasks: Vec<_> = instance
                .tasks
                .lock()
                .unwrap()
                .drain()
                .map(|(_, task)| task)
                .collect();
            (drained, tasks)
        };
        #[cfg(test)]
        instance.close_drained.notify_one();
        if drained.is_empty() && tasks.is_empty() {
            break;
        }
        for mut record in drained {
            if let Some(sink) = &record.stream {
                sink.fail_error(
                    ErrorPayload::new("插件已卸载：这条流没有收尾就被收回")
                        .with_code("host_lifecycle")
                        .with_category("lifecycle")
                        .with_certainty("unknown"),
                );
            }
            if let Some(tx) = record.sink.take() {
                let _ = tx.send(Err(ErrorPayload::new("插件已卸载：挂起的入站续延被结算")
                    .with_code("host_lifecycle")
                    .with_category("lifecycle")
                    .with_certainty("unknown")));
            }
            if let Some(entry) = record.outbound {
                outbound.push(entry);
            }
        }
        for task in &tasks {
            task.cancel.cancel();
        }
        let mut pending_rx: Vec<tokio::sync::oneshot::Receiver<()>> =
            tasks.into_iter().map(|task| task.done).collect();
        let mut idx = 0usize;
        while idx < pending_rx.len() {
            if tokio::time::timeout_at(deadline, &mut pending_rx[idx])
                .await
                .is_err()
            {
                if let Ok(rt) = tokio::runtime::Handle::try_current() {
                    let leftover_rx: Vec<_> = pending_rx.drain(idx..).collect();
                    let limit_ms = drain_budget.map(|d| d.as_millis()).unwrap_or(0);
                    let detail = format!(
                        "等待插件 {plugin_name} 宿主后台任务排空超时（预算 {limit_ms} ms，仍有 {} 条任务在后台运行），\
                         已将动态库保活与 OP_DISPOSE 清理移交后台等待退出",
                        leftover_rx.len()
                    );
                    eprintln!("[cordis] {detail}");
                    if let Some(fiber) = fiber {
                        fiber.record_cleanup_error(CleanupErrorKind::Timeout, detail);
                    }
                    let bg_guard = fiber.map(|f| f.defer_background_cleanup());
                    let instance = instance.clone();
                    let applied_flag = applied_flag.clone();
                    let fiber = fiber.cloned();
                    rt.spawn(async move {
                        let _bg_guard = bg_guard;
                        for rx in leftover_rx {
                            let _ = rx.await;
                        }
                        wait_lifecycle_exclusive(&instance).await;
                        finish_instance_teardown_unbounded(
                            handle,
                            &instance,
                            close_lease,
                            &applied_flag,
                            error_format,
                            fiber.as_ref(),
                            true,
                            outbound,
                        )
                        .await;
                        unregister_instance(handle);
                    });
                    return TeardownStatus::DeferredToBackground;
                }
                for rx in pending_rx.drain(idx..) {
                    let _ = rx.await;
                }
                break;
            }
            idx += 1;
        }
    }
    finish_teardown_ffi_phase(
        handle,
        instance,
        close_lease,
        applied_flag,
        error_format,
        fiber,
        offload_dispose,
        outbound,
    )
    .await;
    TeardownStatus::Completed
}

/// 带插件错误能力位的内部版本；公开 wrapper 传 0，保持旧调用方语义。
fn apply_via_with_keepalive_and_format(
    call: CallFn,
    ctx: Ctx,
    config: Config,
    name: &str,
    keepalive: Option<Arc<dyn Any + Send + Sync>>,
    error_format: u32,
) -> BoxFuture<Result<()>> {
    let name = name.to_string();
    Box::pin(async move {
        // 容量准入失败 = 插件装载失败；此时还没有登记实例、也没有 effect 需要清理。
        let handle = register_instance_with_keepalive(ctx.clone(), call, keepalive, error_format)
            .map_err(|err| format!("插件 {name} 装载失败: {err}"))?;
        let applied = Arc::new(AtomicBool::new(false));
        let applied_flag = applied.clone();
        // 用 `try_effect`：若注册时 fiber 已被并发处置，不能让 `effect` 的 assert
        // 炸掉 apply，也不能把刚登记的实例留在 slab 里。
        let Some(fiber) = ctx.fiber_opt() else {
            unregister_instance(handle);
            return Err("插件 fiber 已被处置，无法注册清理 effect".into());
        };
        let fiber_weak = Arc::downgrade(&fiber);
        let plugin_name = name.clone();
        let registered = fiber.try_effect(move |_| {
            Disposable::new(move || {
                let fiber_weak = fiber_weak.clone();
                let applied_flag = applied_flag.clone();
                let plugin_name = plugin_name.clone();
                Box::pin(async move {
                    // 先关闭准入，再异步等待已经登记的同步调用退出。
                    let instance = instances().read().unwrap().get(handle);
                    let fiber = fiber_weak.upgrade();
                    if let Some(instance) = instance {
                        let close_lease = {
                            let _table = instance.dispatches.lock().unwrap();
                            instance.closed.store(true, Ordering::SeqCst);
                            call_lease(&instance)
                        };
                        let drain_timeout = instance
                            .ctx
                            .store
                            .lifecycle_policy()
                            .drain_timeout_ms
                            .map(std::time::Duration::from_millis);
                        let drain_deadline =
                            drain_timeout.map(|limit| tokio::time::Instant::now() + limit);
                        let drain_timed_out = if instance.apply_timed_out.load(Ordering::SeqCst) {
                            *instance.lifecycle.lock().unwrap() > 0
                        } else if let (Some(deadline), Ok(_)) =
                            (drain_deadline, tokio::runtime::Handle::try_current())
                        {
                            tokio::time::timeout_at(deadline, wait_lifecycle_exclusive(&instance))
                                .await
                                .is_err()
                        } else {
                            wait_lifecycle_exclusive(&instance).await;
                            false
                        };
                        if drain_timed_out {
                            if let Ok(rt) = tokio::runtime::Handle::try_current() {
                                let detail = if instance.apply_timed_out.load(Ordering::SeqCst) {
                                    format!(
                                        "插件 {plugin_name} 的 OP_APPLY 超时后仍在运行，\
                                         已将动态库保活与 OP_DISPOSE 清理移交后台等待原生调用退出"
                                    )
                                } else {
                                    let limit_ms =
                                        drain_timeout.map(|d| d.as_millis()).unwrap_or(0);
                                    format!(
                                        "等待插件 {plugin_name} 在途原生调用排空超时（预算 {limit_ms} ms），\
                                         已将动态库保活与 OP_DISPOSE 清理移交后台等待原生调用退出"
                                    )
                                };
                                eprintln!("[cordis] {detail}");
                                if let Some(fiber) = &fiber {
                                    fiber.record_cleanup_error(CleanupErrorKind::Timeout, detail);
                                }
                                // 先就地结算宿主侧等待者（流与入站挂起续延）并取消宿主后台任务，
                                // 避免外部调用方一直挂起；涉及插件 FFI 的回调、OP_DISPOSE 与
                                // 实例注销/动态库卸载必须严格等到最后一次原生调用退出后由后台任务完成。
                                {
                                    let mut table = instance.dispatches.lock().unwrap();
                                    for record in table.values_mut() {
                                        if let Some(sink) = &record.stream {
                                            sink.fail_error(
                                                ErrorPayload::new(
                                                    "插件已卸载：这条流没有收尾就被收回",
                                                )
                                                .with_code("host_lifecycle")
                                                .with_category("lifecycle")
                                                .with_certainty("unknown"),
                                            );
                                        }
                                        if let Some(tx) = record.sink.take() {
                                            let _ = tx.send(Err(ErrorPayload::new(
                                                "插件已卸载：挂起的入站续延被结算",
                                            )
                                            .with_code("host_lifecycle")
                                            .with_category("lifecycle")
                                            .with_certainty("unknown")));
                                        }
                                    }
                                    let tasks = instance.tasks.lock().unwrap();
                                    for task in tasks.values() {
                                        task.cancel.cancel();
                                    }
                                }
                                let bg_guard = fiber.as_ref().map(|f| f.defer_background_cleanup());
                                rt.spawn(async move {
                                    let _bg_guard = bg_guard;
                                    wait_lifecycle_exclusive(&instance).await;
                                    finish_instance_teardown_unbounded(
                                        handle,
                                        &instance,
                                        close_lease,
                                        &applied_flag,
                                        error_format,
                                        fiber.as_ref(),
                                        true,
                                        Vec::new(),
                                    )
                                    .await;
                                    unregister_instance(handle);
                                });
                                return;
                            }
                            // 无运行时无法移交后台：保活红线优先，退回同步等待。
                            wait_lifecycle_exclusive(&instance).await;
                        }
                        let offload_dispose = fiber
                            .as_ref()
                            .is_some_and(|f| f.should_offload_dispose());
                        if matches!(
                            finish_instance_teardown(
                                handle,
                                &plugin_name,
                                &instance,
                                close_lease,
                                &applied_flag,
                                error_format,
                                fiber.as_ref(),
                                offload_dispose,
                                drain_timeout,
                                drain_deadline,
                                Vec::new(),
                            )
                            .await,
                            TeardownStatus::DeferredToBackground
                        ) {
                            return;
                        }
                    }
                    unregister_instance(handle);
                })
            })
        });
        if let Err(err) = registered {
            unregister_instance(handle);
            return Err(err);
        }
        let payload = config.to_string();
        let offload_apply = ctx.store.lifecycle_policy().apply_timeout_ms.is_some();
        if offload_apply {
            if let Ok(rt) = tokio::runtime::Handle::try_current() {
                let Some(instance) = instances().read().unwrap().get(handle) else {
                    return Err(format!("插件 {name} 装载失败: 实例已被提前注销").into());
                };
                let Some(active) = admit_call(&instance) else {
                    return Err(format!("插件 {name} 装载失败: 实例已关闭").into());
                };
                let mut in_flight = ApplyInFlightGuard {
                    completed: false,
                    apply_timed_out: instance.apply_timed_out.clone(),
                };
                let applied_for_apply = applied.clone();
                let name_owned = name.clone();
                let correlation_id = cordis_core::current_correlation_id();
                let result = rt
                    .spawn_blocking(move || {
                        let _active = active;
                        let _corr =
                            correlation_id.map(|cid| cordis_core::enter_correlation(Some(cid)));
                        let res =
                            invoke_apply_op(call, handle, &name_owned, &payload, error_format);
                        if res.is_ok() {
                            applied_for_apply.store(true, Ordering::SeqCst);
                        }
                        res
                    })
                    .await;
                in_flight.completed = true;
                return match result {
                    Ok(res) => res,
                    Err(err) => Err(if err.is_panic() {
                        format!(
                            "插件 {name} 装载失败: apply 阻塞线程 panic: {}",
                            cordis_core::panic_message(&*err.into_panic())
                        )
                        .into()
                    } else {
                        format!("插件 {name} 装载失败: apply 阻塞任务被取消").into()
                    }),
                };
            }
        }
        let Some(instance) = instances().read().unwrap().get(handle) else {
            return Err(format!("插件 {name} 装载失败: 实例已被提前注销").into());
        };
        let Some(_active) = admit_call(&instance) else {
            return Err(format!("插件 {name} 装载失败: 实例已关闭").into());
        };
        invoke_apply_op(call, handle, &name, &payload, error_format)?;
        applied.store(true, Ordering::SeqCst);
        Ok(())
    })
}

fn parse_inject(raw: &str) -> Inject {
    match serde_json::from_str::<Value>(raw) {
        Ok(Value::Array(list)) => Inject(
            list.iter()
                .filter_map(|value| value.as_str())
                .map(|name| (name.into(), Config::Null))
                .collect(),
        ),
        Ok(Value::Object(map)) => Inject(
            map.into_iter()
                .map(|(name, config)| (name.into(), config))
                .collect(),
        ),
        _ => Inject::default(),
    }
}

/// 解析 `provision` 声明（JSON 字符串数组）。v1 插件读出空串 ⇒ 未声明 ⇒ `[]`。
fn parse_provision(raw: &str) -> Vec<String> {
    serde_json::from_str::<Value>(raw)
        .ok()
        .and_then(|value| value.as_array().cloned())
        .map(|list| {
            list.iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

// ------------------------------------------------------------ 能力清单（M5）

/// 能力面的来源标签。CLI 与 [`Capabilities::origin`] 共用，避免文案漂移。
pub const CAPABILITY_ORIGIN: &str = "plugin-self-description";

/// 一份插件的**能力面**：挂载之前就能读到的三项声明（A5 的机制）。
///
/// - `inject`：这个插件声明要消费的服务；
/// - `provision`：它声明自己能安装的 key（命名空间与 `Store::impls_owned_by` 同一口径）；
/// - `volatile`：配置只差这些路径时不需要重挂（M4-5）。
///
/// 三项都是**静态能力**：原生插件从 `Plugin` 上读，ABI 插件从 `PluginDesc`（inject /
/// provision）与 `OP_VOLATILE_PATHS`（volatile，约定句柄 0）读——
/// 因此 [`probe_capabilities`] 不需要挂载、也不需要实例。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Capabilities {
    pub plugin: String,
    /// 插件自述的可选版本（诊断/身份用，不是 ABI 授权）。
    pub version: Option<String>,
    pub inject: Vec<String>,
    pub provision: Vec<String>,
    pub volatile: Vec<String>,
    /// 插件自述的配置 schema；`None` = 未声明或不是合法 JSON 对象。
    ///
    /// 私有字段 + [`Capabilities::schema`]：阶段 3 优先暴露方法，不新增公开字段。
    schema: Option<String>,
    /// 声明了 schema 但无法展示的原因（`None` = 未声明或合法）。
    schema_error: Option<String>,
}

impl Capabilities {
    /// 从一个**已加载、未挂载**的插件对象取能力面（原生插件与 `LoadedPlugin` 都走它）。
    pub fn of_plugin(plugin: &dyn Plugin) -> Self {
        let schema = plugin
            .config_schema()
            .map(schema_from_text)
            .unwrap_or_default();
        Self {
            plugin: plugin.name().to_string(),
            version: plugin.version().map(str::to_string),
            inject: plugin
                .inject()
                .0
                .iter()
                .map(|(name, _)| name.to_string())
                .collect(),
            provision: plugin.provision(),
            volatile: plugin.volatile_paths(),
            schema: schema.text,
            schema_error: schema.error,
        }
    }

    /// 插件自述的配置 schema（合法 JSON 对象时返回原文）。
    ///
    /// 这是**展示用元数据**：宿主不据此验证配置，也不把它当成批准。
    pub fn schema(&self) -> Option<&str> {
        self.schema.as_deref()
    }

    /// 声明了 schema 但无法展示的原因（`None` = 未声明或合法）。
    pub fn schema_error(&self) -> Option<&str> {
        self.schema_error.as_deref()
    }

    /// 能力面的来源。当前所有能力都来自插件自己的声明，不是宿主审批。
    pub fn origin(&self) -> &'static str {
        CAPABILITY_ORIGIN
    }

    /// 直接从一份 `PluginDesc` 读（**不经动态库与实例**，M5 的 ABI 生产路径）。
    ///
    /// # Safety
    /// `desc` 必须按 ABI 约定有效（与 [`LoadedPlugin::open`] 的读法同一套判界规则：
    /// 先校验 `abi_version` / `size`，`provision` 是尾字段，靠 `field_at` 按 `size`
    /// 判断存在性）。
    pub unsafe fn of_desc(desc: *const crate::PluginDesc) -> Self {
        // 与 `LoadedPlugin::open` 相同：门禁必须先于任何尾字段读取。调用方即使传了
        // 尺寸不足的 desc，也只会得到默认能力面，而不是越界读。
        let abi_version = (*desc).abi_version;
        let size = (*desc).size as usize;
        if desc_gate(abi_version, size, cordis_abi::PLUGIN_DESC_PREFIX_SIZE).is_err() {
            return Self::default();
        }
        // 不用 `&*desc`：v1 插件的分配可能只有 72B，形成 96B `&PluginDesc` 本身就是
        // 超尺寸引用。这里只读冻结前缀内的字段，尾字段一律走 `field_at(desc, ...)`。
        let attach = (*desc).attach;
        // 与 `LoadedPlugin::open` 同一套前置：**先把函数表交给插件**（copy-on-attach），
        // 否则插件侧的 `call` 会以"ABI 版本不匹配"拒绝服务（它还没有宿主表）。
        unsafe { attach(host_api()) };
        let provision = PluginDesc::field_at::<crate::Blob>(
            desc,
            std::mem::offset_of!(crate::PluginDesc, provision),
        )
        .map(|blob| String::from_utf8_lossy(blob.as_bytes()).into_owned())
        .unwrap_or_default();
        let inject_raw = String::from_utf8_lossy((*desc).inject.as_bytes());
        let inject = parse_inject(&inject_raw);
        // 与 `Capabilities::of_plugin` 保持一致：保留插件声明顺序，不在这里排序。
        let inject: Vec<String> = inject.0.iter().map(|(name, _)| name.to_string()).collect();
        let volatile = match unsafe {
            call_op_read(
                (*desc).call,
                0,
                cordis_abi::OP_VOLATILE_PATHS,
                "",
                PluginDesc::error_format_at(desc),
            )
        } {
            Ok(CallOutcome::Ok(value)) => value
                .as_array()
                .map(|list| {
                    list.iter()
                        .filter_map(|item| item.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
            // 旧插件不认识这个 op ⇒ 空声明（与 `LoadedPlugin::volatile_paths` 同一条降级）。
            Ok(CallOutcome::Unsupported) => Vec::new(),
            Ok(CallOutcome::Err(payload)) => {
                eprintln!("[cordis] 插件 volatile 声明查询失败: {}", payload.error);
                Vec::new()
            }
            Err(message) => {
                eprintln!("[cordis] 插件 volatile 声明查询出错: {message}");
                Vec::new()
            }
        };
        let schema = schema_from_blob((*desc).schema);
        Self {
            plugin: String::from_utf8_lossy((*desc).id.as_bytes()).into_owned(),
            // 冻结的 PluginDesc 没有 semver 字段；sidecar 版本不会从这里冒充。
            version: None,
            inject,
            provision: parse_provision(&provision),
            volatile,
            schema: schema.text,
            schema_error: schema.error,
        }
    }
}

/// 一次 schema 读取的结果：合法声明与“声明了但不合法”必须可区分。
#[derive(Default)]
struct SchemaRead {
    text: Option<String>,
    error: Option<String>,
}

fn schema_from_text(text: String) -> SchemaRead {
    if text.len() > cordis_abi::MAX_CONFIG_SCHEMA_BYTES {
        return SchemaRead {
            text: None,
            error: Some(format!(
                "schema 超过 {} 字节上限",
                cordis_abi::MAX_CONFIG_SCHEMA_BYTES
            )),
        };
    }
    if !is_json_object(&text) {
        return SchemaRead {
            text: None,
            error: Some("schema 不是合法 JSON 对象".to_string()),
        };
    }
    SchemaRead {
        text: Some(text),
        error: None,
    }
}

/// 读一个 schema blob：只接受 UTF-8 的 JSON 对象；其他情况记录为非法声明。
///
/// schema 在冻结前缀里，`desc_gate` 通过后可直接读；Blob 指向插件内存，
/// 这里立刻拷成 `String`，不把指针带出插件生命周期。
fn schema_from_blob(blob: crate::Blob) -> SchemaRead {
    // `desc_gate` 只保证描述表前缀可读；Blob 的 ptr/len 仍由插件提供。
    // 半初始化或损坏的 Blob 不能变成 UB：NULL 与超过 `isize::MAX` 的长度
    // 记录为非法声明。ABI 契约要求 ptr 指向 `len` 字节，但宿主不应把
    // 违反契约的输入升级成进程崩溃。
    if blob.len == 0 {
        return SchemaRead::default();
    }
    if blob.ptr.is_null() || blob.len > isize::MAX as usize {
        return SchemaRead {
            text: None,
            error: Some("schema Blob 指针为空或长度非法".to_string()),
        };
    }
    if blob.len > cordis_abi::MAX_CONFIG_SCHEMA_BYTES {
        return SchemaRead {
            text: None,
            error: Some(format!(
                "schema 超过 {} 字节上限",
                cordis_abi::MAX_CONFIG_SCHEMA_BYTES
            )),
        };
    }
    // SAFETY：ptr 非空、len 非 0 且不超过 `isize::MAX` 与 schema 上限；
    // 其余可读性由 ABI 契约保证（`desc_gate` 已确保描述表本身位于冻结前缀内）。
    let bytes = unsafe { std::slice::from_raw_parts(blob.ptr, blob.len) };
    let Ok(text) = std::str::from_utf8(bytes) else {
        return SchemaRead {
            text: None,
            error: Some("schema 不是 UTF-8".to_string()),
        };
    };
    schema_from_text(text.to_string())
}

fn is_json_object(text: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(text)
        .map(|value| value.is_object())
        .unwrap_or(false)
}

/// **预挂载探测**（A5 的机制）：解析 → 暂存 → 打开 → 探活 → 读能力面 → **立刻放手**。
///
/// 与装载共用 [`plugin_source`]（D8 的 seam）：真宿主走 `DiskPluginSource`，
/// 测试可以塞假源（所以"读能力面"这条链本身不需要动态库就能验证，见
/// `crates/host/tests/probe_capabilities.rs`）。配置了进程级
/// [`crate::source::active_policy`] 时，`probe_capabilities_in` 会在打开动态库
/// **之前**先验原始 artifact 的 sidecar manifest 与 allow-list。
/// **不挂载**：不建 fiber、不注册实例、不派生 ctx。但它仍会加载动态库并调用
/// `OP_PING` / `OP_VOLATILE_PATHS`，插件代码可能产生副作用；只对受信插件使用
/// （CLI 的 `inspect` 就是这么用的）。
pub fn probe_capabilities(path: &std::path::Path) -> Result<Capabilities> {
    let search: Vec<PathBuf> = path
        .parent()
        .map(|dir| vec![dir.to_path_buf()])
        .unwrap_or_default();
    probe_capabilities_in(path, &search)
}

/// [`probe_capabilities`] 的显式搜索目录版本（CLI 的 `--plugin-dir` 用）。
pub fn probe_capabilities_in(path: &std::path::Path, search: &[PathBuf]) -> Result<Capabilities> {
    probe_capabilities_with(plugin_source(), path, search)
}

/// 探测的**来源版本**（私有）：准入与装载用**同一个**来源对象、**同一份**结论。
///
/// O9：以前这里读进程级 [`crate::source::admission`]，而 `load` 可能走的是另一个
/// 带局部策略的来源——同一次探测里"准入按全局、打开按局部"是两份快照。把来源
/// 显式传进来之后，结论只来自一个地方。
fn probe_capabilities_with(
    source: Arc<dyn loader::PluginSource>,
    path: &std::path::Path,
    search: &[PathBuf],
) -> Result<Capabilities> {
    let entry = patch::Entry {
        name: path.to_string_lossy().into_owned(),
        ..patch::Entry::default()
    };
    // P0-1a：有生效来源策略时，inspect 也必须在打开动态库**之前**验证原始 artifact。
    // 先解析到真实路径（`entry.name` 可能只是裸插件名）；没有策略时保持原有顺序，
    // 这样测试注入的假 `PluginSource` 仍能在没有磁盘文件的情况下工作。
    //
    // O9：结论只取**一次**，并原样交给 `load_admitted`。以前的写法是"自己校验一次，
    // 再让 `source.load` 去读第二次准入"：两步之间全局被改，打开就走了另一份结论
    // （甚至走未校验分支），暂存副本也不再与已验证的 hash 绑定。
    let admission = source.admission();
    if let Some(policy) = admission.resolve()? {
        let resolved = resolve_plugin_path(&entry.name, search)?;
        policy.verify(&resolved)?;
    }
    let loaded = source.load_admitted(&entry, search, next_use_revision(), &admission)?;
    // `loaded` 在这一句之后 drop：先 FreeLibrary、后删暂存副本（M0b 的字段序保证）。
    Ok(Capabilities::of_plugin(&*loaded.plugin))
}

#[cfg(test)]
mod credits_limit_tests;

#[cfg(test)]
mod derived_dispose_tests;

#[cfg(test)]
mod observer_close_tests;

#[cfg(test)]
mod panic_close_tests;

#[cfg(test)]
mod tests;
