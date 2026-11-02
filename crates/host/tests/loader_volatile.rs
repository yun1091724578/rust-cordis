//! M4-5：**volatile 配置**——只差 volatile 字段的配置变更**不重挂**。
//!
//! # 上游参照
//! vendor `loader/src/config/entry.ts:145-197`（`_commitVolatile`）：
//! 1. **前置条件三连**（`:146-148`）：改的只有 `config` 一项、fiber 处于 ACTIVE、
//!    `getPrototypeOf(ctx) === parent.ctx`（上下文没被重新派生），
//!    且**原始配置**除 schema 声明的 volatile 字段外逐字相等（`equalExceptVolatile`）；
//! 2. 满足 ⇒ `fiber._config = 新 raw`（下次激活用它）→ 用 `internal/config` +
//!    `resolveConfig` 算出**候选**（`!!js` 会跟着上下文再算一次）；
//! 3. 候选解析失败 ⇒ 警告 + **保留运行值**（不重挂，下次激活用新 raw）；
//! 4. 候选的**普通值**变了（`deepEqual(fiber.config, candidate)`）⇒ 交回普通更新；
//! 5. 否则逐路径把候选值写进**已有的引用**（`updateVolatile`，引用身份不变），
//!    发 `loader/volatile-update`（owner-filter，监听者失败不影响这次更新）。
//!
//! # 本仓的两处落地差异（§8 M4-5）
//! - **声明方式**：上游靠 schemastery 的 `.volatile()` 元数据；本仓是显式声明
//!   （`Plugin::volatile_paths` / `declare_volatile(...)`），ABI 侧走
//!   `OP_VOLATILE_PATHS` 查询 + `OP_VOLATILE_APPLY` 推送；
//! - **引用形态**：上游把引用放进"已解析配置"里；本仓的配置值只有 JSON，
//!   所以引用由内核按路径持有，插件用 `ctx.volatile("data")` 取（同一个
//!   `Arc<VolatileRef>` 在 volatile 提交后**身份不变、值变化**）。
//!
//! 这些测试**不需要动态库**：`PluginSource` seam 用假插件顶替加载。

use cordis_core::{
    App, Config, Ctx, Dyn, ExternValue, Inject, Plugin, ValidationError, VolatileRef,
};
use cordis_host::loader::{
    declare_volatile, load_with_source, Action, LoadedFile, LoaderConfig, LoaderState,
    PluginSource, ReconcileOutcome, ReloadOutcome,
};
use cordis_host::patch::{parse_entries, Entry};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------- 探针

/// 外部能看见的一切：apply 次数、引用、事件。
///
/// "不重挂"这条判据就落在 `applies` 上：volatile 提交**不会**再跑一次 `apply`。
#[derive(Default)]
struct Probe {
    applies: AtomicUsize,
    /// 每次 apply 时拿到的引用（volatile 提交后**同一个 Arc 的值**会变）。
    refs: Mutex<Vec<Arc<VolatileRef>>>,
    /// 行内看到的 `loader/partial-dispose` 载荷（volatile 路径**不该**发它）。
    partial_disposes: Mutex<Vec<Value>>,
    /// 行内看到的 `loader/volatile-update` 载荷。
    volatile_events: Mutex<Vec<Value>>,
}

impl Probe {
    fn applies(&self) -> usize {
        self.applies.load(Ordering::SeqCst)
    }
}

/// 假插件源：`volatile` = 声明了 `["data"]` 的插件；`plain` = 什么都没声明。
struct FakeSource {
    probe: Arc<Probe>,
}

impl PluginSource for FakeSource {
    fn load(
        &self,
        entry: &Entry,
        _search: &[PathBuf],
        _revision: u64,
    ) -> Result<LoadedFile, String> {
        let probe = self.probe.clone();
        match entry.name.as_str() {
            // 声明了 volatile 的插件：校验器要求 `data` 是数字（用来造"无效候选"）。
            "volatile" => {
                let plugin = declare_volatile(
                    cordis_core::plugin_fn_validated(
                        "volatile",
                        Inject::default(),
                        |config: Config| {
                            if config.get("data").and_then(Value::as_f64).is_none() {
                                return Err(ValidationError {
                                    issues: vec!["data: expected number (at data)".to_string()],
                                });
                            }
                            Ok(config)
                        },
                        move |ctx: Ctx, _config| {
                            let probe = probe.clone();
                            Box::pin(async move {
                                probe.applies.fetch_add(1, Ordering::SeqCst);
                                let reference = ctx
                                    .volatile("data")
                                    .expect("声明了 data，内核就该在 load 时建好引用");
                                probe.refs.lock().unwrap().push(reference.clone());
                                // 引用本身就是服务值：测试拿到的句柄跨 volatile 提交保持有效
                                // （插件的真实写法也是这样——引用是稳定的，值不是）。
                                ctx.provide("snapshot", Dyn::shared(reference))?;
                                {
                                    let events = probe.clone();
                                    ctx.on("loader/volatile-update", move |call| {
                                        let events = events.clone();
                                        Box::pin(async move {
                                            events
                                                .volatile_events
                                                .lock()
                                                .unwrap()
                                                .push(payload_of(&call));
                                            None
                                        })
                                    });
                                }
                                {
                                    let events = probe.clone();
                                    ctx.on("loader/partial-dispose", move |call| {
                                        let events = events.clone();
                                        Box::pin(async move {
                                            events
                                                .partial_disposes
                                                .lock()
                                                .unwrap()
                                                .push(payload_of(&call));
                                            None
                                        })
                                    });
                                }
                                Ok(())
                            })
                        },
                    ),
                    &["data"],
                );
                Ok(fake_file("volatile", plugin))
            }
            // 什么都没声明的插件（`volatile_paths` 为空 ⇒ 永远走普通更新）。
            "plain" => {
                let plugin: Arc<dyn Plugin> =
                    cordis_core::plugin_fn("plain", Inject::default(), move |ctx: Ctx, _c| {
                        let probe = probe.clone();
                        Box::pin(async move {
                            probe.applies.fetch_add(1, Ordering::SeqCst);
                            ctx.provide("snapshot", Dyn::new(0u32))?;
                            Ok(())
                        })
                    });
                Ok(fake_file("plain", plugin))
            }
            other => Err(format!("假插件源不认识 \"{other}\"")),
        }
    }
}

fn fake_file(name: &str, plugin: Arc<dyn Plugin>) -> LoadedFile {
    let path = PathBuf::from(format!("<fake:{name}>"));
    LoadedFile {
        plugin,
        source: path.clone(),
        staged: path,
        fingerprint: None,
    }
}

/// 事件的载荷（我们都是单实参 JSON）。
fn payload_of(call: &cordis_core::Call) -> Value {
    call.args
        .first()
        .and_then(|arg| arg.downcast::<ExternValue>())
        .map(|value| value.0.clone())
        .unwrap_or(Value::Null)
}

// ---------------------------------------------------------------- 夹具

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "cordis-test-{}-{}-{tag}",
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

fn config_for(path: &Path) -> LoaderConfig {
    LoaderConfig {
        path: path.to_string_lossy().into_owned(),
        patches: Vec::new(),
        plugin_dir: None,
        strict: true,
        strict_cycles: false,
        hmr: None,
        js_evaluator: None,
        interpolation: cordis_host::loader::InterpolationMode::Builtin,
    }
}

async fn load(app: &App, dir: &TempDir, yaml: &str, probe: Arc<Probe>) -> Arc<LoaderState> {
    let path = dir.write("cordis.yml", yaml);
    load_with_source(
        app.root.clone(),
        config_for(&path),
        Arc::new(FakeSource { probe }) as Arc<dyn PluginSource>,
    )
    .await
    .expect("装载")
}

async fn reload_now(state: &Arc<LoaderState>) -> ReconcileOutcome {
    match state.reload().await {
        ReloadOutcome::Reconciled(outcome) => outcome,
        other => panic!("期望 Reconciled，实际：{other:?}"),
    }
}

/// 插件里的那份引用（通过服务取回来——它跨 volatile 提交保持同一身份）。
fn reference(app: &App) -> Arc<VolatileRef> {
    app.root
        .get("snapshot")
        .and_then(|value| value.downcast::<VolatileRef>())
        .expect("snapshot 服务（VolatileRef）")
}

fn row(data: &str, note: &str) -> String {
    format!("- id: a\n  name: volatile\n  config:\n    data: {data}\n    note: {note}\n")
}

// ---------------------------------------------------------------- 六条判据

/// M4-5 核心：只差 volatile 字段的配置变更**原地提交**——不重挂、不发
/// `loader/partial-dispose`、引用身份不变而值更新，并发一条带路径的
/// `loader/volatile-update`。
#[tokio::test]
async fn volatile_only_change_commits_in_place() {
    let app = App::new();
    let dir = TempDir::new("volatile");
    let probe = Arc::new(Probe::default());
    let state = load(&app, &dir, &row("1", "keep"), probe.clone()).await;

    assert_eq!(probe.applies(), 1, "初次装载 apply 一次");
    let before = reference(&app);
    assert_eq!(before.get(), json!(1));
    let epoch_before = state.fiber_of("a").expect("实例").running_epoch();

    // 只改 volatile 路径（`data`）：`note` 一字不动。
    dir.write("cordis.yml", &row("2", "keep"));
    let outcome = reload_now(&state).await;

    assert_eq!(outcome.volatile, ["a"], "这次更新应当被记为 volatile 提交");
    assert!(outcome.updated.is_empty(), "不该走普通更新：{outcome:?}");
    assert_eq!(
        probe.applies(),
        1,
        "volatile 提交**不重挂**——apply 不许再跑"
    );
    let after = reference(&app);
    assert!(
        Arc::ptr_eq(&before, &after),
        "引用身份必须稳定（插件手里的 Arc 继续有效）"
    );
    assert_eq!(after.get(), json!(2), "同一个引用的值要变成新值");
    assert_eq!(
        state.fiber_of("a").expect("实例").running_epoch(),
        epoch_before,
        "实例的 epoch 不该变（没卸载重装）"
    );
    assert!(
        probe.partial_disposes.lock().unwrap().is_empty(),
        "volatile 路径不发 loader/partial-dispose：{:?}",
        probe.partial_disposes.lock().unwrap()
    );
    assert_eq!(
        probe.volatile_events.lock().unwrap().clone(),
        vec![json!(["data"])],
        "事件载荷就是**变化的路径**"
    );
}

/// M4-5：**普通值**参与的变化照旧走普通更新（重挂）——包括"volatile 与普通字段
/// 一起变"的形态（上游的候选比较会把这种情况交回普通更新）。
#[tokio::test]
async fn ordinary_change_still_restarts() {
    let app = App::new();
    let dir = TempDir::new("ordinary");
    let probe = Arc::new(Probe::default());
    let state = load(&app, &dir, &row("1", "keep"), probe.clone()).await;

    // ① 改普通字段：重挂。
    dir.write("cordis.yml", &row("1", "changed"));
    let outcome = reload_now(&state).await;
    assert_eq!(outcome.updated, ["a"], "普通更新走 update 桶：{outcome:?}");
    assert!(outcome.volatile.is_empty());
    assert_eq!(probe.applies(), 2, "普通更新必须重新 apply");
    assert_eq!(
        probe.partial_disposes.lock().unwrap().len(),
        1,
        "普通更新照旧发 partial-dispose"
    );
    assert_eq!(reference(&app).get(), json!(1), "data 没变，新实例读到旧值");

    // ② volatile 与普通字段一起变：也是普通更新（候选里"普通值"确实变了）。
    dir.write("cordis.yml", &row("3", "again"));
    let outcome = reload_now(&state).await;
    assert_eq!(outcome.updated, ["a"], "{outcome:?}");
    assert!(outcome.volatile.is_empty());
    assert_eq!(probe.applies(), 3);
    assert_eq!(reference(&app).get(), json!(3));
}

/// M4-5：**候选无效**（这里让插件的校验器拒绝新值）⇒ 保留运行值 + 记警告 + 记 `lagging`，
/// **不重挂**。raw 仍然推进（下次激活用新的）——上游 `_commitVolatile` 的同一处置。
#[tokio::test]
async fn invalid_candidate_keeps_running_values() {
    let app = App::new();
    let dir = TempDir::new("invalid");
    let probe = Arc::new(Probe::default());
    let logs: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = logs.clone();
    app.store
        .set_log_sink(Arc::new(move |_level, name, message| {
            sink.lock().unwrap().push(format!("{name}: {message}"));
        }));
    let state = load(&app, &dir, &row("1", "keep"), probe.clone()).await;

    // `data` 必须是数字；这次给字符串 ⇒ 候选解析失败。
    dir.write("cordis.yml", &row("\"nope\"", "keep"));
    let outcome = reload_now(&state).await;

    assert_eq!(
        outcome.lagging,
        ["a"],
        "无效候选进 lagging（实例落后于配置）：{outcome:?}"
    );
    assert!(outcome.volatile.is_empty());
    assert!(outcome.failures.is_empty(), "这不是装载失败：{outcome:?}");
    assert_eq!(probe.applies(), 1, "不重挂");
    assert_eq!(reference(&app).get(), json!(1), "运行值必须保留");
    assert!(
        logs.lock()
            .unwrap()
            .iter()
            .any(|line| line.contains("volatile") && line.contains("a")),
        "必须留下一条可诊断的警告：{:?}",
        logs.lock().unwrap()
    );
}

/// M4-5：`loader/volatile-update` 是 **owner-filter** 事件——只到这一行的 fiber 子树，
/// 外面的监听者收不到（上游 `entry.ts:188-191` 的动态 filter 同理）。
#[tokio::test]
async fn volatile_event_is_owner_filtered() {
    let app = App::new();
    let dir = TempDir::new("filter");
    let probe = Arc::new(Probe::default());

    // 行外的监听者（注册在 root 上）。
    let outside: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let keeper = {
        let outside = outside.clone();
        app.root.on("loader/volatile-update", move |call| {
            let outside = outside.clone();
            Box::pin(async move {
                outside.lock().unwrap().push(payload_of(&call));
                None
            })
        })
    };

    let state = load(&app, &dir, &row("1", "keep"), probe.clone()).await;
    dir.write("cordis.yml", &row("2", "keep"));
    let outcome = reload_now(&state).await;
    assert_eq!(outcome.volatile, ["a"]);

    assert_eq!(
        probe.volatile_events.lock().unwrap().clone(),
        vec![json!(["data"])],
        "行内监听者必须收到"
    );
    assert!(
        outside.lock().unwrap().is_empty(),
        "行外监听者不该收到：{:?}",
        outside.lock().unwrap()
    );
    drop(keeper);
}

/// M4-5：**没声明 volatile 的插件永远不走 volatile 路径**——哪怕配置里
/// 恰好有个叫 `data` 的字段、恰好只有它变了。声明才是开关（"显式声明"这条口径）。
#[tokio::test]
async fn plugin_without_declaration_never_goes_volatile() {
    let app = App::new();
    let dir = TempDir::new("undeclared");
    let probe = Arc::new(Probe::default());
    let yaml = "- id: a\n  name: plain\n  config:\n    data: 1\n    note: keep\n";
    let state = load(&app, &dir, yaml, probe.clone()).await;
    assert_eq!(probe.applies(), 1);

    dir.write(
        "cordis.yml",
        "- id: a\n  name: plain\n  config:\n    data: 2\n    note: keep\n",
    );
    let outcome = reload_now(&state).await;

    assert_eq!(outcome.updated, ["a"], "没声明就是普通更新：{outcome:?}");
    assert!(outcome.volatile.is_empty());
    assert_eq!(probe.applies(), 2, "必须重挂");
}

/// M4-5 前置条件：**上下文变了**（这里是行级 `isolate` 变更）⇒ volatile 被跳过，
/// 走 `Replace`（上游第三条前置条件 `getPrototypeOf(ctx) === parent.ctx` 的对应物：
/// 本仓用"身份与 `scope_inputs` 未变"来表达它，见 §8 M4-5）。
#[tokio::test]
async fn precondition_parent_ctx_mismatch_skips_volatile() {
    let app = App::new();
    let dir = TempDir::new("ctx-mismatch");
    let probe = Arc::new(Probe::default());
    let state = load(&app, &dir, &row("1", "keep"), probe.clone()).await;

    // ① 只差 volatile 路径、上下文没动 ⇒ 计划里就是 volatile 提交。
    let volatile_only = parse_entries(&row("2", "keep")).expect("parse");
    assert!(
        matches!(state.plan(&volatile_only).as_slice(), [Action::Volatile { label, .. }] if label == "a"),
        "期望 Action::Volatile，实际：{:?}",
        state.plan(&volatile_only)
    );

    // ② 同一份配置变更，但这一行的 `isolate` 也变了 ⇒ volatile 让位给 Replace。
    let with_isolate = parse_entries(concat!(
        "- id: a\n  name: volatile\n  isolate:\n    svc: true\n",
        "  config:\n    data: 2\n    note: keep\n",
    ))
    .expect("parse");
    assert_eq!(
        state.plan(&with_isolate),
        vec![Action::Replace("a".to_string())],
        "上下文变了就必须换实例，volatile 不适用"
    );
}
