//! 流（ADR 0001 §5 的验收口径）：T2 插件向宿主推块，窗口 = 4。
//!
//! 冻结的契约：
//! - 调用方用 `Ctx::call_iface_stream` 开流，拿到 `AsyncStream`；
//! - 插件用 `ctx_stream_push` 推块：窗口内 `STATUS_OK`，满窗口 `STATUS_PENDING`
//!   （**块没有被接受**），并登记一个 credit 续延 id；
//! - 消费者每取走一块 ⇒ 宿主回调插件（`kind:"stream-credit"`，载荷带可用窗口）；
//! - `ctx_stream_close` 正常收尾、`ctx_stream_fail` 失败收尾；
//! - 插件的返回值 = 流的最后一件（不推块就是长度 1 的流）；
//! - 卸载 / 取消 / 消费者离场 ⇒ **先排水**：等待中的 credit 被驳回，推送拿到
//!   `STATUS_STREAM_GONE`，派发记录（含帧）归零。
//!
//! 无 DLL（D9）：ABI 插件经 [`apply_via`] + 手写 `CallFn` 构造，宿主函数经
//! [`host_api`] 的**真表**调用。夹具铁律（D10）：fake `CallFn` 内**禁止 assert**，
//! 结局只记进 static `Mutex`；每个测试用自己的夹具静态（同一二进制内并行跑）。

use cordis_abi::{
    Blob, Out, KIND_STREAM_CREDIT, OP_APPLY, OP_CALLBACK, OP_DISPOSE, STATUS_OK, STATUS_PENDING,
    STATUS_STREAM_GONE, STATUS_UNSUPPORTED,
};
use cordis_core::{App, Config, Ctx, FiberState, Inject, Plugin, STREAM_WINDOW};
use cordis_host::{apply_via, dispatches_len, host_api, pending_len, streams_len, waterfall_len};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 提供者侧 iface 分发器的回调 id（本测试自选；SDK 场景由注册表铸造）。
const IFACE: u64 = 31;
/// 推块时登记的 credit 续延 id（本测试自选；一次只可能有一个在等，所以是常量）。
const CREDIT: u64 = 32;
/// 一百块：窗口 4 ⇒ 必然多次撞满窗口（ADR 0001 §5 的验收数字）。
const CHUNK: usize = 100;

/// 一个测试一套夹具（同一二进制内并行跑，不能共用静态）。
struct Fixture {
    log: Vec<String>,
    handle: u64,
    dispatch: u64,
    mode: String,
    total: usize,
    sent: usize,
    /// 已推、还没被消费者取走的块数（提供者视角的"未归还"）。
    outstanding: usize,
    max_outstanding: usize,
    /// 被窗口挡住的次数。
    blocked: usize,
    /// 推送拿到 GONE 的次数。
    gone: usize,
}

impl Fixture {
    const fn new() -> Self {
        Self {
            log: Vec::new(),
            handle: 0,
            dispatch: 0,
            mode: String::new(),
            total: 0,
            sent: 0,
            outstanding: 0,
            max_outstanding: 0,
            blocked: 0,
            gone: 0,
        }
    }
}

macro_rules! fixture {
    ($name:ident) => {
        static $name: Mutex<Fixture> = Mutex::new(Fixture::new());
    };
}

fixture!(A);
fixture!(B);
fixture!(C);
fixture!(D);
fixture!(U);

fn abi_plugin(name: &'static str, call: cordis_host::CallFn) -> Arc<dyn Plugin> {
    Arc::new(cordis_core::FnPlugin {
        name,
        key: cordis_core::unique_key(name),
        inject: Inject::default(),
        validator: None,
        f: Arc::new(move |ctx: Ctx, config: Config| apply_via(call, ctx, config, name)),
    })
}

async fn until(what: &str, mut ready: impl FnMut() -> bool) {
    for _ in 0..300 {
        if ready() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("等待超时：{what}");
}

// ---------------------------------------------------------------- 假提供者

/// 提供者共享实现。方法（由调用方点名）：
/// - `stream`：推 [`CHUNK`] 块（跨窗口，靠 credit 继续）；
/// - `close`：推 3 块后正常收尾；`fail`：推 2 块后失败收尾；
/// - `value`：不推，直接返回值（⇒ 长度 1 的流）；
/// - `probe`：在**非流**调用上试推（可读错误 + 不存在的派发 ⇒ GONE）。
unsafe fn plugin_body(
    fx: &'static Mutex<Fixture>,
    handle: u64,
    op: u32,
    a: Blob,
    out: *mut Out,
) -> i32 {
    match op {
        OP_APPLY => {
            fx.lock().unwrap().handle = handle;
            let mut raw = [0u8; 256];
            let mut probe = Out {
                ptr: raw.as_mut_ptr(),
                cap: raw.len(),
                len: 0,
            };
            match host_api().ctx_provide_iface {
                Some(provide) => provide(
                    handle,
                    Blob::new(b"s@1"),
                    1,
                    Blob::new(br#"["stream","close","fail","value","probe"]"#),
                    IFACE,
                    &mut probe,
                ),
                None => STATUS_UNSUPPORTED,
            }
        }
        OP_CALLBACK => {
            let payload: Value = serde_json::from_slice(a.as_bytes()).unwrap_or(Value::Null);
            let kind = payload
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if kind == KIND_STREAM_CREDIT {
                return on_credit(fx, &payload, out);
            }
            let method = payload
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let dispatch = payload
                .get("dispatch_id")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            {
                let mut state = fx.lock().unwrap();
                state.dispatch = dispatch;
                state.mode = method.clone();
                state.total = match method.as_str() {
                    "stream" => CHUNK,
                    "close" => 3,
                    "fail" => 2,
                    _ => 0,
                };
                state.sent = 0;
                state.outstanding = 0;
                state.max_outstanding = 0;
                state.log.push(format!("call:{method}"));
            }
            match method.as_str() {
                "value" => (*out).write(br#"{"ret":{"v":1}}"#),
                "probe" => {
                    probe_pushes(fx, handle, dispatch);
                    (*out).write(br#"{"ret":"probe"}"#)
                }
                // 真流：能推多少推多少；推不完就挂起（等 credit）。
                _ => {
                    if pump(fx, handle, dispatch) {
                        (*out).write(br#"{"ret":null}"#)
                    } else {
                        STATUS_PENDING
                    }
                }
            }
        }
        OP_DISPOSE => {
            fx.lock().unwrap().log.push("dispose".into());
            (*out).write(b"")
        }
        _ => STATUS_UNSUPPORTED,
    }
}

/// 尽量把窗口灌满。返回 `true` = 这次流已经推完（收尾已发出）。
unsafe fn pump(fx: &'static Mutex<Fixture>, handle: u64, dispatch: u64) -> bool {
    loop {
        let (mode, next, total) = {
            let state = fx.lock().unwrap();
            (state.mode.clone(), state.sent + 1, state.total)
        };
        if next > total {
            finish(fx, handle, dispatch, &mode);
            return true;
        }
        let Some(push) = host_api().ctx_stream_push else {
            return true;
        };
        let payload = json!(next).to_string();
        let mut raw = [0u8; 512];
        let mut probe = Out {
            ptr: raw.as_mut_ptr(),
            cap: raw.len(),
            len: 0,
        };
        let status = push(
            handle,
            dispatch,
            Blob::new(payload.as_bytes()),
            CREDIT,
            &mut probe,
        );
        if status == STATUS_OK {
            let mut state = fx.lock().unwrap();
            state.sent += 1;
            state.outstanding += 1;
            state.max_outstanding = state.max_outstanding.max(state.outstanding);
        } else if status == STATUS_PENDING {
            fx.lock().unwrap().blocked += 1;
            return false; // 等 credit，别忙等
        } else {
            let mut state = fx.lock().unwrap();
            state.gone += 1;
            let text = String::from_utf8_lossy(&raw[..probe.len]).into_owned();
            state.log.push(format!("push:{status}:{text}"));
            return true;
        }
    }
}

/// 收尾：`fail` 走失败收尾，其余正常收尾。
unsafe fn finish(fx: &'static Mutex<Fixture>, handle: u64, dispatch: u64, mode: &str) {
    let mut raw = [0u8; 256];
    let mut probe = Out {
        ptr: raw.as_mut_ptr(),
        cap: raw.len(),
        len: 0,
    };
    if mode == "fail" {
        if let Some(fail) = host_api().ctx_stream_fail {
            let _ = fail(handle, dispatch, Blob::new(b"boom"), &mut probe);
        }
    } else if let Some(close) = host_api().ctx_stream_close {
        let _ = close(handle, dispatch, &mut probe);
    }
    fx.lock().unwrap().log.push(format!("finish:{mode}"));
}

/// 宿主归还额度（或驳回）时的回调：`args[0]` 是 `{"result":{"window":n,"dispatch_id":id}}`。
unsafe fn on_credit(fx: &'static Mutex<Fixture>, payload: &Value, out: *mut Out) -> i32 {
    let args = payload
        .get("args")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let inner = args.first().cloned().unwrap_or(Value::Null);

    if let Some(error) = inner
        .get("error")
        .and_then(Value::as_str)
        .map(str::to_string)
    {
        let (handle, dispatch) = {
            let mut state = fx.lock().unwrap();
            state.log.push(format!("denied:{error}"));
            (state.handle, state.dispatch)
        };
        // 流已经不在了：再推一次必须拿到 GONE。
        if let Some(push) = host_api().ctx_stream_push {
            let mut raw = [0u8; 512];
            let mut probe = Out {
                ptr: raw.as_mut_ptr(),
                cap: raw.len(),
                len: 0,
            };
            let status = push(handle, dispatch, Blob::new(b"1"), CREDIT, &mut probe);
            if status != STATUS_OK {
                let mut state = fx.lock().unwrap();
                if status == STATUS_STREAM_GONE {
                    state.gone += 1;
                }
                let text = String::from_utf8_lossy(&raw[..probe.len]).into_owned();
                state.log.push(format!("push:{status}:{text}"));
            }
        }
        return (*out).write(br#"{"ret":null}"#);
    }

    let result = inner.get("result").cloned().unwrap_or(Value::Null);
    let window = result.get("window").and_then(Value::as_u64).unwrap_or(0) as usize;
    let dispatch = result
        .get("dispatch_id")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let handle = {
        let mut state = fx.lock().unwrap();
        // 归还的额度先从"未归还"里扣掉——这样 max_outstanding 就是
        // "同一时刻最多有多少块没被消费者取走"（ADR §5 的观测口径）。
        state.outstanding = state.outstanding.saturating_sub(window);
        state.log.push(format!("credit:{window}"));
        state.handle
    };
    let _ = pump(fx, handle, dispatch);
    (*out).write(br#"{"ret":null}"#)
}

/// 在**非流**调用上试推：本次派发（存在但不是流）⇒ 可读错误；不存在的派发 ⇒ GONE。
unsafe fn probe_pushes(fx: &'static Mutex<Fixture>, handle: u64, dispatch: u64) {
    let Some(push) = host_api().ctx_stream_push else {
        return;
    };
    for target in [dispatch, 999_999] {
        let mut raw = [0u8; 512];
        let mut probe = Out {
            ptr: raw.as_mut_ptr(),
            cap: raw.len(),
            len: 0,
        };
        let status = push(handle, target, Blob::new(b"1"), 0, &mut probe);
        let text = String::from_utf8_lossy(&raw[..probe.len]).into_owned();
        fx.lock()
            .unwrap()
            .log
            .push(format!("probe:{target}:{status}:{text}"));
    }
}

macro_rules! plugin {
    ($wrapper:ident, $fixture:ident) => {
        unsafe extern "C" fn $wrapper(
            handle: u64,
            op: u32,
            a: Blob,
            _: Blob,
            out: *mut Out,
        ) -> i32 {
            plugin_body(&$fixture, handle, op, a, out)
        }
    };
}

plugin!(call_a, A);
plugin!(call_b, B);
plugin!(call_c, C);
plugin!(call_d, D);
plugin!(call_u, U);

// ---------------------------------------------------------------- 验收：100 块 / 窗口 4

#[tokio::test]
async fn a_stream_flows_through_a_window_of_four_with_credit_callbacks() {
    let app = App::new();
    let fiber = app
        .root
        .plugin(abi_plugin("stream-provider", call_a), Config::Null);
    fiber.settle().await;
    assert_eq!(fiber.state(), FiberState::Active);
    let handle = A.lock().unwrap().handle;

    let mut stream = app
        .root
        .call_iface_stream("s@1", 1, "stream", json!([CHUNK]))
        .expect("开流必须成功");
    assert_eq!(streams_len(handle), 1, "开流之后宿主要记着这条流");
    assert_eq!(pending_len(handle), 1, "推不完的流在宿主侧就是在飞派发");

    let mut seen = Vec::new();
    let drain = async {
        loop {
            match stream.next().await {
                Ok(Some(chunk)) => seen.push(chunk),
                Ok(None) => break,
                Err(err) => panic!("这条流不该失败：{err}"),
            }
        }
    };
    // 超时护栏（D10 的同一口径）：背压写错时"永远收不完"是失败模式之一，
    // 必须表现为一次**可读的失败**，而不是把测试挂死。
    tokio::time::timeout(Duration::from_secs(10), drain)
        .await
        .expect("100 块必须在 10s 内流完（背压不得把提供者饿死）");

    assert_eq!(
        seen,
        (1..=CHUNK)
            .map(|index| json!(index))
            .collect::<Vec<Value>>(),
        "块必须按序、不丢不重（逐块比对，不只是长度）"
    );
    let (blocked, max_outstanding, sent, gone) = {
        let state = A.lock().unwrap();
        (state.blocked, state.max_outstanding, state.sent, state.gone)
    };
    assert!(blocked >= 1, "窗口必须真的挡住过（否则背压是空的）");
    assert_eq!(
        max_outstanding, STREAM_WINDOW,
        "插件观测到的未归还块数峰值必须等于窗口"
    );
    assert_eq!(sent, CHUNK, "每一块都推成功");
    assert_eq!(gone, 0, "这条流没有中途消失");
    assert_eq!(
        pending_len(handle),
        0,
        "流结束后不留挂起条目（A10 的同一口径）"
    );
    assert_eq!(waterfall_len(handle), 0);
    assert_eq!(
        dispatches_len(handle),
        0,
        "流结束后派发记录必须归零（含帧与出站节点）"
    );
    assert_eq!(streams_len(handle), 0, "流记录必须被摘");
}

// ---------------------------------------------------------------- 收尾的两种形态 + 单件流

#[tokio::test]
async fn close_and_value_both_end_a_stream_cleanly() {
    let app = App::new();
    let fiber = app
        .root
        .plugin(abi_plugin("close-provider", call_b), Config::Null);
    fiber.settle().await;
    let handle = B.lock().unwrap().handle;

    // 阶段 1：推 3 块 + close（都没满窗口：整个过程是同步发生的）
    let mut stream = app
        .root
        .call_iface_stream("s@1", 1, "close", json!([]))
        .expect("开流");
    assert_eq!(stream.next().await.unwrap(), Some(json!(1)));
    assert_eq!(stream.next().await.unwrap(), Some(json!(2)));
    assert_eq!(stream.next().await.unwrap(), Some(json!(3)));
    assert_eq!(stream.next().await.unwrap(), None, "close 之后是 Ok(None)");
    assert_eq!(pending_len(handle), 0, "收尾 ⇒ 派发记录立刻归零");
    assert_eq!(streams_len(handle), 0);

    // 阶段 2：不推块、只返回值 ⇒ 长度 1 的流
    let mut stream = app
        .root
        .call_iface_stream("s@1", 1, "value", json!([]))
        .expect("开流");
    assert_eq!(
        stream.next().await.unwrap(),
        Some(json!({ "v": 1 })),
        "返回值就是长度 1 的流"
    );
    assert_eq!(stream.next().await.unwrap(), None);
    assert_eq!(pending_len(handle), 0);
    assert_eq!(streams_len(handle), 0);
}

#[tokio::test]
async fn fail_ends_the_stream_with_the_error_and_pushing_where_there_is_no_stream_is_readable() {
    let app = App::new();
    let fiber = app
        .root
        .plugin(abi_plugin("fail-provider", call_c), Config::Null);
    fiber.settle().await;
    let handle = C.lock().unwrap().handle;

    // 失败收尾：缓冲被丢，消费者立刻拿到错误。
    let mut stream = app
        .root
        .call_iface_stream("s@1", 1, "fail", json!([]))
        .expect("开流");
    let err = match stream.next().await {
        Err(err) => err,
        Ok(_) => panic!("失败收尾必须给错误，而不是值"),
    };
    assert!(err.to_string().contains("boom"), "{err}");
    assert_eq!(pending_len(handle), 0, "失败收尾也要把记录摘干净");
    assert_eq!(streams_len(handle), 0);

    // 非流调用上推送：可读错误（不是静默丢掉）；不存在的派发：GONE。
    let value = app
        .root
        .call_iface("s@1", 1, "probe", json!([]))
        .await
        .expect("非流调用照常返回值");
    assert_eq!(value, json!("probe"));
    let dispatch = C.lock().unwrap().dispatch;
    let log = C.lock().unwrap().log.clone();
    let non_stream = log
        .iter()
        .find(|line| line.starts_with(&format!("probe:{dispatch}:")))
        .cloned()
        .unwrap_or_default();
    assert!(
        non_stream.contains("不是流"),
        "非流调用上的推送必须给出可读原因：{non_stream}"
    );
    let absent = log
        .iter()
        .find(|line| line.starts_with("probe:999999:"))
        .cloned()
        .unwrap_or_default();
    assert!(
        absent.starts_with(&format!("probe:999999:{STATUS_STREAM_GONE}:")),
        "不存在的派发必须报「流已经不在了」：{absent}"
    );
}

// ---------------------------------------------------------------- 两条排水路径

#[tokio::test]
async fn unloading_the_provider_drains_the_stream_before_dispose() {
    let app = App::new();
    let fiber = app
        .root
        .plugin(abi_plugin("unload-provider", call_u), Config::Null);
    fiber.settle().await;
    let handle = U.lock().unwrap().handle;

    let mut stream = app
        .root
        .call_iface_stream("s@1", 1, "stream", json!([CHUNK]))
        .expect("开流");
    assert_eq!(stream.next().await.unwrap(), Some(json!(1)));
    assert_eq!(stream.next().await.unwrap(), Some(json!(2)));
    assert_eq!(streams_len(handle), 1, "卸载之前这条流还在");

    fiber.dispose().await;

    let err = match stream.next().await {
        Err(err) => err,
        Ok(_) => panic!("提供者卸载 ⇒ 没读完的流必须以错误结束"),
    };
    assert!(err.to_string().contains("卸载"), "{err}");
    // 实例连同它的记录一起被排水（句柄已失效 ⇒ 计数为 0）。
    assert_eq!(streams_len(handle), 0, "卸载之后流记录必须随实例一起消失");
    assert_eq!(pending_len(handle), 0);

    let log = U.lock().unwrap().log.clone();
    let denied = log
        .iter()
        .position(|line| line.starts_with("denied:"))
        .unwrap_or_else(|| panic!("等待中的 credit 必须先被驳回：{log:?}"));
    let disposed = log
        .iter()
        .position(|line| line == "dispose")
        .unwrap_or_else(|| panic!("插件必须收到 OP_DISPOSE：{log:?}"));
    assert!(denied < disposed, "先排水再 dispose：{log:?}");
}

#[tokio::test]
async fn dropping_the_stream_notifies_the_provider_and_settles_the_dispatch() {
    let app = App::new();
    let fiber = app
        .root
        .plugin(abi_plugin("drop-provider", call_d), Config::Null);
    fiber.settle().await;
    let handle = D.lock().unwrap().handle;

    let mut stream = app
        .root
        .call_iface_stream("s@1", 1, "stream", json!([CHUNK]))
        .expect("开流");
    assert_eq!(stream.next().await.unwrap(), Some(json!(1)));
    drop(stream);

    until("插件观察到「流已经不在了」", || {
        let log = D.lock().unwrap().log.clone();
        log.iter().any(|line| line.starts_with("denied:"))
    })
    .await;

    assert_eq!(
        dispatches_len(handle),
        0,
        "消费者离场 ⇒ 派发记录立刻归零（条目即生命周期）"
    );
    assert_eq!(pending_len(handle), 0);
    assert_eq!(streams_len(handle), 0);
    let (log, gone) = {
        let state = D.lock().unwrap();
        (state.log.clone(), state.gone)
    };
    assert!(gone >= 1, "流没了之后的推送必须报 GONE：{log:?}");
    assert!(
        log.iter()
            .any(|line| line.contains(&format!(":{STATUS_STREAM_GONE}:"))),
        "{log:?}"
    );
}
