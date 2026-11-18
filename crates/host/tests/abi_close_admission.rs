//! 关闭后的重入只拒绝新工作，仍允许卸载清理读取服务。
use cordis_abi::{
    Blob, Out, ERROR_FORMAT_V1, OP_APPLY, OP_CALLBACK, OP_DISPOSE, STATUS_ERR, STATUS_OK,
    STATUS_UNSUPPORTED,
};
use cordis_core::{
    App, BoxFuture, Config, Ctx, Dyn, ExternService, ExternValue, Inject, Result as CoreResult,
};
use cordis_host::{apply_via_with_error_format, host_api};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

static CALLS: AtomicUsize = AtomicUsize::new(0);
static EVENTS: AtomicUsize = AtomicUsize::new(0);
static DROPS: AtomicUsize = AtomicUsize::new(0);
static DISPOSES: AtomicUsize = AtomicUsize::new(0);
static ABORTS: Mutex<Vec<Value>> = Mutex::new(Vec::new());
static RESULTS: Mutex<Vec<(u32, &'static str, i32, Value)>> = Mutex::new(Vec::new());
struct Guard;
impl Drop for Guard {
    fn drop(&mut self) {
        DROPS.fetch_add(1, Ordering::SeqCst);
    }
}
struct Service;
impl ExternService for Service {
    fn service(&self) -> &str {
        "close@1"
    }
    fn version(&self) -> u32 {
        1
    }
    fn methods(&self) -> Vec<String> {
        vec!["hang".into(), "probe".into()]
    }
    fn call(&self, _: Option<Config>, method: String, _: Config) -> BoxFuture<CoreResult<Config>> {
        CALLS.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if method == "hang" {
                let _guard = Guard;
                std::future::pending::<()>().await;
            }
            Ok(Config::Null)
        })
    }
}
unsafe fn record(phase: u32, name: &'static str, f: impl FnOnce(*mut Out) -> i32) {
    let mut buffer = [0u8; 4096];
    let mut out = Out {
        ptr: buffer.as_mut_ptr(),
        cap: buffer.len(),
        len: 0,
    };
    let status = f(&mut out);
    let value =
        serde_json::from_slice(buffer.get(..out.len).unwrap_or_default()).unwrap_or(Value::Null);
    if let Ok(mut results) = RESULTS.lock() {
        results.push((phase, name, status, value));
    }
}
unsafe fn reenter(handle: u64, phase: u32) {
    let api = host_api();
    record(phase, "iface", |out| match api.ctx_call_iface {
        Some(f) => f(
            handle,
            Blob::new(b"close@1"),
            1,
            Blob::new(b"probe"),
            Blob::new(b"[]"),
            8,
            out,
        ),
        None => STATUS_UNSUPPORTED,
    });
    for (name, call, continuation) in [("dispatch", api.ctx_dispatch, 9), ("emit", api.ctx_emit, 0)]
    {
        record(phase, name, |out| match call {
            Some(f) => f(
                handle,
                Blob::new(b"close-event"),
                Blob::new(br#"{"mode":"bail","args":[]}"#),
                continuation,
                out,
            ),
            None => STATUS_UNSUPPORTED,
        });
    }
    for (name, call) in [("get", api.ctx_get), ("require", api.ctx_require)] {
        record(phase, name, |out| match call {
            Some(f) => f(handle, Blob::new(b"cleanup"), out),
            None => STATUS_UNSUPPORTED,
        });
    }
}
unsafe extern "C" fn consumer(handle: u64, op: u32, a: Blob, _: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => {
            record(op, "initial", |slot| match host_api().ctx_call_iface {
                Some(f) => f(
                    handle,
                    Blob::new(b"close@1"),
                    1,
                    Blob::new(b"hang"),
                    Blob::new(b"[]"),
                    7,
                    slot,
                ),
                None => STATUS_UNSUPPORTED,
            });
        }
        OP_CALLBACK => {
            if let Ok(mut aborts) = ABORTS.lock() {
                aborts.push(serde_json::from_slice(a.as_bytes()).unwrap_or(Value::Null));
            }
            reenter(handle, op);
        }
        OP_DISPOSE => {
            DISPOSES.fetch_add(1, Ordering::SeqCst);
            reenter(handle, op);
        }
        _ => return STATUS_UNSUPPORTED,
    }
    (*out).write(b"")
}
#[tokio::test]
async fn close_reentry_rejects_work_but_preserves_cleanup_reads() {
    let app = App::new();
    let provider = cordis_core::plugin_fn("close-provider", Inject::default(), |ctx: Ctx, _| {
        Box::pin(async move {
            ctx.provide_iface("close@1", Arc::new(Service))?;
            ctx.provide("cleanup", Dyn::new(ExternValue(json!(42))))?;
            ctx.on("close-event", |_| {
                EVENTS.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { None })
            });
            Ok(())
        })
    });
    let provider_fiber = app.plugin(provider, Config::Null).await.unwrap();
    let plugin = cordis_core::plugin_fn(
        "close-consumer",
        Inject::of(&["close@1", "cleanup"]),
        |ctx, config| {
            apply_via_with_error_format(consumer, ctx, config, "close-consumer", ERROR_FORMAT_V1)
        },
    );
    let fiber = app.plugin(plugin, Config::Null).await.unwrap();
    assert_eq!(CALLS.load(Ordering::SeqCst), 1);
    assert_eq!(DROPS.load(Ordering::SeqCst), 0);
    assert_eq!(RESULTS.lock().unwrap()[0].3["pending"], true);
    fiber.dispose().await;
    // The native provider is deliberately still mounted during both reentry phases.
    let results = RESULTS.lock().unwrap().clone();
    for phase in [OP_CALLBACK, OP_DISPOSE] {
        for name in ["iface", "dispatch", "emit"] {
            let (_, _, status, payload) = results
                .iter()
                .find(|(p, n, _, _)| *p == phase && *n == name)
                .unwrap();
            assert_eq!(
                *status, STATUS_ERR,
                "closed {phase}/{name} admitted work: {payload}"
            );
            assert_eq!(payload["code"], "host_lifecycle", "{payload}");
            assert_eq!(payload["category"], "lifecycle", "{payload}");
            assert_eq!(payload["certainty"], "not_executed", "{payload}");
        }
        for name in ["get", "require"] {
            let (_, _, status, payload) = results
                .iter()
                .find(|(p, n, _, _)| *p == phase && *n == name)
                .unwrap();
            assert_eq!(*status, STATUS_OK, "{phase}/{name}: {payload}");
            assert_eq!(payload["value"], 42);
        }
    }
    assert_eq!(CALLS.load(Ordering::SeqCst), 1);
    assert_eq!(EVENTS.load(Ordering::SeqCst), 0);
    assert_eq!(DROPS.load(Ordering::SeqCst), 1);
    assert_eq!(DISPOSES.load(Ordering::SeqCst), 1);
    let aborts = ABORTS.lock().unwrap().clone();
    assert_eq!(aborts.len(), 1);
    assert_eq!(aborts[0]["kind"], "iface");
    assert_eq!(aborts[0]["id"], 7);
    assert_eq!(aborts[0]["args"][0]["code"], "host_lifecycle");
    assert_eq!(aborts[0]["args"][0]["category"], "lifecycle");
    assert_eq!(aborts[0]["args"][0]["certainty"], "unknown");
    provider_fiber.dispose().await;
}
