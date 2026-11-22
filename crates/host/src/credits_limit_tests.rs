//! O8a-1：宿主层 credit 上限（只读内部边界）。
//!
//! 背景：`StreamSink::push_awaiting` 在窗口满时把 `on_credit` 记进 core 的等待队列，
//! 宿主同时在 `credits` 表里按 `(dispatch_id, on_credit)` 记一次债，SDK 侧还有一张
//! continuation 表。三者都按**推送次数**增长，`maxDispatchesPerInstance` /
//! `maxStreamsPerInstance` 都管不到它：坏插件只要每次换一个新的 `on_credit` id
//! 就能无限放大这三张表。这里钉住宿主自己的那道闸门。
//!
//! 夹具铁律与 `observer_close_tests` 相同：fake `CallFn` 内禁止 assert，结局只记进
//! static `Mutex`；每个测试用自己的句柄键，同一二进制内并行跑。

use super::*;
use std::sync::OnceLock;

/// 与实现里的私有上限对齐：crate 内用例才读得到它（这正是把用例放在这里的原因）。
const LIMIT: usize = MAX_OUTSTANDING_CREDITS_PER_STREAM;

/// 每条流在 core 里的窗口（`StreamSink::new(1)`）：一块就能填满。
const DISPATCH: u64 = 71;

fn calls() -> &'static Mutex<Vec<(u64, u32, Value)>> {
    static CALLS: OnceLock<Mutex<Vec<(u64, u32, Value)>>> = OnceLock::new();
    CALLS.get_or_init(Default::default)
}

unsafe extern "C" fn record(handle: u64, op: u32, args: Blob, _: Blob, _: *mut Out) -> i32 {
    if op == OP_CALLBACK {
        let payload: Value = serde_json::from_slice(args.as_bytes()).unwrap_or(Value::Null);
        if let Ok(mut calls) = calls().lock() {
            calls.push((handle, op, payload));
        }
    }
    STATUS_OK
}

struct Fixture {
    /// 持有 App 才保证实例不被处置；本组用例不需要 `close`。
    _app: cordis_core::App,
    handle: u64,
    instance: Instance,
}

impl Fixture {
    /// 能力位 = 1：容量拒绝必须能以结构化载荷回到插件侧。
    async fn new() -> Self {
        let app = cordis_core::App::new();
        apply_via_with_keepalive_and_format(
            record,
            app.root.clone(),
            Value::Null,
            "credits-cap",
            None,
            ERROR_FORMAT_V1,
        )
        .await
        .unwrap();
        let handle = instance_handle_of(&app.root).unwrap();
        let instance = instances().read().unwrap().get(handle).unwrap();
        calls()
            .lock()
            .unwrap()
            .retain(|(owner, _, _)| *owner != handle);
        Self {
            _app: app,
            handle,
            instance,
        }
    }

    /// 一条窗口 = 1 的流，并预先推进一块**已接受**的块（窗口因此是满的）。
    fn full_window_stream(&self) -> (StreamSink, cordis_core::AsyncStream) {
        let (sink, stream) = self.new_stream();
        let (status, text) = self.push(0xFFFF_FFFF);
        assert_eq!(status, STATUS_OK, "{text}");
        assert_eq!(sink.waiting(), 0, "被接受的块不登记 credit");
        (sink, stream)
    }

    fn new_stream(&self) -> (StreamSink, cordis_core::AsyncStream) {
        let (sink, stream) = StreamSink::new(1);
        sink.observe(Arc::new(HostStreamObserver {
            handle: self.handle,
            dispatch_id: DISPATCH,
        }));
        insert_dispatch(
            &self.instance,
            DISPATCH,
            DispatchRecord {
                sink: None,
                frame: None,
                parent: None,
                outbound: None,
                stream: Some(sink.clone()),
            },
        )
        .unwrap_or_else(|_| panic!("admission"));
        (sink, stream)
    }

    /// 推一块，返回（状态码，插件侧看到的文本）。
    fn push(&self, id: u64) -> (i32, String) {
        let mut buffer = vec![0u8; 4 * 1024];
        let mut out = Out {
            ptr: buffer.as_mut_ptr(),
            cap: buffer.len(),
            len: 0,
        };
        let status =
            unsafe { host_stream_push(self.handle, DISPATCH, Blob::new(b"1"), id, &mut out) };
        let written = out.len.min(buffer.len());
        let text = String::from_utf8_lossy(&buffer[..written]).into_owned();
        (status, text)
    }

    fn push_status(&self, id: u64) -> i32 {
        self.push(id).0
    }

    /// credit 表里**键的个数**（一条键 = 一个挂起中的推送 id）。
    fn credits_len(&self) -> usize {
        self.instance
            .credits
            .lock()
            .unwrap()
            .keys()
            .filter(|(dispatch, _)| *dispatch == DISPATCH)
            .count()
    }

    /// 某个 `on_credit` id 记了几次债（重复 id 按次数计数）。
    fn credits_count(&self, id: u64) -> usize {
        *self
            .instance
            .credits
            .lock()
            .unwrap()
            .get(&(DISPATCH, id))
            .unwrap_or(&0)
    }

    fn credit_events(&self, id: u64) -> Vec<Value> {
        calls()
            .lock()
            .unwrap()
            .iter()
            .filter(|(owner, _, payload)| {
                *owner == self.handle && payload.get("id").and_then(Value::as_u64) == Some(id)
            })
            .map(|(_, _, payload)| payload.clone())
            .collect()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        calls()
            .lock()
            .unwrap()
            .retain(|(owner, _, _)| *owner != self.handle);
    }
}

/// a. 满窗口后用**不同 id** 连推：第 N+1 次被拒（STATUS_ERR / host_capacity），
///    且 credit 表停在 N —— 拒绝不得先记债再说话。
#[tokio::test]
async fn full_window_rejects_pushes_past_the_internal_credit_cap() {
    let f = Fixture::new().await;
    let (sink, _stream) = f.full_window_stream();
    for id in 0..LIMIT as u64 {
        let (status, text) = f.push(id);
        assert_eq!(status, STATUS_PENDING, "id {id} 应登记为挂起：{text}");
    }
    assert_eq!(f.credits_len(), LIMIT);
    assert_eq!(sink.waiting(), LIMIT, "core 侧等待队列同步增长");

    let (status, text) = f.push(LIMIT as u64);
    assert_eq!(status, STATUS_ERR, "超过内部上限必须拒绝：{text}");
    let payload = ErrorPayload::from_json(&text).expect("能力位为 1 时拒绝必须是结构化的");
    assert_eq!(payload.code, "host_capacity", "{}", payload.error);
    assert_eq!(payload.category, "capacity", "{}", payload.error);
    assert_eq!(payload.certainty, "not_executed", "{}", payload.error);
    assert!(
        payload.error.contains("maxOutstandingCreditsPerStream"),
        "可读错误必须点名上限：{}",
        payload.error
    );
    // 文案必须说的是"这一块没被接受"，不能复用派发准入那句：
    // 推送被拒时派发在跑、插件代码也正在执行。
    assert!(
        payload.error.contains("这块未被接受"),
        "推送拒绝必须说明被拒的是这一块：{}",
        payload.error
    );
    assert!(
        !payload.error.contains("拒绝这次派发") && !payload.error.contains("插件代码未执行"),
        "推送拒绝不得复用派发准入的文案：{}",
        payload.error
    );
    assert_eq!(
        payload.detail["scope"].as_str(),
        Some("maxOutstandingCreditsPerStream"),
        "结构化 detail 必须点名同一上限"
    );
    assert_eq!(f.credits_len(), LIMIT, "被拒的推送不得记债");
    assert_eq!(sink.waiting(), LIMIT, "被拒的推送不得进入 core 的等待队列");
}

/// b. 被拒之后消费者正常排空，该流仍能继续推完并正常收尾（拒绝不伤流）。
#[tokio::test]
async fn a_rejected_push_does_not_wound_the_stream() {
    let f = Fixture::new().await;
    let (sink, mut stream) = f.full_window_stream();
    for id in 0..LIMIT as u64 {
        assert_eq!(f.push_status(id), STATUS_PENDING);
    }
    assert_eq!(f.push_status(LIMIT as u64), STATUS_ERR);

    // 消费者取走那一块：core 唤醒一个等待中的 id，宿主的债随之减一。
    let first = stream.next().await.unwrap().expect("还有一块");
    assert_eq!(first, Value::Number(1.into()));
    assert_eq!(f.credits_len(), LIMIT - 1, "一次 credit 只结算一条债");
    assert_eq!(f.credit_events(0).len(), 1, "插件应收到 id=0 的额度回调");

    // 窗口腾出来后照常推：先被接受，再挂起。
    assert_eq!(f.push_status(LIMIT as u64 + 1), STATUS_OK);
    assert_eq!(f.push_status(LIMIT as u64 + 2), STATUS_PENDING);

    // 正常收尾：剩下的块读完就是 EOF，而不是错误。
    assert_eq!(
        unsafe {
            let mut out = Out {
                ptr: std::ptr::null_mut(),
                cap: 0,
                len: 0,
            };
            host_stream_close(f.handle, DISPATCH, &mut out)
        },
        STATUS_OK
    );
    assert!(sink.is_finished());
    assert_eq!(stream.next().await.unwrap(), Some(Value::Number(1.into())));
    assert_eq!(stream.next().await.unwrap(), None, "拒绝不得把流变成失败");
}

/// c. 同一个 on_credit id 重复推送（少于上限）仍**按次数**计数、不去重：
///    这是已验收行为（`[9, 9, 10]` 的驳回顺序依赖它）。
#[tokio::test]
async fn repeated_on_credit_ids_still_count_per_push() {
    let f = Fixture::new().await;
    let (sink, _stream) = f.full_window_stream();
    for _ in 0..3 {
        assert_eq!(f.push_status(9), STATUS_PENDING);
    }
    assert_eq!(f.credits_len(), 1, "同一个 id 只有一条键");
    assert_eq!(f.credits_count(9), 3, "重复 id 按次数记债");
    assert_eq!(sink.waiting(), 3, "core 侧也是三次");
}

/// d. 关闭时被拒的那次不进入等待队列、不改写流的终态：收尾照常恰好一次，
///    收尾之后再推仍然是 STREAM_GONE（不伪装成容量问题）。
#[tokio::test]
async fn a_rejected_push_is_not_queued_and_close_keeps_the_terminal_state() {
    let f = Fixture::new().await;
    let (sink, mut stream) = f.full_window_stream();
    for id in 0..LIMIT as u64 {
        assert_eq!(f.push_status(id), STATUS_PENDING);
    }
    assert_eq!(f.push_status(LIMIT as u64), STATUS_ERR);
    assert_eq!(sink.waiting(), LIMIT);

    let mut out = Out {
        ptr: std::ptr::null_mut(),
        cap: 0,
        len: 0,
    };
    assert_eq!(
        unsafe { host_stream_close(f.handle, DISPATCH, &mut out) },
        STATUS_OK
    );
    assert!(sink.is_finished());
    assert_eq!(f.credits_len(), 0, "收尾把所有等待中的债驳回");
    assert_eq!(
        f.credit_events(LIMIT as u64).len(),
        0,
        "被拒的 id 从来没登记过，收尾时也无从驳回"
    );

    assert_eq!(
        f.push_status(LIMIT as u64 + 3),
        STATUS_STREAM_GONE,
        "收尾之后只报 STREAM_GONE"
    );
    assert_eq!(stream.next().await.unwrap(), Some(Value::Number(1.into())));
    assert_eq!(stream.next().await.unwrap(), None);
}
