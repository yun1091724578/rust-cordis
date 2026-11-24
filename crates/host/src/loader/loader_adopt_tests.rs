//! O5a：reload 候选采纳（红→绿测试，crate 内）。
//!
//! # 为什么这些测试在 crate 内
//! 它们要看的正是**不该成为公共 API** 的那几个字段（`main_only` / `main_baseline` /
//! `owners` / `pending`）：候选采纳的缺陷恰恰是"元数据被推进了，但树没采纳"，
//! 从外部只看得见"回写被拒绝"这种二手症状。放进 `#[cfg(test)]` 子模块既能直接断言
//! 已采纳状态，又不给嵌入方多开观测面（ADR 0006）。
//!
//! 同步一律用闸门（[`HookPoint`]）+ `Semaphore`，超时只当看门狗，不靠 sleep 竞态。

use super::*;
use crate::patch::parse_entries;
use crate::plan::PlanEntrySpec;
use cordis_core::App;
use serde_json::Value;
use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};
use std::time::Duration;

// ---------------------------------------------------------------- 夹具

/// 每个用例一个临时目录（跨平台安全）。
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "cordis-o5a-{}-{}-{tag}",
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

/// 一行的 YAML（`id` 唯一 ⇒ label 就是它）。
fn yaml_row(label: &str, service: &str, value: u32) -> String {
    format!("- id: {label}\n  name: fake\n  config:\n    service: {service}\n    value: {value}\n")
}

fn plan_spec(id: &str) -> PlanEntrySpec {
    PlanEntrySpec {
        id: id.to_string(),
        version: "1.0.0".to_string(),
        provides: BTreeMap::new(),
        requires: BTreeMap::new(),
    }
}

/// 剧本式假来源：`load` 给一个"把 `config.service` 注册成 `config.value`"的假插件，
/// `plan_entry` 按剧本逐次返回——于是"两次预检结果不同"可以被精确构造。
struct ScriptSource {
    plans: Mutex<VecDeque<std::result::Result<Option<PlanEntrySpec>, String>>>,
    plan_calls: Arc<AtomicUsize>,
    /// `suicidal` 行在 apply 时把 fiber 交出来（模拟"插件自己卸载自己"）。
    stashed: Arc<Mutex<Vec<Arc<Fiber>>>>,
}

impl ScriptSource {
    fn new(plans: Vec<std::result::Result<Option<PlanEntrySpec>, String>>) -> Arc<Self> {
        Arc::new(Self {
            plans: Mutex::new(plans.into()),
            plan_calls: Arc::new(AtomicUsize::new(0)),
            stashed: Arc::new(Mutex::new(Vec::new())),
        })
    }

    fn plan_calls(&self) -> usize {
        self.plan_calls.load(Ordering::SeqCst)
    }

    fn stashed_fiber(&self) -> Arc<Fiber> {
        self.stashed
            .lock()
            .unwrap()
            .pop()
            .expect("`suicidal` 行应当把 fiber 交出来")
    }
}

impl PluginSource for ScriptSource {
    fn load(
        &self,
        entry: &Entry,
        _search: &[PathBuf],
        _revision: u64,
    ) -> std::result::Result<LoadedFile, String> {
        let config = entry.config.clone().unwrap_or(Config::Null);
        let service = config
            .get("service")
            .and_then(Value::as_str)
            .unwrap_or("svc")
            .to_string();
        let stash_self = entry.name == "suicidal";
        let stashed = self.stashed.clone();
        let plugin: Arc<dyn Plugin> =
            cordis_core::plugin_fn("fake", Inject::default(), move |ctx: Ctx, cfg: Config| {
                let service = service.clone();
                let stashed = stashed.clone();
                Box::pin(async move {
                    // 每次 apply 都重读配置：`fiber.update` 的"不重挂"路径靠这一点被观察到。
                    let value = cfg.get("value").and_then(Value::as_u64).unwrap_or(0) as u32;
                    if stash_self {
                        stashed.lock().unwrap().push(ctx.fiber());
                    }
                    ctx.provide(&service, Dyn::new(value))?;
                    Ok(())
                })
            });
        let path = PathBuf::from("<fake>");
        Ok(LoadedFile {
            plugin,
            source: path.clone(),
            staged: path,
            // 假源没有真实文件指纹：`None` = "指纹未知"，不参与插件文件轮询。
            fingerprint: None,
        })
    }

    fn plan_entry(
        &self,
        _entry: &Entry,
        _search: &[PathBuf],
    ) -> std::result::Result<Option<PlanEntrySpec>, String> {
        self.plan_calls.fetch_add(1, Ordering::SeqCst);
        self.plans.lock().unwrap().pop_front().unwrap_or(Ok(None))
    }
}

fn config_for(path: &Path) -> LoaderConfig {
    LoaderConfig {
        path: path.to_string_lossy().into_owned(),
        patches: Vec::new(),
        plugin_dir: None,
        strict: true,
        strict_cycles: false,
        // 每个用例自己决定：多数用例要 `true`，才能把"预检 Err"升级成可观察的失败。
        strict_sidecars: false,
        hmr: None,
        js_evaluator: None,
        interpolation: InterpolationMode::Builtin,
        dispatch_policy: None,
        host_limits: None,
        dependency_plan: Vec::new(),
    }
}

async fn load_scripted(app: &App, path: &Path, source: Arc<ScriptSource>) -> Arc<LoaderState> {
    let mut config = config_for(path);
    config.strict_sidecars = true;
    load_with_source(app.root.clone(), config, source)
        .await
        .expect("装载")
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

fn service_of(app: &App, name: &str) -> Option<u32> {
    app.root
        .get(name)
        .and_then(|value| value.downcast::<u32>())
        .map(|value| *value)
}

/// 已采纳的**回写元数据**：`main_only` + 文件基线（配置回写的唯一来源）。
fn accepted_metadata(state: &Arc<LoaderState>) -> (Vec<Entry>, String) {
    (
        state.main_only.lock().unwrap().clone(),
        state.main_baseline.lock().unwrap().clone(),
    )
}

fn value_of(entry: &Entry) -> Value {
    entry
        .config
        .as_ref()
        .and_then(|config| config.get("value").cloned())
        .unwrap_or(Value::Null)
}

fn tree_value(state: &Arc<LoaderState>, label: &str) -> Value {
    state
        .tree_snapshot()
        .iter()
        .find(|entry| entry.id.as_deref() == Some(label))
        .map(value_of)
        .unwrap_or(Value::Null)
}

/// 一次运行时回写（`fiber.update`）：走 `internal/commit` → 装载器的提交观察者。
async fn writeback(state: &Arc<LoaderState>, label: &str, value: u32) {
    let fiber = state.fiber_of(label).expect("行必须还挂着");
    fiber
        .update(json!({ "service": "svc", "value": value }))
        .await
        .expect("运行时回写应当被接受");
}

// ---------------------------------------------------------------- O5a-1

/// 预检失败的 reload **不得推进已采纳的回写元数据**。
///
/// 缺陷：`parse_snapshot` 在预检之前就把 `main_only` / `owners` / `main_baseline`
/// 写成了新配置。预检失败后回滚是在 `config_gate` 之外、三次独立加锁采的旧值，
/// 且无条件整份覆盖——于是"一份永远不会被采纳的配置"参与了回写决策。
///
/// 断言必须落在**解析完成、预检未跑**那一刻：只断言 reload 返回后的元数据是没用的
/// ——旧实现会把它们回滚到同一个值，去掉修复这条用例仍会绿。所以这里停在
/// [`HookPoint::AfterParse`]（此刻候选还没预检、更没被采纳），当场要求元数据仍是
/// **已采纳的那一份**；旧实现此刻已经把它换成了候选的值 2（必红）。
#[tokio::test]
async fn reload_preflight_failure_does_not_advance_writeback_metadata() {
    let app = App::new();
    let dir = TempDir::new("preflight-metadata");
    let path = dir.write("cordis.yml", &yaml_row("single", "svc", 1));
    // 初装预检 Ok；reload 的预检 Err（strict_sidecars ⇒ 升级成失败）。
    let source = ScriptSource::new(vec![
        Ok(Some(plan_spec("fake"))),
        Err("sidecar manifest 被截断".into()),
    ]);
    let state = load_scripted(&app, &path, source.clone()).await;
    assert_eq!(service_of(&app, "svc"), Some(1));
    let accepted = accepted_metadata(&state);

    // 外部编辑：值 2（能解析，但预检会失败）。
    std::fs::write(&path, yaml_row("single", "svc", 2)).expect("写新配置");
    let gate = state.install_hook(HookPoint::AfterParse);
    let reloading = {
        let state = state.clone();
        tokio::spawn(async move { state.reload().await })
    };
    gate.wait_arrived().await;
    state.clear_hooks();

    // 停点：解析已完成，预检**还没**跑。已采纳的元数据必须一个字节都没动。
    assert_eq!(
        accepted_metadata(&state),
        accepted,
        "解析完成、预检未跑时元数据必须仍指向已采纳的那一份：解析不得推进 main_only / main_baseline"
    );

    gate.release();
    match tokio::time::timeout(Duration::from_secs(5), reloading)
        .await
        .expect("reload 超时")
        .expect("reload 任务")
    {
        ReloadOutcome::Failed(err) => assert!(err.contains("预检"), "失败必须可诊断：{err}"),
        other => panic!("预检失败必须报告 Failed，实际：{other:?}"),
    }

    assert_eq!(
        accepted_metadata(&state),
        accepted,
        "未采纳的配置不得推进 main_only / main_baseline"
    );
    assert_eq!(service_of(&app, "svc"), Some(1), "旧实例必须照旧在服务");
}

/// 第二次预检（对账里那次）失败同样不得推进元数据，也不得换树。
///
/// 缺陷：`reconcile_once_inner` 的预检失败**没有回滚**——元数据停在候选配置，
/// 树仍是旧树，候选被丢弃。之后的回写会按这份没被采纳的配置操作。
#[tokio::test]
async fn reconcile_preflight_failure_discards_the_candidate_without_advancing_metadata() {
    let app = App::new();
    let dir = TempDir::new("reconcile-preflight-metadata");
    let path = dir.write("cordis.yml", &yaml_row("single", "svc", 1));
    // ① 初装预检 Ok、② reload 预检 Ok、③ 对账预检 Err。
    let source = ScriptSource::new(vec![
        Ok(Some(plan_spec("fake"))),
        Ok(Some(plan_spec("fake"))),
        Err("sidecar manifest 不可用".into()),
    ]);
    let state = load_scripted(&app, &path, source.clone()).await;
    let accepted = accepted_metadata(&state);
    let tree_before = state.tree_snapshot();

    std::fs::write(&path, yaml_row("single", "svc", 2)).expect("写新配置");
    let outcome = match state.reload().await {
        ReloadOutcome::Reconciled(outcome) => outcome,
        other => panic!("第二次预检失败应当在对账产物里报出来，实际：{other:?}"),
    };
    assert!(
        outcome.failures.iter().any(|line| line.contains("预检")),
        "第二次预检失败必须可诊断：{outcome:?}"
    );
    assert_eq!(source.plan_calls(), 3, "两次预检都要真的问过来源");

    assert_eq!(
        accepted_metadata(&state),
        accepted,
        "被丢弃的候选不得推进 main_only / main_baseline"
    );
    assert_eq!(state.tree_snapshot(), tree_before, "候选没采纳：树必须原样");
    assert_eq!(service_of(&app, "svc"), Some(1));
}

/// 被丢弃的候选不得吞掉一次**成功**的运行时回写。
///
/// 缺陷：预检失败走的是"三次独立加锁回滚"，会把在这之间成功落盘的运行期回写
/// 的基线整份盖回去——此后每一次回写都被"已被外部修改但尚未载入"拒绝，
/// 而磁盘上其实只有装载器自己写的内容。
#[tokio::test]
async fn a_rejected_candidate_does_not_swallow_a_successful_writeback() {
    let app = App::new();
    let dir = TempDir::new("rejected-candidate-writeback");
    let path = dir.write("cordis.yml", &yaml_row("single", "svc", 1));
    let logs = capture_logs(&app);
    let source = ScriptSource::new(vec![
        Ok(Some(plan_spec("fake"))),
        Err("sidecar manifest 被截断".into()),
    ]);
    let state = load_scripted(&app, &path, source.clone()).await;
    assert_eq!(service_of(&app, "svc"), Some(1));

    // 让第二轮 reload 解析完就停住：此刻磁盘内容仍是装载器采纳过的那一份。
    let gate = state.install_hook(HookPoint::AfterParse);
    let reloading = {
        let state = state.clone();
        tokio::spawn(async move { state.reload().await })
    };
    gate.wait_arrived().await;
    state.clear_hooks();

    // 在飞期间发生一次**成功**的运行时回写（磁盘基线未变 ⇒ 允许写盘）。
    writeback(&state, "single", 9).await;
    let text = std::fs::read_to_string(&path).expect("读回配置");
    assert!(
        text.contains("value: 9"),
        "在飞期间的运行时回写必须落盘：\n{text}"
    );

    // 放行那轮 reload：预检失败 ⇒ 候选不采纳。
    gate.release();
    match tokio::time::timeout(Duration::from_secs(5), reloading)
        .await
        .expect("reload 超时")
        .expect("reload 任务")
    {
        ReloadOutcome::Failed(err) => assert!(err.contains("预检"), "{err}"),
        other => panic!("预检失败必须报告 Failed，实际：{other:?}"),
    }

    // 再来一次运行时回写：它必须成功写盘，而不是被"已被外部修改但尚未载入"拒绝。
    writeback(&state, "single", 7).await;
    let text = std::fs::read_to_string(&path).expect("读回配置");
    assert!(
        text.contains("value: 7"),
        "回写被上一次失败候选的回滚吞掉了：\n{text}"
    );
    let refused = logs
        .lock()
        .unwrap()
        .iter()
        .any(|line| line.contains("已被外部修改但尚未载入"));
    assert!(!refused, "不该出现外部编辑拒绝：{:?}", logs.lock().unwrap());
}

// ---------------------------------------------------------------- O5a-2

/// 迟到的**旧**解析不得复活：A 在解析后停住，B 完整跑完，再放行 A。
///
/// 缺陷：合并槽里只存 `Vec<Entry>`、没有代次，于是"到达顺序"被当成了"新旧顺序"：
/// A（更旧的配置）稍晚登记就顶掉 B，最终树与元数据都退回 A——用户会看到
/// "新加的插件被删掉、删掉的插件又活了"。
#[tokio::test]
async fn a_late_old_parse_does_not_resurrect() {
    let app = App::new();
    let dir = TempDir::new("gen-order");
    let path = dir.write("cordis.yml", &yaml_row("base", "base", 1));
    let source = ScriptSource::new(Vec::new());
    let state = load_scripted(&app, &path, source.clone()).await;
    assert_eq!(state.mounted_labels(), ["base"]);

    // A：旧配置（base + a）。解析完就停住。
    let gate = state.install_hook(HookPoint::AfterParse);
    std::fs::write(
        &path,
        format!("{}{}", yaml_row("base", "base", 1), yaml_row("a", "a", 1)),
    )
    .expect("写 A 的配置");
    let late_a = {
        let state = state.clone();
        tokio::spawn(async move { state.reload().await })
    };
    gate.wait_arrived().await;
    state.clear_hooks();

    // B：新配置（base + b）完整跑完。
    std::fs::write(
        &path,
        format!("{}{}", yaml_row("base", "base", 1), yaml_row("b", "b", 1)),
    )
    .expect("写 B 的配置");
    match state.reload().await {
        ReloadOutcome::Reconciled(outcome) => assert!(outcome.failures.is_empty(), "{outcome:?}"),
        other => panic!("B 应当完整采纳：{other:?}"),
    }
    assert_eq!(state.mounted_labels(), ["b", "base"]);
    let adopted_by_b = state.adopted_generation();
    assert!(adopted_by_b > 0, "B 必须留下已采纳的代次");

    // 放行 A：它的解析结果已经过期，不得再采纳一次。
    gate.release();
    let late = tokio::time::timeout(Duration::from_secs(5), late_a)
        .await
        .expect("A 超时")
        .expect("A 任务");
    assert!(
        !matches!(late, ReloadOutcome::Reconciled(_)),
        "迟到的旧解析不得再采纳一次：{late:?}"
    );

    assert_eq!(state.mounted_labels(), ["b", "base"], "最终装载表必须是 B");
    assert_eq!(service_of(&app, "a"), None, "A 的行不得复活");
    assert_eq!(service_of(&app, "b"), Some(1));
    let labels: Vec<String> = state
        .tree_snapshot()
        .iter()
        .map(|entry| entry.id.clone().unwrap_or_default())
        .collect();
    assert_eq!(labels, ["base", "b"], "最终树必须是 B");
    let (main, baseline) = accepted_metadata(&state);
    let main_labels: Vec<String> = main
        .iter()
        .map(|entry| entry.id.clone().unwrap_or_default())
        .collect();
    assert_eq!(main_labels, ["base", "b"], "回写主树必须是 B");
    assert!(
        baseline.contains("id: b") && !baseline.contains("id: a"),
        "文件基线必须是 B：\n{baseline}"
    );
    assert_eq!(
        state.adopted_generation(),
        adopted_by_b,
        "A 不得再产生一次采纳"
    );
    assert!(
        state.pending_entries().is_none(),
        "过期候选不得留在合并槽里"
    );
}

// ---------------------------------------------------------------- O5a-3

/// 在飞候选遇上运行时回写（`no_save`）= 候选过期 ⇒ 必须按磁盘**重建**后采纳，
/// 而不是把候选的整份树盖到已被回写改过的树上。
///
/// 缺陷（修复前）：采纳用的是候选的 `entries` 整份覆盖树。watcher 的指纹已经对齐磁盘，
/// 不会因为这次覆盖再自动补偿——运行时刚改的值被悄悄退回配置里的旧值，而且没有任何
/// 失败信号。修复后过期候选不再被采纳：驱动器重新稳读 → 解析 → 预检 → 重建
/// （快照新的回写版本）再采纳，于是"文件是真相"这条不变量成立：
/// 树 / 回写元数据 / 磁盘三者一致，`plan()` 收敛为空。
///
/// 代价说清楚：`no_save=true` 的纯内存值没有落盘，重建后的候选按磁盘内容收敛——
/// 于是那个纯内存值被文件内容取代（它本来就没写进文件）。落盘分支见
/// [`a_writeback_version_bump_keeps_disk_and_memory_consistent`]。
#[tokio::test]
async fn a_writeback_invalidates_an_in_flight_candidate() {
    let app = App::new();
    let dir = TempDir::new("writeback-stale-nosave");
    let path = dir.write("cordis.yml", &yaml_row("single", "svc", 1));
    let source = ScriptSource::new(Vec::new());
    let state = load_scripted(&app, &path, source.clone()).await;
    assert_eq!(service_of(&app, "svc"), Some(1));

    // 外部编辑：值 2 —— 候选要挂的就是它。
    std::fs::write(&path, yaml_row("single", "svc", 2)).expect("写新配置");
    let gate = state.install_hook(HookPoint::BeforeAdopt);
    let reloading = {
        let state = state.clone();
        tokio::spawn(async move { state.reload().await })
    };
    gate.wait_arrived().await;
    state.clear_hooks();

    // 运行时回写（`no_save`：只推进内存目标，不落盘）。
    let before_writeback = state.writeback_version();
    let fiber = state.fiber_of("single").expect("行必须还挂着");
    fiber
        .update_with(json!({ "service": "svc", "value": 9 }), true)
        .await
        .expect("运行时回写应当被接受");
    assert_eq!(tree_value(&state, "single"), json!(9), "回写必须生效");
    assert!(
        state.writeback_version() > before_writeback,
        "`no_save` 的纯内存提交同样推进回写版本（否则在飞候选仍会盖回去）"
    );

    // 放行候选：它基于旧的回写版本 ⇒ 过期。修复后**不采纳旧候选**，
    // 而是按磁盘重建一份再采纳（重试次数有界）。
    gate.release();
    let outcome = match tokio::time::timeout(Duration::from_secs(5), reloading)
        .await
        .expect("reload 超时")
        .expect("reload 任务")
    {
        ReloadOutcome::Reconciled(outcome) => outcome,
        other => panic!("过期候选必须有可诊断的对账产物：{other:?}"),
    };
    assert!(
        outcome.failures.is_empty(),
        "过期候选必须重建后采纳，不得只剩一条失败：{outcome:?}"
    );
    assert_eq!(state.stale_rebuild_count(), 1, "过期候选必须按磁盘重建一次");
    assert!(
        outcome.updated.contains(&"single".to_string()),
        "重建后实例必须收敛到磁盘内容：{outcome:?}"
    );
    assert_eq!(
        tree_value(&state, "single"),
        json!(2),
        "采纳的必须是磁盘上的内容（旧候选的旧树不得覆盖）"
    );
    assert_eq!(service_of(&app, "svc"), Some(2), "实例必须收敛到磁盘内容");
    let text = std::fs::read_to_string(&path).expect("读回配置");
    assert!(text.contains("value: 2"), "磁盘必须是那次编辑：\n{text}");
    assert!(
        state.plan(&state.tree_snapshot()).is_empty(),
        "装载记录与树必须一致（计划收敛为空）：{:?}",
        state.plan(&state.tree_snapshot())
    );
    assert!(!state.reconciling.load(Ordering::SeqCst), "单飞位必须复位");
}

/// 回写成功落盘之后，过期候选同样不得把树与磁盘拆开；按磁盘重建后收敛。
///
/// 这里走 `save` 分支：回写真的写盘（基线仍等于磁盘），随后过期的候选被丢弃并按
/// 磁盘重建，于是"内存树 / 装载记录 / 回写元数据 / 磁盘"四处仍是一致的
/// （`plan()` 收敛为空）。修复前这条候选只会留下一条"过期"失败：树停在回写值、
/// 回写元数据也停在候选构造时的旧值。
#[tokio::test]
async fn a_writeback_version_bump_keeps_disk_and_memory_consistent() {
    let app = App::new();
    let dir = TempDir::new("writeback-stale-save");
    let path = dir.write("cordis.yml", &yaml_row("single", "svc", 1));
    let source = ScriptSource::new(Vec::new());
    let state = load_scripted(&app, &path, source.clone()).await;

    // 候选来自"值 2"的编辑。
    std::fs::write(&path, yaml_row("single", "svc", 2)).expect("写新配置");
    let gate = state.install_hook(HookPoint::BeforeAdopt);
    let reloading = {
        let state = state.clone();
        tokio::spawn(async move { state.reload().await })
    };
    gate.wait_arrived().await;
    state.clear_hooks();

    // 用户撤销了那次编辑（磁盘回到已采纳的内容）⇒ 运行时回写允许落盘。
    std::fs::write(&path, yaml_row("single", "svc", 1)).expect("撤销外部编辑");
    writeback(&state, "single", 9).await;

    gate.release();
    let outcome = match tokio::time::timeout(Duration::from_secs(5), reloading)
        .await
        .expect("reload 超时")
        .expect("reload 任务")
    {
        ReloadOutcome::Reconciled(outcome) => outcome,
        other => panic!("过期候选必须有可诊断的对账产物：{other:?}"),
    };
    assert!(
        outcome.failures.is_empty(),
        "过期候选必须重建后采纳，不得只剩一条失败：{outcome:?}"
    );
    assert_eq!(state.stale_rebuild_count(), 1, "过期候选必须按磁盘重建一次");
    let text = std::fs::read_to_string(&path).expect("读回配置");
    assert!(
        text.contains("value: 9"),
        "磁盘必须是运行时回写的结果：\n{text}"
    );
    assert_eq!(tree_value(&state, "single"), json!(9), "树不得被退回");
    let (main, baseline) = accepted_metadata(&state);
    assert_eq!(
        main.iter()
            .find(|entry| entry.id.as_deref() == Some("single"))
            .map(value_of),
        Some(json!(9)),
        "回写主树必须与重建后的磁盘一致"
    );
    assert!(
        baseline.contains("value: 9"),
        "文件基线必须是重建时读到的磁盘内容：\n{baseline}"
    );
    assert!(
        state.plan(&state.tree_snapshot()).is_empty(),
        "磁盘与内存必须一致（计划收敛为空）：{:?}",
        state.plan(&state.tree_snapshot())
    );
}

// ---------------------------------------------------------------- O5a-3b（重建路径）

/// 有界等待闸门到达（看门狗）：回归时"下一轮永远不会来"必须变成**失败**，
/// 而不是挂死整个测试进程。
async fn wait_arrived_bounded(gate: &Arc<TestGate>, what: &str) {
    tokio::time::timeout(Duration::from_secs(5), gate.wait_arrived())
        .await
        .unwrap_or_else(|_| panic!("{what}：闸门到达超时（这一轮没有发生）"));
}

/// 回写过期**不得**把用户编辑判死刑：候选必须按磁盘重建并最终采纳。
///
/// 规格 §O5："运行时回写…使基于旧版本的在飞/排队候选失效；**失效候选必须重新读取/
/// 构造并预检**，不能用旧 baseline 覆盖回写。"
///
/// 这里的交错是确定性的：用户把值改成 B（磁盘 = B），`reload` 已过预检、正要采纳；
/// 此刻插件回写 `fiber.update`，磁盘因**外部编辑保护**被拒（文件仍是 B），但回写已经
/// 改了内存目标并推进回写版本 ⇒ 候选过期。watcher 的指纹早已对齐磁盘，文件不变就
/// 不会再来一次 reload——修复前 B 永久丢失（树停在运行值，只剩一条 Error 日志 +
/// 一条 `failures`）。修复后驱动器按磁盘重建候选、再预检、再采纳：B 落地，
/// 树 / 回写元数据 / 磁盘三者一致，重试次数有界且单飞位复位。
///
/// **可证伪性**：快照候选（B = 2）之后、放行采纳之前，磁盘再被改成 C（值 3）。
/// 于是"重建"必须**真的重读磁盘**才可能采纳 3；一个同代次、只刷新 `base_writeback`
/// 再登记的伪重建会采纳内存里的旧解析结果（2），本条立刻红。`plan_entry` 的调用
/// 计数进一步钉住"重建重跑了预检"（伪重建不会多问来源一次）。
#[tokio::test]
async fn a_stale_candidate_is_rebuilt_from_disk_and_the_user_edit_is_adopted() {
    let app = App::new();
    let dir = TempDir::new("stale-rebuild-adopts");
    let path = dir.write("cordis.yml", &yaml_row("single", "svc", 1));
    let logs = capture_logs(&app);
    let source = ScriptSource::new(Vec::new());
    let state = load_scripted(&app, &path, source.clone()).await;
    assert_eq!(service_of(&app, "svc"), Some(1));

    // 用户编辑 B：磁盘值 2。reload 解析完、预检过，正要采纳（停在对账内核里）。
    std::fs::write(&path, yaml_row("single", "svc", 2)).expect("写 B");
    let gate = state.install_hook(HookPoint::BeforeAdopt);
    let reloading = {
        let state = state.clone();
        tokio::spawn(async move { state.reload().await })
    };
    wait_arrived_bounded(&gate, "reload 还没走到采纳之前").await;
    state.clear_hooks();

    // 在飞期间插件回写（值 9）：磁盘上是 B（≠ 已采纳基线）⇒ 落盘被外部编辑保护拒绝，
    // 文件仍是 B；但内存目标与回写版本已经推进 ⇒ 在飞候选**过期**。
    let before = state.writeback_version();
    writeback(&state, "single", 9).await;
    assert!(
        state.writeback_version() > before,
        "回写必须推进回写版本（否则候选不会过期，这条用例就没有意义）"
    );
    let text = std::fs::read_to_string(&path).expect("读回配置");
    assert!(text.contains("value: 2"), "磁盘仍是用户编辑的 B：\n{text}");
    assert!(
        logs.lock()
            .unwrap()
            .iter()
            .any(|line| line.contains("已被外部修改但尚未载入")),
        "回写落盘必须被外部编辑保护拒绝：{:?}",
        logs.lock().unwrap()
    );

    // 第二次编辑 C：磁盘值 3——发生在候选 B 已快照、已过预检、正要采纳之后。
    // 只有**真的重读磁盘**的重建才可能读到它；伪重建会沿用 B 的旧解析结果（2）。
    std::fs::write(&path, yaml_row("single", "svc", 3)).expect("写 C");
    let text = std::fs::read_to_string(&path).expect("读回配置");
    assert!(
        text.contains("value: 3"),
        "放行前磁盘必须已经是 C（写入已完成，重建要读的就是它）：\n{text}"
    );

    gate.release();
    let outcome = match tokio::time::timeout(Duration::from_secs(5), reloading)
        .await
        .expect("reload 超时")
        .expect("reload 任务")
    {
        ReloadOutcome::Reconciled(outcome) => outcome,
        other => panic!("过期候选重建后应当完成一轮对账：{other:?}"),
    };

    // 修复前：`failures` 只有一条"过期"，树停在 9、磁盘停在 B —— B 永久丢失。
    assert!(
        outcome.failures.is_empty(),
        "用户编辑不得丢失（不得只剩一条失败）：{outcome:?}"
    );
    assert_eq!(state.stale_rebuild_count(), 1, "过期候选必须按磁盘重建一次");
    // 预检重跑的证据：每次预检 = 逐行问一次来源（本配置只有 `single` 一行，于是
    // `plan_entry` 恰好调一次）。整轮下来共 5 次：
    // ① 初装（`load_with_source` 挂载前的预检；初装那次对账用 `skip_next_preflight`
    //    跳过，不再问）、
    // ② `reload` 的 `build_candidate` 预检、
    // ③ 该候选的 `reconcile_attempt` 预检、
    // ④ **重建**的 `build_candidate` 预检、
    // ⑤ 重建候选的 `reconcile_attempt` 预检。
    // ④ 是重建独有的：伪重建（只刷新 `base_writeback` 再登记同一个代次）不读磁盘、
    //    不重跑预检，整轮只有 4 次（①②③ + 重建候选照旧过的对账预检 ⑤），
    //    于是这个 5 精确钉住"重建真的重跑了预检"（实测伪重建下是 4）。
    assert_eq!(
        source.plan_calls(),
        5,
        "重建必须重新预检（每步一次 `plan_entry`）：见本断言上方的 ①..⑤ 分解"
    );
    let rebuilt_logs: Vec<String> = logs
        .lock()
        .unwrap()
        .iter()
        .filter(|line| line.contains("已按磁盘重建"))
        .cloned()
        .collect();
    assert_eq!(
        rebuilt_logs.len(),
        1,
        "重建必须留且只留一条「已按磁盘重建」日志（可诊断）：{:?}",
        logs.lock().unwrap()
    );
    assert!(
        rebuilt_logs[0].contains("第 1/3 次"),
        "日志必须说明这是同一代次的第几次重建：{}",
        rebuilt_logs[0]
    );

    // 树 / 回写元数据 / 磁盘三者一致：都是 C（值 3）。
    assert_eq!(
        tree_value(&state, "single"),
        json!(3),
        "树必须是重建时从磁盘读到的 C：{:?}",
        state.tree_snapshot()
    );
    let (main, baseline) = accepted_metadata(&state);
    assert_eq!(
        main.iter()
            .find(|entry| entry.id.as_deref() == Some("single"))
            .map(value_of),
        Some(json!(3)),
        "回写主树必须是 C"
    );
    assert!(
        baseline.contains("value: 3"),
        "文件基线必须是重建时读到的 C：\n{baseline}"
    );
    let text = std::fs::read_to_string(&path).expect("读回配置");
    assert!(
        text.contains("value: 3"),
        "磁盘必须是 C（重建不得把它写回 B）：\n{text}"
    );
    assert_eq!(service_of(&app, "svc"), Some(3), "实例必须收敛到 C");
    assert!(
        state.plan(&state.tree_snapshot()).is_empty(),
        "装载记录与树必须一致（计划收敛为空）：{:?}",
        state.plan(&state.tree_snapshot())
    );
    assert!(
        !state.reconciling.load(Ordering::SeqCst),
        "running 标记必须复位"
    );
    assert!(state.pending_entries().is_none(), "合并槽必须被抽干");
}

/// 连续过期超过上限：不得无限循环，返回可诊断失败，槽位与单飞位都干净。
///
/// 每一轮（首轮 + 每次重建）都在"预检过了、还没采纳"这一刻再改一次运行时值，
/// 于是重建出来的候选**每一轮**都过期。上限（[`MAX_STALE_REBUILDS`]）到点必须停：
/// 保留那条过期失败、再加一条"已达上限"，而不是无限重建。
///
/// 停点用闸门而不用 sleep：每次放行**之前**先为下一轮装好新闸门（替换槽位不影响
/// 已经等在旧闸门上的那一轮），于是"轮次"是完全确定的。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stale_candidate_stops_rebuilding_at_the_cap() {
    let app = App::new();
    let dir = TempDir::new("stale-rebuild-cap");
    let path = dir.write("cordis.yml", &yaml_row("single", "svc", 1));
    let logs = capture_logs(&app);
    let source = ScriptSource::new(Vec::new());
    let state = load_scripted(&app, &path, source.clone()).await;
    assert_eq!(service_of(&app, "svc"), Some(1));

    // 用户编辑 B（磁盘值 2），reload 停在采纳之前。
    std::fs::write(&path, yaml_row("single", "svc", 2)).expect("写 B");
    let mut gate = state.install_hook(HookPoint::BeforeAdopt);
    let reloading = {
        let state = state.clone();
        tokio::spawn(async move { state.reload().await })
    };
    wait_arrived_bounded(&gate, "首轮 reload 还没走到采纳之前").await;

    // 轮次 0 = 首轮；轮次 1..=MAX = 每次重建。每轮都在停点上再推一次回写版本。
    let rounds = MAX_STALE_REBUILDS + 1;
    let mut last_value = 0;
    for round in 0..rounds {
        // 为**下一轮**预装闸门：只有还会再来一轮时才装（最后一轮直接收工）。
        let next = (round + 1 < rounds).then(|| state.install_hook(HookPoint::BeforeAdopt));
        let value = 20 + round;
        writeback(&state, "single", value).await;
        last_value = value;
        gate.release();
        match next {
            Some(next) => {
                wait_arrived_bounded(&next, "过期候选没有重建（下一轮对账没来）").await;
                gate = next;
            }
            None => break,
        }
    }

    let outcome = match tokio::time::timeout(Duration::from_secs(5), reloading)
        .await
        .expect("reload 超时（重建没有停下来 ⇒ 无限循环）")
        .expect("reload 任务")
    {
        ReloadOutcome::Reconciled(outcome) => outcome,
        other => panic!("超过上限必须有可诊断的对账产物：{other:?}"),
    };

    assert_eq!(
        state.stale_rebuild_count(),
        MAX_STALE_REBUILDS as u64,
        "重建次数必须被上限卡住（不得无限重试）"
    );
    assert!(
        outcome.failures.iter().any(|line| line.contains("过期")),
        "过期失败必须保留：{outcome:?}"
    );
    assert!(
        outcome.failures.iter().any(|line| line.contains("上限")),
        "停止重建必须给出原因：{outcome:?}"
    );
    assert!(
        !state.reconciling.load(Ordering::SeqCst),
        "running 标记必须复位"
    );
    assert!(state.pending_entries().is_none(), "合并槽必须干净");
    // 停在上限之后没有任何一次采纳：树停在运行值，磁盘仍是用户编辑的 B。
    assert_eq!(
        tree_value(&state, "single"),
        json!(last_value),
        "失败的重建不得改动树"
    );
    // 实例侧同样停在最后一次运行时回写的值（重建一次都没采纳，所以没有"收敛到磁盘"）。
    assert_eq!(
        service_of(&app, "svc"),
        Some(last_value),
        "失败的重建不得改动实例：实例必须仍是最后一次运行时回写的值"
    );
    let text = std::fs::read_to_string(&path).expect("读回配置");
    assert!(
        text.contains("value: 2"),
        "失败的重建路径不得写盘：\n{text}"
    );
    // 装载记录与树仍然自洽（`plan()` 是对账的目标态差异，与"是否采纳过"无关）。
    assert!(
        state.plan(&state.tree_snapshot()).is_empty(),
        "装载记录与树必须一致（计划收敛为空）：{:?}",
        state.plan(&state.tree_snapshot())
    );
    // 每次重建都必须留一条「已按磁盘重建」，且恰好是上限那么多次——不是零次
    // （`stale_rebuild_count` 数到了但日志没写），也不是更多（偷偷多重建了一轮）。
    let rebuilt_logs = logs
        .lock()
        .unwrap()
        .iter()
        .filter(|line| line.contains("已按磁盘重建"))
        .count();
    assert_eq!(
        rebuilt_logs,
        MAX_STALE_REBUILDS as usize,
        "重建日志次数必须与上限一致：{:?}",
        logs.lock().unwrap()
    );
}

/// 程序化登记的候选（`request_reconcile`，不带文件派生的回写元数据）过期时
/// **不**从磁盘重建：那会把调用方给的条目树换成磁盘内容。仍按既有语义报出失败。
///
/// 这条边界是重建路径的分寸：只有 `reload` 造出来的候选（`adopt_metadata`）才"属于
/// 文件"，重建它得到的仍是同一个意图；调用方自己算出来的树没有这份归属。
#[tokio::test]
async fn a_programmatic_candidate_that_goes_stale_is_reported_not_rebuilt() {
    let app = App::new();
    let dir = TempDir::new("stale-programmatic");
    let path = dir.write("cordis.yml", &yaml_row("base", "base", 1));
    let logs = capture_logs(&app);
    let source = ScriptSource::new(Vec::new());
    let state = load_scripted(&app, &path, source.clone()).await;
    assert_eq!(service_of(&app, "base"), Some(1));

    // 候选：base + extra（直接登记；磁盘上从来没有 extra）。
    let candidate = parse_entries(&format!(
        "{}{}",
        yaml_row("base", "base", 1),
        yaml_row("extra", "extra", 1)
    ))
    .expect("解析候选");
    let gate = state.install_hook(HookPoint::BeforeAdopt);
    let reconciling = {
        let state = state.clone();
        tokio::spawn(async move { state.request_reconcile(candidate).await })
    };
    wait_arrived_bounded(&gate, "程序化候选还没走到采纳之前").await;
    state.clear_hooks();

    // 回写推进版本 ⇒ 候选过期（磁盘基线未变，所以这次回写会落盘）。
    let fiber = state.fiber_of("base").expect("行 base 必须还挂着");
    fiber
        .update_with(json!({ "service": "base", "value": 9 }), false)
        .await
        .expect("运行时回写应当被接受");

    gate.release();
    let outcome = tokio::time::timeout(Duration::from_secs(5), reconciling)
        .await
        .expect("对账超时")
        .expect("对账任务")
        .expect("候选必须真的跑完一轮对账");
    assert!(
        outcome.failures.iter().any(|line| line.contains("过期")),
        "过期候选必须报出来（不得静默）：{outcome:?}"
    );
    assert_eq!(
        state.stale_rebuild_count(),
        0,
        "程序化候选不得从磁盘重建（会替换掉调用方给的内容）"
    );
    assert_eq!(
        service_of(&app, "extra"),
        None,
        "未采纳的候选不得挂上任何东西"
    );
    assert_eq!(
        tree_value(&state, "base"),
        json!(9),
        "树必须停在运行时值（候选的旧树不得覆盖）"
    );
    // 磁盘：这次回写走 `save` 分支（基线仍等于磁盘）⇒ 值 9 必须真的落盘。
    let text = std::fs::read_to_string(&path).expect("读回配置");
    assert!(
        text.contains("value: 9"),
        "save 分支的运行时回写必须落盘：\n{text}"
    );
    assert!(
        !text.contains("value: 1\n"),
        "磁盘不得退回候选构造时的旧值：\n{text}"
    );
    // 回写元数据：候选被丢弃，但那次**成功**的回写必须推进主树/基线。
    let (main, baseline) = accepted_metadata(&state);
    assert_eq!(
        main.iter()
            .find(|entry| entry.id.as_deref() == Some("base"))
            .map(value_of),
        Some(json!(9)),
        "成功落盘的回写必须推进 `main_only`（被丢弃的候选不得把它盖回去）"
    );
    assert!(
        baseline.contains("value: 9"),
        "文件基线必须是回写后的磁盘内容：\n{baseline}"
    );
    // 边界本身：程序化候选不得触发"按磁盘重建"。
    assert_eq!(
        logs.lock()
            .unwrap()
            .iter()
            .filter(|line| line.contains("已按磁盘重建"))
            .count(),
        0,
        "程序化候选不得记录任何重建日志：{:?}",
        logs.lock().unwrap()
    );
    assert!(
        !state.reconciling.load(Ordering::SeqCst),
        "running 标记必须复位"
    );
    assert!(state.pending_entries().is_none(), "合并槽必须干净");
}

// ---------------------------------------------------------------- O5a-4

fn labels_of(entries: &[Entry]) -> Vec<String> {
    entries
        .iter()
        .map(|entry| entry.id.clone().unwrap_or_default())
        .collect()
}

/// 驱动被取消/超时时，**已 take 但未采纳**的候选不得丢，也不得永久占用 running 标记。
///
/// 缺陷：`drive_pending` 把候选 take 出来后就只活在 future 的栈上；`abort`/`timeout`
/// 丢弃 future，候选随之蒸发——配置永远不会被应用，而且没有任何补偿路径。
#[tokio::test]
async fn cancelling_the_driver_keeps_the_candidate_for_the_next_request() {
    let app = App::new();
    let dir = TempDir::new("cancel-keeps-candidate");
    let path = dir.write("cordis.yml", &yaml_row("base", "base", 1));
    let source = ScriptSource::new(Vec::new());
    let state = load_scripted(&app, &path, source.clone()).await;
    assert_eq!(state.mounted_labels(), ["base"]);

    // 候选：base + extra（正在采纳前被取消）。
    let target = format!(
        "{}{}",
        yaml_row("base", "base", 1),
        yaml_row("extra", "extra", 1)
    );
    std::fs::write(&path, &target).expect("写新配置");
    let gate = state.install_hook(HookPoint::BeforeAdopt);
    let driver = {
        let state = state.clone();
        tokio::spawn(async move { state.reload().await })
    };
    gate.wait_arrived().await;
    state.clear_hooks();

    // 取消（模拟 timeout/abort）：future 被丢弃。
    driver.abort();
    assert!(
        tokio::time::timeout(Duration::from_secs(5), driver)
            .await
            .is_ok(),
        "取消必须立刻生效"
    );
    assert!(
        !state.reconciling.load(Ordering::SeqCst),
        "running 标记不得被永久占用"
    );
    let pending = state
        .pending_entries()
        .expect("已 take 但未采纳的候选必须回填合并槽");
    assert_eq!(
        labels_of(&pending),
        ["base", "extra"],
        "回填的必须是那份候选"
    );

    // 补偿执行者（下一次请求/补偿驱动）必须把它采纳掉。
    let outcome = tokio::time::timeout(Duration::from_secs(5), state.drive_pending())
        .await
        .expect("补偿驱动超时")
        .expect("补偿驱动必须真的执行一轮对账");
    assert!(outcome.failures.is_empty(), "{outcome:?}");
    assert_eq!(state.mounted_labels(), ["base", "extra"]);
    assert_eq!(service_of(&app, "extra"), Some(1), "候选必须被采纳");
}

/// 有界等待：反复让出执行权直到条件成立（看门狗判超时，不用 sleep 制造时序）。
async fn wait_until(limit: Duration, mut ready: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + limit;
    loop {
        if ready() {
            return;
        }
        assert!(std::time::Instant::now() < deadline, "等待超时");
        tokio::task::yield_now().await;
    }
}

/// 采纳的"校验 → 整份写树"与回写方的"改树 → 推进版本"必须互斥（TOCTOU）。
///
/// 缺陷：`adopt` 在 `config_gate` 内校验回写版本之后才整份写树，而提交观察者与
/// `mark_self_disposed` **不持**闸门就改树 + 推进版本。于是校验通过后仍可能被一次
/// 并发回写覆盖：内存树退回候选内容，装载记录与磁盘却是新值，而 watcher 的指纹
/// 早已对齐磁盘、不会补偿（用户看到"刚改的值自己变回去了"）。
///
/// 这里的交错是确定的：让采纳停在"校验通过、还没写树"（此时已持闸门），另一个线程
/// 做一次完整回写。修复前回写能穿过去（完成、被随后的整份写树盖掉）；修复后它只能
/// 等在闸门外，采纳结束后才落地——于是回写是**最后**写入树的那一份。
///
/// "被挡住"这个负向观测不能靠墙钟裸猜：慢 CI 上"回写任务还没被调度"与"被闸门挡住"
/// 长得一模一样。所以先用**到达信号**确认回写任务真的进了提交观察者
/// （[`LoaderState::install_commit_entered`]），再断言它既没完成、也没推进回写版本。
///
/// 白盒：候选用 `parse_entries` 直接构造、`request_reconcile` 直接登记——绕过 `reload`
/// 的磁盘/元数据路径（磁盘一个字节都没动），于是这里看的正是采纳内核自己的互斥。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_writeback_cannot_slip_between_the_version_check_and_the_tree_write() {
    let app = App::new();
    let dir = TempDir::new("adopt-gate-toctou");
    let path = dir.write("cordis.yml", &yaml_row("base", "base", 1));
    let source = ScriptSource::new(Vec::new());
    let state = load_scripted(&app, &path, source.clone()).await;
    assert_eq!(service_of(&app, "base"), Some(1));

    // 候选 = base 原样 + extra。直接登记（不碰磁盘），于是下面那次回写仍允许落盘
    // （磁盘基线没变）——"记录与磁盘是新值、树被退回"这个不一致才完整可见。
    let candidate = parse_entries(&format!(
        "{}{}",
        yaml_row("base", "base", 1),
        yaml_row("extra", "extra", 1)
    ))
    .expect("解析候选");

    let gate = state.install_block_hook(HookPoint::InsideAdoptGate);
    let reconciling = {
        let state = state.clone();
        tokio::spawn(async move { state.request_reconcile(candidate).await })
    };
    gate.wait_arrived().await;
    state.clear_hooks();

    // 回写方的"已进入提交观察者"信号 + 当时的回写版本。
    let entered = state.install_commit_entered();
    let version_before = state.writeback_version();

    // 另一个线程做一次运行时回写（值 9）。
    let mut writeback_task = {
        let state = state.clone();
        tokio::spawn(async move { writeback(&state, "base", 9).await })
    };
    // 先等到"真的跑起来了"（有界）：此刻它在观察者里、正要取 `config_gate`。
    tokio::time::timeout(Duration::from_secs(5), entered.acquire())
        .await
        .expect("回写必须进入提交观察者（否则'没完成'只是还没被调度）")
        .expect("到达信号未关闭")
        .forget();
    // 修复后：回写必须被 `config_gate` 挡在外面（采纳结束前不可能"完成"）。
    let early = tokio::time::timeout(Duration::from_secs(1), &mut writeback_task).await;
    let blocked = early.is_err();
    // 此刻仍在闸门内：顺手记下回写版本，断言放到放行之后做——
    // 失败时若还卡着采纳（它阻塞在闸门上），runtime 收尾会等一个永不返回的 worker，
    // 用例会挂住而不是干净地失败。
    let version_while_gated = state.writeback_version();

    gate.release();
    let outcome = tokio::time::timeout(Duration::from_secs(5), reconciling)
        .await
        .expect("采纳超时")
        .expect("采纳任务");
    assert!(outcome.is_some(), "候选必须真的跑完一轮对账");
    assert_eq!(
        version_while_gated, version_before,
        "回写不得在 `config_gate` 外推进回写版本（闸门内那一轮采纳还没写完树）"
    );
    match early {
        // 没被挡住（旧行为）：结果已经在手上，不能再 poll 一次句柄。
        Ok(joined) => joined.expect("回写任务不得 panic"),
        Err(_) => tokio::time::timeout(Duration::from_secs(5), writeback_task)
            .await
            .expect("回写超时")
            .expect("回写任务不得 panic"),
    }

    assert_eq!(
        tree_value(&state, "base"),
        json!(9),
        "候选不得把回写覆盖回去（内存树退回了候选内容）：{:?}",
        state.tree_snapshot()
    );
    assert_eq!(service_of(&app, "base"), Some(9), "运行时值必须保留");
    assert_eq!(service_of(&app, "extra"), Some(1), "候选本身仍要生效");
    let text = std::fs::read_to_string(&path).expect("读回配置");
    assert!(text.contains("value: 9"), "磁盘必须是回写的结果：\n{text}");
    assert!(
        state.plan(&state.tree_snapshot()).is_empty(),
        "装载记录与树必须一致（计划收敛为空）：{:?}",
        state.plan(&state.tree_snapshot())
    );
    assert!(
        blocked,
        "回写必须在 config_gate 外等：采纳的\"校验 + 写树\"与回写的\"改树 + 推进版本\"必须互斥"
    );
}

/// 取消持单飞位的驱动时，槽里**更新的**候选必须被补偿驱动带走（否则丢唤醒）。
///
/// 缺陷：`ReconcileDriver::drop` 在回填被拒（槽里已有更新候选）时只清 `reconciling`、
/// 不重新驱动。那份候选登记时单飞位被占着（请求方只拿到 Coalesced），watcher 的指纹
/// 又已对齐磁盘、不会重试——它就此永久滞留：**用户最后一次编辑永不生效**。
///
/// 白盒：两份候选用 `parse_entries` 直接构造、`request_reconcile` 直接登记——绕过
/// `reload` 的磁盘/元数据路径（磁盘一个字节都没动），好让"单飞位 + 合并槽"这套
/// 取消语义单独被钉住。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelling_the_driver_still_adopts_a_newer_queued_candidate() {
    let app = App::new();
    let dir = TempDir::new("cancel-newer-candidate");
    let path = dir.write("cordis.yml", &yaml_row("base", "base", 1));
    let source = ScriptSource::new(Vec::new());
    let state = load_scripted(&app, &path, source.clone()).await;
    assert_eq!(state.mounted_labels(), ["base"]);

    let five = parse_entries(&format!(
        "{}{}",
        yaml_row("base", "base", 1),
        yaml_row("five", "five", 5)
    ))
    .expect("解析 gen5");
    let seven = parse_entries(&format!(
        "{}{}",
        yaml_row("base", "base", 1),
        yaml_row("seven", "seven", 7)
    ))
    .expect("解析 gen7");

    // T1：gen5 持单飞位，停在对账中（已 take、未采纳）。
    let gate = state.install_hook(HookPoint::BeforeAdopt);
    let driver = {
        let state = state.clone();
        tokio::spawn(async move { state.request_reconcile(five).await })
    };
    gate.wait_arrived().await;
    state.clear_hooks();

    // T2：gen7 只能进合并槽（单飞位被 T1 占着）⇒ Coalesced，没人驱动它。
    assert!(
        state.request_reconcile(seven).await.is_none(),
        "T1 在飞：gen7 只能是 Coalesced"
    );
    assert_eq!(
        state.pending_entries().as_deref().map(labels_of),
        Some(vec!["base".to_string(), "seven".to_string()]),
        "gen7 必须进合并槽"
    );

    // 取消 T1：Drop 回填被 gen7 拒（代次更新）⇒ 必须安排补偿驱动。
    driver.abort();
    assert!(
        tokio::time::timeout(Duration::from_secs(5), driver)
            .await
            .is_ok(),
        "取消必须立刻生效"
    );

    // 修复前：gen7 永久滞留，这里超时（红）。
    wait_until(Duration::from_secs(5), || {
        state.mounted_labels() == ["base", "seven"]
            && !state.reconciling.load(Ordering::SeqCst)
            && state.pending_entries().is_none()
    })
    .await;

    assert_eq!(state.mounted_labels(), ["base", "seven"], "gen7 必须被采纳");
    assert_eq!(
        service_of(&app, "seven"),
        Some(7),
        "采纳的必须是 gen7 的内容"
    );
    assert_eq!(service_of(&app, "five"), None, "gen5 已被 gen7 取代");
    assert!(state.pending_entries().is_none(), "合并槽必须被抽干");
    assert!(
        !state.reconciling.load(Ordering::SeqCst),
        "running 标记不得被永久占用"
    );
    assert_eq!(
        labels_of(&state.tree_snapshot()),
        ["base", "seven"],
        "树必须是 gen7"
    );
}

/// R1：取消时"回填成功后、清位之前"这一窗口里登记的更新候选，同样不得滞留。
///
/// 与被修的丢唤醒（[`cancelling_the_driver_still_adopts_a_newer_queued_candidate`]）同类，
/// 但窗口只有两条指令：`restore_candidate` **成功**（`Some(true)`）之后、
/// `reconciling.store(false)` 之前。此刻另一请求登记的更新候选只进合并槽——它的
/// `drive_pending` 读到单飞位仍是 `true`，于是返回 `None`（Coalesced）；清位之后槽里
/// 就是一份没人驱动的候选，而 watcher 的指纹早已对齐磁盘、不会再来一次：
/// **用户最后一次编辑永不生效**。
///
/// 既有语义必须保留：若清位后槽里仍是"我们刚回填的那一份"，不自动驱动
/// （`cancelled_reconcile_does_not_stick_the_single_flight_flag` 要求由下一次请求接管）。
///
/// 停点用**同步回调**而不是闸门：`Drop` 不能 await，而阻塞一个 runtime worker 会把
/// timer driver 一起卡住（同 runtime 里的 `sleep` 再也不推进）——于是测试会偶发地卡死，
/// 而不是干净地失败。
///
/// 白盒：候选直接构造 [`Candidate`] 并走 `register_candidate`——绕过 `reload` 的
/// 磁盘/元数据路径，只借用它的解析结果（`parse_snapshot`）造出"带回写元数据"的候选。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_candidate_queued_in_the_release_window_is_still_driven() {
    let app = App::new();
    let dir = TempDir::new("release-window-candidate");
    let path = dir.write("cordis.yml", &yaml_row("base", "base", 1));
    let source = ScriptSource::new(Vec::new());
    let state = load_scripted(&app, &path, source.clone()).await;
    assert_eq!(state.mounted_labels(), ["base"]);

    // T1：磁盘上是 C5（base + five），reload 已 take 到候选、停在对账中。
    std::fs::write(
        &path,
        format!(
            "{}{}",
            yaml_row("base", "base", 1),
            yaml_row("five", "five", 5)
        ),
    )
    .expect("写 C5");
    let adopt_gate = state.install_hook(HookPoint::BeforeAdopt);
    let driver = {
        let state = state.clone();
        tokio::spawn(async move { state.reload().await })
    };
    adopt_gate.wait_arrived().await;
    state.clear_hooks();

    // T2 的候选：磁盘改成 C7（base + seven）。走与 `reload` 同一条解析路径，
    // 所以它带回写元数据（`adopt_metadata`），最终 tree / main_only / 基线都会被断言。
    // 磁盘已对齐 C7 ⇒ watcher 的指纹也不会再来一次：滞留就等于"最后一次编辑永不生效"。
    let seven_text = format!(
        "{}{}",
        yaml_row("base", "base", 1),
        yaml_row("seven", "seven", 7)
    );
    std::fs::write(&path, &seven_text).expect("写 C7");
    let parsed_c7 = state
        .parse_snapshot(&FileSnapshot {
            main: seven_text.clone(),
            patches: Vec::new(),
        })
        .expect("解析 C7");
    let t2 = Candidate {
        gen: state.next_candidate_gen(),
        base_writeback: state.writeback(),
        entries: parsed_c7.entries,
        adopt_metadata: true,
        main: parsed_c7.main,
        main_startup: parsed_c7.main_startup,
        row_startup: parsed_c7.row_startup,
        owners: parsed_c7.owners,
        baseline: parsed_c7.baseline,
    };

    // 取消 T1：`ReconcileDriver::drop` 会先回填 C5（成功），再清单飞位——
    // 这两条指令之间就是 R1 的窗口。回调里扮演 T2：把 C7 登记进合并槽，
    // 此刻单飞位仍被 T1 占着，所以跟进来的驱动请求只会拿到 `None`。
    let window = Arc::new(Mutex::new(None));
    {
        let state = state.clone();
        let queued_t2 = Arc::new(Mutex::new(Some(t2)));
        let window = window.clone();
        let hook_state = state.clone();
        state.install_release_hook(Arc::new(move || {
            let Some(candidate) = queued_t2.lock().unwrap().take() else {
                return;
            };
            let registered = hook_state.register_candidate(candidate);
            let flag_held = hook_state.reconciling.load(Ordering::SeqCst);
            let queued = hook_state.pending_entries().as_deref().map(labels_of);
            *window.lock().unwrap() = Some((registered, flag_held, queued));
        }));
    }
    driver.abort();
    assert!(
        tokio::time::timeout(Duration::from_secs(5), driver)
            .await
            .is_ok(),
        "取消必须立刻生效"
    );
    state.clear_hooks();

    // 窗口里发生的事：C7 进了槽、单飞位还没放开。
    let (registered, flag_held, queued) = window
        .lock()
        .unwrap()
        .take()
        .expect("R1 的窗口必须经过一次");
    assert!(registered, "T2 的候选必须能进合并槽");
    assert!(
        flag_held,
        "窗口里单飞位仍被 T1 占着：T2 的 drive_pending 只会返回 None（Coalesced）"
    );
    assert_eq!(
        queued,
        Some(vec!["base".to_string(), "seven".to_string()]),
        "窗口里槽里必须是 C7（它顶掉了 T1 刚回填的 C5）"
    );

    // 修复前：C7 永久滞留（没人驱动它），这里超时（红）。
    wait_until(Duration::from_secs(5), || {
        state.mounted_labels() == ["base", "seven"]
            && !state.reconciling.load(Ordering::SeqCst)
            && state.pending_entries().is_none()
    })
    .await;

    // 最终状态必须是 C7：树、装载表、回写元数据三处一致。
    assert_eq!(state.mounted_labels(), ["base", "seven"], "C7 必须被采纳");
    assert_eq!(service_of(&app, "seven"), Some(7), "采纳的必须是 C7 的内容");
    assert_eq!(service_of(&app, "five"), None, "C5 已被 C7 取代");
    assert_eq!(
        labels_of(&state.tree_snapshot()),
        ["base", "seven"],
        "树必须是 C7"
    );
    let (main, baseline) = accepted_metadata(&state);
    assert_eq!(labels_of(&main), ["base", "seven"], "回写主树必须是 C7");
    assert!(
        baseline.contains("id: seven") && !baseline.contains("id: five"),
        "文件基线必须是 C7：\n{baseline}"
    );
    assert!(state.pending_entries().is_none(), "合并槽必须被抽干");
    assert!(!state.reconciling.load(Ordering::SeqCst), "单飞位必须复位");
}

/// `main_only` 的写入必须在 `config_gate` 之内：采纳不会插进"改主树 → 落盘"之间。
///
/// 缺陷：提交观察者与插件自我卸载在 `config_gate` **之外**改 `main_only`，与字段注释
/// 声称的串行化不符——一次在飞的对账采纳可以把刚写的这一行整份盖回去。
#[tokio::test]
async fn main_only_writes_happen_under_config_gate() {
    let app = App::new();
    let dir = TempDir::new("gate-discipline");
    let path = dir.write(
        "cordis.yml",
        concat!(
            "- id: single\n  name: fake\n  config:\n    service: svc\n    value: 1\n",
            "- id: suicidal\n  name: suicidal\n  config:\n    service: other\n    value: 2\n",
        ),
    );
    let source = ScriptSource::new(Vec::new());
    let state = load_scripted(&app, &path, source.clone()).await;
    assert_eq!(state.mounted_labels(), ["single", "suicidal"]);

    // ① 运行时回写（提交观察者改 tree + main_only + 落盘）。
    writeback(&state, "single", 5).await;
    // 计数为 0 还不够：**读回磁盘**确认这条路径真的写过。否则"两次写入压根没发生"
    // 也会让计数是 0——那正是这条用例最容易假绿的地方。
    let text = std::fs::read_to_string(&path).expect("读回配置");
    assert!(
        text.contains("value: 5"),
        "提交观察者必须真的落盘：\n{text}"
    );
    // ② 插件自我卸载（改 tree + main_only + 落盘）。
    source.stashed_fiber().dispose().await;
    let text = std::fs::read_to_string(&path).expect("读回配置");
    assert!(
        text.contains("disabled: true"),
        "自我卸载必须真的把 disabled 写进配置：\n{text}"
    );
    assert!(
        text.contains("value: 5"),
        "自我卸载的落盘必须用的是**已更新**的主树（不能把回写的值盖回去）：\n{text}"
    );

    assert_eq!(
        state.ungated_main_write_count(),
        0,
        "main_only 的写入必须在 config_gate 之内（锁序纪律）"
    );
}

// ---------------------------------------------------------------- O7 复核：预检报告只描述当前树

/// 候选被 `adopt` 拒绝时，`report.preflight` 必须仍是**上一份已采纳**结论。
///
/// 缺陷：`reconcile_attempt` 在 `adopt` 之前就把这一轮的预检结论写进报告槽。
/// 候选随后被判过期拒绝，树没换，可报告里的 `preflight` 已经在描述那份被丢弃的
/// 候选——读者会看到 `preflight.verdict=incomplete`，而 `mounted`/`disabled` 说的
/// 却是另一棵树，两者对不上，且没有任何字段提示"这是两棵树"。
#[tokio::test]
async fn a_rejected_candidate_does_not_overwrite_the_adopted_preflight_report() {
    let app = App::new();
    let dir = TempDir::new("rejected-candidate-preflight");
    let path = dir.write("cordis.yml", &yaml_row("single", "svc", 1));
    // 初装预检：有声明 ⇒ `checked`（这将是"已采纳"的那一份结论）。
    let source = ScriptSource::new(vec![Ok(Some(plan_spec("fake")))]);
    let state = load_scripted(&app, &path, source.clone()).await;
    let adopted_preflight = state.report()["preflight"].clone();
    assert_eq!(
        adopted_preflight["verdict"],
        json!("checked"),
        "初装结论必须是 checked，否则这条用例观察不到「覆盖」：{adopted_preflight}"
    );

    // 候选：预检会得到**另一种**结论（`Ok(None)` ⇒ `incomplete`），但回写版本过期
    // ⇒ `adopt` 拒绝。候选基于一棵过时的树，绝不会成为当前树。
    let mut candidate = Candidate::local(state.tree_snapshot());
    candidate.gen = 1;
    candidate.base_writeback = state.writeback_version() + 1;

    let attempt = state.reconcile_attempt(&candidate).await;
    match &attempt.rejection {
        Some(AdoptRejection::StaleWriteback(_)) => {}
        Some(other) => panic!("候选应当以回写版本过期被拒，实际：{}", other.message()),
        None => panic!("候选必须被拒，实际被采纳：{:?}", attempt.outcome),
    }
    assert_eq!(
        source.plan_calls(),
        2,
        "候选这一轮确实预检过（有东西可被误写）"
    );

    let report = state.report();
    assert_eq!(
        report["preflight"], adopted_preflight,
        "被拒候选不得覆盖预检报告：{report}"
    );
    assert_eq!(
        report["mounted"],
        json!(1),
        "树没换 ⇒ mounted 仍是已采纳的那一份：{report}"
    );
}

/// 对账里**预检本身失败**时，`report.failed` 必须看得见这条失败。
///
/// 缺陷：预检失败的早退分支只写日志。`report.preflight` 保持上一份结论是对的
/// （这一轮什么都没检查），但 `report.failed` 也保持"干净"，于是"上一轮对账不成功"
/// 这件事在报告里完全消失——而 `adopt` 被拒的分支一直是写 `last` 并刷新的。
#[tokio::test]
async fn reconcile_preflight_failure_is_visible_in_the_report() {
    let app = App::new();
    let dir = TempDir::new("preflight-failure-report");
    let path = dir.write("cordis.yml", &yaml_row("single", "svc", 1));
    // ① 初装预检 Ok、② 对账预检 Err（strict_sidecars ⇒ 升级成失败）。
    let source = ScriptSource::new(vec![
        Ok(Some(plan_spec("fake"))),
        Err("sidecar manifest 不可用".into()),
    ]);
    let state = load_scripted(&app, &path, source.clone()).await;
    let adopted_preflight = state.report()["preflight"].clone();

    let outcome = state.sync().await;
    assert!(
        outcome
            .failures
            .iter()
            .any(|line| line.contains("依赖预检失败")),
        "对账产物必须点名预检失败：{outcome:?}"
    );
    assert_eq!(source.plan_calls(), 2, "对账确实重跑了一次预检");

    let report = state.report();
    let failed = report["failed"]
        .as_array()
        .unwrap_or_else(|| panic!("failed 必须是数组：{report}"));
    assert!(
        failed.iter().any(|line| line
            .as_str()
            .is_some_and(|text| text.contains("依赖预检失败"))),
        "依赖预检失败必须进 report.failed：{report}"
    );
    assert_eq!(
        report["preflight"], adopted_preflight,
        "预检失败的一轮什么都没检查，不得覆盖上一份结论：{report}"
    );
    assert_eq!(report["mounted"], json!(1), "{report}");

    // `state.report()` 是内部视图：它变了不代表**发布**了。失败必须经
    // `loader.report` 这个读口真的到得了读者手上。
    let published = published_report(&app.root);
    assert!(
        published["failed"].as_array().is_some_and(|lines| lines
            .iter()
            .any(|line| line.as_str().is_some_and(|t| t.contains("依赖预检失败")))),
        "预检失败必须真的被 publish_report 发布出去：{published}"
    );
}

/// **已发布**的 `loader.report`（`ctx.set` / `provide` 的那一份，不是 `state.report()`
/// 的内部视图）——用来钉住"发布"这一步，而不只是"算对了"。
fn published_report(ctx: &Ctx) -> Value {
    let value = ctx
        .require("loader.report")
        .expect("loader.report 必须已发布");
    value
        .downcast::<ExternValue>()
        .expect("ExternValue")
        .0
        .clone()
}

/// `reload_one` 的依赖预检失败也必须进 `last` 并**发布**出去。
///
/// 缺陷：预检失败的早退分支直接 `Err`，既不写 `last` 也不 `publish_report`。
/// 于是"热重载被预检挡住"这件事在 `report.failed` 里完全看不到——HMR 每次重试
/// 都只留下一条日志，而 `loader.report` 看起来一切正常。对账路径早就是
/// "写 `last` + 刷新报告"的同构做法。
#[tokio::test]
async fn reload_one_preflight_failure_is_visible_in_the_report() {
    let app = App::new();
    let dir = TempDir::new("reload-one-preflight-report");
    let path = dir.write("cordis.yml", &yaml_row("single", "svc", 1));
    // ① 初装预检 Ok、② reload 的预检 Err（strict_sidecars ⇒ 升级成失败）。
    let source = ScriptSource::new(vec![
        Ok(Some(plan_spec("fake"))),
        Err("sidecar manifest 被截断".into()),
    ]);
    let state = load_scripted(&app, &path, source.clone()).await;
    let adopted_preflight = state.report()["preflight"].clone();
    assert_eq!(
        adopted_preflight["verdict"],
        json!("checked"),
        "初装结论必须是 checked：{adopted_preflight}"
    );

    let error = reload_one(&app.root, &state, "single")
        .await
        .expect_err("预检失败必须让 reload 失败");
    assert!(
        error.contains("依赖预检失败（reload 未执行）"),
        "失败文案保持既有前缀：{error}"
    );
    assert_eq!(source.plan_calls(), 2, "reload 确实重跑了一次预检");

    let last = state.last_outcome().expect("预检失败也必须写 last");
    assert!(
        last.failures
            .iter()
            .any(|line| line.contains("依赖预检失败")),
        "last 必须点名预检失败：{last:?}"
    );

    let published = published_report(&app.root);
    assert!(
        published["failed"].as_array().is_some_and(|lines| lines
            .iter()
            .any(|line| line.as_str().is_some_and(|t| t.contains("依赖预检失败")))),
        "热重载的预检失败必须进已发布的 report.failed：{published}"
    );
    assert_eq!(
        published["preflight"], adopted_preflight,
        "预检失败的一轮什么都没检查，不得覆盖上一份结论：{published}"
    );
}

/// 「快照 → 预检 → 记录报告」必须整体落在 `mutation` 临界区内。
///
/// 缺陷：这三步在锁外做。并发的对账可以在"取快照"与"写报告"之间 `adopt` 新树 ⇒
/// 报告里的 `preflight` 描述旧树、`mounted` 描述新树：一份报告混着两棵树，
/// 而没有任何字段提示这一点。
///
/// 复现方式：让另一个持有者先占住 `mutation`（模拟"一次在飞的对账"），
/// 此刻 `reload_one` 连预检都不许跑——否则它看到的就不是"当前树"。
#[tokio::test]
async fn reload_one_preflights_inside_the_mutation_critical_section() {
    let app = App::new();
    let dir = TempDir::new("reload-one-critical-section");
    let path = dir.write("cordis.yml", &yaml_row("single", "svc", 1));
    let source = ScriptSource::new(vec![
        Ok(Some(plan_spec("fake"))),
        Ok(Some(plan_spec("fake"))),
    ]);
    let state = load_scripted(&app, &path, source.clone()).await;
    assert_eq!(source.plan_calls(), 1, "初装预检一次");

    let guard = state.mutation.lock().await;
    let reloading = {
        let state = state.clone();
        let root = app.root.clone();
        tokio::spawn(async move { reload_one(&root, &state, "single").await })
    };
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        source.plan_calls(),
        1,
        "快照/预检不得跑在 mutation 临界区之外（否则并发 adopt 会让报告与实例分属两棵树）"
    );
    drop(guard);

    // 拿到锁之后才重跑预检。这个假源的 artifact 只是 `<fake>`，所以后面的暂存会失败
    // ——本条用例只关心预检发生在哪一侧。
    let _ = tokio::time::timeout(Duration::from_secs(5), reloading)
        .await
        .expect("reload 超时")
        .expect("reload 任务");
    assert_eq!(source.plan_calls(), 2, "临界区内重跑了一次预检");
}
