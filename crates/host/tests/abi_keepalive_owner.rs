//! O4 回归：公开 keepalive（`apply_via_with_keepalive`）的唯一 Arc owner 必须活到
//! `unregister_instance`，而不是在 `apply` 返回时就被丢掉。
//!
//! 断言全部针对进程级状态（实例表 / 全局计数），所以本二进制只放**一个** test：
//! 多个 test 并行会互相污染计数与槽位代际。
//!
//! fake C 入口只记录，**禁止** assert/panic/unwrap —— `extern "C"` 内 panic 会
//! abort 整个测试二进制，把"断言失败"变成"进程消失"。

use cordis_abi::{Blob, Out, OP_APPLY, OP_CALLBACK, OP_DISPOSE, STATUS_OK, STATUS_UNSUPPORTED};
use cordis_core::{App, Config, Ctx, FnPlugin, Inject, Plugin};
use cordis_host::{apply_via_with_keepalive, host_api};
use std::any::Any;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const EVENT: &[u8] = b"abi-keepalive";

static EVENT_CALLS: AtomicUsize = AtomicUsize::new(0);
static DISPOSES: AtomicUsize = AtomicUsize::new(0);

/// 模拟"动态库"：Drop 即 FreeLibrary。用探针而不是 `Arc::strong_count` 判定，
/// 因为计数会被监听器闭包等合法强引用扰动。
struct DropProbe(Arc<AtomicBool>);

impl Drop for DropProbe {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

fn make_out(buffer: &mut [u8]) -> Out {
    Out {
        ptr: buffer.as_mut_ptr(),
        cap: buffer.len(),
        len: 0,
    }
}

unsafe extern "C" fn probe_call(handle: u64, op: u32, _a: Blob, _b: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => {
            let Some(on) = host_api().ctx_on else {
                return STATUS_UNSUPPORTED;
            };
            let mut buffer = [0u8; 64];
            let mut slot = make_out(&mut buffer);
            let status = on(handle, Blob::new(EVENT), 1, &mut slot);
            if status != STATUS_OK {
                return status;
            }
            (*out).write(b"")
        }
        OP_CALLBACK => {
            EVENT_CALLS.fetch_add(1, Ordering::SeqCst);
            (*out).write(br#"{"ret":null}"#)
        }
        OP_DISPOSE => {
            DISPOSES.fetch_add(1, Ordering::SeqCst);
            (*out).write(b"")
        }
        _ => STATUS_UNSUPPORTED,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_keepalive_owner_lives_until_unregister() {
    EVENT_CALLS.store(0, Ordering::SeqCst);
    DISPOSES.store(0, Ordering::SeqCst);

    let dropped = Arc::new(AtomicBool::new(false));
    let owner: Arc<dyn Any + Send + Sync> = Arc::new(DropProbe(dropped.clone()));
    // 唯一 Arc：移交给 apply 之后，除了实例表槽位没人再持有它。
    let handoff: Arc<Mutex<Option<Arc<dyn Any + Send + Sync>>>> = Arc::new(Mutex::new(Some(owner)));
    let handoff_for_apply = handoff.clone();

    let plugin: Arc<dyn Plugin> = Arc::new(FnPlugin {
        name: "abi-keepalive-owner",
        key: cordis_core::unique_key("abi-keepalive-owner"),
        inject: Inject::default(),
        validator: None,
        f: Arc::new(move |ctx: Ctx, config: Config| {
            let owner = handoff_for_apply
                .lock()
                .ok()
                .and_then(|mut slot| slot.take());
            apply_via_with_keepalive(probe_call, ctx, config, "abi-keepalive-owner", owner)
        }),
    });

    let app = App::new();
    let fiber = app.root.plugin(plugin, Config::Null);
    fiber.settle().await;

    let dropped_after_apply = dropped.load(Ordering::SeqCst);

    // 保护期内：事件监听器必须真的被调用一次。旧实现 registration 时就 downgrade
    // 并立刻丢弃唯一强引用，弱引用当场就 dead，call_inbound 一律拒绝（0 次）。
    tokio::time::timeout(
        Duration::from_secs(2),
        app.root.emit("abi-keepalive", Vec::new()),
    )
    .await
    .expect("emit 必须在看门狗内完成");
    let event_calls = EVENT_CALLS.load(Ordering::SeqCst);
    let dropped_after_emit = dropped.load(Ordering::SeqCst);

    app.close().await;

    let disposes = DISPOSES.load(Ordering::SeqCst);
    // 关闭后探针最终释放：不是永久钉住，也没有环。
    let mut released = dropped.load(Ordering::SeqCst);
    for _ in 0..2_000 {
        if released {
            break;
        }
        tokio::task::yield_now().await;
        released = dropped.load(Ordering::SeqCst);
    }
    // 提前把观察值打出来：红跑时第一条断言就会中断，剩下的事实也要可见。
    eprintln!(
        "observations: dropped_after_apply={dropped_after_apply} \
         event_calls={event_calls} dropped_after_emit={dropped_after_emit} \
         disposes={disposes} released_after_close={released}"
    );

    assert!(
        !dropped_after_apply,
        "apply 成功后探针不得 drop：唯一强 owner 必须随注册移入实例表槽位"
    );
    assert_eq!(event_calls, 1, "保护期内事件监听器必须被调用一次");
    assert!(!dropped_after_emit, "保护期内（emit 之后）探针仍不得 drop");
    assert_eq!(disposes, 1, "app.close() 后 OP_DISPOSE 必须恰好一次");
    assert!(
        released,
        "关闭后探针最终必须 drop（不能永久钉住库，也不能有环）"
    );
}
