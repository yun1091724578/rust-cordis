//! 资源 / 暂存 soak（A5）。
//!
//! 默认被 `#[ignore]` 跳过；用 `cargo xtask soak [cycles]` 运行。每轮做
//! 成功 / 失败；每 4 轮额外丢弃在飞 Future，每 3 轮额外做同 key 重载；
//! 每轮另外做两轮暂存台账 `stage -> adopt -> drop`。判据是实例 / 派发 /
//! 挂起 / effect / 暂存台账全部回到基线。
//!
//! 暂存部分是 `stage_plugin` / `StagedFile` 的 primitive 记账；真 DLL 的
//! `LoadedPlugin::open_staged` 由冻结夹具 / e2e 覆盖。
//!
//! 本文件必须保持**单测试**：`loaded_instances` / `staged_leaked` 是进程级
//! 计数，第二个测试会互相踩。

use cordis_abi::{
    Blob, Out, KIND_EVENT, OP_APPLY, OP_CALLBACK, OP_CLEANUP, OP_DISPOSE, STATUS_ERR, STATUS_OK,
    STATUS_PENDING, STATUS_UNSUPPORTED,
};
use cordis_core::{App, Config, Ctx, FiberState, Inject, Plugin, Ret};
use cordis_host::{
    apply_via, dispatches_len, host_api, loaded_instances, pending_len, set_dispatch_policy,
    stage_plugin, staged_leaked, DispatchPolicy, StagedFile,
};
use serde_json::{json, Value};
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

const OK: u8 = 0;
const ERR: u8 = 1;
const PENDING: u8 = 2;
const EVENT: u64 = 1;
const CLEANUP: u64 = 2;
/// `apply_via` + 一个事件监听器 + 一个 `ctx_effect`。
const EXPECTED_EFFECTS: usize = 3;

static MODE: AtomicU8 = AtomicU8::new(OK);
static HANDLE: AtomicU64 = AtomicU64::new(0);
static CALLS: AtomicU64 = AtomicU64::new(0);
static APPLIES: AtomicU64 = AtomicU64::new(0);
static CLEANUPS: AtomicU64 = AtomicU64::new(0);
static DISPOSES: AtomicU64 = AtomicU64::new(0);

unsafe extern "C" fn soak_call(handle: u64, op: u32, a: Blob, _b: Blob, out: *mut Out) -> i32 {
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
            if on(handle, Blob::new(b"soak/event"), EVENT, &mut probe) != STATUS_OK {
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
            if payload.get("kind").and_then(Value::as_str) != Some(KIND_EVENT) {
                (*out).write(b"");
                return STATUS_OK;
            }
            let id = payload.get("id").and_then(Value::as_u64).unwrap_or(0);
            if id != EVENT {
                (*out).write(b"");
                return STATUS_OK;
            }
            CALLS.fetch_add(1, Ordering::SeqCst);
            match MODE.load(Ordering::SeqCst) {
                OK => {
                    (*out).write(br#"{"ret":"ok"}"#);
                    STATUS_OK
                }
                ERR => {
                    (*out).write(b"soak failure");
                    STATUS_ERR
                }
                _ => STATUS_PENDING,
            }
        }
        OP_CLEANUP => {
            CLEANUPS.fetch_add(1, Ordering::SeqCst);
            (*out).write(b"")
        }
        OP_DISPOSE => {
            DISPOSES.fetch_add(1, Ordering::SeqCst);
            (*out).write(b"")
        }
        _ => STATUS_UNSUPPORTED,
    }
}

fn plugin() -> Arc<dyn Plugin> {
    Arc::new(cordis_core::FnPlugin {
        name: "soak",
        key: cordis_core::unique_key("soak"),
        inject: Inject::default(),
        validator: None,
        f: Arc::new(|ctx: Ctx, config: Config| apply_via(soak_call, ctx, config, "soak")),
    })
}

fn cycles() -> usize {
    let raw = std::env::var("CORDIS_SOAK_CYCLES").unwrap_or_else(|_| "1000".to_string());
    let cycles = raw
        .parse::<usize>()
        .ok()
        .filter(|cycles| *cycles > 0)
        .unwrap_or_else(|| panic!("CORDIS_SOAK_CYCLES 必须是正整数，实际 {raw:?}"));
    eprintln!("soak: {cycles} cycles");
    cycles
}

async fn within<T>(what: &str, future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(5), future)
        .await
        .unwrap_or_else(|_| panic!("{what} 超时"))
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
        "{name} 的 effect 数必须回到基线"
    );
    (fiber, handle)
}

async fn wait_for_pending(handle: u64, cycle: usize) {
    for _ in 0..5000 {
        if pending_len(handle) == 1 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    panic!("第 {cycle} 轮必须制造在飞记录");
}

async fn wait_until_zero(handle: u64) {
    for _ in 0..5000 {
        if pending_len(handle) == 0 && dispatches_len(handle) == 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert_no_residue(handle, "归零等待后");
}

fn assert_no_residue(handle: u64, name: &str) {
    assert_eq!(pending_len(handle), 0, "{name}：不得留下挂起条目");
    assert_eq!(dispatches_len(handle), 0, "{name}：不得留下派发记录");
}

fn assert_store_baseline(app: &App, name: &str) {
    assert_eq!(app.store.runtime_count(), 0, "{name}：不得留下 runtime");
    assert_eq!(
        app.store.hook_count("soak/event"),
        0,
        "{name}：不得留下事件 hook"
    );
}

fn assert_task_did_not_panic(joined: Result<Ret, tokio::task::JoinError>, name: &str) {
    match joined {
        Ok(_) => {}
        Err(err) if err.is_cancelled() => {}
        Err(err) => panic!("{name}：任务不得 panic：{err:?}"),
    }
}

async fn dispose_clean(app: &App, fiber: &Arc<cordis_core::Fiber>, handle: u64, name: &str) {
    // 必须在 dispose 之前看：dispose 之后句柄失效，计数天然是 0，断言会变空。
    assert_no_residue(handle, name);
    within(&format!("{name} dispose"), fiber.dispose()).await;
    assert!(
        fiber.cleanup_error().is_none(),
        "{name}：清理不得报告异常：{:?}",
        fiber.cleanup_error()
    );
    assert_eq!(fiber.effect_count(), 0, "{name}：恢复栈必须清空");
    assert_eq!(loaded_instances(), 0, "{name}：实例必须回收");
    assert_store_baseline(app, name);
}

fn fixture_source() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cordis-soak-staged-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("建临时目录");
    let source = dir.join("soak.dll");
    std::fs::write(&source, b"stub-not-a-real-dll").expect("造假源文件");
    source
}

fn unique_revision() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    std::process::id() as u64 * 1_000_000 + NEXT.fetch_add(1, Ordering::SeqCst)
}

#[tokio::test]
#[ignore = "run via cargo xtask soak"]
async fn repeated_soak_cycles_leave_no_residue() {
    let cycles = cycles();
    set_dispatch_policy(DispatchPolicy::default());
    assert_eq!(loaded_instances(), 0, "soak 开始前不得有残留实例");
    let staged_before = staged_leaked();
    assert_eq!(staged_before, 0, "soak 开始前暂存台账必须为零");
    let cleanups_before = CLEANUPS.load(Ordering::SeqCst);
    let disposes_before = DISPOSES.load(Ordering::SeqCst);
    let source = fixture_source();

    let app = App::new();
    // 失败轮是刻意制造的；soak 自己的 stdout 只需要最终结果，不刷错误日志。
    app.store.set_log_sink(Arc::new(|_, _, _| {}));
    assert_store_baseline(&app, "soak 开始前");
    for cycle in 0..cycles {
        // ① 成功：返回值必须存在，监听器必须真的跑一次。
        MODE.store(OK, Ordering::SeqCst);
        let before = CALLS.load(Ordering::SeqCst);
        let (fiber, handle) = mount(&app, "成功轮").await;
        let outcome = within("成功轮 bail", app.root.bail("soak/event", vec![])).await;
        assert!(outcome.is_some(), "成功轮必须带回返回值（cycle {cycle}）");
        assert_eq!(
            CALLS.load(Ordering::SeqCst),
            before + 1,
            "成功轮监听器必须真的跑一次（cycle {cycle}）"
        );
        dispose_clean(&app, &fiber, handle, "成功轮").await;

        // ② 失败：返回 None，但仍必须回收。
        MODE.store(ERR, Ordering::SeqCst);
        let before = CALLS.load(Ordering::SeqCst);
        let (fiber, handle) = mount(&app, "失败轮").await;
        let outcome = within("失败轮 bail", app.root.bail("soak/event", vec![])).await;
        assert!(
            outcome.is_none(),
            "失败轮必须被当作没有意见（cycle {cycle}）"
        );
        assert_eq!(
            CALLS.load(Ordering::SeqCst),
            before + 1,
            "失败轮监听器必须真的跑一次（cycle {cycle}）"
        );
        dispose_clean(&app, &fiber, handle, "失败轮").await;

        // ③ 丢弃在飞 Future：先证明 pending_len == 1，再 abort。
        if cycle % 4 == 0 {
            MODE.store(PENDING, Ordering::SeqCst);
            let before = CALLS.load(Ordering::SeqCst);
            let (fiber, handle) = mount(&app, "丢弃轮").await;
            let task = tokio::spawn(app.root.bail("soak/event", vec![]));
            wait_for_pending(handle, cycle).await;
            assert_eq!(
                CALLS.load(Ordering::SeqCst),
                before + 1,
                "丢弃轮监听器必须真的跑一次（cycle {cycle}）"
            );
            task.abort();
            let joined = within("丢弃轮任务", task).await;
            assert_task_did_not_panic(joined, "丢弃轮");
            within("丢弃轮归零", wait_until_zero(handle)).await;
            dispose_clean(&app, &fiber, handle, "丢弃轮").await;
        }

        // ④ 同 key 重载：必须重新 apply，且不留下旧实例。
        if cycle % 3 == 0 {
            MODE.store(OK, Ordering::SeqCst);
            let applies_before = APPLIES.load(Ordering::SeqCst);
            let (fiber, old_handle) = mount(&app, "重载轮").await;
            assert_eq!(
                APPLIES.load(Ordering::SeqCst),
                applies_before + 1,
                "首次挂载必须 apply 一次（cycle {cycle}）"
            );
            within("重载 update", fiber.update(json!({ "cycle": cycle })))
                .await
                .expect("重载必须成功");
            assert_eq!(
                APPLIES.load(Ordering::SeqCst),
                applies_before + 2,
                "同 key 重载必须重新 apply（cycle {cycle}）"
            );
            let new_handle = HANDLE.load(Ordering::SeqCst);
            assert_ne!(new_handle, old_handle, "重载必须换实例（cycle {cycle}）");
            assert_eq!(loaded_instances(), 1, "重载不得留下旧实例（cycle {cycle}）");
            assert_eq!(
                fiber.effect_count(),
                EXPECTED_EFFECTS,
                "重载后 effect 数必须回到基线（cycle {cycle}）"
            );
            let calls_before = CALLS.load(Ordering::SeqCst);
            let outcome = within("重载后 bail", app.root.bail("soak/event", vec![])).await;
            assert!(outcome.is_some(), "重载后的监听器必须仍然工作");
            assert_eq!(
                CALLS.load(Ordering::SeqCst),
                calls_before + 1,
                "重载后监听器必须真的跑一次（cycle {cycle}）"
            );
            assert_no_residue(new_handle, "重载轮");
            dispose_clean(&app, &fiber, new_handle, "重载轮").await;
        }

        // ⑤ 暂存台账：每轮两轮 stage -> adopt -> drop。
        for round in 0..2 {
            let staged = stage_plugin(&source, unique_revision()).expect("暂存假文件必然成功");
            assert_eq!(
                staged_leaked(),
                staged_before + 1,
                "暂存后未认领 ⇒ 台账必须 +1（cycle {cycle} round {round}）"
            );
            let adopted = StagedFile::new(&staged);
            assert_eq!(staged_leaked(), staged_before, "认领后台账必须归零");
            assert!(adopted.path().exists(), "认领 ≠ 删除");
            drop(adopted);
            assert!(!staged.exists(), "drop 后暂存文件必须删除");
        }
    }

    assert_eq!(loaded_instances(), 0, "soak 结束不得留下实例");
    assert_store_baseline(&app, "soak 结束");
    assert_eq!(
        staged_leaked(),
        staged_before,
        "soak 结束暂存台账必须回基线"
    );
    assert!(
        CLEANUPS.load(Ordering::SeqCst) > cleanups_before,
        "OP_CLEANUP 至少必须被投递一次"
    );
    assert!(
        DISPOSES.load(Ordering::SeqCst) > disposes_before,
        "OP_DISPOSE 至少必须被投递一次"
    );
    if let Some(dir) = source.parent() {
        let _ = std::fs::remove_dir_all(dir);
    }
}
