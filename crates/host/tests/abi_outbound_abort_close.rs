//! `ctx_abort` 之后 `App::close` 仍必须等待已被取消的任务真正结束。
//!
//! 取消路径会先摘掉派发记录；如果完成信号随记录一起被丢掉，关闭流程就再也
//! 找不到这个任务。实例级 `reclaiming` 集合把完成信号保留到任务结束。

use cordis_abi::{Blob, Out, OP_APPLY, OP_CALLBACK, OP_DISPOSE, STATUS_OK, STATUS_UNSUPPORTED};
use cordis_core::{App, Config, Ctx, Inject, Plugin};
use cordis_host::{apply_via, host_api, loaded_instances};
use serde_json::Value;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

static CONSUMER: AtomicU64 = AtomicU64::new(0);
static DISPATCH_ID: AtomicU64 = AtomicU64::new(0);
static APPLY_OUT: Mutex<String> = Mutex::new(String::new());
static TASK_ALIVE: AtomicBool = AtomicBool::new(false);
static SLEEPING: AtomicBool = AtomicBool::new(false);
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
                Blob::new(b"abort-then-close"),
                Blob::new(br#"{"mode":"bail","args":[]}"#),
                7,
                &mut slot,
            );
            if status != STATUS_OK {
                return status;
            }
            let text = String::from_utf8_lossy(&buffer[..slot.len]).into_owned();
            if let Ok(value) = serde_json::from_str::<Value>(&text) {
                DISPATCH_ID.store(
                    value
                        .get("dispatch_id")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                    Ordering::SeqCst,
                );
            }
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
        name: "abort-close-consumer",
        key: cordis_core::unique_key("abort-close-consumer"),
        inject: Inject::default(),
        validator: None,
        f: Arc::new(|ctx: Ctx, config: Config| {
            apply_via(consumer_call, ctx, config, "abort-close-consumer")
        }),
    })
}

struct AliveGuard;

impl Drop for AliveGuard {
    fn drop(&mut self) {
        TASK_ALIVE.store(false, Ordering::SeqCst);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn close_waits_for_task_cancelled_by_abort() {
    CONSUMER.store(0, Ordering::SeqCst);
    DISPATCH_ID.store(0, Ordering::SeqCst);
    APPLY_OUT.lock().unwrap().clear();
    TASK_ALIVE.store(false, Ordering::SeqCst);
    SLEEPING.store(false, Ordering::SeqCst);
    SIDE_EFFECT.store(false, Ordering::SeqCst);

    let app = App::new();
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let entered2 = entered.clone();
    let release2 = release.clone();
    let _listener = app.root.on("abort-then-close", move |_call| {
        TASK_ALIVE.store(true, Ordering::SeqCst);
        let guard = AliveGuard;
        let entered = entered2.clone();
        let release = release2.clone();
        Box::pin(async move {
            let _guard = guard;
            entered.notify_one();
            release.notified().await;
            // 第二拍同步占住一个 worker：取消只能在下一次 poll 生效，
            // “关闭是否等待”因此有一个可观测的窗口。
            SLEEPING.store(true, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(600));
            SLEEPING.store(false, Ordering::SeqCst);
            tokio::task::yield_now().await;
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
    let dispatch_id = DISPATCH_ID.load(Ordering::SeqCst);
    assert_ne!(handle, 0, "consumer 必须拿到 ABI 句柄");
    assert_ne!(dispatch_id, 0, "派发必须带 dispatch_id");

    tokio::time::timeout(Duration::from_secs(1), entered.notified())
        .await
        .expect("任务应当已经进入监听器");
    release.notify_one();

    let deadline = Instant::now() + Duration::from_secs(2);
    while !SLEEPING.load(Ordering::SeqCst) {
        assert!(
            Instant::now() < deadline,
            "任务没有进入同步睡眠阶段，测试前提不成立"
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    // abort 先摘记录；任务此刻仍卡在同步 poll 里，取消尚未生效。
    let mut buffer = [0u8; 256];
    let mut out = Out {
        ptr: buffer.as_mut_ptr(),
        cap: buffer.len(),
        len: 0,
    };
    let status = unsafe { host_api().ctx_abort.expect("ctx_abort")(handle, dispatch_id, &mut out) };
    assert_eq!(status, STATUS_OK, "ctx_abort 应成功");

    let started = Instant::now();
    let mut close = Box::pin(app.close());
    let early = tokio::time::timeout(Duration::from_millis(200), &mut close).await;
    if early.is_err() {
        tokio::time::timeout(Duration::from_secs(3), &mut close)
            .await
            .expect("App::close 必须在被取消任务结束后完成");
    }
    let elapsed = started.elapsed();

    assert!(
        early.is_err() || elapsed >= Duration::from_millis(200),
        "ctx_abort 后 App::close 不得在任务析构完成前返回（elapsed={elapsed:?}）"
    );
    assert!(
        !TASK_ALIVE.load(Ordering::SeqCst),
        "App::close 返回时，已被 abort 的任务仍在运行"
    );
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(
        !SIDE_EFFECT.load(Ordering::SeqCst),
        "被 abort 的任务在关闭后仍执行了副作用"
    );
    assert_eq!(loaded_instances(), 0, "关闭后不得残留实例");
}
