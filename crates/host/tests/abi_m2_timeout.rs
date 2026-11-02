//! M2：派发看门狗（`DispatchPolicy`）。
//!
//! **独立测试二进制**：看门狗是**进程级**设置，和别的测试共享进程会互相污染
//! （D10 的隔离纪律：需要绝对状态时用单独的文件）。
//!
//! 时间语义：`start_paused = true`（tokio test-util，M0 已开 feature）——
//! 运行时空闲时自动推进虚拟时间，看门狗因此是**确定性**触发的，不比墙钟。

use cordis_abi::{
    Blob, Out, OP_APPLY, OP_CALLBACK, OP_DISPOSE, STATUS_PENDING, STATUS_UNSUPPORTED,
};
use cordis_core::{App, Config, Ctx, ExternValue, FiberState, Inject, Plugin, Ret};
use cordis_host::{apply_via, host_api, pending_len, set_dispatch_policy, DispatchPolicy};
use serde_json::Value;
use std::sync::{Arc, Mutex};
use std::time::Duration;

static HANDLE: Mutex<u64> = Mutex::new(0);

fn json_of(ret: Ret) -> Option<Value> {
    ret.and_then(|value| value.downcast::<ExternValue>())
        .map(|value| value.0.clone())
}

unsafe extern "C" fn timeout_call(handle: u64, op: u32, _: Blob, _: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => {
            *HANDLE.lock().unwrap() = handle;
            // 宿主不支持 ctx_on ⇒ 原样返回 Unsupported（绝不在 extern "C" 里 panic）。
            let Some(api) = host_api().ctx_on else {
                return STATUS_UNSUPPORTED;
            };
            let mut buffer = [0u8; 64];
            let mut event_out = Out {
                ptr: buffer.as_mut_ptr(),
                cap: buffer.len(),
                len: 0,
            };
            api(handle, Blob::new(b"m2/timeout"), 7, &mut event_out);
            (*out).write(b"")
        }
        // 永不结算：等看门狗把它结算成超时，并把可读原因写入日志。
        OP_CALLBACK => STATUS_PENDING,
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

/// M2 红测试 10：`DispatchPolicy.timeout_ms` 到点后，未被 `ctx_fulfill` 的挂起
/// 续延被移出、日志写出可读的超时原因。
///
/// 这是**清理与措辞**证据，不证明调用者收到或能识别结构化 `Unknown`：
/// 事件回调没有返回协议（ADR 0003 §7 / ADR 0004 §2.6），该端到端断言在 A4。
#[tokio::test(start_paused = true)]
async fn dispatch_policy_timeout_settles_a_never_fulfilled_continuation_as_timeout() {
    set_dispatch_policy(DispatchPolicy {
        timeout_ms: Some(50),
        ..DispatchPolicy::default()
    });
    *HANDLE.lock().unwrap() = 0;

    let app = App::new();
    let logs: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let logs = logs.clone();
        app.store
            .set_log_sink(Arc::new(move |level, name, message| {
                logs.lock()
                    .unwrap()
                    .push(format!("{level:?}:{name}:{message}"));
            }));
    }

    let plugin: Arc<dyn Plugin> = Arc::new(cordis_core::FnPlugin {
        name: "m2-timeout",
        key: cordis_core::unique_key("m2-timeout"),
        inject: Inject::default(),
        validator: None,
        f: Arc::new(|ctx: Ctx, config: Config| apply_via(timeout_call, ctx, config, "m2-timeout")),
    });
    let fiber = app.root.plugin(plugin, Config::Null);
    fiber.settle().await;
    assert_eq!(fiber.state(), FiberState::Active);
    let handle = *HANDLE.lock().unwrap();

    let emit = {
        let ctx = app.root.clone();
        tokio::spawn(async move { ctx.bail("m2/timeout", vec![]).await })
    };

    // 只让出执行权（**不推进时间**）：等派发进入挂起。
    let mut suspended = false;
    for _ in 0..200 {
        if pending_len(handle) == 1 {
            suspended = true;
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(suspended, "派发必须先进入挂起（看门狗只对挂起生效）");

    // 现在等待：运行时空闲 ⇒ 自动推进虚拟时间 ⇒ 看门狗在 50ms 触发。
    let ret = tokio::time::timeout(Duration::from_secs(10), emit)
        .await
        .expect("看门狗必须结算这次派发（否则挂死）")
        .expect("派发任务不得 panic");
    assert_eq!(json_of(ret), None, "超时结算后监听器等于没有意见");

    assert_eq!(pending_len(handle), 0, "超时也必须把条目移出挂起表");
    let text = logs.lock().unwrap().join("\n");
    assert!(
        text.contains("等待插件结算超时") && !text.contains("业务失败"),
        "超时必须是可读原因（本测试不证明调用者 Unknown），不能写成业务失败：{text}"
    );

    // 复位（本文件独占进程，仍保持干净退出）。
    set_dispatch_policy(DispatchPolicy::default());
}
