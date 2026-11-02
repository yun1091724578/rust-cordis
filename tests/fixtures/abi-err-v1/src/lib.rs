//! Cordis 插件 ABI（v1）。
//!
//! # 为什么需要它
//! cordis 的"插件"在 JS 里是**一个 ESM 模块 + 具名导出**，运行期契约是"对象图语义"，
//! 根本没有二进制 ABI。所以要在 Rust 里做到"任意插件可安装"，必须**发明**这一层。
//! 这是移植工作中唯一不能靠翻译得到、只能靠设计的部分。
//!
//! # 三条设计取舍
//!
//! 1. **单入口 + op 码，而不是一大堆函数指针。**
//!    ABI 面越窄，向后兼容越容易：新增能力 = 新增 op 码，旧宿主遇到不认识的 op
//!    返回 `Unsupported`，新插件优雅降级；旧插件在新宿主上永远照常工作。
//!
//! 2. **全部内存归宿主所有。**
//!    `host → plugin` 的入参数组由宿主提供、调用期间有效；
//!    `plugin → host` 的返回值由插件写进宿主提供的 `Out` 缓冲区。
//!    **跨 ABI 边界零分配**，于是不存在"谁 free、用哪个 allocator free"这类
//!    跨语言插件系统最难缠的一类崩溃。缓冲区不够时返回 `TooSmall` 并回报所需长度，
//!    宿主扩容重试——这是唯一需要两步的路径，但它只发生在边界上。
//!
//! 3. **跨边界的值统一降级为 JSON。**
//!    Rust 进程内插件之间可以直接传 `Arc<T>`（零拷贝、类型安全）；
//!    一旦越过 ABI，服务值必须是 JSON（`cordis_core::ExternValue`）。
//!    这是有意的：Rust 类型标识不可能跨编译单元稳定，而 JSON 可以，且天然前向兼容
//!    （未知字段忽略）。热路径可以后续用版本化的 `#[repr(C)]` vtable 替换，
//!    但那是第二期优化，不是第一期的正确性来源。

#![allow(non_camel_case_types)]

use std::os::raw::c_char;

/// ABI 主版本。宿主只接受**相等**的 `abi_version`（次版本用 `size` 字段做扩展探测）。
pub const ABI_VERSION: u32 = 1;

/// 插件必须导出的符号名。
pub const ENTRY_SYMBOL: &[u8] = b"cordis_plugin_v1\0";

// ------------------------------------------------------------------ op 码

/// 装载插件：`a` = 该行的 config JSON。插件应在此刻注册服务与监听器。
pub const OP_APPLY: u32 = 1;
/// 卸载插件：释放插件自己持有的状态（宿主负责回收 fiber 上的所有注册）。
///
/// 契约（M1-1）：只在插件**成功装载**（OP_APPLY 返回 STATUS_OK）之后投递，
/// 且排在该插件注册的全部清理动作（OP_CLEANUP）**之后**——插件的 cleanup 期间
/// ctx 句柄仍然有效；apply 失败的实例不会收到本通知。
pub const OP_DISPOSE: u32 = 2;
/// 宿主回调插件注册的监听器：`a` = `{"kind": "event"|"iface"|"dispatch", "id": <u64>, "dispatch_id": <u64>, "event": ..., "method": ..., "args": [...]}`，
/// （`dispatch_id` 自 ABI v2 #17 起随入站回调下发：要挂起就记住它，之后用 `ctx_fulfill` 结算。）
/// 返回值是 `{"ret": <JSON>}`；`null` 与 `false` 都**不是** bail 值（对齐 `events.ts:7`，M1-3），
/// 其余值（含 `0`、空串）都会截断 `bail`/`serial` 链。
/// 也可以返回 `STATUS_PENDING` 表示**挂起**（M2）：宿主会等 `ctx_fulfill(handle, dispatch_id, payload)` 再继续。
pub const OP_CALLBACK: u32 = 3;
/// 宿主回调插件注册的清理动作：`a` = `{"id": <u64>}`。
pub const OP_CLEANUP: u32 = 4;
/// 探活：宿主用它确认插件真的在跑（也想给"版本不匹配但结构可读"留一条路）。
pub const OP_PING: u32 = 5;
/// 查询插件的 **volatile 声明**（M4-5）：`ret` = 点号路径的 JSON 数组（如 `["data"]`）。
///
/// 与 [`OP_APPLY`] 的关系：声明是插件的**静态能力**（编译期就知道），
/// 所以这个查询在 `apply` 前后都能答；宿主在装载成功后查一次并缓存。
/// 不参与的插件返回 `STATUS_OK` + `[]`（或对旧插件而言，本 op 不存在 ⇒ `Unsupported`）。
pub const OP_VOLATILE_PATHS: u32 = 6;
/// 推送 volatile 新值（M4-5）：`a` = `{"paths": [{"path": "data", "value": <JSON>}]}`。
///
/// 这是"引用"在跨 ABI 时的替身：进程内插件共享一个 `VolatileRef`，跨边界只能把
/// **新值**推给插件，由插件更新**该实例**那份副本（`sdk::volatile("data")` 读的就是它）。
/// 同一个 cdylib 的多个实例各自保存自己的副本；`ctx` 参数指明这次推给哪个实例。
/// 推送失败（返回 `STATUS_ERR`）⇒ 宿主放弃这次提交、保留运行值。
pub const OP_VOLATILE_APPLY: u32 = 7;

/// `kind` 取值（宿主与插件共用的判别字段）。
pub const KIND_EVENT: &str = "event";
pub const KIND_IFACE: &str = "iface";
pub const KIND_DISPATCH: &str = "dispatch";
/// M2b：waterfall 的 `on_done` 交付——`{kind:"next", id:on_done, args:[{"result": …}]}`。
pub const KIND_NEXT: &str = "next";
/// 流（ADR 0001 §5）：宿主把"窗口腾出位置了"送回提供者——
/// `{kind:"stream-credit", id:on_credit, args:[{"result":{"window":n,"dispatch_id":id}}]}`；
/// `args:[{"error": …}]` 表示这条流已经不在了（别再推）。
///
/// 与 [`KIND_NEXT`] 同一形态：**路由靠 `id`（续延），不靠 `kind`**——插件侧不需要
/// 为它加解析分支。
pub const KIND_STREAM_CREDIT: &str = "stream-credit";

// ------------------------------------------------------------------ 函数指针别名

pub type CtxGetFn = unsafe extern "C" fn(ctx: u64, name: Blob, out: *mut Out) -> i32;
pub type CtxProvideFn =
    unsafe extern "C" fn(ctx: u64, name: Blob, value_json: Blob, out: *mut Out) -> i32;
pub type CtxOnFn = unsafe extern "C" fn(ctx: u64, event: Blob, cb: u64, out: *mut Out) -> i32;
pub type CtxEffectFn = unsafe extern "C" fn(ctx: u64, cb: u64, out: *mut Out) -> i32;
pub type CtxEmitFn =
    unsafe extern "C" fn(ctx: u64, event: Blob, payload: Blob, cb: u64, out: *mut Out) -> i32;
pub type LogFn = unsafe extern "C" fn(ctx: u64, level: u32, message: Blob) -> i32;
pub type CtxProvideIfaceFn = unsafe extern "C" fn(
    ctx: u64,
    name: Blob,
    version: u32,
    methods: Blob,
    cb: u64,
    out: *mut Out,
) -> i32;
pub type CtxCallIfaceFn = unsafe extern "C" fn(
    ctx: u64,
    service: Blob,
    version: u32,
    method: Blob,
    args: Blob,
    cb: u64,
    out: *mut Out,
) -> i32;
pub type CtxDispatchFn =
    unsafe extern "C" fn(ctx: u64, event: Blob, payload: Blob, cb: u64, out: *mut Out) -> i32;
/// M2-1：读一个**必需**服务。签名与 [`CtxGetFn`] 相同，但语义更严格——
/// 未声明 `inject` / 提供者未就绪 / 跨隔离域都会返回 `STATUS_ERR` 与一条可读消息。
pub type CtxRequireFn = unsafe extern "C" fn(ctx: u64, name: Blob, out: *mut Out) -> i32;
/// M2-2：覆盖**自己提供的**服务值（对应 JS 的 `ctx.set(name, value)`）。
/// 名字没 provide 过、或实现者不是当前 fiber，都会返回 `STATUS_ERR`。
pub type CtxSetFn =
    unsafe extern "C" fn(ctx: u64, name: Blob, value_json: Blob, out: *mut Out) -> i32;
/// ABI v2 #12（M1）：派生 realm 子 context。`key` = 隔离键、`label` = 可读标签，
/// 成功时 `out` = `{"handle": <u64>}`——一个 `bit63=1` 的**派生句柄**。
///
/// 子 context 继承调用方实例身份（回调 id 仍走原实例派发），但 realm 隔离：
/// 通过派生句柄的 `ctx_get`/`ctx_require` 读的是隔离域内的值。
pub type CtxIsolateFn =
    unsafe extern "C" fn(ctx: u64, key: Blob, label: Blob, out: *mut Out) -> i32;
/// ABI v2 #13（M1）：派生 intercept 子 context。`metadata` = 拦截配置 JSON，
/// 成功时 `out` = `{"handle": <u64>}`。子 context（isolate/intercept 派生）
/// 可以读到这份 metadata；真正的消费点在 M3。
pub type CtxInterceptFn =
    unsafe extern "C" fn(ctx: u64, key: Blob, metadata: Blob, out: *mut Out) -> i32;
/// ABI v2 #14（M1）：挂一个子组件。`url` 交给 `PluginSource` 解析；
/// `url = "@native:<name>"` 走 `NativeRegistry`。成功时 `out` = `{"handle": <u64>}`。
/// 新 fiber 是**调用方 fiber 的一个 effect**：父 dispose ⇒ 子 dispose。
pub type CtxUseFn = unsafe extern "C" fn(ctx: u64, url: Blob, config: Blob, out: *mut Out) -> i32;
/// ABI v2 #15（M1）：回收一个派生 ctx 句柄或子 fiber 句柄。
/// `target` = `{"handle": <u64>}` 形式的 JSON。
pub type CtxDisposeFn = unsafe extern "C" fn(ctx: u64, target: Blob, out: *mut Out) -> i32;
/// ABI v2 #16（M1）：卸载调用者自己。宿主把调用插件所在 loader 行标记
/// disabled 并**持久化**（绝不静默）。
pub type CtxDisposeSelfFn = unsafe extern "C" fn(ctx: u64, out: *mut Out) -> i32;
/// ABI v2 #17（M2）：结算一个返回过 [`STATUS_PENDING`] 的入站续延。
///
/// `dispatch_id` 由宿主在发起入站回调时随载荷下发；`payload` = 这次回调的
/// **返回值本身**（与同步路径 `{"ret": …}` 的内层同物）。
/// 键是「实例 × dispatch_id」：每 id **恰好结算一次**，重复结算是一条可读错误。
pub type CtxFulfillFn =
    unsafe extern "C" fn(ctx: u64, dispatch_id: u64, payload: Blob, out: *mut Out) -> i32;
/// ABI v2 #18（M2b）：续跑 waterfall 的**剩余链**。
///
/// 在 `OP_CALLBACK`（`mode:"waterfall"`）**内部**调用；`a` = `{"args": [...]}`
/// 是传给下游的新实参。宿主用一个内部任务推进剩余链（同步时就地完成），
/// 完成后以 `{kind:"next", id:on_done, args:[{"result": …}]}` 回调插件，
/// 插件随后用 `ctx_fulfill(dispatch_id, 终值)` 结算。
pub type CtxNextFn = unsafe extern "C" fn(
    ctx: u64,
    dispatch_id: u64,
    args: Blob,
    on_done: u64,
    out: *mut Out,
) -> i32;
/// ABI v2 #19（M2c）：取消一次在飞派发——该派发**及其后代**（在同一实例上的
/// 挂起续延、waterfall 帧、出站派发的续延）全部以「已取消」结算。
pub type CtxAbortFn = unsafe extern "C" fn(ctx: u64, dispatch_id: u64, out: *mut Out) -> i32;
/// ABI v2 #20（流，ADR 0001 §5）：往**这次派发对应的流**里推一块（JSON）。
///
/// - `STATUS_OK`：块已被接受（进了窗口，最多 `STREAM_WINDOW` 块未归还未取走）；
/// - [`STATUS_PENDING`]：窗口满了，**块没有被接受**——等宿主回调 `on_credit`
///   （`{kind:"stream-credit", id:on_credit, args:[{"result":{"window":n,…}}]}`）再重推；
/// - [`STATUS_STREAM_GONE`]：这条流已经不在了（消费者离场 / 已收尾 / 插件已卸载），停手；
/// - `STATUS_ERR`：这次派发**不是流**（调用方走的是 `ctx_call_iface`），
///   `out` 里是可读原因。
///
/// `on_credit` 只在返回 `STATUS_PENDING` 时被记账（一次一个位置）；返回其它码时
/// 它不被使用（插件侧应当把登记的那条续延忘掉）。
pub type CtxStreamPushFn = unsafe extern "C" fn(
    ctx: u64,
    dispatch_id: u64,
    chunk: Blob,
    on_credit: u64,
    out: *mut Out,
) -> i32;
/// ABI v2 #21（流）：正常收尾——消费者读完已缓冲的块之后拿到 `Ok(None)`。
pub type CtxStreamCloseFn = unsafe extern "C" fn(ctx: u64, dispatch_id: u64, out: *mut Out) -> i32;
/// ABI v2 #22（流）：失败收尾——消费者**立刻**读到 `reason`（缓冲丢弃）。
pub type CtxStreamFailFn =
    unsafe extern "C" fn(ctx: u64, dispatch_id: u64, reason: Blob, out: *mut Out) -> i32;

// ------------------------------------------------------------------ 状态码

pub const STATUS_OK: i32 = 0;
/// 失败：`out` 里是 UTF-8 的错误消息。
pub const STATUS_ERR: i32 = 1;
/// 输出缓冲区不够：`out.len` 回填所需字节数，宿主扩容后重试。
pub const STATUS_TOO_SMALL: i32 = 2;
/// 宿主调用了本插件不认识的 op：插件必须**原样返回这个码**，而不是崩溃。
pub const STATUS_UNSUPPORTED: i32 = 3;
/// **挂起**（M2）：入站回调（`OP_CALLBACK`）告诉宿主"结果稍后经 `ctx_fulfill` 到达"。
///
/// 只有 `OP_CALLBACK` 的返回码会用到它；其它 op 返回它是协议错误——
/// 例外是 [`CtxStreamPushFn`]：宿主用同一个码告诉提供者"窗口满了，等 credit"。
/// 用**状态码**而不是载荷里的标记（如 `{"pending":true}`）表达挂起，
/// 是为了让"合法返回值恰好长得像标记"在结构上不可能发生。
pub const STATUS_PENDING: i32 = 4;
/// **流已经不在了**（ADR 0001 §5）：[`CtxStreamPushFn`] 在"这次派发已经没有对应的流"
/// （消费者离场 / 流已收尾 / 插件已卸载）时返回它。
///
/// 它不是错误：提供者应当**停手并收尾**（重复的 `ctx_stream_close` 也是幂等的）。
pub const STATUS_STREAM_GONE: i32 = 5;

/// 一段只读字节。归属见模块头部说明：**永远由调用方拥有**。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Blob {
    pub ptr: *const u8,
    pub len: usize,
}

impl Blob {
    pub const EMPTY: Blob = Blob {
        ptr: std::ptr::null(),
        len: 0,
    };

    pub fn new(bytes: &[u8]) -> Self {
        Blob {
            ptr: bytes.as_ptr(),
            len: bytes.len(),
        }
    }

    /// # Safety
    /// `ptr` 必须在 `len` 字节内有效，且是 UTF-8。
    pub unsafe fn as_bytes<'a>(self) -> &'a [u8] {
        if self.ptr.is_null() || self.len == 0 {
            &[]
        } else {
            std::slice::from_raw_parts(self.ptr, self.len)
        }
    }

    /// # Safety
    /// 同 `as_bytes`。
    pub unsafe fn as_str<'a>(self) -> &'a str {
        std::str::from_utf8_unchecked(self.as_bytes())
    }
}

/// 宿主提供的输出缓冲区。
#[repr(C)]
pub struct Out {
    pub ptr: *mut u8,
    pub cap: usize,
    /// 插件写入的实际长度；`STATUS_TOO_SMALL` 时回填所需长度。
    pub len: usize,
}

impl Out {
    /// 把 `bytes` 写出去。缓冲区不够时回填所需长度并返回 `STATUS_TOO_SMALL`。
    ///
    /// # Safety
    /// `self.ptr`/`self.cap` 必须指向宿主提供的一块有效可写内存。
    pub unsafe fn write(&mut self, bytes: &[u8]) -> i32 {
        if bytes.len() > self.cap {
            self.len = bytes.len();
            return STATUS_TOO_SMALL;
        }
        if !bytes.is_empty() {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.ptr, bytes.len());
        }
        self.len = bytes.len();
        STATUS_OK
    }
}

/// 宿主函数表。插件**只能**通过它访问 cordis 内核，绝不直接链接宿主。
///
/// # 每个字段都是 `Option` + 尾追加，二者共同构成前向兼容
/// - `None`：宿主**有能力但不提供**这个函数（比如为了安全策略关闭）；
/// - 字段不存在：宿主是更老的版本，`size` 会小于该字段的偏移 + 宽度。
///
/// 插件必须用 [`HostApi::field`] 家族去读字段，**绝不能直接访问**
/// ——越过 `size` 读字段就是读取未分配内存。这两个机制合起来保证了
/// "新插件 + 旧宿主"与"旧插件 + 新宿主"两个方向都不会崩，只会优雅降级。
#[repr(C)]
pub struct HostApi {
    pub abi_version: u32,
    /// `sizeof(HostApi)`。宿主与插件都用它做"字段存在性"探测。
    pub size: u32,

    /// 读一个服务（可选服务探测也用这个）。返回 `{"value": <JSON>}` 或 `{"missing": true}`。
    pub ctx_get: Option<CtxGetFn>,
    /// 注册一个服务，返回 disposer 信息（此处句柄由 fiber 拥有，插件不需要记账）。
    pub ctx_provide: Option<CtxProvideFn>,
    /// 注册一个事件监听器，`cb` = 回调 id。
    pub ctx_on: Option<CtxOnFn>,
    /// 注册一个"卸载时回调"的清理动作，`cb` = 回调 id。
    pub ctx_effect: Option<CtxEffectFn>,
    /// 派发事件。`payload` = `{"mode": "...", "args": [...]}`。
    ///
    /// 这是"同步 ABI 接异步内核"的关键：宿主会**先尝试同步完成**（所有监听器都
    /// 立即就绪时直接返回结果），否则把结果通过 `cb` 异步回调回来。
    /// 因此 `cb` 是**必填**的：调用方必须同时处理"内联返回"与"稍后回调"两种路径。
    pub ctx_emit: Option<CtxEmitFn>,
    /// 日志。
    pub log: Option<LogFn>,

    // ↓ 以下为 v1 的尾追加扩展（`size` 比它们小的宿主不认识这些函数）
    /// 注册一个**带方法的服务**：`methods` = 方法名 JSON 数组，`cb` = 方法分发回调。
    pub ctx_provide_iface: Option<CtxProvideIfaceFn>,
    /// 调用另一个插件的服务方法；结果通过 `cb` 回来（宿主可能内联回调）。
    pub ctx_call_iface: Option<CtxCallIfaceFn>,
    /// 独立的异步派发入口（返回值通过 `cb` 回来）。
    pub ctx_dispatch: Option<CtxDispatchFn>,
    /// M2-1 尾追加：读一个**必需**服务（注入检查 + `committed` 快照）。
    ///
    /// 与 `ctx_get` 的分工是"断言 vs 探测"：
    /// - `ctx_get` 不要求声明 `inject`，`{"missing": true}` 同时表示"没声明"和"不存在"；
    /// - `ctx_require` 要求声明 `inject`（否则报 `cannot get property "…" without inject`，
    ///   而不是把配置错误伪装成 `missing`），且读的是 **committed 快照**——
    ///   组件在**自己的拆除过程**中仍能读到正在离开的依赖，这对
    ///   "卸载时回调依赖做收尾"的插件是必需的。
    ///
    /// 成功时载荷与 `ctx_get` 相同（`{"value": <JSON>}`）；失败时返回 `STATUS_ERR`
    /// 与 `ResolveError` 的可读消息。
    pub ctx_require: Option<CtxRequireFn>,
    /// M2-2 尾追加：覆盖**自己提供的**服务值（对应 JS 的 `ctx.set(name, value)`，
    /// 上游 `reflect.ts:164-175`）。
    ///
    /// 两条硬约束由内核强制：
    /// - 名字**没 provide 过** → `cannot set property "…" without provide`；
    /// - 实现者**不是当前 fiber** → `cannot set property "…" in multiple fibers`。
    ///
    /// `set` 换的是**值**，不改变"谁提供"，因此**不通知依赖者**重载——
    /// 依赖者的 epoch 只关心实现者身份（上游同样如此）。
    pub ctx_set: Option<CtxSetFn>,

    // ↓ ABI v2 尾追加（附录 A #12–#19，M1/M2）。`size` 比这些字段小的宿主上
    //   它们探测为 `None`，SDK 给出「宿主不支持」的可读错误。
    /// #12：派生 realm 子 context，返回派生句柄（`{"handle": <u64>}`）。
    pub ctx_isolate: Option<CtxIsolateFn>,
    /// #13：派生 intercept 子 context（存 metadata，供子 context 读取；消费在 M3）。
    pub ctx_intercept: Option<CtxInterceptFn>,
    /// #14：挂子组件（`url` 交 `PluginSource`；`@native:<name>` 走 `NativeRegistry`）。
    pub ctx_use: Option<CtxUseFn>,
    /// #15：回收派生 ctx 句柄或子 fiber 句柄（`target` = `{"handle": …}`）。
    pub ctx_dispose: Option<CtxDisposeFn>,
    /// #16：卸载调用者自己（loader 行标记 disabled 并持久化）。
    pub ctx_dispose_self: Option<CtxDisposeSelfFn>,
    /// #17（M2）：结算一个挂起的入站续延（`STATUS_PENDING` 的配对操作）。
    pub ctx_fulfill: Option<CtxFulfillFn>,
    /// #18（M2b）：续跑 waterfall 的剩余链（在 waterfall 回调内部调用）。
    pub ctx_next: Option<CtxNextFn>,
    /// #19（M2c）：取消一次在飞派发（含其后代）。
    pub ctx_abort: Option<CtxAbortFn>,

    // ↓ 流（ADR 0001 §5）：`call_iface_stream` 的提供者侧三件套。
    //   `size` 比这些字段小的宿主上它们探测为 `None`，SDK 给出可读错误。
    /// #20：往这次派发对应的流里推一块（满窗口 ⇒ `STATUS_PENDING` + credit 回调）。
    pub ctx_stream_push: Option<CtxStreamPushFn>,
    /// #21：正常收尾（`Ok(None)`）。
    pub ctx_stream_close: Option<CtxStreamCloseFn>,
    /// #22：失败收尾（`reason` 直达消费者）。
    pub ctx_stream_fail: Option<CtxStreamFailFn>,
}

impl HostApi {
    /// 安全地读一个尾追加字段：`size` 不够就当作"宿主不支持"。
    ///
    /// # Safety
    /// `self` 必须指向宿主提供的、仍然有效的 `HostApi`。
    pub unsafe fn field<T: Copy>(&self, value: Option<T>, offset: usize) -> Option<T> {
        if (self.size as usize) >= offset + std::mem::size_of::<Option<T>>() {
            value
        } else {
            None
        }
    }
}

/// 插件描述表。`cordis_plugin_v1()` 返回它的静态地址。
///
/// 注意尾部字段是可选的扩展位：宿主用 `size` 判断字段是否存在，
/// 所以**只能往尾部追加**，永远不要改动已有字段的顺序或类型。
#[repr(C)]
pub struct PluginDesc {
    pub abi_version: u32,
    /// `sizeof(PluginDesc)`（v1 插件则是 [`PLUGIN_DESC_PREFIX_SIZE`]）。
    pub size: u32,
    /// 插件身份名（建议用包名），同时是注册表身份键的一部分。
    pub id: Blob,
    /// `inject` 声明：JSON 数组 `["tools", ...]` 或对象 `{"name": <intercept config>}`。
    pub inject: Blob,
    /// 配置校验用的 JSON Schema（可空）。
    pub schema: Blob,

    /// 宿主加载后**立刻**调用一次，把函数表交给插件。此后插件把它存进自己的静态变量。
    pub attach: unsafe extern "C" fn(host: *const HostApi),
    /// 唯一入口。
    pub call: unsafe extern "C" fn(ctx: u64, op: u32, a: Blob, b: Blob, out: *mut Out) -> i32,
    /// 尾追加（v2 起）：`provision` 声明，JSON 数组（如 `["tools"]`）。
    ///
    /// v1 插件没有这个字段——宿主读它必须走 [`PluginDesc::field_at`]（按 `size`
    /// 判断存在性），读到 `None` 就是「未声明」，绝不是越界读（附录 A）。
    pub provision: Blob,
}

impl PluginDesc {
    /// 按 `size` 安全地读一个**尾追加**字段（附录 A）。
    ///
    /// `offset + 字段宽度 <= size` 才读，否则 `None` = 「插件没声明这个字段」。
    /// 之所以用 `read_unaligned`：`*const PluginDesc` 来自 `extern "C"` 边界，
    /// 声明方（第三方插件）没有义务保证对齐。
    pub fn field_at<T: Copy>(&self, offset: usize) -> Option<T> {
        if self.size as usize >= offset + std::mem::size_of::<T>() {
            // SAFETY：上面的判界保证 `offset + width` 落在插件自己声明的结构体内。
            let base = self as *const Self as *const u8;
            Some(unsafe { std::ptr::read_unaligned(base.add(offset) as *const T) })
        } else {
            None
        }
    }
}

/// **冻结前缀**（v1/v2 兼容契约）：`call`（含）之前的全部字段的总尺寸。
///
/// 宿主门禁只要求 `PluginDesc.size >= PLUGIN_DESC_PREFIX_SIZE`——v2+ 给
/// `PluginDesc` 尾追加字段**不是**破坏性变更，v1 插件照常加载（G14）。
/// 这个数一旦发布就永远不能变：它不引用任何宿主侧类型/别名，插件与宿主
/// 各自编译也得到同一个值。
pub const PLUGIN_DESC_PREFIX_SIZE: usize = std::mem::offset_of!(PluginDesc, call)
    + std::mem::size_of::<unsafe extern "C" fn(u64, u32, Blob, Blob, *mut Out)>();

/// 插件入口符号签名。
pub type EntryFn = unsafe extern "C" fn() -> *const PluginDesc;

/// `PluginDesc.id` 的可读化（诊断用）。
///
/// # Safety
/// `desc` 必须指向一个仍然有效的 `PluginDesc`。
pub unsafe fn plugin_id(desc: *const PluginDesc) -> String {
    let raw = (*desc).id.as_str();
    raw.to_string()
}

/// 一个 C 字符串字面量的便捷包装（插件侧构造 `Blob` 用）。
///
/// # Safety
/// `s` 要么为空，要么指向一个以 NUL 结尾、在返回的 `Blob` 使用期间保持有效的 C 字符串。
pub unsafe fn cstr_to_blob(s: *const c_char) -> Blob {
    if s.is_null() {
        return Blob::EMPTY;
    }
    let len = (0..).take_while(|&i| unsafe { *s.add(i) } != 0).count();
    Blob {
        ptr: s as *const u8,
        len,
    }
}

// ------------------------------------------------------------------ 冻结夹具
//
// 下面这段是本夹具的插件入口：只使用上面冻结的 ABI 类型，不链接
// workspace 的 `cordis-abi`。结构体是**真正的 v1 尺寸**（只到 `call`，
// 没有 `provision` 字段），并通过 `size` 判界读取宿主函数表。

use std::sync::OnceLock;

/// v1 结构体：与 `PluginDesc` 的前 7 个字段逐字段同布局，但没有 `provision`。
#[repr(C)]
struct PluginDescV1 {
    abi_version: u32,
    size: u32,
    id: Blob,
    inject: Blob,
    schema: Blob,
    attach: unsafe extern "C" fn(host: *const HostApi),
    call: unsafe extern "C" fn(ctx: u64, op: u32, a: Blob, b: Blob, out: *mut Out) -> i32,
}

const _: () = assert!(
    std::mem::size_of::<PluginDescV1>() == 72,
    "abi-err-v1 必须保持 72B v1 前缀"
);

// 描述表在初始化后只读；函数指针与静态 Blob 都是进程级常量。
unsafe impl Send for PluginDescV1 {}
unsafe impl Sync for PluginDescV1 {}

static DESC: OnceLock<PluginDescV1> = OnceLock::new();

unsafe extern "C" fn frozen_attach(_host: *const HostApi) {}

unsafe extern "C" fn frozen_call(_handle: u64, op: u32, _a: Blob, _b: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => {
            // 旧插件没有结构化能力：只返回人类可读文本。
            (*out).write(b"legacy apply failed");
            STATUS_ERR
        }
        OP_CALLBACK => {
            // 同样走纯文本错误，供 call_plugin_detailed / 异步回调路径使用。
            (*out).write(b"legacy callback failed");
            STATUS_ERR
        }
        OP_PING => (*out).write(b""),
        OP_VOLATILE_PATHS => {
            // 宿主按 `{"ret": ...}` 解包；裸数组会被当成「未声明」。
            (*out).write(br#"{"ret":["data"]}"#)
        }
        OP_DISPOSE => {
            // 可观察副作用：测试用它证明宿主真的投递了 OP_DISPOSE。
            if let Ok(path) = std::env::var("CORDIS_FROZEN_MARKER") {
                let _ = std::fs::write(path, b"disposed");
            }
            (*out).write(b"")
        }
        _ => STATUS_UNSUPPORTED,
    }
}

fn frozen_desc() -> PluginDescV1 {
    let abi_version = if cfg!(feature = "bad-version") {
        ABI_VERSION + 1
    } else {
        ABI_VERSION
    };
    let size = if cfg!(feature = "bad-size") {
        (PLUGIN_DESC_PREFIX_SIZE - 1) as u32
    } else {
        std::mem::size_of::<PluginDescV1>() as u32
    };
    PluginDescV1 {
        abi_version,
        size,
        id: Blob::new(b"abi-err-v1"),
        inject: Blob::new(b"[]"),
        schema: Blob::EMPTY,
        attach: frozen_attach,
        call: frozen_call,
    }
}

/// 插件入口符号（`ENTRY_SYMBOL` 的冻结字面量）。
#[no_mangle]
pub extern "C" fn cordis_plugin_v1() -> *const PluginDesc {
    let desc = DESC.get_or_init(frozen_desc);
    desc as *const PluginDescV1 as *const PluginDesc
}
