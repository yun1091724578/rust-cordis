use super::*;
use std::future::Future;
use std::sync::Barrier;

type Gate = Arc<(Barrier, Barrier)>;

#[derive(Default)]
struct Events {
    history: Mutex<Vec<(u32, Value, bool)>>,
    gate: Mutex<Option<Gate>>,
    dropped: Mutex<Option<Arc<AtomicBool>>>,
    release_owner: Mutex<Option<Arc<dyn Any + Send + Sync>>>,
    reentry_status: Mutex<Option<i32>>,
    business_dropped: Mutex<Option<Arc<AtomicBool>>>,
    dispose_after_drop: Mutex<Option<bool>>,
}
fn logs() -> &'static Mutex<HashMap<u64, Arc<Events>>> {
    static LOGS: OnceLock<Mutex<HashMap<u64, Arc<Events>>>> = OnceLock::new();
    LOGS.get_or_init(Default::default)
}

// Fake C only records results; all assertions live in the Rust test bodies.
unsafe extern "C" fn record(handle: u64, op: u32, args: Blob, _: Blob, _: *mut Out) -> i32 {
    let log = logs()
        .lock()
        .ok()
        .and_then(|logs| logs.get(&handle).cloned());
    if let Some(log) = log {
        let payload: Value = serde_json::from_slice(args.as_bytes()).unwrap_or(Value::Null);
        let alive = log
            .dropped
            .lock()
            .ok()
            .and_then(|flag| flag.clone())
            .is_none_or(|flag| !flag.load(Ordering::SeqCst));
        if let Ok(mut events) = log.history.lock() {
            events.push((op, payload.clone(), alive));
        }
        if op == OP_DISPOSE {
            if let Some(flag) = log
                .business_dropped
                .lock()
                .ok()
                .and_then(|flag| flag.clone())
            {
                if let Ok(mut observed) = log.dispose_after_drop.lock() {
                    *observed = Some(flag.load(Ordering::SeqCst));
                }
            }
        }
        if op == OP_CALLBACK {
            let gate = log.gate.lock().ok().and_then(|mut gate| gate.take());
            if let Some(gate) = gate {
                gate.0.wait();
                gate.1.wait();
            }
            let owner = log
                .release_owner
                .lock()
                .ok()
                .and_then(|mut owner| owner.take());
            drop(owner);
            if let Some(id) = payload.get("reenter").and_then(Value::as_u64) {
                let mut out = Out {
                    ptr: std::ptr::null_mut(),
                    cap: 0,
                    len: 0,
                };
                let status = host_stream_close(handle, id, &mut out);
                if let Ok(mut result) = log.reentry_status.lock() {
                    *result = Some(status);
                }
            }
        }
    }
    STATUS_OK
}

struct Fixture {
    app: Option<cordis_core::App>,
    handle: u64,
    instance: Instance,
    log: Arc<Events>,
}
impl Fixture {
    async fn new(owner: Option<Arc<dyn Any + Send + Sync>>) -> Self {
        Self::with_format(owner, 0).await
    }
    async fn with_format(owner: Option<Arc<dyn Any + Send + Sync>>, format: u32) -> Self {
        let app = cordis_core::App::new();
        apply_via_with_keepalive_and_format(
            record,
            app.root.clone(),
            Value::Null,
            "observer-close",
            owner,
            format,
        )
        .await
        .unwrap();
        let handle = instance_handle_of(&app.root).unwrap();
        let instance = instances().read().unwrap().get(handle).unwrap();
        let log = Arc::new(Events::default());
        logs().lock().unwrap().insert(handle, log.clone());
        Self {
            app: Some(app),
            handle,
            instance,
            log,
        }
    }
    fn stream(&self) -> (StreamSink, cordis_core::AsyncStream) {
        let (sink, stream) = StreamSink::new(1);
        sink.observe(Arc::new(HostStreamObserver {
            handle: self.handle,
            dispatch_id: 71,
        }));
        insert_dispatch(
            &self.instance,
            71,
            DispatchRecord {
                sink: None,
                frame: None,
                parent: None,
                outbound: None,
                stream: Some(sink.clone()),
            },
        )
        .unwrap_or_else(|_| panic!("admission"));
        assert_eq!(self.push(8), STATUS_OK);
        (sink, stream)
    }
    fn pending_next(&self) -> (Arc<AtomicBool>, CancelToken) {
        pending_next(&self.instance, self.handle, &self.log)
    }
    fn push(&self, id: u64) -> i32 {
        let mut out = Out {
            ptr: std::ptr::null_mut(),
            cap: 0,
            len: 0,
        };
        unsafe { host_stream_push(self.handle, 71, Blob::new(b"1"), id, &mut out) }
    }
    fn events(&self) -> Vec<(u32, Value, bool)> {
        self.log.history.lock().unwrap().clone()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        logs().lock().unwrap().remove(&self.handle);
    }
}
fn pending_next(instance: &Instance, handle: u64, log: &Events) -> (Arc<AtomicBool>, CancelToken) {
    let dropped = Arc::new(AtomicBool::new(false));
    *log.business_dropped.lock().unwrap() = Some(dropped.clone());
    let marker = dropped.clone();
    instance
        .dispatches
        .lock()
        .unwrap()
        .get_mut(&71)
        .unwrap()
        .frame = Some(Next::some(move |_| {
        let guard = DropProbe(marker.clone());
        Box::pin(async move {
            let _guard = guard;
            std::future::pending::<()>().await;
            None
        })
    }));
    let mut buffer = [0u8; 1024];
    let mut out = Out {
        ptr: buffer.as_mut_ptr(),
        cap: buffer.len(),
        len: 0,
    };
    assert_eq!(
        unsafe { host_ctx_next(handle, 71, Blob::new(b"[]"), 98, &mut out) },
        STATUS_OK
    );
    let tasks = instance.tasks.lock().unwrap();
    assert_eq!(tasks.len(), 1);
    assert!(!dropped.load(Ordering::SeqCst));
    (dropped, tasks.values().next().unwrap().cancel.clone())
}

#[tokio::test]
async fn o1e_close_waits_for_real_next_task_exit() {
    let mut f = Fixture::new(None).await;
    let (_sink, _stream) = f.stream();
    let instance = f.instance.clone();
    let handle = f.handle;
    let log = f.log.clone();
    let gate = gate();
    let worker_gate = gate.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let state = pending_next(&instance, handle, &log);
                let dropped = state.0.clone();
                tx.send(state).unwrap();
                worker_gate.0.wait();
                worker_gate.1.wait();
                while !dropped.load(Ordering::SeqCst) {
                    tokio::task::yield_now().await;
                }
            });
    });
    let (dropped, cancel) = rx.recv().unwrap();
    gate.0.wait();
    let mut close = Box::pin(f.app.take().unwrap().close());
    let early = std::future::poll_fn(|cx| {
        let result = close.as_mut().poll(cx);
        if result.is_ready() || cancel.is_cancelled() {
            Poll::Ready(result)
        } else {
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    })
    .await;
    let active = *f.instance.lifecycle.lock().unwrap();
    let dropped_early = dropped.load(Ordering::SeqCst);
    let disposed_early = f.events().iter().any(|(op, _, _)| *op == OP_DISPOSE);
    gate.1.wait();
    worker.join().unwrap();
    if early.is_pending() {
        close.await;
    }
    assert!(early.is_pending() && cancel.is_cancelled());
    assert_eq!(active, 0, "this wait is TaskExit, not ActiveCall");
    assert!(!dropped_early && !disposed_early);
    assert_eq!(*f.log.dispose_after_drop.lock().unwrap(), Some(true));
}

fn gate() -> Gate {
    Arc::new((Barrier::new(2), Barrier::new(2)))
}
fn read_one(mut stream: cordis_core::AsyncStream) {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(stream.next())
        .unwrap();
}
fn assert_gone_before_dispose(events: &[(u32, Value, bool)], ids: &[u64]) {
    let dispose = events
        .iter()
        .position(|(op, _, _)| *op == OP_DISPOSE)
        .unwrap();
    assert_eq!(
        events.iter().filter(|(op, _, _)| *op == OP_DISPOSE).count(),
        1
    );
    assert!(
        events[dispose + 1..]
            .iter()
            .all(|(op, _, _)| *op != OP_CALLBACK),
        "observer called after dispose: {events:?}"
    );
    let mut actual = Vec::new();
    for (_, payload, alive) in events.iter().filter(|(op, _, _)| *op == OP_CALLBACK) {
        assert!(*alive, "callback lost its lease");
        assert!(payload["args"][0]["error"].is_string(), "{payload}");
        actual.push(payload["id"].as_u64().unwrap());
    }
    actual.sort_unstable();
    let mut expected = ids.to_vec();
    expected.sort_unstable();
    assert_eq!(
        actual, expected,
        "every accepted registration must settle once"
    );
}

async fn paused_observer(finish: bool) {
    let mut f = Fixture::new(None).await;
    let (sink, stream) = f.stream();
    assert_eq!(f.push(9), STATUS_PENDING);
    let gate = gate();
    *f.instance.observer_gate.lock().unwrap() = Some(gate.clone());
    let worker = std::thread::spawn(move || {
        if finish {
            sink.close();
        } else {
            read_one(stream);
        }
    });
    gate.0.wait();
    f.app.take().unwrap().close().await;
    gate.1.wait();
    worker.join().unwrap();
    assert_gone_before_dispose(&f.events(), &[9]);
}
struct DeferredService;
impl cordis_core::ExternService for DeferredService {
    fn service(&self) -> &str {
        "o1d"
    }
    fn version(&self) -> u32 {
        1
    }
    fn methods(&self) -> Vec<String> {
        vec!["run".into()]
    }
    fn call(&self, _: Option<Config>, _: String, _: Config) -> BoxFuture<Result<Config>> {
        Box::pin(async {
            tokio::task::yield_now().await;
            Ok(json!(42))
        })
    }
}

async fn real_outbound_terminal_window(iface: bool) {
    let mut f = Fixture::new(None).await;
    f.instance
        .ctx
        .provide_iface("o1d", Arc::new(DeferredService))
        .unwrap();
    let _listener = f.instance.ctx.on("o1d", |_| {
        Box::pin(async {
            tokio::task::yield_now().await;
            Some(Dyn::new(ExternValue(json!(42))))
        })
    });
    let gate = gate();
    *f.instance.outbound_gate.lock().unwrap() = Some(gate.clone());
    let handle = f.handle;
    let worker = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let mut buffer = [0u8; 1024];
                let mut out = Out {
                    ptr: buffer.as_mut_ptr(),
                    cap: buffer.len(),
                    len: 0,
                };
                let status = unsafe {
                    if iface {
                        host_ctx_call_iface(
                            handle,
                            Blob::new(b"o1d"),
                            1,
                            Blob::new(b"run"),
                            Blob::new(b"[]"),
                            99,
                            &mut out,
                        )
                    } else {
                        host_ctx_dispatch(
                            handle,
                            Blob::new(b"o1d"),
                            Blob::new(br#"{"mode":"bail","args":[]}"#),
                            99,
                            &mut out,
                        )
                    }
                };
                assert_eq!(status, STATUS_OK);
                let response: Value = serde_json::from_slice(&buffer[..out.len]).unwrap();
                assert_eq!(response["pending"], true);
                while instances()
                    .read()
                    .unwrap()
                    .get(handle)
                    .is_some_and(|i| !i.tasks.lock().unwrap().is_empty())
                {
                    tokio::task::yield_now().await;
                }
            });
    });
    gate.0.wait();
    let mut close = Box::pin(f.app.take().unwrap().close());
    std::future::poll_fn(|cx| {
        let _ = close.as_mut().poll(cx);
        if f.instance.closed.load(Ordering::SeqCst) {
            std::task::Poll::Ready(())
        } else {
            cx.waker().wake_by_ref();
            std::task::Poll::Pending
        }
    })
    .await;
    gate.1.wait();
    close.await;
    worker.join().unwrap();
    let events = f.events();
    let callbacks: Vec<_> = events
        .iter()
        .filter(|(op, _, _)| *op == OP_CALLBACK)
        .collect();
    assert_eq!(callbacks.len(), 1, "{events:?}");
    assert_eq!(
        callbacks[0].1,
        json!({
            "kind": if iface { KIND_IFACE } else { KIND_DISPATCH },
            "id": 99,
            "args": [{ "result": 42 }],
        })
    );
    assert_eq!(events.last().unwrap().0, OP_DISPOSE);
    assert_eq!(
        events.iter().filter(|(op, _, _)| *op == OP_DISPOSE).count(),
        1
    );
}

async fn cancel_close_window(close_first: bool, format: u32, kind: ContinuationKind) {
    let dropped = Arc::new(AtomicBool::new(false));
    let owner: Arc<dyn Any + Send + Sync> = Arc::new(DropProbe(dropped.clone()));
    let mut f = Fixture::with_format(Some(owner.clone()), format).await;
    // 槽位那份强引用也收回测试侧：否则它会一直兜住"库"，`alive` 恒真。
    let slot_owner = take_slot_owner(f.handle).expect("注册期内实例必须仍在槽位里");
    *f.log.dropped.lock().unwrap() = Some(dropped.clone());
    let (sink, mut stream) = f.stream();
    assert_eq!(f.push(9), STATUS_PENDING);
    let (business_dropped, cancel) = f.pending_next();
    let (tx, rx) = tokio::sync::oneshot::channel();
    insert_dispatch(
        &f.instance,
        72,
        DispatchRecord {
            sink: Some(tx),
            frame: None,
            parent: Some(71),
            stream: None,
            outbound: Some(Outbound {
                owner: f.handle,
                continuation: 99,
                kind,
                trace_parent: None,
                trace_correlation: None,
            }),
        },
    )
    .unwrap_or_else(|_| panic!("admission"));
    let active = close_first.then(|| admit_call(&f.instance).unwrap());
    let gate = gate();
    if !close_first {
        *f.instance.cancel_gate.lock().unwrap() = Some(gate.clone());
    }
    let handle = f.handle;
    let abort = move || {
        let mut buffer = [0u8; 1024];
        let mut out = Out {
            ptr: buffer.as_mut_ptr(),
            cap: buffer.len(),
            len: 0,
        };
        unsafe { host_ctx_abort(handle, 71, &mut out) }
    };
    let worker = if close_first {
        None
    } else {
        Some(std::thread::spawn(abort))
    };
    if !close_first {
        gate.0.wait();
    }
    drop(owner);
    // 测试侧两份强引用都在这里交还：此后只有在飞的 call lease 与 close lease
    // 撑着"库"，`alive` 才重新有判别力。
    drop(slot_owner);
    let mut close = Box::pin(f.app.take().unwrap().close());
    let mut completed = false;
    std::future::poll_fn(|cx| {
        completed = close.as_mut().poll(cx).is_ready();
        if f.instance.closed.load(Ordering::SeqCst) {
            std::task::Poll::Ready(())
        } else {
            cx.waker().wake_by_ref();
            std::task::Poll::Pending
        }
    })
    .await;
    if close_first {
        assert_eq!(abort(), STATUS_ERR);
        assert_eq!(f.instance.dispatches.lock().unwrap().len(), 2);
        drop(active);
    } else {
        gate.1.wait();
        assert_eq!(worker.unwrap().join().unwrap(), STATUS_OK);
    }
    assert!(!completed, "close must wait for the claimed cancellation");
    assert_eq!(*f.instance.lifecycle.lock().unwrap(), 0);
    close.await;
    assert!(dropped.load(Ordering::SeqCst));
    assert!(f.events().iter().all(|(_, _, alive)| *alive));
    assert!(cancel.is_cancelled());
    assert!(business_dropped.load(Ordering::SeqCst));
    assert_eq!(*f.log.dispose_after_drop.lock().unwrap(), Some(true));
    assert!(f.instance.tasks.lock().unwrap().is_empty());
    let inbound = rx.await.unwrap().unwrap_err();
    assert_eq!(
        inbound,
        ErrorPayload::new(if close_first {
            "插件已卸载：挂起的入站续延被结算"
        } else {
            "已取消：调用方撤销了这次派发"
        })
        .with_code(if close_first {
            "host_lifecycle"
        } else {
            "host_cancel"
        })
        .with_category(if close_first { "lifecycle" } else { "cancel" })
        .with_certainty("unknown")
    );
    assert_eq!(
        stream.next().await.unwrap_err().to_string(),
        if close_first {
            "插件已卸载：这条流没有收尾就被收回"
        } else {
            "已取消：调用方撤销了这次派发"
        }
    );
    drop(sink);
    let events = f.events();
    let callbacks: Vec<_> = events
        .iter()
        .filter(|(op, _, _)| *op == OP_CALLBACK)
        .collect();
    assert_gone_before_dispose(&events, &[9, 98, 99]);
    let credit = callbacks
        .iter()
        .find(|(_, payload, _)| payload["id"] == 9)
        .unwrap();
    assert_eq!(
        credit.1,
        json!({
            "kind": KIND_STREAM_CREDIT, "id": 9,
            "args": [{ "error": "插件已卸载：这条流已被收回" }],
        })
    );
    assert!(f.instance.credits.lock().unwrap().is_empty());
    let error = if close_first {
        "插件已卸载：等待中的调用被结算"
    } else {
        "已取消：调用方撤销了这次派发"
    };
    let mut arg = json!({ "error": error });
    if format == ERROR_FORMAT_V1 {
        arg = json!({ "error": error, "code": if close_first { "host_lifecycle" } else { "host_cancel" },
            "category": if close_first { "lifecycle" } else { "cancel" }, "certainty": "unknown", "retry": "unknown", "detail": null });
    }
    let next_callbacks: Vec<_> = callbacks
        .iter()
        .filter(|(_, payload, _)| payload["id"] == 98)
        .collect();
    assert_eq!(next_callbacks.len(), 1, "{events:?}");
    assert_eq!(
        next_callbacks[0].1,
        json!({ "kind": KIND_NEXT, "id": 98, "args": [arg.clone()] })
    );
    let callbacks: Vec<_> = callbacks
        .into_iter()
        .filter(|(_, payload, _)| payload["id"] == 99)
        .collect();
    assert_eq!(callbacks.len(), 1, "{events:?}");
    assert_eq!(
        callbacks[0].1,
        json!({ "kind": kind.wire_name(), "id": 99, "args": [arg] })
    );
    assert_eq!(events.last().unwrap().0, OP_DISPOSE);
    assert_eq!(
        events.iter().filter(|(op, _, _)| *op == OP_DISPOSE).count(),
        1
    );
}

#[tokio::test]
async fn o1e_dead_weak_host_cleanup_blocks_close() {
    let (owner, dropped) = owner();
    let mut f = Fixture::new(Some(owner.clone())).await;
    let (_sink, mut stream) = f.stream();
    let gate = gate();
    *f.instance.cancel_gate.lock().unwrap() = Some(gate.clone());
    kill_keepalive(&mut f);
    // 槽位那份强引用已被探针摘走并 drop；这里释放的是测试侧留下的唯一一份克隆，
    // 它负责让下面的 `dropped` 真正反映"最后一枚强引用已释放"。
    drop(owner);
    assert!(dropped.load(Ordering::SeqCst));
    let handle = f.handle;
    let worker = std::thread::spawn(move || {
        let mut buffer = [0u8; 1024];
        let mut out = Out {
            ptr: buffer.as_mut_ptr(),
            cap: buffer.len(),
            len: 0,
        };
        unsafe { host_ctx_abort(handle, 71, &mut out) }
    });
    gate.0.wait();
    let mut close = Box::pin(f.app.take().unwrap().close());
    let early = std::future::poll_fn(|cx| {
        let result = close.as_mut().poll(cx);
        if result.is_ready() || f.instance.closed.load(Ordering::SeqCst) {
            Poll::Ready(result)
        } else {
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    })
    .await;
    let active = *f.instance.lifecycle.lock().unwrap();
    gate.1.wait();
    assert_eq!(worker.join().unwrap(), STATUS_OK);
    if early.is_pending() {
        close.await;
    }
    assert!(early.is_pending());
    assert_eq!(
        active, 1,
        "host-only cleanup retains ActiveCall without FFI permission"
    );
    assert!(stream.next().await.is_err());
    assert!(f.events().is_empty());
}

#[tokio::test]
async fn o1e_dead_weak_abort_cleanup() {
    dead_weak_cleanup(false).await;
}

#[tokio::test]
async fn o1e_dead_weak_timeout_cleanup() {
    dead_weak_cleanup(true).await;
}

async fn dead_weak_cleanup(timeout: bool) {
    let (owner, dropped) = owner();
    let mut f = Fixture::new(Some(owner.clone())).await;
    let (_sink, mut stream) = f.stream();
    assert_eq!(f.push(9), STATUS_PENDING);
    let (business_dropped, cancel) = f.pending_next();
    let (tx, rx) = tokio::sync::oneshot::channel();
    insert_dispatch(
        &f.instance,
        72,
        DispatchRecord {
            sink: Some(tx),
            frame: None,
            parent: Some(71),
            stream: None,
            outbound: Some(Outbound {
                owner: f.handle,
                continuation: 99,
                kind: ContinuationKind::Iface,
                trace_parent: None,
                trace_correlation: None,
            }),
        },
    )
    .unwrap_or_else(|_| panic!("admission"));
    kill_keepalive(&mut f);
    // 同上：探针已摘走槽位那份，这里是测试侧最后一份克隆的释放点。
    drop(owner);
    assert!(dropped.load(Ordering::SeqCst));
    if timeout {
        let (tx, root_rx) = tokio::sync::oneshot::channel();
        f.instance
            .dispatches
            .lock()
            .unwrap()
            .get_mut(&71)
            .unwrap()
            .sink = Some(tx);
        let mut instance = f.instance.clone();
        instance.dispatch_policy.timeout_ms = Some(1);
        let pending = PendingContinuation {
            instance: instance.clone(),
            dispatch_id: 71,
            rx: root_rx,
            cancel: None,
            _abandon: PendingDropGuard {
                instance,
                dispatch_id: 71,
            },
            trace_parent: None,
            traced: false,
            callee: (0, Box::from("test")),
            caller_uid: 0,
            correlation_id: None,
            name: Box::from("test"),
            started: std::time::Instant::now(),
        };
        assert!(pending.settle("dead owner").await.is_err());
    } else {
        let mut buffer = [0u8; 1024];
        let mut out = Out {
            ptr: buffer.as_mut_ptr(),
            cap: buffer.len(),
            len: 0,
        };
        assert_eq!(unsafe { host_ctx_abort(f.handle, 71, &mut out) }, STATUS_OK);
    }
    assert!(f.instance.dispatches.lock().unwrap().is_empty());
    let expected = ErrorPayload::new(if timeout {
        "dead owner：等待插件结算超时（1ms）"
    } else {
        "已取消：调用方撤销了这次派发"
    })
    .with_code(if timeout {
        "HOST_TIMEOUT"
    } else {
        "host_cancel"
    })
    .with_category(if timeout { "timeout" } else { "cancel" })
    .with_certainty("unknown");
    assert_eq!(rx.await.unwrap().unwrap_err(), expected);
    assert_eq!(stream.next().await.unwrap_err().to_string(), expected.error);
    assert!(f.instance.credits.lock().unwrap().is_empty());
    assert!(f.events().is_empty(), "dead Weak must never enter FFI");
    assert!(cancel.is_cancelled());
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while !f.instance.tasks.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        business_dropped.load(Ordering::SeqCst),
        "abort/timeout must drop the future without close"
    );
    f.app.take().unwrap().close().await;
    assert!(f.events().is_empty());
}

#[tokio::test]
async fn o1e_abort_claim_before_close() {
    for format in [0, ERROR_FORMAT_V1] {
        for kind in [ContinuationKind::Iface, ContinuationKind::Dispatch] {
            cancel_close_window(false, format, kind).await;
        }
    }
}

#[tokio::test]
async fn o1e_close_claim_before_abort() {
    for format in [0, ERROR_FORMAT_V1] {
        for kind in [ContinuationKind::Iface, ContinuationKind::Dispatch] {
            cancel_close_window(true, format, kind).await;
        }
    }
}

#[tokio::test]
async fn o1d_real_iface_terminal_window() {
    real_outbound_terminal_window(true).await;
}

#[tokio::test]
async fn o1d_real_dispatch_terminal_window() {
    real_outbound_terminal_window(false).await;
}

#[tokio::test]
async fn o1d_outbound_terminal_window() {
    for in_callback in [false, true] {
        for kind in [ContinuationKind::Iface, ContinuationKind::Dispatch] {
            let dropped = Arc::new(AtomicBool::new(false));
            let owner: Arc<dyn Any + Send + Sync> = Arc::new(DropProbe(dropped.clone()));
            let mut f = Fixture::new(Some(owner.clone())).await;
            // 槽位那份强引用收回测试侧，稍后与测试自己那份一起 drop。
            let slot_owner = take_slot_owner(f.handle).expect("注册期内实例必须仍在槽位里");
            *f.log.dropped.lock().unwrap() = Some(dropped.clone());
            insert_dispatch(
                &f.instance,
                72,
                DispatchRecord {
                    sink: None,
                    frame: None,
                    parent: None,
                    stream: None,
                    outbound: Some(Outbound {
                        owner: f.handle,
                        continuation: 99,
                        kind,
                        trace_parent: None,
                        trace_correlation: None,
                    }),
                },
            )
            .unwrap_or_else(|_| panic!("admission"));
            let gate = gate();
            if in_callback {
                *f.log.gate.lock().unwrap() = Some(gate.clone());
            } else {
                *f.instance.outbound_gate.lock().unwrap() = Some(gate.clone());
            }
            let expected = json!({
                "kind": kind.wire_name(),
                "id": 99,
                "args": [{ "result": 42 }],
            });
            let payload = expected.to_string();
            let handle = f.handle;
            let worker = std::thread::spawn(move || {
                deliver_outbound(handle, 72, &payload);
            });
            gate.0.wait();
            drop(owner);
            // 两份强引用都交还：终端交付能否活下来，只由它自己的 call lease 决定。
            drop(slot_owner);
            assert!(!dropped.load(Ordering::SeqCst));
            let mut close = Box::pin(f.app.take().unwrap().close());
            let early = std::future::poll_fn(|cx| {
                let result = close.as_mut().poll(cx);
                if result.is_ready() || f.instance.closed.load(Ordering::SeqCst) {
                    std::task::Poll::Ready(result)
                } else {
                    cx.waker().wake_by_ref();
                    std::task::Poll::Pending
                }
            })
            .await;
            gate.1.wait();
            worker.join().unwrap();
            if early.is_pending() {
                close.await;
            }
            let events = f.events();
            let callbacks: Vec<_> = events
                .iter()
                .filter(|(op, _, _)| *op == OP_CALLBACK)
                .collect();
            assert!(early.is_pending(), "close must wait for admitted delivery");
            assert!(dropped.load(Ordering::SeqCst));
            assert!(events.iter().all(|(_, _, alive)| *alive));
            assert_eq!(callbacks.len(), 1, "terminal delivery lost: {events:?}");
            assert_eq!(callbacks[0].1, expected);
            assert_eq!(
                events.iter().filter(|(op, _, _)| *op == OP_DISPOSE).count(),
                1
            );
            let delivered = events
                .iter()
                .position(|(op, _, _)| *op == OP_CALLBACK)
                .unwrap();
            let disposed = events
                .iter()
                .position(|(op, _, _)| *op == OP_DISPOSE)
                .unwrap();
            assert!(delivered < disposed);
        }
    }
}

#[tokio::test]
async fn o1d_close_wins_keeps_outbound_for_abort() {
    for kind in [ContinuationKind::Iface, ContinuationKind::Dispatch] {
        // 本用例经共用断言 `assert_gone_before_dispose` 检查 `alive`，所以必须真的
        // 有一枚可观测的"库"：强引用只由测试侧（`active` 的 call lease）持有。
        let dropped = Arc::new(AtomicBool::new(false));
        let owner: Arc<dyn Any + Send + Sync> = Arc::new(DropProbe(dropped.clone()));
        let mut f = Fixture::new(Some(owner.clone())).await;
        *f.log.dropped.lock().unwrap() = Some(dropped.clone());
        insert_dispatch(
            &f.instance,
            72,
            DispatchRecord {
                sink: None,
                frame: None,
                parent: None,
                stream: None,
                outbound: Some(Outbound {
                    owner: f.handle,
                    continuation: 99,
                    kind,
                    trace_parent: None,
                    trace_correlation: None,
                }),
            },
        )
        .unwrap_or_else(|_| panic!("admission"));
        let active = admit_call(&f.instance).unwrap();
        // 槽位那份强引用也交还：`drop(active)` 之后只剩 close lease 撑着"库"，
        // 取消交付与 OP_DISPOSE 的 `alive` 才由它决定。
        drop(owner);
        drop(take_slot_owner(f.handle).expect("注册期内实例必须仍在槽位里"));
        let mut close = Box::pin(f.app.take().unwrap().close());
        std::future::poll_fn(|cx| {
            let _ = close.as_mut().poll(cx);
            if f.instance.closed.load(Ordering::SeqCst) {
                std::task::Poll::Ready(())
            } else {
                cx.waker().wake_by_ref();
                std::task::Poll::Pending
            }
        })
        .await;
        let expected_kind = kind.wire_name();
        let payload = json!({
            "kind": expected_kind,
            "id": 99,
            "args": [{ "result": 42 }],
        })
        .to_string();
        deliver_outbound(f.handle, 72, &payload);
        assert!(f.instance.dispatches.lock().unwrap().contains_key(&72));
        drop(active);
        close.await;
        let events = f.events();
        assert_gone_before_dispose(&events, &[99]);
        let callback = events.iter().find(|(op, _, _)| *op == OP_CALLBACK).unwrap();
        assert_eq!(
            callback.1,
            json!({
                "kind": expected_kind,
                "id": 99,
                "args": [{ "error": "插件已卸载：等待中的调用被结算" }],
            })
        );
    }
}

#[tokio::test]
async fn o1c_popped_credit_close() {
    paused_observer(false).await;
}
#[tokio::test]
async fn o1c_taken_finish_close() {
    paused_observer(true).await;
}

#[tokio::test]
async fn o1c_full_window_duplicates_and_removed_record() {
    for remove_record in [false, true] {
        let mut f = Fixture::new(None).await;
        let (_sink, _stream) = f.stream();
        for id in [9, 9, 10] {
            assert_eq!(f.push(id), STATUS_PENDING);
        }
        if remove_record {
            let removed = f.instance.dispatches.lock().unwrap().remove(&71);
            drop(removed);
        }
        f.app.take().unwrap().close().await;
        assert_gone_before_dispose(&f.events(), &[9, 9, 10]);
        assert!(f.instance.credits.lock().unwrap().is_empty());
        assert_eq!(f.push(11), STATUS_ERR);
    }
}

struct DropProbe(Arc<AtomicBool>);
impl Drop for DropProbe {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}
fn owner() -> (Arc<dyn Any + Send + Sync>, Arc<AtomicBool>) {
    let dropped = Arc::new(AtomicBool::new(false));
    (Arc::new(DropProbe(dropped.clone())), dropped)
}

/// 构造"注册期内的 dead Weak"。
///
/// 前提已随所有权模型改变：库保活强 owner 现在住在**实例表槽位**里，注册期内
/// 弱引用不可能自然失效（旧实现注册时 downgrade 后立刻丢弃，才让 dead Weak
/// 变成常态）。要保留"dead Weak 绝不碰裸 `CallFn`"这条既有纪律，只能用定向
/// 探针：摘掉槽位强 owner，并把调用方手里的 `Instance` 克隆同步到同一个 dead Weak
/// （克隆不共享槽位的那一项）。摘下来的 owner 在探针里锁外 drop，因此随后的
/// `assert!(dropped)` 仍然测得"最后一枚强引用已释放"。
fn kill_keepalive(f: &mut Fixture) {
    let dead = force_dead_weak(f.handle).expect("注册期内实例必须仍在槽位里");
    f.instance.keepalive = Some(dead);
}

#[tokio::test]
async fn o1c_admitted_callback_lease_and_close_wait() {
    let (owner, dropped) = owner();
    let mut f = Fixture::new(Some(owner.clone())).await;
    *f.log.dropped.lock().unwrap() = Some(dropped.clone());
    // 槽位那份强引用收回测试侧：否则它一直兜住"库"，`alive_during_call` 无判别力。
    let slot_owner = take_slot_owner(f.handle).expect("注册期内实例必须仍在槽位里");
    let (_sink, stream) = f.stream();
    assert_eq!(f.push(9), STATUS_PENDING);
    let gate = gate();
    *f.log.gate.lock().unwrap() = Some(gate.clone());
    let worker = std::thread::spawn(move || read_one(stream));
    gate.0.wait();
    drop(owner);
    // 观测者正在回调里（持有 call lease），两份测试侧强引用在这里交还：
    // `alive_during_call` 从此只由 call lease 决定。
    drop(slot_owner);
    let alive_during_call = !dropped.load(Ordering::SeqCst);
    let mut close = Box::pin(f.app.take().unwrap().close());
    let pending = std::future::poll_fn(|cx| {
        let pending = close.as_mut().poll(cx).is_pending();
        if !pending || f.instance.closed.load(Ordering::SeqCst) {
            Poll::Ready(pending)
        } else {
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    })
    .await;
    let closed = f.instance.closed.load(Ordering::SeqCst);
    let disposed_early = f.events().iter().any(|(op, _, _)| *op == OP_DISPOSE);
    gate.1.wait();
    worker.join().unwrap();
    close.await;
    assert!(alive_during_call, "observer must hold a strong call lease");
    assert!(
        pending && closed && !disposed_early,
        "close must wait for admitted callback"
    );
    assert!(
        dropped.load(Ordering::SeqCst),
        "call and close leases must release"
    );
    let events = f.events();
    assert_eq!(events.len(), 2, "credit once, then dispose: {events:?}");
    assert_eq!(events[0].1["args"][0]["result"]["window"], 1);
    assert_eq!(events[1].0, OP_DISPOSE);
    assert!(events.iter().all(|(_, _, alive)| *alive));
}

#[tokio::test]
async fn o1c_close_lease_covers_gone_abort_dispose() {
    let (owner, dropped) = owner();
    let mut f = Fixture::new(Some(owner.clone())).await;
    *f.log.dropped.lock().unwrap() = Some(dropped.clone());
    // 唯一强引用交给夹具的 `release_owner`：它在第一个 OP_CALLBACK 里被 drop，
    // 之后只有 close lease 撑着"库"。槽位那份也必须一并交出来，否则它会兜住
    // `alive`，让"close 是否持 lease"失去判别力。
    let slot_owner = take_slot_owner(f.handle).expect("注册期内实例必须仍在槽位里");
    drop(owner);
    *f.log.release_owner.lock().unwrap() = Some(slot_owner);
    let (_sink, _stream) = f.stream();
    assert_eq!(f.push(9), STATUS_PENDING);
    insert_dispatch(
        &f.instance,
        72,
        DispatchRecord {
            sink: None,
            frame: None,
            parent: None,
            stream: None,
            outbound: Some(Outbound {
                owner: f.handle,
                continuation: 10,
                kind: ContinuationKind::Iface,
                trace_parent: None,
                trace_correlation: None,
            }),
        },
    )
    .unwrap_or_else(|_| panic!("admission"));
    f.app.take().unwrap().close().await;
    let events = f.events();
    assert_eq!(events.len(), 3, "Gone, abort, dispose: {events:?}");
    assert_eq!(events[0].1["kind"], KIND_STREAM_CREDIT);
    assert_eq!(events[1].1["kind"], KIND_IFACE);
    assert_eq!(events[2].0, OP_DISPOSE);
    assert!(
        events.iter().all(|(_, _, alive)| *alive),
        "close lost lease: {events:?}"
    );
    assert!(dropped.load(Ordering::SeqCst));
}

#[tokio::test]
async fn o1c_dead_weak_never_calls_raw_target() {
    let (owner, dropped) = owner();
    let mut f = Fixture::new(Some(owner.clone())).await;
    let (_sink, mut stream) = f.stream();
    assert_eq!(f.push(9), STATUS_PENDING);
    // 注册期内弱引用不再可能自然变 dead（强 owner 在实例表槽位），
    // 用定向探针构造旧模型下"drop 掉唯一 owner"的那个状态。
    kill_keepalive(&mut f);
    drop(owner);
    assert!(dropped.load(Ordering::SeqCst));
    stream.next().await.unwrap();
    assert!(unsafe { call_live(f.handle, OP_CALLBACK, "{}") }.is_err());
    // 只断言 `is_err()` 太松：将来换成别的拒绝理由（例如句柄查不到）也会继续绿。
    let failure = unsafe {
        call_inbound(
            &f.instance,
            f.handle,
            OP_CALLBACK,
            json!({}),
            None,
            None,
            None,
            None,
            false,
            None,
        )
    }
    .err()
    .expect("dead Weak 必须拒绝入站回调");
    assert_eq!(
        failure.payload.code, "host_lifecycle",
        "拒绝必须来自生命周期门控（库已释放），不能是别的错误理由"
    );
    f.app.take().unwrap().close().await;
    assert!(
        f.events().is_empty(),
        "dead Weak must never call a bare CallFn"
    );
    assert!(f.instance.credits.lock().unwrap().is_empty());
}

// Run the synchronous reentry probe in a child: a lock regression must fail,
// rather than trapping the entire lib test process in an unjoinable FFI call.
#[test]
fn o1c_reentrant_close_child() {
    let Ok(mode) = std::env::var("CORDIS_O1C_REENTRY") else {
        return;
    };
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(async {
            let mut f = Fixture::new(None).await;
            let (_sink, _stream) = f.stream();
            assert_eq!(f.push(9), STATUS_PENDING);
            let gate = gate();
            *f.log.gate.lock().unwrap() = Some(gate.clone());
            let handle = f.handle;
            let instance = f.instance.clone();
            let worker = std::thread::spawn(move || unsafe {
                if mode == "live" {
                    call_live(handle, OP_CALLBACK, "{\"reenter\":71}")
                        .map(|_| ())
                        .unwrap();
                } else {
                    call_inbound(
                        &instance,
                        handle,
                        OP_CALLBACK,
                        json!({"reenter":71}),
                        None,
                        None,
                        None,
                        None,
                        false,
                        None,
                    )
                    .map(|_| ())
                    .unwrap_or_else(|_| panic!("inbound failed"));
                }
            });
            gate.0.wait();
            let mut close = Box::pin(f.app.take().unwrap().close());
            let pending = std::future::poll_fn(|cx| {
                let pending = close.as_mut().poll(cx).is_pending();
                if !pending || f.instance.closed.load(Ordering::SeqCst) {
                    Poll::Ready(pending)
                } else {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            })
            .await;
            let closed = f.instance.closed.load(Ordering::SeqCst);
            gate.1.wait();
            worker.join().unwrap();
            close.await;
            assert!(pending && closed);
            assert_eq!(*f.log.reentry_status.lock().unwrap(), Some(STATUS_OK));
            assert_gone_before_dispose(&f.events()[1..], &[9]);
        });
}

#[test]
fn o1c_reentrant_close_has_no_lock_or_self_wait() {
    for mode in ["live", "inbound"] {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "observer_close_tests::o1c_reentrant_close_child",
                "--nocapture",
            ])
            .env("CORDIS_O1C_REENTRY", mode)
            .spawn()
            .unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        // The deadline is a deadlock watchdog; barriers establish the ordering.
        let id = child.id();
        let waiter = std::thread::spawn(move || {
            let _ = tx.send(child.wait());
        });
        match rx.recv_timeout(std::time::Duration::from_secs(20)) {
            Ok(status) => {
                waiter.join().unwrap();
                assert!(status.unwrap().success(), "{mode}");
            }
            Err(error) => {
                #[cfg(windows)]
                let _ = std::process::Command::new("taskkill")
                    .args(["/PID", &id.to_string(), "/F"])
                    .status();
                #[cfg(not(windows))]
                let _ = std::process::Command::new("kill")
                    .args(["-9", &id.to_string()])
                    .status();
                waiter.join().unwrap();
                panic!("reentry deadlocked ({mode}): {error}");
            }
        }
    }
}

#[tokio::test]
async fn r2_p1_drain_timed_out_still_delivers_pending_next_on_done_before_dispose() {
    let mut f = Fixture::new(None).await;
    f.instance
        .ctx
        .store
        .set_lifecycle_policy(cordis_core::LifecyclePolicy {
            drain_timeout_ms: Some(30),
            ..Default::default()
        })
        .unwrap();
    let (_sink, _stream) = f.stream();
    let (_dropped, _cancel) = f.pending_next();

    // Simulate an in-flight native call that outlives `drain_timeout_ms` so close takes
    // the `drain_timed_out` handoff branch while a `host_ctx_next` task is still pending.
    *f.instance.lifecycle.lock().unwrap() += 1;
    let instance_release = f.instance.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(80));
        let mut active = instance_release.lifecycle.lock().unwrap();
        *active -= 1;
        if *active == 0 {
            instance_release.calls_changed.notify_waiters();
        }
    });

    let _report = f.app.take().unwrap().close_with_report(None).await;
    // Wait for background teardown to complete OP_DISPOSE.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        if f.events().iter().any(|(op, _, _)| *op == OP_DISPOSE) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "background teardown did not finish OP_DISPOSE in time: {:?}",
            f.events()
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let events = f.events();
    let dispose_idx = events
        .iter()
        .position(|(op, _, _)| *op == OP_DISPOSE)
        .expect("OP_DISPOSE must be delivered");
    let next_callbacks: Vec<_> = events[..dispose_idx]
        .iter()
        .filter(|(op, payload, _)| {
            *op == OP_CALLBACK && payload["kind"] == KIND_NEXT && payload["id"].as_u64() == Some(98)
        })
        .collect();
    assert_eq!(
        next_callbacks.len(),
        1,
        "drain_timed_out must still deliver pending next on_done once before OP_DISPOSE: {events:?}"
    );
    assert!(
        next_callbacks[0].1["args"][0]["error"].is_string(),
        "on_done abort must carry error payload: {:?}",
        next_callbacks[0].1
    );
}

#[tokio::test]
async fn r2_p1_deliver_next_racing_with_close_delivers_on_done_exactly_once() {
    let mut f = Fixture::new(None).await;
    let (_sink, _stream) = f.stream();
    let release_next = Arc::new(tokio::sync::Notify::new());
    let release_for_frame = release_next.clone();
    f.instance
        .dispatches
        .lock()
        .unwrap()
        .get_mut(&71)
        .unwrap()
        .frame = Some(Next::some(move |_| {
        let release = release_for_frame.clone();
        Box::pin(async move {
            release.notified().await;
            Some(Dyn::new(ExternValue(json!("downstream-ok"))))
        })
    }));
    let mut buffer = [0u8; 1024];
    let mut out = Out {
        ptr: buffer.as_mut_ptr(),
        cap: buffer.len(),
        len: 0,
    };
    assert_eq!(
        unsafe { host_ctx_next(f.handle, 71, Blob::new(b"[]"), 98, &mut out) },
        STATUS_OK
    );

    // Pause close right after `instance.closed = true` (inside `wait_lifecycle_exclusive`),
    // BEFORE `finish_instance_teardown` drains `instance.tasks`.
    *f.instance.lifecycle.lock().unwrap() += 1;
    let mut close = Box::pin(f.app.take().unwrap().close());
    std::future::poll_fn(|cx| {
        let _ = close.as_mut().poll(cx);
        if f.instance.closed.load(Ordering::SeqCst) {
            Poll::Ready(())
        } else {
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    })
    .await;

    // Now let the downstream `next` future finish while `instance.closed == true`!
    release_next.notify_one();
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }

    // Release the artificial active call so close finishes teardown.
    {
        let mut active = f.instance.lifecycle.lock().unwrap();
        *active -= 1;
        f.instance.calls_changed.notify_waiters();
    }
    close.await;

    let events = f.events();
    let dispose_idx = events
        .iter()
        .position(|(op, _, _)| *op == OP_DISPOSE)
        .expect("OP_DISPOSE must be delivered");
    let next_callbacks: Vec<_> = events[..dispose_idx]
        .iter()
        .filter(|(op, payload, _)| {
            *op == OP_CALLBACK && payload["kind"] == KIND_NEXT && payload["id"].as_u64() == Some(98)
        })
        .collect();
    assert_eq!(
        next_callbacks.len(),
        1,
        "deliver_next racing with close must deliver on_done exactly once before OP_DISPOSE: {events:?}"
    );
}

// R3 P1-5: Sync fast-path `host_ctx_next` drops `TaskExit` BEFORE `deliver_next`;
// if unload sets `instance.closed = true` in that window, `on_done` MUST still be
// delivered exactly once via `deferred_outbounds` before `OP_DISPOSE`.
#[tokio::test]
async fn r3_p1_host_ctx_next_sync_fast_path_racing_with_close_delivers_on_done_once() {
    let mut f = Fixture::new(None).await;
    let delivered = Arc::new(AtomicBool::new(false));
    let cancel_slot = Arc::new(std::sync::Mutex::new(Some(Outbound {
        owner: f.handle,
        continuation: 198,
        kind: ContinuationKind::Next,
        trace_parent: None,
        trace_correlation: None,
    })));
    // Mark closed and start close paused in `wait_lifecycle_exclusive`.
    *f.instance.lifecycle.lock().unwrap() += 1;
    let mut close = Box::pin(f.app.take().unwrap().close());
    std::future::poll_fn(|cx| {
        let _ = close.as_mut().poll(cx);
        if f.instance.closed.load(Ordering::SeqCst) {
            Poll::Ready(())
        } else {
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    })
    .await;

    // `instance.tasks` is empty (as after sync `complete_or_defer` dropped `TaskExit` before `closed` flipped).
    // Calling `deliver_next` now sees `closed == true` and MUST hand `cancel_slot` to `deferred_outbounds`.
    deliver_next(
        f.handle,
        71,
        198,
        0,
        &delivered,
        Some(&cancel_slot),
        Ok(Some(Dyn::new(ExternValue(json!("sync-ok"))))),
    );
    {
        let mut active = f.instance.lifecycle.lock().unwrap();
        *active -= 1;
        f.instance.calls_changed.notify_waiters();
    }
    close.await;

    let events = f.events();
    let dispose_idx = events
        .iter()
        .position(|(op, _, _)| *op == OP_DISPOSE)
        .expect("OP_DISPOSE must be delivered");
    let next_callbacks: Vec<_> = events[..dispose_idx]
        .iter()
        .filter(|(op, payload, _)| {
            *op == OP_CALLBACK
                && payload["kind"] == KIND_NEXT
                && payload["id"].as_u64() == Some(198)
        })
        .collect();
    assert_eq!(
        next_callbacks.len(),
        1,
        "同步快路径 TaskExit 已先退出后 deliver_next 遇 closed==true 仍须交付一次 on_done: {events:?}"
    );
}

// R3 P2-9: `host_ctx_call_iface` returning synchronously after re-entrant `host_ctx_abort`
// must observe `unregister_dispatch_node == false` and return `pending: true` (no double settlement).
#[tokio::test]
async fn r3_p2_host_ctx_call_iface_reentrant_abort_does_not_double_settle() {
    struct AbortingCallee {
        handle: u64,
        parent_dispatch_id: u64,
    }
    impl cordis_core::ExternService for AbortingCallee {
        fn service(&self) -> &str {
            "abort-callee"
        }
        fn version(&self) -> u32 {
            1
        }
        fn methods(&self) -> Vec<String> {
            vec!["boom".to_string()]
        }
        fn call(
            &self,
            _caller_meta: Option<Config>,
            _method: String,
            _args: Config,
        ) -> cordis_core::BoxFuture<cordis_core::Result<Config>> {
            let handle = self.handle;
            let parent_id = self.parent_dispatch_id;
            Box::pin(async move {
                let mut buf = [0u8; 256];
                let mut out = Out {
                    ptr: buf.as_mut_ptr(),
                    cap: buf.len(),
                    len: 0,
                };
                let _ = unsafe { host_ctx_abort(handle, parent_id, &mut out) };
                Ok(json!("inline-val"))
            })
        }
    }

    let f = Fixture::new(None).await;
    let app = f.app.as_ref().unwrap();
    let _prov = app
        .root
        .provide_iface(
            "abort-callee@1",
            Arc::new(AbortingCallee {
                handle: f.handle,
                parent_dispatch_id: 500,
            }),
        )
        .unwrap();
    // Register parent inbound dispatch 500 so child `call_iface` attaches under it.
    f.instance.dispatches.lock().unwrap().insert(
        500,
        DispatchRecord {
            parent: None,
            sink: None,
            frame: None,
            outbound: None,
            stream: None,
        },
    );

    let mut buf = [0u8; 1024];
    let mut out = Out {
        ptr: buf.as_mut_ptr(),
        cap: buf.len(),
        len: 0,
    };
    let rc = {
        let _scope = scope_push(f.handle, 500);
        unsafe {
            host_ctx_call_iface(
                f.handle,
                Blob::new(b"abort-callee@1"),
                1,
                Blob::new(b"boom"),
                Blob::new(b"[]"),
                501,
                &mut out,
            )
        }
    };
    assert_eq!(rc, STATUS_OK);
    let reply: serde_json::Value = serde_json::from_slice(&buf[..out.len]).unwrap();
    assert_eq!(
        reply["pending"],
        json!(true),
        "重入 ctx_abort 已通过 OP_CALLBACK 结算后，host_ctx_call_iface 必须返回 pending=true 而非二次内联结算: {reply}"
    );
}

// R3 P3-8: During `drain_timed_out` window (`closed == true` and `record.stream == None`),
// `host_stream_close`/`fail`/`fail_payload` must return `STATUS_STREAM_GONE` (idempotent), not `STATUS_ERR`.
#[tokio::test]
async fn r3_p3_stream_close_and_fail_during_closed_drain_return_stream_gone() {
    let f = Fixture::new(None).await;
    f.instance.dispatches.lock().unwrap().insert(
        600,
        DispatchRecord {
            parent: None,
            sink: None,
            frame: None,
            outbound: None,
            stream: None, // cleared by `drain_timed_out` before `HostStreamObserver::finished` removes the key
        },
    );
    f.instance.closed.store(true, Ordering::SeqCst);

    let mut buf = [0u8; 256];
    let mut out = Out {
        ptr: buf.as_mut_ptr(),
        cap: buf.len(),
        len: 0,
    };
    assert_eq!(
        unsafe { host_stream_close(f.handle, 600, &mut out) },
        STATUS_STREAM_GONE,
        "实例已关闭排水窗口内 stream_close 必须返回 STATUS_STREAM_GONE"
    );
    assert_eq!(
        unsafe { host_stream_fail(f.handle, 600, Blob::new(b"err"), &mut out) },
        STATUS_STREAM_GONE,
        "实例已关闭排水窗口内 stream_fail 必须返回 STATUS_STREAM_GONE"
    );
    let payload = ErrorPayload::new("err").to_json();
    assert_eq!(
        unsafe { host_stream_fail_payload(f.handle, 600, Blob::new(payload.as_bytes()), &mut out) },
        STATUS_STREAM_GONE,
        "实例已关闭排水窗口内 stream_fail_payload 必须返回 STATUS_STREAM_GONE"
    );
}
