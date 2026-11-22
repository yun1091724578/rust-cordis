//! O3：宿主 poll 边界的 panic **结算与收尾**。
//!
//! # 承诺范围（窄而明确）
//! 只覆盖**宿主进程内、future 被 poll 时**的 Rust unwind（`panic=unwind`）：
//! `extern "C"` 边界 abort、OOM/abort、`panic=abort`、永久同步 hang、panic-in-Drop、
//! 跨 ABI 插件内部 panic（SDK 已截）都**不在**这里的承诺里。
//!
//! # 为什么这些测试值得独占一个文件
//! 每个用例都要求"记录被摘干净、插件恰好收到一条结局、关闭时不再补发"。
//! 夹具铁律沿用既有约定：假 C 入口**只记录，绝不 assert / panic**——
//! 跨 `extern "C"` 的 panic 会 abort 整个测试进程，红证据就糊成一团。
//!
//! 注入点只用 Rust：`ExternService::call` 的 future、`Ctx::on` 的闭包、
//! `Next::some` 的闭包。同步一律用 `Notify` / `oneshot`；超时只当看门狗。

use super::*;

// ------------------------------------------------------------ 假 C 入口（只记录）

#[derive(Default)]
struct Recorder {
    events: Mutex<Vec<(u32, Value)>>,
    callback: tokio::sync::Notify,
}

fn recorders() -> &'static Mutex<HashMap<u64, Arc<Recorder>>> {
    static RECORDERS: OnceLock<Mutex<HashMap<u64, Arc<Recorder>>>> = OnceLock::new();
    RECORDERS.get_or_init(Default::default)
}

unsafe extern "C" fn record(handle: u64, op: u32, args: Blob, _: Blob, out: *mut Out) -> i32 {
    let recorder = recorders()
        .lock()
        .ok()
        .and_then(|map| map.get(&handle).cloned());
    if let Some(recorder) = recorder {
        let payload: Value = serde_json::from_slice(args.as_bytes()).unwrap_or(Value::Null);
        if let Ok(mut events) = recorder.events.lock() {
            events.push((op, payload));
        }
        if op == OP_CALLBACK {
            // `notify_one` 会把许可留下来：晚到的等待者也能立刻返回（不丢唤醒）。
            recorder.callback.notify_one();
        }
    }
    (*out).write(b"");
    STATUS_OK
}

// ------------------------------------------------------------ 夹具

struct Fixture {
    app: Option<cordis_core::App>,
    handle: u64,
    instance: Instance,
    recorder: Arc<Recorder>,
    _hooks: Vec<Disposable>,
}

impl Fixture {
    async fn new(error_format: u32) -> Self {
        let app = cordis_core::App::new();
        apply_via_with_keepalive_and_format(
            record,
            app.root.clone(),
            Value::Null,
            "panic-close",
            None,
            error_format,
        )
        .await
        .expect("假插件必须装载成功");
        let handle = instance_handle_of(&app.root).expect("装载后必须有实例句柄");
        let instance = instances()
            .read()
            .unwrap()
            .get(handle)
            .expect("实例必须可解析");
        let recorder = Arc::new(Recorder::default());
        recorders().lock().unwrap().insert(handle, recorder.clone());
        Self {
            app: Some(app),
            handle,
            instance,
            recorder,
            _hooks: Vec::new(),
        }
    }

    /// 提供一个"一 poll 就 panic / 等到闸门再 panic"的接口实现。
    fn provide_boom(&mut self, gate: Option<Arc<tokio::sync::Notify>>) {
        let disposable = self
            .app
            .as_ref()
            .unwrap()
            .root
            .provide_iface("boom@1", Arc::new(PanickingService { gate }))
            .expect("provide_iface 必须成功");
        self._hooks.push(disposable);
    }

    /// 在根 ctx 上挂一个 `boom` 监听器（闭包由调用方给出：panic 注入点）。
    fn on_boom(
        &mut self,
        handler: impl Fn(cordis_core::Call) -> BoxFuture<Ret> + Send + Sync + 'static,
    ) {
        let disposable = self.app.as_ref().unwrap().root.on("boom", handler);
        self._hooks.push(disposable);
    }

    fn dispatches(&self) -> usize {
        self.instance.dispatches.lock().unwrap().len()
    }

    fn tasks(&self) -> usize {
        self.instance.tasks.lock().unwrap().len()
    }

    fn events(&self) -> Vec<(u32, Value)> {
        self.recorder.events.lock().unwrap().clone()
    }

    fn callbacks(&self) -> Vec<Value> {
        self.events()
            .into_iter()
            .filter(|(op, _)| *op == OP_CALLBACK)
            .map(|(_, payload)| payload)
            .collect()
    }

    fn disposes(&self) -> usize {
        self.events()
            .iter()
            .filter(|(op, _)| *op == OP_DISPOSE)
            .count()
    }

    async fn close(&mut self) {
        self.app.take().unwrap().close().await;
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        recorders().lock().unwrap().remove(&self.handle);
    }
}

/// 一个在 poll 时 panic 的接口提供者。
struct PanickingService {
    /// `None` = 首 poll 就 panic；`Some` = 先等闸门，再在后台 poll 里 panic。
    gate: Option<Arc<tokio::sync::Notify>>,
}

impl ExternService for PanickingService {
    fn service(&self) -> &str {
        "boom@1"
    }

    fn version(&self) -> u32 {
        1
    }

    fn methods(&self) -> Vec<String> {
        vec!["go".to_string()]
    }

    fn call(
        &self,
        _caller_meta: Option<Config>,
        _method: String,
        _args: Config,
    ) -> BoxFuture<Result<Config>> {
        let gate = self.gate.clone();
        Box::pin(async move {
            if let Some(gate) = gate {
                gate.notified().await;
            }
            panic!("iface 提供者在 poll 时 panic（测试注入）");
        })
    }
}

// ------------------------------------------------------------ 调用与等待工具

type Outcome = (i32, String);

fn call_iface(handle: u64, service: &str, method: &str, continuation: u64) -> Outcome {
    let api = host_api()
        .ctx_call_iface
        .expect("宿主函数表必须带 ctx_call_iface");
    let mut buffer = vec![0u8; 4096];
    let mut out = Out {
        ptr: buffer.as_mut_ptr(),
        cap: buffer.len(),
        len: 0,
    };
    let status = unsafe {
        api(
            handle,
            Blob::new(service.as_bytes()),
            1,
            Blob::new(method.as_bytes()),
            Blob::new(b"[]"),
            continuation,
            &mut out,
        )
    };
    (status, text_of(&buffer, out.len))
}

fn call_dispatch(handle: u64, event: &str, session: &str, continuation: u64) -> Outcome {
    let api = host_api()
        .ctx_dispatch
        .expect("宿主函数表必须带 ctx_dispatch");
    let mut buffer = vec![0u8; 4096];
    let mut out = Out {
        ptr: buffer.as_mut_ptr(),
        cap: buffer.len(),
        len: 0,
    };
    let status = unsafe {
        api(
            handle,
            Blob::new(event.as_bytes()),
            Blob::new(session.as_bytes()),
            continuation,
            &mut out,
        )
    };
    (status, text_of(&buffer, out.len))
}

fn call_emit(handle: u64, event: &str, session: &str) -> Outcome {
    let api = host_api().ctx_emit.expect("宿主函数表必须带 ctx_emit");
    let mut buffer = vec![0u8; 4096];
    let mut out = Out {
        ptr: buffer.as_mut_ptr(),
        cap: buffer.len(),
        len: 0,
    };
    let status = unsafe {
        api(
            handle,
            Blob::new(event.as_bytes()),
            Blob::new(session.as_bytes()),
            0,
            &mut out,
        )
    };
    (status, text_of(&buffer, out.len))
}

fn call_next(handle: u64, dispatch_id: u64, on_done: u64) -> Outcome {
    let api = host_api().ctx_next.expect("宿主函数表必须带 ctx_next");
    let mut buffer = vec![0u8; 4096];
    let mut out = Out {
        ptr: buffer.as_mut_ptr(),
        cap: buffer.len(),
        len: 0,
    };
    let status = unsafe { api(handle, dispatch_id, Blob::new(b"[]"), on_done, &mut out) };
    (status, text_of(&buffer, out.len))
}

fn text_of(buffer: &[u8], len: usize) -> String {
    String::from_utf8_lossy(&buffer[..len.min(buffer.len())]).into_owned()
}

fn structured(outcome: &Outcome) -> Value {
    serde_json::from_str(&outcome.1).unwrap_or_else(|err| {
        panic!(
            "期望结构化错误（status {}），实际正文不是 JSON: {}（{err}）",
            outcome.0, outcome.1
        )
    })
}

/// 结构化错误的人类可读正文（线格式里叫 `error`）。
fn error_text(payload: &Value) -> &str {
    payload["error"].as_str().unwrap_or_default()
}

/// 断言文案来自 **O3 的 poll 边界**，而不是 `extern "C"` 的兜底。
///
/// 两段文案只差场景词：O3 是「宿主在驱动异步操作时 panic（{scene}，已在
/// poll 边界截获）」，ABI 兜底是「宿主内部 panic 已在 ABI 边界截获」。所以
/// 「（{scene}，」只会出现在 O3 路径上——注入的 panic 文案不含这个形态，
/// 删掉 O3 的 `CatchUnwind` 之后这条断言必红。
fn assert_poll_boundary(text: &str, scene: &str) {
    assert!(
        text.contains(&format!("（{scene}，")) && text.contains("已在 poll 边界截获"),
        "错误必须由 O3 的 poll 边界结算，落到 ABI 兜底就是覆盖缺口（scene={scene}）：{text}"
    );
}

/// 只用作看门狗：真正的同步靠 `Notify` 与 `yield_now`。
async fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while !ready() {
        assert!(
            std::time::Instant::now() < deadline,
            "等 {what} 超时（看门狗 2s）"
        );
        tokio::task::yield_now().await;
    }
}

/// 按 `(handle, scene)` 取诊断：并行测试各用各的实例句柄，互不串扰。
fn notes_for(handle: u64, scene: &'static str) -> Vec<PanicNote> {
    panic_notes()
        .lock()
        .unwrap()
        .iter()
        .filter(|note| note.handle == handle && note.scene == scene)
        .cloned()
        .collect()
}

/// 在实例里放一条带 `frame` 的在飞记录（`ctx_next` 的入站侧）。
fn insert_frame(instance: &Instance, dispatch_id: u64, frame: Option<Next>) {
    insert_dispatch(
        instance,
        dispatch_id,
        DispatchRecord {
            sink: None,
            frame,
            parent: None,
            outbound: None,
            stream: None,
        },
    )
    .unwrap_or_else(|_| panic!("admission"));
}

// ------------------------------------------------------------ 红测：iface

#[tokio::test]
async fn o3_iface_first_poll_panic_returns_structured_error_without_residue() {
    let mut f = Fixture::new(cordis_abi::ERROR_FORMAT_V1).await;
    f.provide_boom(None);

    let outcome = call_iface(f.handle, "boom@1", "go", 99);
    assert_eq!(outcome.0, STATUS_ERR, "首 poll panic 必须是可读失败");
    let payload = structured(&outcome);
    assert_eq!(payload["code"], "host_panic", "错误必须带稳定 code");
    assert_eq!(
        payload["certainty"], "unknown",
        "宿主无法证明副作用没发生，绝不能写 not_executed"
    );
    assert_eq!(payload["category"], "unknown", "分类保持默认 unknown");
    assert_eq!(payload["retry"], "unknown", "重试事实同样不能猜：{payload}");
    assert_poll_boundary(error_text(&payload), "iface");
    assert_eq!(f.tasks(), 0, "首 poll panic 后任务必须退出");
    assert_eq!(
        f.dispatches(),
        0,
        "首 poll panic 不得留下永远不结算的登记记录"
    );
    let notes = notes_for(f.handle, "iface");
    assert_eq!(notes.len(), 1, "首 poll panic 必须留下一条诊断：{notes:?}");
    assert!(notes[0].message.contains("panic"), "诊断要能定位");
    assert!(
        notes_for(f.handle, "on_done").is_empty(),
        "首 poll panic 之后 on_done 一行都不该跑"
    );

    f.close().await;
    assert_eq!(
        f.callbacks().len(),
        0,
        "调用方已经同步拿到错误，关闭时绝不能再补一次 abort：{:?}",
        f.callbacks()
    );
    assert_eq!(f.disposes(), 1, "插件仍然只收到一次 OP_DISPOSE");
}

#[tokio::test]
async fn o3_iface_background_panic_delivers_exactly_one_iface_error() {
    let mut f = Fixture::new(cordis_abi::ERROR_FORMAT_V1).await;
    let gate = Arc::new(tokio::sync::Notify::new());
    f.provide_boom(Some(gate.clone()));

    let outcome = call_iface(f.handle, "boom@1", "go", 99);
    assert_eq!(outcome.0, STATUS_OK, "未就绪时应当挂起而不是失败");
    let pending: Value = serde_json::from_str(&outcome.1).expect("挂起响应应当是 JSON");
    assert_eq!(pending["pending"], true, "挂起响应必须带 pending:true");
    assert_eq!(f.tasks(), 1, "后台任务必须已登记");
    assert_eq!(f.dispatches(), 1, "挂起期间必须有在飞记录");

    gate.notify_one();
    wait_until("后台 panic 交付", || !f.callbacks().is_empty()).await;

    let callbacks = f.callbacks();
    assert_eq!(
        callbacks.len(),
        1,
        "后台 panic 必须恰好交付一条回调：{callbacks:?}"
    );
    let payload = &callbacks[0];
    assert_eq!(payload["kind"], KIND_IFACE);
    assert_eq!(payload["id"], 99);
    assert_eq!(payload["args"][0]["code"], "host_panic");
    assert_eq!(
        payload["args"][0]["certainty"], "unknown",
        "后台 panic 同样不能声称 not_executed"
    );
    assert_eq!(
        payload["args"][0]["retry"], "unknown",
        "后台 panic 同样不能声称重试安全：{payload}"
    );
    assert_poll_boundary(error_text(&payload["args"][0]), "iface");
    wait_until("后台任务退出", || f.tasks() == 0).await;
    assert_eq!(f.dispatches(), 0, "交付后必须撤销在飞记录");
    assert_eq!(
        notes_for(f.handle, "iface").len(),
        1,
        "后台 poll panic 必须留下恰好一条诊断"
    );

    f.close().await;
    assert_eq!(
        f.callbacks().len(),
        1,
        "关闭不得补发第二条：{:?}",
        f.callbacks()
    );
    assert_eq!(f.disposes(), 1);
}

#[tokio::test]
async fn o3_iface_error_format_zero_panic_stays_plain_text() {
    let mut f = Fixture::new(0).await;
    f.provide_boom(None);

    let outcome = call_iface(f.handle, "boom@1", "go", 99);
    assert_eq!(outcome.0, STATUS_ERR);
    assert!(
        outcome.1.contains("panic"),
        "纯文本降级至少要说清是 panic：{}",
        outcome.1
    );
    // error_format=0 只剩这一条正文：正好用它把 O3 与 ABI 兜底区分开。
    assert_poll_boundary(&outcome.1, "iface");

    assert!(
        !outcome.1.contains("\"code\""),
        "error_format=0 不得塞结构化字段：{}",
        outcome.1
    );
    assert_eq!(f.dispatches(), 0, "纯文本路径同样不得留下记录");
    f.close().await;
    assert_eq!(f.callbacks().len(), 0);
}

// ------------------------------------------------------------ 红测：dispatch

#[tokio::test]
async fn o3_dispatch_first_poll_panic_unregisters_and_reports() {
    let mut f = Fixture::new(cordis_abi::ERROR_FORMAT_V1).await;
    f.on_boom(|_call| {
        Box::pin(async move { panic!("dispatch 监听器首 poll panic（测试注入）") })
    });

    let outcome = call_dispatch(f.handle, "boom", r#"{"args":[],"mode":"bail"}"#, 77);
    assert_eq!(outcome.0, STATUS_ERR, "首 poll panic 必须同步返回错误");
    let payload = structured(&outcome);
    assert_eq!(payload["code"], "host_panic");
    assert_eq!(payload["category"], "unknown");
    assert_eq!(payload["certainty"], "unknown");
    assert_eq!(payload["retry"], "unknown", "重试事实同样不能猜：{payload}");
    assert_poll_boundary(error_text(&payload), "dispatch");
    assert_eq!(f.tasks(), 0, "首 poll panic 后任务必须退出");
    assert_eq!(f.dispatches(), 0, "首 poll panic 必须先摘表再返回错误");
    let notes = notes_for(f.handle, "dispatch");
    assert_eq!(
        notes.len(),
        1,
        "首 poll panic 必须留下恰好一条 dispatch 诊断：{notes:?}"
    );

    f.close().await;
    assert_eq!(
        f.callbacks().len(),
        0,
        "同步错误之外不得再补一条 abort：{:?}",
        f.callbacks()
    );
    assert_eq!(f.disposes(), 1);
}

#[tokio::test]
async fn o3_dispatch_background_panic_delivers_exactly_one_dispatch_error() {
    let mut f = Fixture::new(cordis_abi::ERROR_FORMAT_V1).await;
    let gate = Arc::new(tokio::sync::Notify::new());
    f.on_boom({
        let gate = gate.clone();
        move |_call| {
            let gate = gate.clone();
            Box::pin(async move {
                gate.notified().await;
                panic!("dispatch 监听器后台 poll panic（测试注入）")
            })
        }
    });

    let outcome = call_dispatch(f.handle, "boom", r#"{"args":[],"mode":"bail"}"#, 77);
    assert_eq!(outcome.0, STATUS_OK, "未就绪时应当挂起：{}", outcome.1);
    let pending: Value = serde_json::from_str(&outcome.1).expect("挂起响应应当是 JSON");
    assert_eq!(pending["pending"], true, "挂起响应必须带 pending:true");
    assert_eq!(f.tasks(), 1);
    assert_eq!(f.dispatches(), 1);

    gate.notify_one();
    wait_until("后台 panic 交付", || !f.callbacks().is_empty()).await;

    let callbacks = f.callbacks();
    assert_eq!(callbacks.len(), 1, "必须恰好一条：{callbacks:?}");
    assert_eq!(callbacks[0]["kind"], KIND_DISPATCH);
    assert_eq!(callbacks[0]["id"], 77);
    assert_eq!(callbacks[0]["args"][0]["code"], "host_panic");
    assert_eq!(callbacks[0]["args"][0]["certainty"], "unknown");
    assert_eq!(
        callbacks[0]["args"][0]["retry"], "unknown",
        "后台 panic 同样不能声称重试安全：{callbacks:?}"
    );
    assert_poll_boundary(error_text(&callbacks[0]["args"][0]), "dispatch");
    wait_until("后台任务退出", || f.tasks() == 0).await;
    assert_eq!(f.dispatches(), 0, "交付后必须撤销在飞记录");
    assert_eq!(
        notes_for(f.handle, "dispatch").len(),
        1,
        "后台 poll panic 必须留下恰好一条诊断"
    );

    f.close().await;
    assert_eq!(f.callbacks().len(), 1, "关闭不得补发第二条");
    assert_eq!(f.disposes(), 1);
}

// ------------------------------------------------------------ 红测：next

#[tokio::test]
async fn o3_next_background_panic_delivers_once_and_keeps_inbound_record() {
    let mut f = Fixture::new(cordis_abi::ERROR_FORMAT_V1).await;
    let gate = Arc::new(tokio::sync::Notify::new());
    insert_frame(
        &f.instance,
        97,
        Some(Next::some({
            let gate = gate.clone();
            move |_args| {
                let gate = gate.clone();
                Box::pin(async move {
                    gate.notified().await;
                    panic!("waterfall 剩余链后台 poll panic（测试注入）")
                })
            }
        })),
    );

    let outcome = call_next(f.handle, 97, 98);
    assert_eq!(outcome.0, STATUS_OK, "首次 next 应当挂起：{}", outcome.1);
    assert_eq!(f.tasks(), 1);
    assert_eq!(f.dispatches(), 1);

    gate.notify_one();
    wait_until("后台 panic 交付", || !f.callbacks().is_empty()).await;

    let callbacks = f.callbacks();
    assert_eq!(callbacks.len(), 1, "必须恰好一条：{callbacks:?}");
    assert_eq!(callbacks[0]["kind"], KIND_NEXT);
    assert_eq!(callbacks[0]["id"], 98);
    assert_eq!(callbacks[0]["args"][0]["code"], "host_panic");
    assert_eq!(callbacks[0]["args"][0]["certainty"], "unknown");
    assert_eq!(
        callbacks[0]["args"][0]["retry"], "unknown",
        "后台 panic 同样不能声称重试安全：{callbacks:?}"
    );
    assert_poll_boundary(error_text(&callbacks[0]["args"][0]), "next");
    wait_until("后台任务退出", || f.tasks() == 0).await;
    assert_eq!(
        f.dispatches(),
        1,
        "next 不摘入站记录（它由事件派发自己收尾）"
    );
    assert_eq!(
        notes_for(f.handle, "next").len(),
        1,
        "后台 poll panic 必须留下恰好一条诊断"
    );

    // 该记录没有出站续延：清掉它，避免关闭时的排水把断言搅浑。
    f.instance.dispatches.lock().unwrap().remove(&97);
    f.close().await;
    assert_eq!(f.callbacks().len(), 1, "关闭不得补发第二条");
    assert_eq!(f.disposes(), 1);
}

/// 保护测试：**旧码已经正确**，这里钉住"不要因为 O3 而漂移"。
///
/// 不是红测：`dispatches() == 1` 那条是旧码既有行为。本用例新增的只有
/// 诊断（`notes_for(_, "next")`）与 O3 文案，这两项在旧码上确实不会成立，
/// 但它们的意义是"别漂移到 ABI 兜底"，不是"证明 O3 修好了什么"。
#[tokio::test]
async fn o3_next_first_poll_panic_is_reported_without_removing_record() {
    let mut f = Fixture::new(cordis_abi::ERROR_FORMAT_V1).await;
    insert_frame(
        &f.instance,
        97,
        Some(Next::some(|_args| {
            Box::pin(async move { panic!("waterfall 剩余链首 poll panic（测试注入）") })
        })),
    );

    let outcome = call_next(f.handle, 97, 98);
    assert_eq!(outcome.0, STATUS_ERR, "首 poll panic 必须同步失败");
    let payload = structured(&outcome);
    assert_eq!(payload["code"], "host_panic");
    assert_eq!(payload["certainty"], "unknown");
    assert_eq!(payload["retry"], "unknown", "重试事实同样不能猜：{payload}");
    assert_poll_boundary(error_text(&payload), "next");
    assert_eq!(f.tasks(), 0, "首 poll panic 后任务必须退出");
    assert_eq!(
        f.dispatches(),
        1,
        "next 不摘入站记录：它由事件派发/取消路径收尾"
    );
    let notes = notes_for(f.handle, "next");
    assert_eq!(
        notes.len(),
        1,
        "首 poll panic 必须留下恰好一条 next 诊断：{notes:?}"
    );
    assert_eq!(f.callbacks().len(), 0, "同步错误之外不得补发回调");

    f.instance.dispatches.lock().unwrap().remove(&97);
    f.close().await;
    assert_eq!(f.callbacks().len(), 0);
    assert_eq!(f.disposes(), 1);
}

// ------------------------------------------------------------ 红测：emit

#[tokio::test]
async fn o3_emit_panic_cleans_record_and_never_fakes_callback() {
    let mut f = Fixture::new(cordis_abi::ERROR_FORMAT_V1).await;
    f.on_boom(|_call| Box::pin(async move { panic!("emit 监听器 panic（测试注入）") }));

    let outcome = call_emit(f.handle, "boom", r#"{"args":[],"mode":"emit"}"#);
    assert_eq!(outcome.0, STATUS_OK, "emit 立即返回：{}", outcome.1);
    assert_eq!(f.tasks(), 1, "emit 的后台任务已登记");

    wait_until("emit 后台任务退出", || {
        f.tasks() == 0 && f.dispatches() == 0
    })
    .await;

    assert!(
        f.callbacks().is_empty(),
        "纯 emit 没有续延：panic 不得伪造任何回调：{:?}",
        f.callbacks()
    );
    assert_eq!(f.dispatches(), 0, "emit panic 后记录必须被摘掉，不能吃配额");
    assert_eq!(
        notes_for(f.handle, "emit").len(),
        1,
        "emit 的 panic 必须留下恰好一条诊断"
    );

    f.close().await;
    assert!(f.callbacks().is_empty(), "关闭也不得补发回调");
    assert_eq!(f.disposes(), 1);
}

// ------------------------------------------------------------ on_done 自身的 panic

/// `on_done` 是宿主的交付闭包：它 panic 也是"宿主内部事故"，
/// 必须留下诊断、不伪造任何回调，并且**不改变任务退出发布**。
///
/// 这条没有红阶段：修复前后的业务可观测量完全相同（任务退出、无回调），
/// 唯一新增的观测量就是诊断本身。这里不谎称它是旧码红测。
#[tokio::test]
async fn o3_on_done_panic_is_diagnosed_without_fake_callback() {
    let mut f = Fixture::new(cordis_abi::ERROR_FORMAT_V1).await;
    let (cancel, done) = insert_dispatch_task(
        &f.instance,
        96,
        DispatchRecord {
            sink: None,
            frame: None,
            parent: None,
            outbound: None,
            stream: None,
        },
    )
    .unwrap_or_else(|_| panic!("admission"));

    let (tx, rx) = tokio::sync::oneshot::channel();
    let future: BoxFuture<()> = Box::pin(async {
        tokio::task::yield_now().await;
    });
    let outcome = complete_or_defer(
        future,
        move |_| {
            // 先发信号、再 panic：测试不需要 sleep 也知道 on_done 跑到了。
            let _ = tx.send(());
            panic!("on_done 里 panic（测试注入）");
        },
        |_payload| panic!("正常完成时不得调用 on_panic"),
        (cancel, done),
        PanicScope {
            scene: "test",
            handle: f.handle,
            dispatch_id: 96,
        },
    );
    assert!(
        matches!(outcome, Ok(None)),
        "未就绪时必须交给运行时，而不是就地失败"
    );

    rx.await.expect("on_done 必须被执行");
    wait_until("on_done panic 后的任务退出", || f.tasks() == 0).await;

    let notes = notes_for(f.handle, "on_done");
    assert_eq!(
        notes.len(),
        1,
        "on_done 的 panic 必须留下恰好一条诊断：{notes:?}"
    );
    assert!(
        notes[0].message.contains("on_done"),
        "诊断要能定位到场景：{}",
        notes[0].message
    );
    assert_eq!(notes[0].dispatch_id, 96, "诊断要能定位到这次派发");
    assert!(
        f.callbacks().is_empty(),
        "on_done 自己 panic 不得被转成一条伪造回调：{:?}",
        f.callbacks()
    );

    // 该记录没有出站续延：清掉它，避免关闭时的排水把断言搅浑。
    f.instance.dispatches.lock().unwrap().remove(&96);
    f.close().await;
    assert!(f.callbacks().is_empty());
    assert_eq!(f.disposes(), 1);
}
