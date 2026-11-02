//! 取消令牌（M2c）：一次派发的**调用方**持有的撤销句柄。
//!
//! 语义（对齐 JS 的 `AbortSignal` 最小子集）：
//! - `clone` 是**共享**的：克隆体与原体是同一个令牌；
//! - `cancel()` **幂等**：第二次是无害空操作；
//! - `cancelled()` 挂起直到取消；**已取消时立即就绪**（不给调用方留一个
//!   永远挂住的 await）。
//!
//! 内核自己**不因取消而中断链**：令牌只是随 [`Call::cancel`](crate::Call)
//! 交给 handler 的"事实"。谁把在飞的挂起续延结算掉，是**载体层**的职责
//! （宿主侧的取消桥：`select!` 到 `cancelled()` 后 `remove`-then-settle）。
//! 这条分工让内核保持"无策略"，也让 T3 这类载体能自己决定取消的粒度。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Clone, Default)]
pub struct CancelToken {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    cancelled: AtomicBool,
    /// 唤醒等待者。`notify_one` 的许可（permit）语义保证"取消先于 await"
    /// 也不会丢唤醒：晚到的 `notified().await` 会消费掉那个许可。
    notify: tokio::sync::Notify,
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    /// 撤销这次派发。**幂等**。
    pub fn cancel(&self) {
        self.inner.cancelled.store(true, Ordering::SeqCst);
        // 两个都要：`notify_waiters` 唤醒**已注册**的等待者；
        // `notify_one` 给"还没开始等"的那个留一个许可（不丢唤醒）。
        self.inner.notify.notify_waiters();
        self.inner.notify.notify_one();
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::SeqCst)
    }

    /// 挂起直到取消；已取消时立即返回。
    pub async fn cancelled(&self) {
        while !self.is_cancelled() {
            self.inner.notify.notified().await;
        }
    }
}

impl std::fmt::Debug for CancelToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CancelToken")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}
