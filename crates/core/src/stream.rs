//! 流（ADR 0001 §5）：一次调用产出一串值，而不是一个值。
//!
//! # 形状
//! 一次**流式调用**产生一对东西：[`StreamSink`]（提供者侧，往里推）与
//! [`AsyncStream`]（消费者侧，逐块 `next().await`）。窗口固定为 [`STREAM_WINDOW`]：
//! 缓冲里最多这么多块，满了之后的**非阻塞**推送拿到 [`PushOutcome::Pending`]——
//! 注意它意味着"**块没有被接受**"，提供者必须在窗口腾出位置后重推这一块。
//!
//! # 背压的两半
//! - **同进程（T1）**：`sink.send(chunk).await` 自己等窗口——`tokio::sync::mpsc`
//!   就是这个语义，零 ABI 成本，提供者不需要认识 credit。
//! - **跨 ABI（T2 提供者）**：插件不能在 FFI 里 await，所以走 `push`（非阻塞）+
//!   登记一个 credit 回调 id（[`StreamSink::push_awaiting`]）。消费者每取走一块，
//!   [`StreamObserver::credit_available`] 被调用一次，宿主据此回调插件"又有位置了"。
//!
//! # 收尾恰好一次
//! `close`（正常）/ `fail`（失败）/ 丢弃（消费者）三者谁先到谁生效：
//! - `close` **不丢缓冲**：消费者读完剩下的块，然后拿到 `Ok(None)`；
//! - `fail` **立刻给 `Err`**，缓冲丢弃——"流断了就断了"，部分数据不作数
//!   （与真实 LLM 流的处置一致：出错的那一轮产出不作数）；
//! - 消费者丢弃 ⇒ 提供者的下一次推送拿到 [`PushOutcome::Gone`]，**等待中的 credit
//!   回调交给 observer**（宿主据此告诉插件"别推了"）；
//! - 提供者丢光手柄（没有 `close`）⇒ 消费者读到 `Ok(None)`（"不会再有人写了"）。
//!
//! # 这一层不知道 ABI 存在
//! 只用 `tokio::sync`。宿主通过 [`StreamObserver`] 装配自己的记账（派发记录、credit
//! 投递、卸载排水），core 不认识 `HostApi`、`dispatch_id` 或任何 FFI 类型。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::Value;
use tokio::sync::{mpsc, watch};

use crate::{CancelToken, Result};

/// 窗口：缓冲里最多这么多块（ADR 0001 §5 的验收口径就是 4）。
pub const STREAM_WINDOW: usize = 4;

/// 一次推送的结局。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PushOutcome {
    /// 块已经进入缓冲。
    Sent,
    /// 窗口满了：**块没有被接受**，等 credit 再重推（`push_awaiting` 会记下回调 id）。
    Pending,
    /// 流已经不在了（消费者离场 / 已收尾）：停手，别再推。
    Gone,
}

/// 一条流的收尾形态（observer 视角）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StreamOutcome {
    /// 正常收尾（提供者 `close`）。
    Closed,
    /// 失败收尾：消费者会读到这条理由。
    Failed(String),
    /// 消费者不再读（丢弃）——提供者的推送会拿到 [`PushOutcome::Gone`]。
    Abandoned,
}

/// 外部设施（宿主）对流事件的观察者：core 只报事件，不关心谁来消费。
pub trait StreamObserver: Send + Sync + 'static {
    /// 窗口腾出了一个位置：`on_credit` 是某个等待中的推送登记的回调 id，
    /// `window` 是当前可用的位置数。
    fn credit_available(&self, on_credit: u64, window: usize);
    /// 这条流结束了（**恰好一次**）。`waiting` 是此刻还在等 credit 的回调 id——
    /// 它们已经不可能成功，宿主应当逐个驳回（"别推了"）。
    fn finished(&self, outcome: &StreamOutcome, waiting: Vec<u64>);
}

/// 流的状态：提供者与消费者共享。
struct StreamState {
    life: Mutex<Life>,
    /// 消费者已经离场（`push` 的快速否定判据）。
    gone: AtomicBool,
    /// 收尾通知（消费者 `next()` 与所有等待窗口的 `send`）。用 `watch` 而不是
    /// `Notify`：一次收尾必须唤醒**全部**等待者，且晚到者订阅时仍能看见 `true`。
    done: watch::Sender<bool>,
    /// 外部观察者（宿主装配；同进程直连的 T1 不需要它）。
    observer: OnceLock<Arc<dyn StreamObserver>>,
    window: usize,
}

struct Life {
    outcome: Option<StreamOutcome>,
    /// 失败/放弃时给消费者看的理由。
    fault: Option<String>,
    /// 等待中的 credit 回调 id（"满了 ⇒ 记下它 ⇒ 每取走一块唤醒一个"）。
    waiting: VecDeque<u64>,
}

impl StreamState {
    /// 收尾（恰好一次）。返回 `true` = 这次调用是首次收尾。
    fn finish(&self, outcome: StreamOutcome, fault: Option<String>) -> bool {
        let waiting = {
            let mut life = self.life.lock().unwrap();
            if life.outcome.is_some() {
                return false;
            }
            life.outcome = Some(outcome.clone());
            if fault.is_some() {
                life.fault = fault;
            }
            life.waiting.drain(..).collect::<Vec<_>>()
        };
        // 更新值并唤醒所有接收者；`send_replace` 即使此刻没有接收者也保留 `true`，
        // 于是"收尾后才订阅"的发送者/消费者不会错过。
        self.done.send_replace(true);
        if let Some(observer) = self.observer.get() {
            observer.finished(&outcome, waiting);
        }
        true
    }

    fn is_finished(&self) -> bool {
        self.life.lock().unwrap().outcome.is_some()
    }

    fn fault(&self) -> Option<String> {
        self.life.lock().unwrap().fault.clone()
    }
}

/// 提供者侧的推送句柄。克隆廉价（`mpsc::Sender` + `Arc`）。
#[derive(Clone)]
pub struct StreamSink {
    tx: mpsc::Sender<Value>,
    state: Arc<StreamState>,
}

impl StreamSink {
    /// 造一条流：返回（提供者侧 `sink`，消费者侧 `stream`）。
    pub fn new(window: usize) -> (StreamSink, AsyncStream) {
        let window = window.max(1);
        let (tx, rx) = mpsc::channel(window);
        let (done, _) = watch::channel(false);
        let state = Arc::new(StreamState {
            life: Mutex::new(Life {
                outcome: None,
                fault: None,
                waiting: VecDeque::new(),
            }),
            gone: AtomicBool::new(false),
            done,
            observer: OnceLock::new(),
            window,
        });
        (
            StreamSink {
                tx,
                state: state.clone(),
            },
            AsyncStream {
                rx,
                state,
                cancel: None,
            },
        )
    }

    /// 装配观察者（宿主用）。**先装配者生效**——一条流只有一次机会。
    pub fn observe(&self, observer: Arc<dyn StreamObserver>) {
        let _ = self.state.observer.set(observer);
    }

    pub fn window(&self) -> usize {
        self.state.window
    }

    /// 缓冲里当前的块数（诊断用）。
    pub fn buffered(&self) -> usize {
        self.tx.max_capacity() - self.tx.capacity()
    }

    /// 还有几个等待中的 credit 回调（诊断用）。
    pub fn waiting(&self) -> usize {
        self.state.life.lock().unwrap().waiting.len()
    }

    /// 这条流是否已经收尾（宿主用它决定"要不要再补一件返回值"）。
    pub fn is_finished(&self) -> bool {
        self.state.is_finished()
    }

    /// 非阻塞推一块：窗口满 ⇒ `Pending`（**块没被接受**），已收尾/消费者离场 ⇒ `Gone`。
    pub fn push(&self, chunk: Value) -> PushOutcome {
        self.push_with(chunk, None)
    }

    /// 与 [`StreamSink::push`] 相同，但窗口满时**记下 `on_credit`**：
    /// 宿主会在窗口腾出位置时用这个 id 回调提供者（跨 ABI 的背压就是它）。
    pub fn push_awaiting(&self, chunk: Value, on_credit: u64) -> PushOutcome {
        self.push_with(chunk, Some(on_credit))
    }

    fn push_with(&self, chunk: Value, on_credit: Option<u64>) -> PushOutcome {
        // 登记与投递在**同一把锁**里：消费者每取走一块也在同一把锁里取走一个 id，
        // 于是"满了、但还没来得及登记"这个丢唤醒窗口在结构上不存在。
        let mut life = self.state.life.lock().unwrap();
        if life.outcome.is_some() || self.state.gone.load(Ordering::SeqCst) {
            return PushOutcome::Gone;
        }
        match self.tx.try_send(chunk) {
            Ok(()) => PushOutcome::Sent,
            Err(mpsc::error::TrySendError::Full(_)) => {
                if let Some(id) = on_credit {
                    life.waiting.push_back(id);
                }
                PushOutcome::Pending
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                drop(life);
                self.state.gone.store(true, Ordering::SeqCst);
                PushOutcome::Gone
            }
        }
    }

    /// 异步推一块：窗口满就等（同进程的背压写法，零 ABI 成本）。
    ///
    /// `close`/`fail`/消费者离场必须能**唤醒所有**正在等窗口的 `send`；
    /// 只 await `tx.send` 会让提供者在流已经结束后永久挂起。
    ///
    /// **提交与收尾判定在同一把锁里**：`reserve` 拿到的是"窗口位置"，
    /// 但收尾可能已在等待期间发生；放行前必须复查 `outcome`。否则
    /// `select!` 同时看到"有位置"与"已收尾"时会随机选中数据分支，
    /// 让收尾之后仍有一块数据进入缓冲（消费者随后会读到 EOF 之后的数据）。
    pub async fn send(&self, chunk: Value) -> PushOutcome {
        let chunk = {
            let life = self.state.life.lock().unwrap();
            if life.outcome.is_some() || self.state.gone.load(Ordering::SeqCst) {
                return PushOutcome::Gone;
            }
            match self.tx.try_send(chunk) {
                Ok(()) => return PushOutcome::Sent,
                Err(mpsc::error::TrySendError::Full(chunk)) => chunk,
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    self.state.gone.store(true, Ordering::SeqCst);
                    return PushOutcome::Gone;
                }
            }
        };
        // 先订阅再复查：既覆盖"复查后才收尾"的竞态，也让所有等待中的发送者
        // 在收尾时一起被唤醒（`watch` 是广播，不是一个许可）。
        let mut done = self.state.done.subscribe();
        if *done.borrow() {
            return PushOutcome::Gone;
        }
        tokio::select! {
            reserved = self.tx.reserve() => match reserved {
                Ok(permit) => {
                    // 关键复查：`reserve` 就绪不等于这条流还活着。与 `finish`
                    // 共用 `life` 锁，于是"收尾先发生 ⇒ 这块永远不提交"与
                    // "这块先提交 ⇒ 收尾的 buffer 语义照常"之间只有一个顺序。
                    let life = self.state.life.lock().unwrap();
                    if life.outcome.is_some() || self.state.gone.load(Ordering::SeqCst) {
                        drop(permit);
                        return PushOutcome::Gone;
                    }
                    permit.send(chunk);
                    PushOutcome::Sent
                }
                Err(_) => {
                    self.state.gone.store(true, Ordering::SeqCst);
                    PushOutcome::Gone
                }
            },
            _ = done.changed() => PushOutcome::Gone,
        }
    }

    /// 正常收尾：消费者读完缓冲之后拿到 `Ok(None)`。
    pub fn close(&self) -> PushOutcome {
        if self.state.finish(StreamOutcome::Closed, None) {
            PushOutcome::Sent
        } else {
            PushOutcome::Gone
        }
    }

    /// 失败收尾：消费者**立刻**读到这条理由（缓冲丢弃）。
    pub fn fail(&self, reason: impl Into<String>) -> PushOutcome {
        let reason = reason.into();
        if self
            .state
            .finish(StreamOutcome::Failed(reason.clone()), Some(reason))
        {
            PushOutcome::Sent
        } else {
            PushOutcome::Gone
        }
    }
}

/// 消费者侧的异步迭代器。
pub struct AsyncStream {
    rx: mpsc::Receiver<Value>,
    state: Arc<StreamState>,
    /// 提供者任务的生命周期句柄（`call_iface_stream` 派生后台任务时装配）。
    ///
    /// 丢弃流是"消费者离场"的明确信号：提供者任务不该继续跑副作用，
    /// 更不该把整棵上下文树吊住。`Drop` 里只发取消（同步、无 await）；
    /// 真正等待任务结束由 fiber 卸载路径负责。
    cancel: Option<CancelToken>,
}

impl AsyncStream {
    pub(crate) fn attach_cancel(&mut self, cancel: CancelToken) {
        self.cancel = Some(cancel);
    }

    /// 取下一块。
    ///
    /// - `Ok(Some(chunk))`：一块数据（顺带归还一个窗口位置）；
    /// - `Ok(None)`：流正常结束（已缓冲的块都已交付）；
    /// - `Err(...)`：流失败（提供者 `fail`、提供者被卸载、宿主收回、取消）。
    pub async fn next(&mut self) -> Result<Option<Value>> {
        let mut done = self.state.done.subscribe();
        loop {
            // ①～③ 必须在**同一把 life 锁**里完成：`fail`/`close` 也要拿它。
            // 否则会出现"检查完 fault ⇒ fail 插进来 ⇒ try_recv 仍把旧缓冲交付"
            // 的窗口，或"看到 EOF ⇒ 发送者提交一块"的倒序。
            enum Step {
                Data(Value),
                End,
                Fault(String),
                Wait,
            }
            let step = {
                let life = self.state.life.lock().unwrap();
                if let Some(fault) = &life.fault {
                    Step::Fault(fault.clone())
                } else {
                    match self.rx.try_recv() {
                        Ok(chunk) => Step::Data(chunk),
                        Err(mpsc::error::TryRecvError::Disconnected) => Step::End,
                        Err(mpsc::error::TryRecvError::Empty) => {
                            if life.outcome.is_some() {
                                Step::End
                            } else {
                                Step::Wait
                            }
                        }
                    }
                }
            };
            match step {
                Step::Data(chunk) => {
                    self.credit();
                    return Ok(Some(chunk));
                }
                Step::End => return self.end(),
                Step::Fault(fault) => return Err(fault.into()),
                Step::Wait => {}
            }
            // ④ 等新块或收尾通知（`watch` 保存收尾值，所以不会丢唤醒）。
            tokio::select! {
                received = self.rx.recv() => match received {
                    Some(chunk) => {
                        // `select!` 可能同时看到"有块"与"已失败"。交付前再复查
                        // 一次 fault；`close` 仍按契约交付收尾前的缓冲。
                        let fault = self.state.life.lock().unwrap().fault.clone();
                        if let Some(fault) = fault {
                            return Err(fault.into());
                        }
                        self.credit();
                        return Ok(Some(chunk));
                    }
                    None => return self.end(),
                },
                _ = done.changed() => continue,
            }
        }
    }

    fn end(&self) -> Result<Option<Value>> {
        match self.state.fault() {
            Some(fault) => Err(fault.into()),
            None => Ok(None),
        }
    }

    /// 归还一个窗口位置：唤醒一个等待中的推送者（没人在等就什么也不做）。
    fn credit(&self) {
        let id = { self.state.life.lock().unwrap().waiting.pop_front() };
        let Some(id) = id else { return };
        let window = self.state.window - self.rx.len();
        if let Some(observer) = self.state.observer.get() {
            observer.credit_available(id, window);
        }
    }
}

impl Drop for AsyncStream {
    /// 消费者离场：提供者的下一次推送拿到 `Gone`，等待中的 credit 回调交给
    /// observer 去驳回（否则插件会一直等一个永远不来的额度）。
    fn drop(&mut self) {
        // 先登记"消费者离场"：这样即使取消立刻唤醒提供者并让它调用 `fail`，
        // observer 看到的仍是 Abandoned（丢弃流）而不是一次伪失败。
        self.state.gone.store(true, Ordering::SeqCst);
        self.state.finish(
            StreamOutcome::Abandoned,
            Some("消费者已经不再读这条流".to_string()),
        );
        // 再取消提供者任务：它可能正卡在一次与流无关的 await 上，单靠
        // `gone` 标记永远不会醒来。取消是同步的；等待在 fiber 卸载路径。
        if let Some(cancel) = self.cancel.take() {
            cancel.cancel();
        }
    }
}
