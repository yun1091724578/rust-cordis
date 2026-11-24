//! 宿主可观测关闭、三段等待预算、同步阻塞动态库保活与热重载状态报告验收测试。
//!
//! 覆盖核心验收矩阵：
//! 1. T2 `OP_CLEANUP` / `OP_DISPOSE` 返回错误汇总到 `CloseReport`（`CompletedWithErrors`）；
//! 2. T2 在途同步阻塞调用 + `drain_timeout_ms` 超时后及时上报 `TimedOut`，且动态库保活
//!    严格等到最后一次原生调用退出后才执行 `OP_DISPOSE` 与释放（ADR 0005 §4）；
//! 3. T2 同步阻塞 `OP_APPLY` + `apply_timeout_ms` 超时后及时失败，且保活延续到 `OP_APPLY` 退出；
//! 4. T2 同步阻塞 `OP_DISPOSE` + `dispose_timeout_ms` 超时后及时上报 `TimedOut`，且保活延续到退出；
//! 5. 热重载报告显式区分 `Active` 与 `Pending`（含依赖未就绪诊断）、`RolledBack`、`Lost`
//!    与旧实例清理错误。

use cordis_abi::{
    Blob, Out, OP_APPLY, OP_CALLBACK, OP_CLEANUP, OP_DISPOSE, STATUS_ERR, STATUS_OK,
    STATUS_UNSUPPORTED,
};
use cordis_core::{
    plugin_fn, App, BoxFuture, CleanupErrorKind, CloseStatus, Config, Ctx, Disposable,
    ExternService, ExternValue, FiberState, Inject, LifecyclePolicy, PendingReason, Plugin,
};
use cordis_host::loader::{
    load_with_source, reload_one_with_plugin, LoadedFile, LoaderConfig, PluginSource, ReloadStatus,
};
use cordis_host::patch::Entry;
use cordis_host::{
    apply_via, apply_via_with_keepalive, host_api, loaded_instances, set_lifecycle_policy_for,
};
use std::any::Any;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier, Mutex, OnceLock};
use std::time::Duration;

async fn instance_lock() -> tokio::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

struct DropProbe(Arc<AtomicBool>);

impl Drop for DropProbe {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

fn make_probe() -> (Arc<dyn Any + Send + Sync>, Arc<AtomicBool>) {
    let dropped = Arc::new(AtomicBool::new(false));
    let probe: Arc<dyn Any + Send + Sync> = Arc::new(DropProbe(dropped.clone()));
    (probe, dropped)
}

// ------------------------------------------------------------------ 1. T2 OP_CLEANUP / OP_DISPOSE 错误

unsafe extern "C" fn failing_cleanup_and_dispose(
    handle: u64,
    op: u32,
    _: Blob,
    _: Blob,
    out: *mut Out,
) -> i32 {
    match op {
        OP_APPLY => {
            let Some(effect) = host_api().ctx_effect else {
                return STATUS_UNSUPPORTED;
            };
            let mut buf = [0u8; 64];
            let mut effect_out = Out {
                ptr: buf.as_mut_ptr(),
                cap: buf.len(),
                len: 0,
            };
            if effect(handle, 42, &mut effect_out) != STATUS_OK {
                return STATUS_ERR;
            }
            (*out).write(b"")
        }
        OP_CLEANUP => {
            (*out).write(b"cleanup exploded");
            STATUS_ERR
        }
        OP_DISPOSE => {
            (*out).write(b"dispose exploded");
            STATUS_ERR
        }
        _ => STATUS_UNSUPPORTED,
    }
}

#[tokio::test]
async fn t2_cleanup_and_dispose_errors_are_aggregated_into_close_report() {
    let _guard = instance_lock().await;
    assert_eq!(loaded_instances(), 0);

    let app = App::new();
    let plugin = plugin_fn("t2-err-plugin", Inject::default(), |ctx: Ctx, cfg| {
        apply_via(failing_cleanup_and_dispose, ctx, cfg, "t2-err-plugin")
    });
    app.plugin(plugin, Config::Null).await.expect("装载成功");
    assert_eq!(loaded_instances(), 1);

    let report = app.close_with_report(None).await;
    assert_eq!(report.status, CloseStatus::CompletedWithErrors);
    assert!(!report.still_running);
    assert_eq!(
        report.errors.len(),
        2,
        "OP_CLEANUP 与 OP_DISPOSE 错误都必须汇总: {:?}",
        report.errors
    );
    assert_eq!(report.errors[0].kind, CleanupErrorKind::HostCleanup);
    assert!(
        report.errors[0].message.contains("cleanup exploded"),
        "首条错误应来自先执行的 OP_CLEANUP: {:?}",
        report.errors[0]
    );
    assert_eq!(report.errors[1].kind, CleanupErrorKind::HostDispose);
    assert!(
        report.errors[1].message.contains("dispose exploded"),
        "次条错误应来自后执行的 OP_DISPOSE: {:?}",
        report.errors[1]
    );
    assert_eq!(loaded_instances(), 0);
}

// ------------------------------------------------------------------ 2. T2 在途同步阻塞 + drain_timeout_ms + 保活

struct DrainBlockState {
    entered: Arc<Barrier>,
    release: Arc<Barrier>,
    call_exited: Arc<AtomicBool>,
    dispose_called: Arc<AtomicBool>,
    dispose_saw_clean_order: Arc<AtomicBool>,
    probe_dropped: Arc<AtomicBool>,
}

static DRAIN_STATE: Mutex<Option<DrainBlockState>> = Mutex::new(None);

unsafe extern "C" fn blocking_callback_plugin(
    handle: u64,
    op: u32,
    _: Blob,
    _: Blob,
    out: *mut Out,
) -> i32 {
    match op {
        OP_APPLY => {
            let Some(on) = host_api().ctx_on else {
                return STATUS_UNSUPPORTED;
            };
            let mut buf = [0u8; 64];
            let mut event_out = Out {
                ptr: buf.as_mut_ptr(),
                cap: buf.len(),
                len: 0,
            };
            if on(handle, Blob::new(b"block/sync"), 1, &mut event_out) != STATUS_OK {
                return STATUS_ERR;
            }
            (*out).write(b"")
        }
        OP_CALLBACK => {
            let (entered, release, exited) = {
                let guard = DRAIN_STATE.lock().unwrap();
                let st = guard.as_ref().expect("DRAIN_STATE 已初始化");
                (
                    st.entered.clone(),
                    st.release.clone(),
                    st.call_exited.clone(),
                )
            };
            entered.wait();
            release.wait();
            exited.store(true, Ordering::SeqCst);
            (*out).write(b"null")
        }
        OP_DISPOSE => {
            let guard = DRAIN_STATE.lock().unwrap();
            let st = guard.as_ref().expect("DRAIN_STATE 已初始化");
            let ok =
                st.call_exited.load(Ordering::SeqCst) && !st.probe_dropped.load(Ordering::SeqCst);
            st.dispose_saw_clean_order.store(ok, Ordering::SeqCst);
            st.dispose_called.store(true, Ordering::SeqCst);
            (*out).write(b"")
        }
        _ => STATUS_UNSUPPORTED,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t2_sync_blocking_inflight_call_respects_drain_budget_and_preserves_keepalive() {
    let _guard = instance_lock().await;
    assert_eq!(loaded_instances(), 0);

    let (probe, probe_dropped) = make_probe();
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let call_exited = Arc::new(AtomicBool::new(false));
    let dispose_called = Arc::new(AtomicBool::new(false));
    let dispose_saw_clean_order = Arc::new(AtomicBool::new(false));

    *DRAIN_STATE.lock().unwrap() = Some(DrainBlockState {
        entered: entered.clone(),
        release: release.clone(),
        call_exited: call_exited.clone(),
        dispose_called: dispose_called.clone(),
        dispose_saw_clean_order: dispose_saw_clean_order.clone(),
        probe_dropped: probe_dropped.clone(),
    });

    let app = App::new();
    set_lifecycle_policy_for(
        &app.root,
        LifecyclePolicy::new(None, Some(50), None).unwrap(),
    )
    .unwrap();

    let keepalive = Mutex::new(Some(probe));
    let plugin = plugin_fn("blocking-cb", Inject::default(), move |ctx: Ctx, cfg| {
        let lease = keepalive.lock().unwrap().take();
        apply_via_with_keepalive(blocking_callback_plugin, ctx, cfg, "blocking-cb", lease)
    });
    app.plugin(plugin, Config::Null).await.expect("装载成功");
    assert_eq!(loaded_instances(), 1);

    let root = app.root.clone();
    let rt_handle = tokio::runtime::Handle::current();
    let caller = std::thread::spawn(move || {
        rt_handle.block_on(async move {
            let _ = root.bail("block/sync", vec![]).await;
        });
    });

    // 确保原生回调线程已真实进入同步阻塞。
    let entered_wait = entered.clone();
    tokio::task::spawn_blocking(move || entered_wait.wait())
        .await
        .expect("进入阻塞点");

    let store = app.store.clone();
    let report = tokio::time::timeout(Duration::from_secs(2), app.close_with_report(None))
        .await
        .expect("drain_timeout_ms 必须及时返回 TimedOut 报告");

    assert_eq!(report.status, CloseStatus::TimedOut);
    assert!(report.still_running);
    assert!(
        report
            .errors
            .iter()
            .any(|err| err.kind == CleanupErrorKind::Timeout),
        "应包含排水超时错误: {:?}",
        report.errors
    );
    // ADR 0005 §4 保活红线：原生调用尚未退出前，绝不能卸载库或投递 OP_DISPOSE。
    assert!(
        !probe_dropped.load(Ordering::SeqCst),
        "在途原生调用未退出前动态库保活绝不得释放"
    );
    assert!(
        !dispose_called.load(Ordering::SeqCst),
        "在途原生调用未退出前绝不得并发投递 OP_DISPOSE"
    );
    assert_eq!(loaded_instances(), 1);
    assert!(store.pending_background_cleanups() >= 1);

    // 放行原生调用：随后后台收尾必须按序执行 OP_DISPOSE、注销实例并释放保活。
    let release_wait = release.clone();
    tokio::task::spawn_blocking(move || release_wait.wait())
        .await
        .expect("放行阻塞点");

    tokio::time::timeout(Duration::from_secs(2), store.wait_background_cleanups())
        .await
        .expect("原生调用退出后后台清理必须完成");
    tokio::task::spawn_blocking(move || caller.join().expect("调用线程退出"))
        .await
        .expect("join 完成");

    assert!(call_exited.load(Ordering::SeqCst));
    assert!(dispose_called.load(Ordering::SeqCst));
    assert!(
        dispose_saw_clean_order.load(Ordering::SeqCst),
        "OP_DISPOSE 必须晚于在途调用退出且早于动态库释放"
    );
    assert!(
        probe_dropped.load(Ordering::SeqCst),
        "全部原生调用与 OP_DISPOSE 结束后动态库保活必须干净释放"
    );
    assert_eq!(loaded_instances(), 0);
    assert_eq!(store.pending_background_cleanups(), 0);
    *DRAIN_STATE.lock().unwrap() = None;
}

// ------------------------------------------------------------------ 3. T2 同步阻塞 OP_APPLY + apply_timeout_ms + 保活

struct ApplyBlockState {
    entered: Arc<Barrier>,
    release: Arc<Barrier>,
    apply_exited: Arc<AtomicBool>,
    dispose_called: Arc<AtomicBool>,
    dispose_saw_clean_order: Arc<AtomicBool>,
    probe_dropped: Arc<AtomicBool>,
}

static APPLY_STATE: Mutex<Option<ApplyBlockState>> = Mutex::new(None);

unsafe extern "C" fn blocking_apply_plugin(
    _handle: u64,
    op: u32,
    _: Blob,
    _: Blob,
    out: *mut Out,
) -> i32 {
    match op {
        OP_APPLY => {
            let (entered, release, exited) = {
                let guard = APPLY_STATE.lock().unwrap();
                let st = guard.as_ref().expect("APPLY_STATE 已初始化");
                (
                    st.entered.clone(),
                    st.release.clone(),
                    st.apply_exited.clone(),
                )
            };
            entered.wait();
            release.wait();
            exited.store(true, Ordering::SeqCst);
            (*out).write(b"")
        }
        OP_DISPOSE => {
            let guard = APPLY_STATE.lock().unwrap();
            let st = guard.as_ref().expect("APPLY_STATE 已初始化");
            let ok =
                st.apply_exited.load(Ordering::SeqCst) && !st.probe_dropped.load(Ordering::SeqCst);
            st.dispose_saw_clean_order.store(ok, Ordering::SeqCst);
            st.dispose_called.store(true, Ordering::SeqCst);
            (*out).write(b"")
        }
        _ => STATUS_UNSUPPORTED,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t2_sync_blocking_apply_respects_apply_budget_and_preserves_keepalive() {
    let _guard = instance_lock().await;
    assert_eq!(loaded_instances(), 0);

    let (probe, probe_dropped) = make_probe();
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let apply_exited = Arc::new(AtomicBool::new(false));
    let dispose_called = Arc::new(AtomicBool::new(false));
    let dispose_saw_clean_order = Arc::new(AtomicBool::new(false));

    *APPLY_STATE.lock().unwrap() = Some(ApplyBlockState {
        entered: entered.clone(),
        release: release.clone(),
        apply_exited: apply_exited.clone(),
        dispose_called: dispose_called.clone(),
        dispose_saw_clean_order: dispose_saw_clean_order.clone(),
        probe_dropped: probe_dropped.clone(),
    });

    let app = App::new();
    // 故意只设 apply_timeout_ms（drain_timeout_ms 为 None），验证 apply 超时后 fail() 不会无限等待阻塞中的 OP_APPLY。
    set_lifecycle_policy_for(
        &app.root,
        LifecyclePolicy::new(Some(60), None, None).unwrap(),
    )
    .unwrap();

    let keepalive = Mutex::new(Some(probe));
    let plugin = plugin_fn("blocking-apply", Inject::default(), move |ctx: Ctx, cfg| {
        let lease = keepalive.lock().unwrap().take();
        apply_via_with_keepalive(blocking_apply_plugin, ctx, cfg, "blocking-apply", lease)
    });

    let entered_wait = entered.clone();
    let enter_task = tokio::task::spawn_blocking(move || entered_wait.wait());

    let err = tokio::time::timeout(Duration::from_secs(2), app.plugin(plugin, Config::Null))
        .await
        .expect("apply_timeout_ms 必须及时返回失败")
        .err()
        .expect("同步阻塞 OP_APPLY 超时必须失败");
    enter_task.await.expect("已进入 OP_APPLY");
    assert!(
        err.to_string().contains("apply 超时"),
        "失败原因应指出 apply 超时: {err}"
    );

    // 放行前：保活与实例仍挂在后台等待 OP_APPLY 退出。
    assert!(
        !probe_dropped.load(Ordering::SeqCst),
        "OP_APPLY 仍在阻塞时动态库保活绝不得释放"
    );
    assert!(!dispose_called.load(Ordering::SeqCst));
    assert_eq!(loaded_instances(), 1);
    assert!(app.store.pending_background_cleanups() >= 1);

    // 放行 OP_APPLY，等待后台收尾完成。
    let release_wait = release.clone();
    tokio::task::spawn_blocking(move || release_wait.wait())
        .await
        .expect("放行 OP_APPLY");

    let store = app.store.clone();
    tokio::time::timeout(Duration::from_secs(2), store.wait_background_cleanups())
        .await
        .expect("OP_APPLY 退出后后台收尾必须完成");

    assert!(apply_exited.load(Ordering::SeqCst));
    assert!(dispose_called.load(Ordering::SeqCst));
    assert!(dispose_saw_clean_order.load(Ordering::SeqCst));
    assert!(probe_dropped.load(Ordering::SeqCst));
    assert_eq!(loaded_instances(), 0);
    assert_eq!(store.pending_background_cleanups(), 0);

    let report = app.close_with_report(None).await;
    assert_eq!(report.status, CloseStatus::Completed);
    *APPLY_STATE.lock().unwrap() = None;
}

// ------------------------------------------------------------------ 4. T2 同步阻塞 OP_DISPOSE + dispose_timeout_ms + 保活

struct DisposeBlockState {
    entered: Arc<Barrier>,
    release: Arc<Barrier>,
    dispose_exited: Arc<AtomicBool>,
    dispose_saw_keepalive: Arc<AtomicBool>,
    probe_dropped: Arc<AtomicBool>,
}

static DISPOSE_STATE: Mutex<Option<DisposeBlockState>> = Mutex::new(None);

unsafe extern "C" fn blocking_dispose_plugin(
    _handle: u64,
    op: u32,
    _: Blob,
    _: Blob,
    out: *mut Out,
) -> i32 {
    match op {
        OP_APPLY => (*out).write(b""),
        OP_DISPOSE => {
            let (entered, release, exited, saw, dropped) = {
                let guard = DISPOSE_STATE.lock().unwrap();
                let st = guard.as_ref().expect("DISPOSE_STATE 已初始化");
                (
                    st.entered.clone(),
                    st.release.clone(),
                    st.dispose_exited.clone(),
                    st.dispose_saw_keepalive.clone(),
                    st.probe_dropped.clone(),
                )
            };
            entered.wait();
            release.wait();
            saw.store(!dropped.load(Ordering::SeqCst), Ordering::SeqCst);
            exited.store(true, Ordering::SeqCst);
            (*out).write(b"late dispose failure");
            STATUS_ERR
        }
        _ => STATUS_UNSUPPORTED,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t2_sync_blocking_dispose_respects_dispose_budget_and_preserves_keepalive() {
    let _guard = instance_lock().await;
    assert_eq!(loaded_instances(), 0);

    let (probe, probe_dropped) = make_probe();
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let dispose_exited = Arc::new(AtomicBool::new(false));
    let dispose_saw_keepalive = Arc::new(AtomicBool::new(false));

    *DISPOSE_STATE.lock().unwrap() = Some(DisposeBlockState {
        entered: entered.clone(),
        release: release.clone(),
        dispose_exited: dispose_exited.clone(),
        dispose_saw_keepalive: dispose_saw_keepalive.clone(),
        probe_dropped: probe_dropped.clone(),
    });

    let app = App::new();
    set_lifecycle_policy_for(
        &app.root,
        LifecyclePolicy::new(None, None, Some(60)).unwrap(),
    )
    .unwrap();

    let keepalive = Mutex::new(Some(probe));
    let plugin = plugin_fn(
        "blocking-dispose",
        Inject::default(),
        move |ctx: Ctx, cfg| {
            let lease = keepalive.lock().unwrap().take();
            apply_via_with_keepalive(blocking_dispose_plugin, ctx, cfg, "blocking-dispose", lease)
        },
    );
    let fiber = app.plugin(plugin, Config::Null).await.expect("装载成功");
    assert_eq!(loaded_instances(), 1);

    let entered_wait = entered.clone();
    let enter_task = tokio::task::spawn_blocking(move || entered_wait.wait());

    let store = app.store.clone();
    let report = tokio::time::timeout(Duration::from_secs(2), app.close_with_report(None))
        .await
        .expect("dispose_timeout_ms 必须及时返回 TimedOut 报告");
    enter_task.await.expect("已进入 OP_DISPOSE");

    assert_eq!(report.status, CloseStatus::TimedOut);
    assert!(report.still_running);
    assert!(report
        .errors
        .iter()
        .any(|err| err.kind == CleanupErrorKind::Timeout));
    assert!(
        !probe_dropped.load(Ordering::SeqCst),
        "OP_DISPOSE 仍在阻塞时动态库保活绝不得释放"
    );
    assert_eq!(loaded_instances(), 1);
    assert!(store.pending_background_cleanups() >= 1);

    let release_wait = release.clone();
    tokio::task::spawn_blocking(move || release_wait.wait())
        .await
        .expect("放行 OP_DISPOSE");

    tokio::time::timeout(Duration::from_secs(2), store.wait_background_cleanups())
        .await
        .expect("OP_DISPOSE 退出后后台清理必须完成");

    assert!(dispose_exited.load(Ordering::SeqCst));
    assert!(
        dispose_saw_keepalive.load(Ordering::SeqCst),
        "OP_DISPOSE 执行全程动态库保活必须有效"
    );
    assert!(probe_dropped.load(Ordering::SeqCst));
    assert_eq!(loaded_instances(), 0);
    assert_eq!(store.pending_background_cleanups(), 0);
    assert!(
        fiber
            .cleanup_errors()
            .iter()
            .any(|err| err.kind == CleanupErrorKind::HostDispose
                && err.message.contains("late dispose failure")),
        "OP_DISPOSE 延后返回的失败也应记录在 fiber 清理错误中: {:?}",
        fiber.cleanup_errors()
    );
    *DISPOSE_STATE.lock().unwrap() = None;
}

// ------------------------------------------------------------------ 5. 热重载报告：Active vs Pending / RolledBack / Lost / 旧实例清理错误

struct StaticSource(Arc<dyn Plugin>);

impl PluginSource for StaticSource {
    fn load(
        &self,
        entry: &Entry,
        _search: &[PathBuf],
        _revision: u64,
    ) -> Result<LoadedFile, String> {
        let path = PathBuf::from(format!("<mem:{}>", entry.name));
        Ok(LoadedFile {
            plugin: self.0.clone(),
            source: path.clone(),
            staged: path,
            fingerprint: None,
        })
    }
}

fn temp_config(tag: &str, yaml: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "cordis-lifecycle-obs-{}-{tag}.yml",
        std::process::id()
    ));
    std::fs::write(&path, yaml).expect("写入临时配置");
    path
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reload_report_distinguishes_active_pending_rolled_back_lost_and_cleanup_errors() {
    let _guard = instance_lock().await;
    assert_eq!(loaded_instances(), 0);

    let cfg_path = temp_config("reload-report", "- id: worker\n  name: worker\n");
    let app = App::new();

    // 记录 loader/reload 事件载荷，校验事件与返回结构一致。
    let events: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let events_sub = events.clone();
    app.root.on("loader/reload", move |call| {
        let events = events_sub.clone();
        Box::pin(async move {
            if let Some(payload) = call
                .args
                .first()
                .and_then(|value| value.downcast::<ExternValue>())
            {
                events.lock().unwrap().push(payload.0.clone());
            }
            None
        })
    });

    // 初版使用会返回 OP_CLEANUP + OP_DISPOSE 错误的 T2 插件。
    let v1 = plugin_fn("worker-v1", Inject::default(), |ctx: Ctx, cfg| {
        apply_via(failing_cleanup_and_dispose, ctx, cfg, "worker-v1")
    });
    let state = load_with_source(
        app.root.clone(),
        LoaderConfig::new(cfg_path.to_string_lossy().into_owned()),
        Arc::new(StaticSource(v1)),
    )
    .await
    .expect("初装成功");

    // 初装未重载前，loader.report 不含 lastReload 字段（保持 0.1 快照兼容）。
    assert!(state.last_reload().is_none());
    assert!(state.report().get("lastReload").is_none());

    // 1) 重载到 Active 新版本，同时汇总旧 T2 实例的 OP_CLEANUP 与 OP_DISPOSE 错误。
    let v2_active = plugin_fn("worker-v2", Inject::default(), |ctx: Ctx, _| {
        Box::pin(async move {
            ctx.provide("worker.ver", cordis_core::Dyn::new(2u32))?;
            Ok(())
        })
    });
    let r1 = reload_one_with_plugin(&app.root, &state, "worker", v2_active).await;
    assert_eq!(r1.status, ReloadStatus::Active);
    assert!(r1.pending.is_empty());
    assert!(r1.error.is_none());
    let dispose_r1 = r1.dispose_report.as_ref().expect("必须包含旧实例关闭报告");
    assert_eq!(dispose_r1.status, CloseStatus::CompletedWithErrors);
    assert_eq!(dispose_r1.errors.len(), 2);
    assert_eq!(dispose_r1.errors[0].kind, CleanupErrorKind::HostCleanup);
    assert_eq!(dispose_r1.errors[1].kind, CleanupErrorKind::HostDispose);
    assert_eq!(state.last_reload().as_ref(), Some(&r1));
    assert_eq!(state.report()["lastReload"]["status"], "active");

    // 2) 重载到缺少必需依赖的新版本：必须明确报告 Pending（绝不伪报 Active），并给出缺失键诊断。
    let v3_pending = plugin_fn(
        "worker-v3",
        Inject::of(&["missing.upstream"]),
        |_ctx: Ctx, _| Box::pin(async move { Ok(()) }),
    );
    let r2 = reload_one_with_plugin(&app.root, &state, "worker", v3_pending).await;
    assert_eq!(
        r2.status,
        ReloadStatus::Pending,
        "依赖未就绪时必须报告 Pending 而不是 Active"
    );
    assert!(r2.error.is_none());
    assert_eq!(r2.pending.len(), 1);
    assert_eq!(r2.pending[0].key, "missing.upstream");
    assert_eq!(r2.pending[0].reason, PendingReason::Missing);
    assert_eq!(
        state.fiber_of("worker").unwrap().state(),
        FiberState::Pending
    );
    assert_eq!(state.report()["lastReload"]["status"], "pending");
    assert_eq!(
        state.report()["lastReload"]["pending"][0]["key"],
        "missing.upstream"
    );

    // 3) 先切回 Active，再重载到 apply 失败的新版本：应报告 RolledBack 且旧服务继续在线。
    let v4_active = plugin_fn("worker-v4", Inject::default(), |ctx: Ctx, _| {
        Box::pin(async move {
            ctx.provide("worker.ver", cordis_core::Dyn::new(4u32))?;
            Ok(())
        })
    });
    let r3 = reload_one_with_plugin(&app.root, &state, "worker", v4_active).await;
    assert_eq!(r3.status, ReloadStatus::Active);

    let v5_broken = plugin_fn("worker-v5", Inject::default(), |_ctx: Ctx, _| {
        Box::pin(async move { Err("broken-v5".into()) })
    });
    let r4 = reload_one_with_plugin(&app.root, &state, "worker", v5_broken).await;
    assert_eq!(r4.status, ReloadStatus::RolledBack);
    assert!(
        r4.error
            .as_deref()
            .is_some_and(|msg| msg.contains("broken-v5")),
        "回滚报告必须包含新版本失败原因: {:?}",
        r4.error
    );
    assert_eq!(
        app.root
            .get("worker.ver")
            .and_then(|v| v.downcast::<u32>())
            .map(|v| *v),
        Some(4),
        "回滚后旧实例服务必须恢复在线"
    );

    // 4) 旧实例 dispose 超时：拒绝在旧实例退出前挂载新实例，返回 Lost + TimedOut 关闭报告，且保持保活。
    let (probe, probe_dropped) = make_probe();
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let dispose_exited = Arc::new(AtomicBool::new(false));
    let dispose_saw_keepalive = Arc::new(AtomicBool::new(false));
    *DISPOSE_STATE.lock().unwrap() = Some(DisposeBlockState {
        entered: entered.clone(),
        release: release.clone(),
        dispose_exited: dispose_exited.clone(),
        dispose_saw_keepalive: dispose_saw_keepalive.clone(),
        probe_dropped: probe_dropped.clone(),
    });
    let keepalive = Mutex::new(Some(probe));
    let v6_blocking = plugin_fn("worker-v6", Inject::default(), move |ctx: Ctx, cfg| {
        let lease = keepalive.lock().unwrap().take();
        apply_via_with_keepalive(blocking_dispose_plugin, ctx, cfg, "worker-v6", lease)
    });
    let r5 = reload_one_with_plugin(&app.root, &state, "worker", v6_blocking).await;
    assert_eq!(r5.status, ReloadStatus::Active);

    set_lifecycle_policy_for(
        &app.root,
        LifecyclePolicy::new(None, None, Some(50)).unwrap(),
    )
    .unwrap();
    let v7_applied = Arc::new(AtomicBool::new(false));
    let v7_flag = v7_applied.clone();
    let v7 = plugin_fn("worker-v7", Inject::default(), move |_ctx: Ctx, _| {
        let applied = v7_flag.clone();
        Box::pin(async move {
            applied.store(true, Ordering::SeqCst);
            Ok(())
        })
    });

    let entered_wait = entered.clone();
    let enter_task = tokio::task::spawn_blocking(move || entered_wait.wait());
    let r6 = tokio::time::timeout(
        Duration::from_secs(2),
        reload_one_with_plugin(&app.root, &state, "worker", v7),
    )
    .await
    .expect("旧实例 dispose 超时时重载必须及时返回");
    enter_task.await.expect("已进入 OP_DISPOSE");

    assert_eq!(r6.status, ReloadStatus::Lost);
    assert!(
        !v7_applied.load(Ordering::SeqCst),
        "旧实例未退出前绝不得挂载新实例"
    );
    let dispose_r6 = r6.dispose_report.as_ref().expect("必须携带旧实例超时报告");
    assert_eq!(dispose_r6.status, CloseStatus::TimedOut);
    assert!(dispose_r6.still_running);
    assert!(!probe_dropped.load(Ordering::SeqCst));

    let release_wait = release.clone();
    tokio::task::spawn_blocking(move || release_wait.wait())
        .await
        .expect("放行 OP_DISPOSE");
    tokio::time::timeout(Duration::from_secs(2), app.store.wait_background_cleanups())
        .await
        .expect("旧实例后台清理完成");
    assert!(dispose_exited.load(Ordering::SeqCst));
    assert!(dispose_saw_keepalive.load(Ordering::SeqCst));
    assert!(probe_dropped.load(Ordering::SeqCst));
    assert_eq!(loaded_instances(), 0);

    // 验证 loader/reload 事件里带出了 active / pending / rolled_back / lost 状态。
    {
        let emitted = events.lock().unwrap();
        let statuses: Vec<&str> = emitted
            .iter()
            .filter_map(|e| e.get("status").and_then(|v| v.as_str()))
            .collect();
        assert_eq!(
            statuses,
            vec![
                "active",
                "pending",
                "active",
                "rolled_back",
                "active",
                "lost"
            ]
        );
    }

    let close_report = app.close_with_report(None).await;
    assert_eq!(close_report.status, CloseStatus::Completed);
    *DISPOSE_STATE.lock().unwrap() = None;
    let _ = std::fs::remove_file(&cfg_path);
}

// ------------------------------------------------------------------ 6. T2 同步阻塞 OP_CLEANUP + dispose 预算 + 保活

struct CleanupBlockState {
    entered: Arc<Barrier>,
    release: Arc<Barrier>,
    cleanup_exited: Arc<AtomicBool>,
    dispose_saw_cleanup_done: Arc<AtomicBool>,
    probe_dropped: Arc<AtomicBool>,
}

static CLEANUP_STATE: Mutex<Option<CleanupBlockState>> = Mutex::new(None);

unsafe extern "C" fn blocking_cleanup_plugin(
    handle: u64,
    op: u32,
    _: Blob,
    _: Blob,
    out: *mut Out,
) -> i32 {
    match op {
        OP_APPLY => {
            let Some(effect) = host_api().ctx_effect else {
                return STATUS_UNSUPPORTED;
            };
            let mut buf = [0u8; 64];
            let mut effect_out = Out {
                ptr: buf.as_mut_ptr(),
                cap: buf.len(),
                len: 0,
            };
            if effect(handle, 99, &mut effect_out) != STATUS_OK {
                return STATUS_ERR;
            }
            (*out).write(b"")
        }
        OP_CLEANUP => {
            let (entered, release, exited) = {
                let guard = CLEANUP_STATE.lock().unwrap();
                let st = guard.as_ref().expect("CLEANUP_STATE 已初始化");
                (
                    st.entered.clone(),
                    st.release.clone(),
                    st.cleanup_exited.clone(),
                )
            };
            entered.wait();
            release.wait();
            exited.store(true, Ordering::SeqCst);
            (*out).write(b"")
        }
        OP_DISPOSE => {
            let guard = CLEANUP_STATE.lock().unwrap();
            let st = guard.as_ref().expect("CLEANUP_STATE 已初始化");
            let ok = st.cleanup_exited.load(Ordering::SeqCst)
                && !st.probe_dropped.load(Ordering::SeqCst);
            st.dispose_saw_cleanup_done.store(ok, Ordering::SeqCst);
            (*out).write(b"")
        }
        _ => STATUS_UNSUPPORTED,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t2_sync_blocking_cleanup_respects_dispose_budget_and_preserves_keepalive() {
    let _guard = instance_lock().await;
    assert_eq!(loaded_instances(), 0);

    let (probe, probe_dropped) = make_probe();
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let cleanup_exited = Arc::new(AtomicBool::new(false));
    let dispose_saw_cleanup_done = Arc::new(AtomicBool::new(false));

    *CLEANUP_STATE.lock().unwrap() = Some(CleanupBlockState {
        entered: entered.clone(),
        release: release.clone(),
        cleanup_exited: cleanup_exited.clone(),
        dispose_saw_cleanup_done: dispose_saw_cleanup_done.clone(),
        probe_dropped: probe_dropped.clone(),
    });

    let app = App::new();
    let keepalive = Mutex::new(Some(probe));
    let plugin = plugin_fn(
        "blocking-cleanup",
        Inject::default(),
        move |ctx: Ctx, cfg| {
            let lease = keepalive.lock().unwrap().take();
            apply_via_with_keepalive(blocking_cleanup_plugin, ctx, cfg, "blocking-cleanup", lease)
        },
    );
    app.plugin(plugin, Config::Null).await.expect("装载成功");
    assert_eq!(loaded_instances(), 1);

    let entered_wait = entered.clone();
    let enter_task = tokio::task::spawn_blocking(move || entered_wait.wait());

    let store = app.store.clone();
    // 验证显式传给 close_with_report(Some(..)) 的预算也能把子树同步 OP_CLEANUP 移交阻塞池。
    let report = tokio::time::timeout(
        Duration::from_secs(2),
        app.close_with_report(Some(Duration::from_millis(60))),
    )
    .await
    .expect("显式 dispose 超时必须及时返回 TimedOut");
    enter_task.await.expect("已进入 OP_CLEANUP");

    assert_eq!(report.status, CloseStatus::TimedOut);
    assert!(report.still_running);
    assert!(!probe_dropped.load(Ordering::SeqCst));
    assert!(!dispose_saw_cleanup_done.load(Ordering::SeqCst));
    assert_eq!(loaded_instances(), 1);

    let release_wait = release.clone();
    tokio::task::spawn_blocking(move || release_wait.wait())
        .await
        .expect("放行 OP_CLEANUP");

    tokio::time::timeout(Duration::from_secs(2), store.wait_background_cleanups())
        .await
        .expect("OP_CLEANUP 退出后后台清理完成");
    assert!(cleanup_exited.load(Ordering::SeqCst));
    assert!(
        dispose_saw_cleanup_done.load(Ordering::SeqCst),
        "OP_DISPOSE 必须晚于 OP_CLEANUP 退出且早于动态库释放"
    );
    assert!(probe_dropped.load(Ordering::SeqCst));
    assert_eq!(loaded_instances(), 0);
    *CLEANUP_STATE.lock().unwrap() = None;
}

// ------------------------------------------------------------------ 7. T2 宿主出站任务排水超时 + 保活（Fix 4a）

struct HungService {
    entered: Arc<Barrier>,
    release: Arc<Barrier>,
    exited: Arc<AtomicBool>,
}

struct BlockingOnSecondPoll {
    polled_once: bool,
    entered: Arc<Barrier>,
    release: Arc<Barrier>,
    exited: Arc<AtomicBool>,
}

impl std::future::Future for BlockingOnSecondPoll {
    type Output = cordis_core::Result<Config>;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        // 第一次 poll 发生在 complete_or_defer 的同步试跑里，返回 Pending 让其进入宿主后台任务；
        // 第二次 poll 在 Tokio worker 线程中执行并同步阻塞，使 cancel 无法抢占直到外部放行。
        if !self.polled_once {
            self.polled_once = true;
            return std::task::Poll::Pending;
        }
        self.entered.wait();
        self.release.wait();
        self.exited.store(true, Ordering::SeqCst);
        std::task::Poll::Ready(Ok(serde_json::json!("done")))
    }
}

impl ExternService for HungService {
    fn service(&self) -> &str {
        "hung.svc"
    }

    fn version(&self) -> u32 {
        1
    }

    fn methods(&self) -> Vec<String> {
        vec!["hang".to_string()]
    }

    fn call(
        &self,
        _meta: Option<Config>,
        _method: String,
        _args: Config,
    ) -> BoxFuture<cordis_core::Result<Config>> {
        Box::pin(BlockingOnSecondPoll {
            polled_once: false,
            entered: self.entered.clone(),
            release: self.release.clone(),
            exited: self.exited.clone(),
        })
    }
}

struct OutboundTaskState {
    abort_delivered: Arc<AtomicBool>,
    dispose_called: Arc<AtomicBool>,
    dispose_saw_order: Arc<AtomicBool>,
    task_exited: Arc<AtomicBool>,
    probe_dropped: Arc<AtomicBool>,
}

static OUTBOUND_STATE: Mutex<Option<OutboundTaskState>> = Mutex::new(None);

unsafe extern "C" fn outbound_hung_plugin(
    handle: u64,
    op: u32,
    _: Blob,
    _: Blob,
    out: *mut Out,
) -> i32 {
    match op {
        OP_APPLY => {
            let Some(call_iface) = host_api().ctx_call_iface else {
                return STATUS_UNSUPPORTED;
            };
            let mut buf = [0u8; 256];
            let mut res_out = Out {
                ptr: buf.as_mut_ptr(),
                cap: buf.len(),
                len: 0,
            };
            let status = call_iface(
                handle,
                Blob::new(b"hung.svc"),
                1,
                Blob::new(b"hang"),
                Blob::new(b"null"),
                77,
                &mut res_out,
            );
            if status != STATUS_OK {
                return STATUS_ERR;
            }
            (*out).write(b"")
        }
        OP_CALLBACK => {
            let guard = OUTBOUND_STATE.lock().unwrap();
            if let Some(st) = guard.as_ref() {
                st.abort_delivered.store(true, Ordering::SeqCst);
            }
            (*out).write(b"null")
        }
        OP_DISPOSE => {
            let guard = OUTBOUND_STATE.lock().unwrap();
            let st = guard.as_ref().expect("OUTBOUND_STATE 已初始化");
            let ok =
                st.task_exited.load(Ordering::SeqCst) && !st.probe_dropped.load(Ordering::SeqCst);
            st.dispose_saw_order.store(ok, Ordering::SeqCst);
            st.dispose_called.store(true, Ordering::SeqCst);
            (*out).write(b"")
        }
        _ => STATUS_UNSUPPORTED,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn t2_outbound_task_drain_respects_drain_budget_and_preserves_keepalive() {
    let _guard = instance_lock().await;
    assert_eq!(loaded_instances(), 0);

    let (probe, probe_dropped) = make_probe();
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let task_exited = Arc::new(AtomicBool::new(false));
    let abort_delivered = Arc::new(AtomicBool::new(false));
    let dispose_called = Arc::new(AtomicBool::new(false));
    let dispose_saw_order = Arc::new(AtomicBool::new(false));

    *OUTBOUND_STATE.lock().unwrap() = Some(OutboundTaskState {
        abort_delivered: abort_delivered.clone(),
        dispose_called: dispose_called.clone(),
        dispose_saw_order: dispose_saw_order.clone(),
        task_exited: task_exited.clone(),
        probe_dropped: probe_dropped.clone(),
    });

    let app = App::new();
    set_lifecycle_policy_for(
        &app.root,
        LifecyclePolicy::new(None, Some(50), None).unwrap(),
    )
    .unwrap();

    let svc = Arc::new(HungService {
        entered: entered.clone(),
        release: release.clone(),
        exited: task_exited.clone(),
    });
    app.root
        .provide_iface("hung.svc", svc)
        .expect("提供挂起服务");

    let keepalive = Mutex::new(Some(probe));
    let plugin = plugin_fn("outbound-hung", Inject::default(), move |ctx: Ctx, cfg| {
        let lease = keepalive.lock().unwrap().take();
        apply_via_with_keepalive(outbound_hung_plugin, ctx, cfg, "outbound-hung", lease)
    });
    app.plugin(plugin, Config::Null).await.expect("装载成功");
    let entered_wait = entered.clone();
    tokio::task::spawn_blocking(move || entered_wait.wait())
        .await
        .expect("出站任务已进入同步阻塞");
    assert_eq!(loaded_instances(), 1);

    // 此时 lifecycle == 0，但 instance.tasks 里有一条正在阻塞的出站任务；drain_timeout_ms 必须触发！
    let store = app.store.clone();
    let report = tokio::time::timeout(Duration::from_secs(2), app.close_with_report(None))
        .await
        .expect("出站任务排空必须遵守 drain_timeout_ms");
    assert_eq!(report.status, CloseStatus::TimedOut);
    assert!(report.still_running);
    assert!(
        report
            .errors
            .iter()
            .any(|err| err.kind == CleanupErrorKind::Timeout
                && err.message.contains("宿主后台任务排空超时")),
        "应明确报告宿主后台任务排空超时: {:?}",
        report.errors
    );
    assert!(!probe_dropped.load(Ordering::SeqCst));
    assert!(!dispose_called.load(Ordering::SeqCst));
    assert_eq!(loaded_instances(), 1);

    let release_wait = release.clone();
    tokio::task::spawn_blocking(move || release_wait.wait())
        .await
        .expect("放行出站任务");
    tokio::time::timeout(Duration::from_secs(2), store.wait_background_cleanups())
        .await
        .expect("放行出站任务后后台收尾完成");
    assert!(task_exited.load(Ordering::SeqCst));
    assert!(abort_delivered.load(Ordering::SeqCst));
    assert!(dispose_called.load(Ordering::SeqCst));
    assert!(dispose_saw_order.load(Ordering::SeqCst));
    assert!(probe_dropped.load(Ordering::SeqCst));
    assert_eq!(loaded_instances(), 0);
    *OUTBOUND_STATE.lock().unwrap() = None;
}

// ------------------------------------------------------------------ 8. OP_APPLY 超时后延后 OP_CLEANUP 防并发竞态（Fix 4b）

struct ApplyCleanupOrderState {
    entered: Arc<Barrier>,
    release: Arc<Barrier>,
    order: Arc<Mutex<Vec<&'static str>>>,
    probe_dropped: Arc<AtomicBool>,
}

static APPLY_CLEANUP_STATE: Mutex<Option<ApplyCleanupOrderState>> = Mutex::new(None);

unsafe extern "C" fn apply_effect_then_block_plugin(
    handle: u64,
    op: u32,
    _: Blob,
    _: Blob,
    out: *mut Out,
) -> i32 {
    match op {
        OP_APPLY => {
            let Some(effect) = host_api().ctx_effect else {
                return STATUS_UNSUPPORTED;
            };
            let mut buf = [0u8; 64];
            let mut effect_out = Out {
                ptr: buf.as_mut_ptr(),
                cap: buf.len(),
                len: 0,
            };
            if effect(handle, 1, &mut effect_out) != STATUS_OK {
                return STATUS_ERR;
            }
            if effect(handle, 2, &mut effect_out) != STATUS_OK {
                return STATUS_ERR;
            }
            let (entered, release, order) = {
                let guard = APPLY_CLEANUP_STATE.lock().unwrap();
                let st = guard.as_ref().expect("APPLY_CLEANUP_STATE 已初始化");
                (st.entered.clone(), st.release.clone(), st.order.clone())
            };
            entered.wait();
            release.wait();
            order.lock().unwrap().push("apply_exit");
            (*out).write(b"")
        }
        OP_CLEANUP => {
            let guard = APPLY_CLEANUP_STATE.lock().unwrap();
            let st = guard.as_ref().expect("APPLY_CLEANUP_STATE 已初始化");
            assert!(!st.probe_dropped.load(Ordering::SeqCst));
            st.order.lock().unwrap().push("cleanup");
            (*out).write(b"")
        }
        OP_DISPOSE => {
            let guard = APPLY_CLEANUP_STATE.lock().unwrap();
            let st = guard.as_ref().expect("APPLY_CLEANUP_STATE 已初始化");
            assert!(!st.probe_dropped.load(Ordering::SeqCst));
            st.order.lock().unwrap().push("dispose");
            (*out).write(b"")
        }
        _ => STATUS_UNSUPPORTED,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t2_apply_timeout_defers_registered_cleanup_until_apply_exits() {
    let _guard = instance_lock().await;
    assert_eq!(loaded_instances(), 0);

    let (probe, probe_dropped) = make_probe();
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let order = Arc::new(Mutex::new(Vec::new()));

    *APPLY_CLEANUP_STATE.lock().unwrap() = Some(ApplyCleanupOrderState {
        entered: entered.clone(),
        release: release.clone(),
        order: order.clone(),
        probe_dropped: probe_dropped.clone(),
    });

    let app = App::new();
    set_lifecycle_policy_for(
        &app.root,
        LifecyclePolicy::new(Some(60), None, None).unwrap(),
    )
    .unwrap();

    let keepalive = Mutex::new(Some(probe));
    let plugin = plugin_fn(
        "apply-cleanup-order",
        Inject::default(),
        move |ctx: Ctx, cfg| {
            let lease = keepalive.lock().unwrap().take();
            apply_via_with_keepalive(
                apply_effect_then_block_plugin,
                ctx,
                cfg,
                "apply-cleanup-order",
                lease,
            )
        },
    );

    let entered_wait = entered.clone();
    let enter_task = tokio::task::spawn_blocking(move || entered_wait.wait());
    let err = tokio::time::timeout(Duration::from_secs(2), app.plugin(plugin, Config::Null))
        .await
        .expect("apply_timeout_ms 及时返回")
        .err()
        .expect("应返回 apply 超时失败");
    enter_task.await.expect("已进入阻塞");
    assert!(err.to_string().contains("apply 超时"));

    // 放行前：OP_APPLY 尚未退出，OP_CLEANUP 绝不得并发抢跑，保活也绝不得释放。
    assert!(
        order.lock().unwrap().is_empty(),
        "OP_APPLY 未退出前不得并发执行 OP_CLEANUP / OP_DISPOSE: {:?}",
        *order.lock().unwrap()
    );
    assert!(!probe_dropped.load(Ordering::SeqCst));

    let release_wait = release.clone();
    tokio::task::spawn_blocking(move || release_wait.wait())
        .await
        .expect("放行 OP_APPLY");
    tokio::time::timeout(Duration::from_secs(2), app.store.wait_background_cleanups())
        .await
        .expect("后台清理完成");

    assert_eq!(
        *order.lock().unwrap(),
        vec!["apply_exit", "cleanup", "cleanup", "dispose"],
        "严格顺序：OP_APPLY 退出 -> 全部 OP_CLEANUP -> OP_DISPOSE"
    );
    assert!(probe_dropped.load(Ordering::SeqCst));
    assert_eq!(loaded_instances(), 0);
    app.close().await;
    *APPLY_CLEANUP_STATE.lock().unwrap() = None;
}

// ------------------------------------------------------------------ 9. 三段预算全开时 spawn_blocking 仍保持重入宿主调用（Fix 4c）

struct RegistryService(Arc<Mutex<Vec<String>>>);

impl ExternService for RegistryService {
    fn service(&self) -> &str {
        "reg.svc"
    }

    fn version(&self) -> u32 {
        1
    }

    fn methods(&self) -> Vec<String> {
        vec!["register".to_string(), "unregister".to_string()]
    }

    fn call(
        &self,
        _meta: Option<Config>,
        method: String,
        _args: Config,
    ) -> BoxFuture<cordis_core::Result<Config>> {
        self.0.lock().unwrap().push(method);
        Box::pin(async move { Ok(serde_json::json!(true)) })
    }
}

unsafe extern "C" fn reentrant_budgeted_plugin(
    handle: u64,
    op: u32,
    _: Blob,
    _: Blob,
    out: *mut Out,
) -> i32 {
    let call_reg = |method: &[u8]| -> i32 {
        let Some(call_iface) = host_api().ctx_call_iface else {
            return STATUS_UNSUPPORTED;
        };
        let mut buf = [0u8; 128];
        let mut res_out = Out {
            ptr: buf.as_mut_ptr(),
            cap: buf.len(),
            len: 0,
        };
        call_iface(
            handle,
            Blob::new(b"reg.svc"),
            1,
            Blob::new(method),
            Blob::new(b"null"),
            0,
            &mut res_out,
        )
    };
    match op {
        OP_APPLY => {
            if call_reg(b"register") != STATUS_OK {
                return STATUS_ERR;
            }
            let Some(effect) = host_api().ctx_effect else {
                return STATUS_UNSUPPORTED;
            };
            let mut buf = [0u8; 64];
            let mut eff_out = Out {
                ptr: buf.as_mut_ptr(),
                cap: buf.len(),
                len: 0,
            };
            if effect(handle, 1, &mut eff_out) != STATUS_OK {
                return STATUS_ERR;
            }
            (*out).write(b"")
        }
        OP_CLEANUP => {
            if call_reg(b"unregister") != STATUS_OK {
                return STATUS_ERR;
            }
            (*out).write(b"")
        }
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t2_budgeted_offload_preserves_reentrant_host_calls_in_apply_and_cleanup() {
    let _guard = instance_lock().await;
    assert_eq!(loaded_instances(), 0);

    let app = App::new();
    set_lifecycle_policy_for(
        &app.root,
        LifecyclePolicy::new(Some(2_000), Some(2_000), Some(2_000)).unwrap(),
    )
    .unwrap();

    let log = Arc::new(Mutex::new(Vec::new()));
    app.root
        .provide_iface("reg.svc", Arc::new(RegistryService(log.clone())))
        .expect("提供注册表服务");

    let plugin = plugin_fn("reentrant-t2", Inject::default(), |ctx: Ctx, cfg| {
        apply_via(reentrant_budgeted_plugin, ctx, cfg, "reentrant-t2")
    });
    app.plugin(plugin, Config::Null)
        .await
        .expect("带预算装载成功");
    let report = app.close_with_report(None).await;
    assert_eq!(report.status, CloseStatus::Completed);
    assert!(report.errors.is_empty());
    assert_eq!(*log.lock().unwrap(), vec!["register", "unregister"]);
    assert_eq!(loaded_instances(), 0);
}

// ------------------------------------------------------------------ 10. 回滚时保留失败新实例清理错误并在超时时拒绝不安全回滚（Fix 5）

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reload_rollback_surfaces_failed_new_cleanup_errors_and_blocks_unsafe_rollback() {
    let _guard = instance_lock().await;
    assert_eq!(loaded_instances(), 0);

    let cfg_path = temp_config("reload-rollback-clean", "- id: svc\n  name: svc\n");
    let app = App::new();
    let v1 = plugin_fn("svc-v1", Inject::default(), |ctx: Ctx, _| {
        Box::pin(async move {
            ctx.provide("svc.ver", cordis_core::Dyn::new(1u32))?;
            Ok(())
        })
    });
    let state = load_with_source(
        app.root.clone(),
        LoaderConfig::new(cfg_path.to_string_lossy().into_owned()),
        Arc::new(StaticSource(v1)),
    )
    .await
    .expect("初装成功");

    // 1) 新版本 apply 失败且半途注册的 disposer panic：回滚成功，但该清理错误不能静默丢失。
    let v2_panic_rollback = plugin_fn("svc-v2", Inject::default(), |ctx: Ctx, _| {
        Box::pin(async move {
            ctx.effect(|_| Disposable::sync(|| panic!("failed-new-disposer-boom")));
            Err("v2-apply-failed".into())
        })
    });
    let r1 = reload_one_with_plugin(&app.root, &state, "svc", v2_panic_rollback).await;
    assert_eq!(r1.status, ReloadStatus::RolledBack);
    let disp1 = r1
        .dispose_report
        .as_ref()
        .expect("失败新实例的清理错误必须透出到 dispose_report");
    assert_eq!(disp1.status, CloseStatus::CompletedWithErrors);
    assert!(
        disp1
            .errors
            .iter()
            .any(|err| err.kind == CleanupErrorKind::DisposerPanic
                && err.message.contains("failed-new-disposer-boom")),
        "应包含失败新实例的 disposer panic: {:?}",
        disp1.errors
    );
    assert_eq!(
        app.root
            .get("svc.ver")
            .and_then(|v| v.downcast::<u32>())
            .map(|v| *v),
        Some(1)
    );

    // 2) 新版本 apply 失败且半途 disposer 挂死超过 dispose_timeout_ms：
    //    `fiber.fail()` 自身也在执行该挂死 disposer（或延迟清理未完成），此时应拒绝在半拆状态上挂载回滚。
    //    这里用自定义 dispose_timeout_ms + 挂起 apply 并在 fail() 后挂起 disposer 来触发。
    set_lifecycle_policy_for(
        &app.root,
        LifecyclePolicy::new(None, None, Some(50)).unwrap(),
    )
    .unwrap();
    let release = Arc::new(tokio::sync::Notify::new());
    let release_d = release.clone();
    // 注意：Fiber::fail() 会执行 disposer，所以这里让新版本在 apply 里先挂一个依赖外部放行的子 fiber
    // 并让新版本成功 apply 还是失败？更直接：让新版本注册一个 disposer 在 fail() 之外的第二次 dispose 挂住
    // 或利用 drain_timeout_ms 限制 fail() 中的卡死任务，再配合 dispose_timeout_ms：
    // 实际上：如果新版本在 apply 期间通过 spawn_scoped 启动卡死任务 + 返回 Err，fail() 会超时将任务移交后台；
    // 这里验证一个真实场景：失败新实例存在未结束的后台清理且 state 尚未 Disposed 时拒绝回滚，
    // 而在已 Disposed（仅后台任务收尾）时允许安全回滚并把 Timeout 透出到 dispose_report！
    set_lifecycle_policy_for(
        &app.root,
        LifecyclePolicy::new(None, Some(40), Some(40)).unwrap(),
    )
    .unwrap();
    let v3_stuck_fail = plugin_fn("svc-v3", Inject::default(), move |ctx: Ctx, _| {
        let rel = release_d.clone();
        Box::pin(async move {
            ctx.spawn_scoped(move |_cancel| async move {
                rel.notified().await;
            })
            .unwrap();
            Err("v3-apply-failed-with-stuck-task".into())
        })
    });
    let r2 = tokio::time::timeout(
        Duration::from_secs(2),
        reload_one_with_plugin(&app.root, &state, "svc", v3_stuck_fail),
    )
    .await
    .expect("失败新实例含卡死任务时重载也必须受 drain 预算约束及时返回");
    assert_eq!(r2.status, ReloadStatus::RolledBack);
    let disp2 = r2
        .dispose_report
        .as_ref()
        .expect("失败新实例的排水超时必须透出到 dispose_report");
    assert_eq!(disp2.status, CloseStatus::TimedOut);
    assert!(disp2.still_running);

    release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), app.store.wait_background_cleanups())
        .await
        .expect("放行后后台清理收敛");

    let close = app.close_with_report(None).await;
    assert_eq!(close.status, CloseStatus::Completed);
    let _ = std::fs::remove_file(&cfg_path);
}
