//! 资源增长基线（阶段 0 #6）。
//!
//! 覆盖：8 轮 × 成功 / 失败 / 丢弃 Future / 带在飞派发卸载 / waterfall /
//! stream / 同 key 重载。判据：实例、派发、挂起、派发帧、流、effect 计数回到基线。
//! 暂存台账由 `crates/host/tests/staged.rs` 的多轮用例覆盖。
//!
//! 正向对照：
//! - 每次普通监听器 / waterfall / stream 回调必须真的跑一次；
//! - 丢弃 Future / 卸载前必须先观测到 `pending_len == 1`；
//! - waterfall / stream 必须先观测到对应计数为 1；
//! - mount 后 `effect_count` 必须等于全部登记数；重载后仍回到这个数；
//! - 同 key 重载必须重新 apply（`APPLIES` +1）且不留下旧实例。
//!
//! 本文件必须保持**单测试**：`loaded_instances` 是进程级计数。

use cordis_abi::{
    Blob, Out, KIND_EVENT, KIND_IFACE, OP_APPLY, OP_CALLBACK, OP_CLEANUP, OP_DISPOSE, STATUS_ERR,
    STATUS_OK, STATUS_PENDING, STATUS_UNSUPPORTED,
};
use cordis_core::{App, BoxFuture, Config, Ctx, FiberState, Inject, Plugin, Ret};
use cordis_host::{
    apply_via, dispatches_len, host_api, loaded_instances, pending_len, set_dispatch_policy,
    streams_len, waterfall_len, DispatchPolicy,
};
use serde_json::{json, Value};
use std::future::Future;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

const OK: u8 = 0;
const ERR: u8 = 1;
const PENDING: u8 = 2;

/// 回调 id 各自独立：普通事件 / iface / waterfall / cleanup。
const EVENT: u64 = 10;
const IFACE: u64 = 11;
const WATERFALL: u64 = 12;
const CLEANUP: u64 = 13;

/// `apply_via` + 两个 listener + iface 提供 + `ctx_effect`。
const EXPECTED_EFFECTS: usize = 5;

static MODE: AtomicU8 = AtomicU8::new(OK);
static HANDLE: AtomicU64 = AtomicU64::new(0);
static CALLS: AtomicU64 = AtomicU64::new(0);
static APPLIES: AtomicU64 = AtomicU64::new(0);
static WATERFALL_CALLS: AtomicU64 = AtomicU64::new(0);
static STREAM_CALLS: AtomicU64 = AtomicU64::new(0);

unsafe extern "C" fn resource_call(handle: u64, op: u32, a: Blob, _b: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => {
            APPLIES.fetch_add(1, Ordering::SeqCst);
            HANDLE.store(handle, Ordering::SeqCst);

            let mut raw = [0u8; 256];
            let mut probe = Out {
                ptr: raw.as_mut_ptr(),
                cap: raw.len(),
                len: 0,
            };
            let Some(on) = host_api().ctx_on else {
                return STATUS_UNSUPPORTED;
            };
            if on(handle, Blob::new(b"baseline/event"), EVENT, &mut probe) != STATUS_OK {
                return STATUS_ERR;
            }

            let mut raw = [0u8; 256];
            let mut probe = Out {
                ptr: raw.as_mut_ptr(),
                cap: raw.len(),
                len: 0,
            };
            if on(
                handle,
                Blob::new(b"baseline/waterfall"),
                WATERFALL,
                &mut probe,
            ) != STATUS_OK
            {
                return STATUS_ERR;
            }

            let mut raw = [0u8; 256];
            let mut probe = Out {
                ptr: raw.as_mut_ptr(),
                cap: raw.len(),
                len: 0,
            };
            let Some(provide) = host_api().ctx_provide_iface else {
                return STATUS_UNSUPPORTED;
            };
            if provide(
                handle,
                Blob::new(b"baseline.stream@1"),
                1,
                Blob::new(br#"["stream"]"#),
                IFACE,
                &mut probe,
            ) != STATUS_OK
            {
                return STATUS_ERR;
            }

            let mut raw = [0u8; 256];
            let mut probe = Out {
                ptr: raw.as_mut_ptr(),
                cap: raw.len(),
                len: 0,
            };
            let Some(effect) = host_api().ctx_effect else {
                return STATUS_UNSUPPORTED;
            };
            if effect(handle, CLEANUP, &mut probe) != STATUS_OK {
                return STATUS_ERR;
            }

            (*out).write(b"")
        }
        OP_CALLBACK => {
            let payload: Value = serde_json::from_slice(a.as_bytes()).unwrap_or(Value::Null);
            match payload.get("kind").and_then(Value::as_str).unwrap_or("") {
                KIND_IFACE => {
                    STREAM_CALLS.fetch_add(1, Ordering::SeqCst);
                    // 不推块：让宿主侧留下一条 stream 记录，消费者 drop 时收场。
                    STATUS_PENDING
                }
                KIND_EVENT => {
                    let id = payload.get("id").and_then(Value::as_u64).unwrap_or(0);
                    if id == WATERFALL {
                        WATERFALL_CALLS.fetch_add(1, Ordering::SeqCst);
                        STATUS_PENDING
                    } else {
                        CALLS.fetch_add(1, Ordering::SeqCst);
                        match MODE.load(Ordering::SeqCst) {
                            OK => {
                                (*out).write(br#"{"ret":"ok"}"#);
                                STATUS_OK
                            }
                            ERR => {
                                (*out).write(b"baseline failure");
                                STATUS_ERR
                            }
                            _ => STATUS_PENDING,
                        }
                    }
                }
                _ => {
                    (*out).write(b"");
                    STATUS_OK
                }
            }
        }
        OP_CLEANUP | OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

fn plugin() -> Arc<dyn Plugin> {
    Arc::new(cordis_core::FnPlugin {
        name: "resource-baseline",
        key: cordis_core::unique_key("resource-baseline"),
        inject: Inject::default(),
        validator: None,
        f: Arc::new(|ctx: Ctx, config: Config| {
            apply_via(resource_call, ctx, config, "resource-baseline")
        }),
    })
}

/// 每个 await 都要有期限：卡住应当是失败，不是挂死 CI。
async fn within<T>(what: &str, future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(2), future)
        .await
        .unwrap_or_else(|_| panic!("{what} 超时"))
}

async fn wait_for_pending(handle: u64, cycle: usize) {
    for _ in 0..400 {
        if pending_len(handle) == 1 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    panic!("第 {cycle} 轮必须真的制造在飞记录");
}

async fn wait_for_waterfall(handle: u64, cycle: usize) {
    for _ in 0..400 {
        if waterfall_len(handle) == 1 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    panic!("第 {cycle} 轮必须真的制造 waterfall 帧");
}

async fn wait_until_zero(handle: u64) {
    for _ in 0..400 {
        if pending_len(handle) == 0
            && dispatches_len(handle) == 0
            && waterfall_len(handle) == 0
            && streams_len(handle) == 0
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert_no_residue(handle, "归零等待后");
}

fn assert_no_residue(handle: u64, name: &str) {
    assert_eq!(pending_len(handle), 0, "{name}：不得留下挂起条目");
    assert_eq!(dispatches_len(handle), 0, "{name}：不得留下派发记录");
    assert_eq!(waterfall_len(handle), 0, "{name}：不得留下派发帧");
    assert_eq!(streams_len(handle), 0, "{name}：不得留下流");
}

fn assert_task_did_not_panic(joined: Result<Ret, tokio::task::JoinError>, name: &str) {
    match joined {
        Ok(_) => {}
        Err(err) if err.is_cancelled() => {}
        Err(err) => panic!("{name}：任务不得 panic：{err:?}"),
    }
}

async fn mount(app: &App, name: &str) -> (Arc<cordis_core::Fiber>, u64) {
    let fiber = app.root.plugin(plugin(), Config::Null);
    within(&format!("{name} settle"), fiber.settle()).await;
    assert_eq!(fiber.state(), FiberState::Active, "{name} 必须激活");
    let handle = HANDLE.load(Ordering::SeqCst);
    assert_ne!(handle, 0, "{name} 必须拿到实例句柄");
    assert_eq!(
        fiber.effect_count(),
        EXPECTED_EFFECTS,
        "{name} 应有 apply_via + 2 listener + iface + ctx_effect"
    );
    (fiber, handle)
}

#[tokio::test]
async fn repeated_cycles_return_to_baseline() {
    // 进程级看门狗若被设过，超时会替 `PendingDropGuard` 撤销，红档会失效。
    set_dispatch_policy(DispatchPolicy::default());
    assert_eq!(loaded_instances(), 0, "测试开始前不得有残留实例");

    let app = App::new();
    const CYCLES: usize = 8;

    for cycle in 0..CYCLES {
        // ① 成功 / 失败：返回值必须可区分。
        for (mode, name) in [(OK, "成功"), (ERR, "失败")] {
            MODE.store(mode, Ordering::SeqCst);
            let before = CALLS.load(Ordering::SeqCst);
            let (fiber, handle) = mount(&app, name).await;

            let outcome = within(
                &format!("{name} bail"),
                app.root.bail("baseline/event", vec![]),
            )
            .await;
            assert_eq!(
                CALLS.load(Ordering::SeqCst),
                before + 1,
                "{name}轮监听器必须真的跑一次"
            );
            if mode == OK {
                assert!(outcome.is_some(), "成功轮必须带回返回值");
            } else {
                assert!(outcome.is_none(), "失败轮必须被当作“没有意见”");
            }
            assert_no_residue(handle, name);

            within(&format!("{name} dispose"), fiber.dispose()).await;
            assert_eq!(fiber.effect_count(), 0, "{name}轮恢复栈必须清空");
            assert_eq!(
                loaded_instances(),
                0,
                "{name}轮实例必须回收（cycle {cycle}）"
            );
        }

        // ② 丢弃 Future：先证明真的制造了在飞记录，再丢弃。
        MODE.store(PENDING, Ordering::SeqCst);
        let before = CALLS.load(Ordering::SeqCst);
        let (fiber, handle) = mount(&app, "丢弃轮").await;

        let task = tokio::spawn(app.root.bail("baseline/event", vec![]));
        wait_for_pending(handle, cycle).await;
        assert_eq!(
            CALLS.load(Ordering::SeqCst),
            before + 1,
            "丢弃轮监听器必须真的跑一次"
        );

        task.abort();
        let joined = within("丢弃轮任务", task).await;
        assert_task_did_not_panic(joined, "丢弃轮");
        within("丢弃轮归零", wait_until_zero(handle)).await;
        assert_no_residue(handle, "丢弃轮");

        within("丢弃轮 dispose", fiber.dispose()).await;
        assert_eq!(fiber.effect_count(), 0);
        assert_eq!(loaded_instances(), 0, "丢弃轮实例必须回收（cycle {cycle}）");

        // ③ 带在飞派发卸载：不 abort，直接 dispose，走宿主排水路径。
        MODE.store(PENDING, Ordering::SeqCst);
        let before = CALLS.load(Ordering::SeqCst);
        let (fiber, handle) = mount(&app, "卸载轮").await;

        let task = tokio::spawn(app.root.bail("baseline/event", vec![]));
        wait_for_pending(handle, cycle).await;
        assert_eq!(
            CALLS.load(Ordering::SeqCst),
            before + 1,
            "卸载轮监听器必须真的跑一次"
        );

        within("带在飞派发卸载", fiber.dispose()).await;
        let joined = within("卸载轮任务", task).await;
        assert!(
            joined.is_ok(),
            "卸载排水后调用方应正常结束，而不是 panic：{joined:?}"
        );
        assert_no_residue(handle, "卸载轮");
        assert_eq!(fiber.effect_count(), 0);
        assert_eq!(loaded_instances(), 0, "卸载轮实例必须回收（cycle {cycle}）");

        // ④ waterfall：制造帧，再丢弃调用方 Future。
        MODE.store(OK, Ordering::SeqCst);
        let waterfall_before = WATERFALL_CALLS.load(Ordering::SeqCst);
        let (fiber, handle) = mount(&app, "waterfall 轮").await;
        let tail: BoxFuture<Ret> = Box::pin(async { None });
        let task = tokio::spawn(
            app.root
                .waterfall("baseline/waterfall", vec![], Some(tail), None),
        );
        wait_for_waterfall(handle, cycle).await;
        assert_eq!(
            WATERFALL_CALLS.load(Ordering::SeqCst),
            waterfall_before + 1,
            "waterfall 监听器必须真的跑一次"
        );
        assert_eq!(dispatches_len(handle), 1, "帧必须挂在派发记录上");

        task.abort();
        let joined = within("waterfall 任务", task).await;
        assert_task_did_not_panic(joined, "waterfall");
        within("waterfall 归零", wait_until_zero(handle)).await;
        assert_no_residue(handle, "waterfall 轮");

        within("waterfall dispose", fiber.dispose()).await;
        assert_eq!(fiber.effect_count(), 0);
        assert_eq!(
            loaded_instances(),
            0,
            "waterfall 轮实例必须回收（cycle {cycle}）"
        );

        // ⑤ stream：提供者返回 PENDING 且不推块，宿主侧留下流记录。
        MODE.store(OK, Ordering::SeqCst);
        let stream_before = STREAM_CALLS.load(Ordering::SeqCst);
        let (fiber, handle) = mount(&app, "stream 轮").await;
        let stream = app
            .root
            .call_iface_stream("baseline.stream@1", 1, "stream", json!([]))
            .expect("开流必须成功");
        assert_eq!(
            STREAM_CALLS.load(Ordering::SeqCst),
            stream_before + 1,
            "stream 提供者必须真的被调用一次"
        );
        assert_eq!(streams_len(handle), 1, "开流后宿主要记着这条流");
        assert_eq!(pending_len(handle), 1, "流在宿主侧就是在飞派发");

        drop(stream);
        within("stream 归零", wait_until_zero(handle)).await;
        assert_no_residue(handle, "stream 轮");

        within("stream dispose", fiber.dispose()).await;
        assert_eq!(fiber.effect_count(), 0);
        assert_eq!(
            loaded_instances(),
            0,
            "stream 轮实例必须回收（cycle {cycle}）"
        );

        // ⑥ 同 key 重载：`fiber.update` 必须在同一 fiber 上重跑 apply。
        MODE.store(OK, Ordering::SeqCst);
        let applies_before = APPLIES.load(Ordering::SeqCst);
        let (fiber, old_handle) = mount(&app, "重载轮").await;
        assert_eq!(
            APPLIES.load(Ordering::SeqCst),
            applies_before + 1,
            "首次挂载必须 apply 一次"
        );

        within("重载 update", fiber.update(json!({ "n": 2 })))
            .await
            .expect("重载必须成功");
        assert_eq!(
            APPLIES.load(Ordering::SeqCst),
            applies_before + 2,
            "同 key 重载必须重新 apply"
        );
        let new_handle = HANDLE.load(Ordering::SeqCst);
        assert_ne!(new_handle, old_handle, "重载必须换成新一代实例");
        assert_eq!(loaded_instances(), 1, "重载不得留下旧实例");
        assert_eq!(
            fiber.effect_count(),
            EXPECTED_EFFECTS,
            "重载后登记数必须回到基线"
        );
        assert_no_residue(new_handle, "重载轮");

        let calls_before = CALLS.load(Ordering::SeqCst);
        let outcome = within("重载轮 bail", app.root.bail("baseline/event", vec![])).await;
        assert!(outcome.is_some(), "重载后的监听器必须仍然工作");
        assert_eq!(
            CALLS.load(Ordering::SeqCst),
            calls_before + 1,
            "重载后监听器必须真的跑一次"
        );

        within("重载 dispose", fiber.dispose()).await;
        assert_eq!(fiber.effect_count(), 0);
        assert_eq!(loaded_instances(), 0, "重载轮实例必须回收（cycle {cycle}）");
    }
}
