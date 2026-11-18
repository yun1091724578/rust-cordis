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
pub mod js;
pub mod loader;
pub mod patch;
pub mod plan;
pub mod source;

use crate::source::PluginSourcePolicy;

use cordis_abi::{
    Blob, EntryFn, ErrorPayload, HostApi, Out, PluginDesc, ABI_VERSION, ENTRY_SYMBOL,
    ERROR_FORMAT_V1, KIND_DISPATCH, KIND_EVENT, KIND_IFACE, KIND_NEXT, KIND_STREAM_CREDIT,
    OP_APPLY, OP_CALLBACK, OP_DISPOSE, STATUS_ERR, STATUS_OK, STATUS_PENDING, STATUS_STREAM_GONE,
    STATUS_TOO_SMALL, STATUS_UNSUPPORTED,
};
use cordis_core::{
    BoxFuture, CancelToken, Config, Ctx, Disposable, Dyn, Error, ExternService, ExternValue, Fiber,
    Inject, Level, Next, Plugin, PluginKey, PushOutcome, Result, Ret, Store, StreamObserver,
    StreamOutcome, StreamSink,
};
use serde::Deserialize;
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

fn instances() -> &'static Mutex<Slab> {
    static INSTANCES: OnceLock<Mutex<Slab>> = OnceLock::new();
    INSTANCES.get_or_init(|| Mutex::new(Slab::default()))
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
    let mut slab = instances().lock().unwrap();
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
    let removed = instances().lock().unwrap().remove(handle);
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
        let mut slab = instances().lock().unwrap();
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
    let mut slab = instances().lock().unwrap();
    let (_, owner) = slab.slot_mut(handle)?;
    owner.take()
}

/// 按**上下文身份**找回实例句柄（M4-5 的 volatile 推送要用它）。
///
/// 为什么不在 `LoadedPlugin` 上缓存句柄：同一个插件对象可以挂多行（多实例），
/// `&self` 里存不下"当前实例"；而句柄由 `apply_via` 在装载时注册、随 fiber 卸载注销，
/// 唯一稳定的关联就是 ctx 本身。表很小，线性查找足够（volatile 提交是低频操作）。
fn instance_handle_of(ctx: &Ctx) -> Option<u64> {
    let slab = instances().lock().unwrap();
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
    instances().lock().unwrap().live()
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
        let table = derived_table().lock().unwrap();
        let entry = table.get(&handle)?;
        Some(Resolved {
            ctx: entry.ctx.clone(),
            instance: entry.instance.clone(),
            owner: entry.owner,
        })
    } else {
        let instance = instances().lock().unwrap().get(handle)?;
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
        let instance = instances().lock().unwrap().get(self.handle);
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
    let target = instances().lock().unwrap().get(handle);
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
    let instance = instances().lock().unwrap().get(handle);
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

/// #22（ADR 0001 §5）：失败收尾——`reason` 直达消费者（缓冲丢弃）。
#[allow(clippy::result_large_err)]
unsafe extern "C" fn host_stream_fail(
    handle: u64,
    dispatch_id: u64,
    reason: Blob,
    out: *mut Out,
) -> i32 {
    let reason = reason.as_str().to_string();
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
            sink.fail(payload.error.clone());
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

/// 派发看门狗（M2，D3；P0-2 起按 App 配置）：宿主等待 `ctx_fulfill` 的上限。
///
/// `None` = 不设上限（默认；与 M2 之前的行为逐字相同）。每个实例在注册时从
/// **所属 App** 的策略注册表取一份快照（见 [`set_dispatch_policy_for`]）；
/// 没有 per-App 条目时回落到进程级的 [`set_dispatch_policy`]。
/// **不设上限时，行为不端的插件可以占住一次派发**——这正是风险 1，缓解手段是
/// 卸载排水与看门狗。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DispatchPolicy {
    /// 入站挂起续延与流式挂起的上限。
    pub timeout_ms: Option<u64>,
    /// 出站 `ctx_call_iface` 的上限（`None` = 不设上限，默认）。
    ///
    /// 这是看门狗而不是业务超时：默认关闭，宿主显式开启后才会把“提供者永远
    /// 不结算”变成一条可读的 `HOST_TIMEOUT` 错误。合法的长调用应保持 `None`
    /// 或设置足够大的值。
    pub outbound_timeout_ms: Option<u64>,
}

impl DispatchPolicy {
    /// 与 [`HostLimits::new`] 对应的最小构造入口；0.1 仍保留公开字段。
    pub fn new(timeout_ms: Option<u64>, outbound_timeout_ms: Option<u64>) -> Self {
        Self {
            timeout_ms,
            outbound_timeout_ms,
        }
    }
}

static DISPATCH_POLICY: RwLock<DispatchPolicy> = RwLock::new(DispatchPolicy {
    timeout_ms: None,
    outbound_timeout_ms: None,
});

/// 设置**进程级**派发看门狗（覆盖式，后设生效）。
///
/// 这是**回落**而不是唯一入口：用 [`set_dispatch_policy_for`] 给单个 App 设置
/// 策略后，该 App 的实例优先使用自己的策略；只有没有 per-App 条目（或条目随
/// App 释放被清掉）时才读这里。已有实例保留注册时的快照。
pub fn set_dispatch_policy(policy: DispatchPolicy) {
    *DISPATCH_POLICY.write().unwrap() = policy;
}

fn dispatch_policy() -> DispatchPolicy {
    *DISPATCH_POLICY.read().unwrap()
}

/// 一个 App 的看门狗条目：`store` 是**弱引用**，App 释放后条目可被顺手清掉，
/// 注册表不会把 `Store` 吊住。
struct AppDispatchPolicy {
    store: Weak<Store>,
    policy: DispatchPolicy,
}

/// 键是 `Arc::as_ptr(&ctx.store)`（App 的身份；派生 ctx 共享同一个 `Arc<Store>`）。
fn app_dispatch_policies() -> &'static Mutex<HashMap<usize, AppDispatchPolicy>> {
    static POLICIES: OnceLock<Mutex<HashMap<usize, AppDispatchPolicy>>> = OnceLock::new();
    POLICIES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 设置**单个 App** 的派发看门狗。
///
/// App 由 `ctx.store` 的同一性标识：同一个 `App` 上派生的所有 ctx（isolate /
/// intercept / 装载器行）共享这条策略；不同 `App` 互不影响。装载器在
/// `LoaderConfig.dispatch_policy` 有值时会在挂载前调用它。
///
/// 只影响**之后注册**的实例（实例在注册时取快照）；已注册实例不会被追溯修改。
pub fn set_dispatch_policy_for(ctx: &Ctx, policy: DispatchPolicy) {
    let key = Arc::as_ptr(&ctx.store) as usize;
    let mut policies = app_dispatch_policies().lock().unwrap();
    policies.retain(|_, entry| entry.store.strong_count() > 0);
    policies.insert(
        key,
        AppDispatchPolicy {
            store: Arc::downgrade(&ctx.store),
            policy,
        },
    );
}

/// 内部读取：per-App 优先，缺失或 App 已释放时回落到进程级策略。
fn dispatch_policy_for_ctx(ctx: &Ctx) -> DispatchPolicy {
    let key = Arc::as_ptr(&ctx.store) as usize;
    let mut policies = app_dispatch_policies().lock().unwrap();
    match policies.get(&key) {
        Some(entry) if entry.store.strong_count() > 0 => entry.policy,
        Some(_) => {
            policies.remove(&key);
            dispatch_policy()
        }
        None => dispatch_policy(),
    }
}

/// 宿主容量/准入上限（P0-3）。
///
/// 这是**宿主侧表项准入**：限制每个 App 的实例数、每个实例的派发记录、挂起续延
/// 与流数量。超限时宿主拒绝这次准入，返回可读的结构化错误，**不执行插件代码**。
///
/// 它不是每插件的内存/CPU 硬上限——T2 插件与宿主同进程共享地址空间；真正的
/// 资源硬限制需要 cgroup/Job Object 或进程外载体（T3）。
///
/// 所有字段都必须大于 0。0 **不是**“无限”，而是非法配置：装载器 / 设置入口会
/// 拒绝它，避免把“忘记填”静默解释成“永不接纳”。缺省值有意放宽以保持既有行为：
/// 1024 / 1024 / 256 / 32。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct HostLimits {
    /// 单个 App 内同时注册的实例上限。
    #[serde(deserialize_with = "deserialize_nonzero_usize")]
    pub max_instances_per_app: usize,
    /// 单个实例的在飞派发记录总上限（入站、iface、出站与流共用这张表）。
    #[serde(deserialize_with = "deserialize_nonzero_usize")]
    pub max_dispatches_per_instance: usize,
    /// 单个实例同时挂起的入站续延上限。
    #[serde(deserialize_with = "deserialize_nonzero_usize")]
    pub max_pending_per_instance: usize,
    /// 单个实例同时存在的流上限。
    #[serde(deserialize_with = "deserialize_nonzero_usize")]
    pub max_streams_per_instance: usize,
}

/// 手写 `Default`：派生 `Default` 会给 usize 全 0，而 0 是非法配置。
impl Default for HostLimits {
    fn default() -> Self {
        Self {
            max_instances_per_app: 1024,
            max_dispatches_per_instance: 1024,
            max_pending_per_instance: 256,
            max_streams_per_instance: 32,
        }
    }
}

impl HostLimits {
    /// 带校验的构造入口：嵌入方可在注册前发现 0/非法上限。
    /// 0.1 仍保留公开字段与字面量构造，这个入口只是更安全的路径。
    pub fn new(
        max_instances_per_app: usize,
        max_dispatches_per_instance: usize,
        max_pending_per_instance: usize,
        max_streams_per_instance: usize,
    ) -> std::result::Result<Self, String> {
        let limits = Self {
            max_instances_per_app,
            max_dispatches_per_instance,
            max_pending_per_instance,
            max_streams_per_instance,
        };
        limits.validate()?;
        Ok(limits)
    }

    fn validate(self) -> std::result::Result<(), String> {
        let fields = [
            ("maxInstancesPerApp", self.max_instances_per_app),
            ("maxDispatchesPerInstance", self.max_dispatches_per_instance),
            ("maxPendingPerInstance", self.max_pending_per_instance),
            ("maxStreamsPerInstance", self.max_streams_per_instance),
        ];
        for (name, value) in fields {
            if value == 0 {
                return Err(format!(
                    "HostLimits.{name} 不能为 0；0 不是“无限”，请给出正整数"
                ));
            }
        }
        Ok(())
    }
}

fn deserialize_nonzero_usize<'de, D>(deserializer: D) -> std::result::Result<usize, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = usize::deserialize(deserializer)?;
    if value == 0 {
        return Err(serde::de::Error::custom(
            "HostLimits 的容量上限不能为 0（0 不是“无限”）",
        ));
    }
    Ok(value)
}

static DEFAULT_HOST_LIMITS: RwLock<HostLimits> = RwLock::new(HostLimits {
    max_instances_per_app: 1024,
    max_dispatches_per_instance: 1024,
    max_pending_per_instance: 256,
    max_streams_per_instance: 32,
});

/// 设置**进程级**容量准入上限（覆盖式，后设生效）。
///
/// 与 [`set_dispatch_policy`] 同一套回落语义：只有没有 per-App 条目（或条目随
/// App 释放被清掉）的 App 才读这里。传入 0 会返回错误且不改动当前值。
pub fn set_default_host_limits(limits: HostLimits) -> std::result::Result<(), String> {
    limits.validate()?;
    *DEFAULT_HOST_LIMITS.write().unwrap() = limits;
    Ok(())
}

fn default_host_limits() -> HostLimits {
    *DEFAULT_HOST_LIMITS.read().unwrap()
}

/// 一个 App 的容量上限条目：弱引用 Store，App 释放后条目可被清理。
struct AppHostLimits {
    store: Weak<Store>,
    limits: HostLimits,
}

/// 键是 `Arc::as_ptr(&ctx.store)`（App 的身份；派生 ctx 共享同一个 `Arc<Store>`）。
fn app_host_limits() -> &'static Mutex<HashMap<usize, AppHostLimits>> {
    static LIMITS: OnceLock<Mutex<HashMap<usize, AppHostLimits>>> = OnceLock::new();
    LIMITS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 设置**单个 App** 的容量准入上限。
///
/// App 由 `ctx.store` 的同一性标识：同一个 `App` 上派生的所有 ctx（isolate /
/// intercept / 装载器行）共享这套上限；不同 `App` 互不影响。装载器在
/// `LoaderConfig.host_limits` 有值时会在挂载前调用它。
///
/// 只影响**之后注册**的实例（实例在注册时取快照）；已注册实例不会被追溯修改。
/// 任一项为 0 会返回错误且不改动当前值。
pub fn set_host_limits_for(ctx: &Ctx, limits: HostLimits) -> std::result::Result<(), String> {
    limits.validate()?;
    let key = Arc::as_ptr(&ctx.store) as usize;
    let mut registry = app_host_limits().lock().unwrap();
    registry.retain(|_, entry| entry.store.strong_count() > 0);
    registry.insert(
        key,
        AppHostLimits {
            store: Arc::downgrade(&ctx.store),
            limits,
        },
    );
    Ok(())
}

/// 内部读取：per-App 优先；缺失或 App 已释放时回落到进程级默认。
fn host_limits_for_ctx(ctx: &Ctx) -> HostLimits {
    let key = Arc::as_ptr(&ctx.store) as usize;
    let mut registry = app_host_limits().lock().unwrap();
    registry.retain(|_, entry| entry.store.strong_count() > 0);
    registry
        .get(&key)
        .map(|entry| entry.limits)
        .unwrap_or_else(default_host_limits)
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
        self.changed.notify_one();
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
        .lock()
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
        match outcome {
            SettleOutcome::Settled(Ok(Ok(value))) => Ok(value),
            SettleOutcome::Settled(Ok(Err(payload))) => Err(CallFailure::from_payload(payload)),
            SettleOutcome::Settled(Err(_)) => Err(CallFailure::new(
                format!("{what}：结算通道已关闭（插件已卸载）"),
                "host_lifecycle",
                "lifecycle",
                "unknown",
            )),
            SettleOutcome::Cancelled => {
                // 收回这条派发：记录连同它上面的流一起（流必须以**错误**结束）。
                let payload =
                    ErrorPayload::new(format!("{what}：已取消——等待中的结算被调用方撤销"))
                        .with_code("host_cancel")
                        .with_category("cancel")
                        .with_certainty("unknown");
                revoke_dispatch(&instance, dispatch_id, &payload);
                Err(CallFailure::from_payload(payload))
            }
            SettleOutcome::TimedOut(ms) => {
                let payload = ErrorPayload::new(format!("{what}：等待插件结算超时（{ms}ms）"))
                    .with_code("HOST_TIMEOUT")
                    .with_category("timeout")
                    .with_certainty("unknown");
                revoke_dispatch(&instance, dispatch_id, &payload);
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
) -> std::result::Result<Inbound, CallFailure> {
    let _lifecycle = admit_call(instance).ok_or_else(|| {
        CallFailure::new(
            format!("ctx 句柄 {handle} 已失效（插件正在卸载或库已释放）"),
            "host_lifecycle",
            "lifecycle",
            "unknown",
        )
    })?;
    {
        let live = instances().lock().unwrap().get(handle);
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
        invoke_raw(instance.call, handle, op, &payload.to_string())
    };
    match status {
        STATUS_OK => {
            let removed = instance.dispatches.lock().unwrap().remove(&dispatch_id);
            drop(removed);
            if text.is_empty() {
                return Ok(Inbound::Ready(Value::Null));
            }
            let parsed: Value = serde_json::from_str(&text).map_err(|err| {
                CallFailure::new(
                    format!("插件回调回复不是合法 JSON: {err}"),
                    "plugin_protocol",
                    "protocol",
                    "unknown",
                )
            })?;
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
        }))),
        STATUS_ERR => {
            let removed = instance.dispatches.lock().unwrap().remove(&dispatch_id);
            drop(removed);
            Err(CallFailure::from_payload(decode_plugin_error(
                instance.error_format,
                &text,
            )))
        }
        STATUS_UNSUPPORTED => {
            let removed = instance.dispatches.lock().unwrap().remove(&dispatch_id);
            drop(removed);
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

/// 给 `ctx_call_iface` 的 future 套上出站看门狗。
///
/// 只看“提供者结算”，不改变内联/异步两条路径：超时后 future 被丢弃，调用方
/// 收到一条可读错误，后续由 `complete_or_defer` 的 `on_done` 撤销派发记录。
fn with_iface_timeout(
    future: BoxFuture<Result<Config>>,
    service: String,
    method: String,
    policy: DispatchPolicy,
) -> BoxFuture<Result<Config>> {
    let Some(ms) = policy.outbound_timeout_ms else {
        return future;
    };
    Box::pin(async move {
        match tokio::time::timeout(std::time::Duration::from_millis(ms), future).await {
            Ok(result) => result,
            Err(_) => Err(CallFailure::new(
                format!("HOST_TIMEOUT: iface {service}.{method} 调用超过 {ms}ms 未结算"),
                "HOST_TIMEOUT",
                "timeout",
                "unknown",
            )
            .into()),
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

// ------------------------------------------------------------ 输出缓冲工具

/// # Safety
/// `out` 必须指向一个有效的宿主 `Out`。
unsafe fn reply(out: *mut Out, bytes: &[u8]) -> i32 {
    if out.is_null() {
        return STATUS_ERR;
    }
    (*out).write(bytes)
}

/// # Safety
/// 同 `reply`。
unsafe fn fail(out: *mut Out, message: &str) -> i32 {
    if !out.is_null() {
        let text = truncate_utf8(message, (*out).cap);
        reply(out, text.as_bytes());
    }
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

/// 写一条宿主→插件的 `STATUS_ERR`：插件声明了结构化能力时写
/// [`cordis_abi::ErrorPayload`]；放不下就退回纯文本，至少保留消息。
///
/// # Safety
/// 同 [`reply`]。
unsafe fn fail_to(
    out: *mut Out,
    error_format: u32,
    message: &str,
    code: &str,
    category: &str,
) -> i32 {
    let payload = ErrorPayload::new(message)
        .with_code(code)
        .with_category(category);
    fail_payload(out, error_format, &payload)
}

/// 与 [`fail_to`] 相同，但完整保留载荷（含 `certainty` / `detail`）。
///
/// 容量拒绝（`host_capacity`）必须让插件看到 `certainty = not_executed`：
/// “插件代码没有执行”是调用方可以据此决定是否重试的事实，不能被压成 unknown。
/// 对 credit 拒绝（[`reserve_credit`]）而言，`not_executed` 指的是**这块数据未入队**
/// ——插件代码当前正在执行；它不表示"插件未执行"。
unsafe fn fail_payload(out: *mut Out, error_format: u32, payload: &ErrorPayload) -> i32 {
    if error_format >= cordis_abi::ERROR_FORMAT_V1 {
        let text = payload.to_json();
        if reply(out, text.as_bytes()) == STATUS_OK {
            return STATUS_ERR;
        }
    }
    fail(out, &payload.error)
}

/// 插件→宿主：只有插件声明了能力位时才尝试解析；解析失败保留原文。
/// 未知码原样保留，绝不解释成成功。
fn decode_plugin_error(error_format: u32, text: &str) -> cordis_abi::ErrorPayload {
    if error_format >= cordis_abi::ERROR_FORMAT_V1 {
        if let Some(payload) = cordis_abi::ErrorPayload::from_json(text) {
            return payload;
        }
    }
    cordis_abi::ErrorPayload::new(text)
}

/// 异步通道内部携带的结构化失败。它实现了 `std::error::Error`，因此可以穿过
/// 内核的 `Box<dyn Error>`，在 `host_ctx_*` 的交付闭包里再 downcast 回载荷。
struct CallFailure {
    payload: ErrorPayload,
    /// 容量拒绝时把 waterfall 的剩余链保留下来，交给事件派发继续执行。
    /// `Mutex` 让 `CallFailure` 仍满足内核错误的 `Send + Sync` 约束。
    frame: Mutex<Option<cordis_core::Next>>,
}

impl std::fmt::Debug for CallFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CallFailure")
            .field("payload", &self.payload)
            .field(
                "has_frame",
                &self
                    .frame
                    .lock()
                    .map(|frame| frame.is_some())
                    .unwrap_or(false),
            )
            .finish()
    }
}

impl CallFailure {
    fn new(
        message: impl Into<String>,
        code: impl Into<String>,
        category: impl Into<String>,
        certainty: impl Into<String>,
    ) -> Self {
        Self {
            frame: Mutex::new(None),
            payload: ErrorPayload::new(message)
                .with_code(code)
                .with_category(category)
                .with_certainty(certainty),
        }
    }

    fn from_payload(payload: ErrorPayload) -> Self {
        Self {
            payload,
            frame: Mutex::new(None),
        }
    }

    fn with_frame(mut self, frame: Option<cordis_core::Next>) -> Self {
        *self
            .frame
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = frame;
        self
    }
}

impl std::fmt::Display for CallFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.payload.error)
    }
}

impl std::error::Error for CallFailure {}

fn payload_of(err: &(dyn std::error::Error + 'static)) -> ErrorPayload {
    err.downcast_ref::<CallFailure>()
        .map(|failure| failure.payload.clone())
        .unwrap_or_else(|| ErrorPayload::new(err.to_string()))
}

/// 异步回调 `args[0]`：旧的 `error` 字符串始终保留；能力位为 1 时追加结构化兄弟字段。
fn error_arg(error_format: u32, payload: &ErrorPayload) -> Value {
    let mut map = serde_json::Map::new();
    map.insert("error".into(), json!(payload.error));
    if error_format >= ERROR_FORMAT_V1 {
        map.insert("code".into(), json!(payload.code));
        map.insert("category".into(), json!(payload.category));
        map.insert("certainty".into(), json!(payload.certainty));
        map.insert("retry".into(), json!(payload.retry));
        map.insert("detail".into(), payload.detail.clone());
    }
    Value::Object(map)
}

/// 统一处理"取实例 → 执行 → 回写"。
///
/// 实例是**克隆出来**的再执行，所以执行期间不持锁：回调可能重入宿主，
/// 持锁执行插件代码是死锁的经典配方。
/// 句柄被回收/代际不符时给出可执行的诊断，而不是静默失败。
///
/// # panic 兜底（M5-1）
/// 所有宿主函数**都是 `extern "C"`**，而 Rust 的 "C" ABI 不可展开：panic 一旦
/// 从这里逃出去，进程直接 abort，插件连一条可读错误都拿不到。精确路径各有明确处置
/// （比如 effect 的状态检查走 `Fiber::try_effect`）；这里接住剩下的一切——
/// 状态机边角、中毒的锁、内部断言——把它降级成"这次调用失败"。
unsafe fn with_instance<T>(
    handle: u64,
    out: *mut Out,
    f: impl FnOnce(&Instance) -> std::result::Result<T, String>,
) -> i32
where
    T: AsRef<[u8]>,
{
    with_instance_as(handle, out, "host_call", "protocol", f)
}

/// 与 [`with_instance`] 相同，但允许调用点给出更具体的错误 `code` / `category`。
#[allow(clippy::result_large_err)]
unsafe fn with_instance_as<T>(
    handle: u64,
    out: *mut Out,
    code: &str,
    category: &str,
    f: impl FnOnce(&Instance) -> std::result::Result<T, String>,
) -> i32
where
    T: AsRef<[u8]>,
{
    with_instance_payload(handle, out, |instance| {
        f(instance).map_err(|message| {
            ErrorPayload::new(message)
                .with_code(code)
                .with_category(category)
        })
    })
}

/// [`with_instance_as`] 的载荷版：调用点可以直接给出完整的 [`ErrorPayload`]。
///
/// 仅用于需要保留 `certainty` / `detail` 的拒绝路径（容量准入）。
#[allow(clippy::result_large_err)]
unsafe fn with_instance_payload<T>(
    handle: u64,
    out: *mut Out,
    f: impl FnOnce(&Instance) -> std::result::Result<T, ErrorPayload>,
) -> i32
where
    T: AsRef<[u8]>,
{
    let instance = instances().lock().unwrap().get(handle);
    match instance {
        None => fail(
            out,
            &format!("ctx 句柄 {handle} 已失效（插件已卸载，或句柄来自上一代实例）"),
        ),
        Some(instance) => {
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&instance))) {
                Ok(Ok(bytes)) => reply(out, bytes.as_ref()),
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

/// 把 `Vec<Dyn>` 转成 JSON：只有 `ExternValue` 是 ABI 可见的，其余记为 `null`。
fn args_to_json(args: &[Dyn]) -> Value {
    Value::Array(
        args.iter()
            .map(|arg| {
                arg.downcast::<ExternValue>()
                    .map(|value| value.0.clone())
                    .unwrap_or(Value::Null)
            })
            .collect(),
    )
}

fn json_to_args(value: &Value) -> Vec<Dyn> {
    value
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|item| Dyn::new(ExternValue(item)))
        .collect()
}

/// 一次 op 调用的**原始结局**（M4-5 起需要把"插件不支持这个 op"单独认出来）。
///
/// 为什么不能压成一句错误消息：`UNSUPPORTED` 是**正常的前向兼容信号**
/// （旧插件遇到新 op 必须原样返回它），而"插件报错"是真的失败——
/// 前者要降级、后者要吵，混在一起就没法处置了。
enum CallOutcome {
    Ok(Value),
    Err(cordis_abi::ErrorPayload),
    Unsupported,
}

/// 调一次插件的 op，返回 `(status, out 文本)`：缓冲区不够时**扩容重试一次**。
///
/// 这是跨边界调用的唯一底层形态；`call_op`（判语义）与 `call_inbound`
/// （判挂起）都在它上面分层。
unsafe fn invoke_raw(call: CallFn, handle: u64, op: u32, payload: &str) -> (i32, String) {
    // 默认不重试：OP_CALLBACK/OP_APPLY 等写操作在回写结果前已经执行。
    invoke_raw_with(call, handle, op, payload, false)
}

/// 只读/幂等 op 的原始调用：允许 `STATUS_TOO_SMALL` 扩容重试。
///
/// **只给 `OP_VOLATILE_PATHS` 用**：它的输出是插件自述的元数据，不是业务载荷。
/// 将来若要给别的只读 op 也开这条路，先确认它同样不受"业务值可以任意大"的约束。
unsafe fn invoke_raw_read(call: CallFn, handle: u64, op: u32, payload: &str) -> (i32, String) {
    debug_assert_eq!(
        op,
        cordis_abi::OP_VOLATILE_PATHS,
        "只读扩容重试只服务于插件自述元数据；业务只读 op 不得静默继承这条路径"
    );
    invoke_raw_with(call, handle, op, payload, true)
}

/// 同步 ABI 调用的线程本地缓冲。
///
/// 16 KiB 是调用协议的初始输出上限；高频调用不应每次向 allocator 要一块。
/// 只回收标准大小，`STATUS_TOO_SMALL` 扩容后的大缓冲直接释放，避免池被
/// 异常大响应永久撑大。
struct CallBuffer(Vec<u8>);

thread_local! {
    static CALL_BUFFER_POOL: std::cell::RefCell<Vec<Vec<u8>>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

impl CallBuffer {
    fn take() -> Self {
        let mut buffer = CALL_BUFFER_POOL
            .with(|pool| pool.borrow_mut().pop())
            .unwrap_or_else(|| vec![0u8; 16 * 1024]);
        // `Drop` 会 clear 池里的 buffer；取出时必须恢复可写的标准长度。
        buffer.resize(16 * 1024, 0);
        Self(buffer)
    }

    fn try_with_capacity(cap: usize) -> std::result::Result<Self, String> {
        if cap == 16 * 1024 {
            return Ok(Self::take());
        }
        // 长度来自插件回填的 `usize`：分配必须**可失败**，不能靠 `vec![0u8; n]`
        // 在 OOM 时 abort。
        let mut bytes: Vec<u8> = Vec::new();
        bytes
            .try_reserve_exact(cap)
            .map_err(|_| format!("宿主无法为 {cap} 字节的插件输出分配缓冲（分配器拒绝）"))?;
        bytes.resize(cap, 0);
        Ok(Self(bytes))
    }

    fn as_mut_ptr(&mut self) -> *mut u8 {
        self.0.as_mut_ptr()
    }

    fn len(&self) -> usize {
        self.0.len()
    }

    fn text(&self, len: usize) -> String {
        String::from_utf8_lossy(&self.0[..len]).into_owned()
    }
}

impl Drop for CallBuffer {
    fn drop(&mut self) {
        let mut buffer = std::mem::take(&mut self.0);
        if buffer.capacity() == 16 * 1024 {
            // 插件可以在 out.len 之外读取到 out.cap；池化缓冲必须清零，
            // 否则同线程的下一个插件会看到上一次调用的残留数据。
            buffer.resize(16 * 1024, 0);
            buffer.fill(0);
            CALL_BUFFER_POOL.with(|pool| pool.borrow_mut().push(buffer));
        }
    }
}

/// 只读 op 扩容重试的输出上限（1 MiB）。
///
/// 与 `cordis_abi::MAX_CONFIG_SCHEMA_BYTES` 同口径：只读 op（`OP_VOLATILE_PATHS`
/// 之类）的输出是**声明**，不是业务载荷——声明大到 1 MiB 以上已经不是声明，
/// 而是坏插件把宿主回填的 `usize` 当分配指令。上限之外的申报一律拒绝，
/// 且拒绝发生在分配之前。
const MAX_READ_RETRY_BYTES: usize = 1024 * 1024;

/// 只读扩容重试要分配的缓冲大小：先拒越界与超限，再给出长度。
///
/// `needed` 来自插件回填的 `out.len`，**不可信**：`vec![0u8; n]` 在
/// `n > isize::MAX` 时 panic，真实 OOM 时 abort——两种都不该由插件的一行数字触发。
fn read_retry_cap(needed: usize) -> std::result::Result<usize, String> {
    if needed > isize::MAX as usize {
        return Err(format!(
            "插件声明输出需要 {needed} 字节，超过地址空间上限（isize::MAX）；拒绝分配"
        ));
    }
    if needed > MAX_READ_RETRY_BYTES {
        return Err(format!(
            "插件声明输出需要 {needed} 字节，超过宿主只读扩容上限 {MAX_READ_RETRY_BYTES} 字节；拒绝分配"
        ));
    }
    Ok(needed.max(16 * 1024))
}

unsafe fn invoke_raw_with(
    call: CallFn,
    handle: u64,
    op: u32,
    payload: &str,
    retry: bool,
) -> (i32, String) {
    let mut buffer = CallBuffer::take();
    let mut out = Out {
        ptr: buffer.as_mut_ptr(),
        cap: buffer.len(),
        len: 0,
    };
    let status = call(
        handle,
        op,
        Blob::new(payload.as_bytes()),
        Blob::EMPTY,
        &mut out,
    );
    if status == STATUS_TOO_SMALL {
        if !retry {
            return (
                status,
                format!(
                    "插件回调输出需要 {} 字节，超过 16 KiB 初始缓冲；该 op 不可重试（重放会重复执行副作用）",
                    out.len
                ),
            );
        }
        buffer = match read_retry_cap(out.len) {
            Ok(cap) => match CallBuffer::try_with_capacity(cap) {
                Ok(buffer) => buffer,
                Err(message) => return (STATUS_TOO_SMALL, message),
            },
            Err(message) => return (STATUS_TOO_SMALL, message),
        };
        out = Out {
            ptr: buffer.as_mut_ptr(),
            cap: buffer.len(),
            len: 0,
        };
        let retried = call(
            handle,
            op,
            Blob::new(payload.as_bytes()),
            Blob::EMPTY,
            &mut out,
        );
        if retried == STATUS_TOO_SMALL {
            // 第二次仍然不够：插件这一轮往往一个字节都没写，缓冲则是刚清零的；
            // 按 `out.len` 读出来就是一串 NUL。给一条说清"扩过容了"的诊断。
            return (
                retried,
                format!(
                    "插件回调在扩容到 {} 字节后仍然报输出不足（声称需要 {} 字节）；不再扩大缓冲",
                    buffer.len(),
                    out.len
                ),
            );
        }
        if out.len > buffer.len() {
            return (
                retried,
                format!(
                    "插件回调输出声称 {} 字节，超过宿主缓冲 {} 字节；消息未完整写入",
                    out.len,
                    buffer.len()
                ),
            );
        }
        return (retried, buffer.text(out.len));
    }
    if out.len > buffer.len() {
        // `Out` 契约要求插件只在 cap 内回填；超限时原消息一个字节都没写，
        // 不能把零缓冲当成错误文本。返回一条有界的宿主诊断。
        return (
            status,
            format!(
                "插件回调输出声称 {} 字节，超过宿主缓冲 {} 字节；消息未完整写入",
                out.len,
                buffer.len()
            ),
        );
    }
    (status, buffer.text(out.len))
}

/// 同步调用插件的回调入口（**带状态码**的版本）。
///
/// 返回值的三态：`Ok(ret)` 正常（`ret` 可能是 `null`）、`Err(message)` 插件报错、
/// `Unsupported` 插件不认识这个 op。把"插件报错"、"不支持"与"协议错误"分开，
/// 是为了让调用方按**语义**处置，而不是压成一句话。
unsafe fn call_op(
    call: CallFn,
    handle: u64,
    op: u32,
    payload: &str,
    error_format: u32,
) -> std::result::Result<CallOutcome, String> {
    call_op_with(call, handle, op, payload, false, error_format)
}

/// 只读 op（`OP_VOLATILE_PATHS`）专用：允许 TOO_SMALL 重试。
///
/// op 由 [`invoke_raw_read`] 断言；这里再钉一次，是为了让"业务只读 op 静默继承
/// 1 MiB 上限"这件事在调用点就现形，而不是等某条大响应被判成协议错误。
unsafe fn call_op_read(
    call: CallFn,
    handle: u64,
    op: u32,
    payload: &str,
    error_format: u32,
) -> std::result::Result<CallOutcome, String> {
    debug_assert_eq!(
        op,
        cordis_abi::OP_VOLATILE_PATHS,
        "只读扩容重试只服务于插件自述元数据；业务只读 op 不得静默继承这条路径"
    );
    call_op_with(call, handle, op, payload, true, error_format)
}

unsafe fn call_op_with(
    call: CallFn,
    handle: u64,
    op: u32,
    payload: &str,
    retry: bool,
    error_format: u32,
) -> std::result::Result<CallOutcome, String> {
    let (status, text) = if retry {
        invoke_raw_read(call, handle, op, payload)
    } else {
        invoke_raw(call, handle, op, payload)
    };
    match status {
        STATUS_OK => {
            // **空正文字面合法的"成功没有返回值"**：`OP_DISPOSE`/`OP_VOLATILE_APPLY`
            // 这类通知型 op 就是这么答的（插件写 0 字节 + OK）。把它当成解析失败
            // 会把成功说成失败——M4-5 的推送第一次跑就撞上了这一条。
            if text.is_empty() {
                return Ok(CallOutcome::Ok(Value::Null));
            }
            let parsed: Value = serde_json::from_str(&text).map_err(|err| err.to_string())?;
            Ok(CallOutcome::Ok(
                parsed.get("ret").cloned().unwrap_or(Value::Null),
            ))
        }
        STATUS_ERR => Ok(CallOutcome::Err(decode_plugin_error(error_format, &text))),
        STATUS_UNSUPPORTED => Ok(CallOutcome::Unsupported),
        STATUS_TOO_SMALL => Err(text),
        other => Err(format!("插件回调返回了意料之外的状态码 {other}")),
    }
}

/// 同步调用插件的回调入口（两态包装：`UNSUPPORTED` 也当成一条错误消息）。
unsafe fn call_plugin(
    call: CallFn,
    handle: u64,
    op: u32,
    payload: &str,
    error_format: u32,
) -> std::result::Result<Value, String> {
    call_plugin_detailed(call, handle, op, payload, error_format).map_err(|payload| payload.error)
}

/// 同步调用插件的回调入口（保留完整结构化错误）。
///
/// 旧 [`call_plugin`] 只返回 `.error`；需要 `code/category/certainty/retry/detail`
/// 的调用者用这个入口。协议错误也会被包成 `ErrorPayload`（`plugin_protocol`）。
///
/// # Safety
/// `call` 必须是仍然有效的插件入口；`handle` 必须属于该插件实例。
#[allow(clippy::result_large_err)]
pub unsafe fn call_plugin_detailed(
    call: CallFn,
    handle: u64,
    op: u32,
    payload: &str,
    error_format: u32,
) -> std::result::Result<Value, cordis_abi::ErrorPayload> {
    match call_op(call, handle, op, payload, error_format) {
        Ok(CallOutcome::Ok(value)) => Ok(value),
        Ok(CallOutcome::Err(payload)) => Err(payload),
        Ok(CallOutcome::Unsupported) => Err(cordis_abi::ErrorPayload::new(format!(
            "插件不支持 op {op}（它可能是为更老的宿主编译的）"
        ))
        .with_code("plugin_unsupported")
        .with_category("protocol")),
        Err(message) => Err(cordis_abi::ErrorPayload::new(message)
            .with_code("plugin_protocol")
            .with_category("protocol")),
    }
}

/// 只向**仍然存活**的实例投递回调：短锁准入、取得调用 lease，锁外执行。
///
/// waterfall next 和取消交付经此准入；普通出站完成由 `deliver_outbound`
/// 将摘表与准入合并。直接使用快照里的裸 `CallFn` 会打进已释放的库。
unsafe fn call_live(handle: u64, op: u32, payload: &str) -> std::result::Result<Value, String> {
    let instance =
        instances().lock().unwrap().get(handle).ok_or_else(|| {
            format!("ctx 句柄 {handle} 已失效（插件已卸载，或句柄来自上一代实例）")
        })?;
    let _lifecycle = admit_call(&instance)
        .ok_or_else(|| format!("ctx 句柄 {handle} 已失效（插件正在卸载或库已释放）"))?;
    call_plugin(instance.call, handle, op, payload, instance.error_format)
}

// ------------------------------------------------------------ 宿主函数实现

unsafe extern "C" fn host_ctx_get(handle: u64, name: Blob, out: *mut Out) -> i32 {
    let name = name.as_str().to_string();
    // 统一解析（M1）：实例句柄与派生句柄同路——派生 ctx 读的是隔离域
    //（fiberless 的宽容读走内核的 unchecked root 语义，见 Store::resolve）。
    with_resolved(handle, out, |resolved| match resolved.ctx.get(&name) {
        None => Ok(json!({ "missing": true }).to_string()),
        Some(value) => encode_service(&name, value),
    })
}

/// 读一个**必需**服务（M2-1）：注入检查 + `committed` 快照，等价原生 `Ctx::require`。
///
/// 与 [`host_ctx_get`] 的两条差异：
/// 1. **要求声明 inject**：未声明就报 `cannot get property "…" without inject`——
///    宽容读把"没声明依赖"和"服务不存在"混成同一个 `missing`，那是最难查的一类静默错误；
/// 2. **读 committed 快照**：组件在**自己的拆除过程**中仍能读到正在离开的依赖，
///    而宽容读在提供者进入 UNLOADING 之后只能返回 `missing`。
unsafe extern "C" fn host_ctx_require(handle: u64, name: Blob, out: *mut Out) -> i32 {
    let name = name.as_str().to_string();
    with_resolved_as(
        handle,
        out,
        "host_require",
        "resolve",
        |resolved| match resolved.ctx.require(&name) {
            Err(err) => Err(err.to_string()),
            Ok(value) => encode_service(&name, value),
        },
    )
}

/// 把一个服务值编码成 ABI 可见的 JSON。
///
/// `ctx_get` 与 `ctx_require` 共用它：**读取语义可以不同（宽容 vs 严格），
/// 但"什么对 ABI 可见"必须只有一条契约**——否则同一个服务会随读取入口
/// 时而可读、时而不可读。
///
/// M6：判据换成 [`Dyn::adapter`]——于是 `ctx.provide_iface`（T2 提供者）与
/// `#[cordis::service]` 的 typed 服务对 ABI 同样可见（后者也带 JSON 面），
/// 而且**能发现**（拿到方法清单与版本）就能**调用**（`ctx_call_iface` 走同一条路）。
fn encode_service(name: &str, value: Dyn) -> std::result::Result<String, String> {
    match value.downcast::<ExternValue>() {
        Some(exposed) => Ok(json!({ "value": exposed.0 }).to_string()),
        None => match value.adapter() {
            // 接口也能读：告知调用方"这是个带方法的服务"，并给出方法与版本。
            Some(iface) => Ok(json!({
                "value": { "iface": iface.service(), "version": iface.version(), "methods": iface.methods() }
            })
            .to_string()),
            None => Err(format!(
                "service \"{name}\" 对 ABI 不可见：提供方需要用 ExternValue（纯数据）\
                 或 ctx.provide_iface（带方法的服务）来注册"
            )),
        },
    }
}

/// 覆盖**自己提供的**服务值（M2-2）：对应 JS 的 `ctx.set(name, value)`。
///
/// 两条硬约束由内核的 `Store::set` 强制（与上游 `reflect.ts:164-175` 逐字一致）：
/// 没 provide 过 → `without provide`；实现者不是当前 fiber → `in multiple fibers`。
/// 换值**不通知依赖者**：依赖者的 epoch 只关心实现者身份，而 `set` 不改变身份。
unsafe extern "C" fn host_ctx_set(handle: u64, name: Blob, value_json: Blob, out: *mut Out) -> i32 {
    let name = name.as_str().to_string();
    let parsed: Value = match serde_json::from_slice(value_json.as_bytes()) {
        Ok(value) => value,
        Err(err) => return fail(out, &format!("set 的载荷不是合法 JSON: {err}")),
    };
    with_instance(handle, out, |instance| {
        instance
            .ctx
            .set(&name, Dyn::new(ExternValue(parsed)))
            .map(|_| String::new())
            .map_err(|err| err.to_string())
    })
}

// ------------------------------------------------------------ ABI v2（M1）：五个尾追加能力

/// #12 `ctx_isolate`：派生 realm 子 context。
///
/// 子 context 继承调用方实例身份（回调 id 仍走原实例派发），但 `key` 进独立
/// 服务域。`label` 语义与 loader 的 `RowScopes` 一致：空 = 行私有域（`alloc`），
/// 非空 = 具名共享域（`named`）。
///
/// 返回 `{"handle": <u64>}`——bit63=1 的派生句柄。派生 ctx **没有 fiber**
/// （G18：子 context 带 fiber 是 M4 的事），所以 M1 的可用面是
/// `ctx_get`/`ctx_require` 的隔离域读；`provide`/`on` 请经 `ctx_use` 挂子组件。
unsafe extern "C" fn host_ctx_isolate(handle: u64, key: Blob, label: Blob, out: *mut Out) -> i32 {
    let key = key.as_str().to_string();
    let label = label.as_str().to_string();
    with_resolved(handle, out, |resolved| {
        let realm = (!label.is_empty()).then(|| resolved.ctx.store.named_realm(&key, &label));
        let derived = resolved.ctx.isolate(&key, realm);
        let handle = register_derived(resolved.owner, derived, resolved.instance.clone(), None);
        Ok(json!({ "handle": handle }).to_string())
    })
}

/// #13 `ctx_intercept`：派生 intercept 子 context。
///
/// M1 只把 metadata 存进派生 ctx 的 intercept 链（祖先优先、后代覆盖），
/// 子 context（isolate/intercept/`ctx_use` 挂的子组件）经内核的
/// `resolve_intercept` 读到它；真正的消费点在 M3。
unsafe extern "C" fn host_ctx_intercept(
    handle: u64,
    key: Blob,
    metadata: Blob,
    out: *mut Out,
) -> i32 {
    let key = key.as_str().to_string();
    let metadata: Value = match serde_json::from_slice(metadata.as_bytes()) {
        Ok(value) => value,
        Err(err) => return fail(out, &format!("intercept 的 metadata 不是合法 JSON: {err}")),
    };
    with_resolved(handle, out, |resolved| {
        let derived = resolved.ctx.intercept(&key, metadata);
        let handle = register_derived(resolved.owner, derived, resolved.instance.clone(), None);
        Ok(json!({ "handle": handle }).to_string())
    })
}

/// #14 `ctx_use`：挂一个子组件。
///
/// `url = "@native:<name>"` 走 [`NativeRegistry`]，其余交给进程级
/// [`PluginSource`]（[`set_plugin_source`]）。新 fiber 是**调用方 fiber 的
/// 一个 effect**（内核 `Fiber::new` 负责）：父 dispose ⇒ 子 dispose，
/// 宿主不需要为父子关系记账。返回子 fiber 的派生句柄。
unsafe extern "C" fn host_ctx_use(handle: u64, url: Blob, config: Blob, out: *mut Out) -> i32 {
    let url = url.as_str().to_string();
    let config: Value = match serde_json::from_slice(config.as_bytes()) {
        Ok(value) => value,
        Err(err) => return fail(out, &format!("use 的 config 不是合法 JSON: {err}")),
    };
    with_resolved(handle, out, |resolved| {
        let plugin = resolve_component(&url)?;
        // 注意挂在**解析出的 ctx** 上：经派生句柄 use ⇒ 子组件落在隔离域内。
        let fiber = mount(&resolved.ctx, plugin, config, None);
        let handle = register_derived(
            resolved.owner,
            fiber.ctx(),
            resolved.instance.clone(),
            Some(Arc::downgrade(&fiber)),
        );
        Ok(json!({ "handle": handle }).to_string())
    })
}

/// #15 `ctx_dispose`：回收一个派生 ctx 句柄或子 fiber 句柄（`target = {"handle": …}`）。
///
/// 子 fiber 条目会真的 dispose 那个 fiber（loader 行因此走既有的自我卸载
/// 识别链）；isolate/intercept 条目只是撤销句柄（域不回收，与上游一致）。
unsafe extern "C" fn host_ctx_dispose(handle: u64, target: Blob, out: *mut Out) -> i32 {
    let parsed: Value = match serde_json::from_slice(target.as_bytes()) {
        Ok(value) => value,
        Err(err) => return fail(out, &format!("dispose 的 target 不是合法 JSON: {err}")),
    };
    let Some(raw) = parsed.get("handle").and_then(Value::as_u64) else {
        return fail(
            out,
            "dispose 的 target 需要 {\"handle\": <u64>} 形式的 JSON",
        );
    };
    if raw & DERIVED_HANDLE_TAG == 0 {
        return fail(
            out,
            "ctx_dispose 只回收派生句柄（bit63=1）；卸载自己请用 ctx_dispose_self",
        );
    }
    with_resolved(handle, out, move |resolved| {
        // 调用者只能回收**自己根实例**的派生句柄；派生句柄的 seq 可枚举，
        // 不加归属检查就能销毁别的插件的子 fiber。
        let mut table = derived_table().lock().unwrap();
        let entry = table
            .get(&raw)
            .ok_or_else(|| format!("派生句柄 {raw} 已失效（已回收，或根实例已注销排水）"))?;
        if entry.owner != resolved.owner {
            return Err(format!(
                "派生句柄 {raw} 不属于调用方根实例 {handle}，拒绝回收"
            ));
        }
        let live_fiber = entry.fiber.as_ref().and_then(Weak::upgrade);
        // 先在摘表前确认运行时：否则 remove 之后才发现无法 dispose，句柄就丢了。
        let runtime = if live_fiber.is_some() {
            match tokio::runtime::Handle::try_current() {
                Ok(runtime) => Some(runtime),
                Err(_) => {
                    // upgrade 可能取得最后强引用，失败分支也必须先放锁再析构。
                    drop(table);
                    drop(live_fiber);
                    return Err("子 fiber 的卸载需要异步运行时（当前在同步上下文里）".into());
                }
            }
        } else {
            None
        };
        #[cfg(test)]
        derived_dispose_tests::before_remove(raw);
        let entry = table.remove(&raw).expect("validated under the same lock");
        drop(table);
        if let (Some(fiber), Some(runtime)) = (live_fiber, runtime) {
            runtime.spawn(async move { fiber.dispose().await });
        }
        drop(entry);
        Ok(String::new())
    })
}

/// #16 `ctx_dispose_self`：卸载调用者自己。
///
/// 拆的是调用插件自己的 fiber；loader 行的处置走既有机制——fiber dispose →
/// `internal/plugin` 观察者 → `mark_self_disposed`（摘装载表 + 树侧
/// `disabled: true` + 落盘）。**绝不静默**：不在 loader 行上的插件同样真拆
/// （fiber 及其全部 effect），只是没有行可标。
unsafe extern "C" fn host_ctx_dispose_self(handle: u64, out: *mut Out) -> i32 {
    with_resolved(handle, out, |resolved| {
        // 用**实例身份**的 fiber：经派生句柄自卸载，拆的也是拥有它的插件。
        let fiber = resolved
            .instance
            .ctx
            .fiber_opt()
            .ok_or("调用方实例没有 fiber，无法自我卸载")?;
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn(async move { fiber.dispose().await });
                Ok(String::new())
            }
            Err(_) => Err("ctx_dispose_self 需要异步运行时（当前在同步上下文里）".into()),
        }
    })
}

/// `ctx_use` 的 url 解析：`@native:<name>` 与 `PluginSource` 两条路。
fn resolve_component(url: &str) -> std::result::Result<Arc<dyn Plugin>, String> {
    if let Some(name) = url.strip_prefix("@native:") {
        return native_registry()
            .and_then(|registry| registry.native(name))
            .ok_or_else(|| format!("@native:{name}：宿主没有注册这个原生组件（NativeRegistry）"));
    }
    let entry = patch::Entry {
        name: url.to_string(),
        ..patch::Entry::default()
    };
    let loaded = plugin_source()
        .load(&entry, &[], next_use_revision())
        .map_err(|err| format!("use \"{url}\" 失败: {err}"))?;
    Ok(loaded.plugin)
}

/// `ctx_use` 交给 `PluginSource` 的修订号（暂存副本文件名要用，进程内单调）。
fn next_use_revision() -> u64 {
    static REVISION: AtomicU64 = AtomicU64::new(0);
    REVISION.fetch_add(1, Ordering::SeqCst) + 1
}

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

unsafe extern "C" fn host_ctx_provide(
    handle: u64,
    name: Blob,
    value_json: Blob,
    out: *mut Out,
) -> i32 {
    let name = name.as_str().to_string();
    let parsed: Value = match serde_json::from_slice(value_json.as_bytes()) {
        Ok(value) => value,
        Err(err) => return fail(out, &format!("provide 的载荷不是合法 JSON: {err}")),
    };
    with_instance(handle, out, |instance| {
        // 整个 provide 是调用者 fiber 的一个 effect：插件卸载时自动摘除，
        // 所以宿主这里不需要为它记账。
        instance
            .ctx
            .provide(&name, Dyn::new(ExternValue(parsed)))
            .map(|_| String::new())
            .map_err(|err| err.to_string())
    })
}

/// 一个由动态库插件提供的服务接口。
///
/// 它是内核 `ExternService` 的实现，但**完全不知道 ABI 细节**：
/// 方法调用被转成一次 `OP_CALLBACK`，带上是哪个服务、哪个方法、什么参数。
/// 于是"服务方法"成了契约，"函数指针"不是——这正是跨语言接口能稳定演进的原因。
struct PluginIface {
    service: String,
    version: u32,
    methods: Vec<String>,
    provider: Instance,
    /// 提供方的 ctx 句柄：回调时要用它把调用送回去。
    provider_handle: u64,
    /// 提供方注册的方法分发回调 id。
    dispatcher: u64,
}

impl ExternService for PluginIface {
    fn service(&self) -> &str {
        &self.service
    }

    fn version(&self) -> u32 {
        self.version
    }

    fn methods(&self) -> Vec<String> {
        self.methods.clone()
    }

    fn call(
        &self,
        caller_meta: Option<Config>,
        method: String,
        args: Config,
    ) -> BoxFuture<Result<Config>> {
        let provider = self.provider.clone();
        let provider_handle = self.provider_handle;
        let service = self.service.clone();
        let dispatcher = self.dispatcher;
        Box::pin(async move {
            let mut payload = json!({
                "kind": KIND_IFACE,
                "id": dispatcher,
                "event": service,
                "method": method,
                "args": [args],
            });
            // M3：调用点上的合并元数据 `d(k) ⊕ ι(k)`（右偏）作为**可选字段**下发。
            // 没有元数据时整个字段不出现——旧插件忽略未知字段，新插件读 `meta`。
            if let Some(meta) = caller_meta {
                payload["meta"] = meta;
            }
            // M2：接口方法可以挂起（返回 `STATUS_PENDING` 后由 `ctx_fulfill` 结算）；
            // M2b/M2c：接口调用不是 waterfall、不从调用方带令牌，但**父节点**照记
            //（它是被谁在回调里发起的，取消级联要追得到）。
            let parent = scope_top().map(|(_, id)| id);
            match unsafe {
                call_inbound(
                    &provider,
                    provider_handle,
                    OP_CALLBACK,
                    payload,
                    parent,
                    None,
                    None,
                    None,
                )
            } {
                Ok(Inbound::Ready(value)) => Ok(value),
                Ok(Inbound::Pending(pending)) => {
                    (*pending).settle("接口方法").await.map_err(Error::from)
                }
                Err(message) => Err(message.into()),
            }
        })
    }

    /// 流式调用（ADR 0001 §5）：把 `sink` 挂到这次派发的记录上，**不等结算**就返回。
    ///
    /// - 插件直接给了一件值 ⇒ 它就是**长度 1 的流**（与内核默认实现同义；
    ///   插件若已经自己推完并收尾，这件返回值被忽略）；
    /// - 插件返回 `STATUS_PENDING` ⇒ 就这么返回：块稍后经 `ctx_stream_push` 到达，
    ///   收尾 / 失败 / 消费者离场都会由观察者把记录摘掉；
    /// - 插件同步失败 ⇒ 流以失败收场，同时把错误交给调用方。
    fn call_stream(
        &self,
        caller_meta: Option<Config>,
        method: String,
        args: Config,
        sink: StreamSink,
    ) -> BoxFuture<Result<()>> {
        let provider = self.provider.clone();
        let provider_handle = self.provider_handle;
        let service = self.service.clone();
        let dispatcher = self.dispatcher;
        Box::pin(async move {
            let mut payload = json!({
                "kind": KIND_IFACE,
                "id": dispatcher,
                "event": service,
                "method": method,
                "args": [args],
            });
            // M3：调用点上的合并元数据 `d(k) ⊕ ι(k)`（右偏）作为**可选字段**下发。
            if let Some(meta) = caller_meta {
                payload["meta"] = meta;
            }
            let parent = scope_top().map(|(_, id)| id);
            let record_sink = sink.clone();
            match unsafe {
                call_inbound(
                    &provider,
                    provider_handle,
                    OP_CALLBACK,
                    payload,
                    parent,
                    None,
                    None,
                    Some(record_sink),
                )
            } {
                Ok(Inbound::Ready(value)) => {
                    if !sink.is_finished() {
                        sink.push(value);
                        sink.close();
                    }
                    Ok(())
                }
                // 流式挂起：记录归流观察者所有（流收尾时摘除）。若配了派发看门狗，
                // 派生一个 settle 任务执行超时策略；超时会 revoke 记录并以错误收流。
                Ok(Inbound::Pending(pending)) => {
                    if provider.dispatch_policy.timeout_ms.is_some()
                        && tokio::runtime::Handle::try_current().is_ok()
                    {
                        spawn_stream_settle(&provider, pending)
                            .map_err(|payload| Error::from(CallFailure::from_payload(payload)))?;
                    }
                    Ok(())
                }
                Err(message) => {
                    sink.fail(message.payload.error.clone());
                    Err(message.into())
                }
            }
        })
    }
}

#[allow(clippy::result_large_err)]
fn spawn_stream_settle(
    instance: &Instance,
    pending: Box<PendingContinuation>,
) -> std::result::Result<(), ErrorPayload> {
    let (cancel, exit) = {
        let table = instance.dispatches.lock().unwrap();
        if instance.closed.load(Ordering::SeqCst) {
            return Err(closed_admission_payload());
        }
        // 流可在 OP_CALLBACK 内正常收尾后仍返回 PENDING；此时无需看门狗。
        if !table.contains_key(&pending.dispatch_id) {
            return Ok(());
        }
        dispatch_task(instance, pending.dispatch_id)
    };
    let mut work = TaskWork {
        future: Box::pin(async move {
            let _ = (*pending).settle("流式调用").await;
        }),
        on_done: None::<fn(())>,
        _exit: exit,
    };
    tokio::runtime::Handle::current().spawn(async move {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {}
            _ = &mut work.future => {}
        }
        drop(work);
    });
    Ok(())
}

unsafe extern "C" fn host_ctx_provide_iface(
    handle: u64,
    name: Blob,
    version: u32,
    methods: Blob,
    dispatcher: u64,
    out: *mut Out,
) -> i32 {
    let service = name.as_str().to_string();
    let parsed: Value = match serde_json::from_slice(methods.as_bytes()) {
        Ok(value) => value,
        Err(err) => return fail(out, &format!("方法清单不是合法 JSON: {err}")),
    };
    let methods: Vec<String> = parsed
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|value| value.as_str().map(str::to_string))
        .collect();
    with_instance(handle, out, |instance| {
        let iface = PluginIface {
            service: service.clone(),
            version,
            methods,
            provider: instance.clone(),
            provider_handle: handle,
            dispatcher,
        };
        instance
            .ctx
            .provide_iface(&service, Arc::new(iface))
            .map(|_| String::new())
            .map_err(|err| err.to_string())
    })
}

#[allow(clippy::result_large_err)]
unsafe extern "C" fn host_ctx_call_iface(
    handle: u64,
    service: Blob,
    version: u32,
    method: Blob,
    args: Blob,
    continuation: u64,
    out: *mut Out,
) -> i32 {
    let service = service.as_str().to_string();
    let method = method.as_str().to_string();
    let parsed: Value = match serde_json::from_slice(args.as_bytes()) {
        Ok(value) => value,
        Err(err) => return fail(out, &format!("call_iface 的 args 不是合法 JSON: {err}")),
    };
    // M3：走 [`with_resolved_payload`] 而不是 [`with_instance`]——调用点上叠的
    // intercept 层挂在**派生 ctx** 上，用实例 ctx 调用等于把它丢掉（"use-site
    // 元数据"就无从谈起）。实例句柄照旧可用（派生表在实例表之后解析）；续延仍
    // 发回**根实例句柄**（派生句柄不是回调目标，与 `ctx_isolate` 的派生条目同一约定）。
    //
    // 用载荷版是为了让容量拒绝保留 `host_capacity` / `capacity` / `not_executed`。
    with_resolved_payload(handle, out, move |resolved| {
        let timeout_service = service.clone();
        let timeout_method = method.clone();
        let error_format = resolved.instance.error_format;
        let future = resolved.ctx.call_iface(&service, version, &method, parsed);
        let future = with_iface_timeout(
            future,
            timeout_service,
            timeout_method,
            resolved.instance.dispatch_policy,
        );
        let consumer = resolved.owner;
        // 把这次 iface 调用登记成消费者实例的一条在飞记录：
        // 消费者卸载时 `apply_via` 的排水会把它撤掉，避免延迟 on_done 打进已释放的库。
        // 容量拒绝时直接返回：future 还没有被 poll，提供者代码不会执行。
        let iface_id = next_dispatch_id();
        let parent = scope_top().map(|(_, id)| id);
        let (cancel, done) = insert_dispatch_task(
            &resolved.instance,
            iface_id,
            DispatchRecord {
                sink: None,
                frame: None,
                parent,
                outbound: Some(Outbound {
                    owner: consumer,
                    continuation,
                    kind: ContinuationKind::Iface,
                }),
                stream: None,
            },
        )
        .map_err(|(payload, _)| payload)?;
        let deliver: Arc<dyn Fn(Result<Config>) + Send + Sync> = Arc::new(move |result| {
            let payload = json!({
                "kind": KIND_IFACE,
                "id": continuation,
                "args": [match &result {
                    Ok(value) => json!({ "result": value }),
                    Err(err) => error_arg(error_format, &payload_of(err.as_ref())),
                }],
            })
            .to_string();
            deliver_outbound(consumer, iface_id, &payload);
        });
        let deferred = {
            let on_done = deliver.clone();
            let on_panic = deliver.clone();
            complete_or_defer(
                future,
                move |result| on_done(result),
                move |payload| on_panic(Err(Box::new(CallFailure::from_payload(payload)))),
                (cancel, done),
                PanicScope {
                    scene: "iface",
                    handle: consumer,
                    dispatch_id: iface_id,
                },
            )
        };
        match deferred {
            // 内联完成：把结果直接带回，插件侧自己消费掉它的续延（零额外跨边界往返）。
            Ok(Some(result)) => {
                unregister_dispatch_node(consumer, iface_id);
                Ok(match result {
                    Ok(value) => json!({ "pending": false, "result": value }).to_string(),
                    Err(err) => {
                        let mut response = json!({ "pending": false });
                        if let Value::Object(map) = &mut response {
                            if let Value::Object(error) =
                                error_arg(error_format, &payload_of(err.as_ref()))
                            {
                                map.extend(error);
                            }
                        }
                        response.to_string()
                    }
                })
            }
            // 未就绪：结果会通过 `OP_CALLBACK` 送到同一个续延 id 上。
            Ok(None) => Ok(json!({ "pending": true }).to_string()),
            Err(DeferFailure::NoRuntime(message)) => {
                unregister_dispatch_node(consumer, iface_id);
                Err(ErrorPayload::new(message)
                    .with_code("host_call")
                    .with_category("protocol"))
            }
            // 首 poll panic：调用方同步拿到错误 ⇒ **先摘表再返回**，
            // 否则关闭路径会为同一次调用补发一条 abort（双交付）。
            Err(DeferFailure::Panic(payload)) => {
                unregister_dispatch_node(consumer, iface_id);
                Err(payload)
            }
        }
    })
}

/// #17 `ctx_fulfill`（M2）：结算一个返回过 `STATUS_PENDING` 的入站续延。
///
/// 键是 `(实例, dispatch_id)`（D3）：同一 handler 的并发派发因此互不干扰；
/// 每 id **恰好结算一次**——第二次调用得到一条可读错误，绝不静默覆盖。
unsafe extern "C" fn host_ctx_fulfill(
    handle: u64,
    dispatch_id: u64,
    payload: Blob,
    out: *mut Out,
) -> i32 {
    let value: Value = if payload.len == 0 {
        Value::Null
    } else {
        match serde_json::from_slice(payload.as_bytes()) {
            Ok(value) => value,
            Err(err) => return fail(out, &format!("fulfill 的载荷不是合法 JSON: {err}")),
        }
    };
    with_instance_as(handle, out, "host_fulfill", "lifecycle", move |instance| {
        // 取出即结算（remove-then-settle）：与"每 id 恰好一次"是同一件事。
        // 但**只允许摘有 sink 的入站记录**：outbound 记录（ctx_dispatch 的
        // dispatch_id）没有 sink，误摘会把它的正常交付静默取消。
        let record = {
            let mut table = instance.dispatches.lock().unwrap();
            match table.get(&dispatch_id) {
                Some(record) if record.sink.is_some() => {}
                Some(_) => {
                    return Err(format!(
                        "dispatch {dispatch_id} 不是可 fulfill 的入站续延（它是出站派发记录）"
                    ))
                }
                None => {
                    return Err(format!(
                        "dispatch {dispatch_id} 不存在或已结算（每次挂起只结算一次）"
                    ))
                }
            }
            table.remove(&dispatch_id).expect("checked above")
        };
        // 流（ADR 0001 §5）：`fulfill` 就等于"补上最后一件并正常收尾"——
        // 于是"值路径"与"流路径"在提供者看来是同一件事（长度 1 的流的推广）。
        if let Some(sink) = &record.stream {
            if !sink.is_finished() {
                sink.push(value.clone());
                sink.close();
            }
        }
        if let Some(tx) = record.sink {
            let _ = tx.send(Ok(value));
        }
        Ok(String::new())
    })
}

/// #19 `ctx_abort`（M2c）：取消一次在飞派发——该派发**及其后代**全部以
/// 「已取消」结算（挂起续延收错误、出站续延收错误、waterfall 帧被丢）。
///
/// 父链来自派发记录的 `parent`（由宿主的**派发作用域**记录，插件不需要传）。
/// 级联在**同一实例**的记录表内递归（骨架范围；跨实例的级联见 §5 风险）。
unsafe extern "C" fn host_ctx_abort(handle: u64, dispatch_id: u64, out: *mut Out) -> i32 {
    with_instance_as(handle, out, "host_abort", "lifecycle", move |instance| {
        // 收集受害者（含自身）：先摘表，再结算——"remove-then-settle"。
        // 与取消/超时共用同一棵树的排水实现，保证级联语义只有一份。
        let payload = ErrorPayload::new("已取消：调用方撤销了这次派发")
            .with_code("host_cancel")
            .with_category("cancel")
            .with_certainty("unknown");
        drain_dispatch_tree(instance, dispatch_id, &payload)
            .map(|_| String::new())
            .ok_or_else(|| format!("dispatch {dispatch_id} 不存在或已结束（无可取消的在飞派发）"))
    })
}

/// 把一个失败原因送给**出站**续延（插件在等它的结果）。
///
/// ADR 0004 A4：原因通过 `args[0].error` 投递；能力位为 1 时带结构化兄弟字段。
/// `kind` 必须与发起时登记的类型一致——iface 调用方只认 `kind:"iface"`。
fn abort_payload(outbound: &Outbound, error_format: u32, error: &ErrorPayload) -> String {
    json!({
        "kind": outbound.kind.wire_name(),
        "id": outbound.continuation,
        "args": [error_arg(error_format, error)],
    })
    .to_string()
}

/// 向已认领的出站续延交付取消：即使 `closed` 已跨入，也必须先于 `OP_DISPOSE`。
/// 调用方必须持有 FFI 准入成功的 ActiveCall 或关闭 lease，并在锁外调用。
fn deliver_abort_direct(instance: &Instance, outbound: Outbound, error: &ErrorPayload) {
    let payload = abort_payload(&outbound, instance.error_format, error);
    if let Err(message) = unsafe {
        call_plugin(
            instance.call,
            outbound.owner,
            OP_CALLBACK,
            &payload,
            instance.error_format,
        )
    } {
        eprintln!("[cordis] 卸载期取消交付失败: {message}");
    }
}

/// #18 `ctx_next`（M2b）：续跑 waterfall 的**剩余链**，完成后以
/// `{kind:"next", id:on_done, args:[{"result": …}]}` 回调插件。
///
/// 剩余链先**同步试跑**（D6 的同步快速路径）：就地完成时就地投递 `on_done`
/// （于是"回调内 next、回调内结算"一次往返搞定）；未就绪则交给运行时。
/// 帧**取出即消费**：第二次 `ctx_next` 是可读错误。
#[allow(clippy::result_large_err)]
unsafe extern "C" fn host_ctx_next(
    handle: u64,
    dispatch_id: u64,
    args: Blob,
    on_done: u64,
    out: *mut Out,
) -> i32 {
    let parsed: Value = match serde_json::from_slice(args.as_bytes()) {
        Ok(value) => value,
        Err(err) => return fail(out, &format!("next 的 args 不是合法 JSON: {err}")),
    };
    let new_args = json_to_args(
        &parsed
            .get("args")
            .cloned()
            .unwrap_or_else(|| parsed.clone()),
    );
    with_instance_payload(handle, out, move |instance| {
        let next_error = |message| {
            ErrorPayload::new(message)
                .with_code("host_next")
                .with_category("lifecycle")
        };
        let error_format = instance.error_format;
        let frame = {
            let mut table = instance.dispatches.lock().unwrap();
            if instance.closed.load(Ordering::SeqCst) {
                return Err(closed_admission_payload());
            }
            match table.get_mut(&dispatch_id) {
                Some(record) => record
                    .frame
                    .take()
                    .map(|frame| (frame, dispatch_task(instance, dispatch_id))),
                None => None,
            }
        };
        let Some((next, (cancel, done))) = frame else {
            return Err(next_error(format!(
                "dispatch {dispatch_id} 没有可续跑的 waterfall 帧（不是 waterfall 派发，或已经 next 过）"
            )));
        };
        let future: BoxFuture<Ret> = Box::pin(next.call(new_args));
        // 剩余链的 future 打上作用域：它交付给插件的回调能记下父节点。
        let scoped: BoxFuture<Ret> = Box::pin(Scoped {
            inner: future,
            scope: (handle, dispatch_id),
        });
        let scoped = with_next_timeout(scoped, dispatch_id, instance.dispatch_policy);
        let deliver: Arc<dyn Fn(std::result::Result<Ret, ErrorPayload>) + Send + Sync> =
            Arc::new(move |result| {
                deliver_next(handle, dispatch_id, on_done, error_format, result);
            });
        let deferred = {
            let deliver = deliver.clone();
            let deliver_panic = deliver.clone();
            complete_or_defer(
                scoped,
                move |result| deliver(result),
                move |payload| deliver_panic(Err(payload)),
                (cancel, done),
                PanicScope {
                    scene: "next",
                    handle,
                    dispatch_id,
                },
            )
        };
        match deferred {
            Ok(Some(result)) => deliver(result),
            Ok(None) => {}
            Err(DeferFailure::NoRuntime(message)) => return Err(next_error(message)),
            // 首 poll panic：**不摘入站记录**（它由事件派发/取消路径收尾），只返回错误。
            Err(DeferFailure::Panic(payload)) => return Err(payload),
        }
        Ok(String::new())
    })
}

/// 把下游结果投递给插件的 `on_done` 续延（`kind:"next"`）。
fn deliver_next(
    handle: u64,
    dispatch_id: u64,
    on_done: u64,
    error_format: u32,
    result: std::result::Result<Ret, ErrorPayload>,
) {
    let args = match result {
        Ok(ret) => json!({ "result": ret_to_json(ret) }),
        Err(err) => error_arg(error_format, &err),
    };
    let payload = json!({
        "kind": KIND_NEXT,
        "id": on_done,
        "args": [args],
    })
    .to_string();
    // 交付期间让"这次派发"留在作用域里：插件在 on_done 里再发派发时父节点仍是它。
    let _scope = scope_push(handle, dispatch_id);
    // `call_live` 短锁登记后调用；插件可重入宿主（fulfill / 再 next）。
    if let Err(message) = unsafe { call_live(handle, OP_CALLBACK, &payload) } {
        eprintln!("[cordis] waterfall 的 on_done 交付失败: {message}");
    }
}

unsafe extern "C" fn host_ctx_on(handle: u64, event: Blob, cb: u64, out: *mut Out) -> i32 {
    let event = event.as_str().to_string();
    with_instance(handle, out, |instance| {
        let ctx = instance.ctx.clone();
        let plugin_handle = handle;
        let event_name = event.clone();
        // 只让**监听器闭包**持有库强引用：钩子随 fiber 卸载被摘除，
        // 快照派发结束后强引用释放，动态库就能正常 FreeLibrary。
        let keepalive = instance.keepalive.as_ref().and_then(|weak| weak.upgrade());
        // M2/M2b：宿主监听器在**调用点**向插件发起入站回调（先登记 dispatch_id、再调用）；
        // waterfall 派发同时把**剩余链**（`call.next`）登记成帧——插件可以在回调内部
        // 调 `ctx_next` 用它；不调就永远不跑下游（惰性，M2b）。
        ctx.on(&event, move |call_args| {
            // 即使这次派发晚于实例注销到达，只要快照还在，库就保持加载；
            // 句柄校验（call_inbound）会拒绝真正进入插件代码。
            let _keepalive = keepalive.clone();
            let call_ctx = call_args.ctx.clone();
            let waterfall = call_args.next.is_linked();
            // 父节点：正在被交付的那次出站派发（作用域栈顶；不在派发里时为 None）。
            let parent = scope_top().map(|(_, id)| id);
            let frame = if waterfall {
                Some(call_args.next)
            } else {
                None
            };
            let cancel = call_args.cancel.clone();
            let next_args = call_args.args.clone();
            let mut payload = json!({
                "kind": KIND_EVENT,
                "id": cb,
                "event": event_name,
                "args": args_to_json(&call_args.args),
            });
            if waterfall {
                payload["mode"] = json!("waterfall");
            }
            // **不捕获快照里的 Instance**：派发可能晚于插件注销到达。
            // 每次调用重新取表项；取不到就把它当作"没有意见"，绝不碰裸 CallFn。
            // 注意先把实例取到局部变量再调用：`match instances().lock()...` 的
            // 临时 guard 会活到整个 match 结束，`call_inbound` 内部再取同一把锁就死锁。
            let live = instances().lock().unwrap().get(plugin_handle);
            let result = match live {
                Some(instance) => unsafe {
                    call_inbound(
                        &instance,
                        plugin_handle,
                        OP_CALLBACK,
                        payload,
                        parent,
                        cancel,
                        frame,
                        None,
                    )
                },
                None => Err(CallFailure::new(
                    format!("ctx 句柄 {plugin_handle} 已失效（插件已卸载，或句柄来自上一代实例）"),
                    "host_lifecycle",
                    "lifecycle",
                    "unknown",
                )),
            };
            Box::pin(async move {
                let settled = match result {
                    Ok(Inbound::Ready(value)) => Ok(value),
                    Ok(Inbound::Pending(pending)) => (*pending).settle("事件监听器").await,
                    Err(message) => Err(message),
                };
                match settled {
                    Ok(Value::Null) => None,
                    Ok(value) => Some(Dyn::new(ExternValue(value))),
                    Err(message) => {
                        // 容量拒绝不是“监听器没有意见”：把剩余 waterfall 链继续跑完，
                        // 否则事件派发会静默吞掉下游。
                        if message.payload.code == "host_capacity" {
                            let frame = message
                                .frame
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner())
                                .take();
                            if let Some(frame) = frame {
                                return frame.call(next_args).await;
                            }
                        }
                        // 其他监听器失败：记日志并当作"没有意见"。
                        // 内核的 handler 签名不可失败（见 `cordis_core::event` 的说明）。
                        call_ctx.log(Level::Error, &format!("事件监听器失败: {message}"));
                        None
                    }
                }
            })
        });
        Ok(String::new())
    })
}

unsafe extern "C" fn host_ctx_effect(handle: u64, cb: u64, out: *mut Out) -> i32 {
    with_instance(handle, out, |instance| {
        let call = instance.call;
        let error_format = instance.error_format;
        let plugin_handle = handle;
        // M5-1：走 `try_effect`——**卸载期**（或已释放的 fiber）注册 effect 会回一条
        // 可读错误。走 `ctx.effect` 的话，这里会在 `extern "C"` 边界上 abort 整个进程，
        // 而"清理里想再挂一个收尾动作"是完全正常的插件写法。
        instance
            .ctx
            .fiber()
            .try_effect(move |_| {
                Disposable::new(move || {
                    Box::pin(async move {
                        let payload = json!({ "id": cb }).to_string();
                        if let Err(message) = call_plugin(
                            call,
                            plugin_handle,
                            cordis_abi::OP_CLEANUP,
                            &payload,
                            error_format,
                        ) {
                            eprintln!("[cordis] 插件清理回调失败: {message}");
                        }
                    })
                })
            })
            .map_err(|err| err.to_string())?;
        Ok(String::new())
    })
}

/// 按 mode 派发。`emit` 不取返回值，其余模式取"第一个 bail 值"。
fn dispatch_by_mode(ctx: &Ctx, mode: &str, event: &str, args: Vec<Dyn>) -> BoxFuture<Option<Dyn>> {
    match mode {
        "emit" => {
            let ctx = ctx.clone();
            let event = event.to_string();
            // 显式标注类型：`Box::pin(async {...})` 是具体类型，需要一次强制转换
            // 才能与其他分支的 `BoxFuture`（trait object）统一。
            let future: BoxFuture<Option<Dyn>> = Box::pin(async move {
                ctx.emit(&event, args).await;
                None
            });
            future
        }
        "serial" => ctx.serial(event, args),
        "parallel" => ctx.parallel(event, args),
        // 默认 bail：有返回值、遇第一个 bail 值即停。
        _ => ctx.bail(event, args),
    }
}

/// `ctx_emit` 与 `ctx_dispatch` 的共同实现：同步快速路径 + 异步兜底。
///
/// `continuation == 0` 表示调用方不要返回值（纯 `emit`）——这与 cordis 的
/// `emit` 语义一致：派发、丢弃返回值、不 await。
///
/// M2c：每次出站派发都在**发起实例**的记录表里登记一个节点（`parent` 来自
/// 派发作用域），响应带回它的 `dispatch_id`——`ctx_abort` 因此可以取消它，
/// 取消树也因此能把它的后代一网打尽。
#[allow(clippy::result_large_err)]
unsafe fn dispatch_impl(
    handle: u64,
    event: Blob,
    payload: Blob,
    continuation: u64,
    out: *mut Out,
) -> i32 {
    let event = event.as_str().to_string();
    let parsed: Value = match serde_json::from_slice(payload.as_bytes()) {
        Ok(value) => value,
        Err(err) => return fail(out, &format!("dispatch 的载荷不是合法 JSON: {err}")),
    };
    let args = json_to_args(&parsed.get("args").cloned().unwrap_or(Value::Null));
    let mode = parsed
        .get("mode")
        .and_then(|value| value.as_str())
        .unwrap_or("emit")
        .to_string();
    // 载荷版：容量拒绝必须保留 `host_capacity` / `capacity` / `not_executed`。
    with_instance_payload(handle, out, move |instance| {
        let ctx = instance.ctx.clone();
        let consumer = handle;
        let error_format = instance.error_format;
        let dispatch_id = next_dispatch_id();
        // 父节点 = 当前正在执行的那次派发（插件在自己的回调里发派发时，
        // 作用域栈顶就是它；不在派发里时为 None）。
        let parent = scope_top().map(|(_, id)| id);
        // 容量拒绝时直接返回：future 还没有被创建/驱动，订阅者不会收到派发。
        let (cancel, done) = insert_dispatch_task(
            instance,
            dispatch_id,
            DispatchRecord {
                sink: None,
                frame: None,
                parent,
                outbound: (continuation != 0).then_some(Outbound {
                    owner: handle,
                    continuation,
                    kind: ContinuationKind::Dispatch,
                }),
                stream: None,
            },
        )
        .map_err(|(payload, _)| payload)?;
        let future = dispatch_by_mode(&ctx, &mode, &event, args);
        // 作用域：这次派发**交付给插件**的入站回调据此记下父节点（取消级联）。
        let future: BoxFuture<Ret> = Box::pin(Scoped {
            inner: future,
            scope: (handle, dispatch_id),
        });
        let future = with_dispatch_timeout(future, event, instance.dispatch_policy);
        if continuation == 0 {
            // 纯 emit：派发后立即返回。这不是妥协——cordis 的 `emit` 本来就不取返回值。
            let Ok(runtime) = tokio::runtime::Handle::try_current() else {
                // 没有运行时就没有人驱动 future：不能假装 emit 成功。
                unregister_dispatch_node(handle, dispatch_id);
                return Err(ErrorPayload::new(
                    "emit 需要异步运行时（当前在同步上下文里，派发无法被驱动）",
                )
                .with_code("host_call")
                .with_category("protocol"));
            };
            let mut work = TaskWork {
                future,
                on_done: None::<fn(std::result::Result<Ret, ErrorPayload>)>,
                _exit: done,
            };
            runtime.spawn(async move {
                let panic = {
                    let mut caught = CatchUnwind::new(&mut work.future);
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => None,
                        outcome = &mut caught => outcome.err(),
                    }
                };
                if let Some(payload) = panic {
                    // 纯 emit 没有续延可交付：只摘表 + 诊断，绝不为它伪造一条回调。
                    record_async_panic("emit", handle, dispatch_id, &*payload);
                }
                unregister_dispatch_node(handle, dispatch_id);
                drop(work);
            });
            return Ok(json!({ "dispatch_id": dispatch_id }).to_string());
        }
        // 交付续延前先摘节点：**节点还在**表示这次派发由正常路径收尾；
        // 节点不在 ⇒ 它已经被取消（取消路径已经交付过一次「已取消」，
        // 这里必须**抑制**自然交付——"恰好一次"）。
        let finished = {
            let handle = handle;
            Arc::new(move || unregister_dispatch_node(handle, dispatch_id))
        };
        let deliver = {
            Arc::new(move |result: std::result::Result<Ret, ErrorPayload>| {
                let args = match result {
                    Ok(ret) => json!({ "result": ret_to_json(ret) }),
                    Err(err) => error_arg(error_format, &err),
                };
                let payload = json!({
                    "kind": KIND_DISPATCH,
                    "id": continuation,
                    "args": [args],
                })
                .to_string();
                deliver_outbound(consumer, dispatch_id, &payload);
            })
        };
        let deferred = {
            let deliver = deliver.clone();
            let deliver_panic = deliver.clone();
            complete_or_defer(
                future,
                move |result| deliver(result),
                move |payload| deliver_panic(Err(payload)),
                (cancel, done),
                PanicScope {
                    scene: "dispatch",
                    handle,
                    dispatch_id,
                },
            )
        };
        Ok(match deferred {
            Ok(Some(Ok(ret))) => {
                // 内联完成：结果直接带回，插件自己消费续延（M2 起的既有语义）；
                // 节点顺手摘掉（不投递）。
                finished();
                json!({ "pending": false, "result": ret_to_json(ret), "dispatch_id": dispatch_id })
                    .to_string()
            }
            Ok(Some(Err(err))) => {
                finished();
                let mut payload = json!({
                    "pending": false,
                    "dispatch_id": dispatch_id,
                });
                if let Value::Object(map) = &mut payload {
                    if let Value::Object(error) = error_arg(error_format, &err) {
                        map.extend(error);
                    }
                }
                payload.to_string()
            }
            Ok(None) => json!({ "pending": true, "dispatch_id": dispatch_id }).to_string(),
            Err(DeferFailure::NoRuntime(message)) => {
                // 没有运行时，future 已被丢弃：必须把登记节点也摘掉，
                // 否则取消/排水会看到一条永远不会结算的记录。
                finished();
                return Err(ErrorPayload::new(message)
                    .with_code("host_call")
                    .with_category("protocol"));
            }
            // 首 poll panic：同样必须**先摘表再返回错误**（否则关闭时补发 abort）。
            Err(DeferFailure::Panic(payload)) => {
                finished();
                return Err(payload);
            }
        })
    })
}

/// 摘除一条**出站派发**的登记节点。返回 `true` 表示"它还在表里"——
/// 即这次派发仍由正常路径收尾；被取消（或已注销）时返回 `false`。
fn unregister_dispatch_node(handle: u64, dispatch_id: u64) -> bool {
    let instance = instances().lock().unwrap().get(handle);
    let removed =
        instance.and_then(|instance| instance.dispatches.lock().unwrap().remove(&dispatch_id));
    removed.is_some()
}

// 普通出站的终态认领与调用准入必须在同一表锁内；关闭先赢则留给排水 abort。
// next 的入站记录可能早已终态，不能复用这个依赖记录存在的认领路径。
fn deliver_outbound(handle: u64, dispatch_id: u64, payload: &str) {
    let Some(instance) = instances().lock().unwrap().get(handle) else {
        return;
    };
    let (removed, _active) = {
        let mut table = instance.dispatches.lock().unwrap();
        if instance.closed.load(Ordering::SeqCst) || !table.contains_key(&dispatch_id) {
            return;
        }
        let Ok(lease) = call_lease(&instance) else {
            return;
        };
        let record = table.remove(&dispatch_id).unwrap();
        *instance.lifecycle.lock().unwrap() += 1;
        (
            record,
            ActiveCall {
                active: instance.lifecycle.clone(),
                changed: instance.calls_changed.clone(),
                lease: Ok(lease),
            },
        )
    };
    drop(removed);
    #[cfg(test)]
    {
        let gate = instance.outbound_gate.lock().unwrap().take();
        if let Some(gate) = gate {
            gate.0.wait();
            gate.1.wait();
        }
    }
    unsafe {
        if let Err(message) = call_plugin(
            instance.call,
            handle,
            OP_CALLBACK,
            payload,
            instance.error_format,
        ) {
            // 回调投递失败过去是静默的：插件等不到回调，宿主也没有任何线索。
            eprintln!(
                "[cordis] 出站续延交付失败（handle {handle}；dispatch {dispatch_id}）：{message}"
            );
        }
    }
}

fn ret_to_json(ret: Option<Dyn>) -> Value {
    ret.and_then(|value| value.downcast::<ExternValue>())
        .map(|value| value.0.clone())
        .unwrap_or(Value::Null)
}

unsafe extern "C" fn host_ctx_emit(
    handle: u64,
    event: Blob,
    payload: Blob,
    continuation: u64,
    out: *mut Out,
) -> i32 {
    dispatch_impl(handle, event, payload, continuation, out)
}

unsafe extern "C" fn host_ctx_dispatch(
    handle: u64,
    event: Blob,
    payload: Blob,
    continuation: u64,
    out: *mut Out,
) -> i32 {
    dispatch_impl(handle, event, payload, continuation, out)
}

unsafe extern "C" fn host_log(handle: u64, level: u32, message: Blob) -> i32 {
    let message = message.as_str().to_string();
    let ctx = instances()
        .lock()
        .unwrap()
        .get(handle)
        .map(|instance| instance.ctx);
    if let Some(ctx) = ctx {
        let level = match level {
            0 => Level::Debug,
            2 => Level::Warn,
            3 => Level::Error,
            _ => Level::Info,
        };
        // 与其它宿主函数一致：log sink 是宿主注入的第三方代码，panic 不能
        // 越过 `extern "C"` 边界 abort 整个进程。
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| ctx.log(level, &message)))
            .is_err()
        {
            eprintln!("[cordis] 宿主 log sink 在记录消息时 panic（已在 ABI 边界截获）");
            return STATUS_ERR;
        }
    }
    STATUS_OK
}

// ------------------------------------------------------------ 动态库加载

/// 一个从动态库装载进来的插件。
pub struct LoadedPlugin {
    /// 持有动态库句柄就持有 `dlopen` 的引用计数；drop 即卸载。
    /// **字段顺序有意义**：它必须活到最后，见 `raw_desc` 的说明。
    _library: Arc<dylib::DynLib>,
    /// 暂存副本的所有权（M0b，G11）：**必须声明在 `_library` 之后**——
    /// 字段按声明序 drop，"先 FreeLibrary 解锁、后 remove_file 删文件"
    /// 由结构本身保证（Windows 上反着来必然删不掉：库还锁着文件）。
    /// `None` = 直接 `open` 的原始路径（不是暂存副本，删不得）。
    staged: Option<StagedFile>,
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
        let path = path.as_ref().to_path_buf();
        let library = dylib::DynLib::open(&path)?;
        let entry: EntryFn = unsafe { std::mem::transmute(library.symbol(ENTRY_SYMBOL)?) };
        let desc = unsafe { entry() };
        if desc.is_null() {
            return Err(format!("{} 的 cordis_plugin_v1 返回了空指针", path.display()).into());
        }
        // **先只读冻结头**：`id`/`inject` 的 offset 在 `size` 覆盖范围之外时，
        // 直接读 `(*desc).id` 会越界。门禁必须发生在任何尾字段读取之前。
        let (abi_version, size) = unsafe { ((*desc).abi_version, (*desc).size as usize) };
        desc_gate(abi_version, size, cordis_abi::PLUGIN_DESC_PREFIX_SIZE)
            .map_err(|error| format!("插件文件 {}：{error}", path.display()))?;
        let (id, inject_json, provision_json, schema, schema_error, error_format) = unsafe {
            let schema = schema_from_blob((*desc).schema);
            (
                (*desc).id.as_str().to_string(),
                (*desc).inject.as_str().to_string(),
                // provision 是 v2 尾追加字段：v1 插件（size 只到冻结前缀）经
                // `field_at` 读作「未声明」；Blob 指向插件内存，必须立刻拷成 String
                // （附录 A）。
                PluginDesc::field_at::<Blob>(desc, std::mem::offset_of!(PluginDesc, provision))
                    .map(|blob| blob.as_str().to_string())
                    .unwrap_or_default(),
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
            _library: Arc::new(library),
            staged: None,
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
        let mut plugin = Self::open(path.as_ref())?;
        plugin.staged = Some(StagedFile::new(path.as_ref()));
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
        let original = original.as_ref();
        let staged = staged.as_ref();
        let original_abs = std::path::absolute(original)
            .map_err(|err| format!("规范化插件 artifact {} 失败: {err}", original.display()))?;
        let staged_abs = std::path::absolute(staged)
            .map_err(|err| format!("规范化暂存副本 {} 失败: {err}", staged.display()))?;
        let same = match (
            std::fs::canonicalize(&original_abs),
            std::fs::canonicalize(&staged_abs),
        ) {
            (Ok(original), Ok(staged)) => original == staged,
            _ => original_abs == staged_abs,
        };
        if same {
            return Err("open_staged_verified 拒绝把原始 artifact 当作暂存副本".into());
        }
        let discard_staged = || {
            drop(StagedFile::new(staged));
        };
        let verified = match policy.verify(&original_abs) {
            Ok(verified) => verified,
            Err(error) => {
                discard_staged();
                return Err(error.into());
            }
        };
        let staged_hash = match crate::source::sha256_file(&staged_abs) {
            Ok(hash) => hash,
            Err(error) => {
                discard_staged();
                return Err(error.into());
            }
        };
        if staged_hash != verified.sha256 {
            discard_staged();
            return Err(format!(
                "暂存副本 SHA256 与已校验 artifact 不匹配: staged={staged_hash} verified={}",
                verified.sha256
            )
            .into());
        }
        let mut plugin = match Self::open(&staged_abs) {
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
            return Err(format!(
                "插件 id 与 manifest 不匹配: manifest={} plugin={plugin_id}",
                verified.manifest.id
            )
            .into());
        }
        plugin.staged = Some(StagedFile::new(staged));
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
        // 动态库句柄以**弱引用**登记为实例保活：`Instance` 克隆不延长 owner——
        // owner 由实例表槽位持有到注销，最后 drop 在表锁之外（那一下才是 `FreeLibrary`）。
        let keepalive: Arc<dyn Any + Send + Sync> = self._library.clone();
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
        let registered = fiber.try_effect(move |_| {
            Disposable::new(move || {
                Box::pin(async move {
                    // 先关闭准入，再异步等待已经登记的同步调用退出。
                    let instance = instances().lock().unwrap().get(handle);
                    let mut outbound = Vec::new();
                    if let Some(instance) = &instance {
                        let close_lease = {
                            let _table = instance.dispatches.lock().unwrap();
                            instance.closed.store(true, Ordering::SeqCst);
                            call_lease(instance)
                        };
                        wait_lifecycle_exclusive(instance).await;
                        // closed 与登记共用 dispatches 锁；任务凭证不随业务摘表消失。
                        loop {
                            let (drained, tasks) = {
                                let mut table = instance.dispatches.lock().unwrap();
                                let drained: Vec<DispatchRecord> =
                                    table.drain().map(|(_, record)| record).collect();
                                let tasks: Vec<_> = instance.tasks.lock().unwrap().drain()
                                    .map(|(_, task)| task).collect();
                                (drained, tasks)
                            };
                            #[cfg(test)]
                            instance.close_drained.notify_one();
                            if drained.is_empty() && tasks.is_empty() {
                                break;
                            }
                            for mut record in drained {
                                // 流（ADR 0001 §5）：先排水——没收尾的流以错误结束，且
                                // **等待中的 credit 回调在这里被驳回**（跨 ABI 的提供者
                                // 只有这条路能知道"别推了"），然后才 OP_DISPOSE。
                                if let Some(sink) = &record.stream {
                                    sink.fail("插件已卸载：这条流没有收尾就被收回");
                                }
                                if let Some(tx) = record.sink.take() {
                                    let _ = tx.send(Err(ErrorPayload::new(
                                        "插件已卸载：挂起的入站续延被结算",
                                    )
                                    .with_code("host_lifecycle")
                                    .with_category("lifecycle")
                                    .with_certainty("unknown")));
                                }
                                if let Some(entry) = record.outbound {
                                    outbound.push(entry);
                                }
                            }
                            // 关闭不能只送 abort：宿主创建的任务必须**终止并等待**，
                            // 否则 `App::close` 返回后工具/副作用仍可能继续执行。
                            for task in &tasks {
                                task.cancel.cancel();
                            }
                            for rx in tasks.into_iter().map(|task| task.done) {
                                let _ = rx.await;
                            }
                        }
                        // 出站续延在 `closed` 之后仍要送达一次（插件在等它），
                        // 但不能再走 call_live（会被 closed 拒绝），直接调用持有保活的实例。
                        let unload_error = ErrorPayload::new("插件已卸载：等待中的调用被结算")
                            .with_code("host_lifecycle")
                            .with_category("lifecycle")
                            .with_certainty("unknown");
                        let credits = std::mem::take(&mut *instance.credits.lock().unwrap());
                        if close_lease.is_ok() {
                            for ((dispatch_id, id), count) in credits {
                                let payload = json!({
                                    "kind": KIND_STREAM_CREDIT,
                                    "id": id,
                                    "args": [{ "error": "插件已卸载：这条流已被收回" }],
                                }).to_string();
                                for _ in 0..count {
                                    let _scope = scope_push(handle, dispatch_id);
                                    let _ = unsafe { call_plugin(instance.call, handle, OP_CALLBACK, &payload, instance.error_format) };
                                }
                            }
                        }
                        for outbound in outbound {
                            if close_lease.is_ok() {
                                deliver_abort_direct(instance, outbound, &unload_error);
                            }
                        }
                        if applied_flag.load(Ordering::SeqCst) && close_lease.is_ok() {
                            let mut buffer = vec![0u8; 16 * 1024];
                            let mut out = Out {
                                ptr: buffer.as_mut_ptr(),
                                cap: buffer.len(),
                                len: 0,
                            };
                            let status = unsafe {
                                call(handle, OP_DISPOSE, Blob::EMPTY, Blob::EMPTY, &mut out)
                            };
                            if status != STATUS_OK {
                                if out.len > buffer.len() {
                                    eprintln!(
                                        "[cordis] 插件卸载通知输出声称 {} 字节，超过宿主缓冲 {} 字节",
                                        out.len,
                                        buffer.len()
                                    );
                                } else {
                                    buffer.truncate(out.len);
                                    let text = String::from_utf8_lossy(&buffer);
                                    let error = decode_plugin_error(error_format, &text);
                                    eprintln!("[cordis] 插件卸载通知失败: {}", error.error);
                                }
                            }
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
        .map(|blob| blob.as_str().to_string())
        .unwrap_or_default();
        let inject = parse_inject((*desc).inject.as_str());
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
            plugin: (*desc).id.as_str().to_string(),
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

// ------------------------------------------------------------ 插件解析

/// 进程级"已暂存、未认领"台账（G11 / D8）：`stage_plugin` 成功即入账，
/// [`StagedFile::new`] 认领即销账——销不掉账的就是泄漏，
/// [`staged_leaked`] / `loader.report` 的 `stagedLeaked` 直接暴露它。
static STAGED_UNADOPTED: OnceLock<Mutex<std::collections::HashSet<PathBuf>>> = OnceLock::new();

fn staged_unadopted() -> &'static Mutex<std::collections::HashSet<PathBuf>> {
    STAGED_UNADOPTED.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

/// 当前**已暂存但无人认领**的文件数（G11 的泄漏计数）。
///
/// 装载全部成功 ⇒ 每个暂存副本都被各自的 [`StagedFile`] 认领 ⇒ 0；
/// `stage_plugin` 成功而 `open` 失败（坏 DLL、探活失败前就报错等）⇒ >0。
pub fn staged_leaked() -> usize {
    staged_unadopted().lock().unwrap().len()
}

/// 一个**已被认领**的暂存文件：drop 时删除自己（M0b，G11 的所有权半边）。
///
/// - 认领（[`StagedFile::new`]）同时从泄漏台账销账；
/// - drop 删除失败只记日志、绝不 panic——文件可能仍被加载中的库锁住，
///   真正可靠的删除发生在 [`LoadedPlugin`] 的字段声明序保证的
///   "先 FreeLibrary、后 remove_file"之后；
/// - `Clone`/`Copy` 故意不存在：所有权唯一，共享请用 `Arc<StagedFile>`
///   （回滚记录就是这么持有它的）。
pub struct StagedFile(PathBuf);

impl StagedFile {
    /// 认领一个暂存文件（从泄漏台账销账）。文件此刻**不会**被删——
    /// 它要活到本对象 drop。
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        staged_unadopted().lock().unwrap().remove(&path);
        StagedFile(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for StagedFile {
    fn drop(&mut self) {
        if let Err(err) = std::fs::remove_file(&self.0) {
            // NotFound 是良性（别的路径已经删过）；其余只记日志：
            // 删除失败不能把宿主进程拖崩。
            if err.kind() != std::io::ErrorKind::NotFound {
                eprintln!("[cordis] 删除暂存文件 {} 失败: {err}", self.0.display());
            }
        }
    }
}

/// 插件名 → 动态库路径，带**版本化的临时副本**。
///
/// # 为什么要有副本
/// Windows 上已加载的 DLL 被文件系统锁住，无法原地覆盖；Linux 上覆盖已映射的
/// `.so` 也会让进程处于未定义状态。热重载因此必须把新库落到一个**新文件名**，
/// 让旧句柄继续指向旧文件、新句柄指向新文件，等旧 fiber 全部卸载后旧文件才可回收。
/// 这不是技巧，是动态加载的硬约束——任何支持热重载的插件系统都会撞上它。
pub fn stage_plugin(path: &Path, revision: u64) -> Result<PathBuf> {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "plugin".into());
    let extension = path.extension().map(|s| s.to_string_lossy().to_string());
    // 修订号只在**单个 LoaderState** 内单调。两个装载器首次加载同一 DLL 时
    // 都会请求 revision 1；若文件名只有 `.r1`，两边会同时写同一个副本，
    // 一边的 `LoadLibrary` 可能读到另一边正在覆盖的字节。把进程号 + 全局序号
    // 放进 `.rN` 之前的 stem，既保持按 `.r<digits>` 识别的既有清理逻辑，
    // 又让每次暂存拥有独立路径。跨进程同时写同一源文件也不会撞名。
    static STAGE_SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let unique = format!(
        "s{}-{}",
        std::process::id(),
        STAGE_SEQUENCE.fetch_add(1, Ordering::SeqCst)
    );
    let file_name = match &extension {
        Some(extension) => format!("{stem}.{unique}.r{revision}.{extension}"),
        None => format!("{stem}.{unique}.r{revision}"),
    };
    let target = path.with_file_name(file_name);
    std::fs::copy(path, &target)
        .map_err(|err| format!("复制插件到 {} 失败: {err}", target.display()))?;
    // 副本落地即入泄漏台账（G11）：在它被 `StagedFile` 认领之前，
    // 它就是一笔潜在的泄漏——认领发生在 `LoadedPlugin::open_staged`。
    staged_unadopted().lock().unwrap().insert(target.clone());
    Ok(target)
}

/// 插件解析：`name` → 动态库路径。
///
/// 这里是"任意插件安装"真正落地的地方。本实现只做最朴素的文件解析；
/// 换成一个包管理器（npm 风格的 `node_modules` 解析、OCI、内置 registry）
/// 只需要替换这一个函数——内核与 ABI 都不需要改。
pub fn resolve_plugin_path(name: &str, search: &[PathBuf]) -> Result<PathBuf> {
    let candidates = |stem: &str| {
        let mut files = vec![PathBuf::from(stem)];
        if cfg!(windows) {
            files.push(PathBuf::from(format!("{stem}.dll")));
        } else if cfg!(target_os = "macos") {
            files.push(PathBuf::from(format!("{stem}.dylib")));
            // Cargo 的 cdylib 产物在 macOS 上也带 `lib` 前缀。
            files.push(PathBuf::from(format!("lib{stem}.dylib")));
        } else {
            files.push(PathBuf::from(format!("{stem}.so")));
            // 也接受 `lib<name>.so` 的约定。
            files.push(PathBuf::from(format!("lib{stem}.so")));
        }
        files
    };
    for candidate in candidates(name) {
        let path = candidate;
        if path.is_file() {
            return Ok(path);
        }
    }
    for dir in search {
        for candidate in candidates(name) {
            let path = dir.join(candidate);
            if path.is_file() {
                return Ok(path);
            }
        }
    }
    Err(format!("找不到插件 \"{name}\"；搜索目录: {search:?}").into())
}

/// 把加载好的插件挂到上下文上，并把行级 `inject` 合并进 fiber。
///
/// **行级 inject 必须在依赖解析之前合并**（`loader/src/index.ts:129-135`）。
/// 这里用"构造 fiber 时直接传入合并后的 inject"实现，比 JS 的
/// `internal/plugin` 观察者路径更直接；观察者路径在内核里也保留着，
/// 供不自己创建 fiber 的外部扩展使用。
///
/// # Panics
/// `row_inject` 形状非法时 panic。装载器路径已在 [`crate::patch::Entry`] 解析期
/// 校验；直接调用本函数应传入同样合法的形状（数组、平铺对象或
/// `{required, optional}`）。
pub fn mount(
    ctx: &Ctx,
    plugin: Arc<dyn Plugin>,
    config: Config,
    row_inject: Option<Config>,
) -> Arc<cordis_core::Fiber> {
    let mut inject = plugin.inject();
    let parsed = row_inject
        .as_ref()
        .map(|row| {
            crate::patch::parse_inject(row)
                .unwrap_or_else(|err| panic!("entry-level inject 已在解析期校验: {err}"))
        })
        .unwrap_or_default();
    for (name, value) in parsed.required {
        if parsed.merge_config {
            inject = inject.with(&name, value);
        } else if !inject.contains(&name) {
            inject.0.push((name.into(), Config::Null));
        }
    }
    ctx.store
        .plugin_with_optional(ctx, plugin, config, inject, &parsed.optional)
}

/// 便利函数：把一个 `Error` 转成 JSON 友好的字符串。
pub fn describe(error: &Error) -> String {
    error.to_string()
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
mod tests {
    use super::*;
    use std::future::Future;

    thread_local! {
        static TASK_TEST_HANDLE: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    }

    unsafe extern "C" fn task_test_call(
        handle: u64,
        op: u32,
        _: Blob,
        _: Blob,
        _: *mut Out,
    ) -> i32 {
        if op == OP_APPLY {
            TASK_TEST_HANDLE.set(handle);
        }
        STATUS_OK
    }

    #[tokio::test]
    async fn o1_task_survives_normal_record_removal() {
        let app = cordis_core::App::new();
        apply_via(task_test_call, app.root.clone(), Value::Null, "task-owner")
            .await
            .unwrap();
        let handle = TASK_TEST_HANDLE.get();
        let instance = instances().lock().unwrap().get(handle).unwrap();
        insert_dispatch(
            &instance,
            91,
            DispatchRecord {
                sink: None,
                frame: None,
                parent: None,
                outbound: None,
                stream: None,
            },
        )
        .unwrap_or_else(|_| panic!("admission"));
        let (_cancel, done) = register_dispatch_task(&instance, 91).unwrap();
        assert!(unregister_dispatch_node(handle, 91));
        // A task in its synchronous delivery tail cannot observe cancellation yet.
        // No lifecycle read lock is held: only task ownership can keep close pending.
        let mut close = Box::pin(app.close());
        let mut drained = Box::pin(instance.close_drained.notified());
        std::future::poll_fn(|cx| {
            assert!(
                close.as_mut().poll(cx).is_pending(),
                "close lost the task after normal record removal"
            );
            drained.as_mut().poll(cx)
        })
        .await;
        drop(done);
        close.await;
    }

    #[tokio::test]
    async fn o1_fulfill_keeps_pending_task_cancellable() {
        let app = cordis_core::App::new();
        apply_via(
            task_test_call,
            app.root.clone(),
            Value::Null,
            "task-fulfill",
        )
        .await
        .unwrap();
        let handle = TASK_TEST_HANDLE.get();
        let instance = instances().lock().unwrap().get(handle).unwrap();
        let (tx, _rx) = tokio::sync::oneshot::channel();
        insert_dispatch(
            &instance,
            92,
            DispatchRecord {
                sink: Some(tx),
                frame: None,
                parent: None,
                outbound: None,
                stream: None,
            },
        )
        .unwrap_or_else(|_| panic!("admission"));
        let (cancel, done) = register_dispatch_task(&instance, 92).unwrap();
        let mut output = Out {
            ptr: std::ptr::null_mut(),
            cap: 0,
            len: 0,
        };
        assert_eq!(
            unsafe { host_ctx_fulfill(handle, 92, Blob::EMPTY, &mut output) },
            STATUS_OK
        );
        let mut close = Box::pin(app.close());
        let mut drained = Box::pin(instance.close_drained.notified());
        std::future::poll_fn(|cx| {
            assert!(
                close.as_mut().poll(cx).is_pending(),
                "close lost the pending next task"
            );
            drained.as_mut().poll(cx)
        })
        .await;
        assert!(
            matches!(
                std::future::poll_fn(|cx| Poll::Ready(
                    Box::pin(cancel.cancelled()).as_mut().poll(cx)
                ))
                .await,
                Poll::Ready(())
            ),
            "fulfill must not discard task cancellation"
        );
        drop(done);
        close.await;
    }

    thread_local! {
        static NEXT_RETURN_STATUS: std::cell::Cell<i32> = const { std::cell::Cell::new(STATUS_OK) };
        static NEXT_CALL_STATUS: std::cell::Cell<i32> = const { std::cell::Cell::new(STATUS_ERR) };
        static NEXT_DROP_MARKER: std::cell::RefCell<Option<Arc<AtomicBool>>> = const { std::cell::RefCell::new(None) };
        static NEXT_DISPOSE_AFTER_DROP: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        static NEXT_COMPLETIONS: std::cell::RefCell<Vec<Value>> = const { std::cell::RefCell::new(Vec::new()) };
        static NEXT_COMPLETED: std::cell::RefCell<Option<Arc<tokio::sync::Notify>>> = const { std::cell::RefCell::new(None) };
    }

    unsafe extern "C" fn next_then_terminal(
        handle: u64,
        op: u32,
        payload: Blob,
        _: Blob,
        _: *mut Out,
    ) -> i32 {
        if op == OP_APPLY {
            TASK_TEST_HANDLE.set(handle);
        }
        if op == OP_CALLBACK {
            let Ok(value) = serde_json::from_slice::<Value>(payload.as_bytes()) else {
                return STATUS_ERR;
            };
            if value["kind"] == KIND_NEXT {
                NEXT_COMPLETIONS.with_borrow_mut(|completions| completions.push(value));
                NEXT_COMPLETED.with_borrow(|completed| {
                    if let Some(completed) = completed {
                        completed.notify_one();
                    }
                });
                return STATUS_OK;
            }
            let Some(id) = value["dispatch_id"].as_u64() else {
                return STATUS_ERR;
            };
            let mut output = Out {
                ptr: std::ptr::null_mut(),
                cap: 0,
                len: 0,
            };
            NEXT_CALL_STATUS.set(host_ctx_next(handle, id, Blob::new(b"[]"), 1, &mut output));
            return NEXT_RETURN_STATUS.get();
        }
        if op == OP_DISPOSE {
            NEXT_DISPOSE_AFTER_DROP.set(NEXT_DROP_MARKER.with_borrow(|marker| {
                marker
                    .as_ref()
                    .is_some_and(|marker| marker.load(Ordering::SeqCst))
            }));
        }
        STATUS_OK
    }

    #[tokio::test]
    async fn o1_real_next_survives_all_inbound_terminal_states() {
        struct Dropped(Arc<AtomicBool>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        for status in [
            STATUS_OK,
            STATUS_ERR,
            STATUS_UNSUPPORTED,
            12345,
            STATUS_PENDING,
        ] {
            NEXT_RETURN_STATUS.set(status);
            NEXT_CALL_STATUS.set(STATUS_ERR);
            let app = cordis_core::App::new();
            apply_via(
                next_then_terminal,
                app.root.clone(),
                Value::Null,
                "next-terminal",
            )
            .await
            .unwrap();
            let handle = TASK_TEST_HANDLE.get();
            let instance = instances().lock().unwrap().get(handle).unwrap();
            let dropped = Arc::new(AtomicBool::new(false));
            NEXT_DROP_MARKER.with_borrow_mut(|marker| *marker = Some(dropped.clone()));
            NEXT_DISPOSE_AFTER_DROP.set(false);
            let marker = dropped.clone();
            let frame = Next::some(move |_| {
                let guard = Dropped(marker.clone());
                Box::pin(async move {
                    let _guard = guard;
                    std::future::pending::<()>().await;
                    None
                })
            });
            let result = unsafe {
                call_inbound(
                    &instance,
                    handle,
                    OP_CALLBACK,
                    json!({}),
                    None,
                    None,
                    Some(frame),
                    None,
                )
            };
            assert_eq!(NEXT_CALL_STATUS.get(), STATUS_OK);
            if let Ok(Inbound::Pending(pending)) = result {
                let mut output = Out {
                    ptr: std::ptr::null_mut(),
                    cap: 0,
                    len: 0,
                };
                assert_eq!(
                    unsafe {
                        host_ctx_fulfill(handle, pending.dispatch_id, Blob::EMPTY, &mut output)
                    },
                    STATUS_OK
                );
                assert_eq!(
                    unsafe { host_ctx_abort(handle, pending.dispatch_id, &mut output) },
                    STATUS_ERR,
                    "已结算 id 不再是可 abort 的在飞派发"
                );
                assert_eq!(instance.tasks.lock().unwrap().len(), 1);
                drop(pending);
            }
            assert!(instance.dispatches.lock().unwrap().is_empty());
            assert_eq!(instance.tasks.lock().unwrap().len(), 1);
            assert!(!dropped.load(Ordering::SeqCst));
            tokio::time::timeout(std::time::Duration::from_secs(3), app.close())
                .await
                .unwrap();
            assert!(
                NEXT_DISPOSE_AFTER_DROP.get(),
                "OP_DISPOSE 必须晚于 next future drop"
            );
            NEXT_DROP_MARKER.with_borrow_mut(|marker| *marker = None);
            assert!(
                dropped.load(Ordering::SeqCst),
                "close returned before next Drop for {status}"
            );
            assert!(instance.tasks.lock().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn o1_real_next_completes_after_inbound_record_removal() {
        for status in [STATUS_OK, STATUS_PENDING] {
            NEXT_RETURN_STATUS.set(status);
            NEXT_CALL_STATUS.set(STATUS_ERR);
            NEXT_COMPLETIONS.with_borrow_mut(Vec::clear);
            let completed = Arc::new(tokio::sync::Notify::new());
            NEXT_COMPLETED.with_borrow_mut(|slot| *slot = Some(completed.clone()));
            let app = cordis_core::App::new();
            apply_via(
                next_then_terminal,
                app.root.clone(),
                Value::Null,
                "next-normal-completion",
            )
            .await
            .unwrap();
            let handle = TASK_TEST_HANDLE.get();
            let instance = instances().lock().unwrap().get(handle).unwrap();
            let release = Arc::new(tokio::sync::Notify::new());
            let started = Arc::new(AtomicBool::new(false));
            let frame = Next::some({
                let release = release.clone();
                let started = started.clone();
                move |_| {
                    let release = release.clone();
                    let started = started.clone();
                    Box::pin(async move {
                        started.store(true, Ordering::SeqCst);
                        release.notified().await;
                        Some(Dyn::new(ExternValue(json!({ "tail": 42 }))))
                    })
                }
            });
            let result = unsafe {
                call_inbound(
                    &instance,
                    handle,
                    OP_CALLBACK,
                    json!({}),
                    None,
                    None,
                    Some(frame),
                    None,
                )
            };
            assert_eq!(NEXT_CALL_STATUS.get(), STATUS_OK);
            assert!(
                started.load(Ordering::SeqCst),
                "next must first poll pending"
            );
            match result.unwrap() {
                Inbound::Ready(value) => {
                    assert_eq!(status, STATUS_OK);
                    assert_eq!(value, Value::Null);
                }
                Inbound::Pending(pending) => {
                    assert_eq!(status, STATUS_PENDING);
                    let mut output = Out {
                        ptr: std::ptr::null_mut(),
                        cap: 0,
                        len: 0,
                    };
                    assert_eq!(
                        unsafe {
                            host_ctx_fulfill(handle, pending.dispatch_id, Blob::EMPTY, &mut output)
                        },
                        STATUS_OK
                    );
                    drop(pending);
                }
            }
            assert!(instance.dispatches.lock().unwrap().is_empty());
            assert_eq!(instance.tasks.lock().unwrap().len(), 1);
            NEXT_COMPLETIONS.with_borrow(|completions| assert!(completions.is_empty()));

            release.notify_one();
            tokio::time::timeout(std::time::Duration::from_secs(3), completed.notified())
                .await
                .unwrap();
            // The current-thread task finishes its synchronous delivery tail before we resume.
            assert!(!instance.closed.load(Ordering::SeqCst));
            assert!(instance.dispatches.lock().unwrap().is_empty());
            assert!(instance.tasks.lock().unwrap().is_empty());
            NEXT_COMPLETIONS.with_borrow(|completions| {
                assert_eq!(
                    completions.as_slice(),
                    &[json!({ "kind": "next", "id": 1, "args": [{ "result": { "tail": 42 } }] })]
                );
            });
            app.close().await;
            NEXT_COMPLETED.with_borrow_mut(|slot| *slot = None);
        }
    }

    #[tokio::test]
    async fn o1_multiple_tasks_reclaim_without_new_work() {
        let instance = dummy_instance("multi-task");
        insert_dispatch(
            &instance,
            93,
            DispatchRecord {
                sink: None,
                frame: None,
                parent: None,
                outbound: None,
                stream: None,
            },
        )
        .unwrap_or_else(|_| panic!("admission"));
        let (cancel_a, exit_a) = register_dispatch_task(&instance, 93).unwrap();
        let (_cancel_b, exit_b) = register_dispatch_task(&instance, 93).unwrap();
        assert_eq!(instance.tasks.lock().unwrap().len(), 2);
        drop(exit_a);
        assert_eq!(instance.tasks.lock().unwrap().len(), 1);
        assert!(std::future::poll_fn(|cx| Poll::Ready(
            Box::pin(cancel_a.cancelled()).as_mut().poll(cx)
        ))
        .await
        .is_pending());
        drop(exit_b);
        assert!(instance.tasks.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn o1_task_work_drops_captures_before_exit_on_panic() {
        struct CheckDrop(Arc<Mutex<HashMap<u64, DispatchTask>>>);
        impl Drop for CheckDrop {
            fn drop(&mut self) {
                assert_eq!(self.0.lock().unwrap().len(), 1);
            }
        }
        let instance = dummy_instance("panic-exit");
        insert_dispatch(
            &instance,
            94,
            DispatchRecord {
                sink: None,
                frame: None,
                parent: None,
                outbound: None,
                stream: None,
            },
        )
        .unwrap_or_else(|_| panic!("admission"));
        let task = register_dispatch_task(&instance, 94).unwrap();
        let future_guard = CheckDrop(instance.tasks.clone());
        let callback_guard = CheckDrop(instance.tasks.clone());
        // O3 起首 poll panic 不再向外 unwind：它收敛成 `DeferFailure::Panic`。
        // 这里钉住的仍是 O1 的析构序——future 与交付闭包的捕获物都必须在
        // 退出信号（`TaskExit`）之前释放（`CheckDrop` 断言 tasks 里还有 1 条）。
        let outcome = complete_or_defer::<()>(
            Box::pin(async move {
                let _guard = future_guard;
                panic!("Rust future panic");
            }),
            move |_| drop(callback_guard),
            |_| panic!("首 poll panic 时不得调用 on_panic"),
            task,
            PanicScope {
                scene: "test",
                handle: 0,
                dispatch_id: 94,
            },
        );
        assert!(
            matches!(outcome, Err(DeferFailure::Panic(_))),
            "首 poll panic 必须收敛成可区分的失败"
        );
        assert!(instance.tasks.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn o1_stream_settle_registers_and_reclaims() {
        let instance = dummy_instance("stream-settle");
        instance.closed.store(true, Ordering::SeqCst);
        let (tx, rx) = tokio::sync::oneshot::channel();
        let pending = PendingContinuation {
            instance: instance.clone(),
            dispatch_id: 95,
            rx,
            cancel: None,
            _abandon: PendingDropGuard {
                instance: instance.clone(),
                dispatch_id: 95,
            },
        };
        assert!(spawn_stream_settle(&instance, Box::new(pending)).is_err());
        assert!(instance.tasks.lock().unwrap().is_empty());
        drop(tx);
        instance.closed.store(false, Ordering::SeqCst);
        let (tx, rx) = tokio::sync::oneshot::channel();
        insert_dispatch(
            &instance,
            96,
            DispatchRecord {
                sink: Some(tx),
                frame: None,
                parent: None,
                outbound: None,
                stream: None,
            },
        )
        .unwrap_or_else(|_| panic!("admission"));
        let pending = PendingContinuation {
            instance: instance.clone(),
            dispatch_id: 96,
            rx,
            cancel: None,
            _abandon: PendingDropGuard {
                instance: instance.clone(),
                dispatch_id: 96,
            },
        };
        spawn_stream_settle(&instance, Box::new(pending)).unwrap();
        assert_eq!(instance.tasks.lock().unwrap().len(), 1);
        instance
            .dispatches
            .lock()
            .unwrap()
            .remove(&96)
            .unwrap()
            .sink
            .unwrap()
            .send(Ok(Value::Null))
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !instance.tasks.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("真实 settle 任务完成后必须自行摘除 registry");
        assert!(instance.tasks.lock().unwrap().is_empty());
    }

    #[test]
    fn schema_blob_rejects_null_and_impossible_lengths() {
        let null = schema_from_blob(Blob {
            ptr: std::ptr::null(),
            len: 1,
        });
        assert!(null.text.is_none(), "NULL + 非零长度不得解引用");
        assert!(null.error.is_some(), "非法 Blob 必须留下诊断");

        let empty = schema_from_blob(Blob::EMPTY);
        assert!(empty.text.is_none());
        assert!(empty.error.is_none(), "空 Blob = 未声明，不是非法");

        let huge = schema_from_blob(Blob {
            ptr: b"{\"a\":1}".as_ptr(),
            len: usize::MAX,
        });
        assert!(huge.text.is_none(), "超长长度不得传给 from_raw_parts");
        assert!(huge.error.is_some());
    }

    #[test]
    fn schema_blob_accepts_only_utf8_json_objects() {
        assert_eq!(
            schema_from_blob(Blob::new(br#"{"type":"object"}"#)).text,
            Some(r#"{"type":"object"}"#.to_string())
        );
        let array = schema_from_blob(Blob::new(br#"["not","object"]"#));
        assert!(array.text.is_none());
        assert!(array.error.is_some(), "非对象必须可诊断，不能等同未声明");

        let bad_utf8 = schema_from_blob(Blob::new(&[0xff]));
        assert!(bad_utf8.text.is_none());
        assert!(bad_utf8.error.is_some());
    }

    #[test]
    fn schema_over_the_display_cap_is_reported_not_parsed() {
        let oversized = format!(
            r#"{{"x":"{}"}}"#,
            "x".repeat(cordis_abi::MAX_CONFIG_SCHEMA_BYTES)
        );
        let read = schema_from_text(oversized);
        assert!(read.text.is_none());
        assert!(read.error.unwrap().contains("上限"));
    }

    #[test]
    fn capabilities_of_desc_gates_before_reading_tail_fields() {
        let desc = PluginDesc {
            abi_version: ABI_VERSION,
            size: 40,
            id: Blob::EMPTY,
            inject: Blob::EMPTY,
            schema: Blob::EMPTY,
            attach: fake_attach,
            call: fake_call,
            provision: Blob::EMPTY,
            error_format: 0,
            tail_reserved: 0,
        };
        let caps = unsafe { Capabilities::of_desc(&desc) };
        assert_eq!(caps, Capabilities::default(), "短 desc 不得读尾字段");
    }

    unsafe extern "C" fn fake_attach(_: *const HostApi) {}

    #[test]
    fn capabilities_of_desc_preserves_inject_order_like_of_plugin() {
        static ID: &[u8] = b"order";
        static INJECT: &[u8] = br#"["b","a"]"#;
        let desc = PluginDesc {
            abi_version: ABI_VERSION,
            size: std::mem::size_of::<PluginDesc>() as u32,
            id: Blob::new(ID),
            inject: Blob::new(INJECT),
            schema: Blob::EMPTY,
            attach: fake_attach,
            call: fake_caps_call,
            provision: Blob::EMPTY,
            error_format: 0,
            tail_reserved: 0,
        };
        let caps = unsafe { Capabilities::of_desc(&desc) };
        assert_eq!(caps.inject, vec!["b".to_string(), "a".to_string()]);
    }

    unsafe extern "C" fn fake_caps_call(_: u64, op: u32, _: Blob, _: Blob, out: *mut Out) -> i32 {
        if op == cordis_abi::OP_VOLATILE_PATHS {
            (*out).write(b"[]");
        }
        STATUS_OK
    }

    #[tokio::test]
    async fn pending_settle_preserves_a_structured_host_failure() {
        let instance = dummy_instance("settle-cause");
        let (tx, rx) = tokio::sync::oneshot::channel();
        let pending = PendingContinuation {
            instance: instance.clone(),
            dispatch_id: 0x5E7,
            rx,
            cancel: None,
            _abandon: PendingDropGuard {
                instance,
                dispatch_id: 0x5E7,
            },
        };
        let payload = ErrorPayload::new("插件已卸载：挂起的入站续延被结算")
            .with_code("host_lifecycle")
            .with_category("lifecycle")
            .with_certainty("unknown");
        tx.send(Err(payload)).expect("sink 必须接收结构化原因");

        let failure = pending
            .settle("接口方法")
            .await
            .expect_err("卸载原因必须作为失败返回");
        assert_eq!(failure.payload.code, "host_lifecycle");
        assert_eq!(failure.payload.category, "lifecycle");
        assert_eq!(failure.payload.certainty, "unknown");
    }

    fn dummy_instance(name: &str) -> Instance {
        let _ = name;
        let app = Box::leak(Box::new(cordis_core::App::new()));
        Instance {
            ctx: app.root.clone(),
            call: fake_call,
            dispatches: Arc::new(Mutex::new(HashMap::new())),
            keepalive: None,
            closed: Arc::new(AtomicBool::new(false)),
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
            error_format: 0,
            dispatch_policy: DispatchPolicy::default(),
            host_limits: HostLimits::default(),
        }
    }

    unsafe extern "C" fn fake_call(_: u64, _: u32, _: Blob, _: Blob, _: *mut Out) -> i32 {
        STATUS_OK
    }

    unsafe extern "C" fn bench_call(_: u64, _: u32, _: Blob, _: Blob, out: *mut Out) -> i32 {
        (*out).write(b"pong")
    }

    #[test]
    fn call_buffer_pool_does_not_expose_stale_bytes() {
        let mut first = CallBuffer::take();
        first.0[..8].copy_from_slice(b"leftover");
        drop(first);
        let second = CallBuffer::take();
        assert_eq!(second.len(), 16 * 1024);
        assert!(
            second.0.iter().all(|byte| *byte == 0),
            "池化缓冲必须清零，不能把上一个插件的残留交给下一个插件"
        );
    }

    /// 手动基准：`cargo test -p rust-cordis-host --lib call_buffer_pool_benchmark -- --ignored --nocapture`
    #[test]
    #[ignore = "manual buffer-pool benchmark"]
    fn call_buffer_pool_benchmark() {
        const CALLS: u32 = 100_000;
        let started = std::time::Instant::now();
        for _ in 0..CALLS {
            let (status, text) = unsafe { invoke_raw(bench_call, 1, cordis_abi::OP_PING, "{}") };
            assert_eq!(status, STATUS_OK);
            assert_eq!(text, "pong");
        }
        let elapsed = started.elapsed();
        println!(
            "call buffer benchmark: {CALLS} calls in {:?} ({:.1} ns/call)",
            elapsed,
            elapsed.as_nanos() as f64 / CALLS as f64
        );
    }

    /// 句柄带代际号：槽位复用后，**陈旧句柄必须被拒绝**。
    ///
    /// 这是热重载系统里最危险的一类 bug 的防线：卸载后残留的句柄，
    /// 如果在新插件装载后又能解析成"一个合法但完全错误的实例"，
    /// 就会变成极难定位的跨插件串线。
    #[test]
    fn stale_handles_are_rejected_after_slot_reuse() {
        let mut slab = Slab::default();
        let first = slab.insert(dummy_instance("first"), None);
        assert!(slab.get(first).is_some());

        // 卸载 → 槽位进入空闲表。
        assert!(slab.remove(first).is_some());
        assert!(slab.get(first).is_none(), "卸载后句柄立刻失效");

        // 新实例复用同一个槽位。
        let second = slab.insert(dummy_instance("second"), None);
        assert_eq!(
            Slab::split(first).0,
            Slab::split(second).0,
            "槽位确实被复用了"
        );
        assert_ne!(first, second, "但代际号不同，所以句柄不同");
        assert!(slab.get(first).is_none(), "陈旧句柄绝不能解析到新实例");
        assert!(slab.get(second).is_some());
    }

    /// 句柄 0 是保留值，永远不是合法实例。
    #[test]
    fn handle_zero_is_reserved() {
        let mut slab = Slab::default();
        assert!(slab.get(0).is_none());
        let handle = slab.insert(dummy_instance("x"), None);
        assert_ne!(handle, 0);
        assert!(slab.get(handle).is_some());
    }

    /// 反复插入/移除不泄漏槽位。
    #[test]
    fn slots_are_recycled_not_leaked() {
        let mut slab = Slab::default();
        let handles: Vec<u64> = (0..8)
            .map(|i| slab.insert(dummy_instance(&format!("p{i}")), None))
            .collect();
        assert_eq!(slab.live(), 8);
        for handle in &handles {
            assert!(slab.remove(*handle).is_some());
        }
        assert_eq!(slab.live(), 0);
        // 再插入 8 个：槽位应当被复用，容量不增长。
        for i in 0..8 {
            slab.insert(dummy_instance(&format!("q{i}")), None);
        }
        assert_eq!(slab.slots.len(), 8, "槽位应被复用而不是无限增长");
    }

    // ------------------------------------------------------ M1-1：ABI 装载/卸载生命周期

    // 假 ABI 插件的调用日志与句柄。
    //
    // `thread_local!` 而非 `static`：`#[tokio::test]` 默认 current_thread，
    // `Store::plugin` 的 settle、dispose 的 disposer 任务都在测试线程上执行，
    // 因此每个用例自带状态、互不干扰。
    thread_local! {
        static ABI_TEST_LOG: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
        static ABI_TEST_HANDLE: std::cell::Cell<u64> = const { std::cell::Cell::new(u64::MAX) };
    }

    /// 假 ABI 插件入口（模拟 SDK 的行为）：
    /// - OP_APPLY：经 `host_ctx_effect` 注册一个 cleanup；
    /// - OP_CLEANUP（cleanup 触发）：用**同一个 handle** 读外部服务 `abi-test-svc`，记录结果；
    /// - OP_DISPOSE（卸载通知）：记录顺序。
    ///
    /// 注意：fake `CallFn` 内**禁止 assert**（panic 跨 `extern "C"` 即 abort），
    /// 只记录状态，由测试体断言。
    unsafe extern "C" fn abi_fake_call(
        handle: u64,
        op: u32,
        _a: Blob,
        _b: Blob,
        out: *mut Out,
    ) -> i32 {
        match op {
            OP_APPLY => {
                ABI_TEST_HANDLE.with(|cell| cell.set(handle));
                let mut buf = [0u8; 64];
                let mut cleanup_out = Out {
                    ptr: buf.as_mut_ptr(),
                    cap: buf.len(),
                    len: 0,
                };
                let status = host_ctx_effect(handle, 7, &mut cleanup_out);
                if status != STATUS_OK {
                    return status;
                }
                (*out).write(b"")
            }
            cordis_abi::OP_CLEANUP => {
                let mut buf = [0u8; 1024];
                let mut read_out = Out {
                    ptr: buf.as_mut_ptr(),
                    cap: buf.len(),
                    len: 0,
                };
                let status = host_ctx_get(handle, Blob::new(b"abi-test-svc"), &mut read_out);
                let text =
                    std::str::from_utf8(std::slice::from_raw_parts(read_out.ptr, read_out.len))
                        .unwrap_or("<non-utf8>")
                        .to_string();
                ABI_TEST_LOG.with(|log| {
                    log.borrow_mut().push(if status == STATUS_OK {
                        format!("cleanup:Ok({text})")
                    } else {
                        format!("cleanup:Err({text})")
                    })
                });
                (*out).write(b"")
            }
            OP_DISPOSE => {
                ABI_TEST_LOG.with(|log| log.borrow_mut().push("dispose".to_string()));
                (*out).write(b"")
            }
            _ => cordis_abi::STATUS_UNSUPPORTED,
        }
    }

    /// M1-1（红→绿）：插件 cleanup 必须在**句柄失效之前**执行、且能读到服务依赖。
    ///
    /// 规格（对齐 JS cordis：cleanup 是 fiber 的 effect，卸载时 LIFO 逆序释放）：
    /// 整个拆除过程（含插件 cleanup）期间 ctx 句柄有效；OP_DISPOSE 是"宿主即将回收
    /// 实例"的通知，必须**最后**投递。
    ///
    /// 预期红（修复前）：`["dispose", "cleanup:Err(ctx 句柄 … 已失效)"]`——
    /// dispose effect 注册在 OP_APPLY **之后**，LIFO 下它先跑，把句柄提前注销了。
    #[tokio::test]
    async fn abi_cleanup_can_read_services_before_dispose() {
        ABI_TEST_LOG.with(|log| log.borrow_mut().clear());

        let app = cordis_core::App::new();
        // 外部提供者：独立原生插件、保持 ACTIVE —— cleanup 读的是它。
        // （不能读"自己提供的服务"：拆除期提供者已进入 UNLOADING，strict 读必然失败。）
        let provider = cordis_core::plugin_fn(
            "abi-test-provider",
            cordis_core::Inject::default(),
            |ctx: Ctx, _cfg| {
                Box::pin(async move {
                    ctx.provide("abi-test-svc", Dyn::new(ExternValue(json!({ "ok": true }))))?;
                    Ok(())
                })
            },
        );
        let provider_fiber = app.root.plugin(provider, Config::Null);
        provider_fiber.settle().await;
        assert_eq!(provider_fiber.state(), cordis_core::FiberState::Active);

        // ABI 风格插件：FnPlugin 包装 apply_via + 假插件入口。
        let plugin: Arc<dyn Plugin> = Arc::new(cordis_core::FnPlugin {
            name: "abi-test",
            key: cordis_core::unique_key("abi-test"),
            inject: cordis_core::Inject::default(),
            validator: None,
            f: Arc::new(|ctx: Ctx, config: Config| {
                apply_via(abi_fake_call, ctx, config, "abi-test")
            }),
        });
        let fiber = app.root.plugin(plugin, Config::Null);
        fiber.settle().await;
        assert_eq!(
            fiber.state(),
            cordis_core::FiberState::Active,
            "假插件应装载成功"
        );

        fiber.dispose().await;

        let log = ABI_TEST_LOG.with(|log| log.borrow().clone());
        assert_eq!(
            log.len(),
            2,
            "应恰好有一次 cleanup 与一次 dispose 通知：{log:?}"
        );
        assert!(
            log[0].starts_with("cleanup:Ok("),
            "cleanup 必须在句柄失效前执行：{log:?}"
        );
        assert!(
            log[0].contains("\"ok\":true"),
            "cleanup 应读到外部服务的值：{log:?}"
        );
        assert_eq!(
            log[1], "dispose",
            "OP_DISPOSE 必须在 cleanup 之后投递：{log:?}"
        );
    }

    /// 假 ABI 插件入口：OP_APPLY 直接失败；其余 op 一律记录（用于断言"不该发生"）。
    unsafe extern "C" fn abi_fake_call_failing(
        handle: u64,
        op: u32,
        _a: Blob,
        _b: Blob,
        out: *mut Out,
    ) -> i32 {
        match op {
            OP_APPLY => {
                ABI_TEST_HANDLE.with(|cell| cell.set(handle));
                let _ = (*out).write(b"boom");
                STATUS_ERR
            }
            OP_DISPOSE => {
                ABI_TEST_LOG.with(|log| log.borrow_mut().push("dispose".to_string()));
                (*out).write(b"")
            }
            _ => cordis_abi::STATUS_UNSUPPORTED,
        }
    }

    /// 假 ABI 插件入口：OP_APPLY 声称写了超过宿主缓冲的字节，但不实际写入。
    unsafe extern "C" fn abi_fake_call_over_cap(
        _handle: u64,
        op: u32,
        _a: Blob,
        _b: Blob,
        out: *mut Out,
    ) -> i32 {
        if op == OP_APPLY {
            (*out).len = 20_000;
            STATUS_ERR
        } else {
            cordis_abi::STATUS_UNSUPPORTED
        }
    }

    /// apply 的错误读取也必须遵守 `out.len <= cap`，不能把零缓冲当成消息。
    #[tokio::test]
    async fn apply_error_output_is_bounded_and_readable() {
        let app = cordis_core::App::new();
        let plugin: Arc<dyn Plugin> = Arc::new(cordis_core::FnPlugin {
            name: "apply-over-cap",
            key: cordis_core::unique_key("apply-over-cap"),
            inject: cordis_core::Inject::default(),
            validator: None,
            f: Arc::new(|ctx: Ctx, config: Config| {
                apply_via_with_error_format(
                    abi_fake_call_over_cap,
                    ctx,
                    config,
                    "apply-over-cap",
                    cordis_abi::ERROR_FORMAT_V1,
                )
            }),
        });
        let fiber = app.root.plugin(plugin, Config::Null);
        fiber.settle().await;

        assert_eq!(fiber.state(), cordis_core::FiberState::Failed);
        let error = fiber.error().expect("失败的 apply 必须留下错误");
        assert!(
            error.contains("超过宿主缓冲"),
            "必须给出有界诊断，而不是 NUL 填充串：{error:?}"
        );
        assert!(!error.contains('\0'), "错误消息不得包含 NUL：{error:?}");
    }

    unsafe extern "C" fn stream_unused_call(_: u64, _: u32, _: Blob, _: Blob, _: *mut Out) -> i32 {
        STATUS_UNSUPPORTED
    }

    static HOST_TOO_SMALL_READ: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);
    static HOST_TOO_SMALL_WRITE: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);

    unsafe extern "C" fn too_small_once_call(
        _: u64,
        op: u32,
        _: Blob,
        _: Blob,
        out: *mut Out,
    ) -> i32 {
        if op == cordis_abi::OP_VOLATILE_PATHS
            && HOST_TOO_SMALL_READ.fetch_add(1, Ordering::SeqCst) == 0
        {
            (*out).len = 32 * 1024;
            return STATUS_TOO_SMALL;
        }
        (*out).write(br#"{"ret":["x"]}"#)
    }

    unsafe extern "C" fn too_small_always_call(
        _: u64,
        _: u32,
        _: Blob,
        _: Blob,
        out: *mut Out,
    ) -> i32 {
        HOST_TOO_SMALL_WRITE.fetch_add(1, Ordering::SeqCst);
        (*out).len = 32 * 1024;
        STATUS_TOO_SMALL
    }

    #[test]
    fn host_too_small_retries_reads_but_not_writes() {
        HOST_TOO_SMALL_READ.store(0, Ordering::SeqCst);
        HOST_TOO_SMALL_WRITE.store(0, Ordering::SeqCst);

        let value =
            unsafe { call_op_read(too_small_once_call, 0, cordis_abi::OP_VOLATILE_PATHS, "", 0) }
                .expect("只读路径应允许扩容");
        match value {
            CallOutcome::Ok(value) => assert_eq!(value, serde_json::json!(["x"])),
            _ => panic!("只读路径应返回解析后的值"),
        }
        assert_eq!(
            HOST_TOO_SMALL_READ.load(Ordering::SeqCst),
            2,
            "只读路径应重试一次"
        );

        let error = match unsafe { call_op(too_small_always_call, 0, OP_CALLBACK, "", 0) } {
            Err(error) => error,
            Ok(_) => panic!("写路径不得重放"),
        };
        assert!(error.contains("不可重试"), "{error}");
        assert_eq!(
            HOST_TOO_SMALL_WRITE.load(Ordering::SeqCst),
            1,
            "写路径只能调用一次"
        );
    }

    /// 与实现里的私有上限对齐（crate 内用例才读得到它）。
    const READ_CAP: usize = MAX_READ_RETRY_BYTES;

    thread_local! {
        static READ_CLAIM: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
        static READ_CLAIM_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
        static WRITE_CLAIM_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    fn read_claim(claim: usize) {
        READ_CLAIM.set(claim);
        READ_CLAIM_CALLS.set(0);
    }

    /// `{"ret":"xxx…"}`，总长度恰好 `total` 字节（前缀 8 + 后缀 2）。
    fn big_ret_json(total: usize) -> String {
        format!(r#"{{"ret":"{}"}}"#, "x".repeat(total - 10))
    }

    /// 第一次申报 `claim` 字节；第二次回填**恰好 claim 字节**的合法 JSON。
    unsafe extern "C" fn read_claim_call(_: u64, _: u32, _: Blob, _: Blob, out: *mut Out) -> i32 {
        let calls = READ_CLAIM_CALLS.get() + 1;
        READ_CLAIM_CALLS.set(calls);
        let claim = READ_CLAIM.get();
        if calls == 1 {
            (*out).len = claim;
            return STATUS_TOO_SMALL;
        }
        (*out).write(big_ret_json(claim).as_bytes())
    }

    /// 永远申报同一个 `claim` 并报 TOO_SMALL：坏插件的"我就是不够"。
    unsafe extern "C" fn read_claim_forever_call(
        _: u64,
        _: u32,
        _: Blob,
        _: Blob,
        out: *mut Out,
    ) -> i32 {
        READ_CLAIM_CALLS.set(READ_CLAIM_CALLS.get() + 1);
        (*out).len = READ_CLAIM.get();
        STATUS_TOO_SMALL
    }

    /// 写 op 的坏申报：一次就是 usize::MAX。
    unsafe extern "C" fn write_huge_claim_call(
        _: u64,
        _: u32,
        _: Blob,
        _: Blob,
        out: *mut Out,
    ) -> i32 {
        WRITE_CLAIM_CALLS.set(WRITE_CLAIM_CALLS.get() + 1);
        (*out).len = usize::MAX;
        STATUS_TOO_SMALL
    }

    /// O8b-1 的上限取数与边界（与 `READ_CAP`/`read_retry_cap` 一对一）。
    #[test]
    fn read_retry_cap_rejects_out_of_range_and_oversize_lengths() {
        assert_eq!(
            read_retry_cap(0),
            Ok(16 * 1024),
            "申报小于初始缓冲时仍按 16 KiB 起步"
        );
        assert_eq!(read_retry_cap(200 * 1024), Ok(200 * 1024));
        assert_eq!(read_retry_cap(READ_CAP), Ok(READ_CAP), "上限本身可用");
        assert!(read_retry_cap(READ_CAP + 1).is_err(), "上限 +1 必须被拒");
        assert!(
            read_retry_cap(isize::MAX as usize).is_err(),
            "isize::MAX 也要被拒（真实 OOM 会 abort）"
        );
        assert!(read_retry_cap(usize::MAX).is_err(), "usize::MAX 必须被拒");
    }

    /// O8b-1 a. 上限内的扩容重试照常工作，并拿回**完整**内容。
    #[test]
    fn read_retry_within_the_cap_returns_the_full_payload() {
        read_claim(200 * 1024);
        let value =
            unsafe { call_op_read(read_claim_call, 0, cordis_abi::OP_VOLATILE_PATHS, "", 0) }
                .expect("上限内的只读扩容应成功");
        match value {
            CallOutcome::Ok(value) => assert_eq!(
                value.as_str().map(str::len),
                Some(200 * 1024 - 10),
                "必须拿回完整内容，而不是被截断/补零的缓冲"
            ),
            _ => panic!("只读路径应返回解析后的值"),
        }
        assert_eq!(READ_CLAIM_CALLS.get(), 2, "只读路径应重试一次");
    }

    /// O8b-1 b. 上限 +1 被拒：拒绝发生在**分配之前**，文案点名上限。
    #[test]
    fn read_retry_past_the_cap_is_rejected_before_allocating() {
        read_claim(READ_CAP + 1);
        let error = match unsafe {
            call_op_read(
                read_claim_forever_call,
                0,
                cordis_abi::OP_VOLATILE_PATHS,
                "",
                0,
            )
        } {
            Err(error) => error,
            Ok(_) => panic!("超过只读扩容上限必须拒绝"),
        };
        assert!(
            error.contains("扩容上限"),
            "必须点名上限；得到 {} 字节的诊断",
            error.len()
        );
        assert!(!error.contains('\0'));
        assert_eq!(READ_CLAIM_CALLS.get(), 1, "超限时不得再调一次（不扩容）");
    }

    /// O8b-1 c. `usize::MAX` / `isize::MAX` 被拒且不 panic（也不真去分配）。
    #[test]
    fn absurd_read_claims_are_rejected_instead_of_panicking() {
        for claim in [usize::MAX, isize::MAX as usize] {
            read_claim(claim);
            let error = match unsafe {
                call_op_read(
                    read_claim_forever_call,
                    0,
                    cordis_abi::OP_VOLATILE_PATHS,
                    "",
                    0,
                )
            } {
                Err(error) => error,
                Ok(_) => panic!("{claim} 字节的申报必须被拒绝"),
            };
            assert!(
                error.contains("拒绝分配"),
                "{claim} 字节的申报必须被明确拒绝；得到 {} 字节的诊断",
                error.len()
            );
            assert!(!error.contains('\0'));
            assert_eq!(READ_CLAIM_CALLS.get(), 1);
        }
    }

    /// O8b-1 d. 第二次仍是 TOO_SMALL 时给出可读原因，而不是全零缓冲的 NUL 串。
    #[test]
    fn a_second_too_small_reports_a_readable_reason() {
        read_claim(2 * 1024);
        let error = match unsafe {
            call_op_read(
                read_claim_forever_call,
                0,
                cordis_abi::OP_VOLATILE_PATHS,
                "",
                0,
            )
        } {
            Err(error) => error,
            Ok(_) => panic!("二次 TOO_SMALL 必须报错"),
        };
        assert!(
            !error.contains('\0'),
            "错误消息不得是 NUL 填充串（{} 字节）",
            error.len()
        );
        assert!(!error.is_empty(), "必须给出原因");
        assert!(error.contains("仍"), "必须说清扩容之后仍然不够：{error}");
    }

    /// O8b-1 e. 写 op 遇到巨量申报仍然只调用一次（不重放、不分配）。
    #[test]
    fn huge_write_claims_still_call_the_plugin_exactly_once() {
        WRITE_CLAIM_CALLS.set(0);
        let error = match unsafe { call_op(write_huge_claim_call, 0, OP_CALLBACK, "", 0) } {
            Err(error) => error,
            Ok(_) => panic!("写路径不得重放"),
        };
        assert!(error.contains("不可重试"), "{error}");
        assert!(!error.contains('\0'));
        assert_eq!(WRITE_CLAIM_CALLS.get(), 1, "写路径只能调用一次");
    }

    #[test]
    fn closed_next_preserves_frame_and_task_before_drain() {
        let app = cordis_core::App::new();
        let handle = register_instance_with_keepalive(
            app.root.clone(),
            stream_unused_call,
            None,
            cordis_abi::ERROR_FORMAT_V1,
        )
        .unwrap();
        let instance = instances().lock().unwrap().get(handle).unwrap();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let tail_calls = calls.clone();
        let frame = Next::some(move |_| {
            tail_calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { None })
        });
        assert!(insert_dispatch(
            &instance,
            1,
            DispatchRecord {
                sink: None,
                frame: Some(frame),
                parent: None,
                outbound: None,
                stream: None,
            }
        )
        .is_ok());
        {
            let _table = instance.dispatches.lock().unwrap();
            instance.closed.store(true, Ordering::SeqCst);
        }
        let mut buffer = [0u8; 4096];
        let mut out = Out {
            ptr: buffer.as_mut_ptr(),
            cap: buffer.len(),
            len: 0,
        };
        let status = unsafe { host_ctx_next(handle, 1, Blob::new(b"[]"), 9, &mut out) };
        let payload: Value = serde_json::from_slice(&buffer[..out.len]).unwrap_or(Value::Null);
        let (frame_preserved, task_absent) = {
            let table = instance.dispatches.lock().unwrap();
            let record = table.get(&1).unwrap();
            (
                record.frame.is_some(),
                instance.tasks.lock().unwrap().is_empty(),
            )
        };
        unregister_instance(handle);
        assert_eq!(status, STATUS_ERR, "closed next admitted work: {payload}");
        assert_eq!(payload["code"], "host_lifecycle");
        assert_eq!(payload["category"], "lifecycle");
        assert_eq!(payload["certainty"], "not_executed");
        assert!(frame_preserved, "拒绝不得消费有效 Next frame");
        assert!(task_absent, "拒绝不得赋值 task");
        assert_eq!(calls.load(Ordering::SeqCst), 0, "拒绝不得执行尾链");
    }

    #[test]
    fn closed_dispatch_admission_precedes_capacity() {
        let app = cordis_core::App::new();
        set_host_limits_for(
            &app.root,
            HostLimits {
                max_dispatches_per_instance: 1,
                ..HostLimits::default()
            },
        )
        .unwrap();
        let handle = register_instance_with_keepalive(
            app.root.clone(),
            stream_unused_call,
            None,
            cordis_abi::ERROR_FORMAT_V1,
        )
        .unwrap();
        let instance = instances().lock().unwrap().get(handle).unwrap();
        let record = || DispatchRecord {
            sink: None,
            frame: Some(Next::none()),
            parent: None,
            outbound: None,
            stream: None,
        };
        assert!(insert_dispatch(&instance, 1, record()).is_ok());
        {
            let _table = instance.dispatches.lock().unwrap();
            instance.closed.store(true, Ordering::SeqCst);
        }
        let result = insert_dispatch(&instance, 2, record());
        let count = instance.dispatches.lock().unwrap().len();
        unregister_instance(handle);
        let (payload, rejected) = result.expect_err("closed 必须拒绝记录");
        assert_eq!(payload.code, "host_lifecycle", "关闭优先于容量");
        assert_eq!(payload.category, "lifecycle");
        assert_eq!(payload.certainty, "not_executed");
        assert_eq!(count, 1);
        assert!(rejected.frame.is_some(), "拒绝必须归还 waterfall frame");
    }

    /// 流式宿主函数的业务错误也必须按插件能力位写结构化载荷。
    #[test]
    fn stream_host_errors_are_structured_for_capable_plugins() {
        let app = cordis_core::App::new();
        let handle = register_instance_with_keepalive(
            app.root.clone(),
            stream_unused_call,
            None,
            cordis_abi::ERROR_FORMAT_V1,
        )
        .expect("默认容量下实例必须能登记");
        {
            let instances = instances().lock().unwrap();
            let instance = instances.get(handle).expect("实例必须已登记");
            assert!(
                insert_dispatch(
                    &instance,
                    1,
                    DispatchRecord {
                        sink: None,
                        frame: None,
                        parent: None,
                        outbound: None,
                        stream: None,
                    },
                )
                .is_ok(),
                "默认容量下派发记录必须能插入"
            );
        }

        let mut buffer = vec![0u8; 4096];
        let mut out = Out {
            ptr: buffer.as_mut_ptr(),
            cap: buffer.len(),
            len: 0,
        };
        let status = unsafe { host_stream_close(handle, 1, &mut out) };
        assert_eq!(status, STATUS_ERR);
        buffer.truncate(out.len);
        let payload = cordis_abi::ErrorPayload::from_json(&String::from_utf8(buffer).unwrap())
            .expect("能力位为 1 时流错误必须是结构化的");
        assert_eq!(payload.code, "host_call");
        assert_eq!(payload.category, "protocol");
        assert!(payload.error.contains("不是流"), "{}", payload.error);

        unregister_instance(handle);
    }

    /// 生命周期类宿主失败（例如 unknown / duplicate fulfill）必须带生命周期分类，
    /// 不能和普通协议错误共用同一个哨兵。
    #[test]
    fn fulfill_failures_are_structured_as_lifecycle() {
        let app = cordis_core::App::new();
        let handle = register_instance_with_keepalive(
            app.root.clone(),
            stream_unused_call,
            None,
            cordis_abi::ERROR_FORMAT_V1,
        )
        .expect("默认容量下实例必须能登记");
        let mut buffer = vec![0u8; 4096];
        let mut out = Out {
            ptr: buffer.as_mut_ptr(),
            cap: buffer.len(),
            len: 0,
        };
        let status = unsafe { host_ctx_fulfill(handle, 999, Blob::new(b"null"), &mut out) };
        assert_eq!(status, STATUS_ERR);
        buffer.truncate(out.len);
        let payload = cordis_abi::ErrorPayload::from_json(&String::from_utf8(buffer).unwrap())
            .expect("能力位为 1 时 fulfill 错误必须是结构化的");
        assert_eq!(payload.code, "host_fulfill");
        assert_eq!(payload.category, "lifecycle");
        assert!(payload.error.contains("不存在"), "{}", payload.error);

        unregister_instance(handle);
    }

    /// 宿主函数的预解析失败必须报协议错误，不能把非法 JSON 静默降级成 `null`。
    #[test]
    fn malformed_host_arguments_are_protocol_errors_not_null() {
        let mut buffer = vec![0u8; 4096];
        let mut out = Out {
            ptr: buffer.as_mut_ptr(),
            cap: buffer.len(),
            len: 0,
        };

        let status = unsafe {
            host_ctx_use(
                0,
                Blob::new(b"@native:missing"),
                Blob::new(b"not-json"),
                &mut out,
            )
        };
        assert_eq!(status, STATUS_ERR);
        let text = String::from_utf8_lossy(&buffer[..out.len]).to_string();
        assert!(text.contains("不是合法 JSON"), "{text}");

        out.len = 0;
        let status = unsafe {
            host_ctx_call_iface(
                0,
                Blob::new(b"svc"),
                1,
                Blob::new(b"method"),
                Blob::new(b"not-json"),
                0,
                &mut out,
            )
        };
        assert_eq!(status, STATUS_ERR);
        let text = String::from_utf8_lossy(&buffer[..out.len]).to_string();
        assert!(text.contains("不是合法 JSON"), "{text}");

        out.len = 0;
        let status = unsafe { host_ctx_next(0, 1, Blob::new(b"not-json"), 0, &mut out) };
        assert_eq!(status, STATUS_ERR);
        let text = String::from_utf8_lossy(&buffer[..out.len]).to_string();
        assert!(text.contains("不是合法 JSON"), "{text}");

        out.len = 0;
        let status =
            unsafe { dispatch_impl(0, Blob::new(b"event"), Blob::new(b"not-json"), 0, &mut out) };
        assert_eq!(status, STATUS_ERR);
        let text = String::from_utf8_lossy(&buffer[..out.len]).to_string();
        assert!(text.contains("不是合法 JSON"), "{text}");
    }

    /// M1-1 守卫（非红：修复前后都应通过）：apply 失败的实例**不会**收到 OP_DISPOSE，
    /// 且句柄不会泄漏。
    ///
    /// 句柄回收由 `apply_via` 里那个"注册在 OP_APPLY 之前"的 effect 单点负责——
    /// fiber 的失败清理（`Fiber::fail` → `dispose_effects`）会跑到它。
    #[tokio::test]
    async fn failed_apply_never_receives_dispose_and_leaks_no_handle() {
        ABI_TEST_LOG.with(|log| log.borrow_mut().clear());
        ABI_TEST_HANDLE.with(|cell| cell.set(u64::MAX));

        let app = cordis_core::App::new();
        let plugin: Arc<dyn Plugin> = Arc::new(cordis_core::FnPlugin {
            name: "abi-test-failing",
            key: cordis_core::unique_key("abi-test-failing"),
            inject: cordis_core::Inject::default(),
            validator: None,
            f: Arc::new(|ctx: Ctx, config: Config| {
                apply_via(abi_fake_call_failing, ctx, config, "abi-test-failing")
            }),
        });
        let fiber = app.root.plugin(plugin, Config::Null);
        fiber.settle().await;
        assert_eq!(
            fiber.state(),
            cordis_core::FiberState::Failed,
            "apply 失败应使 fiber 进入 FAILED"
        );
        assert!(fiber.error().unwrap().contains("boom"));

        let log = ABI_TEST_LOG.with(|log| log.borrow().clone());
        assert!(
            log.is_empty(),
            "失败路径不得投递 OP_DISPOSE / OP_CLEANUP：{log:?}"
        );

        let handle = ABI_TEST_HANDLE.with(|cell| cell.get());
        assert_ne!(handle, u64::MAX, "插件应被调用过（拿到过句柄）");
        assert!(
            instances().lock().unwrap().get(handle).is_none(),
            "apply 失败后句柄必须已回收（由 dispose effect 单点负责）"
        );
    }

    // ------------------------------------------------------ M2-1：ctx_require

    /// 假 ABI 插件入口（M2-1）：
    /// - OP_APPLY：记录 handle，并经 `host_ctx_effect` 注册一个 cleanup；
    /// - OP_CLEANUP（卸载期间触发）：用同一个 handle 分别走 `ctx_get` 与 `ctx_require`
    ///   读 `abi-require-svc`，两者都记进日志（测试 2 用它看拆除期的差异）；
    /// - OP_DISPOSE：记录顺序。
    ///
    /// 与 M1-1 的 `abi_fake_call` 同规矩：fake `CallFn` 内禁止 assert，只落日志。
    unsafe extern "C" fn abi_require_probe_call(
        handle: u64,
        op: u32,
        _a: Blob,
        _b: Blob,
        out: *mut Out,
    ) -> i32 {
        match op {
            OP_APPLY => {
                ABI_TEST_HANDLE.with(|cell| cell.set(handle));
                let mut buf = [0u8; 64];
                let mut cleanup_out = Out {
                    ptr: buf.as_mut_ptr(),
                    cap: buf.len(),
                    len: 0,
                };
                let status = host_ctx_effect(handle, 7, &mut cleanup_out);
                if status != STATUS_OK {
                    return status;
                }
                (*out).write(b"")
            }
            cordis_abi::OP_CLEANUP => {
                let tolerant = probe_read(handle, b"abi-require-svc", host_ctx_get);
                let strict = probe_read(handle, b"abi-require-svc", host_ctx_require);
                ABI_TEST_LOG.with(|log| {
                    log.borrow_mut().push(format!("cleanup get={tolerant}"));
                    log.borrow_mut().push(format!("cleanup require={strict}"));
                });
                (*out).write(b"")
            }
            OP_DISPOSE => {
                ABI_TEST_LOG.with(|log| log.borrow_mut().push("dispose".to_string()));
                (*out).write(b"")
            }
            _ => cordis_abi::STATUS_UNSUPPORTED,
        }
    }

    /// 用给定的宿主读函数读一个服务，返回 `status:text` 的可读快照。
    unsafe fn probe_read(
        handle: u64,
        name: &[u8],
        read: unsafe extern "C" fn(u64, Blob, *mut Out) -> i32,
    ) -> String {
        let mut buf = [0u8; 1024];
        let mut read_out = Out {
            ptr: buf.as_mut_ptr(),
            cap: buf.len(),
            len: 0,
        };
        let status = read(handle, Blob::new(name), &mut read_out);
        let text = std::str::from_utf8(std::slice::from_raw_parts(
            read_out.ptr,
            read_out.len.min(read_out.cap),
        ))
        .unwrap_or("<non-utf8>");
        format!("{status}:{text}")
    }

    type ProvideFn = Arc<dyn Fn(&Ctx) -> Result<()> + Send + Sync>;

    /// 造一对"外部提供者 + ABI 消费者"并收敛到 ACTIVE。
    async fn mount_require_pair(
        inject: cordis_core::Inject,
        provide: ProvideFn,
        call: CallFn,
    ) -> (
        cordis_core::App,
        Ctx,
        Arc<cordis_core::Fiber>,
        Arc<cordis_core::Fiber>,
    ) {
        let app = cordis_core::App::new();
        let outer = provide.clone();
        let provider = cordis_core::plugin_fn(
            "abi-require-provider",
            cordis_core::Inject::default(),
            move |ctx: Ctx, _cfg| {
                let provide = outer.clone();
                Box::pin(async move { provide(&ctx) })
            },
        );
        let provider_fiber = app.root.plugin(provider, Config::Null);
        provider_fiber.settle().await;
        assert_eq!(
            provider_fiber.state(),
            cordis_core::FiberState::Active,
            "提供者应就绪"
        );

        let plugin: Arc<dyn Plugin> = Arc::new(cordis_core::FnPlugin {
            name: "abi-require-consumer",
            key: cordis_core::unique_key("abi-require-consumer"),
            inject,
            validator: None,
            f: Arc::new(move |ctx: Ctx, config: Config| {
                apply_via(call, ctx, config, "abi-require-consumer")
            }),
        });
        let consumer = app.root.plugin(plugin, Config::Null);
        consumer.settle().await;
        assert_eq!(
            consumer.state(),
            cordis_core::FiberState::Active,
            "消费者应装载成功"
        );
        let root = app.root.clone();
        (app, root, provider_fiber, consumer)
    }

    /// M2-1（红→绿）：`ctx_require` 必须**要求声明 inject**。
    ///
    /// 现状（探针实测）：ABI 插件未声明任何 inject 也能读到服务
    /// （`status=0 text={"value":{"n":1}}`）——`ctx_get` 是宽容探测，
    /// 把"没声明依赖"和"服务不存在"混成了同一个 `missing`。
    #[tokio::test]
    async fn abi_require_enforces_inject_check() {
        ABI_TEST_LOG.with(|log| log.borrow_mut().clear());
        ABI_TEST_HANDLE.with(|cell| cell.set(u64::MAX));

        let (_app, _root, _provider, consumer) = mount_require_pair(
            cordis_core::Inject::default(),
            Arc::new(|ctx: &Ctx| {
                ctx.provide("abi-require-svc", Dyn::new(ExternValue(json!({ "n": 1 }))))?;
                Ok(())
            }),
            abi_require_probe_call,
        )
        .await;
        let handle = ABI_TEST_HANDLE.with(|cell| cell.get());

        // 宽容读的行为不变：可选探测仍然可用。
        let tolerant = unsafe { probe_read(handle, b"abi-require-svc", host_ctx_get) };
        assert!(
            tolerant.starts_with("0:"),
            "ctx_get 必须保持宽容探测语义：{tolerant}"
        );
        assert!(tolerant.contains(r#""n":1"#), "宽容读应拿到值：{tolerant}");

        // 必需读：未声明 inject ⇒ 明确报错，而不是伪装成 missing。
        let strict = unsafe { probe_read(handle, b"abi-require-svc", host_ctx_require) };
        assert!(
            strict.contains(r#"cannot get property "abi-require-svc" without inject"#),
            "ctx_require 必须要求 inject 声明：{strict}"
        );

        consumer.dispose().await;
    }

    /// M2-1（红→绿）：`ctx_require` 读 **committed 快照**——组件在提供者拆除期间
    /// 仍能读到正在离开的依赖；同一个窗口里 `ctx_get` 只能给出 `missing`。
    ///
    /// 现状（探针实测）：提供者拆除 → 消费者卸载期间
    /// `cleanup ctx_get: status=0 text={"missing":true}`——**拆除期读依赖在 ABI 侧不可达**，
    /// 而"卸载时回调依赖做收尾"恰恰是最需要它的时刻。
    #[tokio::test]
    async fn abi_require_reads_committed_during_departing_teardown() {
        ABI_TEST_LOG.with(|log| log.borrow_mut().clear());
        ABI_TEST_HANDLE.with(|cell| cell.set(u64::MAX));

        let (_app, _root, provider, consumer) = mount_require_pair(
            cordis_core::Inject::of(&["abi-require-svc"]),
            Arc::new(|ctx: &Ctx| {
                ctx.provide("abi-require-svc", Dyn::new(ExternValue(json!({ "n": 1 }))))?;
                Ok(())
            }),
            abi_require_probe_call,
        )
        .await;

        // 拆掉**提供者**：消费者会被级联卸载，其 cleanup 在卸载期间跑。
        provider.dispose().await;
        assert_ne!(
            consumer.state(),
            cordis_core::FiberState::Active,
            "依赖离开后消费者不应保持 Active"
        );

        let log = ABI_TEST_LOG.with(|log| log.borrow().clone());
        let get = log
            .iter()
            .find(|line| line.starts_with("cleanup get="))
            .expect("cleanup 应已执行")
            .clone();
        let require = log
            .iter()
            .find(|line| line.starts_with("cleanup require="))
            .expect("cleanup 应已执行")
            .clone();
        assert!(
            get.contains(r#""missing":true"#),
            "宽容读在提供者 UNLOADING 后只能 missing：{log:?}"
        );
        assert!(
            require.starts_with("cleanup require=0:") && require.contains(r#""n":1"#),
            "必需读必须从 committed 快照读到正在离开的依赖：{log:?}"
        );
        assert!(
            log.contains(&"dispose".to_string()),
            "OP_DISPOSE 仍须最后投递：{log:?}"
        );
    }

    /// M2-1：`ctx_require` 与 `ctx_get` **读到同一个载荷、看到同一条可见性契约**。
    ///
    /// 读取语义可以不同（宽容 vs 严格），但"什么对 ABI 可见"必须只有一个答案；
    /// 否则同一个服务会随入口时而可读时而不可读。
    #[tokio::test]
    async fn abi_require_shares_payload_and_visibility_with_get() {
        ABI_TEST_LOG.with(|log| log.borrow_mut().clear());
        ABI_TEST_HANDLE.with(|cell| cell.set(u64::MAX));

        let (_app, _root, _provider, consumer) = mount_require_pair(
            cordis_core::Inject::of(&["abi-require-svc", "abi-require-opaque"]),
            Arc::new(|ctx: &Ctx| {
                ctx.provide("abi-require-svc", Dyn::new(ExternValue(json!({ "n": 1 }))))?;
                // 对 ABI 不可见的服务：既不是 ExternValue 也不是带方法的接口。
                ctx.provide("abi-require-opaque", Dyn::new(Config::Null))?;
                Ok(())
            }),
            abi_require_probe_call,
        )
        .await;
        let handle = ABI_TEST_HANDLE.with(|cell| cell.get());

        let get = unsafe { probe_read(handle, b"abi-require-svc", host_ctx_get) };
        let require = unsafe { probe_read(handle, b"abi-require-svc", host_ctx_require) };
        assert!(
            get.starts_with("0:") && require.starts_with("0:"),
            "两者都应成功：{get} / {require}"
        );
        assert_eq!(
            get.split_once(':').unwrap().1,
            require.split_once(':').unwrap().1,
            "载荷必须完全一致"
        );

        let get_opaque = unsafe { probe_read(handle, b"abi-require-opaque", host_ctx_get) };
        let require_opaque = unsafe { probe_read(handle, b"abi-require-opaque", host_ctx_require) };
        assert!(
            get_opaque.contains("对 ABI 不可见"),
            "宽容读也受可见性契约约束：{get_opaque}"
        );
        assert_eq!(
            get_opaque.split_once(':').unwrap().1,
            require_opaque.split_once(':').unwrap().1,
            "可见性错误必须是同一条：{get_opaque} / {require_opaque}"
        );

        consumer.dispose().await;
    }

    // ------------------------------------------------------ M2-2：ctx_set

    /// 假 ABI 插件入口（M2-2）：OP_APPLY 里按脚本走一遍 `ctx.set`，结果全部落日志。
    ///
    /// 脚本：provide `abi-set-svc` → set 它自己 → set 一个**别人提供的**服务
    /// (`abi-set-foreign`) → set 一个**从未 provide** 的名字 (`abi-set-missing`)。
    unsafe extern "C" fn abi_set_probe_call(
        handle: u64,
        op: u32,
        _a: Blob,
        _b: Blob,
        out: *mut Out,
    ) -> i32 {
        match op {
            OP_APPLY => {
                ABI_TEST_HANDLE.with(|cell| cell.set(handle));
                let mut provide_buf = [0u8; 64];
                let mut provide_out = Out {
                    ptr: provide_buf.as_mut_ptr(),
                    cap: provide_buf.len(),
                    len: 0,
                };
                let provided = host_ctx_provide(
                    handle,
                    Blob::new(b"abi-set-svc"),
                    Blob::new(b"{\"v\":1}"),
                    &mut provide_out,
                );
                ABI_TEST_LOG.with(|log| log.borrow_mut().push(format!("provide:{provided}")));

                ABI_TEST_LOG.with(|log| {
                    log.borrow_mut().push(format!(
                        "set-own:{}",
                        probe_set(handle, b"abi-set-svc", b"{\"v\":2}")
                    ));
                    log.borrow_mut().push(format!(
                        "set-foreign:{}",
                        probe_set(handle, b"abi-set-foreign", b"{\"x\":1}")
                    ));
                    log.borrow_mut().push(format!(
                        "set-missing:{}",
                        probe_set(handle, b"abi-set-missing", b"{\"x\":1}")
                    ));
                });
                (*out).write(b"")
            }
            OP_DISPOSE => {
                ABI_TEST_LOG.with(|log| log.borrow_mut().push("dispose".to_string()));
                (*out).write(b"")
            }
            _ => cordis_abi::STATUS_UNSUPPORTED,
        }
    }

    /// `ctx.set` 的调用快照：`status:text`。
    unsafe fn probe_set(handle: u64, name: &[u8], value: &[u8]) -> String {
        let mut buf = [0u8; 512];
        let mut set_out = Out {
            ptr: buf.as_mut_ptr(),
            cap: buf.len(),
            len: 0,
        };
        let status = host_ctx_set(handle, Blob::new(name), Blob::new(value), &mut set_out);
        let text = std::str::from_utf8(std::slice::from_raw_parts(
            set_out.ptr,
            set_out.len.min(set_out.cap),
        ))
        .unwrap_or("<non-utf8>");
        format!("{status}:{text}")
    }

    /// M2-2（红→绿）：`ctx_set` 覆盖**自己提供的**服务值——后续任何读者
    /// （含 ABI 侧）看到的都是新值。
    ///
    /// 对齐上游 `reflect.ts:164-175`：`set` 换的是**值**，不改变"谁提供"，
    /// 因此**不通知依赖者**重载（上游同样如此——epoch 只关心实现者身份）。
    #[tokio::test]
    async fn abi_set_overwrites_own_service() {
        ABI_TEST_LOG.with(|log| log.borrow_mut().clear());
        ABI_TEST_HANDLE.with(|cell| cell.set(u64::MAX));

        let (_app, _root, _provider, consumer) = mount_require_pair(
            cordis_core::Inject::default(),
            Arc::new(|_ctx: &Ctx| Ok(())),
            abi_set_probe_call,
        )
        .await;
        let handle = ABI_TEST_HANDLE.with(|cell| cell.get());

        let log = ABI_TEST_LOG.with(|log| log.borrow().clone());
        assert!(
            log.contains(&"provide:0".to_string()),
            "provide 应成功：{log:?}"
        );
        assert!(
            log.contains(&"set-own:0:".to_string()),
            "覆盖自己提供的服务应成功：{log:?}"
        );

        // 覆盖后的值必须对 ABI 读者可见（且只有一份值：没有产生第二条实现）。
        let read = unsafe { probe_read(handle, b"abi-set-svc", host_ctx_get) };
        assert_eq!(
            read, r#"0:{"value":{"v":2}}"#,
            "set 之后的读取必须是新值：{read}"
        );

        consumer.dispose().await;
    }

    /// M2-2（红→绿）：`ctx_set` 的两条硬约束与内核一致——
    /// 没 provide 过 → "without provide"；不是自己提供的 → "in multiple fibers"。
    #[tokio::test]
    async fn abi_set_rejects_foreign_and_unprovisioned_writes() {
        ABI_TEST_LOG.with(|log| log.borrow_mut().clear());
        ABI_TEST_HANDLE.with(|cell| cell.set(u64::MAX));

        let (_app, _root, _provider, consumer) = mount_require_pair(
            cordis_core::Inject::default(),
            Arc::new(|ctx: &Ctx| {
                // 别人提供的服务：ABI 插件 set 它必须被拒。
                ctx.provide("abi-set-foreign", Dyn::new(ExternValue(json!({ "x": 0 }))))?;
                Ok(())
            }),
            abi_set_probe_call,
        )
        .await;

        let log = ABI_TEST_LOG.with(|log| log.borrow().clone());
        let foreign = log
            .iter()
            .find(|line| line.starts_with("set-foreign:"))
            .expect("脚本应走到这里")
            .clone();
        assert!(
            foreign.contains(r#"cannot set property "abi-set-foreign" in multiple fibers"#),
            "不得覆盖别人提供的服务：{log:?}"
        );
        let missing = log
            .iter()
            .find(|line| line.starts_with("set-missing:"))
            .expect("脚本应走到这里")
            .clone();
        assert!(
            missing.contains(r#"cannot set property "abi-set-missing" without provide"#),
            "不得凭空造出服务：{log:?}"
        );

        consumer.dispose().await;
    }

    #[test]
    fn structured_plugin_errors_are_decoded() {
        let text = cordis_abi::ErrorPayload::new("boom")
            .with_code("E_BOOM")
            .with_category("business")
            .to_json();
        let payload = decode_plugin_error(cordis_abi::ERROR_FORMAT_V1, &text);
        assert_eq!(payload.error, "boom");
        assert_eq!(payload.code, "E_BOOM");
        assert_eq!(payload.category, "business");
    }

    #[test]
    fn plugin_text_errors_survive_without_capability() {
        let payload = decode_plugin_error(0, r#"{"error":"boom"}"#);
        assert_eq!(
            payload.error, r#"{"error":"boom"}"#,
            "没有能力位时不得私自解析结构"
        );
    }

    #[test]
    fn invalid_structured_plugin_errors_fall_back_to_text() {
        let payload = decode_plugin_error(cordis_abi::ERROR_FORMAT_V1, "plain error");
        assert_eq!(payload.error, "plain error");
    }

    #[test]
    fn host_failures_are_structured_only_for_capable_plugins() {
        let mut buffer = vec![0u8; 4096];
        let mut out = Out {
            ptr: buffer.as_mut_ptr(),
            cap: buffer.len(),
            len: 0,
        };
        let status = unsafe {
            fail_to(
                &mut out,
                cordis_abi::ERROR_FORMAT_V1,
                "boom",
                "E_HOST",
                "lifecycle",
            )
        };
        assert_eq!(status, STATUS_ERR);
        buffer.truncate(out.len);
        let payload =
            cordis_abi::ErrorPayload::from_json(&String::from_utf8(buffer).unwrap()).unwrap();
        assert_eq!(payload.error, "boom");
        assert_eq!(payload.code, "E_HOST");

        let mut buffer = vec![0u8; 4096];
        let mut out = Out {
            ptr: buffer.as_mut_ptr(),
            cap: buffer.len(),
            len: 0,
        };
        let status = unsafe { fail_to(&mut out, 0, "boom", "E_HOST", "lifecycle") };
        assert_eq!(status, STATUS_ERR);
        buffer.truncate(out.len);
        assert_eq!(String::from_utf8(buffer).unwrap(), "boom");

        // 结构化放不下 ⇒ 纯文本；纯文本也放不下 ⇒ 截断到 cap，绝不回填 len > cap。
        let mut buffer = vec![0u8; 8];
        let mut out = Out {
            ptr: buffer.as_mut_ptr(),
            cap: buffer.len(),
            len: 0,
        };
        let status = unsafe {
            fail_to(
                &mut out,
                cordis_abi::ERROR_FORMAT_V1,
                "abcdefghij",
                "E_HOST",
                "lifecycle",
            )
        };
        assert_eq!(status, STATUS_ERR);
        assert!(out.len <= 8, "out.len 不得超过 cap：{}", out.len);
        buffer.truncate(out.len);
        assert_eq!(String::from_utf8(buffer).unwrap(), "abcde…");
    }

    #[test]
    fn chinese_fail_message_truncates_on_a_char_boundary() {
        let mut buffer = vec![0u8; 8];
        let mut out = Out {
            ptr: buffer.as_mut_ptr(),
            cap: buffer.len(),
            len: 0,
        };
        let status = unsafe {
            fail_to(
                &mut out,
                cordis_abi::ERROR_FORMAT_V1,
                "中文错误",
                "E_HOST",
                "business",
            )
        };
        assert_eq!(status, STATUS_ERR);
        assert!(out.len <= 8, "out.len 不得超过 cap：{}", out.len);
        buffer.truncate(out.len);
        let text = String::from_utf8(buffer).expect("截断后必须仍是合法 UTF-8");
        assert!(text.ends_with('…'), "必须有截断标记：{text}");
    }

    unsafe extern "C" fn structured_call(_: u64, _: u32, _: Blob, _: Blob, out: *mut Out) -> i32 {
        let payload = cordis_abi::ErrorPayload::new("boom")
            .with_code("E_PLUGIN")
            .to_json();
        (*out).write(payload.as_bytes());
        STATUS_ERR
    }

    #[test]
    fn call_op_decodes_plugin_errors_only_with_capability() {
        let outcome = unsafe {
            call_op(
                structured_call,
                0,
                OP_CALLBACK,
                "",
                cordis_abi::ERROR_FORMAT_V1,
            )
        }
        .unwrap();
        match outcome {
            CallOutcome::Err(payload) => {
                assert_eq!(payload.error, "boom");
                assert_eq!(payload.code, "E_PLUGIN");
            }
            _ => panic!("必须是插件错误"),
        }

        let outcome = unsafe { call_op(structured_call, 0, OP_CALLBACK, "", 0) }.unwrap();
        match outcome {
            CallOutcome::Err(payload) => {
                assert!(
                    payload.error.contains("\"error\":\"boom\""),
                    "没有能力位时必须保留原始 JSON 文本：{}",
                    payload.error
                );
            }
            _ => panic!("必须是插件错误"),
        }
    }

    static CALL_CODE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    unsafe extern "C" fn coded_call(_: u64, _: u32, _: Blob, _: Blob, out: *mut Out) -> i32 {
        use std::sync::atomic::Ordering;
        let code = if CALL_CODE.fetch_add(1, Ordering::SeqCst) == 0 {
            "E_ONE"
        } else {
            "E_TWO"
        };
        let payload = cordis_abi::ErrorPayload::new("same message")
            .with_code(code)
            .to_json();
        (*out).write(payload.as_bytes());
        STATUS_ERR
    }

    #[test]
    fn call_plugin_detailed_distinguishes_codes_with_the_same_message() {
        use std::sync::atomic::Ordering;
        CALL_CODE.store(0, Ordering::SeqCst);
        let first = unsafe { call_plugin_detailed(coded_call, 0, OP_CALLBACK, "", 1) }.unwrap_err();
        let second =
            unsafe { call_plugin_detailed(coded_call, 0, OP_CALLBACK, "", 1) }.unwrap_err();
        assert_eq!(first.error, second.error, "消息必须保持相同");
        assert_ne!(first.code, second.code, "调用者必须能靠 code 区分");
        assert_eq!(
            unsafe { call_plugin(coded_call, 0, OP_CALLBACK, "", 1) }.unwrap_err(),
            "same message"
        );
    }

    unsafe extern "C" fn over_cap_call(_: u64, _: u32, _: Blob, _: Blob, out: *mut Out) -> i32 {
        (*out).len = 20_000;
        STATUS_ERR
    }

    #[test]
    fn oversized_plugin_output_is_bounded_and_readable() {
        let message = unsafe { call_plugin(over_cap_call, 0, OP_CALLBACK, "", 0) }.unwrap_err();
        assert!(
            message.contains("超过宿主缓冲"),
            "必须给出有界诊断：{message}"
        );
        assert!(!message.contains('\0'), "不得把零缓冲当成消息");
    }

    #[test]
    fn twenty_k_plugin_message_is_truncated_to_the_incoming_cap() {
        let message = "x".repeat(20_000);
        let mut buffer = vec![0u8; 16 * 1024];
        let mut out = Out {
            ptr: buffer.as_mut_ptr(),
            cap: buffer.len(),
            len: 0,
        };
        let status = unsafe {
            fail_to(
                &mut out,
                cordis_abi::ERROR_FORMAT_V1,
                &message,
                "E_HOST",
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

    #[test]
    fn tiny_host_buffer_is_bounded_and_not_a_nul_string() {
        let mut buffer = vec![0u8; 2];
        let mut out = Out {
            ptr: buffer.as_mut_ptr(),
            cap: buffer.len(),
            len: 0,
        };
        let status = unsafe {
            fail_to(
                &mut out,
                cordis_abi::ERROR_FORMAT_V1,
                "中文",
                "E_HOST",
                "business",
            )
        };
        assert_eq!(status, STATUS_ERR);
        assert!(out.len <= 2, "out.len 不得超过 cap：{}", out.len);
        buffer.truncate(out.len);
        assert_eq!(String::from_utf8(buffer).unwrap(), "");
    }
}
