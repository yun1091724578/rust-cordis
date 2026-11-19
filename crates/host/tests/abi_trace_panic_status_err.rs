//! ABI 边界的 `STATUS_ERR`（插件把 panic 收敛成错误码）走**入站结算**：
//! 记录带真实 `call_id` / `callee_uid`，不能被当成宿主 poll 边界 panic 的
//! `call_id=0` 记录。

mod trace_support;

use cordis_abi::{
    Blob, Out, OP_APPLY, OP_CALLBACK, OP_DISPOSE, STATUS_ERR, STATUS_OK, STATUS_UNSUPPORTED,
};
use cordis_core::{App, Config, Inject, Outcome};
use serde_json::Value;
use trace_support::{abi_plugin, call_iface, collect, provide};

static CONSUMER_OUT: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

unsafe extern "C" fn panic_status_err_call(
    handle: u64,
    op: u32,
    _a: Blob,
    _b: Blob,
    out: *mut Out,
) -> i32 {
    match op {
        OP_APPLY => provide(handle, "spanic@1", r#"["run"]"#, 1, out),
        OP_CALLBACK => {
            (*out).write(
                br#"{"error":"plugin panic","code":"host_panic","category":"unknown","certainty":"unknown","retry":"unknown"}"#,
            );
            STATUS_ERR
        }
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

unsafe extern "C" fn consumer_call(handle: u64, op: u32, _a: Blob, _b: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => {
            let (status, text) = call_iface(handle, "spanic@1", "run", 0);
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
async fn status_err_from_a_plugin_keeps_the_real_dispatch_identity() {
    *CONSUMER_OUT.lock().unwrap() = None;

    let app = App::new();
    let records = collect(&app);

    let provider = app.root.plugin(
        abi_plugin("trace-spanic", Inject::default(), panic_status_err_call),
        Config::Null,
    );
    provider.settle().await;
    let consumer = app.root.plugin(
        abi_plugin(
            "trace-spanic-consumer",
            Inject::of(&["spanic@1"]),
            consumer_call,
        ),
        Config::Null,
    );
    consumer.settle().await;

    let text = CONSUMER_OUT
        .lock()
        .unwrap()
        .clone()
        .expect("consumer 拿到错误");
    let error: Value = serde_json::from_str(&text).expect("结构化错误");
    assert_eq!(error["code"], "host_panic");

    {
        let records = records.lock().unwrap();
        assert_eq!(records.len(), 1, "入站结算只写一条：{records:?}");
        let record = &records[0];
        assert_eq!(&*record.name, "spanic@1.run");
        assert_ne!(record.call_id, 0, "ABI 边界的 STATUS_ERR 有真实派发 id");
        assert_eq!(record.callee_uid, provider.uid());
        assert_eq!(&*record.callee_plugin, "trace-spanic");
        assert_eq!(record.outcome, Outcome::Err("host_panic".into()));
    }

    consumer.dispose().await;
    provider.dispose().await;
}
