//! `STATUS_OK` 但回复体不是合法 JSON：记录必须是 `plugin_protocol`，不是 Ok。
//!
//! 这条钉住 `call_inbound` “先解析、后 emit”的顺序；写反时记录会变成 Ok，
//! 而调用方拿到失败，归因与调用结果分叉。

mod trace_support;

use cordis_abi::{Blob, Out, OP_APPLY, OP_CALLBACK, OP_DISPOSE, STATUS_OK, STATUS_UNSUPPORTED};
use cordis_core::{App, Config, Inject, Outcome};
use serde_json::Value;
use trace_support::{abi_plugin, call_iface, collect, provide};

static CONSUMER_OUT: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

unsafe extern "C" fn bad_body_call(handle: u64, op: u32, _a: Blob, _b: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => provide(handle, "bad@1", r#"["run"]"#, 1, out),
        // `Out::write` 自身返回 STATUS_OK：这是“插件声称成功、body 却非法”的夹具。
        OP_CALLBACK => (*out).write(b"not-json"),
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

unsafe extern "C" fn consumer_call(handle: u64, op: u32, _a: Blob, _b: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => {
            let (status, text) = call_iface(handle, "bad@1", "run", 0);
            if status != STATUS_OK {
                (*out).write(text.as_bytes());
                return status;
            }
            *CONSUMER_OUT.lock().unwrap() = Some(text);
            (*out).write(b"");
            STATUS_OK
        }
        OP_CALLBACK => (*out).write(b""),
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

#[tokio::test]
async fn malformed_ok_body_is_recorded_as_plugin_protocol() {
    *CONSUMER_OUT.lock().unwrap() = None;

    let app = App::new();
    let records = collect(&app);

    let provider = app.root.plugin(
        abi_plugin("trace-bad", Inject::default(), bad_body_call),
        Config::Null,
    );
    provider.settle().await;
    let consumer = app.root.plugin(
        abi_plugin("trace-bad-consumer", Inject::of(&["bad@1"]), consumer_call),
        Config::Null,
    );
    consumer.settle().await;

    let text = CONSUMER_OUT
        .lock()
        .unwrap()
        .clone()
        .expect("consumer 拿到错误");
    let error: Value = serde_json::from_str(&text).expect("宿主返回结构化错误");
    assert_eq!(error["code"], "plugin_protocol");

    {
        let records = records.lock().unwrap();
        assert_eq!(records.len(), 1, "非法 body 只写一条：{records:?}");
        let record = &records[0];
        assert_eq!(&*record.name, "bad@1.run");
        assert_ne!(record.call_id, 0);
        assert_eq!(record.callee_uid, provider.uid());
        assert_eq!(&*record.callee_plugin, "trace-bad");
        assert_eq!(record.outcome, Outcome::Err("plugin_protocol".into()));
    }

    consumer.dispose().await;
    provider.dispose().await;
}
