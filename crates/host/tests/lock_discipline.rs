//! P0-4：回调期间的生命周期锁纪律。
//!
//! 两个场景共用同一个测试进程，避免 `loaded_instances` 全局量互相踩：
//! 1. 插件在自己的回调里 `ctx_dispose_self`：dispose 任务必须等回调退出读锁，
//!    然后完成，不死锁。
//! 2. 回调阻塞时并发 dispose：dispose 必须等回调释放读锁，不能与插件代码并发；
//!    释放后实例归零。

use cordis_abi::{Blob, Out, OP_APPLY, OP_CALLBACK, OP_DISPOSE, STATUS_OK};
use cordis_core::{App, Config, Ctx, FiberState, Inject, Plugin};
use cordis_host::{apply_via, host_api, loaded_instances};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const SELF_DISPOSE: u8 = 0;
const BLOCKING: u8 = 1;
static MODE: AtomicU8 = AtomicU8::new(SELF_DISPOSE);
static ENTERED: Mutex<Option<Sender<()>>> = Mutex::new(None);
static RELEASE: Mutex<Option<Receiver<()>>> = Mutex::new(None);

fn plugin(name: &'static str) -> Arc<dyn Plugin> {
    struct P(&'static str);
    impl Plugin for P {
        fn name(&self) -> &str {
            self.0
        }
        fn inject(&self) -> Inject {
            Inject::default()
        }
        fn apply(
            &self,
            ctx: Ctx,
            config: Config,
        ) -> cordis_core::BoxFuture<cordis_core::Result<()>> {
            apply_via(lock_call, ctx, config, self.0)
        }
    }
    Arc::new(P(name))
}

unsafe extern "C" fn lock_call(handle: u64, op: u32, _: Blob, _: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => {
            let api = host_api();
            let Some(on) = api.ctx_on else {
                return cordis_abi::STATUS_UNSUPPORTED;
            };
            let mut buffer = [0u8; 64];
            let mut event_out = Out {
                ptr: buffer.as_mut_ptr(),
                cap: buffer.len(),
                len: 0,
            };
            on(handle, Blob::new(b"lock/event"), 7, &mut event_out);
            (*out).write(b"")
        }
        OP_CALLBACK => {
            // 第一条回调：在自己的回调里请求卸载自己。
            // 第二条回调：阻塞到测试释放，用来验证 dispose 等待读锁。
            if MODE.load(Ordering::SeqCst) == SELF_DISPOSE {
                let api = host_api();
                if let Some(dispose) = api.ctx_dispose_self {
                    let mut buffer = [0u8; 64];
                    let mut dispose_out = Out {
                        ptr: buffer.as_mut_ptr(),
                        cap: buffer.len(),
                        len: 0,
                    };
                    dispose(handle, &mut dispose_out);
                }
                (*out).write(b"")
            } else {
                if let Some(tx) = ENTERED.lock().unwrap().take() {
                    let _ = tx.send(());
                }
                if let Some(rx) = RELEASE.lock().unwrap().take() {
                    let _ = rx.recv();
                }
                (*out).write(b"")
            }
        }
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_OK,
    }
}

async fn mount(app: &App, name: &'static str) -> Arc<cordis_core::Fiber> {
    let fiber = app.root.plugin(plugin(name), Config::Null);
    fiber.settle().await;
    assert_eq!(fiber.state(), FiberState::Active, "{name} 必须上线");
    fiber
}

async fn wait_loaded_zero() {
    for _ in 0..200 {
        if loaded_instances() == 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("实例必须在期限内置零");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn callback_dispose_waits_for_lifecycle_readers_without_deadlock() {
    // 场景 1：回调内自卸载。
    MODE.store(SELF_DISPOSE, Ordering::SeqCst);
    let app = App::new();
    let fiber = mount(&app, "lock-self").await;
    tokio::time::timeout(
        Duration::from_secs(5),
        app.root.bail("lock/event", Vec::new()),
    )
    .await
    .expect("自卸载回调不能死锁");
    wait_loaded_zero().await;

    // 场景 2：回调阻塞，dispose 必须等读锁释放。
    MODE.store(BLOCKING, Ordering::SeqCst);
    let (entered_tx, entered_rx) = channel();
    let (release_tx, release_rx) = channel();
    *ENTERED.lock().unwrap() = Some(entered_tx);
    *RELEASE.lock().unwrap() = Some(release_rx);
    let fiber2 = mount(&app, "lock-block").await;
    let event = {
        let root = app.root.clone();
        tokio::spawn(async move { root.bail("lock/event", Vec::new()).await })
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if entered_rx.try_recv().is_ok() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("插件回调必须进入");

    let dispose = tokio::spawn(async move { fiber2.dispose().await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!dispose.is_finished(), "dispose 必须等待在飞回调退出读锁");
    release_tx.send(()).expect("释放回调");
    tokio::time::timeout(Duration::from_secs(5), event)
        .await
        .expect("事件回调必须退出")
        .expect("事件任务不得 panic");
    tokio::time::timeout(Duration::from_secs(5), dispose)
        .await
        .expect("dispose 必须在回调退出后完成")
        .expect("dispose 任务不得 panic");
    wait_loaded_zero().await;
    assert_eq!(fiber.state(), FiberState::Disposed, "第一个实例必须已处置");
}
