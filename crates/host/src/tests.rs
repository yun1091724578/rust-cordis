use super::*;
use std::future::Future;

thread_local! {
    static TASK_TEST_HANDLE: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

unsafe extern "C" fn task_test_call(handle: u64, op: u32, _: Blob, _: Blob, _: *mut Out) -> i32 {
    if op == OP_APPLY {
        TASK_TEST_HANDLE.set(handle);
    }
    STATUS_OK
}

#[tokio::test]
async fn o1_task_survives_normal_record_removal() {
    let app = cordis_core::App::new();
    apply_via(task_test_call, app.root.clone(), Value::Null, "task-owner")
        .await
        .unwrap();
    let handle = TASK_TEST_HANDLE.get();
    let instance = instances().read().unwrap().get(handle).unwrap();
    insert_dispatch(
        &instance,
        91,
        DispatchRecord {
            sink: None,
            frame: None,
            parent: None,
            outbound: None,
            stream: None,
        },
    )
    .unwrap_or_else(|_| panic!("admission"));
    let (_cancel, done) = register_dispatch_task(&instance, 91).unwrap();
    assert!(unregister_dispatch_node(handle, 91));
    // A task in its synchronous delivery tail cannot observe cancellation yet.
    // No lifecycle read lock is held: only task ownership can keep close pending.
    let mut close = Box::pin(app.close());
    let mut drained = Box::pin(instance.close_drained.notified());
    std::future::poll_fn(|cx| {
        assert!(
            close.as_mut().poll(cx).is_pending(),
            "close lost the task after normal record removal"
        );
        drained.as_mut().poll(cx)
    })
    .await;
    drop(done);
    close.await;
}

#[tokio::test]
async fn o1_fulfill_keeps_pending_task_cancellable() {
    let app = cordis_core::App::new();
    apply_via(
        task_test_call,
        app.root.clone(),
        Value::Null,
        "task-fulfill",
    )
    .await
    .unwrap();
    let handle = TASK_TEST_HANDLE.get();
    let instance = instances().read().unwrap().get(handle).unwrap();
    let (tx, _rx) = tokio::sync::oneshot::channel();
    insert_dispatch(
        &instance,
        92,
        DispatchRecord {
            sink: Some(tx),
            frame: None,
            parent: None,
            outbound: None,
            stream: None,
        },
    )
    .unwrap_or_else(|_| panic!("admission"));
    let (cancel, done) = register_dispatch_task(&instance, 92).unwrap();
    let mut output = Out {
        ptr: std::ptr::null_mut(),
        cap: 0,
        len: 0,
    };
    assert_eq!(
        unsafe { host_ctx_fulfill(handle, 92, Blob::EMPTY, &mut output) },
        STATUS_OK
    );
    let mut close = Box::pin(app.close());
    let mut drained = Box::pin(instance.close_drained.notified());
    std::future::poll_fn(|cx| {
        assert!(
            close.as_mut().poll(cx).is_pending(),
            "close lost the pending next task"
        );
        drained.as_mut().poll(cx)
    })
    .await;
    assert!(
        matches!(
            std::future::poll_fn(|cx| Poll::Ready(Box::pin(cancel.cancelled()).as_mut().poll(cx)))
                .await,
            Poll::Ready(())
        ),
        "fulfill must not discard task cancellation"
    );
    drop(done);
    close.await;
}

thread_local! {
    static NEXT_RETURN_STATUS: std::cell::Cell<i32> = const { std::cell::Cell::new(STATUS_OK) };
    static NEXT_CALL_STATUS: std::cell::Cell<i32> = const { std::cell::Cell::new(STATUS_ERR) };
    static NEXT_DROP_MARKER: std::cell::RefCell<Option<Arc<AtomicBool>>> = const { std::cell::RefCell::new(None) };
    static NEXT_DISPOSE_AFTER_DROP: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static NEXT_COMPLETIONS: std::cell::RefCell<Vec<Value>> = const { std::cell::RefCell::new(Vec::new()) };
    static NEXT_COMPLETED: std::cell::RefCell<Option<Arc<tokio::sync::Notify>>> = const { std::cell::RefCell::new(None) };
}

unsafe extern "C" fn next_then_terminal(
    handle: u64,
    op: u32,
    payload: Blob,
    _: Blob,
    _: *mut Out,
) -> i32 {
    if op == OP_APPLY {
        TASK_TEST_HANDLE.set(handle);
    }
    if op == OP_CALLBACK {
        let Ok(value) = serde_json::from_slice::<Value>(payload.as_bytes()) else {
            return STATUS_ERR;
        };
        if value["kind"] == KIND_NEXT {
            NEXT_COMPLETIONS.with_borrow_mut(|completions| completions.push(value));
            NEXT_COMPLETED.with_borrow(|completed| {
                if let Some(completed) = completed {
                    completed.notify_one();
                }
            });
            return STATUS_OK;
        }
        let Some(id) = value["dispatch_id"].as_u64() else {
            return STATUS_ERR;
        };
        let mut output = Out {
            ptr: std::ptr::null_mut(),
            cap: 0,
            len: 0,
        };
        NEXT_CALL_STATUS.set(host_ctx_next(handle, id, Blob::new(b"[]"), 1, &mut output));
        return NEXT_RETURN_STATUS.get();
    }
    if op == OP_DISPOSE {
        NEXT_DISPOSE_AFTER_DROP.set(NEXT_DROP_MARKER.with_borrow(|marker| {
            marker
                .as_ref()
                .is_some_and(|marker| marker.load(Ordering::SeqCst))
        }));
    }
    STATUS_OK
}

#[tokio::test]
async fn o1_real_next_survives_all_inbound_terminal_states() {
    struct Dropped(Arc<AtomicBool>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    for status in [
        STATUS_OK,
        STATUS_ERR,
        STATUS_UNSUPPORTED,
        12345,
        STATUS_PENDING,
    ] {
        NEXT_RETURN_STATUS.set(status);
        NEXT_CALL_STATUS.set(STATUS_ERR);
        let app = cordis_core::App::new();
        apply_via(
            next_then_terminal,
            app.root.clone(),
            Value::Null,
            "next-terminal",
        )
        .await
        .unwrap();
        let handle = TASK_TEST_HANDLE.get();
        let instance = instances().read().unwrap().get(handle).unwrap();
        let dropped = Arc::new(AtomicBool::new(false));
        NEXT_DROP_MARKER.with_borrow_mut(|marker| *marker = Some(dropped.clone()));
        NEXT_DISPOSE_AFTER_DROP.set(false);
        let marker = dropped.clone();
        let frame = Next::some(move |_| {
            let guard = Dropped(marker.clone());
            Box::pin(async move {
                let _guard = guard;
                std::future::pending::<()>().await;
                None
            })
        });
        let result = unsafe {
            call_inbound(
                &instance,
                handle,
                OP_CALLBACK,
                json!({}),
                None,
                None,
                Some(frame),
                None,
                false,
                None,
            )
        };
        assert_eq!(NEXT_CALL_STATUS.get(), STATUS_OK);
        if let Ok(Inbound::Pending(pending)) = result {
            let mut output = Out {
                ptr: std::ptr::null_mut(),
                cap: 0,
                len: 0,
            };
            assert_eq!(
                unsafe { host_ctx_fulfill(handle, pending.dispatch_id, Blob::EMPTY, &mut output) },
                STATUS_OK
            );
            assert_eq!(
                unsafe { host_ctx_abort(handle, pending.dispatch_id, &mut output) },
                STATUS_ERR,
                "已结算 id 不再是可 abort 的在飞派发"
            );
            assert_eq!(instance.tasks.lock().unwrap().len(), 1);
            drop(pending);
        }
        assert!(instance.dispatches.lock().unwrap().is_empty());
        assert_eq!(instance.tasks.lock().unwrap().len(), 1);
        assert!(!dropped.load(Ordering::SeqCst));
        tokio::time::timeout(std::time::Duration::from_secs(3), app.close())
            .await
            .unwrap();
        assert!(
            NEXT_DISPOSE_AFTER_DROP.get(),
            "OP_DISPOSE 必须晚于 next future drop"
        );
        NEXT_DROP_MARKER.with_borrow_mut(|marker| *marker = None);
        assert!(
            dropped.load(Ordering::SeqCst),
            "close returned before next Drop for {status}"
        );
        assert!(instance.tasks.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn o1_real_next_completes_after_inbound_record_removal() {
    for status in [STATUS_OK, STATUS_PENDING] {
        NEXT_RETURN_STATUS.set(status);
        NEXT_CALL_STATUS.set(STATUS_ERR);
        NEXT_COMPLETIONS.with_borrow_mut(Vec::clear);
        let completed = Arc::new(tokio::sync::Notify::new());
        NEXT_COMPLETED.with_borrow_mut(|slot| *slot = Some(completed.clone()));
        let app = cordis_core::App::new();
        apply_via(
            next_then_terminal,
            app.root.clone(),
            Value::Null,
            "next-normal-completion",
        )
        .await
        .unwrap();
        let handle = TASK_TEST_HANDLE.get();
        let instance = instances().read().unwrap().get(handle).unwrap();
        let release = Arc::new(tokio::sync::Notify::new());
        let started = Arc::new(AtomicBool::new(false));
        let frame = Next::some({
            let release = release.clone();
            let started = started.clone();
            move |_| {
                let release = release.clone();
                let started = started.clone();
                Box::pin(async move {
                    started.store(true, Ordering::SeqCst);
                    release.notified().await;
                    Some(Dyn::new(ExternValue(json!({ "tail": 42 }))))
                })
            }
        });
        let result = unsafe {
            call_inbound(
                &instance,
                handle,
                OP_CALLBACK,
                json!({}),
                None,
                None,
                Some(frame),
                None,
                false,
                None,
            )
        };
        assert_eq!(NEXT_CALL_STATUS.get(), STATUS_OK);
        assert!(
            started.load(Ordering::SeqCst),
            "next must first poll pending"
        );
        match result.unwrap() {
            Inbound::Ready(value) => {
                assert_eq!(status, STATUS_OK);
                assert_eq!(value, Value::Null);
            }
            Inbound::Pending(pending) => {
                assert_eq!(status, STATUS_PENDING);
                let mut output = Out {
                    ptr: std::ptr::null_mut(),
                    cap: 0,
                    len: 0,
                };
                assert_eq!(
                    unsafe {
                        host_ctx_fulfill(handle, pending.dispatch_id, Blob::EMPTY, &mut output)
                    },
                    STATUS_OK
                );
                drop(pending);
            }
        }
        assert!(instance.dispatches.lock().unwrap().is_empty());
        assert_eq!(instance.tasks.lock().unwrap().len(), 1);
        NEXT_COMPLETIONS.with_borrow(|completions| assert!(completions.is_empty()));

        release.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(3), completed.notified())
            .await
            .unwrap();
        // The current-thread task finishes its synchronous delivery tail before we resume.
        assert!(!instance.closed.load(Ordering::SeqCst));
        assert!(instance.dispatches.lock().unwrap().is_empty());
        assert!(instance.tasks.lock().unwrap().is_empty());
        NEXT_COMPLETIONS.with_borrow(|completions| {
            assert_eq!(
                completions.as_slice(),
                &[json!({ "kind": "next", "id": 1, "args": [{ "result": { "tail": 42 } }] })]
            );
        });
        app.close().await;
        NEXT_COMPLETED.with_borrow_mut(|slot| *slot = None);
    }
}

#[tokio::test]
async fn o1_multiple_tasks_reclaim_without_new_work() {
    let instance = dummy_instance("multi-task");
    insert_dispatch(
        &instance,
        93,
        DispatchRecord {
            sink: None,
            frame: None,
            parent: None,
            outbound: None,
            stream: None,
        },
    )
    .unwrap_or_else(|_| panic!("admission"));
    let (cancel_a, exit_a) = register_dispatch_task(&instance, 93).unwrap();
    let (_cancel_b, exit_b) = register_dispatch_task(&instance, 93).unwrap();
    assert_eq!(instance.tasks.lock().unwrap().len(), 2);
    drop(exit_a);
    assert_eq!(instance.tasks.lock().unwrap().len(), 1);
    assert!(std::future::poll_fn(|cx| Poll::Ready(
        Box::pin(cancel_a.cancelled()).as_mut().poll(cx)
    ))
    .await
    .is_pending());
    drop(exit_b);
    assert!(instance.tasks.lock().unwrap().is_empty());
}

#[tokio::test]
async fn o1_task_work_drops_captures_before_exit_on_panic() {
    struct CheckDrop(Arc<Mutex<HashMap<u64, DispatchTask>>>);
    impl Drop for CheckDrop {
        fn drop(&mut self) {
            assert_eq!(self.0.lock().unwrap().len(), 1);
        }
    }
    let instance = dummy_instance("panic-exit");
    insert_dispatch(
        &instance,
        94,
        DispatchRecord {
            sink: None,
            frame: None,
            parent: None,
            outbound: None,
            stream: None,
        },
    )
    .unwrap_or_else(|_| panic!("admission"));
    let task = register_dispatch_task(&instance, 94).unwrap();
    let future_guard = CheckDrop(instance.tasks.clone());
    let callback_guard = CheckDrop(instance.tasks.clone());
    // O3 起首 poll panic 不再向外 unwind：它收敛成 `DeferFailure::Panic`。
    // 这里钉住的仍是 O1 的析构序——future 与交付闭包的捕获物都必须在
    // 退出信号（`TaskExit`）之前释放（`CheckDrop` 断言 tasks 里还有 1 条）。
    let outcome = complete_or_defer::<()>(
        Box::pin(async move {
            let _guard = future_guard;
            panic!("Rust future panic");
        }),
        move |_| drop(callback_guard),
        |_| panic!("首 poll panic 时不得调用 on_panic"),
        task,
        PanicScope {
            scene: "test",
            handle: 0,
            dispatch_id: 94,
        },
    );
    assert!(
        matches!(outcome, Err(DeferFailure::Panic(_))),
        "首 poll panic 必须收敛成可区分的失败"
    );
    assert!(instance.tasks.lock().unwrap().is_empty());
}

#[tokio::test]
async fn o1_stream_settle_registers_and_reclaims() {
    let instance = dummy_instance("stream-settle");
    instance.closed.store(true, Ordering::SeqCst);
    let (tx, rx) = tokio::sync::oneshot::channel();
    let pending = PendingContinuation {
        instance: instance.clone(),
        dispatch_id: 95,
        rx,
        cancel: None,
        _abandon: PendingDropGuard {
            instance: instance.clone(),
            dispatch_id: 95,
        },
        trace_parent: None,
        traced: false,
        callee: (0, Box::from("test")),
        caller_uid: 0,
        correlation_id: None,
        name: Box::from("test"),
        started: std::time::Instant::now(),
    };
    assert!(spawn_stream_settle(&instance, Box::new(pending)).is_err());
    assert!(instance.tasks.lock().unwrap().is_empty());
    drop(tx);
    instance.closed.store(false, Ordering::SeqCst);
    let (tx, rx) = tokio::sync::oneshot::channel();
    insert_dispatch(
        &instance,
        96,
        DispatchRecord {
            sink: Some(tx),
            frame: None,
            parent: None,
            outbound: None,
            stream: None,
        },
    )
    .unwrap_or_else(|_| panic!("admission"));
    let pending = PendingContinuation {
        instance: instance.clone(),
        dispatch_id: 96,
        rx,
        cancel: None,
        _abandon: PendingDropGuard {
            instance: instance.clone(),
            dispatch_id: 96,
        },
        trace_parent: None,
        traced: false,
        callee: (0, Box::from("test")),
        caller_uid: 0,
        correlation_id: None,
        name: Box::from("test"),
        started: std::time::Instant::now(),
    };
    spawn_stream_settle(&instance, Box::new(pending)).unwrap();
    assert_eq!(instance.tasks.lock().unwrap().len(), 1);
    instance
        .dispatches
        .lock()
        .unwrap()
        .remove(&96)
        .unwrap()
        .sink
        .unwrap()
        .send(Ok(Value::Null))
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while !instance.tasks.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("真实 settle 任务完成后必须自行摘除 registry");
    assert!(instance.tasks.lock().unwrap().is_empty());
}

#[test]
fn schema_blob_rejects_null_and_impossible_lengths() {
    let null = schema_from_blob(Blob {
        ptr: std::ptr::null(),
        len: 1,
    });
    assert!(null.text.is_none(), "NULL + 非零长度不得解引用");
    assert!(null.error.is_some(), "非法 Blob 必须留下诊断");

    let empty = schema_from_blob(Blob::EMPTY);
    assert!(empty.text.is_none());
    assert!(empty.error.is_none(), "空 Blob = 未声明，不是非法");

    let huge = schema_from_blob(Blob {
        ptr: b"{\"a\":1}".as_ptr(),
        len: usize::MAX,
    });
    assert!(huge.text.is_none(), "超长长度不得传给 from_raw_parts");
    assert!(huge.error.is_some());
}

#[test]
fn schema_blob_accepts_only_utf8_json_objects() {
    assert_eq!(
        schema_from_blob(Blob::new(br#"{"type":"object"}"#)).text,
        Some(r#"{"type":"object"}"#.to_string())
    );
    let array = schema_from_blob(Blob::new(br#"["not","object"]"#));
    assert!(array.text.is_none());
    assert!(array.error.is_some(), "非对象必须可诊断，不能等同未声明");

    let bad_utf8 = schema_from_blob(Blob::new(&[0xff]));
    assert!(bad_utf8.text.is_none());
    assert!(bad_utf8.error.is_some());
}

#[test]
fn schema_over_the_display_cap_is_reported_not_parsed() {
    let oversized = format!(
        r#"{{"x":"{}"}}"#,
        "x".repeat(cordis_abi::MAX_CONFIG_SCHEMA_BYTES)
    );
    let read = schema_from_text(oversized);
    assert!(read.text.is_none());
    assert!(read.error.unwrap().contains("上限"));
}

#[test]
fn capabilities_of_desc_gates_before_reading_tail_fields() {
    let desc = PluginDesc {
        abi_version: ABI_VERSION,
        size: 40,
        id: Blob::EMPTY,
        inject: Blob::EMPTY,
        schema: Blob::EMPTY,
        attach: fake_attach,
        call: fake_call,
        provision: Blob::EMPTY,
        error_format: 0,
        tail_reserved: 0,
    };
    let caps = unsafe { Capabilities::of_desc(&desc) };
    assert_eq!(caps, Capabilities::default(), "短 desc 不得读尾字段");
}

unsafe extern "C" fn fake_attach(_: *const HostApi) {}

#[test]
fn capabilities_of_desc_preserves_inject_order_like_of_plugin() {
    static ID: &[u8] = b"order";
    static INJECT: &[u8] = br#"["b","a"]"#;
    let desc = PluginDesc {
        abi_version: ABI_VERSION,
        size: std::mem::size_of::<PluginDesc>() as u32,
        id: Blob::new(ID),
        inject: Blob::new(INJECT),
        schema: Blob::EMPTY,
        attach: fake_attach,
        call: fake_caps_call,
        provision: Blob::EMPTY,
        error_format: 0,
        tail_reserved: 0,
    };
    let caps = unsafe { Capabilities::of_desc(&desc) };
    assert_eq!(caps.inject, vec!["b".to_string(), "a".to_string()]);
}

unsafe extern "C" fn fake_caps_call(_: u64, op: u32, _: Blob, _: Blob, out: *mut Out) -> i32 {
    if op == cordis_abi::OP_VOLATILE_PATHS {
        (*out).write(b"[]");
    }
    STATUS_OK
}

#[tokio::test]
async fn pending_settle_preserves_a_structured_host_failure() {
    let instance = dummy_instance("settle-cause");
    let (tx, rx) = tokio::sync::oneshot::channel();
    let pending = PendingContinuation {
        instance: instance.clone(),
        dispatch_id: 0x5E7,
        rx,
        cancel: None,
        _abandon: PendingDropGuard {
            instance,
            dispatch_id: 0x5E7,
        },
        trace_parent: None,
        traced: false,
        callee: (0, Box::from("test")),
        caller_uid: 0,
        correlation_id: None,
        name: Box::from("test"),
        started: std::time::Instant::now(),
    };
    let payload = ErrorPayload::new("插件已卸载：挂起的入站续延被结算")
        .with_code("host_lifecycle")
        .with_category("lifecycle")
        .with_certainty("unknown");
    tx.send(Err(payload)).expect("sink 必须接收结构化原因");

    let failure = pending
        .settle("接口方法")
        .await
        .expect_err("卸载原因必须作为失败返回");
    assert_eq!(failure.payload.code, "host_lifecycle");
    assert_eq!(failure.payload.category, "lifecycle");
    assert_eq!(failure.payload.certainty, "unknown");
}

fn dummy_instance(name: &str) -> Instance {
    let _ = name;
    let app = Box::leak(Box::new(cordis_core::App::new()));
    Instance {
        ctx: app.root.clone(),
        call: fake_call,
        dispatches: Arc::new(Mutex::new(HashMap::new())),
        keepalive: None,
        closed: Arc::new(AtomicBool::new(false)),
        apply_timed_out: Arc::new(AtomicBool::new(false)),
        deferred_cleanups: Arc::new(Mutex::new(Vec::new())),
        lifecycle: Arc::new(Mutex::new(0)),
        calls_changed: Arc::new(tokio::sync::Notify::new()),
        credits: Arc::new(Mutex::new(HashMap::new())),
        #[cfg(test)]
        close_drained: Arc::new(tokio::sync::Notify::new()),
        #[cfg(test)]
        observer_gate: Arc::new(Mutex::new(None)),
        #[cfg(test)]
        outbound_gate: Arc::new(Mutex::new(None)),
        #[cfg(test)]
        cancel_gate: Arc::new(Mutex::new(None)),
        tasks: Arc::new(Mutex::new(HashMap::new())),
        deferred_outbounds: Arc::new(Mutex::new(Vec::new())),
        error_format: 0,
        dispatch_policy: DispatchPolicy::default(),
        host_limits: HostLimits::default(),
    }
}

unsafe extern "C" fn fake_call(_: u64, _: u32, _: Blob, _: Blob, _: *mut Out) -> i32 {
    STATUS_OK
}

unsafe extern "C" fn bench_call(_: u64, _: u32, _: Blob, _: Blob, out: *mut Out) -> i32 {
    (*out).write(b"pong")
}

#[test]
fn call_buffer_pool_does_not_expose_stale_bytes() {
    let mut first = CallBuffer::take();
    first.0[..8].copy_from_slice(b"leftover");
    drop(first);
    let second = CallBuffer::take();
    assert_eq!(second.len(), 16 * 1024);
    assert!(
        second.0.iter().all(|byte| *byte == 0),
        "池化缓冲必须清零，不能把上一个插件的残留交给下一个插件"
    );
}

/// 手动基准：`cargo test -p rust-cordis-host --lib call_buffer_pool_benchmark -- --ignored --nocapture`
#[test]
#[ignore = "manual buffer-pool benchmark"]
fn call_buffer_pool_benchmark() {
    const CALLS: u32 = 100_000;
    let started = std::time::Instant::now();
    for _ in 0..CALLS {
        let (status, text) = unsafe { invoke_raw(bench_call, 1, cordis_abi::OP_PING, "{}") };
        assert_eq!(status, STATUS_OK);
        assert_eq!(text, "pong");
    }
    let elapsed = started.elapsed();
    println!(
        "call buffer benchmark: {CALLS} calls in {:?} ({:.1} ns/call)",
        elapsed,
        elapsed.as_nanos() as f64 / CALLS as f64
    );
}

/// 句柄带代际号：槽位复用后，**陈旧句柄必须被拒绝**。
///
/// 这是热重载系统里最危险的一类 bug 的防线：卸载后残留的句柄，
/// 如果在新插件装载后又能解析成"一个合法但完全错误的实例"，
/// 就会变成极难定位的跨插件串线。
#[test]
fn stale_handles_are_rejected_after_slot_reuse() {
    let mut slab = Slab::default();
    let first = slab.insert(dummy_instance("first"), None);
    assert!(slab.get(first).is_some());

    // 卸载 → 槽位进入空闲表。
    assert!(slab.remove(first).is_some());
    assert!(slab.get(first).is_none(), "卸载后句柄立刻失效");

    // 新实例复用同一个槽位。
    let second = slab.insert(dummy_instance("second"), None);
    assert_eq!(
        Slab::split(first).0,
        Slab::split(second).0,
        "槽位确实被复用了"
    );
    assert_ne!(first, second, "但代际号不同，所以句柄不同");
    assert!(slab.get(first).is_none(), "陈旧句柄绝不能解析到新实例");
    assert!(slab.get(second).is_some());
}

/// 句柄 0 是保留值，永远不是合法实例。
#[test]
fn handle_zero_is_reserved() {
    let mut slab = Slab::default();
    assert!(slab.get(0).is_none());
    let handle = slab.insert(dummy_instance("x"), None);
    assert_ne!(handle, 0);
    assert!(slab.get(handle).is_some());
}

/// 反复插入/移除不泄漏槽位。
#[test]
fn slots_are_recycled_not_leaked() {
    let mut slab = Slab::default();
    let handles: Vec<u64> = (0..8)
        .map(|i| slab.insert(dummy_instance(&format!("p{i}")), None))
        .collect();
    assert_eq!(slab.live(), 8);
    for handle in &handles {
        assert!(slab.remove(*handle).is_some());
    }
    assert_eq!(slab.live(), 0);
    // 再插入 8 个：槽位应当被复用，容量不增长。
    for i in 0..8 {
        slab.insert(dummy_instance(&format!("q{i}")), None);
    }
    assert_eq!(slab.slots.len(), 8, "槽位应被复用而不是无限增长");
}

// ------------------------------------------------------ M1-1：ABI 装载/卸载生命周期

// 假 ABI 插件的调用日志与句柄。
//
// `thread_local!` 而非 `static`：`#[tokio::test]` 默认 current_thread，
// `Store::plugin` 的 settle、dispose 的 disposer 任务都在测试线程上执行，
// 因此每个用例自带状态、互不干扰。
thread_local! {
    static ABI_TEST_LOG: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
    static ABI_TEST_HANDLE: std::cell::Cell<u64> = const { std::cell::Cell::new(u64::MAX) };
}

/// 假 ABI 插件入口（模拟 SDK 的行为）：
/// - OP_APPLY：经 `host_ctx_effect` 注册一个 cleanup；
/// - OP_CLEANUP（cleanup 触发）：用**同一个 handle** 读外部服务 `abi-test-svc`，记录结果；
/// - OP_DISPOSE（卸载通知）：记录顺序。
///
/// 注意：fake `CallFn` 内**禁止 assert**（panic 跨 `extern "C"` 即 abort），
/// 只记录状态，由测试体断言。
unsafe extern "C" fn abi_fake_call(handle: u64, op: u32, _a: Blob, _b: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => {
            ABI_TEST_HANDLE.with(|cell| cell.set(handle));
            let mut buf = [0u8; 64];
            let mut cleanup_out = Out {
                ptr: buf.as_mut_ptr(),
                cap: buf.len(),
                len: 0,
            };
            let status = host_ctx_effect(handle, 7, &mut cleanup_out);
            if status != STATUS_OK {
                return status;
            }
            (*out).write(b"")
        }
        cordis_abi::OP_CLEANUP => {
            let mut buf = [0u8; 1024];
            let mut read_out = Out {
                ptr: buf.as_mut_ptr(),
                cap: buf.len(),
                len: 0,
            };
            let status = host_ctx_get(handle, Blob::new(b"abi-test-svc"), &mut read_out);
            let text = std::str::from_utf8(std::slice::from_raw_parts(read_out.ptr, read_out.len))
                .unwrap_or("<non-utf8>")
                .to_string();
            ABI_TEST_LOG.with(|log| {
                log.borrow_mut().push(if status == STATUS_OK {
                    format!("cleanup:Ok({text})")
                } else {
                    format!("cleanup:Err({text})")
                })
            });
            (*out).write(b"")
        }
        OP_DISPOSE => {
            ABI_TEST_LOG.with(|log| log.borrow_mut().push("dispose".to_string()));
            (*out).write(b"")
        }
        _ => cordis_abi::STATUS_UNSUPPORTED,
    }
}

/// M1-1（红→绿）：插件 cleanup 必须在**句柄失效之前**执行、且能读到服务依赖。
///
/// 规格（对齐 JS cordis：cleanup 是 fiber 的 effect，卸载时 LIFO 逆序释放）：
/// 整个拆除过程（含插件 cleanup）期间 ctx 句柄有效；OP_DISPOSE 是"宿主即将回收
/// 实例"的通知，必须**最后**投递。
///
/// 预期红（修复前）：`["dispose", "cleanup:Err(ctx 句柄 … 已失效)"]`——
/// dispose effect 注册在 OP_APPLY **之后**，LIFO 下它先跑，把句柄提前注销了。
#[tokio::test]
async fn abi_cleanup_can_read_services_before_dispose() {
    ABI_TEST_LOG.with(|log| log.borrow_mut().clear());

    let app = cordis_core::App::new();
    // 外部提供者：独立原生插件、保持 ACTIVE —— cleanup 读的是它。
    // （不能读"自己提供的服务"：拆除期提供者已进入 UNLOADING，strict 读必然失败。）
    let provider = cordis_core::plugin_fn(
        "abi-test-provider",
        cordis_core::Inject::default(),
        |ctx: Ctx, _cfg| {
            Box::pin(async move {
                ctx.provide("abi-test-svc", Dyn::new(ExternValue(json!({ "ok": true }))))?;
                Ok(())
            })
        },
    );
    let provider_fiber = app.root.plugin(provider, Config::Null);
    provider_fiber.settle().await;
    assert_eq!(provider_fiber.state(), cordis_core::FiberState::Active);

    // ABI 风格插件：FnPlugin 包装 apply_via + 假插件入口。
    let plugin: Arc<dyn Plugin> = Arc::new(cordis_core::FnPlugin {
        name: "abi-test",
        key: cordis_core::unique_key("abi-test"),
        inject: cordis_core::Inject::default(),
        validator: None,
        f: Arc::new(|ctx: Ctx, config: Config| apply_via(abi_fake_call, ctx, config, "abi-test")),
    });
    let fiber = app.root.plugin(plugin, Config::Null);
    fiber.settle().await;
    assert_eq!(
        fiber.state(),
        cordis_core::FiberState::Active,
        "假插件应装载成功"
    );

    fiber.dispose().await;

    let log = ABI_TEST_LOG.with(|log| log.borrow().clone());
    assert_eq!(
        log.len(),
        2,
        "应恰好有一次 cleanup 与一次 dispose 通知：{log:?}"
    );
    assert!(
        log[0].starts_with("cleanup:Ok("),
        "cleanup 必须在句柄失效前执行：{log:?}"
    );
    assert!(
        log[0].contains("\"ok\":true"),
        "cleanup 应读到外部服务的值：{log:?}"
    );
    assert_eq!(
        log[1], "dispose",
        "OP_DISPOSE 必须在 cleanup 之后投递：{log:?}"
    );
}

/// 假 ABI 插件入口：OP_APPLY 直接失败；其余 op 一律记录（用于断言"不该发生"）。
unsafe extern "C" fn abi_fake_call_failing(
    handle: u64,
    op: u32,
    _a: Blob,
    _b: Blob,
    out: *mut Out,
) -> i32 {
    match op {
        OP_APPLY => {
            ABI_TEST_HANDLE.with(|cell| cell.set(handle));
            let _ = (*out).write(b"boom");
            STATUS_ERR
        }
        OP_DISPOSE => {
            ABI_TEST_LOG.with(|log| log.borrow_mut().push("dispose".to_string()));
            (*out).write(b"")
        }
        _ => cordis_abi::STATUS_UNSUPPORTED,
    }
}

/// 假 ABI 插件入口：OP_APPLY 声称写了超过宿主缓冲的字节，但不实际写入。
unsafe extern "C" fn abi_fake_call_over_cap(
    _handle: u64,
    op: u32,
    _a: Blob,
    _b: Blob,
    out: *mut Out,
) -> i32 {
    if op == OP_APPLY {
        (*out).len = 20_000;
        STATUS_ERR
    } else {
        cordis_abi::STATUS_UNSUPPORTED
    }
}

/// apply 的错误读取也必须遵守 `out.len <= cap`，不能把零缓冲当成消息。
#[tokio::test]
async fn apply_error_output_is_bounded_and_readable() {
    let app = cordis_core::App::new();
    let plugin: Arc<dyn Plugin> = Arc::new(cordis_core::FnPlugin {
        name: "apply-over-cap",
        key: cordis_core::unique_key("apply-over-cap"),
        inject: cordis_core::Inject::default(),
        validator: None,
        f: Arc::new(|ctx: Ctx, config: Config| {
            apply_via_with_error_format(
                abi_fake_call_over_cap,
                ctx,
                config,
                "apply-over-cap",
                cordis_abi::ERROR_FORMAT_V1,
            )
        }),
    });
    let fiber = app.root.plugin(plugin, Config::Null);
    fiber.settle().await;

    assert_eq!(fiber.state(), cordis_core::FiberState::Failed);
    let error = fiber.error().expect("失败的 apply 必须留下错误");
    assert!(
        error.contains("超过宿主缓冲"),
        "必须给出有界诊断，而不是 NUL 填充串：{error:?}"
    );
    assert!(!error.contains('\0'), "错误消息不得包含 NUL：{error:?}");
}

unsafe extern "C" fn stream_unused_call(_: u64, _: u32, _: Blob, _: Blob, _: *mut Out) -> i32 {
    STATUS_UNSUPPORTED
}

static HOST_TOO_SMALL_READ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static HOST_TOO_SMALL_WRITE: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

unsafe extern "C" fn too_small_once_call(_: u64, op: u32, _: Blob, _: Blob, out: *mut Out) -> i32 {
    if op == cordis_abi::OP_VOLATILE_PATHS
        && HOST_TOO_SMALL_READ.fetch_add(1, Ordering::SeqCst) == 0
    {
        (*out).len = 32 * 1024;
        return STATUS_TOO_SMALL;
    }
    (*out).write(br#"{"ret":["x"]}"#)
}

unsafe extern "C" fn too_small_always_call(_: u64, _: u32, _: Blob, _: Blob, out: *mut Out) -> i32 {
    HOST_TOO_SMALL_WRITE.fetch_add(1, Ordering::SeqCst);
    (*out).len = 32 * 1024;
    STATUS_TOO_SMALL
}

#[test]
fn host_too_small_retries_reads_but_not_writes() {
    HOST_TOO_SMALL_READ.store(0, Ordering::SeqCst);
    HOST_TOO_SMALL_WRITE.store(0, Ordering::SeqCst);

    let value =
        unsafe { call_op_read(too_small_once_call, 0, cordis_abi::OP_VOLATILE_PATHS, "", 0) }
            .expect("只读路径应允许扩容");
    match value {
        CallOutcome::Ok(value) => assert_eq!(value, serde_json::json!(["x"])),
        _ => panic!("只读路径应返回解析后的值"),
    }
    assert_eq!(
        HOST_TOO_SMALL_READ.load(Ordering::SeqCst),
        2,
        "只读路径应重试一次"
    );

    let error = match unsafe { call_op(too_small_always_call, 0, OP_CALLBACK, "", 0) } {
        Err(error) => error,
        Ok(_) => panic!("写路径不得重放"),
    };
    assert!(error.contains("不可重试"), "{error}");
    assert_eq!(
        HOST_TOO_SMALL_WRITE.load(Ordering::SeqCst),
        1,
        "写路径只能调用一次"
    );
}

/// 与实现里的私有上限对齐（crate 内用例才读得到它）。
const READ_CAP: usize = MAX_READ_RETRY_BYTES;

thread_local! {
    static READ_CLAIM: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static READ_CLAIM_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static WRITE_CLAIM_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn read_claim(claim: usize) {
    READ_CLAIM.set(claim);
    READ_CLAIM_CALLS.set(0);
}

/// `{"ret":"xxx…"}`，总长度恰好 `total` 字节（前缀 8 + 后缀 2）。
fn big_ret_json(total: usize) -> String {
    format!(r#"{{"ret":"{}"}}"#, "x".repeat(total - 10))
}

/// 第一次申报 `claim` 字节；第二次回填**恰好 claim 字节**的合法 JSON。
unsafe extern "C" fn read_claim_call(_: u64, _: u32, _: Blob, _: Blob, out: *mut Out) -> i32 {
    let calls = READ_CLAIM_CALLS.get() + 1;
    READ_CLAIM_CALLS.set(calls);
    let claim = READ_CLAIM.get();
    if calls == 1 {
        (*out).len = claim;
        return STATUS_TOO_SMALL;
    }
    (*out).write(big_ret_json(claim).as_bytes())
}

/// 永远申报同一个 `claim` 并报 TOO_SMALL：坏插件的"我就是不够"。
unsafe extern "C" fn read_claim_forever_call(
    _: u64,
    _: u32,
    _: Blob,
    _: Blob,
    out: *mut Out,
) -> i32 {
    READ_CLAIM_CALLS.set(READ_CLAIM_CALLS.get() + 1);
    (*out).len = READ_CLAIM.get();
    STATUS_TOO_SMALL
}

/// 写 op 的坏申报：一次就是 usize::MAX。
unsafe extern "C" fn write_huge_claim_call(_: u64, _: u32, _: Blob, _: Blob, out: *mut Out) -> i32 {
    WRITE_CLAIM_CALLS.set(WRITE_CLAIM_CALLS.get() + 1);
    (*out).len = usize::MAX;
    STATUS_TOO_SMALL
}

/// O8b-1 的上限取数与边界（与 `READ_CAP`/`read_retry_cap` 一对一）。
#[test]
fn read_retry_cap_rejects_out_of_range_and_oversize_lengths() {
    assert_eq!(
        read_retry_cap(0),
        Ok(16 * 1024),
        "申报小于初始缓冲时仍按 16 KiB 起步"
    );
    assert_eq!(read_retry_cap(200 * 1024), Ok(200 * 1024));
    assert_eq!(read_retry_cap(READ_CAP), Ok(READ_CAP), "上限本身可用");
    assert!(read_retry_cap(READ_CAP + 1).is_err(), "上限 +1 必须被拒");
    assert!(
        read_retry_cap(isize::MAX as usize).is_err(),
        "isize::MAX 也要被拒（真实 OOM 会 abort）"
    );
    assert!(read_retry_cap(usize::MAX).is_err(), "usize::MAX 必须被拒");
}

/// O8b-1 a. 上限内的扩容重试照常工作，并拿回**完整**内容。
#[test]
fn read_retry_within_the_cap_returns_the_full_payload() {
    read_claim(200 * 1024);
    let value = unsafe { call_op_read(read_claim_call, 0, cordis_abi::OP_VOLATILE_PATHS, "", 0) }
        .expect("上限内的只读扩容应成功");
    match value {
        CallOutcome::Ok(value) => assert_eq!(
            value.as_str().map(str::len),
            Some(200 * 1024 - 10),
            "必须拿回完整内容，而不是被截断/补零的缓冲"
        ),
        _ => panic!("只读路径应返回解析后的值"),
    }
    assert_eq!(READ_CLAIM_CALLS.get(), 2, "只读路径应重试一次");
}

/// O8b-1 b. 上限 +1 被拒：拒绝发生在**分配之前**，文案点名上限。
#[test]
fn read_retry_past_the_cap_is_rejected_before_allocating() {
    read_claim(READ_CAP + 1);
    let error = match unsafe {
        call_op_read(
            read_claim_forever_call,
            0,
            cordis_abi::OP_VOLATILE_PATHS,
            "",
            0,
        )
    } {
        Err(error) => error,
        Ok(_) => panic!("超过只读扩容上限必须拒绝"),
    };
    assert!(
        error.contains("扩容上限"),
        "必须点名上限；得到 {} 字节的诊断",
        error.len()
    );
    assert!(!error.contains('\0'));
    assert_eq!(READ_CLAIM_CALLS.get(), 1, "超限时不得再调一次（不扩容）");
}

/// O8b-1 c. `usize::MAX` / `isize::MAX` 被拒且不 panic（也不真去分配）。
#[test]
fn absurd_read_claims_are_rejected_instead_of_panicking() {
    for claim in [usize::MAX, isize::MAX as usize] {
        read_claim(claim);
        let error = match unsafe {
            call_op_read(
                read_claim_forever_call,
                0,
                cordis_abi::OP_VOLATILE_PATHS,
                "",
                0,
            )
        } {
            Err(error) => error,
            Ok(_) => panic!("{claim} 字节的申报必须被拒绝"),
        };
        assert!(
            error.contains("拒绝分配"),
            "{claim} 字节的申报必须被明确拒绝；得到 {} 字节的诊断",
            error.len()
        );
        assert!(!error.contains('\0'));
        assert_eq!(READ_CLAIM_CALLS.get(), 1);
    }
}

/// O8b-1 d. 第二次仍是 TOO_SMALL 时给出可读原因，而不是全零缓冲的 NUL 串。
#[test]
fn a_second_too_small_reports_a_readable_reason() {
    read_claim(2 * 1024);
    let error = match unsafe {
        call_op_read(
            read_claim_forever_call,
            0,
            cordis_abi::OP_VOLATILE_PATHS,
            "",
            0,
        )
    } {
        Err(error) => error,
        Ok(_) => panic!("二次 TOO_SMALL 必须报错"),
    };
    assert!(
        !error.contains('\0'),
        "错误消息不得是 NUL 填充串（{} 字节）",
        error.len()
    );
    assert!(!error.is_empty(), "必须给出原因");
    assert!(error.contains("仍"), "必须说清扩容之后仍然不够：{error}");
}

/// O8b-1 e. 写 op 遇到巨量申报仍然只调用一次（不重放、不分配）。
#[test]
fn huge_write_claims_still_call_the_plugin_exactly_once() {
    WRITE_CLAIM_CALLS.set(0);
    let error = match unsafe { call_op(write_huge_claim_call, 0, OP_CALLBACK, "", 0) } {
        Err(error) => error,
        Ok(_) => panic!("写路径不得重放"),
    };
    assert!(error.contains("不可重试"), "{error}");
    assert!(!error.contains('\0'));
    assert_eq!(WRITE_CLAIM_CALLS.get(), 1, "写路径只能调用一次");
}

#[test]
fn closed_next_preserves_frame_and_task_before_drain() {
    let app = cordis_core::App::new();
    let handle = register_instance_with_keepalive(
        app.root.clone(),
        stream_unused_call,
        None,
        cordis_abi::ERROR_FORMAT_V1,
    )
    .unwrap();
    let instance = instances().read().unwrap().get(handle).unwrap();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let tail_calls = calls.clone();
    let frame = Next::some(move |_| {
        tail_calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { None })
    });
    assert!(insert_dispatch(
        &instance,
        1,
        DispatchRecord {
            sink: None,
            frame: Some(frame),
            parent: None,
            outbound: None,
            stream: None,
        }
    )
    .is_ok());
    {
        let _table = instance.dispatches.lock().unwrap();
        instance.closed.store(true, Ordering::SeqCst);
    }
    let mut buffer = [0u8; 4096];
    let mut out = Out {
        ptr: buffer.as_mut_ptr(),
        cap: buffer.len(),
        len: 0,
    };
    let status = unsafe { host_ctx_next(handle, 1, Blob::new(b"[]"), 9, &mut out) };
    let payload: Value = serde_json::from_slice(&buffer[..out.len]).unwrap_or(Value::Null);
    let (frame_preserved, task_absent) = {
        let table = instance.dispatches.lock().unwrap();
        let record = table.get(&1).unwrap();
        (
            record.frame.is_some(),
            instance.tasks.lock().unwrap().is_empty(),
        )
    };
    unregister_instance(handle);
    assert_eq!(status, STATUS_ERR, "closed next admitted work: {payload}");
    assert_eq!(payload["code"], "host_lifecycle");
    assert_eq!(payload["category"], "lifecycle");
    assert_eq!(payload["certainty"], "not_executed");
    assert!(frame_preserved, "拒绝不得消费有效 Next frame");
    assert!(task_absent, "拒绝不得赋值 task");
    assert_eq!(calls.load(Ordering::SeqCst), 0, "拒绝不得执行尾链");
}

#[test]
fn closed_dispatch_admission_precedes_capacity() {
    let app = cordis_core::App::new();
    set_host_limits_for(
        &app.root,
        HostLimits {
            max_dispatches_per_instance: 1,
            ..HostLimits::default()
        },
    )
    .unwrap();
    let handle = register_instance_with_keepalive(
        app.root.clone(),
        stream_unused_call,
        None,
        cordis_abi::ERROR_FORMAT_V1,
    )
    .unwrap();
    let instance = instances().read().unwrap().get(handle).unwrap();
    let record = || DispatchRecord {
        sink: None,
        frame: Some(Next::none()),
        parent: None,
        outbound: None,
        stream: None,
    };
    assert!(insert_dispatch(&instance, 1, record()).is_ok());
    {
        let _table = instance.dispatches.lock().unwrap();
        instance.closed.store(true, Ordering::SeqCst);
    }
    let result = insert_dispatch(&instance, 2, record());
    let count = instance.dispatches.lock().unwrap().len();
    unregister_instance(handle);
    let (payload, rejected) = result.expect_err("closed 必须拒绝记录");
    assert_eq!(payload.code, "host_lifecycle", "关闭优先于容量");
    assert_eq!(payload.category, "lifecycle");
    assert_eq!(payload.certainty, "not_executed");
    assert_eq!(count, 1);
    assert!(rejected.frame.is_some(), "拒绝必须归还 waterfall frame");
}

/// 流式宿主函数的业务错误也必须按插件能力位写结构化载荷。
#[test]
fn stream_host_errors_are_structured_for_capable_plugins() {
    let app = cordis_core::App::new();
    let handle = register_instance_with_keepalive(
        app.root.clone(),
        stream_unused_call,
        None,
        cordis_abi::ERROR_FORMAT_V1,
    )
    .expect("默认容量下实例必须能登记");
    {
        let instances = instances().read().unwrap();
        let instance = instances.get(handle).expect("实例必须已登记");
        assert!(
            insert_dispatch(
                &instance,
                1,
                DispatchRecord {
                    sink: None,
                    frame: None,
                    parent: None,
                    outbound: None,
                    stream: None,
                },
            )
            .is_ok(),
            "默认容量下派发记录必须能插入"
        );
    }

    let mut buffer = vec![0u8; 4096];
    let mut out = Out {
        ptr: buffer.as_mut_ptr(),
        cap: buffer.len(),
        len: 0,
    };
    let status = unsafe { host_stream_close(handle, 1, &mut out) };
    assert_eq!(status, STATUS_ERR);
    buffer.truncate(out.len);
    let payload = cordis_abi::ErrorPayload::from_json(&String::from_utf8(buffer).unwrap())
        .expect("能力位为 1 时流错误必须是结构化的");
    assert_eq!(payload.code, "host_call");
    assert_eq!(payload.category, "protocol");
    assert!(payload.error.contains("不是流"), "{}", payload.error);

    unregister_instance(handle);
}

/// 生命周期类宿主失败（例如 unknown / duplicate fulfill）必须带生命周期分类，
/// 不能和普通协议错误共用同一个哨兵。
#[test]
fn fulfill_failures_are_structured_as_lifecycle() {
    let app = cordis_core::App::new();
    let handle = register_instance_with_keepalive(
        app.root.clone(),
        stream_unused_call,
        None,
        cordis_abi::ERROR_FORMAT_V1,
    )
    .expect("默认容量下实例必须能登记");
    let mut buffer = vec![0u8; 4096];
    let mut out = Out {
        ptr: buffer.as_mut_ptr(),
        cap: buffer.len(),
        len: 0,
    };
    let status = unsafe { host_ctx_fulfill(handle, 999, Blob::new(b"null"), &mut out) };
    assert_eq!(status, STATUS_ERR);
    buffer.truncate(out.len);
    let payload = cordis_abi::ErrorPayload::from_json(&String::from_utf8(buffer).unwrap())
        .expect("能力位为 1 时 fulfill 错误必须是结构化的");
    assert_eq!(payload.code, "host_fulfill");
    assert_eq!(payload.category, "lifecycle");
    assert!(payload.error.contains("不存在"), "{}", payload.error);

    unregister_instance(handle);
}

/// 宿主函数的预解析失败必须报协议错误，不能把非法 JSON 静默降级成 `null`。
#[test]
fn malformed_host_arguments_are_protocol_errors_not_null() {
    let mut buffer = vec![0u8; 4096];
    let mut out = Out {
        ptr: buffer.as_mut_ptr(),
        cap: buffer.len(),
        len: 0,
    };

    let status = unsafe {
        host_ctx_use(
            0,
            Blob::new(b"@native:missing"),
            Blob::new(b"not-json"),
            &mut out,
        )
    };
    assert_eq!(status, STATUS_ERR);
    let text = String::from_utf8_lossy(&buffer[..out.len]).to_string();
    assert!(text.contains("不是合法 JSON"), "{text}");

    out.len = 0;
    let status = unsafe {
        host_ctx_call_iface(
            0,
            Blob::new(b"svc"),
            1,
            Blob::new(b"method"),
            Blob::new(b"not-json"),
            0,
            &mut out,
        )
    };
    assert_eq!(status, STATUS_ERR);
    let text = String::from_utf8_lossy(&buffer[..out.len]).to_string();
    assert!(text.contains("不是合法 JSON"), "{text}");

    out.len = 0;
    let status = unsafe { host_ctx_next(0, 1, Blob::new(b"not-json"), 0, &mut out) };
    assert_eq!(status, STATUS_ERR);
    let text = String::from_utf8_lossy(&buffer[..out.len]).to_string();
    assert!(text.contains("不是合法 JSON"), "{text}");

    out.len = 0;
    let status =
        unsafe { dispatch_impl(0, Blob::new(b"event"), Blob::new(b"not-json"), 0, &mut out) };
    assert_eq!(status, STATUS_ERR);
    let text = String::from_utf8_lossy(&buffer[..out.len]).to_string();
    assert!(text.contains("不是合法 JSON"), "{text}");
}

/// M1-1 守卫（非红：修复前后都应通过）：apply 失败的实例**不会**收到 OP_DISPOSE，
/// 且句柄不会泄漏。
///
/// 句柄回收由 `apply_via` 里那个"注册在 OP_APPLY 之前"的 effect 单点负责——
/// fiber 的失败清理（`Fiber::fail` → `dispose_effects`）会跑到它。
#[tokio::test]
async fn failed_apply_never_receives_dispose_and_leaks_no_handle() {
    ABI_TEST_LOG.with(|log| log.borrow_mut().clear());
    ABI_TEST_HANDLE.with(|cell| cell.set(u64::MAX));

    let app = cordis_core::App::new();
    let plugin: Arc<dyn Plugin> = Arc::new(cordis_core::FnPlugin {
        name: "abi-test-failing",
        key: cordis_core::unique_key("abi-test-failing"),
        inject: cordis_core::Inject::default(),
        validator: None,
        f: Arc::new(|ctx: Ctx, config: Config| {
            apply_via(abi_fake_call_failing, ctx, config, "abi-test-failing")
        }),
    });
    let fiber = app.root.plugin(plugin, Config::Null);
    fiber.settle().await;
    assert_eq!(
        fiber.state(),
        cordis_core::FiberState::Failed,
        "apply 失败应使 fiber 进入 FAILED"
    );
    assert!(fiber.error().unwrap().contains("boom"));

    let log = ABI_TEST_LOG.with(|log| log.borrow().clone());
    assert!(
        log.is_empty(),
        "失败路径不得投递 OP_DISPOSE / OP_CLEANUP：{log:?}"
    );

    let handle = ABI_TEST_HANDLE.with(|cell| cell.get());
    assert_ne!(handle, u64::MAX, "插件应被调用过（拿到过句柄）");
    assert!(
        instances().read().unwrap().get(handle).is_none(),
        "apply 失败后句柄必须已回收（由 dispose effect 单点负责）"
    );
}

// ------------------------------------------------------ M2-1：ctx_require

/// 假 ABI 插件入口（M2-1）：
/// - OP_APPLY：记录 handle，并经 `host_ctx_effect` 注册一个 cleanup；
/// - OP_CLEANUP（卸载期间触发）：用同一个 handle 分别走 `ctx_get` 与 `ctx_require`
///   读 `abi-require-svc`，两者都记进日志（测试 2 用它看拆除期的差异）；
/// - OP_DISPOSE：记录顺序。
///
/// 与 M1-1 的 `abi_fake_call` 同规矩：fake `CallFn` 内禁止 assert，只落日志。
unsafe extern "C" fn abi_require_probe_call(
    handle: u64,
    op: u32,
    _a: Blob,
    _b: Blob,
    out: *mut Out,
) -> i32 {
    match op {
        OP_APPLY => {
            ABI_TEST_HANDLE.with(|cell| cell.set(handle));
            let mut buf = [0u8; 64];
            let mut cleanup_out = Out {
                ptr: buf.as_mut_ptr(),
                cap: buf.len(),
                len: 0,
            };
            let status = host_ctx_effect(handle, 7, &mut cleanup_out);
            if status != STATUS_OK {
                return status;
            }
            (*out).write(b"")
        }
        cordis_abi::OP_CLEANUP => {
            let tolerant = probe_read(handle, b"abi-require-svc", host_ctx_get);
            let strict = probe_read(handle, b"abi-require-svc", host_ctx_require);
            ABI_TEST_LOG.with(|log| {
                log.borrow_mut().push(format!("cleanup get={tolerant}"));
                log.borrow_mut().push(format!("cleanup require={strict}"));
            });
            (*out).write(b"")
        }
        OP_DISPOSE => {
            ABI_TEST_LOG.with(|log| log.borrow_mut().push("dispose".to_string()));
            (*out).write(b"")
        }
        _ => cordis_abi::STATUS_UNSUPPORTED,
    }
}

/// 用给定的宿主读函数读一个服务，返回 `status:text` 的可读快照。
unsafe fn probe_read(
    handle: u64,
    name: &[u8],
    read: unsafe extern "C" fn(u64, Blob, *mut Out) -> i32,
) -> String {
    let mut buf = [0u8; 1024];
    let mut read_out = Out {
        ptr: buf.as_mut_ptr(),
        cap: buf.len(),
        len: 0,
    };
    let status = read(handle, Blob::new(name), &mut read_out);
    let text = std::str::from_utf8(std::slice::from_raw_parts(
        read_out.ptr,
        read_out.len.min(read_out.cap),
    ))
    .unwrap_or("<non-utf8>");
    format!("{status}:{text}")
}

type ProvideFn = Arc<dyn Fn(&Ctx) -> Result<()> + Send + Sync>;

/// 造一对"外部提供者 + ABI 消费者"并收敛到 ACTIVE。
async fn mount_require_pair(
    inject: cordis_core::Inject,
    provide: ProvideFn,
    call: CallFn,
) -> (
    cordis_core::App,
    Ctx,
    Arc<cordis_core::Fiber>,
    Arc<cordis_core::Fiber>,
) {
    let app = cordis_core::App::new();
    let outer = provide.clone();
    let provider = cordis_core::plugin_fn(
        "abi-require-provider",
        cordis_core::Inject::default(),
        move |ctx: Ctx, _cfg| {
            let provide = outer.clone();
            Box::pin(async move { provide(&ctx) })
        },
    );
    let provider_fiber = app.root.plugin(provider, Config::Null);
    provider_fiber.settle().await;
    assert_eq!(
        provider_fiber.state(),
        cordis_core::FiberState::Active,
        "提供者应就绪"
    );

    let plugin: Arc<dyn Plugin> = Arc::new(cordis_core::FnPlugin {
        name: "abi-require-consumer",
        key: cordis_core::unique_key("abi-require-consumer"),
        inject,
        validator: None,
        f: Arc::new(move |ctx: Ctx, config: Config| {
            apply_via(call, ctx, config, "abi-require-consumer")
        }),
    });
    let consumer = app.root.plugin(plugin, Config::Null);
    consumer.settle().await;
    assert_eq!(
        consumer.state(),
        cordis_core::FiberState::Active,
        "消费者应装载成功"
    );
    let root = app.root.clone();
    (app, root, provider_fiber, consumer)
}

/// M2-1（红→绿）：`ctx_require` 必须**要求声明 inject**。
///
/// 现状（探针实测）：ABI 插件未声明任何 inject 也能读到服务
/// （`status=0 text={"value":{"n":1}}`）——`ctx_get` 是宽容探测，
/// 把"没声明依赖"和"服务不存在"混成了同一个 `missing`。
#[tokio::test]
async fn abi_require_enforces_inject_check() {
    ABI_TEST_LOG.with(|log| log.borrow_mut().clear());
    ABI_TEST_HANDLE.with(|cell| cell.set(u64::MAX));

    let (_app, _root, _provider, consumer) = mount_require_pair(
        cordis_core::Inject::default(),
        Arc::new(|ctx: &Ctx| {
            ctx.provide("abi-require-svc", Dyn::new(ExternValue(json!({ "n": 1 }))))?;
            Ok(())
        }),
        abi_require_probe_call,
    )
    .await;
    let handle = ABI_TEST_HANDLE.with(|cell| cell.get());

    // 宽容读的行为不变：可选探测仍然可用。
    let tolerant = unsafe { probe_read(handle, b"abi-require-svc", host_ctx_get) };
    assert!(
        tolerant.starts_with("0:"),
        "ctx_get 必须保持宽容探测语义：{tolerant}"
    );
    assert!(tolerant.contains(r#""n":1"#), "宽容读应拿到值：{tolerant}");

    // 必需读：未声明 inject ⇒ 明确报错，而不是伪装成 missing。
    let strict = unsafe { probe_read(handle, b"abi-require-svc", host_ctx_require) };
    assert!(
        strict.contains(r#"cannot get property "abi-require-svc" without inject"#),
        "ctx_require 必须要求 inject 声明：{strict}"
    );

    consumer.dispose().await;
}

/// M2-1（红→绿）：`ctx_require` 读 **committed 快照**——组件在提供者拆除期间
/// 仍能读到正在离开的依赖；同一个窗口里 `ctx_get` 只能给出 `missing`。
///
/// 现状（探针实测）：提供者拆除 → 消费者卸载期间
/// `cleanup ctx_get: status=0 text={"missing":true}`——**拆除期读依赖在 ABI 侧不可达**，
/// 而"卸载时回调依赖做收尾"恰恰是最需要它的时刻。
#[tokio::test]
async fn abi_require_reads_committed_during_departing_teardown() {
    ABI_TEST_LOG.with(|log| log.borrow_mut().clear());
    ABI_TEST_HANDLE.with(|cell| cell.set(u64::MAX));

    let (_app, _root, provider, consumer) = mount_require_pair(
        cordis_core::Inject::of(&["abi-require-svc"]),
        Arc::new(|ctx: &Ctx| {
            ctx.provide("abi-require-svc", Dyn::new(ExternValue(json!({ "n": 1 }))))?;
            Ok(())
        }),
        abi_require_probe_call,
    )
    .await;

    // 拆掉**提供者**：消费者会被级联卸载，其 cleanup 在卸载期间跑。
    provider.dispose().await;
    assert_ne!(
        consumer.state(),
        cordis_core::FiberState::Active,
        "依赖离开后消费者不应保持 Active"
    );

    let log = ABI_TEST_LOG.with(|log| log.borrow().clone());
    let get = log
        .iter()
        .find(|line| line.starts_with("cleanup get="))
        .expect("cleanup 应已执行")
        .clone();
    let require = log
        .iter()
        .find(|line| line.starts_with("cleanup require="))
        .expect("cleanup 应已执行")
        .clone();
    assert!(
        get.contains(r#""missing":true"#),
        "宽容读在提供者 UNLOADING 后只能 missing：{log:?}"
    );
    assert!(
        require.starts_with("cleanup require=0:") && require.contains(r#""n":1"#),
        "必需读必须从 committed 快照读到正在离开的依赖：{log:?}"
    );
    assert!(
        log.contains(&"dispose".to_string()),
        "OP_DISPOSE 仍须最后投递：{log:?}"
    );
}

/// M2-1：`ctx_require` 与 `ctx_get` **读到同一个载荷、看到同一条可见性契约**。
///
/// 读取语义可以不同（宽容 vs 严格），但"什么对 ABI 可见"必须只有一个答案；
/// 否则同一个服务会随入口时而可读时而不可读。
#[tokio::test]
async fn abi_require_shares_payload_and_visibility_with_get() {
    ABI_TEST_LOG.with(|log| log.borrow_mut().clear());
    ABI_TEST_HANDLE.with(|cell| cell.set(u64::MAX));

    let (_app, _root, _provider, consumer) = mount_require_pair(
        cordis_core::Inject::of(&["abi-require-svc", "abi-require-opaque"]),
        Arc::new(|ctx: &Ctx| {
            ctx.provide("abi-require-svc", Dyn::new(ExternValue(json!({ "n": 1 }))))?;
            // 对 ABI 不可见的服务：既不是 ExternValue 也不是带方法的接口。
            ctx.provide("abi-require-opaque", Dyn::new(Config::Null))?;
            Ok(())
        }),
        abi_require_probe_call,
    )
    .await;
    let handle = ABI_TEST_HANDLE.with(|cell| cell.get());

    let get = unsafe { probe_read(handle, b"abi-require-svc", host_ctx_get) };
    let require = unsafe { probe_read(handle, b"abi-require-svc", host_ctx_require) };
    assert!(
        get.starts_with("0:") && require.starts_with("0:"),
        "两者都应成功：{get} / {require}"
    );
    assert_eq!(
        get.split_once(':').unwrap().1,
        require.split_once(':').unwrap().1,
        "载荷必须完全一致"
    );

    let get_opaque = unsafe { probe_read(handle, b"abi-require-opaque", host_ctx_get) };
    let require_opaque = unsafe { probe_read(handle, b"abi-require-opaque", host_ctx_require) };
    assert!(
        get_opaque.contains("对 ABI 不可见"),
        "宽容读也受可见性契约约束：{get_opaque}"
    );
    assert_eq!(
        get_opaque.split_once(':').unwrap().1,
        require_opaque.split_once(':').unwrap().1,
        "可见性错误必须是同一条：{get_opaque} / {require_opaque}"
    );

    consumer.dispose().await;
}

// ------------------------------------------------------ M2-2：ctx_set

/// 假 ABI 插件入口（M2-2）：OP_APPLY 里按脚本走一遍 `ctx.set`，结果全部落日志。
///
/// 脚本：provide `abi-set-svc` → set 它自己 → set 一个**别人提供的**服务
/// (`abi-set-foreign`) → set 一个**从未 provide** 的名字 (`abi-set-missing`)。
unsafe extern "C" fn abi_set_probe_call(
    handle: u64,
    op: u32,
    _a: Blob,
    _b: Blob,
    out: *mut Out,
) -> i32 {
    match op {
        OP_APPLY => {
            ABI_TEST_HANDLE.with(|cell| cell.set(handle));
            let mut provide_buf = [0u8; 64];
            let mut provide_out = Out {
                ptr: provide_buf.as_mut_ptr(),
                cap: provide_buf.len(),
                len: 0,
            };
            let provided = host_ctx_provide(
                handle,
                Blob::new(b"abi-set-svc"),
                Blob::new(b"{\"v\":1}"),
                &mut provide_out,
            );
            ABI_TEST_LOG.with(|log| log.borrow_mut().push(format!("provide:{provided}")));

            ABI_TEST_LOG.with(|log| {
                log.borrow_mut().push(format!(
                    "set-own:{}",
                    probe_set(handle, b"abi-set-svc", b"{\"v\":2}")
                ));
                log.borrow_mut().push(format!(
                    "set-foreign:{}",
                    probe_set(handle, b"abi-set-foreign", b"{\"x\":1}")
                ));
                log.borrow_mut().push(format!(
                    "set-missing:{}",
                    probe_set(handle, b"abi-set-missing", b"{\"x\":1}")
                ));
            });
            (*out).write(b"")
        }
        OP_DISPOSE => {
            ABI_TEST_LOG.with(|log| log.borrow_mut().push("dispose".to_string()));
            (*out).write(b"")
        }
        _ => cordis_abi::STATUS_UNSUPPORTED,
    }
}

/// `ctx.set` 的调用快照：`status:text`。
unsafe fn probe_set(handle: u64, name: &[u8], value: &[u8]) -> String {
    let mut buf = [0u8; 512];
    let mut set_out = Out {
        ptr: buf.as_mut_ptr(),
        cap: buf.len(),
        len: 0,
    };
    let status = host_ctx_set(handle, Blob::new(name), Blob::new(value), &mut set_out);
    let text = std::str::from_utf8(std::slice::from_raw_parts(
        set_out.ptr,
        set_out.len.min(set_out.cap),
    ))
    .unwrap_or("<non-utf8>");
    format!("{status}:{text}")
}

/// M2-2（红→绿）：`ctx_set` 覆盖**自己提供的**服务值——后续任何读者
/// （含 ABI 侧）看到的都是新值。
///
/// 对齐上游 `reflect.ts:164-175`：`set` 换的是**值**，不改变"谁提供"，
/// 因此**不通知依赖者**重载（上游同样如此——epoch 只关心实现者身份）。
#[tokio::test]
async fn abi_set_overwrites_own_service() {
    ABI_TEST_LOG.with(|log| log.borrow_mut().clear());
    ABI_TEST_HANDLE.with(|cell| cell.set(u64::MAX));

    let (_app, _root, _provider, consumer) = mount_require_pair(
        cordis_core::Inject::default(),
        Arc::new(|_ctx: &Ctx| Ok(())),
        abi_set_probe_call,
    )
    .await;
    let handle = ABI_TEST_HANDLE.with(|cell| cell.get());

    let log = ABI_TEST_LOG.with(|log| log.borrow().clone());
    assert!(
        log.contains(&"provide:0".to_string()),
        "provide 应成功：{log:?}"
    );
    assert!(
        log.contains(&"set-own:0:".to_string()),
        "覆盖自己提供的服务应成功：{log:?}"
    );

    // 覆盖后的值必须对 ABI 读者可见（且只有一份值：没有产生第二条实现）。
    let read = unsafe { probe_read(handle, b"abi-set-svc", host_ctx_get) };
    assert_eq!(
        read, r#"0:{"value":{"v":2}}"#,
        "set 之后的读取必须是新值：{read}"
    );

    consumer.dispose().await;
}

/// M2-2（红→绿）：`ctx_set` 的两条硬约束与内核一致——
/// 没 provide 过 → "without provide"；不是自己提供的 → "in multiple fibers"。
#[tokio::test]
async fn abi_set_rejects_foreign_and_unprovisioned_writes() {
    ABI_TEST_LOG.with(|log| log.borrow_mut().clear());
    ABI_TEST_HANDLE.with(|cell| cell.set(u64::MAX));

    let (_app, _root, _provider, consumer) = mount_require_pair(
        cordis_core::Inject::default(),
        Arc::new(|ctx: &Ctx| {
            // 别人提供的服务：ABI 插件 set 它必须被拒。
            ctx.provide("abi-set-foreign", Dyn::new(ExternValue(json!({ "x": 0 }))))?;
            Ok(())
        }),
        abi_set_probe_call,
    )
    .await;

    let log = ABI_TEST_LOG.with(|log| log.borrow().clone());
    let foreign = log
        .iter()
        .find(|line| line.starts_with("set-foreign:"))
        .expect("脚本应走到这里")
        .clone();
    assert!(
        foreign.contains(r#"cannot set property "abi-set-foreign" in multiple fibers"#),
        "不得覆盖别人提供的服务：{log:?}"
    );
    let missing = log
        .iter()
        .find(|line| line.starts_with("set-missing:"))
        .expect("脚本应走到这里")
        .clone();
    assert!(
        missing.contains(r#"cannot set property "abi-set-missing" without provide"#),
        "不得凭空造出服务：{log:?}"
    );

    consumer.dispose().await;
}

#[test]
fn structured_plugin_errors_are_decoded() {
    let text = cordis_abi::ErrorPayload::new("boom")
        .with_code("E_BOOM")
        .with_category("business")
        .to_json();
    let payload = decode_plugin_error(cordis_abi::ERROR_FORMAT_V1, &text);
    assert_eq!(payload.error, "boom");
    assert_eq!(payload.code, "E_BOOM");
    assert_eq!(payload.category, "business");
}

#[test]
fn plugin_text_errors_survive_without_capability() {
    let payload = decode_plugin_error(0, r#"{"error":"boom"}"#);
    assert_eq!(
        payload.error, r#"{"error":"boom"}"#,
        "没有能力位时不得私自解析结构"
    );
}

#[test]
fn invalid_structured_plugin_errors_fall_back_to_text() {
    let payload = decode_plugin_error(cordis_abi::ERROR_FORMAT_V1, "plain error");
    assert_eq!(payload.error, "plain error");
}

#[test]
fn host_failures_are_structured_only_for_capable_plugins() {
    let mut buffer = vec![0u8; 4096];
    let mut out = Out {
        ptr: buffer.as_mut_ptr(),
        cap: buffer.len(),
        len: 0,
    };
    let status = unsafe {
        fail_to(
            &mut out,
            cordis_abi::ERROR_FORMAT_V1,
            "boom",
            "E_HOST",
            "lifecycle",
        )
    };
    assert_eq!(status, STATUS_ERR);
    buffer.truncate(out.len);
    let payload = cordis_abi::ErrorPayload::from_json(&String::from_utf8(buffer).unwrap()).unwrap();
    assert_eq!(payload.error, "boom");
    assert_eq!(payload.code, "E_HOST");

    let mut buffer = vec![0u8; 4096];
    let mut out = Out {
        ptr: buffer.as_mut_ptr(),
        cap: buffer.len(),
        len: 0,
    };
    let status = unsafe { fail_to(&mut out, 0, "boom", "E_HOST", "lifecycle") };
    assert_eq!(status, STATUS_ERR);
    buffer.truncate(out.len);
    assert_eq!(String::from_utf8(buffer).unwrap(), "boom");

    // 结构化放不下 ⇒ 纯文本；纯文本也放不下 ⇒ 截断到 cap，绝不回填 len > cap。
    let mut buffer = vec![0u8; 8];
    let mut out = Out {
        ptr: buffer.as_mut_ptr(),
        cap: buffer.len(),
        len: 0,
    };
    let status = unsafe {
        fail_to(
            &mut out,
            cordis_abi::ERROR_FORMAT_V1,
            "abcdefghij",
            "E_HOST",
            "lifecycle",
        )
    };
    assert_eq!(status, STATUS_ERR);
    assert!(out.len <= 8, "out.len 不得超过 cap：{}", out.len);
    buffer.truncate(out.len);
    assert_eq!(String::from_utf8(buffer).unwrap(), "abcde…");
}

#[test]
fn chinese_fail_message_truncates_on_a_char_boundary() {
    let mut buffer = vec![0u8; 8];
    let mut out = Out {
        ptr: buffer.as_mut_ptr(),
        cap: buffer.len(),
        len: 0,
    };
    let status = unsafe {
        fail_to(
            &mut out,
            cordis_abi::ERROR_FORMAT_V1,
            "中文错误",
            "E_HOST",
            "business",
        )
    };
    assert_eq!(status, STATUS_ERR);
    assert!(out.len <= 8, "out.len 不得超过 cap：{}", out.len);
    buffer.truncate(out.len);
    let text = String::from_utf8(buffer).expect("截断后必须仍是合法 UTF-8");
    assert!(text.ends_with('…'), "必须有截断标记：{text}");
}

unsafe extern "C" fn structured_call(_: u64, _: u32, _: Blob, _: Blob, out: *mut Out) -> i32 {
    let payload = cordis_abi::ErrorPayload::new("boom")
        .with_code("E_PLUGIN")
        .to_json();
    (*out).write(payload.as_bytes());
    STATUS_ERR
}

#[test]
fn call_op_decodes_plugin_errors_only_with_capability() {
    let outcome = unsafe {
        call_op(
            structured_call,
            0,
            OP_CALLBACK,
            "",
            cordis_abi::ERROR_FORMAT_V1,
        )
    }
    .unwrap();
    match outcome {
        CallOutcome::Err(payload) => {
            assert_eq!(payload.error, "boom");
            assert_eq!(payload.code, "E_PLUGIN");
        }
        _ => panic!("必须是插件错误"),
    }

    let outcome = unsafe { call_op(structured_call, 0, OP_CALLBACK, "", 0) }.unwrap();
    match outcome {
        CallOutcome::Err(payload) => {
            assert!(
                payload.error.contains("\"error\":\"boom\""),
                "没有能力位时必须保留原始 JSON 文本：{}",
                payload.error
            );
        }
        _ => panic!("必须是插件错误"),
    }
}

static CALL_CODE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

unsafe extern "C" fn coded_call(_: u64, _: u32, _: Blob, _: Blob, out: *mut Out) -> i32 {
    use std::sync::atomic::Ordering;
    let code = if CALL_CODE.fetch_add(1, Ordering::SeqCst) == 0 {
        "E_ONE"
    } else {
        "E_TWO"
    };
    let payload = cordis_abi::ErrorPayload::new("same message")
        .with_code(code)
        .to_json();
    (*out).write(payload.as_bytes());
    STATUS_ERR
}

#[test]
fn call_plugin_detailed_distinguishes_codes_with_the_same_message() {
    use std::sync::atomic::Ordering;
    CALL_CODE.store(0, Ordering::SeqCst);
    let first = unsafe { call_plugin_detailed(coded_call, 0, OP_CALLBACK, "", 1) }.unwrap_err();
    let second = unsafe { call_plugin_detailed(coded_call, 0, OP_CALLBACK, "", 1) }.unwrap_err();
    assert_eq!(first.error, second.error, "消息必须保持相同");
    assert_ne!(first.code, second.code, "调用者必须能靠 code 区分");
    assert_eq!(
        unsafe { call_plugin(coded_call, 0, OP_CALLBACK, "", 1) }.unwrap_err(),
        "same message"
    );
}

unsafe extern "C" fn over_cap_call(_: u64, _: u32, _: Blob, _: Blob, out: *mut Out) -> i32 {
    (*out).len = 20_000;
    STATUS_ERR
}

#[test]
fn oversized_plugin_output_is_bounded_and_readable() {
    let message = unsafe { call_plugin(over_cap_call, 0, OP_CALLBACK, "", 0) }.unwrap_err();
    assert!(
        message.contains("超过宿主缓冲"),
        "必须给出有界诊断：{message}"
    );
    assert!(!message.contains('\0'), "不得把零缓冲当成消息");
}

#[test]
fn twenty_k_plugin_message_is_truncated_to_the_incoming_cap() {
    let message = "x".repeat(20_000);
    let mut buffer = vec![0u8; 16 * 1024];
    let mut out = Out {
        ptr: buffer.as_mut_ptr(),
        cap: buffer.len(),
        len: 0,
    };
    let status = unsafe {
        fail_to(
            &mut out,
            cordis_abi::ERROR_FORMAT_V1,
            &message,
            "E_HOST",
            "business",
        )
    };
    assert_eq!(status, STATUS_ERR);
    assert!(out.len <= 16 * 1024, "out.len 不得超过 cap：{}", out.len);
    buffer.truncate(out.len);
    let text = String::from_utf8(buffer).unwrap();
    assert!(text.ends_with('…'), "必须带截断标记");
    assert!(!text.contains('\0'), "不得出现 NUL 串");
}

#[test]
fn tiny_host_buffer_is_bounded_and_not_a_nul_string() {
    let mut buffer = vec![0u8; 2];
    let mut out = Out {
        ptr: buffer.as_mut_ptr(),
        cap: buffer.len(),
        len: 0,
    };
    let status = unsafe {
        fail_to(
            &mut out,
            cordis_abi::ERROR_FORMAT_V1,
            "中文",
            "E_HOST",
            "business",
        )
    };
    assert_eq!(status, STATUS_ERR);
    assert!(out.len <= 2, "out.len 不得超过 cap：{}", out.len);
    buffer.truncate(out.len);
    assert_eq!(String::from_utf8(buffer).unwrap(), "");
}

#[test]
fn call_scope_nests_and_pops() {
    assert_eq!(call_scope_top(), None);
    let outer = call_scope_push(7);
    assert_eq!(call_scope_top(), Some(7));
    {
        let _inner = call_scope_push(9);
        assert_eq!(call_scope_top(), Some(9));
    }
    assert_eq!(call_scope_top(), Some(7));
    drop(outer);
    assert_eq!(call_scope_top(), None);
}

static PROBE_SCOPE: AtomicU64 = AtomicU64::new(u64::MAX);
static PROBE_CORRELATION: Mutex<Option<String>> = Mutex::new(None);
static PROBE_LOCK: Mutex<()> = Mutex::new(());

struct RecordCollector(Arc<Mutex<Vec<cordis_core::TraceRecord>>>);

impl cordis_core::TraceSink for RecordCollector {
    fn record(&self, record: cordis_core::TraceRecord) {
        self.0.lock().unwrap().push(record);
    }
}

unsafe extern "C" fn scope_probe_call(_: u64, _: u32, _: Blob, _: Blob, _: *mut Out) -> i32 {
    PROBE_SCOPE.store(call_scope_top().unwrap_or(0), Ordering::SeqCst);
    *PROBE_CORRELATION.lock().unwrap() =
        cordis_core::current_correlation_id().map(|id| id.to_string());
    STATUS_OK
}

fn probe_instance() -> (u64, Instance, Arc<Mutex<Vec<cordis_core::TraceRecord>>>) {
    let app = Box::leak(Box::new(cordis_core::App::new()));
    let records = Arc::new(Mutex::new(Vec::new()));
    assert!(app.set_trace_sink(Arc::new(RecordCollector(records.clone()))));
    let handle =
        register_instance_with_keepalive(app.root.clone(), scope_probe_call, None, 0).unwrap();
    let instance = instances().read().unwrap().get(handle).unwrap();
    (handle, instance, records)
}

#[test]
fn inbound_pushes_call_scope_only_when_traced() {
    let _guard = PROBE_LOCK.lock().unwrap();
    let (handle, instance, records) = probe_instance();
    PROBE_SCOPE.store(u64::MAX, Ordering::SeqCst);
    let result = unsafe {
        call_inbound(
            &instance,
            handle,
            OP_CALLBACK,
            json!({"event": "svc", "method": "m"}),
            None,
            None,
            None,
            None,
            true,
            None,
        )
    };
    assert!(result.is_ok());
    let seen = PROBE_SCOPE.load(Ordering::SeqCst);
    assert_ne!(seen, 0, "iface 入站必须在 CALL_SCOPE 里执行");
    assert_ne!(seen, u64::MAX, "探针必须被写");
    assert_eq!(records.lock().unwrap().len(), 1, "iface 入站写一条记录");

    PROBE_SCOPE.store(u64::MAX, Ordering::SeqCst);
    let result = unsafe {
        call_inbound(
            &instance,
            handle,
            OP_CALLBACK,
            json!({"event": "svc", "method": "m"}),
            None,
            None,
            None,
            None,
            false,
            None,
        )
    };
    assert!(result.is_ok());
    assert_eq!(
        PROBE_SCOPE.load(Ordering::SeqCst),
        0,
        "事件/流入站不得压 CALL_SCOPE"
    );
    assert_eq!(records.lock().unwrap().len(), 1, "事件/流入站不得写记录");
    unregister_instance(handle);
}

#[test]
fn continuation_is_delivered_inside_the_callers_hop() {
    let _guard = PROBE_LOCK.lock().unwrap();
    let (handle, instance, _records) = probe_instance();
    insert_dispatch(
        &instance,
        handle.wrapping_add(1000),
        DispatchRecord {
            sink: None,
            frame: None,
            parent: None,
            outbound: Some(Outbound {
                owner: handle,
                continuation: 99,
                kind: ContinuationKind::Iface,
                trace_parent: Some(42),
                trace_correlation: Some(Arc::from("sess-resume")),
            }),
            stream: None,
        },
    )
    .unwrap_or_else(|_| panic!("admission"));
    PROBE_SCOPE.store(u64::MAX, Ordering::SeqCst);
    *PROBE_CORRELATION.lock().unwrap() = None;
    deliver_outbound(handle, handle.wrapping_add(1000), "{}");
    assert_eq!(
        PROBE_SCOPE.load(Ordering::SeqCst),
        42,
        "续延交付必须恢复调用方那一跳"
    );
    assert_eq!(
        PROBE_CORRELATION.lock().unwrap().as_deref(),
        Some("sess-resume"),
        "续延交付必须恢复调用方的 correlation_id"
    );
    assert_eq!(
        cordis_core::current_correlation_id(),
        None,
        "续延交付结束后必须恢复 correlation 作用域"
    );
    unregister_instance(handle);
}

#[test]
fn inbound_record_carries_name_and_call_id() {
    let _guard = PROBE_LOCK.lock().unwrap();
    let app = Box::leak(Box::new(cordis_core::App::new()));
    let records = Arc::new(Mutex::new(Vec::new()));
    assert!(app.set_trace_sink(Arc::new(RecordCollector(records.clone()))));
    let handle =
        register_instance_with_keepalive(app.root.clone(), scope_probe_call, None, 0).unwrap();
    let instance = instances().read().unwrap().get(handle).unwrap();
    PROBE_SCOPE.store(u64::MAX, Ordering::SeqCst);
    let result = unsafe {
        call_inbound(
            &instance,
            handle,
            OP_CALLBACK,
            json!({"event": "svc", "method": "m"}),
            None,
            None,
            None,
            None,
            true,
            None,
        )
    };
    assert!(result.is_ok());
    let records = records.lock().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(&*records[0].name, "svc.m");
    assert_ne!(records[0].call_id, 0);
    assert_eq!(records[0].call_id, PROBE_SCOPE.load(Ordering::SeqCst));
    assert_eq!(records[0].outcome, cordis_core::Outcome::Ok);
    unregister_instance(handle);
}

struct NoRuntimeService;

impl ExternService for NoRuntimeService {
    fn service(&self) -> &str {
        "no-rt@1"
    }

    fn version(&self) -> u32 {
        1
    }

    fn methods(&self) -> Vec<String> {
        vec!["run".into()]
    }

    fn call(
        &self,
        _caller_meta: Option<Config>,
        _method: String,
        _args: Config,
    ) -> BoxFuture<Result<Config>> {
        Box::pin(std::future::pending())
    }
}

/// 同步上下文里首个 poll 未就绪、又没有 tokio runtime：调用方拿到
/// `host_call`，同时必须留下一条 `call_id=0` 的记录。
#[test]
fn no_runtime_is_recorded_as_host_call() {
    let _guard = PROBE_LOCK.lock().unwrap();
    let app = Box::leak(Box::new(cordis_core::App::new()));
    let records = Arc::new(Mutex::new(Vec::new()));
    assert!(app.set_trace_sink(Arc::new(RecordCollector(records.clone()))));
    app.root
        .provide_iface("no-rt@1", Arc::new(NoRuntimeService))
        .expect("provide");
    let handle =
        register_instance_with_keepalive(app.root.clone(), scope_probe_call, None, 0).unwrap();
    let mut buffer = [0u8; 512];
    let mut out = Out {
        ptr: buffer.as_mut_ptr(),
        cap: buffer.len(),
        len: 0,
    };
    let status = unsafe {
        host_ctx_call_iface(
            handle,
            Blob::new(b"no-rt@1"),
            1,
            Blob::new(b"run"),
            Blob::new(b"[]"),
            0,
            &mut out,
        )
    };
    assert_eq!(status, STATUS_ERR);
    let records = records.lock().unwrap();
    assert_eq!(records.len(), 1, "NoRuntime 必须写一条记录：{records:?}");
    let record = &records[0];
    assert_eq!(&*record.name, "no-rt@1.run");
    assert_eq!(record.call_id, 0);
    assert_eq!(record.callee_uid, 0);
    assert_eq!(&*record.callee_plugin, "no-rt@1");
    assert_eq!(
        record.outcome,
        cordis_core::Outcome::Err("host_call".into())
    );
    drop(records);
    unregister_instance(handle);
}

#[test]
fn no_runtime_with_outbound_timeout_is_still_recorded_as_host_call() {
    let _guard = PROBE_LOCK.lock().unwrap();
    let app = Box::leak(Box::new(cordis_core::App::new()));
    set_dispatch_policy_for(&app.root, DispatchPolicy::new(None, Some(50)));
    let records = Arc::new(Mutex::new(Vec::new()));
    assert!(app.set_trace_sink(Arc::new(RecordCollector(records.clone()))));
    app.root
        .provide_iface("no-rt-to@1", Arc::new(NoRuntimeService))
        .expect("provide");
    let handle = register_instance_with_keepalive(
        app.root.clone(),
        scope_probe_call,
        None,
        cordis_abi::ERROR_FORMAT_V1,
    )
    .unwrap();
    let mut buffer = [0u8; 512];
    let mut out = Out {
        ptr: buffer.as_mut_ptr(),
        cap: buffer.len(),
        len: 0,
    };
    let status = unsafe {
        host_ctx_call_iface(
            handle,
            Blob::new(b"no-rt-to@1"),
            1,
            Blob::new(b"run"),
            Blob::new(b"[]"),
            0,
            &mut out,
        )
    };
    assert_eq!(status, STATUS_ERR);
    let text = String::from_utf8_lossy(&buffer[..out.len.min(buffer.len())]);
    let payload = ErrorPayload::from_json(&text).expect("结构化错误载荷");
    assert_eq!(
        payload.code, "host_call",
        "配置出站超时后在无运行时线程调用仍须归类为 host_call，不得误报 host_panic：{payload:?}"
    );
    let records = records.lock().unwrap();
    assert_eq!(records.len(), 1, "NoRuntime 必须写一条记录：{records:?}");
    assert_eq!(
        records[0].outcome,
        cordis_core::Outcome::Err("host_call".into())
    );
    drop(records);
    unregister_instance(handle);
}

// #10 [P1]: `drain_timeout_ms` 触发时若 `dispatches` 中存在带 `HostStreamObserver` 的流，
// 不得在持有 `instance.dispatches` 互斥锁期间调用 `sink.fail_error`（同线程重入死锁）。
#[tokio::test]
async fn drain_timeout_with_active_stream_does_not_deadlock_dispatches_mutex() {
    let app = cordis_core::App::new();
    app.store
        .set_lifecycle_policy(cordis_core::LifecyclePolicy {
            drain_timeout_ms: Some(40),
            ..Default::default()
        })
        .unwrap();

    let plugin: Arc<dyn cordis_core::Plugin> = Arc::new(cordis_core::FnPlugin {
        name: "drain-stream-owner",
        key: cordis_core::unique_key("drain-stream-owner"),
        inject: cordis_core::Inject::default(),
        validator: None,
        f: Arc::new(|ctx, config| apply_via(task_test_call, ctx, config, "drain-stream-owner")),
    });
    let fiber = app.root.plugin(plugin, Value::Null);
    fiber.settle().await;
    let handle = TASK_TEST_HANDLE.get();
    let instance = instances().read().unwrap().get(handle).unwrap();

    let (sink, mut stream) = StreamSink::new(1);
    sink.observe(Arc::new(HostStreamObserver {
        handle,
        dispatch_id: 777,
    }));
    insert_dispatch(
        &instance,
        777,
        DispatchRecord {
            sink: None,
            frame: None,
            parent: None,
            outbound: None,
            stream: Some(sink),
        },
    )
    .unwrap_or_else(|_| panic!("admission"));

    // 占住一枚在飞调用租约，强制卸载排水超时进入 drain_timed_out 分支。
    let active = admit_call(&instance).expect("admit_call");
    tokio::time::timeout(std::time::Duration::from_secs(2), fiber.dispose())
        .await
        .expect("drain_timed_out 持 dispatches 锁调 sink.fail_error 导致同线程死锁");

    let err = tokio::time::timeout(std::time::Duration::from_secs(1), stream.next())
        .await
        .expect("stream.next 不得挂起")
        .expect_err("流必须以卸载错误收尾");
    assert!(
        err.to_string().contains("插件已卸载"),
        "预期卸载收尾错误，实际：{err}"
    );

    // 释放调用租约后，后台移交的收尾任务必须完成并注销实例。
    drop(active);
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while instances().read().unwrap().get(handle).is_some() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("后台移交任务必须在调用退出后注销实例");

    app.close().await;
}

// #13 [P2]: FFI JSON 载荷与描述表入口必须拒绝 `len > isize::MAX` 与 `{ null, non-zero }` Blob，
// 返回 STATUS_ERR（或安全跳过）而不是触发 `Blob::as_bytes()` UB/panic。
#[test]
fn ffi_json_entries_reject_invalid_blobs_without_ub() {
    let bad_blobs = [
        Blob {
            ptr: b"{\"a\":1}".as_ptr(),
            len: (isize::MAX as usize) + 1,
        },
        Blob {
            ptr: std::ptr::null(),
            len: 1,
        },
    ];
    let mut buf = [0u8; 256];
    let mut out = Out {
        ptr: buf.as_mut_ptr(),
        cap: buf.len(),
        len: 0,
    };

    for bad in bad_blobs {
        assert_eq!(
            unsafe { host_ctx_set(0, Blob::new(b"svc"), bad, &mut out) },
            STATUS_ERR
        );
        assert_eq!(
            unsafe { host_ctx_provide(0, Blob::new(b"svc"), bad, &mut out) },
            STATUS_ERR
        );
        assert_eq!(unsafe { host_ctx_fulfill(0, 1, bad, &mut out) }, STATUS_ERR);
        assert_eq!(unsafe { host_ctx_next(0, 1, bad, 1, &mut out) }, STATUS_ERR);
        assert_eq!(
            unsafe { host_ctx_provide_iface(0, Blob::new(b"s@1"), 1, bad, 1, &mut out) },
            STATUS_ERR
        );
        assert_eq!(
            unsafe { host_stream_push(0, 1, bad, 1, &mut out) },
            STATUS_ERR
        );
        assert_eq!(unsafe { host_stream_fail(0, 1, bad, &mut out) }, STATUS_ERR);
        assert_eq!(
            unsafe { host_stream_fail_payload(0, 1, bad, &mut out) },
            STATUS_ERR
        );

        let desc = PluginDesc {
            abi_version: ABI_VERSION,
            size: std::mem::size_of::<PluginDesc>() as u32,
            id: bad,
            inject: bad,
            schema: bad,
            attach: fake_attach,
            call: fake_call,
            provision: bad,
            error_format: 0,
            tail_reserved: 0,
        };
        let caps = unsafe { Capabilities::of_desc(&desc) };
        assert!(caps.plugin.is_empty());
        assert!(caps.inject.is_empty());
        assert!(caps.provision.is_empty());
    }
}

#[test]
fn r3_p2_patch_apostrophe_in_plain_scalar_preserves_subsequent_js_tags() {
    let yaml = r#"
- name: alpha
  config:
    note: don't break
- name: beta
  disabled: !!js false
  config:
    token: !!js "process.env.API_KEY"
"#;
    let entries = crate::patch::parse_entries(yaml).expect("valid yaml with apostrophe");
    assert_eq!(entries.len(), 2);
    assert_eq!(
        entries[1].disabled_expr.as_ref().map(|e| &e.body),
        Some(&serde_json::json!(false)),
        "无引号标量里的撇号不得吃掉后续行的 disabled: !!js 标签"
    );
    let caps = crate::patch::entry_js_capabilities(&entries[1]);
    assert_eq!(
        caps.env,
        vec!["API_KEY".to_string()],
        "无引号标量里的撇号不得吃掉后续行的 config !!js 标签与能力审计"
    );
}

#[test]
fn r3_p2_capabilities_of_desc_null_and_zero_fn_are_safe() {
    let null_caps = unsafe { Capabilities::of_desc(std::ptr::null()) };
    assert!(null_caps.plugin.is_empty());

    // Simulate a corrupt PluginDesc with zeroed `attach` and `call` function pointers.
    #[repr(C)]
    struct RawCorruptDesc {
        abi_version: u32,
        size: u32,
        id: Blob,
        inject: Blob,
        schema: Blob,
        attach: *const (),
        call: *const (),
        provision: Blob,
        error_format: u32,
        tail_reserved: u32,
    }
    let raw = RawCorruptDesc {
        abi_version: ABI_VERSION,
        size: std::mem::size_of::<PluginDesc>() as u32,
        id: Blob::new(b"corrupt"),
        inject: Blob::EMPTY,
        schema: Blob::EMPTY,
        attach: std::ptr::null(),
        call: std::ptr::null(),
        provision: Blob::EMPTY,
        error_format: 0,
        tail_reserved: 0,
    };
    let caps = unsafe { Capabilities::of_desc(&raw as *const RawCorruptDesc as *const PluginDesc) };
    assert!(
        caps.plugin.is_empty(),
        "attach/call 为空指针的畸形 PluginDesc 必须安全降级而非解引用空函数指针 UB"
    );
}

#[test]
fn r3_p2_probe_capabilities_freezes_fallback_admission() {
    struct CapturingSource {
        seen_mode: std::sync::Mutex<Option<String>>,
    }
    impl crate::loader::PluginSource for CapturingSource {
        fn load(
            &self,
            _entry: &crate::patch::Entry,
            _search: &[PathBuf],
            _revision: u64,
        ) -> std::result::Result<crate::loader::LoadedFile, String> {
            unreachable!("probe_capabilities_with must call load_admitted")
        }
        fn load_admitted(
            &self,
            _entry: &crate::patch::Entry,
            _search: &[PathBuf],
            _revision: u64,
            admission: &crate::source::SourceAdmission,
        ) -> std::result::Result<crate::loader::LoadedFile, String> {
            *self.seen_mode.lock().unwrap() = Some(admission.mode().to_string());
            Err("stop-after-capture".to_string())
        }
    }

    // Install dev-unverified temporarily so Fallback resolves cleanly without touching disk.
    let prev_allow = crate::source::allow_unverified();
    crate::source::set_allow_unverified(true);
    let src = Arc::new(CapturingSource {
        seen_mode: std::sync::Mutex::new(None),
    });
    let _ = crate::probe_capabilities_with(src.clone(), Path::new("fake.dll"), &[]);
    crate::source::set_allow_unverified(prev_allow);

    assert_eq!(
        src.seen_mode.lock().unwrap().as_deref(),
        Some("dev-unverified"),
        "probe_capabilities_with 必须冻结 Fallback 准入结论，不得把未冻结的 Fallback 传给 load_admitted"
    );
}

#[test]
fn r3_p3_js_capabilities_detects_optional_chaining_bracket_env() {
    let caps1 = crate::js::capabilities_of("process.env?.['SECRET']");
    assert_eq!(caps1.env, vec!["SECRET".to_string()]);
    let caps2 = crate::js::capabilities_of("env?.['TOKEN']");
    assert_eq!(caps2.env, vec!["TOKEN".to_string()]);
}

#[test]
fn r3_p3_resolve_plugin_path_preserves_parent_for_lib_and_blocks_windows_root_escape() {
    let cands = crate::staging::plugin_path_candidates_for_test("sub/demo");
    #[cfg(not(windows))]
    {
        let has_nested_lib = cands
            .iter()
            .any(|p| p == Path::new("sub/libdemo.so") || p == Path::new("sub/libdemo.dylib"));
        assert!(
            has_nested_lib,
            "Unix/macOS lib 前缀必须加在文件名上而非整条路径前: {cands:?}"
        );
    }
    let _ = cands;

    #[cfg(windows)]
    {
        let err = crate::staging::resolve_plugin_path(
            "\\definitely-not-in-search",
            &[PathBuf::from("C:\\nonexistent")],
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("不允许") || err.contains("路径"),
            "Windows 无盘符根路径不得逃逸 search 目录: {err}"
        );
        let err2 = crate::staging::resolve_plugin_path(
            "D:relative-escape",
            &[PathBuf::from("C:\\nonexistent")],
        )
        .unwrap_err()
        .to_string();
        assert!(
            err2.contains("不允许") || err2.contains("路径"),
            "Windows 驱动器相对路径不得逃逸 search 目录: {err2}"
        );
    }
}

#[tokio::test]
async fn r4_p0_ctx_use_child_handle_cannot_impersonate_child_identity_or_inject() {
    use cordis_core::{plugin_fn, BoxFuture, Config, Dyn, ExternService, ExternValue, Inject};

    struct VaultSvc;
    impl ExternService for VaultSvc {
        fn service(&self) -> &str {
            "vault"
        }
        fn version(&self) -> u32 {
            1
        }
        fn methods(&self) -> Vec<String> {
            vec!["read_secret".to_string()]
        }
        fn call(
            &self,
            _caller_meta: Option<Config>,
            _method: String,
            _args: Config,
        ) -> BoxFuture<cordis_core::Result<Config>> {
            Box::pin(async { Ok(json!("TOP-SECRET-VALUE")) })
        }
    }

    struct PrivChildRegistry;
    impl NativeRegistry for PrivChildRegistry {
        fn native(&self, name: &str) -> Option<Arc<dyn Plugin>> {
            if name == "r4-priv-child" {
                Some(plugin_fn(
                    "r4-priv-child",
                    Inject::of(&["vault@1"]),
                    |ctx, _| {
                        Box::pin(async move {
                            let _ = ctx.provide("r4-child-pub", Dyn::new(ExternValue(json!("ok"))));
                            Ok(())
                        })
                    },
                ))
            } else {
                None
            }
        }
    }

    let app = cordis_core::App::new();
    let _vault = app
        .root
        .provide_iface("vault@1", Arc::new(VaultSvc))
        .unwrap();
    let allow =
        crate::policy::MethodAllowList::new().allow_plugin_service("r4-priv-child", "vault@1");
    crate::policy::set_method_allow_list_for(&app.root, Some(allow));

    set_native_registry_for(&app.root, Arc::new(PrivChildRegistry));

    // Mount an unprivileged parent fiber with NO `vault@1` in `inject` and NOT in `allow`.
    let parent_fiber = app.root.plugin(
        plugin_fn("r4-unpriv-parent", Inject::default(), |_, _| {
            Box::pin(async { Ok(()) })
        }),
        Config::Null,
    );
    parent_fiber.settle().await;
    let parent_handle = register_instance_with_keepalive(
        parent_fiber.ctx(),
        stream_unused_call,
        None,
        cordis_abi::ERROR_FORMAT_V1,
    )
    .unwrap();

    // Parent mounts `@native:r4-priv-child` via `host_ctx_use`.
    let mut buf = vec![0u8; 4096];
    let mut out = Out {
        ptr: buf.as_mut_ptr(),
        cap: buf.len(),
        len: 0,
    };
    let st = unsafe {
        host_ctx_use(
            parent_handle,
            Blob::new(b"@native:r4-priv-child"),
            Blob::new(b"{}"),
            &mut out,
        )
    };
    assert_eq!(st, STATUS_OK, "ctx_use should succeed");
    let parsed: Value = serde_json::from_slice(&buf[..out.len]).unwrap();
    let child_handle = parsed["handle"].as_u64().unwrap();

    // Wait briefly for child fiber to activate.
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;

    // 1. Calling `vault@1.read_secret` through `child_handle` MUST be denied (no identity impersonation!).
    out.len = 0;
    let call_st = unsafe {
        host_ctx_call_iface(
            child_handle,
            Blob::new(b"vault@1"),
            1,
            Blob::new(b"read_secret"),
            Blob::new(b"[]"),
            0,
            &mut out,
        )
    };
    let call_text = String::from_utf8_lossy(&buf[..out.len]).to_string();
    assert_eq!(
        call_st, STATUS_ERR,
        "未授权父插件不得借用 ctx_use 子句柄冒充子插件身份调用受保护方法: {call_text}"
    );
    assert!(
        !call_text.contains("TOP-SECRET-VALUE"),
        "绝不得泄露受保护服务返回值: {call_text}"
    );

    // 2. Calling `ctx_require(child_handle, "vault@1")` MUST NOT borrow child's `inject`!
    out.len = 0;
    let req_st = unsafe { host_ctx_require(child_handle, Blob::new(b"vault@1"), &mut out) };
    let req_text = String::from_utf8_lossy(&buf[..out.len]).to_string();
    assert_eq!(
        req_st, STATUS_ERR,
        "父插件不得借用 ctx_use 子句柄绕过自身 inject 限制: {req_text}"
    );

    // 3. `ctx_get(child_handle, "r4-child-pub")` in the shared/isolated realm MUST still work.
    out.len = 0;
    let get_st = unsafe { host_ctx_get(child_handle, Blob::new(b"r4-child-pub"), &mut out) };
    assert_eq!(get_st, STATUS_OK);
    let get_val: Value = serde_json::from_slice(&buf[..out.len]).unwrap();
    assert_eq!(get_val["value"], json!("ok"));

    unregister_instance(parent_handle);
    app.close().await;
}

#[tokio::test]
async fn r4_p0_ctx_use_respects_per_app_strict_plugin_source_over_global() {
    use crate::loader::{load_with_source, LoadedFile, LoaderConfig, PluginSource};
    use cordis_core::{plugin_fn, Inject};

    struct StrictAppASource;
    impl PluginSource for StrictAppASource {
        fn load(
            &self,
            entry: &crate::patch::Entry,
            _search: &[PathBuf],
            _revision: u64,
        ) -> std::result::Result<LoadedFile, String> {
            if entry.name == "allowed-parent" {
                Ok(LoadedFile {
                    plugin: plugin_fn("allowed-parent", Inject::default(), |_, _| {
                        Box::pin(async { Ok(()) })
                    }),
                    source: PathBuf::new(),
                    staged: PathBuf::new(),
                    fingerprint: None,
                })
            } else {
                Err(format!("strict-app-a-reject: {}", entry.name))
            }
        }
    }

    struct LooseGlobalSource;
    impl PluginSource for LooseGlobalSource {
        fn load(
            &self,
            _entry: &crate::patch::Entry,
            _search: &[PathBuf],
            _revision: u64,
        ) -> std::result::Result<LoadedFile, String> {
            Ok(LoadedFile {
                plugin: plugin_fn("rogue-global-plugin", Inject::default(), |_, _| {
                    Box::pin(async { Ok(()) })
                }),
                source: PathBuf::new(),
                staged: PathBuf::new(),
                fingerprint: None,
            })
        }
    }

    let tmp_dir = std::env::temp_dir().join(format!("cordis-r4-app-src-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp_dir);
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let cfg_path = tmp_dir.join("cordis.yml");
    std::fs::write(&cfg_path, "- id: parent\n  name: allowed-parent\n").unwrap();

    // Process-global source is loose (accepts anything); restore after test to avoid cross-test pollution.
    struct RestoreGlobalSource(Option<Arc<dyn PluginSource>>);
    impl Drop for RestoreGlobalSource {
        fn drop(&mut self) {
            *PLUGIN_SOURCE.write().unwrap() = self.0.take();
        }
    }
    let prev_global = PLUGIN_SOURCE.write().unwrap().take();
    let _restore = RestoreGlobalSource(prev_global);
    set_plugin_source(Arc::new(LooseGlobalSource));

    // App A mounts its loader with StrictAppASource.
    let app_a = cordis_core::App::new();
    let state_a = load_with_source(
        app_a.root.clone(),
        LoaderConfig::new(cfg_path.to_string_lossy().into_owned()),
        Arc::new(StrictAppASource),
    )
    .await
    .unwrap();
    let parent_fiber = state_a.fiber_of("parent").unwrap();
    let handle_a = register_instance_with_keepalive(
        parent_fiber.ctx(),
        stream_unused_call,
        None,
        cordis_abi::ERROR_FORMAT_V1,
    )
    .unwrap();

    let mut buf = vec![0u8; 4096];
    let mut out = Out {
        ptr: buf.as_mut_ptr(),
        cap: buf.len(),
        len: 0,
    };
    let st = unsafe {
        host_ctx_use(
            handle_a,
            Blob::new(b"unverified-evil.dll"),
            Blob::new(b"{}"),
            &mut out,
        )
    };
    let text = String::from_utf8_lossy(&buf[..out.len]).to_string();
    assert_eq!(
        st, STATUS_ERR,
        "App A 内插件调用 ctx_use 不得绕过 per-App PluginSource 走全局宽松源: {text}"
    );
    assert!(
        text.contains("strict-app-a-reject"),
        "应命中 App A 专属来源拒绝原因，实际: {text}"
    );

    unregister_instance(handle_a);
    app_a.close().await;
    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[tokio::test]
async fn r5_p1_child_fiber_handle_rejects_isolate_intercept_and_nested_use() {
    use cordis_core::{plugin_fn, Inject};

    struct ChildReg;
    impl NativeRegistry for ChildReg {
        fn native(&self, name: &str) -> Option<Arc<dyn cordis_core::Plugin>> {
            let static_name = match name {
                "r5-child" => "r5-child",
                _ => "r5-grandchild",
            };
            Some(plugin_fn(static_name, Inject::default(), |_, _| {
                Box::pin(async { Ok(()) })
            }))
        }
    }

    let app = cordis_core::App::new();
    set_native_registry_for(&app.root, Arc::new(ChildReg));

    let parent_fiber = app.root.plugin(
        plugin_fn("r5-parent", Inject::default(), |_, _| {
            Box::pin(async { Ok(()) })
        }),
        cordis_core::Config::Null,
    );
    parent_fiber.settle().await;
    let parent_handle = register_instance_with_keepalive(
        parent_fiber.ctx(),
        stream_unused_call,
        None,
        cordis_abi::ERROR_FORMAT_V1,
    )
    .unwrap();

    let mut buf = vec![0u8; 4096];
    let mut out = Out {
        ptr: buf.as_mut_ptr(),
        cap: buf.len(),
        len: 0,
    };
    let st = unsafe {
        host_ctx_use(
            parent_handle,
            Blob::new(b"@native:r5-child"),
            Blob::new(b"{}"),
            &mut out,
        )
    };
    assert_eq!(st, STATUS_OK);
    let child_json: Value = serde_json::from_slice(&buf[..out.len]).unwrap();
    let child_handle = child_json["handle"].as_u64().unwrap();

    // 1. `ctx_isolate(child_handle, ...)` MUST be rejected so child handle cannot be laundered into a scope handle.
    out.len = 0;
    let iso_st =
        unsafe { host_ctx_isolate(child_handle, Blob::new(b"k"), Blob::new(b""), &mut out) };
    assert_eq!(
        iso_st,
        STATUS_ERR,
        "ctx_use 子句柄不得经 ctx_isolate 洗白为可调用域句柄: {}",
        String::from_utf8_lossy(&buf[..out.len])
    );

    // 2. `ctx_intercept(child_handle, ...)` MUST be rejected for the same reason.
    out.len = 0;
    let int_st =
        unsafe { host_ctx_intercept(child_handle, Blob::new(b"k"), Blob::new(b"{}"), &mut out) };
    assert_eq!(
        int_st,
        STATUS_ERR,
        "ctx_use 子句柄不得经 ctx_intercept 洗白为可调用域句柄: {}",
        String::from_utf8_lossy(&buf[..out.len])
    );

    // 3. `ctx_use(child_handle, ...)` MUST be rejected instead of silently mounting grandchild onto parent.
    out.len = 0;
    let use_st = unsafe {
        host_ctx_use(
            child_handle,
            Blob::new(b"@native:r5-grandchild"),
            Blob::new(b"{}"),
            &mut out,
        )
    };
    assert_eq!(
        use_st,
        STATUS_ERR,
        "ctx_use 子句柄不得接受嵌套 ctx_use 避免孙组件泄漏挂载到父 fiber: {}",
        String::from_utf8_lossy(&buf[..out.len])
    );

    unregister_instance(parent_handle);
    app.close().await;
}

#[tokio::test]
async fn r4_p1_register_derived_rejects_closed_instance_no_derived_table_leak() {
    let app = cordis_core::App::new();
    let handle = register_instance_with_keepalive(
        app.root.clone(),
        stream_unused_call,
        None,
        cordis_abi::ERROR_FORMAT_V1,
    )
    .unwrap();
    let inst = instances().read().unwrap().get(handle).unwrap();
    // Simulate teardown having marked the instance closed right before `unregister_instance`.
    inst.closed.store(true, Ordering::SeqCst);

    let mut buf = vec![0u8; 4096];
    let mut out = Out {
        ptr: buf.as_mut_ptr(),
        cap: buf.len(),
        len: 0,
    };
    let st_iso = unsafe { host_ctx_isolate(handle, Blob::new(b"svc"), Blob::new(b""), &mut out) };
    assert_eq!(
        st_iso, STATUS_ERR,
        "closed 实例上调用 host_ctx_isolate 必须拒绝"
    );

    out.len = 0;
    let st_int =
        unsafe { host_ctx_intercept(handle, Blob::new(b"svc"), Blob::new(b"{}"), &mut out) };
    assert_eq!(
        st_int, STATUS_ERR,
        "closed 实例上调用 host_ctx_intercept 必须拒绝"
    );

    let leaked = derived_table()
        .lock()
        .unwrap()
        .values()
        .filter(|e| e.owner == handle)
        .count();
    assert_eq!(
        leaked, 0,
        "closed 实例的派生调用不得向全局 DERIVED 表泄漏任何条目"
    );

    unregister_instance(handle);
    app.close().await;
}
