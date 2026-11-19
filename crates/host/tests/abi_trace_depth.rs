//! 三跳失败链：`consumer → mid@1 → relay@1 → deep@1`，叶子返回 `STATUS_ERR`。
//!
//! 断言 ADR 0009/0010 的核心事实：一次 A→B→C→D 链产出三条记录（consumer 是
//! 根，不是一跳），`parent_call_id` 逐跳串起来，叶子 Err 的 code 原样传播。
//!
//! 夹具铁律：fake `CallFn` 内禁止 assert，结局只记进 static `Mutex`。

mod trace_support;

use cordis_abi::{
    Blob, Out, OP_APPLY, OP_CALLBACK, OP_DISPOSE, STATUS_ERR, STATUS_OK, STATUS_UNSUPPORTED,
};
use cordis_core::{App, Config, FiberState, Inject, Outcome};
use serde_json::{json, Value};
use trace_support::{abi_plugin, call_iface, collect, provide};

static CONSUMER_OUT: std::sync::Mutex<Option<(i32, String)>> = std::sync::Mutex::new(None);

fn forward_error(reply: &Value, out: *mut Out) -> i32 {
    let error = json!({
        "error": reply.get("error").cloned().unwrap_or(json!("deep 调用失败")),
        "code": reply.get("code").cloned().unwrap_or(json!("deep_bad")),
        "category": reply.get("category").cloned().unwrap_or(json!("business")),
        "certainty": reply.get("certainty").cloned().unwrap_or(json!("reported")),
        "retry": reply.get("retry").cloned().unwrap_or(json!("unknown")),
    })
    .to_string();
    unsafe { (*out).write(error.as_bytes()) };
    STATUS_ERR
}

unsafe fn forward(handle: u64, target: &str, out: *mut Out) -> i32 {
    let (status, text) = call_iface(handle, target, "run", 0);
    if status != STATUS_OK {
        (*out).write(text.as_bytes());
        return status;
    }
    let reply: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if reply.get("pending").and_then(Value::as_bool) != Some(false) {
        return STATUS_ERR;
    }
    if reply.get("result").is_some() {
        let ret = json!({ "ret": reply["result"] }).to_string();
        (*out).write(ret.as_bytes());
        STATUS_OK
    } else {
        forward_error(&reply, out)
    }
}

unsafe extern "C" fn deep_call(handle: u64, op: u32, _a: Blob, _b: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => provide(handle, "deep@1", r#"["run"]"#, 3, out),
        OP_CALLBACK => {
            (*out).write(
                br#"{"error":"deep failed","code":"deep_bad","category":"business","certainty":"reported","retry":"unknown"}"#,
            );
            STATUS_ERR
        }
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

unsafe extern "C" fn relay_call(handle: u64, op: u32, _a: Blob, _b: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => provide(handle, "relay@1", r#"["run"]"#, 2, out),
        OP_CALLBACK => forward(handle, "deep@1", out),
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

unsafe extern "C" fn mid_call(handle: u64, op: u32, _a: Blob, _b: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => provide(handle, "mid@1", r#"["run"]"#, 1, out),
        OP_CALLBACK => forward(handle, "relay@1", out),
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

unsafe extern "C" fn consumer_call(handle: u64, op: u32, _a: Blob, _b: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => {
            let outcome = call_iface(handle, "mid@1", "run", 0);
            *CONSUMER_OUT.lock().unwrap() = Some(outcome);
            (*out).write(b"");
            STATUS_OK
        }
        OP_CALLBACK => (*out).write(b""),
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

fn mount(
    app: &App,
    name: &'static str,
    inject: Inject,
    call: cordis_host::CallFn,
) -> std::sync::Arc<cordis_core::Fiber> {
    app.root
        .plugin(abi_plugin(name, inject, call), Config::Null)
}

#[tokio::test]
async fn three_hop_failure_chain_links_each_parent() {
    *CONSUMER_OUT.lock().unwrap() = None;

    let app = App::new();
    let records = collect(&app);

    let deep = mount(&app, "trace-deep", Inject::default(), deep_call);
    deep.settle().await;
    let relay = mount(&app, "trace-relay", Inject::of(&["deep@1"]), relay_call);
    relay.settle().await;
    let mid = mount(&app, "trace-mid3", Inject::of(&["relay@1"]), mid_call);
    mid.settle().await;
    let consumer = mount(
        &app,
        "trace-consumer3",
        Inject::of(&["mid@1"]),
        consumer_call,
    );
    consumer.settle().await;

    assert_eq!(deep.state(), FiberState::Active);
    assert_eq!(relay.state(), FiberState::Active);
    assert_eq!(mid.state(), FiberState::Active);
    assert_eq!(consumer.state(), FiberState::Active);

    let (_status, text) = CONSUMER_OUT
        .lock()
        .unwrap()
        .clone()
        .expect("consumer 必须发起过一次调用");
    let error: Value = serde_json::from_str(&text).expect("结构化错误应当是 JSON");
    assert_eq!(error["code"], "deep_bad", "错误码必须原样传播：{error}");

    {
        let records = records.lock().unwrap();
        assert_eq!(
            records.len(),
            3,
            "三跳 ABI 调用必须恰好三条记录：{records:?}"
        );
        let by_name = |name: &str| {
            records
                .iter()
                .find(|record| &*record.name == name)
                .unwrap_or_else(|| panic!("缺少记录 {name}：{records:?}"))
        };
        let mid_record = by_name("mid@1.run");
        let relay_record = by_name("relay@1.run");
        let deep_record = by_name("deep@1.run");

        assert_eq!(mid_record.parent_call_id, 0, "consumer 是根，不是一跳");
        assert_eq!(
            relay_record.parent_call_id, mid_record.call_id,
            "relay 的父必须是 mid 的派发 id"
        );
        assert_eq!(
            deep_record.parent_call_id, relay_record.call_id,
            "deep 的父必须是 relay 的派发 id"
        );
        for record in [mid_record, relay_record, deep_record] {
            assert_eq!(
                record.outcome,
                Outcome::Err("deep_bad".into()),
                "失败必须沿链传播：{record:?}"
            );
        }
        assert_eq!(deep_record.callee_uid, deep.uid());
        assert_eq!(relay_record.callee_uid, relay.uid());

        let leaves: Vec<_> = records
            .iter()
            .filter(|record| {
                !records
                    .iter()
                    .any(|child| child.parent_call_id == record.call_id)
            })
            .collect();
        assert_eq!(leaves.len(), 1, "按叶子聚合只能剩一跳：{records:?}");
        assert_eq!(&*leaves[0].name, "deep@1.run");
        assert!(matches!(leaves[0].outcome, Outcome::Err(_)));
    }

    consumer.dispose().await;
    mid.dispose().await;
    relay.dispose().await;
    deep.dispose().await;
}
