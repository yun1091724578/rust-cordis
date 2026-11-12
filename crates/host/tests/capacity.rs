//! P0-3：宿主容量/准入。
//!
//! 覆盖四类表项准入：
//! - 每个 App 的实例数（`maxInstancesPerApp`）；
//! - 每个实例的派发记录总数（`maxDispatchesPerInstance`）；
//! - 每个实例的挂起续延（`maxPendingPerInstance`）；
//! - 每个实例的流（`maxStreamsPerInstance`）。
//!
//! 默认值足够宽，既有行为不变；这里只按 App 配小上限。拒绝必须可读、结构化
//! （`host_capacity` / `capacity` / `not_executed`），且不执行插件代码、不留下
//! 派发记录残留。
//!
//! 夹具铁律（D10）：fake `CallFn` 内禁止 assert，结局只记进 static `Mutex` /
//! 原子量；每个测试用自己的夹具静态（同一二进制内并行跑）。

use cordis_abi::{
    Blob, ErrorPayload, Out, ERROR_FORMAT_V1, KIND_IFACE, OP_APPLY, OP_CALLBACK, OP_DISPOSE,
    STATUS_ERR, STATUS_OK, STATUS_PENDING, STATUS_UNSUPPORTED,
};
use cordis_core::{plugin_fn, App, Config, Fiber, FiberState, Inject, Plugin};
use cordis_host::{
    apply_via_with_error_format, dispatches_len, host_api, pending_len, set_default_host_limits,
    set_host_limits_for, streams_len, CallFn, HostLimits,
};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicI32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const APPLY_DEADLINE: Duration = Duration::from_secs(10);

fn abi_plugin(name: &'static str, call: CallFn) -> Arc<dyn Plugin> {
    plugin_fn(name, Inject::default(), move |ctx, config| {
        // 能力位 1：容量拒绝必须带结构化兄弟字段（code/category/certainty/detail）。
        apply_via_with_error_format(call, ctx, config, name, ERROR_FORMAT_V1)
    })
}

async fn mount(app: &App, name: &'static str, call: CallFn) -> Arc<Fiber> {
    let fiber = app.root.plugin(abi_plugin(name, call), Config::Null);
    tokio::time::timeout(APPLY_DEADLINE, fiber.settle())
        .await
        .expect("apply 结算超时");
    fiber
}

async fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !ready() {
        if tokio::time::Instant::now() >= deadline {
            panic!("等待超时：{what}");
        }
        tokio::task::yield_now().await;
    }
}

/// 在插件 apply 期间注册一个事件监听器；返回 `false` 表示宿主不支持 `ctx_on`。
unsafe fn register_event(handle: u64, event: &[u8], cb: u64) -> bool {
    let Some(on) = host_api().ctx_on else {
        return false;
    };
    let mut buffer = [0u8; 64];
    let mut slot = Out {
        ptr: buffer.as_mut_ptr(),
        cap: buffer.len(),
        len: 0,
    };
    on(handle, Blob::new(event), cb, &mut slot);
    true
}

/// 在插件 apply 期间注册一个 iface；返回 `false` 表示宿主不支持 `ctx_provide_iface`。
unsafe fn provide_iface(
    handle: u64,
    service: &[u8],
    version: u32,
    methods: &[u8],
    dispatcher: u64,
) -> bool {
    let Some(provide) = host_api().ctx_provide_iface else {
        return false;
    };
    let mut buffer = [0u8; 128];
    let mut slot = Out {
        ptr: buffer.as_mut_ptr(),
        cap: buffer.len(),
        len: 0,
    };
    provide(
        handle,
        Blob::new(service),
        version,
        Blob::new(methods),
        dispatcher,
        &mut slot,
    ) == STATUS_OK
}

fn decode_payload(text: &[u8]) -> ErrorPayload {
    ErrorPayload::from_json(&String::from_utf8_lossy(text))
        .expect("能力位为 1 时容量拒绝必须是结构化的")
}

fn assert_capacity(payload: &ErrorPayload, scope: &str) {
    assert_eq!(payload.code, "host_capacity", "{}", payload.error);
    assert_eq!(payload.category, "capacity", "{}", payload.error);
    assert_eq!(payload.certainty, "not_executed", "{}", payload.error);
    assert!(
        payload.error.contains(scope),
        "可读错误必须点名上限（{scope}）：{}",
        payload.error
    );
    assert!(
        payload.error.contains("插件代码未执行"),
        "可读错误必须说明插件代码没有执行：{}",
        payload.error
    );
}

// ---------------------------------------------------------------- 实例上限

unsafe extern "C" fn ok_call(_: u64, op: u32, _: Blob, _: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY | OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

/// 同一 App 填满后拒绝下一个实例；另一个 App 完全不受影响。
#[tokio::test]
async fn instance_cap_is_per_app_and_readable() {
    let app_a = App::new();
    set_host_limits_for(
        &app_a.root,
        HostLimits {
            max_instances_per_app: 1,
            ..HostLimits::default()
        },
    )
    .expect("正整数上限必须被接受");

    let first = mount(&app_a, "cap-a-first", ok_call).await;
    assert_eq!(first.state(), FiberState::Active, "第一个实例应成功");

    let second = mount(&app_a, "cap-a-second", ok_call).await;
    assert_eq!(
        second.state(),
        FiberState::Failed,
        "超过 maxInstancesPerApp 的实例必须装载失败"
    );
    let error = second.error().expect("失败的 apply 必须留下错误");
    assert!(
        error.contains("maxInstancesPerApp") && error.contains("上限"),
        "实例上限拒绝必须可读：{error}"
    );

    // 另一个 App 有自己的注册表条目；A 的上限不能泄漏过去。
    let app_b = App::new();
    let b_first = mount(&app_b, "cap-b-first", ok_call).await;
    let b_second = mount(&app_b, "cap-b-second", ok_call).await;
    assert_eq!(b_first.state(), FiberState::Active);
    assert_eq!(
        b_second.state(),
        FiberState::Active,
        "App B 未配置上限，必须保持默认宽松行为"
    );

    first.dispose().await;
    b_first.dispose().await;
    b_second.dispose().await;
    app_a.close().await;
    app_b.close().await;
}

/// 0 不是“无限”，而是非法配置：两个设置入口都必须拒绝，且缺省值是可用的。
#[test]
fn zero_limits_are_rejected_not_unlimited() {
    let app = App::new();
    let err = set_host_limits_for(
        &app.root,
        HostLimits {
            max_instances_per_app: 0,
            ..HostLimits::default()
        },
    )
    .expect_err("per-App 的 0 必须被拒绝");
    assert!(err.contains("maxInstancesPerApp"), "{err}");

    let err = set_default_host_limits(HostLimits {
        max_streams_per_instance: 0,
        ..HostLimits::default()
    })
    .expect_err("进程级默认的 0 必须被拒绝");
    assert!(err.contains("maxStreamsPerInstance"), "{err}");

    let defaults = HostLimits::default();
    assert_eq!(defaults.max_instances_per_app, 1024);
    assert_eq!(defaults.max_dispatches_per_instance, 1024);
    assert_eq!(defaults.max_pending_per_instance, 256);
    assert_eq!(defaults.max_streams_per_instance, 32);
    set_default_host_limits(defaults).expect("默认值必须自洽");
}

// ---------------------------------------------------------------- 派发总数上限

static DISPATCH_HANDLE: AtomicU64 = AtomicU64::new(0);
static DISPATCH_EVENT_CALLS: AtomicUsize = AtomicUsize::new(0);
static DISPATCH_NESTED_CALLS: AtomicUsize = AtomicUsize::new(0);
static DISPATCH_NESTED_STATUS: AtomicI32 = AtomicI32::new(-1);
static DISPATCH_NESTED_TEXT: Mutex<String> = Mutex::new(String::new());

unsafe extern "C" fn dispatch_cap_call(
    handle: u64,
    op: u32,
    a: Blob,
    _: Blob,
    out: *mut Out,
) -> i32 {
    match op {
        OP_APPLY => {
            DISPATCH_HANDLE.store(handle, Ordering::SeqCst);
            if !register_event(handle, b"cap/reenter", 7) {
                return STATUS_UNSUPPORTED;
            }
            (*out).write(b"")
        }
        OP_CALLBACK => {
            let payload: Value = serde_json::from_slice(a.as_bytes()).unwrap_or(Value::Null);
            let event = payload
                .get("event")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if event == "cap/reenter" {
                DISPATCH_EVENT_CALLS.fetch_add(1, Ordering::SeqCst);
                let Some(dispatch) = host_api().ctx_dispatch else {
                    return STATUS_UNSUPPORTED;
                };
                let mut buffer = [0u8; 512];
                let mut slot = Out {
                    ptr: buffer.as_mut_ptr(),
                    cap: buffer.len(),
                    len: 0,
                };
                // 外层回调的入站记录仍占着唯一的名额：嵌套派发必须在插入时被拒绝。
                let status = dispatch(
                    handle,
                    Blob::new(b"cap/nested"),
                    Blob::new(br#"{"args":[]}"#),
                    9,
                    &mut slot,
                );
                DISPATCH_NESTED_STATUS.store(status, Ordering::SeqCst);
                *DISPATCH_NESTED_TEXT.lock().unwrap() =
                    String::from_utf8_lossy(&buffer[..slot.len]).into_owned();
                (*out).write(b"")
            } else {
                // 被拒绝的嵌套派发绝不应该走到这里。
                DISPATCH_NESTED_CALLS.fetch_add(1, Ordering::SeqCst);
                (*out).write(b"")
            }
        }
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

/// 插件在自己的回调里发起第二次派发：记录表已满 ⇒ 结构化拒绝，插件代码不执行，
/// 回调返回后记录表回到基线。
#[tokio::test]
async fn dispatch_cap_rejects_before_plugin_code_and_leaves_no_residue() {
    DISPATCH_HANDLE.store(0, Ordering::SeqCst);
    DISPATCH_EVENT_CALLS.store(0, Ordering::SeqCst);
    DISPATCH_NESTED_CALLS.store(0, Ordering::SeqCst);
    DISPATCH_NESTED_STATUS.store(-1, Ordering::SeqCst);
    DISPATCH_NESTED_TEXT.lock().unwrap().clear();

    let app = App::new();
    set_host_limits_for(
        &app.root,
        HostLimits {
            max_dispatches_per_instance: 1,
            ..HostLimits::default()
        },
    )
    .expect("正整数上限必须被接受");

    let fiber = mount(&app, "cap-dispatch", dispatch_cap_call).await;
    assert_eq!(fiber.state(), FiberState::Active);
    let handle = DISPATCH_HANDLE.load(Ordering::SeqCst);
    assert_ne!(handle, 0, "apply 必须登记实例");

    app.root.emit("cap/reenter", Vec::new()).await;

    assert_eq!(
        DISPATCH_EVENT_CALLS.load(Ordering::SeqCst),
        1,
        "外层回调恰好执行一次"
    );
    assert_eq!(
        DISPATCH_NESTED_CALLS.load(Ordering::SeqCst),
        0,
        "被拒绝的嵌套派发不得执行插件代码"
    );
    assert_eq!(
        DISPATCH_NESTED_STATUS.load(Ordering::SeqCst),
        STATUS_ERR,
        "容量拒绝必须以 STATUS_ERR 返回"
    );
    let text = DISPATCH_NESTED_TEXT.lock().unwrap().clone();
    let payload = decode_payload(text.as_bytes());
    assert_capacity(&payload, "maxDispatchesPerInstance");

    assert_eq!(dispatches_len(handle), 0, "拒绝后不得留下派发记录");
    assert_eq!(pending_len(handle), 0, "拒绝不得改变挂起计数");
    assert_eq!(streams_len(handle), 0, "拒绝不得改变流计数");

    fiber.dispose().await;
    app.close().await;
}

// ---------------------------------------------------------------- waterfall 容量拒绝

static WATERFALL_HANDLE: AtomicU64 = AtomicU64::new(0);
static WATERFALL_DOWNSTREAM: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn waterfall_cap_call(
    handle: u64,
    op: u32,
    a: Blob,
    _: Blob,
    out: *mut Out,
) -> i32 {
    match op {
        OP_APPLY => {
            WATERFALL_HANDLE.store(handle, Ordering::SeqCst);
            if !register_event(handle, b"cap/hold", 7)
                || !register_event(handle, b"cap/waterfall", 8)
            {
                return STATUS_UNSUPPORTED;
            }
            (*out).write(b"")
        }
        OP_CALLBACK => {
            let payload: Value = serde_json::from_slice(a.as_bytes()).unwrap_or(Value::Null);
            let event = payload.get("event").and_then(Value::as_str).unwrap_or("");
            if event == "cap/hold" {
                STATUS_PENDING
            } else {
                (*out).write(b"")
            }
        }
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

/// 事件监听器因容量被拒绝时，waterfall 的剩余链仍必须执行。
#[tokio::test]
async fn capacity_rejection_does_not_drop_waterfall_tail() {
    WATERFALL_HANDLE.store(0, Ordering::SeqCst);
    WATERFALL_DOWNSTREAM.store(0, Ordering::SeqCst);
    let app = App::new();
    set_host_limits_for(
        &app.root,
        HostLimits {
            max_dispatches_per_instance: 1,
            ..HostLimits::default()
        },
    )
    .expect("正整数上限必须被接受");
    let fiber = mount(&app, "cap-waterfall", waterfall_cap_call).await;
    let handle = WATERFALL_HANDLE.load(Ordering::SeqCst);
    assert_ne!(handle, 0);

    // 占住唯一派发名额：监听器挂起，不再结算。
    let hold = {
        let root = app.root.clone();
        tokio::spawn(async move { root.emit("cap/hold", Vec::new()).await })
    };
    wait_until("挂起记录进入派发表", || pending_len(handle) == 1).await;

    // 后注册的下游 handler：如果容量拒绝吞掉 frame，它不会执行。
    app.root.on("cap/waterfall", move |_call| {
        WATERFALL_DOWNSTREAM.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { None })
    });
    let result = app.root.bail("cap/waterfall", Vec::new()).await;
    assert!(result.is_none());
    assert_eq!(
        WATERFALL_DOWNSTREAM.load(Ordering::SeqCst),
        1,
        "容量拒绝不能吞掉 waterfall 下游"
    );

    hold.abort();
    fiber.dispose().await;
    app.close().await;
}

// ---------------------------------------------------------------- 挂起上限

static PENDING_HANDLE: AtomicU64 = AtomicU64::new(0);
static PENDING_EVENT_CALLS: AtomicUsize = AtomicUsize::new(0);
static PENDING_IFACE_CALLS: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn pending_cap_call(
    handle: u64,
    op: u32,
    a: Blob,
    _: Blob,
    out: *mut Out,
) -> i32 {
    match op {
        OP_APPLY => {
            PENDING_HANDLE.store(handle, Ordering::SeqCst);
            if !register_event(handle, b"cap/pending", 7) {
                return STATUS_UNSUPPORTED;
            }
            if !provide_iface(handle, b"cap.pending@1", 1, br#"["ping"]"#, 9) {
                return STATUS_UNSUPPORTED;
            }
            (*out).write(b"")
        }
        OP_CALLBACK => {
            let payload: Value = serde_json::from_slice(a.as_bytes()).unwrap_or(Value::Null);
            let kind = payload
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if kind == KIND_IFACE {
                PENDING_IFACE_CALLS.fetch_add(1, Ordering::SeqCst);
                (*out).write(br#"{"ret":null}"#)
            } else {
                // 事件监听器永远挂起：它的入站记录占住唯一的挂起名额。
                PENDING_EVENT_CALLS.fetch_add(1, Ordering::SeqCst);
                STATUS_PENDING
            }
        }
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

/// 已有一条挂起续延时，新的入站派发必须被拒绝；挂起计数保持基线，provider 代码不执行。
#[tokio::test]
async fn pending_cap_rejects_before_plugin_code_and_keeps_counters() {
    PENDING_HANDLE.store(0, Ordering::SeqCst);
    PENDING_EVENT_CALLS.store(0, Ordering::SeqCst);
    PENDING_IFACE_CALLS.store(0, Ordering::SeqCst);

    let app = App::new();
    set_host_limits_for(
        &app.root,
        HostLimits {
            max_pending_per_instance: 1,
            ..HostLimits::default()
        },
    )
    .expect("正整数上限必须被接受");

    let fiber = mount(&app, "cap-pending", pending_cap_call).await;
    assert_eq!(fiber.state(), FiberState::Active);
    let handle = PENDING_HANDLE.load(Ordering::SeqCst);
    assert_ne!(handle, 0, "apply 必须登记实例");

    let emit = {
        let ctx = app.root.clone();
        tokio::spawn(async move { ctx.bail("cap/pending", Vec::new()).await })
    };
    wait_until("事件监听器挂起", || pending_len(handle) == 1).await;
    let baseline_dispatches = dispatches_len(handle);
    let baseline_pending = pending_len(handle);
    let baseline_streams = streams_len(handle);
    assert_eq!(baseline_pending, 1);
    assert_eq!(PENDING_EVENT_CALLS.load(Ordering::SeqCst), 1);

    let call = host_api().ctx_call_iface.expect("ctx_call_iface");
    let mut buffer = [0u8; 512];
    let mut slot = Out {
        ptr: buffer.as_mut_ptr(),
        cap: buffer.len(),
        len: 0,
    };
    let status = unsafe {
        call(
            handle,
            Blob::new(b"cap.pending@1"),
            1,
            Blob::new(b"ping"),
            Blob::new(b"[]"),
            0,
            &mut slot,
        )
    };
    // iface 的同步失败沿用既有内联响应契约：STATUS_OK + 响应里的 error 兄弟字段。
    assert_eq!(status, STATUS_OK, "iface 失败以内联响应交付");
    let response: Value =
        serde_json::from_slice(&buffer[..slot.len]).expect("iface 响应必须是 JSON");
    assert_eq!(response["pending"], json!(false));
    assert_eq!(response["code"], "host_capacity", "{response}");
    assert_eq!(response["category"], "capacity", "{response}");
    assert_eq!(response["certainty"], "not_executed", "{response}");
    let message = response["error"].as_str().unwrap_or("");
    assert!(
        message.contains("maxPendingPerInstance"),
        "挂起上限拒绝必须可读：{message}"
    );
    assert!(
        message.contains("插件代码未执行"),
        "挂起上限拒绝必须说明插件代码没有执行：{message}"
    );

    assert_eq!(
        PENDING_IFACE_CALLS.load(Ordering::SeqCst),
        0,
        "拒绝不得进入 provider 的 iface 回调"
    );
    assert_eq!(
        pending_len(handle),
        baseline_pending,
        "拒绝不得改变挂起计数"
    );
    assert_eq!(
        dispatches_len(handle),
        baseline_dispatches,
        "拒绝不得留下派发记录"
    );
    assert_eq!(streams_len(handle), baseline_streams, "拒绝不得改变流计数");

    emit.abort();
    let _ = emit.await;
    fiber.dispose().await;
    app.close().await;
}

// ---------------------------------------------------------------- 流上限

static STREAM_HANDLE: AtomicU64 = AtomicU64::new(0);
static STREAM_CALLS: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn stream_cap_call(handle: u64, op: u32, _: Blob, _: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => {
            STREAM_HANDLE.store(handle, Ordering::SeqCst);
            if !provide_iface(handle, b"cap.stream@1", 1, br#"["hang"]"#, 9) {
                return STATUS_UNSUPPORTED;
            }
            (*out).write(b"")
        }
        OP_CALLBACK => {
            // 流式方法永远挂起：第一条流保持打开，第二条必须被容量拒绝。
            STREAM_CALLS.fetch_add(1, Ordering::SeqCst);
            STATUS_PENDING
        }
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

/// 已有一条打开的流时，第二条流必须被拒绝；拒绝不进入 provider 代码、不增加流/派发计数。
#[tokio::test]
async fn stream_cap_rejects_before_plugin_code_and_keeps_counters() {
    STREAM_HANDLE.store(0, Ordering::SeqCst);
    STREAM_CALLS.store(0, Ordering::SeqCst);

    let app = App::new();
    set_host_limits_for(
        &app.root,
        HostLimits {
            max_streams_per_instance: 1,
            ..HostLimits::default()
        },
    )
    .expect("正整数上限必须被接受");

    let fiber = mount(&app, "cap-stream", stream_cap_call).await;
    assert_eq!(fiber.state(), FiberState::Active);
    let handle = STREAM_HANDLE.load(Ordering::SeqCst);
    assert_ne!(handle, 0, "apply 必须登记实例");

    let first = app
        .root
        .call_iface_stream("cap.stream@1", 1, "hang", json!([]))
        .expect("第一条流必须开得起来");
    assert_eq!(streams_len(handle), 1, "第一条流必须留下记录");
    assert_eq!(STREAM_CALLS.load(Ordering::SeqCst), 1);
    let baseline_dispatches = dispatches_len(handle);
    let baseline_streams = streams_len(handle);

    let error = app
        .root
        .call_iface_stream("cap.stream@1", 1, "hang", json!([]))
        .err()
        .expect("第二条流必须被拒绝");
    let message = error.to_string();
    assert!(
        message.contains("maxStreamsPerInstance"),
        "流上限拒绝必须可读：{message}"
    );
    assert!(
        message.contains("插件代码未执行"),
        "流上限拒绝必须说明插件代码没有执行：{message}"
    );
    assert_eq!(
        STREAM_CALLS.load(Ordering::SeqCst),
        1,
        "被拒绝的第二条流不得进入 provider 代码"
    );
    assert_eq!(streams_len(handle), baseline_streams, "拒绝不得改变流计数");
    assert_eq!(
        dispatches_len(handle),
        baseline_dispatches,
        "拒绝不得留下派发记录"
    );

    drop(first);
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        while streams_len(handle) != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await;
    fiber.dispose().await;
    app.close().await;
}
