//! M1（ABI 能力五项）：`ctx_use` / `@native:` / `ctx_dispose_self`。
//!
//! 全部无 DLL（D9）：
//! - 子组件经进程级 [`cordis_host::set_plugin_source`] 注入的假 `PluginSource`
//!   （D8 的 seam，与 `set_native_registry` 对称）；
//! - `@native:<name>` 经 [`cordis_host::NativeRegistry`]（D8：host 类型不能进
//!   core 的 App 字段，所以是进程级全局 + pub setter）；
//! - 自卸载走既有 loader 行状态机制：fiber dispose → `internal/plugin` 观察者
//!   → 行标记 disabled + **落盘**——绝不静默。
//!
//! 夹具铁律（D10）：fake `CallFn` 内禁止 assert；跨线程状态用 static `Mutex`。
//! 本文件的四个用例并行跑且共享进程级 source，所以假 source 必须**认识全部**
//! 用到的名字（谁后 set 都等价）。

use cordis_abi::{Blob, Out, OP_APPLY, OP_CALLBACK, OP_DISPOSE, STATUS_ERR, STATUS_OK};
use cordis_core::{App, Config, Ctx, Dyn, ExternValue, FiberState, Inject, Plugin};
use cordis_host::loader::{load_with_source, LoadedFile, LoaderConfig, PluginSource};
use cordis_host::patch::Entry;
use cordis_host::{apply_via, host_api, set_native_registry, set_plugin_source, NativeRegistry};
use serde_json::json;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 派生句柄的 bit63 标记（与 `cordis_host::DERIVED_HANDLE_TAG` 同值）。
const TAG: u64 = 1 << 63;

/// 假插件源：认识本文件全部用例的名字（进程级 seam，并行用例谁 set 都等价）。
struct FixtureSource;

impl PluginSource for FixtureSource {
    fn load(
        &self,
        entry: &Entry,
        _search: &[PathBuf],
        _revision: u64,
    ) -> Result<LoadedFile, String> {
        let plugin: Arc<dyn Plugin> = match entry.name.as_str() {
            "fake-child" => {
                cordis_core::plugin_fn("fake-child", Inject::default(), |ctx: Ctx, _cfg| {
                    Box::pin(async move {
                        ctx.provide(
                            "child-svc",
                            Dyn::new(ExternValue(json!({ "from": "fake-child" }))),
                        )?;
                        Ok(())
                    })
                })
            }
            "boom-child" => {
                cordis_core::plugin_fn("boom-child", Inject::default(), |ctx: Ctx, _cfg| {
                    Box::pin(async move {
                        ctx.provide(
                            "boom-child-svc",
                            Dyn::new(ExternValue(json!({ "from": "boom-child" }))),
                        )?;
                        Ok(())
                    })
                })
            }
            "abi-self" => Arc::new(cordis_core::FnPlugin {
                name: "abi-self",
                key: cordis_core::unique_key("abi-self"),
                inject: Inject::default(),
                validator: None,
                f: Arc::new(|ctx: Ctx, config: Config| apply_via(t8_call, ctx, config, "abi-self")),
            }),
            other => return Err(format!("假插件源不认识 \"{other}\"")),
        };
        let path = PathBuf::from(format!("<fixture:{}>", entry.name));
        Ok(LoadedFile {
            plugin,
            source: path.clone(),
            staged: path,
            fingerprint: None,
        })
    }
}

/// 事件轮询：等到条件成立（给异步 settle / dispose 任务留时间），超时即败。
async fn until(what: &str, mut ready: impl FnMut() -> bool) {
    for _ in 0..500 {
        if ready() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("等待超时: {what}");
}

/// 从 `{"handle": <u64>}` 应答里取句柄。
fn handle_of(text: &str) -> Option<u64> {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()?
        .get("handle")?
        .as_u64()
}

fn abi_plugin(name: &'static str, call: cordis_host::CallFn) -> Arc<dyn Plugin> {
    Arc::new(cordis_core::FnPlugin {
        name,
        key: cordis_core::unique_key(name),
        inject: Inject::default(),
        validator: None,
        f: Arc::new(move |ctx: Ctx, config: Config| apply_via(call, ctx, config, name)),
    })
}

/// 一次宿主调用（三参形态）的结局快照。
unsafe fn probe3(
    f: Option<unsafe extern "C" fn(u64, Blob, Blob, *mut Out) -> i32>,
    handle: u64,
    a: &[u8],
    b: &[u8],
) -> (i32, String) {
    let mut buffer = [0u8; 1024];
    let mut out = Out {
        ptr: buffer.as_mut_ptr(),
        cap: buffer.len(),
        len: 0,
    };
    match f {
        Some(f) => {
            let status = f(handle, Blob::new(a), Blob::new(b), &mut out);
            (
                status,
                String::from_utf8_lossy(&buffer[..out.len]).into_owned(),
            )
        }
        None => (-1, "宿主不支持这个函数（None）".into()),
    }
}

// ------------------------------------------------------------------ 测试 5

static T5: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// 假插件：OP_APPLY 里 `ctx_use` 挂假子组件，结局落日志。
unsafe extern "C" fn t5_call(handle: u64, op: u32, _: Blob, _: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => {
            let (status, text) = probe3(host_api().ctx_use, handle, b"fake-child", b"{}");
            T5.lock().unwrap().push(format!("use:{status}:{text}"));
            (*out).write(b"")
        }
        OP_DISPOSE => {
            T5.lock().unwrap().push("dispose".into());
            (*out).write(b"")
        }
        _ => cordis_abi::STATUS_UNSUPPORTED,
    }
}

/// M1 红测试 5：`ctx_use` 挂子组件，且子组件随父卸载（假 `PluginSource`，无 DLL）。
///
/// "新 fiber 是调用方 fiber 的一个 effect"：父 dispose ⇒ 子 dispose 由内核的
/// effect 链保证，宿主不需要任何额外记账——本测试同时钉住这条性质。
#[tokio::test]
async fn abi_use_mounts_a_child_component_and_disposes_it_with_the_parent() {
    T5.lock().unwrap().clear();
    set_plugin_source(Arc::new(FixtureSource));

    let app = App::new();
    let fiber = app.root.plugin(abi_plugin("t5-abi", t5_call), Config::Null);
    fiber.settle().await;
    assert_eq!(fiber.state(), FiberState::Active, "父插件应装载成功");

    let log = T5.lock().unwrap().clone();
    assert_eq!(log.len(), 1, "OP_APPLY 里应恰有一次 use：{log:?}");
    assert!(
        log[0].starts_with(&format!("use:{STATUS_OK}:")),
        "ctx_use 必须成功，实际：{}",
        log[0]
    );
    let child = handle_of(log[0].splitn(3, ':').nth(2).unwrap_or("")).expect("use 应答里有 handle");
    assert_ne!(child & TAG, 0, "ctx_use 返回的必须是派生句柄：{child:#x}");

    // 子组件真的上线了（settle 异步推进，轮询等它）。
    until("child-svc 上线", || {
        app.root
            .get("child-svc")
            .and_then(|value| value.downcast::<ExternValue>())
            .map(|value| value.0 == json!({ "from": "fake-child" }))
            .unwrap_or(false)
    })
    .await;

    // 父卸载 ⇒ 子随行（effect 链），子提供的服务随之消失。
    fiber.dispose().await;
    until("child-svc 随父卸载消失", || {
        app.root.get("child-svc").is_none()
    })
    .await;
    assert!(
        T5.lock().unwrap().iter().any(|entry| entry == "dispose"),
        "父插件应收到 OP_DISPOSE（正常生命周期）"
    );
}

// ------------------------------------------------------------------ 测试 6

static T6: Mutex<Vec<String>> = Mutex::new(Vec::new());

unsafe extern "C" fn t6_call(handle: u64, op: u32, _: Blob, _: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => {
            let ok = probe3(host_api().ctx_use, handle, b"@native:gizmo", b"{}");
            T6.lock().unwrap().push(format!("gizmo:{}:{}", ok.0, ok.1));
            let missing = probe3(host_api().ctx_use, handle, b"@native:missing", b"{}");
            T6.lock()
                .unwrap()
                .push(format!("missing:{}:{}", missing.0, missing.1));
            (*out).write(b"")
        }
        OP_DISPOSE => (*out).write(b""),
        _ => cordis_abi::STATUS_UNSUPPORTED,
    }
}

/// 假原生组件注册表：只认识 `gizmo`。
struct GizmoRegistry;

impl NativeRegistry for GizmoRegistry {
    fn native(&self, name: &str) -> Option<Arc<dyn Plugin>> {
        (name == "gizmo").then(|| {
            cordis_core::plugin_fn("gizmo", Inject::default(), |ctx: Ctx, _cfg| {
                Box::pin(async move {
                    ctx.provide(
                        "native-svc",
                        Dyn::new(ExternValue(json!({ "from": "@native" }))),
                    )?;
                    Ok(())
                })
            })
        })
    }
}

/// M1 红测试 6：`url = "@native:<name>"` 经 `NativeRegistry` 解析。
///
/// 注册表里有的名字 → 挂上原生组件；没有的名字 → 可读错误
/// （`@native:` 前缀不会被误当成插件名丢给 `PluginSource`）。
#[tokio::test]
async fn abi_use_resolves_native_components_by_at_name() {
    T6.lock().unwrap().clear();
    set_native_registry(Arc::new(GizmoRegistry));

    let app = App::new();
    let fiber = app.root.plugin(abi_plugin("t6-abi", t6_call), Config::Null);
    fiber.settle().await;
    assert_eq!(fiber.state(), FiberState::Active);

    let log = T6.lock().unwrap().clone();
    assert_eq!(log.len(), 2, "应有 gizmo 与 missing 两条记录：{log:?}");
    assert!(
        log[0].starts_with(&format!("gizmo:{STATUS_OK}:")),
        "@native:gizmo 必须挂载成功，实际：{}",
        log[0]
    );
    assert!(
        log[1].starts_with(&format!("missing:{STATUS_ERR}:")),
        "未注册的原生组件必须是可读错误，实际：{}",
        log[1]
    );
    assert!(
        log[1].contains("@native:missing"),
        "错误信息必须带上名字（可定位）：{}",
        log[1]
    );

    until("native-svc 上线", || {
        app.root
            .get("native-svc")
            .and_then(|value| value.downcast::<ExternValue>())
            .map(|value| value.0 == json!({ "from": "@native" }))
            .unwrap_or(false)
    })
    .await;
    fiber.dispose().await;
}

// ------------------------------------------------------------------ 测试 7

static T7: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// 假插件：OP_APPLY 注册事件监听；OP_CALLBACK（事件 boom）里 `ctx_use` 挂子组件。
///
/// 重入路径：事件派发 → 宿主监听器 → OP_CALLBACK → **再进宿主**（ctx_use →
/// with_instance → instances 锁 → mount）。锁结构若在回调派发期间持实例锁，
/// 这里就是死锁——multi_thread 运行时让 timeout 真的能触发。
unsafe extern "C" fn t7_call(handle: u64, op: u32, a: Blob, _: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => {
            let mut buffer = [0u8; 256];
            let mut on_out = Out {
                ptr: buffer.as_mut_ptr(),
                cap: buffer.len(),
                len: 0,
            };
            match host_api().ctx_on {
                Some(on) => {
                    let status = on(handle, Blob::new(b"boom"), 1, &mut on_out);
                    T7.lock().unwrap().push(format!("on:{status}"));
                    (*out).write(b"")
                }
                None => {
                    (*out).write("宿主没有 ctx_on".as_bytes());
                    STATUS_ERR
                }
            }
        }
        OP_CALLBACK => {
            let payload = String::from_utf8_lossy(unsafe { a.as_bytes() }).into_owned();
            if payload.contains("\"id\":1") {
                let (status, text) = probe3(host_api().ctx_use, handle, b"boom-child", b"{}");
                T7.lock()
                    .unwrap()
                    .push(format!("use-in-callback:{status}:{text}"));
            }
            (*out).write(b"")
        }
        OP_DISPOSE => (*out).write(b""),
        _ => cordis_abi::STATUS_UNSUPPORTED,
    }
}

/// M1 红测试 7：回调内部 `ctx_use` 不得死锁（重入安全）。
///
/// 事件处理器里挂子组件是真实需求（收到配置事件 → 拉起依赖）。
/// 宿主的实例表锁只在"取实例"一瞬间持有、克隆后才执行插件代码——
/// 这条结构性质由本测试钉死：整条 emit 必须在时限内完成。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn abi_use_from_inside_a_callback_does_not_deadlock() {
    T7.lock().unwrap().clear();
    set_plugin_source(Arc::new(FixtureSource));

    let app = App::new();
    let fiber = app.root.plugin(abi_plugin("t7-abi", t7_call), Config::Null);
    fiber.settle().await;
    assert_eq!(fiber.state(), FiberState::Active);
    assert!(
        T7.lock().unwrap().first().map(|s| s.as_str()) == Some("on:0"),
        "监听器注册必须成功"
    );

    // 派发事件：监听器（宿主监听器闭包 → OP_CALLBACK → ctx_use → 再进宿主）。
    let emitted =
        tokio::time::timeout(Duration::from_secs(10), app.root.emit("boom", Vec::new())).await;
    assert!(
        emitted.is_ok(),
        "回调内部的 ctx_use 不得死锁（10s 超时触发）"
    );

    let log = T7.lock().unwrap().clone();
    assert!(
        log.iter()
            .any(|entry| entry.starts_with(&format!("use-in-callback:{STATUS_OK}:"))),
        "回调里的 ctx_use 必须成功：{log:?}"
    );
    until("boom-child-svc 上线", || {
        app.root
            .get("boom-child-svc")
            .and_then(|value| value.downcast::<ExternValue>())
            .is_some()
    })
    .await;
    fiber.dispose().await;
}

// ------------------------------------------------------------------ 测试 8

static T8: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// 假插件（loader 行 `abi-self` 的实现体）：
/// - OP_APPLY：注册 `kill-self` 事件监听；
/// - OP_CALLBACK（kill-self）：调 `ctx_dispose_self` 自卸载，结局落日志；
/// - OP_DISPOSE：落日志（证明真的拆了，不是只摘 loader 行）。
unsafe extern "C" fn t8_call(handle: u64, op: u32, a: Blob, _: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => {
            let mut buffer = [0u8; 256];
            let mut on_out = Out {
                ptr: buffer.as_mut_ptr(),
                cap: buffer.len(),
                len: 0,
            };
            match host_api().ctx_on {
                Some(on) => {
                    let status = on(handle, Blob::new(b"kill-self"), 1, &mut on_out);
                    T8.lock().unwrap().push(format!("on:{status}"));
                    (*out).write(b"")
                }
                None => {
                    (*out).write("宿主没有 ctx_on".as_bytes());
                    STATUS_ERR
                }
            }
        }
        OP_CALLBACK => {
            let payload = String::from_utf8_lossy(unsafe { a.as_bytes() }).into_owned();
            if payload.contains("\"id\":1") {
                let mut buffer = [0u8; 256];
                let mut self_out = Out {
                    ptr: buffer.as_mut_ptr(),
                    cap: buffer.len(),
                    len: 0,
                };
                match host_api().ctx_dispose_self {
                    Some(dispose_self) => {
                        let status = dispose_self(handle, &mut self_out);
                        let text = String::from_utf8_lossy(&buffer[..self_out.len]).into_owned();
                        T8.lock()
                            .unwrap()
                            .push(format!("dispose-self:{status}:{text}"));
                    }
                    None => T8.lock().unwrap().push("dispose-self:none".into()),
                }
            }
            (*out).write(b"")
        }
        OP_DISPOSE => {
            T8.lock().unwrap().push("dispose".into());
            (*out).write(b"")
        }
        _ => cordis_abi::STATUS_UNSUPPORTED,
    }
}

/// 每个用例一个临时目录。
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "cordis-m1-self-{}-{}-{tag}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&path).expect("建临时目录");
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// M1 红测试 8：`ctx_dispose_self` 把 loader 行标 disabled 并**持久化**。
///
/// 机制走既有行状态链：fiber dispose → `internal/plugin` 观察者 →
/// `mark_self_disposed`（摘装载表 + 树侧 disabled + 落盘）。
/// 断言三件事：行从装载表消失、**配置文件里**出现 `disabled: true`（持久化，
/// 不是内存里划掉）、插件真的收到 OP_DISPOSE（不是静默消失）。
#[tokio::test]
async fn abi_dispose_self_marks_the_loader_row_disabled_and_persists() {
    T8.lock().unwrap().clear();
    let dir = TempDir::new("dispose-self");
    let config_path = dir.0.join("cordis.yml");
    std::fs::write(&config_path, "- id: abi-self\n  name: abi-self\n").expect("写配置");

    let app = App::new();
    let state = load_with_source(
        app.root.clone(),
        LoaderConfig {
            path: config_path.to_string_lossy().into_owned(),
            patches: Vec::new(),
            plugin_dir: None,
            strict: true,
            strict_cycles: false,
            hmr: None,
            js_evaluator: None,
            interpolation: cordis_host::loader::InterpolationMode::Builtin,
            dispatch_policy: None,
        },
        Arc::new(FixtureSource),
    )
    .await
    .expect("装载器应装载成功");
    assert_eq!(
        state.mounted_labels(),
        vec!["abi-self".to_string()],
        "装载表应恰有一行 abi-self"
    );

    // 触发自卸载：事件 → 插件回调 → ctx_dispose_self。
    app.root.emit("kill-self", Vec::new()).await;

    until("loader 行摘除 + 落盘 + OP_DISPOSE", || {
        let row_gone = state.mounted_labels().is_empty();
        let disposed = T8.lock().unwrap().iter().any(|entry| entry == "dispose");
        row_gone && disposed
    })
    .await;

    let persisted = std::fs::read_to_string(&config_path).expect("配置文件应仍可读");
    assert!(
        persisted.contains("disabled: true"),
        "自卸载必须落盘（重启后不得复活），实际文件内容：\n{persisted}"
    );
    let log = T8.lock().unwrap().clone();
    assert!(
        log.iter().any(|entry| entry.starts_with("dispose-self:0")),
        "ctx_dispose_self 本身应成功：{log:?}"
    );
}
