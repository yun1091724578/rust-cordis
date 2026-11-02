//! M2c（取消）：`ctx_abort` + 取消树（父子级联）+ **取消桥**（令牌 ⇒ 挂起续延）。
//!
//! 三条纪律（§4 M2c / D3）：
//! 1. `ctx_abort(handle, dispatch_id)` 把该派发**及其后代**的在飞续延全部
//!    `remove`-then-settle（以「已取消」结算），绝不静默、绝不残留；
//! 2. 父子级联靠派发记录里的 `parent`：插件在自己的回调里发起的出站派发，
//!    父节点是**当前正在执行的那次派发**（由宿主的作用域记录，插件不需要传）；
//! 3. 取消桥：派发携带的 `CancelToken` 被取消时，宿主 `select!` 到它并把挂起
//!    续延以错误结算（光丢 future 不够——条目必须移出）。
//!
//! 夹具铁律（D10）：fake `CallFn` 内**禁止 assert**；缺失能力降级成可读状态。
//! **每个测试用自己的夹具静态**（测试二进制里并行跑，共享静态会互相清表）。

use cordis_abi::{
    Blob, Out, KIND_DISPATCH, OP_APPLY, OP_CALLBACK, OP_DISPOSE, STATUS_ERR, STATUS_OK,
    STATUS_PENDING, STATUS_UNSUPPORTED,
};
use cordis_core::{
    App, CancelToken, Config, Ctx, Dyn, ExternValue, FiberState, Inject, Mode, Plugin, Ret,
};
use cordis_host::{apply_via, dispatches_len, host_api, pending_len};
use serde_json::{json, Value};
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

fn payload_of(a: &[u8]) -> Value {
    serde_json::from_slice(a).unwrap_or(Value::Null)
}

fn dispatch_id_of(payload: &[u8]) -> u64 {
    serde_json::from_slice::<Value>(payload)
        .ok()
        .and_then(|value| value.get("dispatch_id").and_then(Value::as_u64))
        .unwrap_or(0)
}

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

/// 插件侧调 `ctx_abort`。
unsafe fn abort(handle: u64, dispatch_id: u64) -> (i32, String) {
    let Some(api) = host_api().ctx_abort else {
        return (-1, "宿主不支持 ctx_abort".into());
    };
    let mut buffer = [0u8; 512];
    let mut out = Out {
        ptr: buffer.as_mut_ptr(),
        cap: buffer.len(),
        len: 0,
    };
    let status = api(handle, dispatch_id, &mut out);
    (
        status,
        String::from_utf8_lossy(&buffer[..out.len]).into_owned(),
    )
}

/// 插件侧调 `ctx_fulfill`。
unsafe fn fulfill(handle: u64, dispatch_id: u64, value: Value) -> (i32, String) {
    let Some(api) = host_api().ctx_fulfill else {
        return (-1, "宿主不支持 ctx_fulfill".into());
    };
    let payload = value.to_string();
    let mut buffer = [0u8; 512];
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

/// 插件侧调 `ctx_dispatch`（出站派发）。
unsafe fn dispatch_out(
    handle: u64,
    event: &str,
    mode: &str,
    args: Value,
    continuation: u64,
) -> (i32, String) {
    let Some(api) = host_api().ctx_dispatch else {
        return (-1, "宿主不支持 ctx_dispatch".into());
    };
    let payload = json!({ "mode": mode, "args": args }).to_string();
    let mut buffer = [0u8; 1024];
    let mut out = Out {
        ptr: buffer.as_mut_ptr(),
        cap: buffer.len(),
        len: 0,
    };
    let status = api(
        handle,
        Blob::new(event.as_bytes()),
        Blob::new(payload.as_bytes()),
        continuation,
        &mut out,
    );
    (
        status,
        String::from_utf8_lossy(&buffer[..out.len]).into_owned(),
    )
}

async fn until(what: &str, mut ready: impl FnMut() -> bool) {
    for _ in 0..400 {
        if ready() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("等待超时：{what}");
}

type Pairs = Mutex<Vec<(u64, String)>>;

/// 取指定实参对应的 dispatch_id。**panic 不发生在持锁期间**（否则锁中毒，
/// 后续 `extern "C"` 里的 unwrap 会 abort 整个测试进程）。
fn pair_id(pairs: &Pairs, arg: &str) -> Option<u64> {
    let guard = pairs
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    guard
        .iter()
        .find(|(_, recorded)| recorded == arg)
        .map(|(id, _)| *id)
}

async fn wait_for_pair(pairs: &Pairs, arg: &str) -> u64 {
    until("插件记录这个实参对应的派发", || {
        pair_id(pairs, arg).is_some()
    })
    .await;
    pair_id(pairs, arg).expect("刚刚才等到，必须有")
}

/// 一个"记录 dispatch_id 就挂起"的假插件（每个测试一份实例）。
macro_rules! pending_plugin {
    ($call:ident, $pairs:ident, $handle:ident, $event:expr) => {
        static $pairs: Pairs = Mutex::new(Vec::new());
        static $handle: Mutex<u64> = Mutex::new(0);

        unsafe extern "C" fn $call(handle: u64, op: u32, a: Blob, _: Blob, out: *mut Out) -> i32 {
            match op {
                OP_APPLY => {
                    *$handle
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) = handle;
                    let status = on_event(handle, $event, 1);
                    $pairs
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .push((0, format!("on:{status}")));
                    (*out).write(b"")
                }
                // 记录 (dispatch_id, 首实参) 然后挂起——永不主动结算。
                OP_CALLBACK => {
                    let payload = payload_of(a.as_bytes());
                    let arg = payload
                        .get("args")
                        .and_then(Value::as_array)
                        .and_then(|args| args.first())
                        .and_then(|first| first.as_str())
                        .unwrap_or("(无)")
                        .to_string();
                    $pairs
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .push((dispatch_id_of(a.as_bytes()), arg));
                    STATUS_PENDING
                }
                OP_DISPOSE => (*out).write(b""),
                _ => STATUS_UNSUPPORTED,
            }
        }
    };
}

// ------------------------------------------------------------------ 测试 1

pending_plugin!(t1_call, T1_PAIRS, T1_HANDLE, "c1/abort");

/// M2c 红测试 1：`ctx_abort` 把在飞的挂起续延以**失败**结算，且不残留。
#[tokio::test]
async fn aborting_a_dispatch_settles_its_pending_continuations_as_failure() {
    let app = App::new();
    let logs: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let logs = logs.clone();
        app.store
            .set_log_sink(Arc::new(move |level, name, message| {
                logs.lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(format!("{level:?}:{name}:{message}"));
            }));
    }
    let fiber = app.root.plugin(abi_plugin("m2c-t1", t1_call), Config::Null);
    fiber.settle().await;
    assert_eq!(fiber.state(), FiberState::Active);
    let handle = *T1_HANDLE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let task = {
        let ctx = app.root.clone();
        tokio::spawn(async move {
            ctx.bail("c1/abort", vec![Dyn::new(ExternValue(json!("only")))])
                .await
        })
    };
    let id = wait_for_pair(&T1_PAIRS, "only").await;
    until("挂起表里有这一条", || pending_len(handle) == 1).await;

    let (status, text) = unsafe { abort(handle, id) };
    assert_eq!(status, STATUS_OK, "ctx_abort 必须成功：{text}");

    let ret = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("取消后派发必须在时限内结算")
        .expect("派发任务不得 panic");
    assert_eq!(json_of(ret), None, "被取消的监听器等于没有意见");

    assert_eq!(pending_len(handle), 0, "取消后挂起表必须归零");
    assert_eq!(
        dispatches_len(handle),
        0,
        "取消后派发记录必须归零（含帧与出站节点）"
    );
    let text = logs
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .join("\n");
    assert!(text.contains("已取消"), "结算必须是可读的取消原因：{text}");
}

// ------------------------------------------------------------------ 测试 2

pending_plugin!(t2_call, T2_PAIRS, T2_HANDLE, "c2/gone");

/// M2c 红测试 2：取消之后再 `ctx_fulfill` 必须被拒绝（条目已移出）。
#[tokio::test]
async fn fulfill_after_abort_is_rejected() {
    let app = App::new();
    let fiber = app.root.plugin(abi_plugin("m2c-t2", t2_call), Config::Null);
    fiber.settle().await;
    let handle = *T2_HANDLE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let task = {
        let ctx = app.root.clone();
        tokio::spawn(async move {
            ctx.bail("c2/gone", vec![Dyn::new(ExternValue(json!("gone")))])
                .await
        })
    };
    let id = wait_for_pair(&T2_PAIRS, "gone").await;

    let (status, _) = unsafe { abort(handle, id) };
    assert_eq!(status, STATUS_OK);

    let (status, text) = unsafe { fulfill(handle, id, json!("late")) };
    assert_eq!(status, STATUS_ERR, "取消后 fulfill 必须被拒绝：{text}");
    assert!(text.contains("不存在或已结算"), "{text}");

    let _ = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("派发必须结算");
}

// ------------------------------------------------------------------ 测试 4

static C4: Mutex<Vec<String>> = Mutex::new(Vec::new());
static C4_PARENT: Mutex<u64> = Mutex::new(0);
static C4_HANDLE: Mutex<u64> = Mutex::new(0);

unsafe extern "C" fn c4_call(handle: u64, op: u32, a: Blob, _: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => {
            *C4_HANDLE
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = handle;
            let one = on_event(handle, "c4/parent", 1);
            let two = on_event(handle, "c4/child", 2);
            C4.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(format!("on:{one}:{two}"));
            (*out).write(b"")
        }
        OP_CALLBACK => {
            let payload = payload_of(a.as_bytes());
            let kind = payload.get("kind").and_then(Value::as_str).unwrap_or("");
            let event = payload.get("event").and_then(Value::as_str).unwrap_or("");
            if kind == KIND_DISPATCH {
                // 出站派发的续延：结果/错误落账。
                let outcome = payload
                    .get("args")
                    .and_then(Value::as_array)
                    .and_then(|args| args.first())
                    .cloned()
                    .unwrap_or(Value::Null);
                C4.lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(format!("child-result:{outcome}"));
                (*out).write(b"")
            } else if event == "c4/parent" {
                let dispatch_id = dispatch_id_of(a.as_bytes());
                *C4_PARENT
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = dispatch_id;
                C4.lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(format!("parent:{dispatch_id}"));
                // 在自己的回调里发起**子派发**：父节点由宿主记录。
                let (status, text) = dispatch_out(handle, "c4/child", "bail", json!([]), 77);
                C4.lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(format!("child-dispatch:{status}:{text}"));
                STATUS_PENDING
            } else {
                let id = dispatch_id_of(a.as_bytes());
                C4.lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(format!("child-inbound:{id}"));
                STATUS_PENDING
            }
        }
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

/// M2c 红测试 4（G20）：取消**父派发**必须级联到它的后代——
/// 子派发的入站挂起与出站续延都被结算，且各恰好一次。
#[tokio::test]
async fn aborting_a_parent_dispatch_cancels_its_child_dispatches() {
    C4.lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();
    *C4_PARENT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = 0;
    *C4_HANDLE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = 0;

    let app = App::new();
    let fiber = app.root.plugin(abi_plugin("m2c-c4", c4_call), Config::Null);
    fiber.settle().await;
    assert_eq!(fiber.state(), FiberState::Active);
    let handle = *C4_HANDLE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let task = {
        let ctx = app.root.clone();
        tokio::spawn(async move { ctx.bail("c4/parent", vec![]).await })
    };
    // 三条记录：父的挂起、子派发的出站节点、子回调的挂起。
    until("父与子都进入在飞状态", || {
        pending_len(handle) == 2 && dispatches_len(handle) == 3
    })
    .await;
    let parent_id = *C4_PARENT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert_ne!(parent_id, 0);

    let (status, text) = unsafe { abort(handle, parent_id) };
    assert_eq!(status, STATUS_OK, "{text}");

    let ret = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("取消后父派发必须在时限内结算")
        .expect("派发任务不得 panic");
    assert_eq!(json_of(ret), None);

    assert_eq!(pending_len(handle), 0, "父与子的挂起都必须被结算");
    assert_eq!(dispatches_len(handle), 0, "出站节点也必须被摘除");

    let log = C4
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    assert!(
        log.iter()
            .any(|entry| entry.starts_with("child-dispatch:0")),
        "ctx_dispatch 必须成功（并把子 dispatch_id 回给插件）：{log:?}"
    );
    assert!(
        log.iter().any(|entry| entry.starts_with("child-inbound")),
        "子派发必须真的送达过插件（否则本测试没有在飞的后代）：{log:?}"
    );
    let results: Vec<&String> = log
        .iter()
        .filter(|entry| entry.starts_with("child-result:"))
        .collect();
    assert_eq!(
        results.len(),
        1,
        "取消必须恰好送达一次（自然完成被抑制）：{log:?}"
    );
    assert!(results[0].contains("已取消"), "{:?}", results[0]);
}

// ------------------------------------------------------------------ 测试 5

static T5_HANDLE: Mutex<u64> = Mutex::new(0);

unsafe extern "C" fn t5_call(handle: u64, op: u32, _: Blob, _: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => {
            *T5_HANDLE
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = handle;
            (*out).write(b"")
        }
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

/// M2c 红测试 5：未知 id / 陈旧句柄上的 `ctx_abort` 都是**可读错误**。
#[tokio::test]
async fn aborting_an_unknown_dispatch_id_is_a_readable_error() {
    *T5_HANDLE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = 0;

    let app = App::new();
    let fiber = app.root.plugin(abi_plugin("m2c-t5", t5_call), Config::Null);
    fiber.settle().await;
    let handle = *T5_HANDLE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let (status, text) = unsafe { abort(handle, 987_654_321) };
    assert_eq!(status, STATUS_ERR, "未知 id 必须失败：{text}");
    assert!(text.contains("不存在"), "错误必须可读：{text}");

    fiber.dispose().await;
    let (status, text) = unsafe { abort(handle, 1) };
    assert_eq!(status, STATUS_ERR);
    assert!(text.contains("已失效"), "陈旧句柄必须被拒绝：{text}");
}

// ------------------------------------------------------------------ 测试 6

pending_plugin!(t6_call, T6_PAIRS, T6_HANDLE, "c6");

/// M2c 红测试 6：取消只作用于目标 id——别的在飞派发照常结算。
#[tokio::test]
async fn abort_does_not_affect_other_dispatch_ids() {
    let app = App::new();
    let fiber = app.root.plugin(abi_plugin("m2c-t6", t6_call), Config::Null);
    fiber.settle().await;
    let handle = *T6_HANDLE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let first = {
        let ctx = app.root.clone();
        tokio::spawn(async move {
            ctx.bail("c6", vec![Dyn::new(ExternValue(json!("first")))])
                .await
        })
    };
    let second = {
        let ctx = app.root.clone();
        tokio::spawn(async move {
            ctx.bail("c6", vec![Dyn::new(ExternValue(json!("second")))])
                .await
        })
    };
    let first_id = wait_for_pair(&T6_PAIRS, "first").await;
    let second_id = wait_for_pair(&T6_PAIRS, "second").await;
    until("两条派发都进入挂起", || pending_len(handle) == 2).await;

    assert_ne!(first_id, second_id);
    let (status, _) = unsafe { abort(handle, first_id) };
    assert_eq!(status, STATUS_OK);

    // 第二条仍然可以正常结算。
    let (status, text) = unsafe { fulfill(handle, second_id, json!("second-value")) };
    assert_eq!(status, STATUS_OK, "{text}");

    let r1 = tokio::time::timeout(Duration::from_secs(10), first)
        .await
        .expect("first 必须结算")
        .unwrap();
    let r2 = tokio::time::timeout(Duration::from_secs(10), second)
        .await
        .expect("second 必须结算")
        .unwrap();
    assert_eq!(json_of(r1), None, "first 被取消");
    assert_eq!(json_of(r2), Some(json!("second-value")), "second 不受影响");
    assert_eq!(pending_len(handle), 0);
}

// ------------------------------------------------------------------ 测试 7

pending_plugin!(t7_call, T7_PAIRS, T7_HANDLE, "c7");

/// M2c 红测试 7（**取消桥**，D3）：派发携带的令牌被取消 ⇒
/// 宿主 `select!` 到它，把挂起续延以错误结算并**移出条目**（光丢 future 不够）。
#[tokio::test]
async fn cancelled_token_settles_its_pending_continuation() {
    let app = App::new();
    let logs: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let logs = logs.clone();
        app.store
            .set_log_sink(Arc::new(move |level, name, message| {
                logs.lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(format!("{level:?}:{name}:{message}"));
            }));
    }
    let fiber = app.root.plugin(abi_plugin("m2c-t7", t7_call), Config::Null);
    fiber.settle().await;
    let handle = *T7_HANDLE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let token = CancelToken::new();
    let future = app.store.dispatch_cancellable(
        Mode::Bail,
        "c7",
        &app.root,
        vec![Dyn::new(ExternValue(json!("bridged")))],
        None,
        None,
        Some(token.clone()),
    );
    let task = tokio::spawn(future);
    until("派发进入挂起", || pending_len(handle) == 1).await;

    assert!(!token.is_cancelled());
    token.cancel();
    token.cancel(); // 幂等：第二次是无害空操作

    let ret = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("令牌取消后派发必须在时限内结算")
        .expect("派发任务不得 panic");
    assert_eq!(json_of(ret), None, "被取消的监听器等于没有意见");
    assert!(token.is_cancelled());

    assert_eq!(
        pending_len(handle),
        0,
        "取消桥必须把条目移出（否则晚到的 fulfill 会悄悄成功）"
    );
    assert_eq!(dispatches_len(handle), 0);
    let text = logs
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .join("\n");
    assert!(text.contains("已取消"), "结算必须说明是取消：{text}");
}
