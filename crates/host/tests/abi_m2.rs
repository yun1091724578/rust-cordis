//! M2（入站异步）：`dispatch_id` + `STATUS_PENDING` + `ctx_fulfill`。
//!
//! 全部无 DLL（D9）：ABI 插件经 [`cordis_host::apply_via`] + 手写 `CallFn`
//! 构造；宿主函数经 [`cordis_host::host_api`] 的**真表**调用。
//!
//! 键的所有权（D3）：挂起表以 **`(实例, dispatch_id)`** 为键——
//! `dispatch_id` 是宿主在**每次**入站回调前铸造并随载荷下发的；
//! 同一 handler 的并发派发因此各自成条（G17 修的正是"用注册 id 当键"）。
//!
//! 夹具铁律（D10）：fake `CallFn` 内**禁止 assert**（panic 跨 `extern "C"`
//! 即 abort），结局只记账进 static `Mutex`；跨线程状态不用 `thread_local`。

use cordis_abi::{
    Blob, Out, OP_APPLY, OP_CALLBACK, OP_DISPOSE, STATUS_ERR, STATUS_PENDING, STATUS_UNSUPPORTED,
};
use cordis_core::{App, Config, Ctx, Dyn, ExternValue, FiberState, Inject, Plugin, Ret};
use cordis_host::{apply_via, host_api, pending_len};
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

/// 插件侧调 `ctx_fulfill`：返回 `(status, 文本)`。
/// 宿主不支持时返回 `-1`——**绝不在 `extern "C"` 里 panic**（D10 夹具铁律）。
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
    let text = String::from_utf8_lossy(&buffer[..out.len]).into_owned();
    (status, text)
}

/// 插件侧调 `ctx_on` 注册一个监听器。宿主不支持时返回 `-1`（同上）。
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

/// 从 OP_CALLBACK 载荷里取宿主铸造的 `dispatch_id`。
fn dispatch_id_of(payload: &[u8]) -> u64 {
    serde_json::from_slice::<Value>(payload)
        .ok()
        .and_then(|value| value.get("dispatch_id").and_then(Value::as_u64))
        .unwrap_or(0)
}

/// `Ret` → JSON（只有 `ExternValue` 是 ABI 可见的）。
fn json_of(ret: Ret) -> Option<Value> {
    ret.and_then(|value| value.downcast::<ExternValue>())
        .map(|value| value.0.clone())
}

/// 轮询直到条件成立（异步推进留时间），超时即败。
async fn until(what: &str, mut ready: impl FnMut() -> bool) {
    for _ in 0..500 {
        if ready() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("等待超时：{what}");
}

// ------------------------------------------------------------------ 测试 1

static T1: Mutex<Vec<String>> = Mutex::new(Vec::new());
static T1_HANDLE: Mutex<u64> = Mutex::new(0);

unsafe extern "C" fn t1_call(handle: u64, op: u32, a: Blob, _: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => {
            *T1_HANDLE.lock().unwrap() = handle;
            let status = on_event(handle, "t1", 7);
            T1.lock().unwrap().push(format!("on:{status}"));
            (*out).write(b"")
        }
        OP_CALLBACK => {
            let id = dispatch_id_of(a.as_bytes());
            // 在回调**内部**结算一次，再结算一次（第二次必须是可读错误）。
            let (first, text) = fulfill(handle, id, json!("v"));
            let (second, text2) = fulfill(handle, id, json!("again"));
            T1.lock().unwrap().push(format!("f1:{first}:{text}"));
            T1.lock().unwrap().push(format!("f2:{second}:{text2}"));
            STATUS_PENDING
        }
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

/// M2 红测试 1：一次挂起**恰好结算一次**——第二次 `ctx_fulfill` 是可读错误。
#[tokio::test]
async fn pending_inbound_continuation_is_settled_exactly_once() {
    T1.lock().unwrap().clear();
    *T1_HANDLE.lock().unwrap() = 0;

    let app = App::new();
    let fiber = app.root.plugin(abi_plugin("m2-t1", t1_call), Config::Null);
    fiber.settle().await;
    assert_eq!(fiber.state(), FiberState::Active, "假插件应装载成功");
    let handle = *T1_HANDLE.lock().unwrap();

    let ret = tokio::time::timeout(Duration::from_secs(10), app.root.bail("t1", vec![]))
        .await
        .expect("派发必须在时限内结算（挂起泄漏会在这里暴露）");
    assert_eq!(
        json_of(ret),
        Some(json!("v")),
        "派发必须带回 fulfill 送来的值"
    );

    let log = T1.lock().unwrap().clone();
    assert_eq!(log.len(), 3, "应恰有三条记录：{log:?}");
    assert_eq!(log[0], "on:0", "监听器注册必须成功");
    assert!(
        log[1].starts_with("f1:0:"),
        "第一次 fulfill 必须成功：{}",
        log[1]
    );
    assert!(
        log[2].starts_with("f2:1:"),
        "第二次 fulfill 必须是错误：{}",
        log[2]
    );
    assert!(log[2].contains("已结算"), "错误必须说明原因：{}", log[2]);
    assert_eq!(pending_len(handle), 0, "结算后挂起表必须清空");
}

// ------------------------------------------------------------------ 测试 2

static T2: Mutex<Vec<String>> = Mutex::new(Vec::new());
static T2_PEND: Mutex<Vec<(u64, String)>> = Mutex::new(Vec::new());
static T2_HANDLE: Mutex<u64> = Mutex::new(0);

unsafe extern "C" fn t2_call(handle: u64, op: u32, a: Blob, _: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => {
            *T2_HANDLE.lock().unwrap() = handle;
            let one = on_event(handle, "t2", 7);
            let two = on_event(handle, "t2/settle", 8);
            T2.lock().unwrap().push(format!("on:{one}:{two}"));
            (*out).write(b"")
        }
        OP_CALLBACK => {
            let payload: Value = serde_json::from_slice(a.as_bytes()).unwrap_or(Value::Null);
            let id = payload.get("id").and_then(Value::as_u64).unwrap_or(0);
            if id == 8 {
                // settle 事件：把已记录的挂起按**记录顺序**结算成 A / B。
                let recorded = T2_PEND.lock().unwrap().clone();
                for (index, (dispatch_id, arg)) in recorded.iter().enumerate() {
                    let value = if index == 0 { json!("A") } else { json!("B") };
                    let (status, text) = fulfill(handle, *dispatch_id, value);
                    T2.lock()
                        .unwrap()
                        .push(format!("settle:{arg}:{status}:{text}"));
                }
                (*out).write(b"")
            } else {
                let arg = payload
                    .get("args")
                    .and_then(Value::as_array)
                    .and_then(|args| args.first())
                    .and_then(Value::as_str)
                    .unwrap_or("?")
                    .to_string();
                T2_PEND
                    .lock()
                    .unwrap()
                    .push((dispatch_id_of(a.as_bytes()), arg));
                STATUS_PENDING
            }
        }
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

/// M2 红测试 2（G17）：同一 handler 的两次**并发**派发各自挂起、各自结算，
/// 不互相覆盖——这正是"用注册 id 当挂起键"会坏掉的地方。
#[tokio::test]
async fn two_concurrent_dispatches_of_the_same_handler_settle_independently() {
    T2.lock().unwrap().clear();
    T2_PEND.lock().unwrap().clear();

    let app = App::new();
    let fiber = app.root.plugin(abi_plugin("m2-t2", t2_call), Config::Null);
    fiber.settle().await;
    assert_eq!(fiber.state(), FiberState::Active);
    let handle = *T2_HANDLE.lock().unwrap();

    let first = {
        let ctx = app.root.clone();
        tokio::spawn(async move {
            ctx.bail("t2", vec![Dyn::new(ExternValue(json!("first")))])
                .await
        })
    };
    let second = {
        let ctx = app.root.clone();
        tokio::spawn(async move {
            ctx.bail("t2", vec![Dyn::new(ExternValue(json!("second")))])
                .await
        })
    };

    until("两条并发派发都进入挂起", || {
        T2_PEND.lock().unwrap().len() == 2 && pending_len(handle) == 2
    })
    .await;

    app.root.emit("t2/settle", vec![]).await;

    let a = tokio::time::timeout(Duration::from_secs(10), first)
        .await
        .expect("first 派发必须在时限内结算")
        .expect("first 派发任务不得 panic");
    let b = tokio::time::timeout(Duration::from_secs(10), second)
        .await
        .expect("second 派发必须在时限内结算")
        .expect("second 派发任务不得 panic");
    let (a, b) = (json_of(a), json_of(b));
    let recorded = T2_PEND.lock().unwrap().clone();
    let first_first = recorded
        .iter()
        .position(|(_, arg)| arg == "first")
        .expect("first 必须被记录")
        == 0;
    let (first_value, second_value) = if first_first {
        (Some(json!("A")), Some(json!("B")))
    } else {
        (Some(json!("B")), Some(json!("A")))
    };
    assert_eq!(
        a, first_value,
        "first 派发必须拿到它自己的结算值（记录序：{recorded:?}）"
    );
    assert_eq!(
        b, second_value,
        "second 派发必须拿到它自己的结算值（记录序：{recorded:?}）"
    );
    assert_eq!(pending_len(handle), 0, "全部结算后挂起表必须清空");
}

// ------------------------------------------------------------------ 测试 3

static T3: Mutex<Vec<String>> = Mutex::new(Vec::new());
static T3_IDS: Mutex<Vec<u64>> = Mutex::new(Vec::new());
static T3A_HANDLE: Mutex<u64> = Mutex::new(0);

unsafe extern "C" fn t3a_call(handle: u64, op: u32, a: Blob, _: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => {
            *T3A_HANDLE.lock().unwrap() = handle;
            let status = on_event(handle, "t3/a", 1); // 注意：与 B 库用**同一个**注册 id 值 1
            T3.lock().unwrap().push(format!("a-on:{status}"));
            (*out).write(b"")
        }
        OP_CALLBACK => {
            let id = dispatch_id_of(a.as_bytes());
            T3_IDS.lock().unwrap().push(id);
            let (status, text) = fulfill(handle, id, json!("A"));
            T3.lock().unwrap().push(format!("a:{status}:{text}"));
            STATUS_PENDING
        }
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

unsafe extern "C" fn t3b_call(handle: u64, op: u32, a: Blob, _: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => {
            let status = on_event(handle, "t3/b", 1); // 同值的注册 id：旧实现会在这里串线
            T3.lock().unwrap().push(format!("b-on:{status}"));
            (*out).write(b"")
        }
        OP_CALLBACK => {
            let id = dispatch_id_of(a.as_bytes());
            T3_IDS.lock().unwrap().push(id);
            let (status, text) = fulfill(handle, id, json!("B"));
            T3.lock().unwrap().push(format!("b:{status}:{text}"));
            STATUS_PENDING
        }
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

/// M2 红测试 3：两个插件库的 `dispatch_id` **不碰撞**（全局计数器铸造），
/// 且挂起表按实例隔离——用 A 的句柄结算 B 的 id 必须被拒绝。
#[tokio::test]
async fn ids_do_not_collide_across_two_plugin_libraries() {
    T3.lock().unwrap().clear();
    T3_IDS.lock().unwrap().clear();

    let app = App::new();
    let a_fiber = app
        .root
        .plugin(abi_plugin("m2-t3a", t3a_call), Config::Null);
    let b_fiber = app
        .root
        .plugin(abi_plugin("m2-t3b", t3b_call), Config::Null);
    a_fiber.settle().await;
    b_fiber.settle().await;
    assert_eq!(a_fiber.state(), FiberState::Active);
    assert_eq!(b_fiber.state(), FiberState::Active);
    let a_handle = *T3A_HANDLE.lock().unwrap();

    let ra = tokio::time::timeout(Duration::from_secs(10), app.root.bail("t3/a", vec![]))
        .await
        .expect("t3/a 必须在时限内结算");
    let rb = tokio::time::timeout(Duration::from_secs(10), app.root.bail("t3/b", vec![]))
        .await
        .expect("t3/b 必须在时限内结算");
    assert_eq!(json_of(ra), Some(json!("A")));
    assert_eq!(json_of(rb), Some(json!("B")));

    let ids = T3_IDS.lock().unwrap().clone();
    assert_eq!(ids.len(), 2, "两个库各挂起一次：{ids:?}");
    assert_ne!(ids[0], ids[1], "跨库的 dispatch_id 必须不碰撞：{ids:?}");

    // 交叉结算：A 的句柄 + B 的 id ⇒ 「不存在」（表按实例隔离）。
    let (status, text) = unsafe { fulfill(a_handle, ids[1], json!("x")) };
    assert_eq!(status, STATUS_ERR, "跨实例结算必须被拒绝：{text}");
    assert!(text.contains("不存在或已结算"), "{text}");
}

// ------------------------------------------------------------------ 测试 4

static T4: Mutex<Vec<String>> = Mutex::new(Vec::new());
static T4_HANDLE: Mutex<u64> = Mutex::new(0);

unsafe extern "C" fn t4_call(handle: u64, op: u32, _: Blob, _: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => {
            *T4_HANDLE.lock().unwrap() = handle;
            let status = on_event(handle, "t4", 7);
            T4.lock().unwrap().push(format!("on:{status}"));
            (*out).write(b"")
        }
        // 永不结算：等实例注销把它排水。
        OP_CALLBACK => STATUS_PENDING,
        OP_DISPOSE => {
            T4.lock().unwrap().push("dispose".into());
            (*out).write(b"")
        }
        _ => STATUS_UNSUPPORTED,
    }
}

/// M2 红测试 4：注销实例**排水**它的挂起续延——派发以「插件已卸载」结算
/// （而不是永远挂住），且 `pending_len` 归零（A10 的观测口径）。
#[tokio::test]
async fn unregistering_an_instance_fails_its_pending_continuations() {
    T4.lock().unwrap().clear();

    let app = App::new();
    let logs: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let logs = logs.clone();
        app.store
            .set_log_sink(Arc::new(move |level, name, message| {
                logs.lock()
                    .unwrap()
                    .push(format!("{level:?}:{name}:{message}"));
            }));
    }

    let fiber = app.root.plugin(abi_plugin("m2-t4", t4_call), Config::Null);
    fiber.settle().await;
    assert_eq!(fiber.state(), FiberState::Active);
    let handle = *T4_HANDLE.lock().unwrap();

    let emit = {
        let ctx = app.root.clone();
        tokio::spawn(async move { ctx.bail("t4", vec![]).await })
    };
    until("派发进入挂起", || pending_len(handle) == 1).await;

    fiber.dispose().await;

    let ret = tokio::time::timeout(Duration::from_secs(10), emit)
        .await
        .expect("卸载后派发必须在时限内结算（排水）")
        .expect("派发任务不得 panic");
    assert_eq!(json_of(ret), None, "被结算成失败的监听器等于没有意见");

    assert_eq!(pending_len(handle), 0, "卸载后挂起表必须归零");
    let text = logs.lock().unwrap().join("\n");
    assert!(text.contains("插件已卸载"), "失败必须带可读原因：{text}");
    assert!(
        T4.lock().unwrap().iter().any(|entry| entry == "dispose"),
        "插件应收到 OP_DISPOSE"
    );
}

// ------------------------------------------------------------------ 测试 5 / 6

static T5_HANDLE: Mutex<u64> = Mutex::new(0);
static T6_HANDLES: Mutex<Vec<u64>> = Mutex::new(Vec::new());

unsafe extern "C" fn t5_call(handle: u64, op: u32, _: Blob, _: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => {
            *T5_HANDLE.lock().unwrap() = handle;
            (*out).write(b"")
        }
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

unsafe extern "C" fn t6_call(handle: u64, op: u32, _: Blob, _: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => {
            T6_HANDLES.lock().unwrap().push(handle);
            (*out).write(b"")
        }
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

/// M2 红测试 5：陈旧句柄上的 `ctx_fulfill` 必须被拒绝（代际校验）。
#[tokio::test]
async fn fulfill_on_a_stale_handle_is_rejected() {
    *T5_HANDLE.lock().unwrap() = 0;

    let app = App::new();
    let fiber = app.root.plugin(abi_plugin("m2-t5", t5_call), Config::Null);
    fiber.settle().await;
    assert_eq!(fiber.state(), FiberState::Active);
    let handle = *T5_HANDLE.lock().unwrap();

    fiber.dispose().await;

    let (status, text) = unsafe { fulfill(handle, 1, json!("x")) };
    assert_eq!(status, STATUS_ERR, "陈旧句柄必须被拒绝：{text}");
    assert!(text.contains("已失效"), "拒绝必须是一条可读错误：{text}");
}

/// M2 守卫：`fulfill` 走 `with_instance`——**代际校验**对它是生效的
/// （旧句柄拒绝"已失效"，新句柄进表后报"不存在"）。
#[tokio::test]
async fn fulfill_goes_through_with_instance_and_respects_generation() {
    T6_HANDLES.lock().unwrap().clear();

    let app = App::new();
    let plugin = abi_plugin("m2-t6", t6_call);
    let first = app.root.plugin(plugin.clone(), Config::Null);
    first.settle().await;
    assert_eq!(first.state(), FiberState::Active);
    let old_handle = T6_HANDLES.lock().unwrap()[0];
    first.dispose().await;

    let second = app.root.plugin(plugin, Config::Null);
    second.settle().await;
    assert_eq!(second.state(), FiberState::Active);
    let handles = T6_HANDLES.lock().unwrap().clone();
    assert_eq!(handles.len(), 2);
    let new_handle = handles[1];
    assert_ne!(
        old_handle, new_handle,
        "同一槽位复用必须换代际（句柄值不同）"
    );

    let (status, text) = unsafe { fulfill(old_handle, 0, json!("x")) };
    assert_eq!(status, STATUS_ERR, "旧代际必须被拒绝：{text}");
    assert!(text.contains("已失效"), "{text}");

    // 新句柄**真的进了挂起表那条路**：不存在的 id 报"不存在/已结算"，
    // 而不是被代际检查挡在门外。
    let (status, text) = unsafe { fulfill(new_handle, 0, json!("x")) };
    assert_eq!(status, STATUS_ERR);
    assert!(text.contains("不存在或已结算"), "{text}");
}

// ------------------------------------------------------------------ 测试 7

static T7: Mutex<Vec<String>> = Mutex::new(Vec::new());
static T7_HANDLE: Mutex<u64> = Mutex::new(0);

unsafe extern "C" fn t7_call(handle: u64, op: u32, a: Blob, _: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => {
            *T7_HANDLE.lock().unwrap() = handle;
            let one = on_event(handle, "t7/inline", 7);
            let two = on_event(handle, "t7/async", 8);
            T7.lock().unwrap().push(format!("on:{one}:{two}"));
            (*out).write(b"")
        }
        OP_CALLBACK => {
            let payload: Value = serde_json::from_slice(a.as_bytes()).unwrap_or(Value::Null);
            let id = payload.get("id").and_then(Value::as_u64).unwrap_or(0);
            if id == 7 {
                // 同步返回一个"长得像挂起标记"的 JSON 值——它只是普通返回值。
                (*out).write(br#"{"ret":{"pending":true}}"#)
            } else {
                let did = dispatch_id_of(a.as_bytes());
                let (status, text) = fulfill(handle, did, json!({ "pending": true }));
                T7.lock().unwrap().push(format!("f:{status}:{text}"));
                STATUS_PENDING
            }
        }
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

/// M2 红测试 7：挂起走**状态码**，与载荷内容无关——
/// 合法返回值恰好长得像 `{"pending":true}` 不会被误读（两个方向都测）。
#[tokio::test]
async fn status_pending_is_not_confused_with_a_json_return_value() {
    T7.lock().unwrap().clear();
    *T7_HANDLE.lock().unwrap() = 0;

    let app = App::new();
    let fiber = app.root.plugin(abi_plugin("m2-t7", t7_call), Config::Null);
    fiber.settle().await;
    assert_eq!(fiber.state(), FiberState::Active);
    let handle = *T7_HANDLE.lock().unwrap();

    let inline = tokio::time::timeout(Duration::from_secs(10), app.root.bail("t7/inline", vec![]))
        .await
        .expect("t7/inline 必须在时限内结算");
    assert_eq!(
        json_of(inline),
        Some(json!({ "pending": true })),
        "长得像标记的同步返回值必须原样带回"
    );
    assert_eq!(pending_len(handle), 0, "同步返回不得留下挂起条目");

    let settled = tokio::time::timeout(Duration::from_secs(10), app.root.bail("t7/async", vec![]))
        .await
        .expect("t7/async 必须在时限内结算");
    assert_eq!(
        json_of(settled),
        Some(json!({ "pending": true })),
        "挂起后结算的同一个值也只是普通返回值"
    );
    assert_eq!(pending_len(handle), 0);
    let log = T7.lock().unwrap().clone();
    assert!(
        log.iter().any(|entry| entry.starts_with("f:0:")),
        "fulfill 必须成功：{log:?}"
    );
}

// ------------------------------------------------------------------ 测试 8

static T8: Mutex<Vec<String>> = Mutex::new(Vec::new());

unsafe extern "C" fn t8_call(handle: u64, op: u32, a: Blob, _: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => {
            let status = on_event(handle, "t8", 7);
            T8.lock().unwrap().push(format!("on:{status}"));
            (*out).write(b"")
        }
        OP_CALLBACK => {
            let id = dispatch_id_of(a.as_bytes());
            // 在回调**内部**观察挂起表：自己的条目必须已经登记（先登记后调用）。
            let seen = pending_len(handle);
            let (status, _) = fulfill(handle, id, json!("early"));
            T8.lock().unwrap().push(format!("seen:{seen}:f:{status}"));
            STATUS_PENDING
        }
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

/// M2 红测试 8：**确定性**地钉死"先登记、后调用"——
/// 插件在自己的回调内部就看到挂起条目，且能当场结算（不比墙钟）。
#[tokio::test]
async fn pending_is_observed_before_the_dispatch_returns() {
    T8.lock().unwrap().clear();

    let app = App::new();
    let fiber = app.root.plugin(abi_plugin("m2-t8", t8_call), Config::Null);
    fiber.settle().await;
    assert_eq!(fiber.state(), FiberState::Active);

    let ret = tokio::time::timeout(Duration::from_secs(10), app.root.bail("t8", vec![]))
        .await
        .expect("t8 必须在时限内结算");
    assert_eq!(
        json_of(ret),
        Some(json!("early")),
        "回调内部的结算必须立即生效"
    );

    let log = T8.lock().unwrap().clone();
    assert_eq!(log.len(), 2, "{log:?}");
    assert_eq!(log[0], "on:0");
    assert_eq!(
        log[1], "seen:1:f:0",
        "回调内部必须看到恰好 1 条自己的挂起条目"
    );
}

// ------------------------------------------------------------------ 测试 9（守卫）

static T9: Mutex<Vec<String>> = Mutex::new(Vec::new());
static T9_HANDLE: Mutex<u64> = Mutex::new(0);

unsafe extern "C" fn t9_call(handle: u64, op: u32, _: Blob, _: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => {
            *T9_HANDLE.lock().unwrap() = handle;
            let status = on_event(handle, "t9", 7);
            T9.lock().unwrap().push(format!("on:{status}"));
            (*out).write(b"")
        }
        OP_CALLBACK => {
            T9.lock().unwrap().push("callback".into());
            (*out).write(br#"{"ret":"plain"}"#)
        }
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

/// M2 守卫：**同步路径逐字不变**——回调恰好调一次、立即带回、无挂起残留。
#[tokio::test]
async fn sync_handler_path_is_unchanged() {
    T9.lock().unwrap().clear();

    let app = App::new();
    let fiber = app.root.plugin(abi_plugin("m2-t9", t9_call), Config::Null);
    fiber.settle().await;
    assert_eq!(fiber.state(), FiberState::Active);
    let handle = *T9_HANDLE.lock().unwrap();

    let ret = tokio::time::timeout(Duration::from_secs(10), app.root.bail("t9", vec![]))
        .await
        .expect("t9 必须在时限内结算");
    assert_eq!(json_of(ret), Some(json!("plain")), "同步返回值必须原样带回");

    let log = T9.lock().unwrap().clone();
    assert_eq!(
        log,
        vec!["on:0".to_string(), "callback".to_string()],
        "回调必须恰好一次：{log:?}"
    );
    assert_eq!(pending_len(handle), 0, "同步路径不得留下挂起条目");
}
