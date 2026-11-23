//! FFI 输出缓冲、线程本地复用池（`CallBuffer`）与底层插件调用入口（O11 拆分）。

use crate::{instances, CallFn, Instance};
use cordis_abi::{
    Blob, ErrorPayload, Out, ERROR_FORMAT_V1, STATUS_ERR, STATUS_OK, STATUS_TOO_SMALL,
    STATUS_UNSUPPORTED,
};
use cordis_core::{Dyn, ExternValue};
use serde_json::{json, Value};
use std::sync::Mutex;

// ------------------------------------------------------------ 输出缓冲工具

/// # Safety
/// `out` 必须指向一个有效的宿主 `Out`。
pub(crate) unsafe fn reply(out: *mut Out, bytes: &[u8]) -> i32 {
    if out.is_null() {
        return STATUS_ERR;
    }
    (*out).write(bytes)
}

/// # Safety
/// 同 `reply`。
pub(crate) unsafe fn fail(out: *mut Out, message: &str) -> i32 {
    if !out.is_null() {
        let text = truncate_utf8(message, (*out).cap);
        reply(out, text.as_bytes());
    }
    STATUS_ERR
}

/// 截到 `max` 字节以内，且不切断 UTF-8 字符边界；有空间时带 `…` 截断标记。
pub(crate) fn truncate_utf8(text: &str, max: usize) -> String {
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
pub(crate) unsafe fn fail_to(
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
pub(crate) unsafe fn fail_payload(out: *mut Out, error_format: u32, payload: &ErrorPayload) -> i32 {
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
pub(crate) fn decode_plugin_error(error_format: u32, text: &str) -> cordis_abi::ErrorPayload {
    if error_format >= cordis_abi::ERROR_FORMAT_V1 {
        if let Some(payload) = cordis_abi::ErrorPayload::from_json(text) {
            return payload;
        }
    }
    cordis_abi::ErrorPayload::new(text)
}

/// 异步通道内部携带的结构化失败。它实现了 `std::error::Error`，因此可以穿过
/// 内核的 `Box<dyn Error>`，在 `host_ctx_*` 的交付闭包里再 downcast 回载荷。
pub(crate) struct CallFailure {
    pub(crate) payload: ErrorPayload,
    /// 容量拒绝时把 waterfall 的剩余链保留下来，交给事件派发继续执行。
    /// `Mutex` 让 `CallFailure` 仍满足内核错误的 `Send + Sync` 约束。
    pub(crate) frame: Mutex<Option<cordis_core::Next>>,
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
    pub(crate) fn new(
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

    pub(crate) fn from_payload(payload: ErrorPayload) -> Self {
        Self {
            payload,
            frame: Mutex::new(None),
        }
    }

    pub(crate) fn with_frame(mut self, frame: Option<cordis_core::Next>) -> Self {
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

pub(crate) fn payload_of(err: &(dyn std::error::Error + 'static)) -> ErrorPayload {
    err.downcast_ref::<CallFailure>()
        .map(|failure| failure.payload.clone())
        .unwrap_or_else(|| ErrorPayload::new(err.to_string()))
}

/// 异步回调 `args[0]`：旧的 `error` 字符串始终保留；能力位为 1 时追加结构化兄弟字段。
pub(crate) fn error_arg(error_format: u32, payload: &ErrorPayload) -> Value {
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
pub(crate) unsafe fn with_instance<T>(
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
pub(crate) unsafe fn with_instance_as<T>(
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
pub(crate) unsafe fn with_instance_payload<T>(
    handle: u64,
    out: *mut Out,
    f: impl FnOnce(&Instance) -> std::result::Result<T, ErrorPayload>,
) -> i32
where
    T: AsRef<[u8]>,
{
    let instance = instances().read().unwrap().get(handle);
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
pub(crate) fn args_to_json(args: &[Dyn]) -> Value {
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

pub(crate) fn json_to_args(value: &Value) -> Vec<Dyn> {
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
pub(crate) enum CallOutcome {
    Ok(Value),
    Err(cordis_abi::ErrorPayload),
    Unsupported,
}

/// 调一次插件的 op，返回 `(status, out 文本)`：缓冲区不够时**扩容重试一次**。
///
/// 这是跨边界调用的唯一底层形态；`call_op`（判语义）与 `call_inbound`
/// （判挂起）都在它上面分层。
pub(crate) unsafe fn invoke_raw(
    call: CallFn,
    handle: u64,
    op: u32,
    payload: &str,
) -> (i32, String) {
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
pub(crate) struct CallBuffer(pub(crate) Vec<u8>);

thread_local! {
    static CALL_BUFFER_POOL: std::cell::RefCell<Vec<Vec<u8>>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

impl CallBuffer {
    pub(crate) fn take() -> Self {
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

    pub(crate) fn len(&self) -> usize {
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
pub(crate) const MAX_READ_RETRY_BYTES: usize = 1024 * 1024;

/// 只读扩容重试要分配的缓冲大小：先拒越界与超限，再给出长度。
///
/// `needed` 来自插件回填的 `out.len`，**不可信**：`vec![0u8; n]` 在
/// `n > isize::MAX` 时 panic，真实 OOM 时 abort——两种都不该由插件的一行数字触发。
pub(crate) fn read_retry_cap(needed: usize) -> std::result::Result<usize, String> {
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

pub(crate) unsafe fn invoke_raw_with(
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
pub(crate) unsafe fn call_op(
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
pub(crate) unsafe fn call_op_read(
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

pub(crate) unsafe fn call_op_with(
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
pub(crate) unsafe fn call_plugin(
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
