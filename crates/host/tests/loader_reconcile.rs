//! M4-2 / M4-3：派生 `disabled`、运行期启停、配置监听与对账状态机。
//!
//! # M4-2（上游 `vendor/loader/src/config/entry.ts:76-95`）
//! - **派生 disabled**：`Entry.disabled` 是 getter——沿祖先链上溯，自身或任一祖先为真
//!   即禁用；**组行自己不挂载**（它只是容器），所以"组禁用"= 整棵子树被挡；
//! - **每次决策现算**：`!!js` 表达式在被读到时求值（`disabledOf`），**不缓存**，
//!   于是 `disabled: !!js "!ctx.get('profileContext')"` 会跟着上下文走；
//! - **运行期启停**：`loader.update(entry, { disabled })` → 该行（及子孙）dispose，
//!   取消禁用后重新 `init()`（`loader/tests/group.spec.ts:52-80`）。
//!
//! # M4-3（上游 `include/src/index.ts` + `loader/src/config/{entry,tree}.ts`）
//! - 文件是真相：轮询指纹 → **稳读**（两遍一致才采纳）→ 解析（含 patch 层）→ 对账；
//!   解析失败**保留上一快照**（编辑中的半截文件是常态，只记警告）；
//! - 对账 = "期望态 vs 现状"的动作表：新增/移除 ⇒ mount/dispose；`disabled` 翻转 ⇒
//!   dispose/remount；只改 `config` ⇒ `fiber.update(config, no_save=true)` 不重挂；
//!   `name`/`inject`/`isolate`/`intercept`（或祖先隔离域）变 ⇒ **replace**（正确性超集）；
//!   相等 ⇒ 空计划；
//! - 每个动作都从行的上下文上跑 `loader/patch-context` 瀑布（tail=动作本身），
//!   移除路径发 `loader/partial-dispose`；
//! - 只有最新的一份树会被应用（在飞期间来的树合并掉）；插件自我卸载 ⇒ 标 `disabled` + 回写。
//!
//! 这些测试全部**不需要动态库**：`PluginSource` seam（M4-1）用假插件顶替加载。

use cordis_core::{App, Config, Ctx, Disposable, Dyn, Fiber, FiberState, Inject, Plugin};
use cordis_host::loader::{
    declare_volatile, load_with_source, Action, HmrConfig, JsEvaluator, LoadedFile, LoaderConfig,
    LoaderState, PluginSource, ReconcileOutcome, ReloadOutcome,
};
use cordis_host::patch::{parse_entries, Entry};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Semaphore;

/// 假插件源：按行名分派假插件，**零动态库**跑完整条装载/对账链。
///
/// - `provides` / `provides-next`：把 `config.service`（默认 `svc`）注册成 `config.value`
///   的值，而且是**在 apply 时读配置**——所以"只 update 不重挂"这条路径可以被观察到
///   （值变了、fiber 没变）；
/// - `self-removing`：apply 时把自己这一行的 fiber 交出来，测试随后手动 dispose，
///   模拟"插件自己把自己卸载了"（上游 `loader/src/index.ts:137-145` 的 case 7）；
/// - `gate`：可选闸门。打开之后每一次挂载都要先拿到一个许可证，于是"对账进行到一半"
///   这一刻可以被钉成确定性的（`no_tearing` / `coalesces` 两条测试靠它）；
/// - `trace`：生命周期轨迹（apply / cleanup 的先后顺序就是"先卸后装"的证据）。
struct FakeSource {
    gate: Arc<Mutex<Option<Arc<Semaphore>>>>,
    stashed: Arc<Mutex<Vec<Arc<Fiber>>>>,
    trace: Arc<Mutex<Vec<String>>>,
}

impl FakeSource {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            gate: Arc::new(Mutex::new(None)),
            stashed: Arc::new(Mutex::new(Vec::new())),
            trace: Arc::new(Mutex::new(Vec::new())),
        })
    }

    /// 打开闸门：此后每次挂载都要先拿许可证（测试用 `add_permits` 放行）。
    fn open_gate(&self) -> Arc<Semaphore> {
        let gate = Arc::new(Semaphore::new(0));
        *self.gate.lock().unwrap() = Some(gate.clone());
        gate
    }

    /// 关闸门：此后**新**的挂载不再等待；已经在等的那几个仍由 `add_permits` 放行。
    fn close_gate(&self) {
        *self.gate.lock().unwrap() = None;
    }

    fn trace(&self) -> Vec<String> {
        self.trace.lock().unwrap().clone()
    }

    fn stashed_fiber(&self) -> Arc<Fiber> {
        self.stashed
            .lock()
            .unwrap()
            .pop()
            .expect("`self-removing` 插件应当把 fiber 交出来")
    }
}

impl PluginSource for FakeSource {
    fn load(
        &self,
        entry: &Entry,
        _search: &[PathBuf],
        _revision: u64,
    ) -> Result<LoadedFile, String> {
        // 轨迹里同时带上**行身份**与**插件名**：前者用来在"同一轮对账里有好几行"时区分
        // （合并测试），后者用来在"同一行换了个插件"时区分（换实例测试）。
        let tag = format!(
            "{}:{}",
            entry.id.clone().unwrap_or_else(|| entry.name.clone()),
            entry.name
        );
        let config = entry.config.clone().unwrap_or(Config::Null);
        let service = config
            .get("service")
            .and_then(Value::as_str)
            .unwrap_or("svc")
            .to_string();
        let gate = self.gate.lock().unwrap().clone();
        let trace = self.trace.clone();
        let stashed = self.stashed.clone();

        let plugin: Arc<dyn Plugin> = match entry.name.as_str() {
            "provides" | "provides-next" | "self-removing" => {
                let name: &'static str = match entry.name.as_str() {
                    "provides" => "provides",
                    "provides-next" => "provides-next",
                    _ => "self-removing",
                };
                // 「插件自我卸载」的模拟：这一行的插件把自己的 fiber 交给测试。
                let stash_self = entry.name == "self-removing";
                cordis_core::plugin_fn(name, Inject::default(), move |ctx: Ctx, cfg: Config| {
                    let gate = gate.clone();
                    let trace = trace.clone();
                    let stashed = stashed.clone();
                    let service = service.clone();
                    let tag = tag.clone();
                    Box::pin(async move {
                        // 配置在**每一次** apply 时重新读：`update`（不重挂）就是靠这一点被观察到的。
                        let value = cfg.get("value").and_then(Value::as_u64).unwrap_or(0) as u32;
                        trace.lock().unwrap().push(format!("{tag}:apply"));
                        let cleaned = trace.clone();
                        let cleaned_tag = tag.clone();
                        ctx.effect(move |_| {
                            Disposable::sync(move || {
                                cleaned
                                    .lock()
                                    .unwrap()
                                    .push(format!("{cleaned_tag}:cleanup"))
                            })
                        });
                        if stash_self {
                            stashed.lock().unwrap().push(ctx.fiber());
                        }
                        if let Some(gate) = gate {
                            let permit = gate.acquire().await.expect("闸门未关闭");
                            permit.forget();
                        }
                        ctx.provide(&service, Dyn::new(value))?;
                        Ok(())
                    })
                })
            }
            // M4-6：带 **volatile 声明**的插件（`loader.report.volatile` 要有非零值）。
            "declares" => {
                let service = entry
                    .config
                    .as_ref()
                    .and_then(|config| config.get("service"))
                    .and_then(Value::as_str)
                    .unwrap_or("svc")
                    .to_string();
                let plugin = declare_volatile(
                    cordis_core::plugin_fn(
                        "declares",
                        Inject::default(),
                        move |ctx: Ctx, cfg: Config| {
                            let service = service.clone();
                            Box::pin(async move {
                                let value =
                                    cfg.get("value").and_then(Value::as_u64).unwrap_or(0) as u32;
                                ctx.provide(&service, Dyn::new(value))?;
                                Ok(())
                            })
                        },
                    ),
                    &["data"],
                );
                plugin
            }
            // apply 阶段失败的插件：用来钉住 `mount_row` 的失败清理
            // （半成品 fiber 必须被 dispose，effect 的 disposer 必须跑过）。
            "fails" => {
                let trace = self.trace.clone();
                let tag = tag.clone();
                cordis_core::plugin_fn("fails", Inject::default(), move |ctx: Ctx, _cfg: Config| {
                    let trace = trace.clone();
                    let tag = tag.clone();
                    Box::pin(async move {
                        trace.lock().unwrap().push(format!("{tag}:apply"));
                        let cleaned = trace.clone();
                        let cleaned_tag = tag.clone();
                        ctx.effect(move |_| {
                            Disposable::sync(move || {
                                cleaned
                                    .lock()
                                    .unwrap()
                                    .push(format!("{cleaned_tag}:cleanup"))
                            })
                        });
                        Err("故意让 apply 失败".into())
                    })
                })
            }
            other => return Err(format!("假插件源不认识 \"{other}\"")),
        };
        let path = PathBuf::from(format!("<fake:{0}>", entry.name));
        Ok(LoadedFile {
            plugin,
            source: path.clone(),
            staged: path,
            // 假源没有真实文件指纹：`None` = "指纹未知"，轮询不会把它当成永远陈旧。
            fingerprint: None,
        })
    }
}

/// 只返回**真实文件路径**的假源：给 `reload_one` 的预检（stage/open/ping）路径用。
///
/// 首次装载不需要真的能打开库（插件对象直接给），但 `reload_one` 会从 `record.source`
/// 复制并 `LoadLibrary` —— 这正是"坏构建"要测的那一步。
struct FileSource {
    source: PathBuf,
    plugin: Arc<dyn Plugin>,
}

impl PluginSource for FileSource {
    fn load(
        &self,
        _entry: &Entry,
        _search: &[PathBuf],
        _revision: u64,
    ) -> Result<LoadedFile, String> {
        Ok(LoadedFile {
            plugin: self.plugin.clone(),
            source: self.source.clone(),
            staged: self.source.clone(),
            fingerprint: file_fingerprint(&self.source),
        })
    }
}

fn file_fingerprint(path: &Path) -> Option<(u64, u64)> {
    let metadata = std::fs::metadata(path).ok()?;
    let mtime = metadata
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos() as u64;
    Some((mtime, metadata.len()))
}

/// 剧本式求值器：把闭包装成 [`JsEvaluator`]（测试要控制"每次决策给出不同答案"）。
struct FnJs<F>(F);

impl<F> JsEvaluator for FnJs<F>
where
    F: Fn(&str, &Ctx) -> Result<Config, String> + Send + Sync,
{
    fn evaluate(&self, expr: &str, ctx: &Ctx) -> Result<Config, String> {
        (self.0)(expr, ctx)
    }
}

/// 每个用例一个临时目录（跨平台安全）。
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

    fn path(&self) -> &Path {
        &self.0
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

fn config_for(path: &Path, evaluator: Option<Arc<dyn JsEvaluator>>) -> LoaderConfig {
    LoaderConfig {
        path: path.to_string_lossy().into_owned(),
        patches: Vec::new(),
        plugin_dir: None,
        strict: true,
        strict_cycles: false,
        hmr: None,
        js_evaluator: evaluator,
        interpolation: cordis_host::loader::InterpolationMode::Builtin,
        dispatch_policy: None,
    }
}

/// 装一份配置（strict）；返回装载器的运行时状态（对账/启停都要用它）。
async fn load(
    app: &App,
    dir: &TempDir,
    name: &str,
    yaml: &str,
    patches: &[&str],
    source: Arc<FakeSource>,
    evaluator: Option<Arc<dyn JsEvaluator>>,
) -> Result<Arc<LoaderState>, String> {
    let path = dir.write(name, yaml);
    let mut config = config_for(&path, evaluator);
    config.patches = patches
        .iter()
        .map(|patch| dir.path().join(patch).to_string_lossy().into_owned())
        .collect();
    load_with_source(app.root.clone(), config, source)
        .await
        .map_err(|err| err.to_string())
}

/// 一次"文件 → 对账"，并断言它由本次调用驱动完成。
async fn reload_now(state: &Arc<LoaderState>) -> ReconcileOutcome {
    match state.reload().await {
        ReloadOutcome::Reconciled(outcome) => outcome,
        other => panic!("期望 Reconciled，实际：{other:?}"),
    }
}

fn tree(yaml: &str) -> Vec<Entry> {
    parse_entries(yaml).expect("parse")
}

/// 一行的 YAML：`id` / 插件名 / 服务名 / 值。
fn yaml_row(label: &str, plugin: &str, service: &str, value: u32) -> String {
    format!(
        "- id: {label}\n  name: {plugin}\n  config:\n    service: {service}\n    value: {value}\n"
    )
}

fn service_of(app: &App, name: &str) -> Option<u32> {
    app.root
        .get(name)
        .and_then(|value| value.downcast::<u32>())
        .map(|value| *value)
}

/// `loader.report` 服务（M4-6 的诊断面；`require` 走的是 committed 视图，
/// 而 root fiber 的 committed 是空表 ⇒ 直接读全局表里的**当前**值）。
fn report(app: &App) -> Value {
    app.root
        .require("loader.report")
        .expect("loader.report")
        .downcast::<cordis_core::ExternValue>()
        .expect("ExternValue")
        .0
        .clone()
}

fn capture_logs(app: &App) -> Arc<Mutex<Vec<String>>> {
    let logs = Arc::new(Mutex::new(Vec::new()));
    let sink = logs.clone();
    app.store
        .set_log_sink(Arc::new(move |level, name, message| {
            sink.lock()
                .unwrap()
                .push(format!("{level:?} {name}: {message}"));
        }));
    logs
}

/// 轮询等待（上限约 2s）：把"某个异步效果出现了"变成确定性断言。
async fn wait_until(what: &str, mut check: impl FnMut() -> bool) {
    for _ in 0..200 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    panic!("等待超时：{what}");
}

// ---------------------------------------------------------------------------
// M4-2：派生 disabled + 运行期启停
// ---------------------------------------------------------------------------

/// M4-2（红→绿）：**组上的 `disabled` 沿祖先链派生到子行**，而且**运行期可往返**。
///
/// 前半（装载时就禁用 ⇒ 子行不挂）此前已成立——它是守卫，钉住"组禁用 = 整棵子树"。
/// 后半是缺陷所在：装载器此前没有启停通道（`load()` 一跑完，条目树就成了死数据），
/// 于是"取消禁用 ⇒ 子行回来"在任何版本里都做不到。
#[tokio::test]
async fn group_disabled_propagates_to_children_and_back() {
    let app = App::new();
    let dir = TempDir::new("group-disabled");

    let state = load(
        &app,
        &dir,
        "cordis.yml",
        concat!(
            "- id: grp\n",
            "  name: grp\n",
            "  group: true\n",
            "  disabled: true\n",
            "  config:\n",
            "    - id: provider\n",
            "      name: provides\n",
            "      config:\n",
            "        value: 3\n",
        ),
        &[],
        FakeSource::new(),
        None,
    )
    .await
    .expect("禁用组不该让装载失败");

    // 派生：组被禁用 ⇒ 子行不挂（守卫：这一半本来就对）。
    assert!(service_of(&app, "svc").is_none(), "禁用组里的行不得挂载");
    assert!(state.mounted_labels().is_empty(), "装载表必须是空的");

    // 运行期启用：子行必须上线。
    let outcome = state
        .set_disabled("grp", Some(false))
        .await
        .expect("启用组");
    assert_eq!(service_of(&app, "svc"), Some(3), "启用组后子行必须上线");
    assert_eq!(state.mounted_labels(), ["provider"]);
    assert_eq!(outcome.mounted, ["provider"], "启停必须报告它挂了哪一行");

    // 运行期禁用：子行必须被**卸载**（不是留在线上）。
    let outcome = state.set_disabled("grp", Some(true)).await.expect("禁用组");
    assert!(service_of(&app, "svc").is_none(), "禁用组后子行必须被卸下");
    assert!(state.mounted_labels().is_empty(), "装载表不得留下幽灵记录");
    assert_eq!(outcome.disposed, ["provider"]);

    // 「再回来」：清掉禁用位（上游 `disabled: null`）⇒ 子行重新上线。
    state.set_disabled("grp", None).await.expect("清掉禁用位");
    assert_eq!(service_of(&app, "svc"), Some(3), "取消禁用后子行必须回来");
    assert_eq!(state.mounted_labels(), ["provider"]);
}

/// M4-2（红→绿）：`disabled: !!js <表达式>` 在**每一次挂载决策**时重新求值。
///
/// 上游 `Entry.disabled` 是 getter：`entry.ts:76-95` 每次读都走 `disabledOf` →
/// `evaluate(...)`。缓存它会让一行永远停在第一次的答案上，而 `!!js` 的用处恰恰是
/// "跟着上下文变"（出厂 bundle 的 `!!js "!ctx.get('profileContext')"` 就是）。
///
/// 现状（红）：M1-4 的处置是"任何 `!!js` 一律 fail loud"，于是这一行根本装不上；
/// 求值器挂点（`LoaderConfig.js_evaluator`）与"每次决策现算"都是 M4-2 才有的。
#[tokio::test]
async fn disabled_js_expr_reevaluated_per_mount_decision() {
    let app = App::new();
    let dir = TempDir::new("js-disabled");

    // 求值器：按调用次序回答 false → true → false，并记录每次都求了哪个表达式。
    let calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let answers = Arc::new(Mutex::new(VecDeque::from(vec![
        Value::Bool(false),
        Value::Bool(true),
        Value::Bool(false),
    ])));
    let evaluator = Arc::new(FnJs({
        let calls = calls.clone();
        let answers = answers.clone();
        move |expr: &str, _ctx: &Ctx| {
            calls.lock().unwrap().push(expr.to_string());
            answers
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| "剧本用尽（多求值了一次？）".to_string())
        }
    }));

    let state = load(
        &app,
        &dir,
        "cordis.yml",
        concat!(
            "- id: gated\n",
            "  name: provides\n",
            "  disabled: !!js feature.gate\n",
            "  config:\n",
            "    value: 5\n",
        ),
        &[],
        FakeSource::new(),
        Some(evaluator),
    )
    .await
    .expect("有求值器时表达式行必须能装载");

    // 第一次决策：答案 = false ⇒ 行挂上。
    assert_eq!(service_of(&app, "svc"), Some(5), "表达式答假 ⇒ 行该挂");
    assert_eq!(
        calls.lock().unwrap().clone(),
        ["feature.gate"],
        "首次决策求值一次"
    );

    // 第二次决策：答案 = true ⇒ 行必须被卸下。
    state.sync().await;
    assert!(
        service_of(&app, "svc").is_none(),
        "表达式答真 ⇒ 行必须被卸下"
    );
    assert_eq!(
        calls.lock().unwrap().len(),
        2,
        "第二次决策必须**重新**求值，而不是复用第一次的答案"
    );

    // 第三次决策：答案 = false ⇒ 行必须回来（现算，绝不缓存）。
    state.sync().await;
    assert_eq!(
        service_of(&app, "svc"),
        Some(5),
        "表达式又答假 ⇒ 行必须回来"
    );
    assert_eq!(
        calls.lock().unwrap().clone(),
        ["feature.gate", "feature.gate", "feature.gate"],
        "每次决策都用自己的答案"
    );
}

// ---------------------------------------------------------------------------
// M4-3：配置监听与对账状态机
// ---------------------------------------------------------------------------

/// M4-3（红→绿）：**只改 `config` 的变更走 `fiber.update`，不重挂**。
///
/// 上游 `entry.ts:125-131`：diff 里含 `config` 时走 `_patchContext` → `fiber.update(config, true)`
/// —— fiber 本身不动（uid 不变），只是重跑一遍 runtime。
/// 现状（红）：改配置文件**没有任何反应**（`load()` 之后没有"再读一次配置"的入口）。
#[tokio::test]
async fn config_change_updates_without_remount() {
    let app = App::new();
    let dir = TempDir::new("update-in-place");
    let source = FakeSource::new();
    let state = load(
        &app,
        &dir,
        "cordis.yml",
        &yaml_row("provider", "provides", "svc", 1),
        &[],
        source.clone(),
        None,
    )
    .await
    .expect("装载");
    let before = state.fiber_of("provider").expect("第一版应当挂上");
    assert_eq!(service_of(&app, "svc"), Some(1));

    dir.write("cordis.yml", &yaml_row("provider", "provides", "svc", 2));
    let outcome = reload_now(&state).await;

    assert_eq!(
        outcome.updated,
        ["provider"],
        "只改 config ⇒ 只 update：{outcome:?}"
    );
    assert!(
        outcome.mounted.is_empty() && outcome.disposed.is_empty() && outcome.replaced.is_empty(),
        "不得重挂：{outcome:?}"
    );
    let after = state.fiber_of("provider").expect("更新后仍在服务");
    assert!(Arc::ptr_eq(&before, &after), "update 不换 fiber（无重挂）");
    assert_eq!(before.uid(), after.uid(), "uid 不变 ⇒ 依赖者不会被动重载");
    assert_eq!(service_of(&app, "svc"), Some(2), "新配置必须真的生效");
    assert_eq!(
        source
            .trace()
            .iter()
            .filter(|line| line.ends_with(":apply"))
            .count(),
        2,
        "update = 重启 runtime（apply 再跑一次），轨迹：{:?}",
        source.trace()
    );
}

/// M4-3（红→绿）：**文件驱动的启停**——`disabled` 在配置里翻转 ⇒ 卸下 / 重新挂上。
///
/// 与 M4-2 的 API 驱动（`set_disabled`）是两条入口、同一套对账。
#[tokio::test]
async fn disabled_toggle_disposes_and_remounts() {
    let app = App::new();
    let dir = TempDir::new("file-disabled");
    let source = FakeSource::new();
    let enabled = yaml_row("provider", "provides", "svc", 4);
    let disabled = format!("{enabled}  disabled: true\n");
    let state = load(
        &app,
        &dir,
        "cordis.yml",
        &enabled,
        &[],
        source.clone(),
        None,
    )
    .await
    .expect("装载");
    assert_eq!(service_of(&app, "svc"), Some(4));

    // 写 disabled: true ⇒ 卸下，并且**不留幽灵记录**。
    dir.write("cordis.yml", &disabled);
    let outcome = reload_now(&state).await;
    assert_eq!(outcome.disposed, ["provider"], "{outcome:?}");
    assert!(service_of(&app, "svc").is_none(), "禁用后实例必须被卸下");
    assert!(state.mounted_labels().is_empty());
    assert_eq!(
        source
            .trace()
            .iter()
            .filter(|line| line.ends_with(":cleanup"))
            .count(),
        1,
        "清理逻辑必须跑过：{:?}",
        source.trace()
    );

    // 写回 enabled ⇒ 重新挂上。
    dir.write("cordis.yml", &enabled);
    let outcome = reload_now(&state).await;
    assert_eq!(outcome.mounted, ["provider"], "{outcome:?}");
    assert_eq!(service_of(&app, "svc"), Some(4), "取消禁用后必须回来");
}

/// M4-3（红→绿）：patch 层插入 / 移除行 ⇒ 挂上 / 卸下（patch 是"用户层改动"的默认落点）。
#[tokio::test]
async fn row_add_and_remove_with_patch() {
    let app = App::new();
    let dir = TempDir::new("patch-rows");
    let source = FakeSource::new();
    dir.write("cordis.patch.yml", "# 暂时没有 patch\n");
    let state = load(
        &app,
        &dir,
        "cordis.yml",
        &yaml_row("provider", "provides", "svc", 1),
        &["cordis.patch.yml"],
        source.clone(),
        None,
    )
    .await
    .expect("装载");
    assert_eq!(state.mounted_labels(), ["provider"]);

    // patch 里 insert 一行（`insert` 不带 id ⇒ 追加到根）。
    dir.write(
        "cordis.patch.yml",
        concat!(
            "# 用户层：追加一行\n",
            "- insert:\n",
            "    - id: extra\n",
            "      name: provides\n",
            "      config:\n",
            "        service: extra\n",
            "        value: 7\n",
        ),
    );
    let outcome = reload_now(&state).await;
    assert_eq!(outcome.mounted, ["extra"], "{outcome:?}");
    assert_eq!(service_of(&app, "extra"), Some(7), "新行必须真的上线");
    assert_eq!(state.mounted_labels(), ["extra", "provider"]);

    // 去掉这一行 ⇒ 卸下，且其它行不受影响。
    dir.write("cordis.patch.yml", "# 又拿掉了\n");
    let outcome = reload_now(&state).await;
    assert_eq!(outcome.disposed, ["extra"], "{outcome:?}");
    assert!(service_of(&app, "extra").is_none());
    assert_eq!(state.mounted_labels(), ["provider"], "邻居不该被牵连");
    assert_eq!(service_of(&app, "svc"), Some(1));
}

/// M4-3（红→绿）：**相等 ⇒ 空计划**（对账不是"每次都重挂"）。
#[tokio::test]
async fn equal_config_is_noop() {
    let app = App::new();
    let dir = TempDir::new("equal-noop");
    let source = FakeSource::new();
    let yaml = yaml_row("provider", "provides", "svc", 1);
    let state = load(&app, &dir, "cordis.yml", &yaml, &[], source.clone(), None)
        .await
        .expect("装载");
    let before = state.fiber_of("provider").expect("挂上了");

    assert!(
        state.plan(&state.tree_snapshot()).is_empty(),
        "现状 vs 当前树必须没有差异"
    );

    let outcome = reload_now(&state).await;
    assert!(outcome.is_noop(), "没改文件 ⇒ 空对账：{outcome:?}");
    let after = state.fiber_of("provider").expect("还在服务");
    assert!(Arc::ptr_eq(&before, &after) && before.uid() == after.uid());
    assert_eq!(
        source
            .trace()
            .iter()
            .filter(|line| line.ends_with(":apply"))
            .count(),
        1,
        "相等不该重启：{:?}",
        source.trace()
    );
}

/// M4-3（红→绿）：**插件自我卸载** ⇒ 树侧标 `disabled`（并**落盘**）+ 摘掉装载表记录。
///
/// 不落盘的话，下一次读取配置它又活了——用户看到的现象是"我明明卸掉了它，重启后又回来"。
/// 上游 `loader/src/index.ts:137-145`（case 7）；vendor 同样在 `internal/plugin` 里做这件事。
#[tokio::test]
async fn self_dispose_marks_entry_disabled_and_persists() {
    let app = App::new();
    let dir = TempDir::new("self-dispose");
    let source = FakeSource::new();
    let yaml = yaml_row("suicidal", "self-removing", "svc", 5);
    let config_path = dir.write("cordis.yml", &yaml);
    let state = load(&app, &dir, "cordis.yml", &yaml, &[], source.clone(), None)
        .await
        .expect("装载");
    assert_eq!(service_of(&app, "svc"), Some(5));

    // 插件自己把自己卸载了：**不是**装载器下的手（没有 set_disabled 调用）。
    source.stashed_fiber().dispose().await;

    assert!(service_of(&app, "svc").is_none(), "实例已经拆掉");
    assert!(state.mounted_labels().is_empty(), "装载表不得留下幽灵记录");
    let snapshot = state.tree_snapshot();
    assert!(snapshot[0].disabled, "树侧必须标上 disabled");

    let text = std::fs::read_to_string(&config_path).expect("读回配置");
    assert!(
        text.contains("disabled: true"),
        "必须落盘，否则重启后它又活了：\n{text}"
    );

    // 再读一次文件：仍然是禁用的（不会偷偷复活）。
    let outcome = reload_now(&state).await;
    assert!(outcome.is_noop(), "文件与树已经一致：{outcome:?}");
    assert!(service_of(&app, "svc").is_none(), "自我卸载的行不得复活");
}

/// M4-3（红→绿）：在飞期间来的树**被合并掉**——只有最新的一份会被应用。
///
/// 上游 `include/src/index.ts:98-100`：`_pendingTree` 只保存最新的一份意图，旧的直接跳过。
/// 这里用闸门把第一轮对账卡在半路，然后塞进去两份树：最终只应看到最后一份的痕迹。
#[tokio::test]
async fn reconcile_coalesces_inflight_changes() {
    let app = App::new();
    let dir = TempDir::new("coalesce");
    let source = FakeSource::new();
    let state = load(
        &app,
        &dir,
        "cordis.yml",
        &yaml_row("base", "provides", "base", 1),
        &[],
        source.clone(),
        None,
    )
    .await
    .expect("装载");
    assert_eq!(state.mounted_labels(), ["base"]);

    // 第一轮：加一行 `blocked` —— 它的 apply 会卡在闸门上（这就是"在飞"）。
    let blocking = format!(
        "{}{}",
        yaml_row("base", "provides", "base", 1),
        yaml_row("blocked", "provides", "blocked", 1)
    );
    let gate = source.open_gate();
    let first = {
        let state = state.clone();
        let entries = tree(&blocking);
        tokio::spawn(async move { state.request_reconcile(entries).await })
    };
    wait_until("第一轮卡在闸门上", || {
        source
            .trace()
            .iter()
            .any(|line| line.contains("blocked:") && line.ends_with(":apply"))
    })
    .await;

    // 在飞期间塞两份新树：第二份（c）会覆盖第一份（b）⇒ b **永远不该被应用**。
    let second = format!("{}{}", blocking, yaml_row("b", "provides", "b", 1));
    let third = format!("{}{}", blocking, yaml_row("c", "provides", "c", 1));
    assert!(
        state.request_reconcile(tree(&second)).await.is_none(),
        "在飞期间的请求应当被合并（返回 None）"
    );
    assert!(state.request_reconcile(tree(&third)).await.is_none());

    // 放行第一轮（并关上闸门：后续那一轮的新行不该再等），然后等它把两份新树中最新的那份应用掉。
    gate.add_permits(1);
    source.close_gate();
    let first = tokio::time::timeout(Duration::from_secs(5), first)
        .await
        .expect("第一轮任务超时（对账没有收敛）")
        .expect("第一轮任务");
    assert!(first.is_some(), "第一轮应当真的执行了对账");

    assert!(service_of(&app, "b").is_none(), "被覆盖的那份树不得被应用");
    assert_eq!(service_of(&app, "c"), Some(1), "最后一份树必须被应用");
    assert_eq!(state.mounted_labels(), ["base", "blocked", "c"]);
    // 装载表也不该记得那份被覆盖的树：最后一次对账的产物里只应有 c。
    let last = state.last_outcome().expect("最近一次对账产物");
    assert_eq!(last.mounted, ["c"], "只有最后一份树被应用：{last:?}");
    assert!(last.failures.is_empty(), "{last:?}");
    assert!(
        state.plan(&state.tree_snapshot()).is_empty(),
        "对账收敛后计划必须为空"
    );
}

/// 取消一轮正在等 `mutation` 的对账后，单飞位必须复位；下一次请求要真的执行，
/// 不能永久返回 `None`（“已合并”）而把 `pending` 留在表里。
#[tokio::test]
async fn cancelled_reconcile_does_not_stick_the_single_flight_flag() {
    let app = App::new();
    let dir = TempDir::new("cancel-reconcile");
    let source = FakeSource::new();
    let base = yaml_row("base", "provides", "base", 1);
    let blocked_disabled = concat!(
        "- id: blocked\n",
        "  name: provides\n",
        "  disabled: true\n",
        "  config:\n",
        "    service: blocked\n",
        "    value: 1\n",
    );
    let initial = format!("{base}{blocked_disabled}");
    let state = load(
        &app,
        &dir,
        "cordis.yml",
        &initial,
        &[],
        source.clone(),
        None,
    )
    .await
    .expect("装载");
    assert_eq!(state.mounted_labels(), ["base"]);

    // 让 set_disabled 持有 mutation 闸门，并卡在 blocked 的 apply 闸门上。
    let gate = source.open_gate();
    let enabling = {
        let state = state.clone();
        tokio::spawn(async move { state.set_disabled("blocked", Some(false)).await })
    };
    wait_until("blocked 的 apply 已经进入闸门", || {
        source
            .trace()
            .iter()
            .any(|line| line.contains("blocked:") && line.ends_with(":apply"))
    })
    .await;

    // 这一轮对账会先置 reconciling，再等 mutation；timeout 丢弃 future。
    let blocked_enabled = concat!(
        "- id: blocked\n",
        "  name: provides\n",
        "  config:\n",
        "    service: blocked\n",
        "    value: 1\n",
    );
    let target = format!(
        "{base}{blocked_enabled}{}",
        yaml_row("after", "provides", "after", 1)
    );
    let cancelled = tokio::time::timeout(
        Duration::from_millis(100),
        state.request_reconcile(tree(&target)),
    )
    .await;
    assert!(
        cancelled.is_err(),
        "第一轮对账应当卡在 mutation 闸门上，才能确定它被取消"
    );

    // 放行 set_disabled；取消的那一轮不会再回来，下一次请求必须接管。
    source.close_gate();
    gate.add_permits(1);
    tokio::time::timeout(Duration::from_secs(2), enabling)
        .await
        .expect("set_disabled 必须完成")
        .expect("set_disabled 任务不得 panic")
        .expect("blocked 行应当成功启用");

    let outcome = tokio::time::timeout(
        Duration::from_secs(2),
        state.request_reconcile(tree(&target)),
    )
    .await
    .expect("取消后再次请求不得挂起")
    .expect("取消后再次请求必须真正驱动对账，而不是永久 Coalesced");
    assert!(
        outcome.mounted.contains(&"after".to_string()),
        "第二轮必须执行：{outcome:?}"
    );
    assert_eq!(service_of(&app, "after"), Some(1));
    assert_eq!(state.mounted_labels(), ["after", "base", "blocked"]);
}

/// M4-3（红→绿）：**执行 `plan(A, B)` 之后的装载表 == 以 B 冷启动的装载表**。
///
/// 这是对账的收敛性回归（v4 把它从"归一化后等价"改成了可判定的**状态等价**断言）：
/// 直接执行差异，与"从头按目标态装载"必须落到同一个状态。
#[tokio::test]
async fn plan_reaches_fresh_load_state() {
    let yaml_a = format!(
        "{}{}",
        yaml_row("provider", "provides", "svc", 1),
        yaml_row("extra", "provides", "extra", 1)
    );
    let yaml_b = format!(
        "{}{}",
        yaml_row("provider", "provides", "svc", 2),
        yaml_row("third", "provides", "third", 3)
    );

    let dir_a = TempDir::new("plan-a");
    let dir_b = TempDir::new("plan-b");
    let app_a = App::new();
    let app_b = App::new();
    let state_a = load(
        &app_a,
        &dir_a,
        "cordis.yml",
        &yaml_a,
        &[],
        FakeSource::new(),
        None,
    )
    .await
    .expect("装载 A");
    let state_b = load(
        &app_b,
        &dir_b,
        "cordis.yml",
        &yaml_b,
        &[],
        FakeSource::new(),
        None,
    )
    .await
    .expect("装载 B");

    // 计划是**纯函数**：先把 A→B 的动作列出来，再执行。
    let plan = state_a.plan(&tree(&yaml_b));
    assert!(
        plan.contains(&Action::Dispose("extra".into())),
        "被移除的行要卸：{plan:?}"
    );
    assert!(
        plan.contains(&Action::Mount("third".into())),
        "新行要挂：{plan:?}"
    );
    assert!(
        plan.contains(&Action::Update {
            label: "provider".into(),
            config: json!({ "service": "svc", "value": 2 }),
        }),
        "只改配置的行要 update：{plan:?}"
    );

    state_a.reconcile_once(tree(&yaml_b)).await;

    // 状态等价：装载表 + 可观测的服务值都必须一致。
    assert_eq!(
        state_a.mounted_labels(),
        state_b.mounted_labels(),
        "装载表必须与冷启动一致"
    );
    assert_eq!(service_of(&app_a, "svc"), service_of(&app_b, "svc"));
    assert_eq!(service_of(&app_a, "third"), service_of(&app_b, "third"));
    assert!(service_of(&app_a, "extra").is_none() && service_of(&app_b, "extra").is_none());
    assert!(
        state_a.plan(&state_a.tree_snapshot()).is_empty(),
        "执行后计划必须收敛为空"
    );
}

/// M4-3（红→绿）：对账的每一刻都**没有撕裂状态**。
///
/// 用闸门把一次"换实例"卡在半路（旧实例已卸、新实例的 apply 还没跑完），在整段过渡里采样：
/// 1. 装载表里的记录**从不**指向已 dispose 的实例（表不撒谎）；
/// 2. 同一服务名**从不**有两代并存（内核会拒绝第二个提供者，所以真并存会以失败暴露）；
/// 3. 半路那一刻旧实例必须已经拆掉（先卸后装的定义性质）。
#[tokio::test]
async fn no_tearing_via_controllable_await_points() {
    let app = App::new();
    let dir = TempDir::new("no-tearing");
    let source = FakeSource::new();
    let state = load(
        &app,
        &dir,
        "cordis.yml",
        &yaml_row("provider", "provides", "svc", 1),
        &[],
        source.clone(),
        None,
    )
    .await
    .expect("装载");
    assert_eq!(service_of(&app, "svc"), Some(1));

    // 换成另一个插件名 ⇒ **replace**（先卸后装）；新实例的 apply 会卡在闸门上。
    let gate = source.open_gate();
    let finished = Arc::new(AtomicBool::new(false));
    let reconciler = {
        let state = state.clone();
        let finished = finished.clone();
        let entries = tree(&yaml_row("provider", "provides-next", "svc", 2));
        tokio::spawn(async move {
            let outcome = state.reconcile_once(entries).await;
            finished.store(true, Ordering::SeqCst);
            outcome
        })
    };
    wait_until("新实例的 apply 已经进入", || {
        source
            .trace()
            .iter()
            .any(|line| line.contains(":provides-next:apply"))
    })
    .await;

    // 半路：旧实例已经卸掉、新实例还没挂上 ⇒ 表与实例都应当是"不在"。
    assert!(
        service_of(&app, "svc").is_none(),
        "这一刻不该有实例在服务（先卸后装）"
    );
    assert!(state.mounted_labels().is_empty(), "表不该声称这一行还在");
    assert!(
        state.mounted_liveness().iter().all(|(_, alive)| *alive),
        "表里的记录不得指向已拆的实例：{:?}",
        state.mounted_liveness()
    );

    // 放行，并在整段过渡里持续采样。
    gate.add_permits(1);
    let mut samples = Vec::new();
    while !finished.load(Ordering::SeqCst) {
        for (label, alive) in state.mounted_liveness() {
            assert!(
                alive,
                "装载表的记录指向了已 dispose 的实例：{label}（采样：{samples:?}）"
            );
        }
        samples.push((state.mounted_labels(), service_of(&app, "svc")));
        tokio::task::yield_now().await;
    }

    let outcome = tokio::time::timeout(Duration::from_secs(5), reconciler)
        .await
        .expect("对账任务超时")
        .expect("对账任务");
    assert!(
        outcome.failures.is_empty(),
        "换实例不该失败（并存会被内核拒绝）：{outcome:?}"
    );
    assert_eq!(outcome.replaced, ["provider"], "{outcome:?}");
    assert_eq!(service_of(&app, "svc"), Some(2), "最终必须是新实例");
    assert_eq!(state.mounted_labels(), ["provider"]);
    assert!(
        state.mounted_liveness().iter().all(|(_, alive)| *alive),
        "收尾后记录也必须指向活实例"
    );

    // 轨迹：旧实例的清理必须在新实例的 apply 之前；新实例的 apply 只跑过一次。
    let trace = source.trace();
    let cleanup = trace
        .iter()
        .position(|line| line.ends_with(":provides:cleanup"))
        .expect("旧实例的清理");
    let applied = trace
        .iter()
        .position(|line| line.ends_with(":provides-next:apply"))
        .expect("新实例的 apply");
    assert!(cleanup < applied, "必须先卸后装：{trace:?}");
    assert_eq!(
        trace
            .iter()
            .filter(|line| line.ends_with(":provides-next:apply"))
            .count(),
        1,
        "不得重复挂载：{trace:?}"
    );
}

/// M4-3（红→绿）：**改掉 `disabled` 表达式本身也算配置变更**——下一轮对账要重新求值。
///
/// 求值器按**表达式文本**作答（`gate.on` 为真、`gate.off` 为假），于是 patch 换掉表达式
/// 就等于换掉了这一行的开关，而且完全不需要 M4-4a 的内置子集。
#[tokio::test]
async fn disabled_expression_change_triggers_reconcile() {
    let app = App::new();
    let dir = TempDir::new("expr-change");
    let source = FakeSource::new();
    let evaluator: Arc<dyn JsEvaluator> = Arc::new(FnJs(|expr: &str, _ctx: &Ctx| {
        Ok(Value::Bool(expr == "gate.on"))
    }));
    dir.write("cordis.patch.yml", "# 暂时没有 patch\n");
    let state = load(
        &app,
        &dir,
        "cordis.yml",
        concat!(
            "- id: gated\n",
            "  name: provides\n",
            "  disabled: !!js gate.off\n",
            "  config:\n",
            "    service: svc\n",
            "    value: 6\n",
        ),
        &["cordis.patch.yml"],
        source.clone(),
        Some(evaluator),
    )
    .await
    .expect("装载");
    assert_eq!(service_of(&app, "svc"), Some(6), "表达式答假 ⇒ 应当挂上");

    // patch 换成会答真的那一个 ⇒ 卸下。
    dir.write(
        "cordis.patch.yml",
        "- id: gated\n  disabled: !!js gate.on\n",
    );
    let outcome = reload_now(&state).await;
    assert_eq!(outcome.disposed, ["gated"], "{outcome:?}");
    assert!(service_of(&app, "svc").is_none(), "表达式翻真 ⇒ 必须卸下");

    // 再换回去 ⇒ 重新挂上（对账是双向的）。
    dir.write(
        "cordis.patch.yml",
        "- id: gated\n  disabled: !!js gate.off\n",
    );
    let outcome = reload_now(&state).await;
    assert_eq!(outcome.mounted, ["gated"], "{outcome:?}");
    assert_eq!(service_of(&app, "svc"), Some(6), "表达式翻假 ⇒ 必须回来");
}

/// M4-3（红→绿）：**解析失败保留上一快照**——半截文件绝不能让正在跑的行被拆掉。
///
/// 上游 `include/src/index.ts:206-215`：读/解析失败只记警告，状态一个字节都不动。
/// 本测试走两条路：直接 `reload()`（确定性）+ watcher 那一侧的警告日志（真实时序）。
#[tokio::test]
async fn parse_failure_keeps_previous_snapshot() {
    let app = App::new();
    let dir = TempDir::new("parse-failure");
    let logs = capture_logs(&app);
    let source = FakeSource::new();
    let path = dir.write("cordis.yml", &yaml_row("provider", "provides", "svc", 1));
    let mut config = config_for(&path, None);
    // 打开轮询（50ms）：既验证 watcher 真的会看配置文件，也让稳读窗口取到 12ms（测试够快）。
    config.hmr = Some(HmrConfig { interval_ms: 50 });
    let state = load_with_source(app.root.clone(), config, source.clone())
        .await
        .expect("装载");
    assert_eq!(service_of(&app, "svc"), Some(1));

    // 半截文件（用户保存到一半）+ 语法坏掉
    dir.write(
        "cordis.yml",
        "- id: provider\n  name: provides\n   bad-indent: [\n",
    );
    match state.reload().await {
        ReloadOutcome::Failed(err) => assert!(!err.is_empty(), "失败必须带原因：{err}"),
        other => panic!("坏文件必须报告失败，实际：{other:?}"),
    }
    assert_eq!(
        service_of(&app, "svc"),
        Some(1),
        "坏文件不得把正在跑的行拆掉"
    );
    assert_eq!(state.mounted_labels(), ["provider"], "装载表必须原样");
    assert_eq!(
        state.tree_snapshot()[0].config.clone().unwrap()["value"],
        json!(1),
        "树必须保留上一份"
    );

    // 修好文件 ⇒ 正常采纳。
    //
    // 注意这一行与 50ms 的 watcher **竞争**：它可能抢先一步把变更应用掉，于是我们这一步
    // "无动作"。真正的判据是**结果**（服务值必须变成 3）——把断言绑在"谁先看到"上
    // 就是一条会偶发失败的测试。
    dir.write("cordis.yml", &yaml_row("provider", "provides", "svc", 3));
    let outcome = reload_now(&state).await;
    assert!(
        outcome.updated == ["provider"] || outcome.is_noop(),
        "要么本次对账推进、要么 watcher 已经推进过：{outcome:?}"
    );
    assert_eq!(service_of(&app, "svc"), Some(3), "修好后必须采纳");

    // watcher 那一侧：解析失败只记**警告**，且状态照旧（不发错误、不卸行）
    dir.write(
        "cordis.yml",
        "- id: provider\n  name: provides\n  broken: [\n",
    );
    wait_until("watcher 报出保留快照的警告", || {
        logs.lock()
            .unwrap()
            .iter()
            .any(|line| line.starts_with("Warn ") && line.contains("未采纳"))
    })
    .await;
    assert_eq!(service_of(&app, "svc"), Some(3), "警告之后行仍必须在服务");
    assert_eq!(state.mounted_labels(), ["provider"]);
}

/// M4-6（红→绿）：`loader.report` 是**实时诊断面**，不是装载时刻的快照。
///
/// 五个新字段各回答一个运维问题：
/// - `disabled`：哪些行正在休眠（"我明明配了它，为什么没生效"）；
/// - `volatile.{declared,supported}`：多少行声明了 volatile、其中多少行**现在**走得了原地提交；
/// - `interpolation.{failed}`：`!!js` 求值失败累计（M4-4 已落地）；
/// - `reconcile.{applied,skipped,lagging}`：最近一次对账下手了几件、被短路几件、滞后几件；
/// - `realms.{default,named}`：默认域表的条数与具名域数（P2-14 的观测面，两张表都只增不减）。
///
/// "实时"这一半同样要钉住：报告原来只在装载时 `provide` 一次，**之后永远不变**——
/// 那样 `reconcile.*` 只是首轮对账的化石，读它的人会被误导。
#[tokio::test]
async fn report_exposes_diagnostics_and_refreshes() {
    let app = App::new();
    let dir = TempDir::new("report");
    let source = FakeSource::new();
    let yaml = concat!(
        "- id: declaring\n  name: declares\n  config:\n    service: svc\n    value: 1\n",
        "- id: dormant\n  name: provides\n  disabled: true\n  config:\n    service: other\n    value: 2\n",
    );
    let state = load(&app, &dir, "cordis.yml", yaml, &[], source, None)
        .await
        .expect("装载");

    let first = report(&app);
    assert_eq!(first["mounted"], json!(1), "{first}");
    assert_eq!(
        first["disabled"],
        json!(["dormant"]),
        "休眠行要**点名**：{first}"
    );
    assert_eq!(
        first["volatile"],
        json!({ "declared": 1, "supported": 1 }),
        "{first}"
    );
    assert_eq!(first["interpolation"]["failed"], json!(0), "{first}");
    assert_eq!(
        first["reconcile"]["applied"],
        json!(1),
        "初次装载挂了一行：{first}"
    );
    assert_eq!(first["reconcile"]["skipped"], json!(0), "{first}");
    assert_eq!(first["reconcile"]["lagging"], json!(0), "{first}");
    assert_eq!(first["realms"]["named"], json!(0), "{first}");
    assert!(
        first["realms"]["default"].as_u64().unwrap_or(0) >= 1,
        "默认域表至少有一条：{first}"
    );
    assert_eq!(first["failed"], json!([]), "{first}");

    // 改配置：放开休眠行 + 给声明行加一个**具名共享域** ⇒ 报告必须跟着变。
    dir.write(
        "cordis.yml",
        concat!(
            "- id: declaring\n  name: declares\n  isolate:\n    svc: shared\n",
            "  config:\n    service: svc\n    value: 1\n",
            "- id: dormant\n  name: provides\n  config:\n    service: other\n    value: 2\n",
        ),
    );
    let outcome = reload_now(&state).await;
    assert_eq!(outcome.mounted, ["dormant"], "{outcome:?}");
    assert_eq!(outcome.replaced, ["declaring"], "{outcome:?}");

    let second = report(&app);
    assert_eq!(
        second["mounted"],
        json!(2),
        "报告必须**刷新**（不是装载时刻的快照）：{second}"
    );
    assert_eq!(second["disabled"], json!([]), "{second}");
    assert_eq!(
        second["volatile"],
        json!({ "declared": 1, "supported": 1 }),
        "{second}"
    );
    assert_eq!(
        second["realms"]["named"],
        json!(1),
        "具名共享域建出来了：{second}"
    );
    assert_eq!(
        second["reconcile"]["applied"],
        json!(2),
        "这次对账下手两件：{second}"
    );

    // `failed` 与 `disabled` 的差别也要看得见：求值失败是**问题**（记消息），
    // 休眠是**状态**（记行标）。
    dir.write(
        "cordis.yml",
        "- id: broken\n  name: provides\n  config:\n    k: !!js \"1 +\"\n",
    );
    let _ = reload_now(&state).await;
    let third = report(&app);
    assert!(
        third["failed"]
            .as_array()
            .map(|list| !list.is_empty())
            .unwrap_or(false),
        "坏表达式要进 failed：{third}"
    );
    assert_eq!(
        third["interpolation"]["failed"],
        json!(1),
        "并计入插值失败数：{third}"
    );
}

/// **装在自己 fiber 下的装载器被拆掉之后，再来一轮对账**——不许再发布报告，
/// 更不许报"刷新 loader.report 失败"。
///
/// 现象（终验收期抓到，概率约 1/20）：收尾期有一轮对账落在 `loader.report` 的提供者
/// 被拆掉之后 ⇒ `ctx.set` 失败 ⇒ 一条 WARN 混进 demo 的 stdout 快照（A2 因此失败）。
/// 而 `loader.report` 的提供者**就是装载器自己**（`load_with_source` 在 provide 之后
/// 才 `mark_report_published`），所以那一刻的"写不进去"是**正常时序**，不是异常：
/// 只有"装载器还 ACTIVE 却写不进自己的报告"才值得报警。
#[tokio::test]
async fn a_reconcile_after_the_loader_fiber_is_disposed_does_not_publish() {
    let app = App::new();
    let logs = capture_logs(&app);
    let dir = TempDir::new("teardown");
    let path = dir.write("cordis.yml", &yaml_row("keep", "provides", "svc", 7));
    let slot: Arc<Mutex<Option<Arc<LoaderState>>>> = Arc::new(Mutex::new(None));
    let source = FakeSource::new();

    // 装载器挂在一个**自己的**插件 fiber 下（demo 的收尾就是这个形态：
    // 拆掉那个 fiber ⇒ 行 fiber 与 loader.report 的提供者一起走）。
    let fiber = {
        let slot = slot.clone();
        let source = source.clone();
        app.root.plugin(
            Arc::new(cordis_core::FnPlugin {
                name: "loader-host",
                key: cordis_core::unique_key("loader-host"),
                inject: Inject::default(),
                validator: None,
                f: Arc::new(move |ctx: Ctx, _config: Config| {
                    let slot = slot.clone();
                    let source = source.clone();
                    let path = path.clone();
                    Box::pin(async move {
                        let state = load_with_source(
                            ctx,
                            config_for(&path, None),
                            source as Arc<dyn PluginSource>,
                        )
                        .await
                        .map_err(|err| err.to_string())?;
                        *slot.lock().unwrap() = Some(state);
                        Ok(())
                    })
                }),
            }),
            Config::Null,
        )
    };
    fiber.settle().await;
    assert_eq!(fiber.state(), FiberState::Active, "装载器宿主必须先激活");
    assert_eq!(service_of(&app, "svc"), Some(7), "那一行确实被挂上了");

    fiber.dispose().await;
    assert!(
        app.root.get("loader.report").is_none(),
        "报告随提供者一起没了"
    );

    // 晚到的一轮对账：它要能跑完，而不是先写一行"刷新失败"。
    let state = slot.lock().unwrap().clone().expect("装载器状态");
    let _ = state.sync().await;
    let text = logs.lock().unwrap().join("\n");
    assert!(
        !text.contains("刷新 loader.report 失败"),
        "收尾期不许再发布报告（A2 的快照里就多过这一行）：{text}"
    );
}

/// `mount_row` 失败时必须把**半成品 fiber** 拆干净。
///
/// `apply` 失败的 fiber 仍然在 runtime 表里（`fibers_by_plugin_name` 看得见），
/// 它注册的 effect 也还没跑过 disposer。不 dispose 的话：
/// 每次失败挂载都漏一条 fiber；下一轮重试时残留实例还可能抢服务名。
#[tokio::test]
async fn failed_mount_disposes_the_half_built_fiber() {
    let app = App::new();
    let dir = TempDir::new("mount-failure");
    let source = FakeSource::new();
    let path = dir.write("cordis.yml", "- id: broken\n  name: fails\n");
    let mut config = config_for(&path, None);
    config.strict = false; // 降级为逐行失败，才能拿到 state 观察清理结果

    let state = load_with_source(app.root.clone(), config, source.clone())
        .await
        .expect("strict=false ⇒ 失败降级为警告");

    assert!(state.mounted_labels().is_empty(), "失败的挂载不得留下记录");
    assert!(
        app.store.fibers_by_plugin_name("fails").is_empty(),
        "失败的 fiber 必须被 dispose（否则 runtime 表里留着半成品）：{:?}",
        app.store.inventory()
    );
    let trace = source.trace();
    assert!(
        trace.iter().any(|line| line.ends_with(":cleanup")),
        "半成品注册的 effect 必须跑过 disposer：{trace:?}"
    );
    assert!(
        state
            .last_outcome()
            .expect("对账产物")
            .failures
            .iter()
            .any(|failure| failure.contains("故意让 apply 失败")),
        "失败原因必须进报告：{:?}",
        state.last_outcome()
    );
}

/// P1：**patch 插入的行自我卸载时，只能写回 patch 层**。
///
/// 旧实现把"主配置 + patch 合并树"dump 回主配置：patch 插入的行被固化进主配置，
/// 下次读取时 patch 又插一遍 ⇒ 行重复、禁用行复活。这里钉住四件事：
/// ① 主配置字节里没有这一行；② patch 文件没被破坏（仍是合法 YAML、insert 还在）；
/// ③ patch 层多了一条 `disabled: true` overlay；④ 再 reload 不重复、不复活。
#[tokio::test]
async fn self_dispose_of_a_patch_inserted_row_writes_the_patch_layer_not_main() {
    let app = App::new();
    let dir = TempDir::new("self-dispose-patch");
    let source = FakeSource::new();
    dir.write(
        "cordis.patch.yml",
        concat!(
            "- insert:\n",
            "    - id: extra\n",
            "      name: self-removing\n",
            "      config:\n",
            "        service: extra\n",
            "        value: 9\n",
        ),
    );
    let state = load(
        &app,
        &dir,
        "cordis.yml",
        &yaml_row("provider", "provides", "svc", 1),
        &["cordis.patch.yml"],
        source.clone(),
        None,
    )
    .await
    .expect("装载");
    assert_eq!(service_of(&app, "extra"), Some(9));
    assert_eq!(state.mounted_labels(), ["extra", "provider"]);
    let main_before = std::fs::read_to_string(dir.path().join("cordis.yml")).expect("读主配置");

    // 插件自己把自己卸载了 —— 这一行来自 patch 层。
    source.stashed_fiber().dispose().await;
    assert!(service_of(&app, "extra").is_none(), "实例已经拆掉");
    assert_eq!(state.mounted_labels(), ["provider"]);

    let main = std::fs::read_to_string(dir.path().join("cordis.yml")).expect("读主配置");
    assert_eq!(main, main_before, "patch 行的自我卸载不得改写主配置（P1）");
    assert!(
        !main.contains("extra"),
        "patch 插入的行绝不能被固化进主配置：\n{main}"
    );
    assert!(
        !main.contains("self-removing"),
        "patch 层的内容不得写进主配置：\n{main}"
    );

    let patch_text =
        std::fs::read_to_string(dir.path().join("cordis.patch.yml")).expect("读 patch");
    assert!(
        patch_text.contains("extra"),
        "patch 层原有的 insert 不得被破坏：\n{patch_text}"
    );
    assert!(
        patch_text.contains("disabled: true"),
        "必须向 patch 层追加 disabled overlay：\n{patch_text}"
    );

    // patch 层仍然合法，且合并后只有一行 extra、它是 disabled。
    let patches =
        cordis_host::patch::parse_patches(&patch_text).expect("patch 层必须仍是合法 YAML");
    let main_entries = parse_entries(&main).expect("主配置必须仍是合法 YAML");
    let mut warnings = Vec::new();
    let merged = cordis_host::patch::apply_entry_patches(main_entries, &patches, &mut |message| {
        warnings.push(message.to_string())
    });
    assert!(warnings.is_empty(), "{warnings:?}");
    let extras: Vec<&Entry> = merged
        .iter()
        .filter(|entry| entry.id.as_deref() == Some("extra"))
        .collect();
    assert_eq!(extras.len(), 1, "不得重复插入：{merged:?}");
    assert!(
        extras[0].disabled,
        "overlay 必须把它标为 disabled：\n{patch_text}"
    );

    // 再读取一次：不重复、不复活。
    let outcome = reload_now(&state).await;
    assert!(outcome.is_noop(), "文件与树已经一致：{outcome:?}");
    assert!(service_of(&app, "extra").is_none(), "自我卸载的行不得复活");
    assert_eq!(
        state
            .tree_snapshot()
            .iter()
            .filter(|entry| entry.id.as_deref() == Some("extra"))
            .count(),
        1,
        "合并树里只能有一行 extra"
    );
}

/// 预检失败（坏 `.dll` ⇒ `open_staged` 失败）的两个必修项：
/// ① 指纹必须推进，否则 HMR 每 500ms 重试同一个坏库；
/// ② 暂存副本必须销账并删除，否则每个坏构建漏一个 `.rN`（`stagedLeaked` 只增不减）。
#[tokio::test]
async fn failed_reload_precheck_advances_fingerprint_and_cleans_the_staged_copy() {
    cordis_host::source::set_allow_unverified(true);
    let app = App::new();
    let dir = TempDir::new("reload-bad-precheck");
    let plugin_path = dir.write("plug.dll", "not a real dll");
    let config_path = dir.write("cordis.yml", "- id: provider\n  name: plug\n");
    let plugin: Arc<dyn Plugin> =
        cordis_core::plugin_fn("plug", Inject::default(), |_ctx: Ctx, _cfg: Config| {
            Box::pin(async { Ok(()) })
        });
    let source = Arc::new(FileSource {
        source: plugin_path.clone(),
        plugin,
    });
    let state = load_with_source(app.root.clone(), config_for(&config_path, None), source)
        .await
        .expect("首次装载");
    assert_eq!(state.mounted_labels(), ["provider"]);
    assert!(state.stale_labels().is_empty(), "刚装载的指纹是最新的");
    let leaked = cordis_host::staged_leaked();

    // 换成坏库：内容是合法文本但不是动态库 ⇒ 指纹变化、open 必然失败。
    dir.write(
        "plug.dll",
        "definitely not a dynamic library, but its size differs",
    );
    assert_eq!(
        state.stale_labels(),
        ["provider"],
        "指纹变了 ⇒ 这一行应当陈旧"
    );

    let error = cordis_host::loader::reload_one(&app.root, &state, "provider")
        .await
        .expect_err("坏库的预检必须失败");
    assert!(!error.is_empty());
    assert!(
        state.stale_labels().is_empty(),
        "预检失败也要推进指纹（否则 HMR 每 500ms 重试坏库）"
    );
    assert_eq!(
        cordis_host::staged_leaked(),
        leaked,
        "失败的暂存副本必须销账并删除（不得泄漏 .rN）"
    );
    assert_eq!(
        state.mounted_labels(),
        ["provider"],
        "预检失败不得动正在跑的实例"
    );
    assert!(
        state.mounted_liveness().iter().all(|(_, alive)| *alive),
        "记录必须仍指向活着的实例：{:?}",
        state.mounted_liveness()
    );
}

/// 对账与运行期启停**互斥**：对账卡在 mount 上时，`set_disabled` 必须等它结束，
/// 结束后行必须是 disabled/已卸下——不能双挂载，也不能被在飞的树复活。
#[tokio::test]
async fn concurrent_set_disabled_waits_for_an_inflight_reconcile() {
    let app = App::new();
    let dir = TempDir::new("mutation-lock");
    let source = FakeSource::new();
    let state = load(
        &app,
        &dir,
        "cordis.yml",
        &yaml_row("base", "provides", "base", 1),
        &[],
        source.clone(),
        None,
    )
    .await
    .expect("装载");
    assert_eq!(service_of(&app, "base"), Some(1));

    // 第一轮：加一行 `blocked`，它的 apply 会卡在闸门上（这就是"在飞"）。
    let gate = source.open_gate();
    let entries = tree(&format!(
        "{}{}",
        yaml_row("base", "provides", "base", 1),
        yaml_row("blocked", "provides", "blocked", 1)
    ));
    let reconciler = {
        let state = state.clone();
        tokio::spawn(async move { state.reconcile_once(entries).await })
    };
    wait_until("blocked 的 apply 已经卡在闸门上", || {
        source
            .trace()
            .iter()
            .any(|line| line.contains("blocked:") && line.ends_with(":apply"))
    })
    .await;

    let disabling = {
        let state = state.clone();
        tokio::spawn(async move { state.set_disabled("base", Some(true)).await })
    };
    // 对账仍在飞：启停不得插进去卸行（互斥的判据）。
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        service_of(&app, "base"),
        Some(1),
        "在飞的对账期间 base 必须仍在服务"
    );
    assert!(
        !disabling.is_finished(),
        "set_disabled 必须等在对账的互斥闸门外"
    );

    gate.add_permits(1);
    source.close_gate();

    let outcome = tokio::time::timeout(Duration::from_secs(5), reconciler)
        .await
        .expect("对账超时（互斥锁死锁？）")
        .expect("对账任务");
    assert!(outcome.failures.is_empty(), "对账不该失败：{outcome:?}");
    let disabled = tokio::time::timeout(Duration::from_secs(5), disabling)
        .await
        .expect("set_disabled 超时（互斥锁死锁？）")
        .expect("set_disabled 任务")
        .expect("禁用成功");

    assert_eq!(disabled.disposed, ["base"], "{disabled:?}");
    assert!(service_of(&app, "base").is_none(), "禁用后 base 必须被卸下");
    assert_eq!(state.mounted_labels(), ["blocked"], "不得留下幽灵记录");
    assert!(
        state
            .tree_snapshot()
            .iter()
            .find(|entry| entry.id.as_deref() == Some("base"))
            .expect("base 行")
            .disabled,
        "树侧必须是 disabled（不是在飞那份树）"
    );
    assert!(
        state.mounted_liveness().iter().all(|(_, alive)| *alive),
        "记录必须指向活实例：{:?}",
        state.mounted_liveness()
    );
    // 无双挂载/复活：两行各自只 apply 过一次。
    let trace = source.trace();
    assert_eq!(
        trace
            .iter()
            .filter(|line| *line == "base:provides:apply")
            .count(),
        1,
        "base 不得被重复挂载/复活：{trace:?}"
    );
    assert_eq!(
        trace
            .iter()
            .filter(|line| *line == "blocked:provides:apply")
            .count(),
        1,
        "{trace:?}"
    );
}

/// 运行时回写必须拒绝覆盖“磁盘已改、但装载器还没采纳”的外部编辑。
///
/// 触发点用插件自我卸载：它会调用 [`LoaderState::persist`] 整份写回主配置。
/// 如果写回按内存旧树 dump，外部编辑（这里是一条注释标记）就会被悄悄抹掉。
#[tokio::test]
async fn runtime_writeback_refuses_to_overwrite_an_unloaded_external_edit() {
    let app = App::new();
    let dir = TempDir::new("external-edit-writeback");
    let source = FakeSource::new();
    let yaml = yaml_row("suicidal", "self-removing", "svc", 5);
    let config_path = dir.write("cordis.yml", &yaml);
    load(&app, &dir, "cordis.yml", &yaml, &[], source.clone(), None)
        .await
        .expect("装载");

    // 外部编辑落盘，但故意不调用 reload：装载器内存仍是旧树。
    let external = format!("{yaml}\n# EXTERNAL EDIT MUST SURVIVE\n");
    std::fs::write(&config_path, &external).expect("写外部编辑");

    source.stashed_fiber().dispose().await;

    let text = std::fs::read_to_string(&config_path).expect("读回配置");
    assert!(
        text.contains("# EXTERNAL EDIT MUST SURVIVE"),
        "运行时回写覆盖了尚未载入的外部编辑：\n{text}"
    );
    assert!(
        !text.contains("disabled: true"),
        "写回被拒绝后不应留下半套内存树：\n{text}"
    );
}

/// 重复 `id` 必须**报出来**，不能静默覆盖装载表记录（那会让其中一行的实例变幽灵）。
///
/// 语义：第一行获胜、重复行跳过不进挂载序列；问题进 `failures`/日志，strict 下整体失败。
#[tokio::test]
async fn duplicate_labels_are_reported_not_silently_overwritten() {
    let app = App::new();
    let dir = TempDir::new("duplicate-labels");
    let source = FakeSource::new();
    let yaml = concat!(
        "- id: dup\n  name: provides\n  config:\n    service: first\n    value: 1\n",
        "- id: dup\n  name: provides\n  config:\n    service: second\n    value: 2\n",
    );
    let path = dir.write("cordis.yml", yaml);
    let mut config = config_for(&path, None);
    config.strict = false;
    let state = load_with_source(app.root.clone(), config, source.clone())
        .await
        .expect("strict=false ⇒ 重复 label 降级为逐行失败");

    let outcome = state.last_outcome().expect("对账产物");
    assert!(
        outcome
            .failures
            .iter()
            .any(|failure| failure.contains("重复 label")),
        "重复 label 必须进报告：{outcome:?}"
    );
    assert_eq!(
        state.mounted_labels(),
        ["dup"],
        "重复行只挂第一份（记录不得被静默覆盖）"
    );
    assert_eq!(service_of(&app, "first"), Some(1));
    assert!(service_of(&app, "second").is_none());

    // strict 下同一份配置必须整体失败（fail loud，而不是悄悄只装第一份）。
    let app2 = App::new();
    let error = load_with_source(
        app2.root.clone(),
        config_for(&path, None),
        FakeSource::new(),
    )
    .await
    .expect_err("strict 模式下重复 label 必须 fail loud");
    assert!(error.to_string().contains("重复 label"), "{error}");
}

/// 主配置行被 patch 覆盖 `disabled` 时，自我卸载也必须写回 patch 层。
#[tokio::test]
async fn self_dispose_writes_disabled_to_the_patch_owner_layer() {
    let app = App::new();
    let dir = TempDir::new("self-dispose-disabled-owner");
    let source = FakeSource::new();
    let main_yaml = "- id: x\n  name: self-removing\n";
    dir.write("cordis.yml", main_yaml);
    dir.write("cordis.patch.yml", "- id: x\n  disabled: false\n");

    let state = load(
        &app,
        &dir,
        "cordis.yml",
        main_yaml,
        &["cordis.patch.yml"],
        source.clone(),
        None,
    )
    .await
    .expect("装载");
    assert_eq!(state.mounted_labels(), ["x"]);

    source.stashed_fiber().dispose().await;
    assert_eq!(state.mounted_labels(), Vec::<String>::new());

    let main = std::fs::read_to_string(dir.path().join("cordis.yml")).expect("读主配置");
    assert_eq!(main, main_yaml, "有 patch owner 时不得改主配置");
    let patch = std::fs::read_to_string(dir.path().join("cordis.patch.yml")).expect("读 patch");
    assert!(
        patch.contains("disabled: true"),
        "disabled 必须写回 patch owner 层：{patch}"
    );

    reload_now(&state).await;
    assert!(
        state.mounted_labels().is_empty(),
        "reload 后该行仍应保持 disabled"
    );
}

/// 主配置行被 patch 覆盖 `config` 时，运行期 update 必须写回该 patch 层。
///
/// 旧实现按“行来自主配置”写主配置；下一次 reload 时 patch 又把它覆盖回去，
/// 用户看到的是“运行期改了、重启后又变回去”。
#[tokio::test]
async fn runtime_config_update_writes_the_patch_owner_layer() {
    let app = App::new();
    let dir = TempDir::new("patch-owner");
    let source = FakeSource::new();
    let main_yaml = yaml_row("x", "provides", "svc", 1);
    let main_path = dir.write("cordis.yml", &main_yaml);
    let patch_path = dir.write(
        "cordis.patch.yml",
        "- id: x\n  config:\n    service: svc\n    value: 2\n",
    );

    let state = load(
        &app,
        &dir,
        "cordis.yml",
        &main_yaml,
        &["cordis.patch.yml"],
        source,
        None,
    )
    .await
    .expect("装载");
    assert_eq!(service_of(&app, "svc"), Some(2), "patch 先覆盖主配置");

    let fiber = state.fiber_of("x").expect("行 x 必须已挂载");
    fiber
        .update_with(json!({"service":"svc","value":3}), false)
        .await
        .expect("运行期 update");

    assert_eq!(service_of(&app, "svc"), Some(3));
    let main_after = std::fs::read_to_string(&main_path).expect("读主配置");
    assert_eq!(main_after, main_yaml, "有 patch owner 时不得改主配置");
    let patch_after = std::fs::read_to_string(&patch_path).expect("读 patch");
    assert!(
        patch_after.contains("value: 3"),
        "运行期值必须写进 patch 层：{patch_after}"
    );

    reload_now(&state).await;
    assert_eq!(
        service_of(&app, "svc"),
        Some(3),
        "reload 后不能被旧 patch 覆盖回 2"
    );
}
