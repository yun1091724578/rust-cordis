//! 内核级调用方身份（`CallerIdentity`）与调用授权钩子（`CallAuthHook`）契约测试。

use cordis_core::{
    plugin_fn, App, BoxFuture, CallerIdentity, Config, Ctx, ExternService, Inject,
    MethodUnauthorized, Result, StreamSink, UID_DISPOSED, UID_ROOT,
};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

struct CountingIface {
    calls: Arc<AtomicUsize>,
    stream_calls: Arc<AtomicUsize>,
}

impl ExternService for CountingIface {
    fn service(&self) -> &str {
        "kv"
    }

    fn version(&self) -> u32 {
        1
    }

    fn methods(&self) -> Vec<String> {
        vec!["read".into(), "write".into()]
    }

    fn call(
        &self,
        _caller_meta: Option<Config>,
        method: String,
        _args: Config,
    ) -> BoxFuture<Result<Config>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move { Ok(json!({ "method": method })) })
    }

    fn call_stream(
        &self,
        _caller_meta: Option<Config>,
        method: String,
        _args: Config,
        sink: StreamSink,
    ) -> BoxFuture<Result<()>> {
        self.stream_calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            sink.push(json!({ "stream": method }));
            sink.close();
            Ok(())
        })
    }
}

#[tokio::test]
async fn caller_identity_binds_fiber_and_label_without_trace_sink() {
    let app = App::new();
    assert!(!app.store.has_trace_sink(), "本测试刻意不安装 TraceSink");

    let root_id = app.root.caller_identity();
    assert_eq!(root_id.fiber_uid, UID_ROOT);
    assert_eq!(root_id.plugin, "root");
    assert_eq!(root_id.row_label, None);
    assert!(root_id.is_root);

    let captured_self = Arc::new(std::sync::Mutex::new(None));
    let captured_derived = Arc::new(std::sync::Mutex::new(None));
    let captured_child = Arc::new(std::sync::Mutex::new(None));

    let c_self = captured_self.clone();
    let c_derived = captured_derived.clone();
    let c_child = captured_child.clone();

    let parent_plugin = plugin_fn("worker", Inject::default(), move |ctx: Ctx, _| {
        let c_self = c_self.clone();
        let c_derived = c_derived.clone();
        let c_child = c_child.clone();
        Box::pin(async move {
            *c_self.lock().unwrap() = Some(ctx.caller_identity());
            let derived = ctx.isolate("svc", None).intercept("svc", json!({ "k": 1 }));
            *c_derived.lock().unwrap() = Some(derived.caller_identity());

            let child_plugin = plugin_fn("helper", Inject::default(), move |child_ctx: Ctx, _| {
                let c_child = c_child.clone();
                Box::pin(async move {
                    *c_child.lock().unwrap() = Some(child_ctx.caller_identity());
                    Ok(())
                })
            });
            let child_fiber = ctx.plugin(child_plugin, Config::Null);
            child_fiber.settle().await;
            Ok(())
        })
    });

    let fiber = app.store.plugin_with_host_label(
        &app.root,
        parent_plugin,
        Config::Null,
        Inject::default(),
        &[],
        Some("row-alpha"),
    );
    fiber.settle().await;
    assert_eq!(fiber.host_label(), Some("row-alpha"));

    let self_id = captured_self
        .lock()
        .unwrap()
        .clone()
        .expect("parent identity");
    assert_eq!(self_id.fiber_uid, fiber.uid());
    assert_eq!(self_id.plugin, "worker");
    assert_eq!(self_id.row_label.as_deref(), Some("row-alpha"));
    assert!(!self_id.is_root);

    let derived_id = captured_derived
        .lock()
        .unwrap()
        .clone()
        .expect("derived identity");
    assert_eq!(
        derived_id, self_id,
        "isolate/intercept 派生上下文必须继承宿主绑定的同一身份"
    );

    let child_id = captured_child
        .lock()
        .unwrap()
        .clone()
        .expect("child identity");
    assert_ne!(child_id.fiber_uid, self_id.fiber_uid);
    assert_eq!(child_id.plugin, "helper");
    assert_eq!(
        child_id.row_label, None,
        "插件自行创建的子 fiber 不得继承父行的 row_label"
    );
    assert!(!child_id.is_root);

    let leaked_ctx = fiber.ctx();
    fiber.dispose().await;
    let disposed_id = leaked_ctx.caller_identity();
    assert_eq!(disposed_id.fiber_uid, UID_DISPOSED);
    assert!(
        !disposed_id.is_root,
        "已处置上下文绝不能被误判为根上下文（fail-closed）"
    );

    app.close().await;
}

#[tokio::test]
async fn call_auth_hook_blocks_call_iface_and_stream_before_provider_and_fails_closed_on_panic() {
    let app = App::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let stream_calls = Arc::new(AtomicUsize::new(0));
    app.root
        .provide_iface(
            "kv",
            Arc::new(CountingIface {
                calls: calls.clone(),
                stream_calls: stream_calls.clone(),
            }),
        )
        .expect("provide kv");

    app.store.set_call_auth_hook(Some(Arc::new(
        |caller: &CallerIdentity, service: &str, method: &str| {
            if caller.row_label.as_deref() == Some("reader") && service == "kv" && method == "read"
            {
                Ok(())
            } else {
                Err(format!("deny {caller} -> {service}.{method}"))
            }
        },
    )));

    let fiber = app.store.plugin_with_host_label(
        &app.root,
        plugin_fn("client", Inject::of(&["kv"]), |_ctx, _| {
            Box::pin(async { Ok(()) })
        }),
        Config::Null,
        Inject::of(&["kv"]),
        &[],
        Some("reader"),
    );
    fiber.settle().await;
    let client_ctx = fiber.ctx();

    // 放行的方法：call_iface 与 call_iface_stream 均正常执行。
    let ok = client_ctx
        .call_iface("kv", 1, "read", Config::Null)
        .await
        .expect("read 应放行");
    assert_eq!(ok["method"], "read");
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let mut stream = client_ctx
        .call_iface_stream("kv", 1, "read", Config::Null)
        .expect("read 流式调用应放行");
    assert_eq!(
        stream.next().await.expect("stream chunk"),
        Some(json!({ "stream": "read" }))
    );
    assert_eq!(stream_calls.load(Ordering::SeqCst), 1);

    // 拒绝的方法：在解析与提供者执行之前拦截，返回 MethodUnauthorized。
    let err = client_ctx
        .call_iface("kv", 1, "write", Config::Null)
        .await
        .expect_err("write 必须被拒绝");
    let unauth = err
        .downcast_ref::<MethodUnauthorized>()
        .expect("错误必须是 MethodUnauthorized");
    assert_eq!(unauth.service, "kv");
    assert_eq!(unauth.method, "write");
    assert_eq!(unauth.caller.row_label.as_deref(), Some("reader"));
    assert_eq!(calls.load(Ordering::SeqCst), 1, "被拒调用不得执行提供者");

    let stream_err = match client_ctx.call_iface_stream("kv", 1, "write", Config::Null) {
        Ok(_) => panic!("流式 write 必须被拒绝"),
        Err(err) => err,
    };
    assert!(
        stream_err.downcast_ref::<MethodUnauthorized>().is_some(),
        "流式拒绝必须是 MethodUnauthorized: {stream_err}"
    );
    assert_eq!(
        stream_calls.load(Ordering::SeqCst),
        1,
        "被拒流式调用不得进入提供者"
    );

    // 授权钩子内部 panic 必须按 fail-closed 拒绝，绝不放行。
    app.store.set_call_auth_hook(Some(Arc::new(
        |_caller: &CallerIdentity, _service: &str, _method: &str| {
            panic!("auth policy bug");
        },
    )));
    let panic_err = client_ctx
        .call_iface("kv", 1, "read", Config::Null)
        .await
        .expect_err("钩子 panic 必须 fail-closed 拒绝");
    assert!(
        panic_err.to_string().contains("fail-closed"),
        "应明确指出 fail-closed: {panic_err}"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1, "panic 时也不得执行提供者");

    app.close().await;
}
