//! 多会话并发归因（ADR 0011）：
//! - 16 个并发会话在多线程 Tokio 运行时上同时调用共享 T2 链
//!   `m@1 →(PENDING) p@1 →(续延恢复后) c@1`；
//! - 同时验证 `Ctx::with_correlation` 与 `scope_correlation` 两条入口；
//! - 断言每跳都携带发起会话的 `correlation_id`，零缺失、零跨会话串线，且父链完整；
//! - 断言仅出站超时（`call_id = 0`）同样保留会话的 `correlation_id`。

mod trace_support;

use cordis_abi::{
    Blob, Out, OP_APPLY, OP_CALLBACK, OP_DISPOSE, STATUS_OK, STATUS_PENDING, STATUS_UNSUPPORTED,
};
use cordis_core::{scope_correlation, App, Config, FiberState, Inject, Outcome};
use cordis_host::{host_api, set_dispatch_policy_for, DispatchPolicy};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use trace_support::{abi_plugin, collect, provide};

static CONT_SEQ: AtomicU64 = AtomicU64::new(100);

fn inflight() -> &'static Mutex<HashMap<u64, u64>> {
    static MAP: OnceLock<Mutex<HashMap<u64, u64>>> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

unsafe fn call_iface_args(
    handle: u64,
    service: &str,
    method: &str,
    args: &str,
    continuation: u64,
) -> (i32, String) {
    let Some(call) = host_api().ctx_call_iface else {
        return (STATUS_UNSUPPORTED, "宿主不支持 ctx_call_iface".into());
    };
    let mut buffer = vec![0u8; 4096];
    let mut out = Out {
        ptr: buffer.as_mut_ptr(),
        cap: buffer.len(),
        len: 0,
    };
    let status = call(
        handle,
        Blob::new(service.as_bytes()),
        1,
        Blob::new(method.as_bytes()),
        Blob::new(args.as_bytes()),
        continuation,
        &mut out,
    );
    (
        status,
        String::from_utf8_lossy(&buffer[..out.len.min(buffer.len())]).into_owned(),
    )
}

unsafe extern "C" fn leaf_call(handle: u64, op: u32, _a: Blob, _b: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => provide(handle, "c@1", r#"["run"]"#, 3, out),
        OP_CALLBACK => (*out).write(br#"{"ret":"leaf"}"#),
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

unsafe extern "C" fn pending_call(handle: u64, op: u32, a: Blob, _b: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => provide(handle, "p@1", r#"["run"]"#, 2, out),
        OP_CALLBACK => {
            let payload: Value = serde_json::from_slice(a.as_bytes()).unwrap_or(Value::Null);
            let Some(dispatch_id) = payload.get("dispatch_id").and_then(Value::as_u64) else {
                return STATUS_UNSUPPORTED;
            };
            let delay_ms = payload["args"]
                .get(0)
                .and_then(|v| v.get("delay_ms"))
                .and_then(Value::as_u64)
                .unwrap_or(10);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(delay_ms));
                let Some(fulfill) = host_api().ctx_fulfill else {
                    return;
                };
                let mut buffer = [0u8; 256];
                let mut slot = Out {
                    ptr: buffer.as_mut_ptr(),
                    cap: buffer.len(),
                    len: 0,
                };
                let _ = fulfill(
                    handle,
                    dispatch_id,
                    Blob::new(br#"{"done":true}"#),
                    &mut slot,
                );
            });
            (*out).write(b"");
            STATUS_PENDING
        }
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

unsafe extern "C" fn mid_call(handle: u64, op: u32, a: Blob, _b: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => provide(handle, "m@1", r#"["run","run_timeout"]"#, 1, out),
        OP_CALLBACK => {
            let payload: Value = serde_json::from_slice(a.as_bytes()).unwrap_or(Value::Null);
            let cb_id = payload.get("id").and_then(Value::as_u64).unwrap_or(0);
            if cb_id == 1 {
                let Some(dispatch_id) = payload.get("dispatch_id").and_then(Value::as_u64) else {
                    return STATUS_UNSUPPORTED;
                };
                let method = payload.get("method").and_then(Value::as_str).unwrap_or("");
                let target = if method == "run_timeout" {
                    "hang@1"
                } else {
                    "p@1"
                };
                let cont = CONT_SEQ.fetch_add(1, Ordering::Relaxed);
                inflight().lock().unwrap().insert(cont, dispatch_id);
                let arg_json = payload["args"].get(0).cloned().unwrap_or(Value::Null);
                let (status, text) =
                    call_iface_args(handle, target, "run", &arg_json.to_string(), cont);
                if status != STATUS_OK {
                    inflight().lock().unwrap().remove(&cont);
                    (*out).write(text.as_bytes());
                    return status;
                }
                let reply: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                if reply.get("pending").and_then(Value::as_bool) == Some(true) {
                    (*out).write(b"");
                    return STATUS_PENDING;
                }
                inflight().lock().unwrap().remove(&cont);
                (*out).write(br#"{"ret":"inline"}"#);
                return STATUS_OK;
            }
            if let Some(inbound_dispatch) = inflight().lock().unwrap().remove(&cb_id) {
                let first = payload["args"].get(0).cloned().unwrap_or(Value::Null);
                let value = if first.get("error").is_some() {
                    json!({ "error": first["error"] })
                } else {
                    let (status, text) = call_iface_args(handle, "c@1", "run", "[]", 0);
                    if status != STATUS_OK {
                        json!({ "error": text })
                    } else {
                        let reply: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                        reply.get("result").cloned().unwrap_or(Value::Null)
                    }
                };
                let Some(fulfill) = host_api().ctx_fulfill else {
                    return STATUS_UNSUPPORTED;
                };
                let mut buffer = [0u8; 256];
                let mut slot = Out {
                    ptr: buffer.as_mut_ptr(),
                    cap: buffer.len(),
                    len: 0,
                };
                let _ = fulfill(
                    handle,
                    inbound_dispatch,
                    Blob::new(value.to_string().as_bytes()),
                    &mut slot,
                );
                (*out).write(b"");
                return STATUS_OK;
            }
            STATUS_UNSUPPORTED
        }
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

unsafe extern "C" fn hang_call(handle: u64, op: u32, _a: Blob, _b: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => provide(handle, "hang@1", r#"["run"]"#, 4, out),
        OP_CALLBACK => {
            (*out).write(b"");
            STATUS_PENDING
        }
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_sessions_propagate_correlation_across_pending_chains() {
    inflight().lock().unwrap().clear();
    let app = App::new();
    let records = collect(&app);

    let leaf = {
        let f = app.root.plugin(
            abi_plugin("corr-c", Inject::default(), leaf_call),
            Config::Null,
        );
        f.settle().await;
        f
    };
    let pending = {
        let f = app.root.plugin(
            abi_plugin("corr-p", Inject::default(), pending_call),
            Config::Null,
        );
        f.settle().await;
        f
    };
    let mid = {
        let f = app.root.plugin(
            abi_plugin("corr-m", Inject::of(&["p@1", "c@1"]), mid_call),
            Config::Null,
        );
        f.settle().await;
        f
    };
    for fiber in [&leaf, &pending, &mid] {
        assert_eq!(fiber.state(), FiberState::Active);
    }

    let mut tasks = Vec::new();
    for i in 0..16usize {
        let session_id = format!("sess-{i:02}");
        let delay_ms = 5 + ((i * 7) % 20) as u64;
        if i % 2 == 0 {
            let ctx = app.root.with_correlation(session_id.clone());
            tasks.push(tokio::spawn(async move {
                let res = ctx
                    .call_iface("m@1", 1, "run", json!({ "delay_ms": delay_ms }))
                    .await
                    .expect("with_correlation 会话调用必须成功");
                assert_eq!(res, json!("leaf"));
                session_id
            }));
        } else {
            let root = app.root.clone();
            tasks.push(tokio::spawn(scope_correlation(
                session_id.clone(),
                async move {
                    tokio::task::yield_now().await;
                    let res = root
                        .call_iface("m@1", 1, "run", json!({ "delay_ms": delay_ms }))
                        .await
                        .expect("scope_correlation 会话调用必须成功");
                    assert_eq!(res, json!("leaf"));
                    session_id
                },
            )));
        }
    }

    for task in tasks {
        task.await.expect("并发任务不得 panic");
    }

    {
        let records = records.lock().unwrap();
        assert_eq!(
            records.len(),
            16 * 3,
            "16 会话 × 3 跳必须恰好 48 条记录：actual={}",
            records.len()
        );
        let mut by_session: HashMap<String, Vec<_>> = HashMap::new();
        for record in records.iter() {
            let cid = record
                .correlation_id
                .as_deref()
                .unwrap_or_else(|| panic!("每条记录都必须带 correlation_id：{record:?}"));
            by_session
                .entry(cid.to_string())
                .or_default()
                .push(record.clone());
        }
        assert_eq!(by_session.len(), 16, "必须覆盖全部 16 个会话");
        for i in 0..16usize {
            let session_id = format!("sess-{i:02}");
            let rows = by_session
                .get(&session_id)
                .unwrap_or_else(|| panic!("缺少会话 {session_id}"));
            assert_eq!(
                rows.len(),
                3,
                "会话 {session_id} 必须恰好 3 跳（m/p/c）：{rows:?}"
            );
            let m = rows
                .iter()
                .find(|r| &*r.name == "m@1.run")
                .expect("缺 m@1.run");
            let p = rows
                .iter()
                .find(|r| &*r.name == "p@1.run")
                .expect("缺 p@1.run");
            let c = rows
                .iter()
                .find(|r| &*r.name == "c@1.run")
                .expect("缺 c@1.run");
            assert_eq!(m.parent_call_id, 0);
            assert_eq!(p.parent_call_id, m.call_id);
            assert_eq!(c.parent_call_id, m.call_id);
            assert_eq!(m.outcome, Outcome::Ok);
            assert_eq!(p.outcome, Outcome::Ok);
            assert_eq!(c.outcome, Outcome::Ok);
        }
    }

    app.close().await;
}

#[tokio::test]
async fn outbound_timeout_preserves_session_correlation_id() {
    inflight().lock().unwrap().clear();
    let app = App::new();
    set_dispatch_policy_for(&app.root, DispatchPolicy::new(None, Some(40)));
    let records = collect(&app);

    let hang = {
        let f = app.root.plugin(
            abi_plugin("corr-hang", Inject::default(), hang_call),
            Config::Null,
        );
        f.settle().await;
        f
    };
    let mid = {
        let f = app.root.plugin(
            abi_plugin("corr-m-to", Inject::of(&["hang@1"]), mid_call),
            Config::Null,
        );
        f.settle().await;
        f
    };
    assert_eq!(hang.state(), FiberState::Active);
    assert_eq!(mid.state(), FiberState::Active);

    let sess_ctx = app.root.with_correlation("sess-timeout");
    let res = sess_ctx
        .call_iface("m@1", 1, "run_timeout", Config::Null)
        .await
        .expect("mid 返回含 error 字段的结算值");
    assert!(
        res.get("error").is_some(),
        "出站超时必须经 mid 返回错误：{res:?}"
    );

    {
        let records = records.lock().unwrap();
        let mid_row = records
            .iter()
            .find(|r| &*r.name == "m@1.run_timeout")
            .unwrap_or_else(|| panic!("缺 m@1.run_timeout：{records:?}"));
        let timeout_row = records
            .iter()
            .find(|r| &*r.name == "hang@1.run")
            .unwrap_or_else(|| panic!("缺 hang@1.run：{records:?}"));
        assert_eq!(mid_row.correlation_id.as_deref(), Some("sess-timeout"));
        assert_eq!(timeout_row.call_id, 0);
        assert_eq!(timeout_row.parent_call_id, mid_row.call_id);
        assert_eq!(timeout_row.outcome, Outcome::Err("HOST_TIMEOUT".into()));
        assert_eq!(
            timeout_row.correlation_id.as_deref(),
            Some("sess-timeout"),
            "出站看门狗超时记录必须保留发起会话的 correlation_id"
        );
    }

    app.close().await;
}
