//! 流（ADR 0001 §5）：窗口 / 背压 / close / fail / drop / 单件流。
//!
//! 这一层**没有 ABI、没有宿主**：`StreamSink` / `AsyncStream` 是"一次调用产出多块"
//! 的唯一设施——T1 插件直接用它的异步一半（`send().await` 自己等窗口，零 ABI 成本），
//! 宿主侧的 T2 流建在它的非阻塞一半（`push`）上。
//!
//! 语义契约（本文件逐条钉死）：
//! - **窗口**：缓冲里最多 [`STREAM_WINDOW`] 块；窗口满了再推 ⇒ [`PushOutcome::Pending`]
//!   （不阻塞、不扩容、不丢块）；
//! - **credit**：消费者每取走一块 ⇒ 恰好唤醒一个等待中的推送者（登记的回调 id 交给
//!   `StreamObserver`）；
//! - **收尾恰好一次**：close / fail / 丢弃 三者谁先到谁生效；
//! - **close 不丢缓冲**（排空后给 `Ok(None)`），**fail 立刻给 `Err`**（缓冲丢弃）；
//! - **消费者离场** ⇒ 提供者的下一次推送拿到 `Gone`，等待中的 credit 回调也被驳回。

use cordis_core::{
    plugin_fn, App, BoxFuture, Config, Ctx, ExternService, Inject, PushOutcome, Result,
    StreamObserver, StreamOutcome, StreamSink, STREAM_WINDOW,
};
use serde_json::json;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{oneshot, Notify};

/// 宿主侧（或别的外部设施）看到的流事件。core 不依赖 ABI，所以这里只认事件。
#[derive(Default)]
struct Recorder {
    credits: Mutex<Vec<(u64, usize)>>,
    finished: Mutex<Vec<(String, Vec<u64>)>>,
}

impl StreamObserver for Recorder {
    fn credit_available(&self, on_credit: u64, window: usize) {
        self.credits.lock().unwrap().push((on_credit, window));
    }

    fn finished(&self, outcome: &StreamOutcome, waiting: Vec<u64>) {
        self.finished
            .lock()
            .unwrap()
            .push((format!("{outcome:?}"), waiting));
    }
}

fn recorder(sink: &StreamSink) -> Arc<Recorder> {
    let recorder = Arc::new(Recorder::default());
    sink.observe(recorder.clone());
    recorder
}

/// `fail`/`close` 必须唤醒一个正卡在满窗口上的 `send`；否则提供者任务会永久挂起。
#[tokio::test]
async fn fail_wakes_a_send_blocked_on_a_full_window() {
    let (sink, mut stream) = StreamSink::new(1);
    assert_eq!(sink.send(json!(1)).await, PushOutcome::Sent);

    let sender = sink.clone();
    let blocked = tokio::spawn(async move { sender.send(json!(2)).await });
    // 给 send 一点时间真正进入“等窗口”状态。
    tokio::time::sleep(Duration::from_millis(20)).await;

    sink.fail("boom");
    let outcome = tokio::time::timeout(Duration::from_secs(1), blocked)
        .await
        .expect("fail 必须唤醒阻塞中的 send")
        .expect("send task 不应 panic");
    assert_eq!(outcome, PushOutcome::Gone, "流已收尾，块不能被接受");
    assert!(stream.next().await.is_err(), "消费者必须先读到失败原因");
}

/// 收尾是**广播**：所有克隆出来的 `send` 都可能同时卡在满窗口上。
#[tokio::test]
async fn close_wakes_every_send_blocked_on_a_full_window() {
    let (sink, mut stream) = StreamSink::new(1);
    assert_eq!(sink.send(json!(1)).await, PushOutcome::Sent);

    let first = tokio::spawn({
        let sink = sink.clone();
        async move { sink.send(json!(2)).await }
    });
    let second = tokio::spawn({
        let sink = sink.clone();
        async move { sink.send(json!(3)).await }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(
        !first.is_finished() && !second.is_finished(),
        "两个发送者都应先被窗口挡住"
    );

    sink.close();
    for sender in [first, second] {
        let outcome = tokio::time::timeout(Duration::from_secs(1), sender)
            .await
            .expect("close 必须唤醒所有阻塞中的 send")
            .expect("send task 不应 panic");
        assert_eq!(outcome, PushOutcome::Gone, "流已收尾，块不能被接受");
    }

    // 消费者在等待期间没有取块：唤醒不依赖它继续读。
    assert_eq!(stream.next().await.unwrap(), Some(json!(1)));
    assert_eq!(stream.next().await.unwrap(), None);
}

#[tokio::test]
async fn pushes_stop_at_the_window_and_a_credit_wakes_exactly_one_waiter() {
    let (sink, mut stream) = StreamSink::new(STREAM_WINDOW);
    let recorder = recorder(&sink);

    for index in 1..=STREAM_WINDOW {
        assert_eq!(
            sink.push(json!(index)),
            PushOutcome::Sent,
            "窗口内的第 {index} 块必须被接受"
        );
    }
    assert_eq!(
        sink.push_awaiting(json!(99), 42),
        PushOutcome::Pending,
        "满窗口必须挡住推送"
    );
    assert_eq!(
        sink.push_awaiting(json!(100), 43),
        PushOutcome::Pending,
        "继续推仍然被挡（不会悄悄扩容）"
    );
    assert!(
        recorder.credits.lock().unwrap().is_empty(),
        "消费者还没取走东西，credit 不该提前到"
    );

    assert_eq!(stream.next().await.unwrap(), Some(json!(1)));
    assert_eq!(
        recorder.credits.lock().unwrap().clone(),
        vec![(42, 1)],
        "取走一块 ⇒ 恰好唤醒一个等待者，并报出可用窗口"
    );

    assert_eq!(
        sink.push(json!(5)),
        PushOutcome::Sent,
        "腾出来的位置确实可用"
    );
    assert_eq!(sink.buffered(), STREAM_WINDOW, "缓冲永远不超过窗口");
}

#[tokio::test]
async fn close_delivers_the_buffer_then_ends_cleanly() {
    let (sink, mut stream) = StreamSink::new(STREAM_WINDOW);
    let recorder = recorder(&sink);
    assert_eq!(sink.push(json!(1)), PushOutcome::Sent);
    assert_eq!(sink.push(json!(2)), PushOutcome::Sent);
    assert_eq!(sink.close(), PushOutcome::Sent);

    assert_eq!(
        stream.next().await.unwrap(),
        Some(json!(1)),
        "正常收尾不丢已经缓冲的块"
    );
    assert_eq!(stream.next().await.unwrap(), Some(json!(2)));
    assert_eq!(
        stream.next().await.unwrap(),
        None,
        "收尾之后是 Ok(None)，不是错误"
    );
    assert_eq!(sink.close(), PushOutcome::Gone, "重复收尾是幂等的 Gone");
    assert_eq!(sink.push(json!(3)), PushOutcome::Gone, "收尾之后不许再推");
    assert_eq!(recorder.finished.lock().unwrap().len(), 1, "收尾恰好一次");
}

#[tokio::test]
async fn fail_ends_the_stream_with_the_error_and_drops_the_buffer() {
    let (sink, mut stream) = StreamSink::new(STREAM_WINDOW);
    let recorder = recorder(&sink);
    sink.push(json!(1));
    sink.push(json!(2));
    assert_eq!(sink.fail("boom"), PushOutcome::Sent);

    let err = stream.next().await.expect_err("失败必须以错误结束");
    assert!(err.to_string().contains("boom"), "{err}");
    assert_eq!(
        recorder.finished.lock().unwrap().len(),
        1,
        "失败也恰好收尾一次"
    );
}

#[tokio::test]
async fn a_sink_that_is_dropped_ends_the_stream_cleanly() {
    let (sink, mut stream) = StreamSink::new(STREAM_WINDOW);
    sink.push(json!(1));
    drop(sink);
    assert_eq!(stream.next().await.unwrap(), Some(json!(1)));
    assert_eq!(
        stream.next().await.unwrap(),
        None,
        "提供者丢了手柄 ⇒ 流正常结束（不是错误）"
    );
}

#[tokio::test]
async fn dropping_the_consumer_marks_the_stream_gone_and_hands_waiting_ids_to_the_observer() {
    let (sink, stream) = StreamSink::new(STREAM_WINDOW);
    let recorder = recorder(&sink);
    for index in 1..=STREAM_WINDOW {
        sink.push(json!(index));
    }
    assert_eq!(sink.push_awaiting(json!(9), 7), PushOutcome::Pending);

    drop(stream);

    assert_eq!(
        sink.push(json!(10)),
        PushOutcome::Gone,
        "消费者走了 ⇒ 推送立刻拿到 Gone"
    );
    let finished = recorder.finished.lock().unwrap().clone();
    assert_eq!(finished.len(), 1, "离场也恰好收尾一次");
    assert!(finished[0].0.contains("Abandoned"), "{finished:?}");
    assert_eq!(
        finished[0].1,
        vec![7],
        "等待中的 credit 回调必须交给宿主去驳回"
    );
}

/// 失败与数据同时就绪时，失败必须赢：消费者不能读到"已经失败之后"的块。
///
/// 这个交错在旧实现里是 select 的随机分支：检查完 fault 后 future 被挂起，
/// 生产者 push+fail 同时把所有分支变成就绪，下一次轮询抽到 recv 就把块交出去。
#[tokio::test(flavor = "current_thread")]
async fn fail_wins_over_buffered_data_when_both_are_ready() {
    for round in 0..200 {
        let (sink, mut stream) = StreamSink::new(STREAM_WINDOW);
        let next = tokio::spawn(async move { stream.next().await });
        // 让消费者真正挂进 select（current_thread 上这里没有别的任务）。
        tokio::time::sleep(Duration::from_millis(1)).await;

        sink.push(json!(1));
        sink.fail("boom");

        let outcome = tokio::time::timeout(Duration::from_secs(1), next)
            .await
            .expect("消费者必须被唤醒")
            .expect("next 不得 panic");
        assert!(
            outcome.is_err(),
            "第 {round} 轮：流已经失败，next 却交付了数据：{outcome:?}"
        );
    }
}

/// `close` 之后，已经等窗口的 `send` 不得再把块提交进缓冲。
///
/// 旧实现的 `select!` 在"窗口腾出位置"与"收尾通知"同时就绪时可能选数据分支；
/// 消费者已经读到 EOF，下一次 `next()` 又会看到这块数据——结束契约被倒置。
#[tokio::test(flavor = "current_thread")]
async fn close_prevents_a_pending_send_from_committing_after_eof() {
    for round in 0..200 {
        let (sink, mut stream) = StreamSink::new(1);
        assert_eq!(sink.push(json!(1)), PushOutcome::Sent);

        let sender = tokio::spawn({
            let sink = sink.clone();
            async move { sink.send(json!(2)).await }
        });
        tokio::time::sleep(Duration::from_millis(1)).await;
        assert!(
            !sender.is_finished(),
            "第 {round} 轮：窗口满时 send 必须先挂起"
        );

        assert_eq!(stream.next().await.unwrap(), Some(json!(1)));
        // 同一个 poll 里关流：此时 sender 的"位置"与"收尾"两个分支都已就绪。
        sink.close();

        let outcome = tokio::time::timeout(Duration::from_secs(1), sender)
            .await
            .expect("send 必须被收尾唤醒")
            .expect("send 不得 panic");
        assert_eq!(
            outcome,
            PushOutcome::Gone,
            "第 {round} 轮：close 之后挂起的 send 仍提交了数据"
        );
        assert_eq!(
            stream.next().await.unwrap(),
            None,
            "第 {round} 轮：消费者已经读到 EOF，不得再出现数据"
        );
    }
}

#[tokio::test]
async fn send_waits_for_room_instead_of_failing() {
    let (sink, mut stream) = StreamSink::new(2);
    assert_eq!(sink.send(json!(1)).await, PushOutcome::Sent);
    assert_eq!(sink.send(json!(2)).await, PushOutcome::Sent);

    let pushing = tokio::spawn({
        let sink = sink.clone();
        async move { sink.send(json!(3)).await }
    });
    tokio::task::yield_now().await;
    assert!(!pushing.is_finished(), "窗口满时 send 必须等，而不是失败");

    assert_eq!(stream.next().await.unwrap(), Some(json!(1)));
    assert_eq!(
        pushing.await.unwrap(),
        PushOutcome::Sent,
        "腾出位置之后 send 立刻完成"
    );
    assert_eq!(stream.next().await.unwrap(), Some(json!(2)));
    assert_eq!(stream.next().await.unwrap(), Some(json!(3)));
}

// ---------------------------------------------------------------- 服务侧（默认实现与 T1）

/// 只会返回值的服务：不实现 `call_stream` ⇒ 走默认实现（长度 1 的流）。
struct ValueProvider;

impl ExternService for ValueProvider {
    fn service(&self) -> &str {
        "svc"
    }

    fn version(&self) -> u32 {
        1
    }

    fn methods(&self) -> Vec<String> {
        vec!["get".to_string()]
    }

    fn call(
        &self,
        _caller_meta: Option<Config>,
        method: String,
        _args: Config,
    ) -> BoxFuture<Result<Config>> {
        Box::pin(async move {
            if method != "get" {
                return Err(format!("没有方法 {method}").into());
            }
            Ok(json!({ "v": 1 }))
        })
    }
}

/// 跨窗口推 6 块的服务：`send().await` 自己等窗口——T1 的背压写法，零 ABI。
struct ChunkProvider;

impl ExternService for ChunkProvider {
    fn service(&self) -> &str {
        "svc"
    }

    fn version(&self) -> u32 {
        1
    }

    fn methods(&self) -> Vec<String> {
        vec!["chunks".to_string()]
    }

    fn call(
        &self,
        _caller_meta: Option<Config>,
        _method: String,
        _args: Config,
    ) -> BoxFuture<Result<Config>> {
        Box::pin(async move { Err("这个服务只能按流调用".into()) })
    }

    fn call_stream(
        &self,
        _caller_meta: Option<Config>,
        _method: String,
        _args: Config,
        sink: StreamSink,
    ) -> BoxFuture<Result<()>> {
        Box::pin(async move {
            for index in 1..=6 {
                if sink.send(json!(index)).await == PushOutcome::Gone {
                    return Ok(());
                }
            }
            sink.close();
            Ok(())
        })
    }
}

fn provider(name: &'static str, service: Arc<dyn ExternService>) -> Arc<dyn cordis_core::Plugin> {
    plugin_fn(name, Inject::default(), move |ctx: Ctx, _config: Config| {
        let service = service.clone();
        Box::pin(async move {
            ctx.provide_iface("svc", service)?;
            Ok(())
        })
    })
}

async fn app_with(name: &'static str, service: Arc<dyn ExternService>) -> App {
    let app = App::new();
    let fiber = app.root.plugin(provider(name, service), Config::Null);
    fiber.settle().await;
    assert_eq!(fiber.state(), cordis_core::FiberState::Active);
    app
}

#[tokio::test]
async fn a_plain_value_becomes_a_one_item_stream() {
    let app = app_with("value", Arc::new(ValueProvider)).await;
    let mut stream = app
        .root
        .call_iface_stream("svc", 1, "get", json!([]))
        .expect("开流");
    assert_eq!(
        stream.next().await.unwrap(),
        Some(json!({ "v": 1 })),
        "返回值就是长度 1 的流"
    );
    assert_eq!(stream.next().await.unwrap(), None);
}

#[tokio::test]
async fn a_t1_provider_can_stream_across_the_window() {
    let app = app_with("chunks", Arc::new(ChunkProvider)).await;
    let mut stream = app
        .root
        .call_iface_stream("svc", 1, "chunks", json!([]))
        .expect("开流");
    let mut seen = Vec::new();
    while let Some(chunk) = stream.next().await.unwrap() {
        seen.push(chunk);
    }
    assert_eq!(
        seen,
        (1..=6).map(|index| json!(index)).collect::<Vec<_>>(),
        "跨窗口的块必须按序到达"
    );
}

/// 一个真正的异步流提供者：先挂起，再产生一个与流无关的副作用。
///
/// 它不检查 `sink` 是否还活着——这正是旧实现泄漏后台任务的形态：
/// 丢弃流或关闭 App 都不会让它醒来。
struct SlowProvider {
    started: Arc<AtomicBool>,
    side_effect: Arc<AtomicBool>,
}

impl ExternService for SlowProvider {
    fn service(&self) -> &str {
        "svc"
    }

    fn version(&self) -> u32 {
        1
    }

    fn methods(&self) -> Vec<String> {
        vec!["slow".to_string()]
    }

    fn call(
        &self,
        _caller_meta: Option<Config>,
        _method: String,
        _args: Config,
    ) -> BoxFuture<Result<Config>> {
        Box::pin(async move { Err("这个服务只能按流调用".into()) })
    }

    fn call_stream(
        &self,
        _caller_meta: Option<Config>,
        _method: String,
        _args: Config,
        sink: StreamSink,
    ) -> BoxFuture<Result<()>> {
        let started = self.started.clone();
        let side_effect = self.side_effect.clone();
        Box::pin(async move {
            started.store(true, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(300)).await;
            side_effect.store(true, Ordering::SeqCst);
            sink.close();
            Ok(())
        })
    }
}

async fn wait_until(flag: &AtomicBool) {
    for _ in 0..200 {
        if flag.load(Ordering::SeqCst) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("等待条件超时");
}

/// `App::close` 必须取消并等待原生流任务，而不是返回后让它继续跑副作用。
#[tokio::test]
async fn app_close_cancels_and_waits_for_a_native_stream_task() {
    let started = Arc::new(AtomicBool::new(false));
    let side_effect = Arc::new(AtomicBool::new(false));
    let app = app_with(
        "slow",
        Arc::new(SlowProvider {
            started: started.clone(),
            side_effect: side_effect.clone(),
        }),
    )
    .await;

    let _stream = app
        .root
        .call_iface_stream("svc", 1, "slow", json!([]))
        .expect("开流");
    wait_until(&started).await;

    tokio::time::timeout(Duration::from_secs(2), app.close())
        .await
        .expect("App::close 不得因为等待流任务而挂起");
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        !side_effect.load(Ordering::SeqCst),
        "App::close 返回后原生流任务仍在执行副作用"
    );
}

/// 丢弃消费者同样是取消信号：提供者不能继续跑与流无关的副作用。
#[tokio::test]
async fn dropping_the_stream_cancels_its_native_provider_task() {
    let started = Arc::new(AtomicBool::new(false));
    let side_effect = Arc::new(AtomicBool::new(false));
    let app = app_with(
        "slow",
        Arc::new(SlowProvider {
            started: started.clone(),
            side_effect: side_effect.clone(),
        }),
    )
    .await;

    let stream = app
        .root
        .call_iface_stream("svc", 1, "slow", json!([]))
        .expect("开流");
    wait_until(&started).await;
    drop(stream);

    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        !side_effect.load(Ordering::SeqCst),
        "丢弃流之后提供者任务仍在执行副作用"
    );
    app.close().await;
}

/// 一个把“结果就绪”交给测试控制的 provider。
///
/// 首次轮询（`call_iface_stream` 的同步预跑）必须返回 `Pending`；测试随后
/// 丢弃流（制造取消），再放行 `oneshot`，让取消与结果在后台任务第一次轮询时
/// **同时就绪**。
struct CancelRaceProvider {
    gate: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    side_effect: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
    /// `true`：先让后台任务跑到自己的挂起点，再制造“取消 + 结果”同时就绪。
    enter_first: bool,
    entered: Arc<Notify>,
}

struct DropFlag(Arc<AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

impl ExternService for CancelRaceProvider {
    fn service(&self) -> &str {
        "svc"
    }

    fn version(&self) -> u32 {
        1
    }

    fn methods(&self) -> Vec<String> {
        vec!["race".to_string()]
    }

    fn call(
        &self,
        _caller_meta: Option<Config>,
        _method: String,
        _args: Config,
    ) -> BoxFuture<Result<Config>> {
        Box::pin(async move { Err("这个服务只能按流调用".into()) })
    }

    fn call_stream(
        &self,
        _caller_meta: Option<Config>,
        _method: String,
        _args: Config,
        sink: StreamSink,
    ) -> BoxFuture<Result<()>> {
        let (tx, rx) = oneshot::channel();
        *self.gate.lock().unwrap() = Some(tx);
        self.finished.store(false, Ordering::SeqCst);
        let side_effect = self.side_effect.clone();
        let finished = self.finished.clone();
        let enter_first = self.enter_first;
        let entered = self.entered.clone();
        Box::pin(async move {
            // 这个 guard 在 future 完成或**被取消丢弃**时都会置位，测试据此
            // 知道后台任务已经做出选择，不必靠 sleep 猜调度。
            let _finished = DropFlag(finished);
            if enter_first {
                // 首次同步预跑停在这里；后台任务下一次轮询会走到 `entered`，
                // 然后挂进 core 的 select。测试收到通知后再制造双分支就绪。
                tokio::task::yield_now().await;
                entered.notify_one();
            }
            rx.await.ok();
            side_effect.store(true, Ordering::SeqCst);
            sink.close();
            Ok(())
        })
    }
}

/// 每个场景 1000 轮反复制造“取消 + 结果同时就绪”，断言取消永远赢。
async fn run_cancel_race(enter_first: bool) {
    let gate = Arc::new(Mutex::new(None));
    let side_effect = Arc::new(AtomicBool::new(false));
    let finished = Arc::new(AtomicBool::new(false));
    let entered = Arc::new(Notify::new());
    let app = app_with(
        "cancel-race",
        Arc::new(CancelRaceProvider {
            gate: gate.clone(),
            side_effect: side_effect.clone(),
            finished: finished.clone(),
            enter_first,
            entered: entered.clone(),
        }),
    )
    .await;

    for round in 0..1000 {
        let stream = app
            .root
            .call_iface_stream("svc", 1, "race", json!([]))
            .expect("开流");
        let tx = gate
            .lock()
            .unwrap()
            .take()
            .expect("call_stream 必须同步登记结果闸门");

        if enter_first {
            // 先让后台任务开到 core 的 select 并挂起，再制造双分支就绪。
            tokio::time::timeout(Duration::from_secs(1), entered.notified())
                .await
                .expect("后台任务必须先进入等待");
        }

        // 取消先发生；随后结果才就绪。
        drop(stream);
        tx.send(()).expect("业务 future 仍持有接收端");

        // 等后台任务真正做出选择（不能靠 sleep 猜）。
        for _ in 0..100 {
            if finished.load(Ordering::SeqCst) {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            finished.load(Ordering::SeqCst),
            "第 {round} 轮：后台任务没有完成选择"
        );
        assert!(
            !side_effect.load(Ordering::SeqCst),
            "第 {round} 轮：取消已发生，但业务 future 仍执行了副作用"
        );
    }

    app.close().await;
}

/// 取消先于后台任务启动：即使结果随后就绪，也不得跑业务 future。
#[tokio::test(flavor = "current_thread")]
async fn cancel_before_task_start_wins_when_result_is_already_ready() {
    run_cancel_race(false).await;
}

/// 后台任务已经挂进 `select!`：取消与结果同时就绪时，取消分支必须固定优先。
#[tokio::test(flavor = "current_thread")]
async fn cancel_wins_when_waiting_task_sees_cancel_and_result_ready_together() {
    run_cancel_race(true).await;
}

#[tokio::test]
async fn stream_calls_enforce_the_same_contract_as_value_calls() {
    let app = app_with("value", Arc::new(ValueProvider)).await;

    let missing = app
        .root
        .call_iface_stream("nope", 1, "get", json!([]))
        .err()
        .expect("缺服务必须当场失败");
    assert!(missing.to_string().contains("nope"), "{missing}");

    let method = app
        .root
        .call_iface_stream("svc", 1, "nope", json!([]))
        .err()
        .expect("未知方法必须失败");
    assert!(method.to_string().contains("可用方法"), "{method}");

    let version = app
        .root
        .call_iface_stream("svc", 9, "get", json!([]))
        .err()
        .expect("请求版本高于提供版本必须失败");
    assert!(version.to_string().contains("版本"), "{version}");
}
