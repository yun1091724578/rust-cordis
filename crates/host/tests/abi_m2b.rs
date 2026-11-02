//! M2b（waterfall 跨 ABI）：`ctx_next` + `kind:"next"` 回调。
//!
//! 协议（D6，第 3 轮评审修正后）：
//! 1. 宿主派发 waterfall 时把「剩余链」按 `dispatch_id` 存在宿主侧，并以
//!    `{kind:"event", mode:"waterfall", dispatch_id, args}` 回调插件；
//! 2. 插件在**这次回调内部**决定：直接返回 ⇒ 链到此为止；要 next ⇒ 调
//!    `ctx_next(handle, dispatch_id, new_args, on_done)`（宿主立刻用一个内部任务
//!    续跑剩余链，不阻塞本次回调）然后返回 `STATUS_PENDING`；
//! 3. 下游完成 ⇒ 宿主回调插件 `{kind:"next", id:on_done, args:[{"result": …}]}`；
//!    插件拿到后 `ctx_fulfill(dispatch_id, 终值)`。
//!
//! **同步快速路径**（D6）：若剩余链在首次 poll 就完成，`kind:"next"` 在
//! `ctx_next` 内部**内联**投递（于是"回调内 next、回调内结算"），但插件仍须
//! 返回 `PENDING` 并走 `ctx_fulfill` —— 这条语义由测试 6 钉死。
//!
//! 夹具铁律（D10）：fake `CallFn` 内**禁止 assert**（panic 跨 `extern "C"`
//! 即 abort）；缺失能力一律降级成可读状态，不 `.expect()`。

use cordis_abi::{
    Blob, Out, KIND_NEXT, OP_APPLY, OP_CALLBACK, OP_DISPOSE, STATUS_PENDING, STATUS_UNSUPPORTED,
};
use cordis_core::{App, Config, Ctx, Dyn, ExternValue, FiberState, Inject, Plugin, Ret};
use cordis_host::{apply_via, host_api, pending_len, waterfall_len};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

// ------------------------------------------------------------------ 夹具

fn abi_plugin(name: &'static str, call: cordis_host::CallFn) -> Arc<dyn Plugin> {
    Arc::new(cordis_core::FnPlugin {
        name,
        key: cordis_core::unique_key(name),
        inject: Inject::default(),
        validator: None,
        f: Arc::new(move |ctx: Ctx, config: Config| apply_via(call, ctx, config, name)),
    })
}

fn json_of(ret: Ret) -> Option<Value> {
    ret.and_then(|value| value.downcast::<ExternValue>())
        .map(|value| value.0.clone())
}

/// 从 OP_CALLBACK 载荷里取宿主铸造的 `dispatch_id`。
fn dispatch_id_of(payload: &[u8]) -> u64 {
    serde_json::from_slice::<Value>(payload)
        .ok()
        .and_then(|value| value.get("dispatch_id").and_then(Value::as_u64))
        .unwrap_or(0)
}

fn payload_of(a: &[u8]) -> Value {
    serde_json::from_slice(a).unwrap_or(Value::Null)
}

/// 插件侧调 `ctx_on` 注册一个监听器。宿主不支持时返回 `-1`。
unsafe fn on_event(handle: u64, event: &str, cb: u64) -> i32 {
    let Some(api) = host_api().ctx_on else {
        return -1;
    };
    let mut buffer = [0u8; 256];
    let mut out = Out {
        ptr: buffer.as_mut_ptr(),
        cap: buffer.len(),
        len: 0,
    };
    api(handle, Blob::new(event.as_bytes()), cb, &mut out)
}

/// 插件侧调 `ctx_next`：返回 `(status, 文本)`。宿主不支持时返回 `-1`。
unsafe fn next_call(handle: u64, dispatch_id: u64, args: Value, on_done: u64) -> (i32, String) {
    let Some(api) = host_api().ctx_next else {
        return (-1, "宿主不支持 ctx_next".into());
    };
    let payload = json!({ "args": args }).to_string();
    let mut buffer = [0u8; 512];
    let mut out = Out {
        ptr: buffer.as_mut_ptr(),
        cap: buffer.len(),
        len: 0,
    };
    let status = api(
        handle,
        dispatch_id,
        Blob::new(payload.as_bytes()),
        on_done,
        &mut out,
    );
    (
        status,
        String::from_utf8_lossy(&buffer[..out.len]).into_owned(),
    )
}

/// 插件侧调 `ctx_fulfill`：返回 `(status, 文本)`。
unsafe fn fulfill(handle: u64, dispatch_id: u64, value: Value) -> (i32, String) {
    let Some(api) = host_api().ctx_fulfill else {
        return (-1, "宿主不支持 ctx_fulfill".into());
    };
    let payload = value.to_string();
    let mut buffer = [0u8; 1024];
    let mut out = Out {
        ptr: buffer.as_mut_ptr(),
        cap: buffer.len(),
        len: 0,
    };
    let status = api(handle, dispatch_id, Blob::new(payload.as_bytes()), &mut out);
    (
        status,
        String::from_utf8_lossy(&buffer[..out.len]).into_owned(),
    )
}

/// 从 `kind:"next"` 载荷里取下游值。
fn downstream_of(payload: &Value) -> Value {
    payload
        .get("args")
        .and_then(Value::as_array)
        .and_then(|args| args.first())
        .and_then(|first| first.get("result"))
        .cloned()
        .unwrap_or(Value::Null)
}

/// 一个把值包成 `ExternValue` 的 tail。
fn tail_of(value: Value) -> BoxFutureOfRet {
    Box::pin(async move { Some(Dyn::new(ExternValue(value))) })
}

type BoxFutureOfRet = cordis_core::BoxFuture<Ret>;

/// 一次 waterfall 派发（统一的超时护栏：失败可读，绝不挂死）。
async fn run_waterfall(app: &App, event: &str, args: Vec<Dyn>, tail: BoxFutureOfRet) -> Ret {
    tokio::time::timeout(
        Duration::from_secs(10),
        app.root.waterfall(event, args, Some(tail), None),
    )
    .await
    .expect("waterfall 派发必须在时限内结算")
}

// ------------------------------------------------------------------ 测试 1

static T1: Mutex<Vec<String>> = Mutex::new(Vec::new());
static T1_HANDLE: Mutex<u64> = Mutex::new(0);

unsafe extern "C" fn t1_call(handle: u64, op: u32, a: Blob, _: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => {
            *T1_HANDLE.lock().unwrap() = handle;
            let status = on_event(handle, "w1", 1);
            T1.lock().unwrap().push(format!("on:{status}"));
            (*out).write(b"")
        }
        OP_CALLBACK => {
            let payload = payload_of(a.as_bytes());
            let mode = payload
                .get("mode")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            T1.lock().unwrap().push(format!("mode:{mode}"));
            // 直接返回 ⇒ 链到此为止（不调 next）。
            (*out).write(br#"{"ret":"mine"}"#)
        }
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

/// M2b 红测试 1：直接返回**短路**整条链——tail 不得运行。
#[tokio::test]
async fn abi_handler_can_return_a_final_value_and_stop_the_chain() {
    T1.lock().unwrap().clear();
    *T1_HANDLE.lock().unwrap() = 0;

    let app = App::new();
    let fiber = app.root.plugin(abi_plugin("m2b-t1", t1_call), Config::Null);
    fiber.settle().await;
    assert_eq!(fiber.state(), FiberState::Active);
    let handle = *T1_HANDLE.lock().unwrap();

    let ran = Arc::new(AtomicBool::new(false));
    let spy = ran.clone();
    let tail: BoxFutureOfRet = Box::pin(async move {
        spy.store(true, Ordering::SeqCst);
        Some(Dyn::new(ExternValue(json!("tail"))))
    });

    let ret = run_waterfall(&app, "w1", vec![], tail).await;
    assert_eq!(
        json_of(ret),
        Some(json!("mine")),
        "插件的直接返回值就是派发结果"
    );
    assert!(
        !ran.load(Ordering::SeqCst),
        "直接返回必须短路：tail 不得运行"
    );

    let log = T1.lock().unwrap().clone();
    assert_eq!(log.len(), 2, "{log:?}");
    assert_eq!(log[0], "on:0");
    assert_eq!(
        log[1], "mode:waterfall",
        "waterfall 派发必须把 mode 下发（插件据此决定要不要 next）"
    );
    assert_eq!(pending_len(handle), 0);
    assert_eq!(waterfall_len(handle), 0);
}

// ------------------------------------------------------------------ 测试 2

static T2: Mutex<Vec<String>> = Mutex::new(Vec::new());
static T2_PEND: Mutex<u64> = Mutex::new(0);
static T2_HANDLE: Mutex<u64> = Mutex::new(0);

unsafe extern "C" fn t2_call(handle: u64, op: u32, a: Blob, _: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => {
            *T2_HANDLE.lock().unwrap() = handle;
            let status = on_event(handle, "w2", 1);
            T2.lock().unwrap().push(format!("on:{status}"));
            (*out).write(b"")
        }
        OP_CALLBACK => {
            let payload = payload_of(a.as_bytes());
            let kind = payload.get("kind").and_then(Value::as_str).unwrap_or("");
            if kind == KIND_NEXT {
                let downstream = downstream_of(&payload);
                T2.lock().unwrap().push(format!("next:{downstream}"));
                let dispatch_id = *T2_PEND.lock().unwrap();
                let (status, _) = fulfill(handle, dispatch_id, downstream);
                T2.lock().unwrap().push(format!("fulfill:{status}"));
                (*out).write(b"")
            } else {
                let dispatch_id = dispatch_id_of(a.as_bytes());
                *T2_PEND.lock().unwrap() = dispatch_id;
                let args = payload.get("args").cloned().unwrap_or(Value::Null);
                T2.lock().unwrap().push("waterfall".into());
                let (status, text) = next_call(handle, dispatch_id, args, 91);
                T2.lock()
                    .unwrap()
                    .push(format!("next-called:{status}:{text}"));
                STATUS_PENDING
            }
        }
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

/// M2b 红测试 2：**原样转发**——`next` 的下游值流回插件，插件再 `fulfill` 回去。
#[tokio::test]
async fn abi_handler_can_delegate_to_next_verbatim() {
    T2.lock().unwrap().clear();
    *T2_PEND.lock().unwrap() = 0;
    *T2_HANDLE.lock().unwrap() = 0;

    let app = App::new();
    let fiber = app.root.plugin(abi_plugin("m2b-t2", t2_call), Config::Null);
    fiber.settle().await;
    assert_eq!(fiber.state(), FiberState::Active);
    let handle = *T2_HANDLE.lock().unwrap();

    let ret = run_waterfall(&app, "w2", vec![], tail_of(json!("tail"))).await;
    assert_eq!(
        json_of(ret),
        Some(json!("tail")),
        "原样转发必须把下游值带回派发方"
    );

    let log = T2.lock().unwrap().clone();
    // 顺序是同步快速路径的直接证据：`kind:next` 的交付（next/fulfill）
    // 发生在 `ctx_next` **返回之前**，所以排在 next-called 之前。
    assert_eq!(
        log,
        vec![
            "on:0".to_string(),
            "waterfall".into(),
            "next:\"tail\"".into(),
            "fulfill:0".into(),
            "next-called:0:".into(),
        ],
        "原样转发的完整时序：{log:?}"
    );
    assert_eq!(pending_len(handle), 0);
    assert_eq!(waterfall_len(handle), 0);
}

// ------------------------------------------------------------------ 测试 3

static T3: Mutex<Vec<String>> = Mutex::new(Vec::new());
static T3_PEND: Mutex<u64> = Mutex::new(0);

unsafe extern "C" fn t3_call(handle: u64, op: u32, a: Blob, _: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => {
            let status = on_event(handle, "w3", 1);
            T3.lock().unwrap().push(format!("on:{status}"));
            (*out).write(b"")
        }
        OP_CALLBACK => {
            let payload = payload_of(a.as_bytes());
            let kind = payload.get("kind").and_then(Value::as_str).unwrap_or("");
            if kind == KIND_NEXT {
                let downstream = downstream_of(&payload);
                let wrapped = json!({ "wrapped": downstream });
                T3.lock().unwrap().push(format!("next:{downstream}"));
                let dispatch_id = *T3_PEND.lock().unwrap();
                let (status, _) = fulfill(handle, dispatch_id, wrapped);
                T3.lock().unwrap().push(format!("fulfill:{status}"));
                (*out).write(b"")
            } else {
                let dispatch_id = dispatch_id_of(a.as_bytes());
                *T3_PEND.lock().unwrap() = dispatch_id;
                let args = payload.get("args").cloned().unwrap_or(Value::Null);
                let (status, _) = next_call(handle, dispatch_id, args, 92);
                T3.lock().unwrap().push(format!("next-called:{status}"));
                STATUS_PENDING
            }
        }
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

/// M2b 红测试 3：**调 next 再改写**——下游值被包一层后成为派发结果。
#[tokio::test]
async fn abi_handler_can_wrap_the_next_result() {
    T3.lock().unwrap().clear();
    *T3_PEND.lock().unwrap() = 0;

    let app = App::new();
    let fiber = app.root.plugin(abi_plugin("m2b-t3", t3_call), Config::Null);
    fiber.settle().await;
    assert_eq!(fiber.state(), FiberState::Active);

    let ret = run_waterfall(&app, "w3", vec![], tail_of(json!("tail"))).await;
    assert_eq!(
        json_of(ret),
        Some(json!({ "wrapped": "tail" })),
        "改写后的值是派发结果"
    );

    let log = T3.lock().unwrap().clone();
    assert!(
        log.iter().any(|entry| entry == "next:\"tail\""),
        "on_done 必须收到下游值：{log:?}"
    );
    assert!(
        log.iter().any(|entry| entry.starts_with("fulfill:0")),
        "{log:?}"
    );
}

// ------------------------------------------------------------------ 测试 4

static T4: Mutex<Vec<String>> = Mutex::new(Vec::new());
static T4_PEND: Mutex<u64> = Mutex::new(0);

unsafe extern "C" fn t4_call(handle: u64, op: u32, a: Blob, _: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => {
            let status = on_event(handle, "w4", 1);
            T4.lock().unwrap().push(format!("on:{status}"));
            (*out).write(b"")
        }
        OP_CALLBACK => {
            let payload = payload_of(a.as_bytes());
            let kind = payload.get("kind").and_then(Value::as_str).unwrap_or("");
            if kind == KIND_NEXT {
                let downstream = downstream_of(&payload);
                let dispatch_id = *T4_PEND.lock().unwrap();
                let (status, _) = fulfill(handle, dispatch_id, downstream);
                T4.lock().unwrap().push(format!("fulfill:{status}"));
                (*out).write(b"")
            } else {
                let dispatch_id = dispatch_id_of(a.as_bytes());
                *T4_PEND.lock().unwrap() = dispatch_id;
                // 换实参：下游应当看到的是**替换后**的实参。
                let (status, _) = next_call(handle, dispatch_id, json!(["replaced"]), 93);
                T4.lock().unwrap().push(format!("next-called:{status}"));
                STATUS_PENDING
            }
        }
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

/// M2b 红测试 4：`next(new_args)` 的实参**替换**下游实参（JS `await next(args)` 语义）。
#[tokio::test]
async fn next_arguments_replace_the_downstream_arguments() {
    T4.lock().unwrap().clear();
    *T4_PEND.lock().unwrap() = 0;

    let app = App::new();
    let fiber = app.root.plugin(abi_plugin("m2b-t4", t4_call), Config::Null);
    fiber.settle().await;
    assert_eq!(fiber.state(), FiberState::Active);

    let tail: BoxFutureOfRet =
        Box::pin(async move { Some(Dyn::new(ExternValue(json!("tail-ok")))) });
    // tail 自己不记实参（BoxFuture 里没有 Call）——用**中间的原生 handler** 记。
    // 链序：ABI 插件（外层，先注册）→ 原生记录者（内层）→ tail。
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let recorder = seen.clone();
    app.root.on_with(
        "w4",
        move |call: cordis_core::Call| {
            let recorder = recorder.clone();
            Box::pin(async move {
                let first = call
                    .args
                    .first()
                    .and_then(|value| value.downcast::<ExternValue>())
                    .map(|value| value.0.to_string())
                    .unwrap_or_else(|| "(无实参)".to_string());
                recorder.lock().unwrap().push(first);
                call.next.call(call.args.clone()).await
            })
        },
        false,
        false,
    );

    let ret = run_waterfall(
        &app,
        "w4",
        vec![Dyn::new(ExternValue(json!("original")))],
        tail,
    )
    .await;
    assert_eq!(json_of(ret), Some(json!("tail-ok")));

    let seen = seen.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec!["\"replaced\"".to_string()],
        "下游必须看到**替换后**的实参（而不是原实参）"
    );
}

// ------------------------------------------------------------------ 测试 5

static T5: Mutex<Vec<String>> = Mutex::new(Vec::new());
static T5_PEND: Mutex<u64> = Mutex::new(0);
static T5_TAIL_RUNS: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn t5_call(handle: u64, op: u32, a: Blob, _: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => {
            let status = on_event(handle, "w5", 1);
            T5.lock().unwrap().push(format!("on:{status}"));
            (*out).write(b"")
        }
        OP_CALLBACK => {
            let payload = payload_of(a.as_bytes());
            let kind = payload.get("kind").and_then(Value::as_str).unwrap_or("");
            if kind == KIND_NEXT {
                let downstream = downstream_of(&payload);
                let dispatch_id = *T5_PEND.lock().unwrap();
                let (status, _) = fulfill(handle, dispatch_id, downstream);
                T5.lock().unwrap().push(format!("fulfill:{status}"));
                (*out).write(b"")
            } else {
                let dispatch_id = dispatch_id_of(a.as_bytes());
                *T5_PEND.lock().unwrap() = dispatch_id;
                let args = payload.get("args").cloned().unwrap_or(Value::Null);
                let (status, _) = next_call(handle, dispatch_id, args, 94);
                T5.lock().unwrap().push(format!("next-called:{status}"));
                T5.lock().unwrap().push("pended".into());
                STATUS_PENDING
            }
        }
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

/// M2b 红测试 5：**链尾**（最后一个 handler 就 delegate 时）仍然运行，且恰好一次。
#[tokio::test]
async fn chain_tail_runs_when_the_last_handler_delegates() {
    T5.lock().unwrap().clear();
    *T5_PEND.lock().unwrap() = 0;
    T5_TAIL_RUNS.store(0, Ordering::SeqCst);

    let app = App::new();
    let fiber = app.root.plugin(abi_plugin("m2b-t5", t5_call), Config::Null);
    fiber.settle().await;
    assert_eq!(fiber.state(), FiberState::Active);

    let tail: BoxFutureOfRet = Box::pin(async move {
        T5_TAIL_RUNS.fetch_add(1, Ordering::SeqCst);
        Some(Dyn::new(ExternValue(json!("tail"))))
    });
    let ret = run_waterfall(&app, "w5", vec![], tail).await;
    assert_eq!(json_of(ret), Some(json!("tail")));
    assert_eq!(
        T5_TAIL_RUNS.load(Ordering::SeqCst),
        1,
        "链尾必须恰好运行一次"
    );

    let log = T5.lock().unwrap().clone();
    assert!(
        log.iter().any(|entry| entry == "pended"),
        "外层回调必须以 PENDING 返回：{log:?}"
    );
    assert!(
        log.iter().any(|entry| entry.starts_with("fulfill:0")),
        "{log:?}"
    );
}

// ------------------------------------------------------------------ 测试 6

static T6: Mutex<Vec<String>> = Mutex::new(Vec::new());
static T6_PEND: Mutex<u64> = Mutex::new(0);

unsafe extern "C" fn t6_call(handle: u64, op: u32, a: Blob, _: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => {
            let status = on_event(handle, "w6", 1);
            T6.lock().unwrap().push(format!("on:{status}"));
            (*out).write(b"")
        }
        OP_CALLBACK => {
            let payload = payload_of(a.as_bytes());
            let kind = payload.get("kind").and_then(Value::as_str).unwrap_or("");
            if kind == KIND_NEXT {
                let downstream = downstream_of(&payload);
                T6.lock().unwrap().push(format!("next:{downstream}"));
                let dispatch_id = *T6_PEND.lock().unwrap();
                let (status, _) = fulfill(handle, dispatch_id, downstream);
                T6.lock().unwrap().push(format!("fulfill:{status}"));
                (*out).write(b"")
            } else {
                T6.lock().unwrap().push("start".into());
                let dispatch_id = dispatch_id_of(a.as_bytes());
                *T6_PEND.lock().unwrap() = dispatch_id;
                let args = payload.get("args").cloned().unwrap_or(Value::Null);
                let (status, _) = next_call(handle, dispatch_id, args, 95);
                T6.lock().unwrap().push(format!("next-called:{status}"));
                T6.lock().unwrap().push("end".into());
                STATUS_PENDING
            }
        }
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

/// M2b 红测试 6：**全同步链**也必须走「返回 PENDING + fulfill」，且 `kind:"next"`
/// 在 `ctx_next` 内部**内联**投递（D6 的同步快速路径）——不比墙钟，看的是调用序。
#[tokio::test]
async fn a_fully_synchronous_chain_still_requires_exactly_one_pending_round_trip() {
    T6.lock().unwrap().clear();
    *T6_PEND.lock().unwrap() = 0;

    let app = App::new();
    let fiber = app.root.plugin(abi_plugin("m2b-t6", t6_call), Config::Null);
    fiber.settle().await;
    assert_eq!(fiber.state(), FiberState::Active);

    let ret = run_waterfall(&app, "w6", vec![], tail_of(json!("tail"))).await;
    assert_eq!(json_of(ret), Some(json!("tail")));

    let log = T6.lock().unwrap().clone();
    assert_eq!(
        log,
        vec![
            "on:0".to_string(),
            "start".into(),
            "next:\"tail\"".into(),
            "fulfill:0".into(),
            "next-called:0".into(),
            "end".into(),
        ],
        "同步快速路径：kind:next 必须在 next-called 返回**之前**内联送达\
         （start → next → fulfill → next-called → end）；外层仍以 PENDING 返回"
    );
}

// ------------------------------------------------------------------ 测试 7

static T7: Mutex<Vec<String>> = Mutex::new(Vec::new());
static T7_HANDLE: Mutex<u64> = Mutex::new(0);

unsafe extern "C" fn t7_call(handle: u64, op: u32, a: Blob, _: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => {
            *T7_HANDLE.lock().unwrap() = handle;
            let status = on_event(handle, "w7", 1);
            T7.lock().unwrap().push(format!("on:{status}"));
            (*out).write(b"")
        }
        // 记下 dispatch_id 就挂起——**不调 next**，帧因此悬置。
        OP_CALLBACK => {
            let dispatch_id = dispatch_id_of(a.as_bytes());
            T7.lock().unwrap().push(format!("waterfall:{dispatch_id}"));
            STATUS_PENDING
        }
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

struct DropSpy(Arc<AtomicBool>);

impl Drop for DropSpy {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// M2b 红测试 7：悬置的 waterfall 帧随实例注销**被清理**——
/// 剩余链（含它的 future）真的被丢，不是泄漏（DropSpy 直接观测）。
#[tokio::test]
async fn unfinished_waterfall_frame_is_cleaned_on_unregister() {
    T7.lock().unwrap().clear();
    *T7_HANDLE.lock().unwrap() = 0;

    let app = App::new();
    let fiber = app.root.plugin(abi_plugin("m2b-t7", t7_call), Config::Null);
    fiber.settle().await;
    assert_eq!(fiber.state(), FiberState::Active);
    let handle = *T7_HANDLE.lock().unwrap();

    let dropped = Arc::new(AtomicBool::new(false));
    let spy = DropSpy(dropped.clone());
    let tail: BoxFutureOfRet = Box::pin(async move {
        let _spy = spy; // 帧被丢 ⇒ 这个 future 被丢 ⇒ spy 触发
        Some(Dyn::new(ExternValue(json!("tail"))))
    });

    let task = {
        let ctx = app.root.clone();
        tokio::spawn(async move { ctx.waterfall("w7", vec![], Some(tail), None).await })
    };
    // 等挂起 + 帧登记（不比墙钟：条件是可观测计数）。
    let mut ready = false;
    for _ in 0..400 {
        ready = pending_len(handle) == 1 && waterfall_len(handle) == 1;
        if ready {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(ready, "派发必须挂起且留下一个 waterfall 帧");
    assert!(!dropped.load(Ordering::SeqCst), "挂起期间剩余链必须还活着");

    fiber.dispose().await;

    let ret = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("注销后派发必须在时限内结算")
        .expect("派发任务不得 panic");
    assert_eq!(json_of(ret), None, "被结算成失败的监听器等于没有意见");

    assert_eq!(pending_len(handle), 0);
    assert_eq!(waterfall_len(handle), 0);
    assert!(
        dropped.load(Ordering::SeqCst),
        "帧必须被丢掉（剩余链的 future 随帧一起释放，不是泄漏）"
    );
}

// ------------------------------------------------------------------ 测试 8（守卫）

/// M2b 守卫：**原生** waterfall 路径不回归（`Next` 升级只加了"可带新实参"）。
/// 既有 `kernel.rs` 的 chain 用例是它的核心版；这条把同一形状放到 host 边界跑。
#[tokio::test]
async fn plain_native_waterfall_is_unchanged() {
    let app = App::new();
    app.root.on("w8", |call| {
        Box::pin(async move {
            let downstream = call.next.call(call.args.clone()).await;
            let n = downstream
                .and_then(|value| value.downcast::<u32>())
                .map(|value| *value)
                .unwrap_or(0);
            Some(Dyn::new(n + 1))
        })
    });

    let ret = run_waterfall(
        &app,
        "w8",
        vec![],
        Box::pin(async { Some(Dyn::new(41u32)) }),
    )
    .await;
    let n = ret
        .and_then(|value| value.downcast::<u32>())
        .map(|value| *value);
    assert_eq!(
        n,
        Some(42),
        "原生链：tail 给 41，外层 +1 ⇒ 42（与 M2b 之前逐字一致）"
    );
}
