//! 并发“取消 → 关闭”：关闭不能落在“记录已摘、done 尚未登记”的空档里。
//!
//! 取消路径在派发表锁内完成摘记录 + 回收登记；关闭取快照时同时持有同一把锁。
//! 本测试让取消回调故意变慢，并在回调执行期间启动关闭，稳定覆盖这个窗口。

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
static ABORT_CALLBACK_ENTERED: AtomicBool = AtomicBool::new(false);
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
                Blob::new(b"cancel-close-race"),
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
        OP_CALLBACK => {
            // 取消交付是同步回调。故意慢：旧实现会先摘记录、再执行这里、
            // 最后才登记 done；关闭在这段时间里会看到两张空表。
            ABORT_CALLBACK_ENTERED.store(true, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(250));
            (*out).write(b"")
        }
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

fn consumer() -> Arc<dyn Plugin> {
    Arc::new(cordis_core::FnPlugin {
        name: "cancel-close-consumer",
        key: cordis_core::unique_key("cancel-close-consumer"),
        inject: Inject::default(),
        validator: None,
        f: Arc::new(|ctx: Ctx, config: Config| {
            apply_via(consumer_call, ctx, config, "cancel-close-consumer")
        }),
    })
}

struct AliveGuard;

impl Drop for AliveGuard {
    fn drop(&mut self) {
        TASK_ALIVE.store(false, Ordering::SeqCst);
    }
}

async fn one_round(round: usize) {
    CONSUMER.store(0, Ordering::SeqCst);
    DISPATCH_ID.store(0, Ordering::SeqCst);
    APPLY_OUT.lock().unwrap().clear();
    TASK_ALIVE.store(false, Ordering::SeqCst);
    SLEEPING.store(false, Ordering::SeqCst);
    ABORT_CALLBACK_ENTERED.store(false, Ordering::SeqCst);
    SIDE_EFFECT.store(false, Ordering::SeqCst);

    let app = App::new();
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let entered2 = entered.clone();
    let release2 = release.clone();
    let _listener = app.root.on("cancel-close-race", move |_call| {
        TASK_ALIVE.store(true, Ordering::SeqCst);
        let guard = AliveGuard;
        let entered = entered2.clone();
        let release = release2.clone();
        Box::pin(async move {
            let _guard = guard;
            entered.notify_one();
            release.notified().await;
            // 同步占住一个 worker：取消/关闭都在这段窗口里并发发生。
            SLEEPING.store(true, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(400));
            SLEEPING.store(false, Ordering::SeqCst);
            tokio::task::yield_now().await;
            SIDE_EFFECT.store(true, Ordering::SeqCst);
            None
        })
    });

    let _fiber = app.plugin(consumer(), Config::Null).await.unwrap();
    assert!(
        APPLY_OUT.lock().unwrap().contains("\"pending\":true"),
        "第 {round} 轮：出站调用应先挂起"
    );
    let handle = CONSUMER.load(Ordering::SeqCst);
    let dispatch_id = DISPATCH_ID.load(Ordering::SeqCst);
    assert_ne!(handle, 0, "第 {round} 轮：consumer 句柄");
    assert_ne!(dispatch_id, 0, "第 {round} 轮：dispatch_id");

    tokio::time::timeout(Duration::from_secs(1), entered.notified())
        .await
        .expect("任务应当已经进入监听器");
    release.notify_one();
    let deadline = Instant::now() + Duration::from_secs(2);
    while !SLEEPING.load(Ordering::SeqCst) {
        assert!(
            Instant::now() < deadline,
            "第 {round} 轮：任务没有进入同步睡眠阶段"
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    let cancel = tokio::spawn(async move {
        let mut buffer = [0u8; 256];
        let mut out = Out {
            ptr: buffer.as_mut_ptr(),
            cap: buffer.len(),
            len: 0,
        };
        unsafe { host_api().ctx_abort.expect("ctx_abort")(handle, dispatch_id, &mut out) }
    });

    // 等取消路径真正进入慢回调：记录此刻已经摘掉。
    // 旧实现要到回调返回之后才登记 done；关闭在这里进入就能看到两张空表。
    let deadline = Instant::now() + Duration::from_secs(2);
    while !ABORT_CALLBACK_ENTERED.load(Ordering::SeqCst) {
        assert!(
            Instant::now() < deadline,
            "第 {round} 轮：取消路径没有进入回调"
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    // 取消仍在进行（回调 sleep 中），关闭并发进入。
    app.close().await;
    let status = cancel.await.expect("取消任务不得 panic");
    assert_eq!(status, STATUS_OK, "第 {round} 轮：ctx_abort 应成功");

    assert!(
        !TASK_ALIVE.load(Ordering::SeqCst),
        "第 {round} 轮：关闭在任务析构完成前返回（撞上了“已摘记录、未登记 done”的空档）"
    );
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert!(
        !SIDE_EFFECT.load(Ordering::SeqCst),
        "第 {round} 轮：被取消的任务在关闭后仍执行了副作用"
    );
    assert_eq!(loaded_instances(), 0, "第 {round} 轮：关闭后不得残留实例");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_abort_and_close_never_returns_before_task_teardown() {
    for round in 0..8 {
        one_round(round).await;
    }
}
