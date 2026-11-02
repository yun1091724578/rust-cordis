//! M2c：取消令牌（内核侧接缝）——`CancelToken` 经 `Call.cancel` 到达**原生** handler。
//!
//! 契约（D3）：
//! - 带令牌的派发（`Store::dispatch_cancellable`）：每个 handler 的 `Call.cancel`
//!   是 `Some(token)`，handler 可以 `token.cancelled().await` 挂住；
//! - 不带令牌的派发：`Call.cancel` 是 `None`（对照面——把"没取消"与"取消不了"分开）；
//! - `cancel()` 幂等；`cancelled()` 在已取消时立即返回。

use cordis_core::{App, CancelToken, Dyn, ExternValue, Mode, Ret};
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::oneshot;

#[tokio::test]
async fn cancel_token_is_visible_to_native_handlers_via_call() {
    let app = App::new();
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    // ① 对照面：普通派发 ⇒ `Call.cancel` 必须是 None。
    {
        let seen = seen.clone();
        app.root.on("plain", move |call| {
            let seen = seen.clone();
            Box::pin(async move {
                seen.lock()
                    .unwrap()
                    .push(format!("plain:{}", call.cancel.is_none()));
                None
            })
        });
    }

    // ② 带令牌的派发：handler 先报"开跑"，再 await 取消。
    let (started_tx, started_rx) = oneshot::channel::<()>();
    let started_tx = Arc::new(Mutex::new(Some(started_tx)));
    {
        let seen = seen.clone();
        let started_tx = started_tx.clone();
        app.root.on("cancellable", move |call| {
            let seen = seen.clone();
            let started_tx = started_tx.clone();
            Box::pin(async move {
                let token = call
                    .cancel
                    .clone()
                    .expect("带令牌的派发必须把令牌交给 handler");
                seen.lock()
                    .unwrap()
                    .push(format!("started:{}", token.is_cancelled()));
                if let Some(tx) = started_tx.lock().unwrap().take() {
                    let _ = tx.send(());
                }
                token.cancelled().await;
                seen.lock()
                    .unwrap()
                    .push(format!("cancelled:{}", token.is_cancelled()));
                Some(Dyn::new(ExternValue(json!("observed"))))
            })
        });
    }

    let plain = app.root.bail("plain", vec![]).await;
    assert!(plain.is_none(), "对照组只是观察，不产生 bail 值");

    let token = CancelToken::new();
    assert!(!token.is_cancelled(), "新令牌必须是未取消的");
    let future: cordis_core::BoxFuture<Ret> = app.store.dispatch_cancellable(
        Mode::Bail,
        "cancellable",
        &app.root,
        vec![],
        None,
        None,
        Some(token.clone()),
    );
    let task = tokio::spawn(future);

    started_rx
        .await
        .expect("handler 必须开跑（先注册再取消，避免竞态）");
    assert!(!token.is_cancelled(), "取消之前令牌必须是未取消的");

    token.cancel();
    token.cancel(); // 幂等：第二次是无害空操作
    assert!(token.is_cancelled());
    // 已取消之后 `cancelled()` 立即就绪（不给调用方留一个永远挂住的 await）。
    tokio::time::timeout(Duration::from_secs(1), token.cancelled())
        .await
        .expect("已取消的令牌必须立即唤醒等待者");

    let ret = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("派发必须在时限内结算")
        .expect("派发任务不得 panic");
    let observed = ret
        .and_then(|value| value.downcast::<ExternValue>())
        .map(|value| value.0.clone());
    assert_eq!(
        observed,
        Some(json!("observed")),
        "handler 在取消后照常返回自己的值"
    );

    let seen = seen.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec![
            "plain:true".to_string(),
            "started:false".into(),
            "cancelled:true".into(),
        ],
        "令牌可见性 / 取消前后的状态必须逐字符合契约：{seen:?}"
    );
}
