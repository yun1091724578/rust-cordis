//! P0-2：派发看门狗按 App 隔离；进程级设置只作为没有 per-App 条目时的回落。
//!
//! 独立测试二进制：回落测试会改进程级设置，不能与其他文件共享进程。

use cordis_abi::{
    Blob, Out, OP_APPLY, OP_CALLBACK, OP_DISPOSE, STATUS_PENDING, STATUS_UNSUPPORTED,
};
use cordis_core::{plugin_fn, App, Config, ExternValue, Fiber, FiberState, Inject, Plugin, Ret};
use cordis_host::loader::{
    load_with_source, InterpolationMode, LoadedFile, LoaderConfig, PluginSource,
};
use cordis_host::patch::Entry;
use cordis_host::{
    apply_via, host_api, pending_len, set_dispatch_policy, set_dispatch_policy_for, CallFn,
    DispatchPolicy,
};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

static SCOPE_HANDLES: Mutex<Vec<u64>> = Mutex::new(Vec::new());
static FALLBACK_HANDLE: Mutex<u64> = Mutex::new(0);
static LOADER_HANDLE: Mutex<u64> = Mutex::new(0);

fn json_of(ret: Ret) -> Option<Value> {
    ret.and_then(|value| value.downcast::<ExternValue>())
        .map(|value| value.0.clone())
}

/// 在插件 apply 期间注册一个事件监听器；返回 `false` 表示宿主不支持 `ctx_on`。
unsafe fn register_event(handle: u64, event: &[u8]) -> bool {
    let Some(api) = host_api().ctx_on else {
        return false;
    };
    let mut buffer = [0u8; 64];
    let mut event_out = Out {
        ptr: buffer.as_mut_ptr(),
        cap: buffer.len(),
        len: 0,
    };
    api(handle, Blob::new(event), 7, &mut event_out);
    true
}

fn pending_plugin(name: &'static str, call: CallFn) -> Arc<dyn Plugin> {
    plugin_fn(name, Inject::default(), move |ctx, config| {
        apply_via(call, ctx, config, name)
    })
}

async fn mount_pending(app: &App, name: &'static str, call: CallFn) -> Arc<Fiber> {
    let fiber = app.root.plugin(pending_plugin(name, call), Config::Null);
    fiber.settle().await;
    assert_eq!(fiber.state(), FiberState::Active, "{name} 必须成功挂载");
    fiber
}

async fn wait_for_pending(handle: u64) {
    for _ in 0..200 {
        if pending_len(handle) == 1 {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("派发必须先进入挂起（看门狗只对挂起生效）");
}

async fn wait_for_no_pending(handle: u64) {
    for _ in 0..200 {
        if pending_len(handle) == 0 {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("放弃等待后挂起条目必须撤销");
}

// ---------------------------------------------------------------- 两个 App 的策略互不影响

unsafe extern "C" fn scope_call(handle: u64, op: u32, _: Blob, _: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => {
            SCOPE_HANDLES.lock().unwrap().push(handle);
            if !register_event(handle, b"scope/pending") {
                return STATUS_UNSUPPORTED;
            }
            (*out).write(b"")
        }
        OP_CALLBACK => STATUS_PENDING,
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

/// A 的 50ms 看门狗到点会结算 A 的挂起；B 配的是 500ms，此刻必须仍然挂起。
/// 进程级默认是 `None`，所以只要策略不是按 App 隔离，B 也会被 A 拉下水。
#[tokio::test(start_paused = true)]
async fn per_app_policies_do_not_leak_between_apps() {
    SCOPE_HANDLES.lock().unwrap().clear();
    let app_a = App::new();
    let app_b = App::new();
    set_dispatch_policy_for(
        &app_a.root,
        DispatchPolicy {
            timeout_ms: Some(50),
            ..DispatchPolicy::default()
        },
    );
    set_dispatch_policy_for(
        &app_b.root,
        DispatchPolicy {
            timeout_ms: Some(500),
            ..DispatchPolicy::default()
        },
    );

    let fiber_a = mount_pending(&app_a, "scope-a", scope_call).await;
    let fiber_b = mount_pending(&app_b, "scope-b", scope_call).await;
    let (handle_a, handle_b) = {
        let handles = SCOPE_HANDLES.lock().unwrap();
        assert_eq!(handles.len(), 2, "两个 App 各注册一个实例");
        (handles[0], handles[1])
    };

    let emit_a = {
        let ctx = app_a.root.clone();
        tokio::spawn(async move { ctx.bail("scope/pending", Vec::new()).await })
    };
    let emit_b = {
        let ctx = app_b.root.clone();
        tokio::spawn(async move { ctx.bail("scope/pending", Vec::new()).await })
    };
    wait_for_pending(handle_a).await;
    wait_for_pending(handle_b).await;

    let ret_a = tokio::time::timeout(Duration::from_secs(10), emit_a)
        .await
        .expect("A 的看门狗必须结算这次派发")
        .expect("派发任务不得 panic");
    assert_eq!(json_of(ret_a), None, "超时结算后监听器等于没有意见");
    assert_eq!(pending_len(handle_a), 0, "A 超时后挂起条目必须移出");
    assert_eq!(
        pending_len(handle_b),
        1,
        "B 的 500ms 策略不能被 A 的 50ms 缩短（策略必须按 App 隔离）"
    );

    // B 仍在挂起：撤销调用方等待，清理记录，避免把实例留在表里。
    emit_b.abort();
    let _ = emit_b.await;
    wait_for_no_pending(handle_b).await;

    fiber_a.dispose().await;
    fiber_b.dispose().await;
}

// ---------------------------------------------------------------- 进程级回落

unsafe extern "C" fn fallback_call(handle: u64, op: u32, _: Blob, _: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => {
            *FALLBACK_HANDLE.lock().unwrap() = handle;
            if !register_event(handle, b"scope/fallback") {
                return STATUS_UNSUPPORTED;
            }
            (*out).write(b"")
        }
        OP_CALLBACK => STATUS_PENDING,
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

/// 没有 per-App 条目时，实例仍取进程级 `set_dispatch_policy` 的快照。
#[tokio::test(start_paused = true)]
async fn global_policy_remains_the_fallback() {
    set_dispatch_policy(DispatchPolicy {
        timeout_ms: Some(50),
        ..DispatchPolicy::default()
    });
    *FALLBACK_HANDLE.lock().unwrap() = 0;

    let app = App::new();
    let fiber = mount_pending(&app, "scope-fallback", fallback_call).await;
    let handle = *FALLBACK_HANDLE.lock().unwrap();
    assert_ne!(handle, 0, "apply 必须登记实例");

    let emit = {
        let ctx = app.root.clone();
        tokio::spawn(async move { ctx.bail("scope/fallback", Vec::new()).await })
    };
    wait_for_pending(handle).await;

    let ret = tokio::time::timeout(Duration::from_secs(10), emit)
        .await
        .expect("进程级回落看门狗必须结算这次派发")
        .expect("派发任务不得 panic");
    assert_eq!(json_of(ret), None, "超时结算后监听器等于没有意见");
    assert_eq!(pending_len(handle), 0, "超时后挂起条目必须移出");

    fiber.dispose().await;
    set_dispatch_policy(DispatchPolicy::default());
}

// ---------------------------------------------------------------- 装载器接线

struct PendingSource;

impl PluginSource for PendingSource {
    fn load(
        &self,
        _entry: &Entry,
        _search: &[PathBuf],
        _revision: u64,
    ) -> std::result::Result<LoadedFile, String> {
        let path = PathBuf::from("<fake:loader-pending>");
        Ok(LoadedFile {
            plugin: pending_plugin("loader-pending", loader_call),
            source: path.clone(),
            staged: path,
            fingerprint: None,
        })
    }
}

unsafe extern "C" fn loader_call(handle: u64, op: u32, _: Blob, _: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => {
            *LOADER_HANDLE.lock().unwrap() = handle;
            if !register_event(handle, b"scope/loader") {
                return STATUS_UNSUPPORTED;
            }
            (*out).write(b"")
        }
        OP_CALLBACK => STATUS_PENDING,
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "cordis-dispatch-scope-{}-{}-{tag}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&path).expect("建临时目录");
        Self(path)
    }

    fn write(&self, name: &str, text: &str) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, text).expect("写配置文件");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn loader_config_for(path: &Path) -> LoaderConfig {
    LoaderConfig {
        path: path.to_string_lossy().into_owned(),
        patches: Vec::new(),
        plugin_dir: None,
        strict: true,
        strict_cycles: false,
        hmr: None,
        js_evaluator: None,
        interpolation: InterpolationMode::Builtin,
        dispatch_policy: Some(DispatchPolicy {
            timeout_ms: Some(50),
            ..DispatchPolicy::default()
        }),
        host_limits: None,
    }
}

/// `LoaderConfig.dispatch_policy` 在挂载前写进本 App 的策略表，实例注册时取到它。
#[tokio::test(start_paused = true)]
async fn loader_config_policy_applies_to_mounted_instances() {
    *LOADER_HANDLE.lock().unwrap() = 0;
    let dir = TempDir::new("loader");
    let path = dir.write("cordis.yml", "- id: alpha\n  name: alpha\n");
    let app = App::new();
    let state = load_with_source(
        app.root.clone(),
        loader_config_for(&path),
        Arc::new(PendingSource),
    )
    .await
    .expect("装载器应装载成功");
    assert_eq!(state.mounted_labels(), vec!["alpha".to_string()]);
    let handle = *LOADER_HANDLE.lock().unwrap();
    assert_ne!(handle, 0, "装载的插件必须注册实例");

    let emit = {
        let ctx = app.root.clone();
        tokio::spawn(async move { ctx.bail("scope/loader", Vec::new()).await })
    };
    wait_for_pending(handle).await;

    let ret = tokio::time::timeout(Duration::from_secs(10), emit)
        .await
        .expect("LoaderConfig 的看门狗必须结算这次派发")
        .expect("派发任务不得 panic");
    assert_eq!(json_of(ret), None, "超时结算后监听器等于没有意见");
    assert_eq!(pending_len(handle), 0, "超时后挂起条目必须移出");

    drop(state);
    app.close().await;
}
