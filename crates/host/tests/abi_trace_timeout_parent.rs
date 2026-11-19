//! 仅出站超时的记录必须挂回“发起时那一跳”，不能写死 0。
//!
//! 场景：T1 consumer → m@1（ABI）→ slow@1（ABI，PENDING 不结算）。
//! T1 消费者本身没有出站看门狗，只有 `m` 的入站回调里发起的那次 slow 调用会
//! 超时；记录 `call_id=0`，父必须是 `m` 的入站派发 id。

mod trace_support;

use cordis_abi::{
    Blob, Out, OP_APPLY, OP_CALLBACK, OP_DISPOSE, STATUS_OK, STATUS_PENDING, STATUS_UNSUPPORTED,
};
use cordis_core::{App, Config, Ctx, FiberState, Inject, Outcome, Plugin};
use cordis_host::{host_api, set_dispatch_policy, DispatchPolicy};
use serde_json::{json, Value};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use trace_support::{abi_plugin, call_iface, collect, provide};

static MID_DISPATCH: Mutex<u64> = Mutex::new(0);
static T1_RESULT: Mutex<Option<bool>> = Mutex::new(None);

fn result_notify() -> &'static tokio::sync::Notify {
    static NOTIFY: OnceLock<tokio::sync::Notify> = OnceLock::new();
    NOTIFY.get_or_init(tokio::sync::Notify::new)
}

unsafe extern "C" fn slow_call(handle: u64, op: u32, _a: Blob, _b: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => provide(handle, "slow@1", r#"["run"]"#, 1, out),
        OP_CALLBACK => {
            (*out).write(b"");
            STATUS_PENDING
        }
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

unsafe extern "C" fn mid_call(handle: u64, op: u32, a: Blob, _b: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => provide(handle, "m@1", r#"["run"]"#, 2, out),
        OP_CALLBACK => {
            let payload: Value = serde_json::from_slice(a.as_bytes()).unwrap_or(Value::Null);
            if payload.get("id").and_then(Value::as_u64) == Some(2) {
                let Some(dispatch_id) = payload.get("dispatch_id").and_then(Value::as_u64) else {
                    return STATUS_UNSUPPORTED;
                };
                *MID_DISPATCH.lock().unwrap() = dispatch_id;
                let (status, text) = call_iface(handle, "slow@1", "run", 7);
                if status != STATUS_OK {
                    (*out).write(text.as_bytes());
                    return status;
                }
                let reply: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                if reply.get("pending").and_then(Value::as_bool) == Some(true) {
                    (*out).write(b"");
                    return STATUS_PENDING;
                }
                return STATUS_UNSUPPORTED;
            }
            if payload.get("id").and_then(Value::as_u64) == Some(7) {
                let Some(abort) = host_api().ctx_abort else {
                    return STATUS_UNSUPPORTED;
                };
                let mut buffer = [0u8; 256];
                let mut slot = Out {
                    ptr: buffer.as_mut_ptr(),
                    cap: buffer.len(),
                    len: 0,
                };
                if unsafe { abort(handle, *MID_DISPATCH.lock().unwrap(), &mut slot) } != STATUS_OK {
                    return cordis_abi::STATUS_ERR;
                }
                (*out).write(b"");
                return STATUS_OK;
            }
            STATUS_UNSUPPORTED
        }
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

fn t1_consumer() -> std::sync::Arc<dyn Plugin> {
    cordis_core::plugin_fn(
        "trace-parent-t1",
        Inject::of(&["m@1"]),
        |ctx: Ctx, _config: Config| {
            Box::pin(async move {
                let result = ctx.call_iface("m@1", 1, "run", json!([])).await;
                *T1_RESULT.lock().unwrap() = Some(result.is_err());
                result_notify().notify_one();
                Ok(())
            })
        },
    )
}

#[tokio::test]
async fn outbound_timeout_is_parented_to_the_hop_that_started_it() {
    *MID_DISPATCH.lock().unwrap() = 0;
    *T1_RESULT.lock().unwrap() = None;
    set_dispatch_policy(DispatchPolicy::new(None, Some(50)));

    let app = App::new();
    let records = collect(&app);

    let slow = {
        let fiber = app.root.plugin(
            abi_plugin("trace-slow", Inject::default(), slow_call),
            Config::Null,
        );
        fiber.settle().await;
        fiber
    };
    let mid = {
        let fiber = app.root.plugin(
            abi_plugin("trace-mid-parent", Inject::of(&["slow@1"]), mid_call),
            Config::Null,
        );
        fiber.settle().await;
        fiber
    };
    let consumer = {
        let fiber = app.root.plugin(t1_consumer(), Config::Null);
        fiber.settle().await;
        fiber
    };
    for fiber in [&slow, &mid, &consumer] {
        assert_eq!(fiber.state(), FiberState::Active);
    }

    tokio::time::timeout(Duration::from_secs(2), async {
        while T1_RESULT.lock().unwrap().is_none() {
            result_notify().notified().await;
        }
    })
    .await
    .expect("出站超时必须在时限内结算 T1 调用");
    assert_eq!(*T1_RESULT.lock().unwrap(), Some(true));

    {
        let records = records.lock().unwrap();
        assert_eq!(records.len(), 2, "只有超时与 m 的结算两条：{records:?}");
        assert_eq!(
            records
                .iter()
                .filter(|record| &*record.name == "slow@1.run")
                .count(),
            1
        );
        assert_eq!(
            records
                .iter()
                .filter(|record| &*record.name == "m@1.run")
                .count(),
            1
        );
        let timeout_record = records
            .iter()
            .find(|record| record.call_id == 0 && &*record.name == "slow@1.run")
            .expect("仅出站超时必须补写一条 call_id=0 的记录");
        let mid_record = records
            .iter()
            .find(|record| &*record.name == "m@1.run")
            .expect("中间一跳必须有入站记录");
        assert_eq!(timeout_record.outcome, Outcome::Err("HOST_TIMEOUT".into()));
        assert_eq!(
            timeout_record.parent_call_id, mid_record.call_id,
            "父必须取发起时 CALL_SCOPE 栈顶，不能写死 0"
        );
        assert_ne!(mid_record.call_id, 0);
    }

    consumer.dispose().await;
    mid.dispose().await;
    slow.dispose().await;
    set_dispatch_policy(DispatchPolicy::default());
}
