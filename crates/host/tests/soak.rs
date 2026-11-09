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
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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

/// panic 展开时打印当前轮次和完整的可观测残留，避免只依赖滞后的 progress 行。
struct FailureTrace<'a> {
    app: &'a App,
    cycle: usize,
    stage: &'static str,
    handle: u64,
    fiber: Option<Arc<cordis_core::Fiber>>,
}

impl<'a> FailureTrace<'a> {
    fn new(app: &'a App, cycle: usize) -> Self {
        Self {
            app,
            cycle,
            stage: "cycle start",
            handle: 0,
            fiber: None,
        }
    }

    fn stage(&mut self, stage: &'static str) {
        self.stage = stage;
    }

    fn attach(&mut self, fiber: &Arc<cordis_core::Fiber>, handle: u64) {
        self.handle = handle;
        self.fiber = Some(fiber.clone());
    }
}

impl Drop for FailureTrace<'_> {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            return;
        }
        let effects = self
            .fiber
            .as_ref()
            .map(|fiber| fiber.effect_count())
            .unwrap_or(0);
        eprintln!(
            "soak failure: cycle={} stage={} handle={} loaded_instances={} staged_leaked={} pending_len={} dispatches_len={} effect_count={} runtime={} hooks={}",
            self.cycle,
            self.stage,
            self.handle,
            loaded_instances(),
            staged_leaked(),
            pending_len(self.handle),
            dispatches_len(self.handle),
            effects,
            self.app.store.runtime_count(),
            self.app.store.hook_count("soak/event"),
        );
    }
}

/// 测试异常退出时也删除暂存目录，避免 PID 复用后读到上一轮的残留文件。
struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl TempDir {
    fn path(&self) -> &Path {
        &self.0
    }
}

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

async fn mount(app: &App, name: &str, cycle: usize) -> (Arc<cordis_core::Fiber>, u64) {
    let fiber = app.root.plugin(plugin(), Config::Null);
    within(&format!("{name} settle（cycle {cycle}）"), fiber.settle()).await;
    assert_eq!(
        fiber.state(),
        FiberState::Active,
        "{name} 必须激活（cycle {cycle}）"
    );
    let handle = HANDLE.load(Ordering::SeqCst);
    assert_ne!(handle, 0, "{name} 必须拿到实例句柄（cycle {cycle}）");
    assert_eq!(
        fiber.effect_count(),
        EXPECTED_EFFECTS,
        "{name} 的 effect 数必须回到基线（cycle {cycle}）"
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
    // 内层预算必须小于外层 `within`，否则超时先发生，下面的残留断言永远不会执行。
    for _ in 0..400 {
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

fn assert_task_was_cancelled(joined: Result<Ret, tokio::task::JoinError>, name: &str) {
    match joined {
        Err(err) if err.is_cancelled() => {}
        Ok(_) => panic!("{name}：abort 后任务不得正常完成"),
        Err(err) => panic!("{name}：任务不得 panic：{err:?}"),
    }
}

async fn dispose_clean(
    app: &App,
    fiber: &Arc<cordis_core::Fiber>,
    handle: u64,
    name: &str,
    cycle: usize,
) {
    // 必须在 dispose 之前看：dispose 之后句柄失效，计数天然是 0，断言会变空。
    assert_no_residue(handle, &format!("{name}（cycle {cycle}）"));
    within(&format!("{name} dispose（cycle {cycle}）"), fiber.dispose()).await;
    assert!(
        fiber.cleanup_error().is_none(),
        "{name}：清理不得报告异常（cycle {cycle}）：{:?}",
        fiber.cleanup_error()
    );
    assert_eq!(
        fiber.effect_count(),
        0,
        "{name}：恢复栈必须清空（cycle {cycle}）"
    );
    assert_eq!(
        loaded_instances(),
        0,
        "{name}：实例必须回收（cycle {cycle}）"
    );
    assert_store_baseline(app, &format!("{name}（cycle {cycle}）"));
}

fn temp_dir() -> TempDir {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("系统时间必须晚于 UNIX_EPOCH")
        .as_nanos();
    let dir =
        std::env::temp_dir().join(format!("cordis-soak-staged-{}-{nonce}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("建临时目录");
    TempDir(dir)
}

fn fixture_source(dir: &Path) -> PathBuf {
    let source = dir.join("soak.dll");
    std::fs::write(&source, b"stub-not-a-real-dll").expect("造假源文件");
    source
}

fn unique_revision() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    std::process::id() as u64 * 1_000_000 + NEXT.fetch_add(1, Ordering::SeqCst)
}

#[cfg(target_os = "linux")]
fn rss_bytes() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let resident_pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    Some(resident_pages.saturating_mul(4096))
}

#[cfg(not(target_os = "linux"))]
fn rss_bytes() -> Option<u64> {
    None
}

const RSS_BUDGET_BYTES: u64 = 128 * 1024 * 1024;

#[tokio::test]
#[ignore = "run via cargo xtask soak"]
async fn repeated_soak_cycles_leave_no_residue() {
    let cycles = cycles();
    set_dispatch_policy(DispatchPolicy::default());
    assert_eq!(loaded_instances(), 0, "soak 开始前不得有残留实例");
    let staged_before = staged_leaked();
    assert_eq!(staged_before, 0, "soak 开始前暂存台账必须为零");
    let calls_before = CALLS.load(Ordering::SeqCst);
    let applies_before = APPLIES.load(Ordering::SeqCst);
    let cleanups_before = CLEANUPS.load(Ordering::SeqCst);
    let disposes_before = DISPOSES.load(Ordering::SeqCst);
    let started = Instant::now();
    let staged_dir = temp_dir();
    let source = fixture_source(staged_dir.path());

    let app = App::new();
    // 失败轮是刻意制造的；soak 自己的 stdout 只需要最终结果，不刷错误日志。
    app.store.set_log_sink(Arc::new(|_, _, _| {}));
    assert_store_baseline(&app, "soak 开始前");
    let rss_baseline = rss_bytes();
    let mut rss_max = rss_baseline;
    for cycle in 0..cycles {
        let mut trace = FailureTrace::new(&app, cycle);

        // ① 成功：返回值必须存在，监听器必须真的跑一次。
        trace.stage("成功轮 mount");
        MODE.store(OK, Ordering::SeqCst);
        let before = CALLS.load(Ordering::SeqCst);
        let (fiber, handle) = mount(&app, "成功轮", cycle).await;
        trace.attach(&fiber, handle);

        trace.stage("成功轮 bail");
        let outcome = within("成功轮 bail", app.root.bail("soak/event", vec![])).await;
        assert!(outcome.is_some(), "成功轮必须带回返回值（cycle {cycle}）");
        assert_eq!(
            CALLS.load(Ordering::SeqCst),
            before + 1,
            "成功轮监听器必须真的跑一次（cycle {cycle}）"
        );

        trace.stage("成功轮 dispose");
        dispose_clean(&app, &fiber, handle, "成功轮", cycle).await;

        // ② 回调失败：返回 None，但仍必须回收。
        trace.stage("失败轮 mount");
        MODE.store(ERR, Ordering::SeqCst);
        let before = CALLS.load(Ordering::SeqCst);
        let (fiber, handle) = mount(&app, "失败轮", cycle).await;
        trace.attach(&fiber, handle);

        trace.stage("失败轮 bail");
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

        trace.stage("失败轮 dispose");
        dispose_clean(&app, &fiber, handle, "失败轮", cycle).await;

        // ③ 丢弃在飞 Future：先证明 pending_len == 1，再 abort。
        if cycle % 4 == 0 {
            trace.stage("丢弃轮 mount");
            MODE.store(PENDING, Ordering::SeqCst);
            let before = CALLS.load(Ordering::SeqCst);
            let (fiber, handle) = mount(&app, "丢弃轮", cycle).await;
            trace.attach(&fiber, handle);

            trace.stage("丢弃轮 bail");
            let task = tokio::spawn(app.root.bail("soak/event", vec![]));
            wait_for_pending(handle, cycle).await;
            assert_eq!(
                CALLS.load(Ordering::SeqCst),
                before + 1,
                "丢弃轮监听器必须真的跑一次（cycle {cycle}）"
            );

            trace.stage("丢弃轮 abort");
            task.abort();
            let joined = within("丢弃轮任务", task).await;
            assert_task_was_cancelled(joined, "丢弃轮");

            trace.stage("丢弃轮归零");
            within("丢弃轮归零", wait_until_zero(handle)).await;

            trace.stage("丢弃轮 dispose");
            dispose_clean(&app, &fiber, handle, "丢弃轮", cycle).await;
        }

        // ④ 同 key 重载：带一条在飞派发，必须排干旧实例后再换新一代实例。
        if cycle % 3 == 0 {
            trace.stage("重载轮 mount");
            MODE.store(PENDING, Ordering::SeqCst);
            let applies_before = APPLIES.load(Ordering::SeqCst);
            let calls_before = CALLS.load(Ordering::SeqCst);
            let (fiber, old_handle) = mount(&app, "重载轮", cycle).await;
            trace.attach(&fiber, old_handle);
            assert_eq!(
                APPLIES.load(Ordering::SeqCst),
                applies_before + 1,
                "首次挂载必须 apply 一次（cycle {cycle}）"
            );

            trace.stage("重载轮在飞 bail");
            let task = tokio::spawn(app.root.bail("soak/event", vec![]));
            wait_for_pending(old_handle, cycle).await;
            assert_eq!(
                CALLS.load(Ordering::SeqCst),
                calls_before + 1,
                "重载前监听器必须真的跑一次（cycle {cycle}）"
            );

            trace.stage("重载 update");
            within("重载 update", fiber.update(json!({ "cycle": cycle })))
                .await
                .expect("重载必须成功");
            let outcome = within("重载在飞任务", task)
                .await
                .expect("重载排水后任务不得 panic");
            assert!(
                outcome.is_none(),
                "重载排水应把在飞调用结算为没有意见（cycle {cycle}）"
            );
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

            trace.stage("重载后 bail");
            trace.attach(&fiber, new_handle);
            MODE.store(OK, Ordering::SeqCst);
            let calls_before = CALLS.load(Ordering::SeqCst);
            let outcome = within("重载后 bail", app.root.bail("soak/event", vec![])).await;
            assert!(outcome.is_some(), "重载后的监听器必须仍然工作");
            assert_eq!(
                CALLS.load(Ordering::SeqCst),
                calls_before + 1,
                "重载后监听器必须真的跑一次（cycle {cycle}）"
            );
            assert_no_residue(new_handle, "重载轮");

            trace.stage("重载轮 dispose");
            dispose_clean(&app, &fiber, new_handle, "重载轮", cycle).await;
        }

        // ⑤ 暂存台账：每轮两轮 stage -> adopt -> drop。
        for round in 0..2 {
            trace.stage("暂存 round");
            let staged = stage_plugin(&source, unique_revision()).expect("暂存假文件必然成功");
            assert_eq!(
                staged_leaked(),
                staged_before + 1,
                "暂存后未认领 ⇒ 台账必须 +1（cycle {cycle} round {round}）"
            );
            let adopted = StagedFile::new(&staged);
            assert_eq!(
                staged_leaked(),
                staged_before,
                "认领后台账必须归零（cycle {cycle} round {round}）"
            );
            assert!(
                adopted.path().exists(),
                "认领 ≠ 删除（cycle {cycle} round {round}）"
            );
            drop(adopted);
            assert!(
                !staged.exists(),
                "drop 后暂存文件必须删除（cycle {cycle} round {round}）"
            );
        }

        if let Some(rss) = rss_bytes() {
            rss_max = Some(rss_max.map_or(rss, |prior| prior.max(rss)));
        }
        if cycle % 1000 == 0 || cycle + 1 == cycles {
            println!(
                "soak progress: cycle={}/{} loaded_instances={} staged_leaked={} runtime={} hooks={} calls={} applies={} cleanups={} disposes={} rss_kb={}",
                cycle + 1,
                cycles,
                loaded_instances(),
                staged_leaked(),
                app.store.runtime_count(),
                app.store.hook_count("soak/event"),
                CALLS.load(Ordering::SeqCst),
                APPLIES.load(Ordering::SeqCst),
                CLEANUPS.load(Ordering::SeqCst),
                DISPOSES.load(Ordering::SeqCst),
                rss_bytes().map(|bytes| bytes / 1024).unwrap_or(0),
            );
        }
    }

    assert_eq!(loaded_instances(), 0, "soak 结束不得留下实例");
    assert_store_baseline(&app, "soak 结束");
    assert_eq!(
        staged_leaked(),
        staged_before,
        "soak 结束暂存台账必须回基线"
    );

    // 每轮固定 2 次成功 apply；每 4 轮多 1 次丢弃轮；每 3 轮多 2 次
    // （首次挂载 + 带在飞派发的 update）。每次成功 apply 恰好对应一次
    // OP_CLEANUP 与一次 OP_DISPOSE；只断言“至少一次”会让后 19,999 轮的
    // 通知丢失仍然通过。
    let expected_ops = 2 * cycles + cycles.div_ceil(4) + 2 * cycles.div_ceil(3);
    assert_eq!(
        (CALLS.load(Ordering::SeqCst) - calls_before) as usize,
        expected_ops,
        "监听器调用总数必须与循环结构一致"
    );
    assert_eq!(
        (APPLIES.load(Ordering::SeqCst) - applies_before) as usize,
        expected_ops,
        "apply 总数必须与循环结构一致"
    );
    assert_eq!(
        (CLEANUPS.load(Ordering::SeqCst) - cleanups_before) as usize,
        expected_ops,
        "OP_CLEANUP 总数必须与 apply 总数一致"
    );
    assert_eq!(
        (DISPOSES.load(Ordering::SeqCst) - disposes_before) as usize,
        expected_ops,
        "OP_DISPOSE 总数必须与 apply 总数一致"
    );

    if let (Some(baseline), Some(max)) = (rss_baseline, rss_max) {
        let growth = max.saturating_sub(baseline);
        assert!(
            growth <= RSS_BUDGET_BYTES,
            "soak RSS 增长 {growth} bytes 超过预算 {RSS_BUDGET_BYTES} bytes（baseline={baseline}, max={max}）"
        );
    }
    println!(
        "soak complete: cycles={} elapsed_ms={} loaded_instances={} staged_leaked={} runtime={} hooks={} calls={} applies={} cleanups={} disposes={} rss_growth_bytes={}",
        cycles,
        started.elapsed().as_millis(),
        loaded_instances(),
        staged_leaked(),
        app.store.runtime_count(),
        app.store.hook_count("soak/event"),
        CALLS.load(Ordering::SeqCst),
        APPLIES.load(Ordering::SeqCst),
        CLEANUPS.load(Ordering::SeqCst),
        DISPOSES.load(Ordering::SeqCst),
        rss_max
            .zip(rss_baseline)
            .map(|(max, baseline)| max.saturating_sub(baseline))
            .unwrap_or(0),
    );
}
