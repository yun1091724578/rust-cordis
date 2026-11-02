//! 关闭必须**终止并等待**宿主创建的出站任务，而不是只撤记录/送 abort。
//!
//! 反例：`complete_or_defer` 把 future 丢给 tokio 后丢掉 JoinHandle。卸载排水
//! 只摘派发记录并投递 abort；任务里的 future 继续跑，`App::close` 返回后仍能
//! 产生副作用。本测试用一个卡在 `Notify` 上的监听器把这条路径钉住。

use cordis_abi::{Blob, Out, OP_APPLY, OP_CALLBACK, OP_DISPOSE, STATUS_OK, STATUS_UNSUPPORTED};
use cordis_core::{App, Config, Ctx, Inject, Plugin};
use cordis_host::{apply_via, dispatches_len, host_api, loaded_instances};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

static CONSUMER: AtomicU64 = AtomicU64::new(0);
static APPLY_OUT: Mutex<String> = Mutex::new(String::new());
static TASK_ALIVE: AtomicBool = AtomicBool::new(false);
static SIDE_EFFECT: AtomicBool = AtomicBool::new(false);

unsafe extern "C" fn consumer_call(handle: u64, op: u32, _a: Blob, _b: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => {
            CONSUMER.store(handle, Ordering::SeqCst);
            let Some(dispatch) = host_api().ctx_dispatch else {
                return STATUS_UNSUPPORTED;
            };
            let mut buffer = [0u8; 512];
            let mut slot = Out {
                ptr: buffer.as_mut_ptr(),
                cap: buffer.len(),
                len: 0,
            };
            let status = dispatch(
                handle,
                Blob::new(b"slow-close"),
                Blob::new(br#"{"mode":"bail","args":[]}"#),
                7,
                &mut slot,
            );
            if status != STATUS_OK {
                return status;
            }
            let text = String::from_utf8_lossy(&buffer[..slot.len]).into_owned();
            *APPLY_OUT.lock().unwrap() = text.clone();
            (*out).write(text.as_bytes())
        }
        OP_CALLBACK => (*out).write(b""),
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

fn consumer() -> Arc<dyn Plugin> {
    Arc::new(cordis_core::FnPlugin {
        name: "close-consumer",
        key: cordis_core::unique_key("close-consumer"),
        inject: Inject::default(),
        validator: None,
        f: Arc::new(|ctx: Ctx, config: Config| {
            apply_via(consumer_call, ctx, config, "close-consumer")
        }),
    })
}

/// 监听器 future 被丢弃（取消生效）时清掉 "任务还活着" 标记。
struct AliveGuard;

impl Drop for AliveGuard {
    fn drop(&mut self) {
        TASK_ALIVE.store(false, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn close_cancels_and_waits_for_outbound_dispatch_task() {
    CONSUMER.store(0, Ordering::SeqCst);
    APPLY_OUT.lock().unwrap().clear();
    TASK_ALIVE.store(false, Ordering::SeqCst);
    SIDE_EFFECT.store(false, Ordering::SeqCst);

    let app = App::new();
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let entered2 = entered.clone();
    let release2 = release.clone();
    let _listener = app.root.on("slow-close", move |_call| {
        TASK_ALIVE.store(true, Ordering::SeqCst);
        let guard = AliveGuard;
        let entered = entered2.clone();
        let release = release2.clone();
        Box::pin(async move {
            let _guard = guard;
            entered.notify_one();
            release.notified().await;
            SIDE_EFFECT.store(true, Ordering::SeqCst);
            None
        })
    });

    let _fiber = app.plugin(consumer(), Config::Null).await.unwrap();
    assert!(
        APPLY_OUT.lock().unwrap().contains("\"pending\":true"),
        "出站调用应先挂起：{:?}",
        APPLY_OUT.lock().unwrap()
    );
    let handle = CONSUMER.load(Ordering::SeqCst);
    assert_ne!(handle, 0, "consumer 必须拿到 ABI 句柄");
    assert_eq!(dispatches_len(handle), 1, "关闭前应有一条在飞记录");

    tokio::time::timeout(Duration::from_secs(1), entered.notified())
        .await
        .expect("出站 future 应当已经进入监听器");
    assert!(
        TASK_ALIVE.load(Ordering::SeqCst),
        "监听器必须处于运行中，否则本测试没有在飞任务"
    );

    let mut close = Box::pin(app.close());
    let early = tokio::time::timeout(Duration::from_millis(100), &mut close).await;
    // 无论关闭是否已经返回，都解除监听器的等待：旧实现会在这里继续执行副作用。
    release.notify_one();
    if early.is_err() {
        tokio::time::timeout(Duration::from_secs(2), &mut close)
            .await
            .expect("App::close 必须在取消出站任务后完成");
    }

    assert!(
        !TASK_ALIVE.load(Ordering::SeqCst),
        "App::close 返回时，宿主创建的出站任务仍在运行"
    );
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(
        !SIDE_EFFECT.load(Ordering::SeqCst),
        "被取消的出站任务在关闭后仍执行了副作用"
    );
    assert_eq!(loaded_instances(), 0, "关闭后不得残留实例");
}
