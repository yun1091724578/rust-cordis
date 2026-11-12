//! P1-1：`LoaderConfig.dependencyPlan` —— **配置级**依赖预检。
//!
//! 这里测的是装载层接线，不是求解语义（那在 `crates/host/src/plan.rs` 的单测里）。
//! 最关键的判据：预检失败必须发生在**任何插件被加载或应用之前**，所以假插件源
//! 的两枚计数器（`load` / `apply`）都要保持 0。
//!
//! 依赖计划是**显式配置**，不是插件自述的推导结果：插件里的 `provision` /
//! `inject` 不会自动变成 `PlanEntry`。最后一条用例把这一点钉住。

use cordis_core::{plugin_fn, with_provision, App, Config, Ctx, Inject, Plugin};
use cordis_host::loader::{
    load_with_source, DiskPluginSource, InterpolationMode, LoadedFile, LoaderConfig, PluginSource,
};
use cordis_host::patch::Entry;
use cordis_host::plan::PlanEntrySpec;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

// ---------------------------------------------------------------- 夹具

fn spec(
    id: &str,
    version: &str,
    provides: &[(&str, &str)],
    requires: &[(&str, &str)],
) -> PlanEntrySpec {
    PlanEntrySpec {
        id: id.to_string(),
        version: version.to_string(),
        provides: provides
            .iter()
            .map(|(key, version)| (key.to_string(), version.to_string()))
            .collect(),
        requires: requires
            .iter()
            .map(|(key, req)| (key.to_string(), req.to_string()))
            .collect(),
    }
}

#[derive(Clone, Default)]
struct Counters {
    loads: Arc<AtomicUsize>,
    applies: Arc<AtomicUsize>,
    plans: Arc<AtomicUsize>,
}

impl Counters {
    fn new() -> Self {
        Self::default()
    }

    fn loads(&self) -> usize {
        self.loads.load(Ordering::SeqCst)
    }

    fn applies(&self) -> usize {
        self.applies.load(Ordering::SeqCst)
    }

    fn plans(&self) -> usize {
        self.plans.load(Ordering::SeqCst)
    }

    /// 假源：每次 `load` 计数 +1；返回的插件在 `apply` 时计数 +1。
    fn source(&self) -> Arc<dyn PluginSource> {
        self.source_with(&[])
    }

    /// 同上，但让插件**自述** `provision`（用来证明它不参与显式计划求解）。
    fn source_with(&self, provision: &[&str]) -> Arc<dyn PluginSource> {
        Arc::new(CountingSource {
            counters: self.clone(),
            provision: provision.iter().map(|key| key.to_string()).collect(),
            plan: BTreeMap::new(),
            plan_error: None,
        })
    }

    /// 假源 + 按行名返回的 manifest 计划（模拟 `DiskPluginSource::plan_entry`）。
    fn source_with_plan(&self, plan: Vec<(&str, PlanEntrySpec)>) -> Arc<dyn PluginSource> {
        Arc::new(CountingSource {
            counters: self.clone(),
            provision: Vec::new(),
            plan: plan
                .into_iter()
                .map(|(name, spec)| (name.to_string(), spec))
                .collect(),
            plan_error: None,
        })
    }

    /// 假源：`plan_entry` 总是报错（模拟损坏的 sidecar manifest）。
    fn source_with_plan_error(&self, error: &str) -> Arc<dyn PluginSource> {
        Arc::new(CountingSource {
            counters: self.clone(),
            provision: Vec::new(),
            plan: BTreeMap::new(),
            plan_error: Some(error.to_string()),
        })
    }
}

struct CountingSource {
    counters: Counters,
    provision: Vec<String>,
    plan: BTreeMap<String, PlanEntrySpec>,
    plan_error: Option<String>,
}

impl PluginSource for CountingSource {
    fn load(
        &self,
        entry: &Entry,
        _search: &[PathBuf],
        _revision: u64,
    ) -> Result<LoadedFile, String> {
        self.counters.loads.fetch_add(1, Ordering::SeqCst);
        let applies = self.counters.applies.clone();
        let plugin: Arc<dyn Plugin> = plugin_fn(
            "counting",
            Inject::default(),
            move |_ctx: Ctx, _config: Config| {
                let applies = applies.clone();
                Box::pin(async move {
                    applies.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
            },
        );
        let plugin = with_provision(plugin, self.provision.clone());
        let path = PathBuf::from(format!("<fake:{}>", entry.name));
        Ok(LoadedFile {
            plugin,
            source: path.clone(),
            staged: path,
            fingerprint: None,
        })
    }

    fn plan_entry(
        &self,
        entry: &Entry,
        _search: &[PathBuf],
    ) -> Result<Option<PlanEntrySpec>, String> {
        self.counters.plans.fetch_add(1, Ordering::SeqCst);
        if let Some(error) = &self.plan_error {
            return Err(error.clone());
        }
        Ok(self.plan.get(&entry.name).cloned())
    }
}

fn config_for(path: &Path, dependency_plan: Vec<PlanEntrySpec>) -> LoaderConfig {
    LoaderConfig {
        path: path.to_string_lossy().into_owned(),
        patches: Vec::new(),
        plugin_dir: None,
        strict: true,
        strict_cycles: false,
        strict_sidecars: true,
        hmr: None,
        js_evaluator: None,
        interpolation: InterpolationMode::Builtin,
        dispatch_policy: None,
        host_limits: None,
        dependency_plan,
    }
}

fn write_config(tag: &str, yaml: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "cordis-dependency-plan-{}-{tag}.yml",
        std::process::id()
    ));
    std::fs::write(&path, yaml).expect("写配置");
    path
}

// ---------------------------------------------------------------- 接线

/// 合法计划：求解通过，唯一的配置行照常挂载并 `apply` 一次。
#[tokio::test]
async fn valid_plan_loads_the_configured_row() {
    let counters = Counters::new();
    let app = App::new();
    let path = write_config("valid", "- id: prod\n  name: prod\n");
    let plan = vec![spec("prod", "1.0.0", &[("svc", "1.0.0")], &[("svc", "^1")])];

    let state = load_with_source(app.root.clone(), config_for(&path, plan), counters.source())
        .await
        .expect("合法计划必须放行");

    assert_eq!(state.mounted_labels(), vec!["prod"]);
    assert_eq!(counters.loads(), 1, "合法计划只加载这一行");
    assert_eq!(counters.applies(), 1, "合法计划照常应用这一行");
}

/// 无效 spec（版本解析失败）与缺失 range 都必须在**加载/应用之前**整体失败。
#[tokio::test]
async fn invalid_or_missing_plan_fails_before_any_load_or_apply() {
    // ① 版本字符串不是 semver：错误要点名下标与 id。
    let counters = Counters::new();
    let app = App::new();
    let path = write_config("invalid-spec", "- id: prod\n  name: prod\n");
    let error = load_with_source(
        app.root.clone(),
        config_for(&path, vec![spec("broken", "not-semver", &[], &[])]),
        counters.source(),
    )
    .await
    .expect_err("非法版本必须让装载失败");
    let message = error.to_string();
    assert!(
        message.contains("dependencyPlan[0]") && message.contains("broken"),
        "错误必须点名下标与 id：{message}"
    );
    assert_eq!(counters.loads(), 0, "预检失败时不得调用 source.load");
    assert_eq!(counters.applies(), 0, "预检失败时不得应用任何插件");

    // ② 没有提供者的 range：一次性报出全部 findings。
    let counters = Counters::new();
    let app = App::new();
    let path = write_config("missing-range", "- id: prod\n  name: prod\n");
    let error = load_with_source(
        app.root.clone(),
        config_for(&path, vec![spec("cons", "1.0.0", &[], &[("svc", "^2")])]),
        counters.source(),
    )
    .await
    .expect_err("缺失依赖必须让装载失败");
    let message = error.to_string();
    assert!(
        message.contains("依赖计划预检失败") && message.contains("没有提供者"),
        "错误必须说明预检失败与缺失原因：{message}"
    );
    assert_eq!(counters.loads(), 0, "预检失败时不得调用 source.load");
    assert_eq!(counters.applies(), 0, "预检失败时不得应用任何插件");
}

/// 同一服务键同一版本被两个插件提供 ⇒ 歧义；仍要在挂载前失败。
#[tokio::test]
async fn ambiguous_duplicate_provider_fails_before_mount() {
    let counters = Counters::new();
    let app = App::new();
    let path = write_config("ambiguous", "- id: prod\n  name: prod\n");
    let plan = vec![
        spec("a", "1.0.0", &[("svc", "1.0.0")], &[]),
        spec("b", "1.0.0", &[("svc", "1.0.0")], &[]),
    ];

    let error = load_with_source(app.root.clone(), config_for(&path, plan), counters.source())
        .await
        .expect_err("同版本多提供者必须让装载失败");
    let message = error.to_string();
    assert!(
        message.contains("多个插件提供") && message.contains("svc"),
        "错误必须指出歧义的服务键：{message}"
    );
    assert_eq!(counters.loads(), 0, "预检失败时不得调用 source.load");
    assert_eq!(counters.applies(), 0, "预检失败时不得应用任何插件");
}

/// 计划是**配置级**声明：插件自述的 `provision` 不会替它补上提供者。
///
/// 假源返回的插件声明提供 `svc`；显式计划里却没有任何 `svc` 提供者。
/// 预检仍然失败——因为求解器只看 `dependencyPlan`，不看插件描述。
#[tokio::test]
async fn plan_is_config_level_and_does_not_read_plugin_self_description() {
    let counters = Counters::new();
    let app = App::new();
    let path = write_config("self-description", "- id: prod\n  name: prod\n");
    let plan = vec![spec("cons", "1.0.0", &[], &[("svc", "^9")])];

    let error = load_with_source(
        app.root.clone(),
        config_for(&path, plan),
        counters.source_with(&["svc"]),
    )
    .await
    .expect_err("插件自述不能当作显式计划的提供者");
    let message = error.to_string();
    assert!(
        message.contains("没有提供者") && message.contains("svc"),
        "错误必须说明计划里没有提供者：{message}"
    );
    assert_eq!(
        counters.loads(),
        0,
        "插件自述不参与求解，故连 load 都不该发生"
    );
    assert_eq!(counters.applies(), 0);
}

// ---------------------------------------------------------------- manifest 采集

/// 来源清单收集到的计划：合法链 ⇒ 两行照常加载/应用，`plan_entry` 被逐行调用。
#[tokio::test]
async fn manifest_plan_is_collected_and_allows_the_rows_to_load() {
    let counters = Counters::new();
    let app = App::new();
    let path = write_config(
        "manifest-valid",
        "- id: provider\n  name: provider\n- id: consumer\n  name: consumer\n",
    );
    let source = counters.source_with_plan(vec![
        (
            "provider",
            spec("provider.plugin", "1.0.0", &[("svc", "1.2.0")], &[]),
        ),
        (
            "consumer",
            spec("consumer.plugin", "2.0.0", &[], &[("svc", "^1.1")]),
        ),
    ]);

    let state = load_with_source(app.root.clone(), config_for(&path, Vec::new()), source)
        .await
        .expect("manifest 计划可满足时必须放行");

    assert_eq!(state.mounted_labels(), vec!["consumer", "provider"]);
    assert_eq!(counters.plans(), 2, "每个静态启用行都要问一次 plan_entry");
    assert_eq!(counters.loads(), 2, "预检通过后两行都要加载");
    assert_eq!(counters.applies(), 2, "预检通过后两行都要应用");
}

/// 同一插件挂多行是合法配置：相同 manifest 计划只应参与一次求解。
#[tokio::test]
async fn duplicate_plugin_instances_share_one_plan_entry() {
    let counters = Counters::new();
    let app = App::new();
    let path = write_config(
        "manifest-duplicate",
        "- id: one\n  name: provider\n- id: two\n  name: provider\n",
    );
    let source = counters.source_with_plan(vec![(
        "provider",
        spec("provider.plugin", "1.0.0", &[("svc", "1.0.0")], &[]),
    )]);

    let state = load_with_source(app.root.clone(), config_for(&path, Vec::new()), source)
        .await
        .expect("同一插件的两个实例不得被当作重复声明");
    assert_eq!(state.mounted_labels(), vec!["one", "two"]);
    assert_eq!(counters.loads(), 2);
    assert_eq!(counters.applies(), 2);
}

/// `disabled: !!js` 的求值需要上下文；预检不能把可能启用的提供者误判为缺失。
#[tokio::test]
async fn dynamic_disabled_provider_still_participates_in_preflight() {
    let counters = Counters::new();
    let app = App::new();
    let path = write_config(
        "manifest-dynamic-disabled",
        "- id: provider\n  name: provider\n  disabled: !!js \"1 == 2\"\n- id: consumer\n  name: consumer\n",
    );
    let source = counters.source_with_plan(vec![
        (
            "provider",
            spec("provider.plugin", "1.0.0", &[("svc", "1.0.0")], &[]),
        ),
        (
            "consumer",
            spec("consumer.plugin", "2.0.0", &[], &[("svc", "^1")]),
        ),
    ]);

    let state = load_with_source(app.root.clone(), config_for(&path, Vec::new()), source)
        .await
        .expect("运行时为 false 的动态 disabled 提供者必须参与预检");
    assert_eq!(state.mounted_labels(), vec!["consumer", "provider"]);
}

/// 启停若让依赖预检失败，必须回滚树并返回错误，不能报告成功。
#[tokio::test]
async fn set_disabled_rolls_back_when_preflight_rejects_the_new_tree() {
    let counters = Counters::new();
    let app = App::new();
    let path = write_config(
        "set-disabled-preflight",
        "- id: provider\n  name: provider\n- id: consumer\n  name: consumer\n",
    );
    let source = counters.source_with_plan(vec![
        (
            "provider",
            spec("provider.plugin", "1.0.0", &[("svc", "1.0.0")], &[]),
        ),
        (
            "consumer",
            spec("consumer.plugin", "2.0.0", &[], &[("svc", "^1")]),
        ),
    ]);
    let state = load_with_source(app.root.clone(), config_for(&path, Vec::new()), source)
        .await
        .expect("初始计划可满足");

    let error = state
        .set_disabled("provider", Some(true))
        .await
        .expect_err("禁用唯一提供者必须失败");
    assert!(
        error.contains("依赖预检失败") || error.contains("没有提供者"),
        "{error}"
    );
    assert!(
        !state
            .tree_snapshot()
            .iter()
            .find(|entry| entry.id.as_deref() == Some("provider"))
            .expect("provider 行")
            .disabled,
        "失败的启停必须回滚树"
    );
    assert_eq!(state.mounted_labels(), vec!["consumer", "provider"]);
}

/// 组行只下钻到子行；静态禁用行不参与收集（它的坏计划不能拖垮整次装载）。
#[tokio::test]
async fn manifest_plan_walks_groups_and_skips_disabled_rows() {
    let counters = Counters::new();
    let app = App::new();
    let path = write_config(
        "manifest-group",
        "- id: grp\n  group: true\n  config:\n    - id: provider\n      name: provider\n    - id: dormant\n      name: dormant\n      disabled: true\n    - id: consumer\n      name: consumer\n",
    );
    let source = counters.source_with_plan(vec![
        (
            "provider",
            spec("provider.plugin", "1.0.0", &[("svc", "1.0.0")], &[]),
        ),
        (
            "consumer",
            spec("consumer.plugin", "1.0.0", &[], &[("svc", "^1")]),
        ),
        (
            "dormant",
            spec("dormant.plugin", "1.0.0", &[], &[("missing", "^1")]),
        ),
    ]);

    let state = load_with_source(app.root.clone(), config_for(&path, Vec::new()), source)
        .await
        .expect("禁用行的坏计划不得参与预检");

    assert_eq!(state.mounted_labels(), vec!["consumer", "provider"]);
    assert_eq!(counters.plans(), 2, "组行不计数，禁用行不询问");
    assert_eq!(counters.loads(), 2);
    assert_eq!(counters.applies(), 2);
}

/// manifest 计划缺失提供者 ⇒ 在任何 `source.load` / `apply` 之前整体失败。
#[tokio::test]
async fn manifest_plan_missing_range_fails_before_any_load_or_apply() {
    let counters = Counters::new();
    let app = App::new();
    let path = write_config("manifest-missing", "- id: consumer\n  name: consumer\n");
    let source = counters.source_with_plan(vec![(
        "consumer",
        spec("consumer.plugin", "2.0.0", &[], &[("svc", "^2")]),
    )]);

    let error = load_with_source(app.root.clone(), config_for(&path, Vec::new()), source)
        .await
        .expect_err("manifest 计划缺失依赖必须让装载失败");
    let message = error.to_string();
    assert!(
        message.contains("依赖计划预检失败")
            && message.contains("没有提供者")
            && message.contains("consumer.plugin"),
        "错误必须说明预检失败、缺失原因与插件 id：{message}"
    );
    assert_eq!(counters.loads(), 0, "预检失败时不得调用 source.load");
    assert_eq!(counters.applies(), 0, "预检失败时不得应用任何插件");
}

/// 损坏的 sidecar（`plan_entry` 报错）同样必须在加载/应用之前失败，并点名行与插件。
#[tokio::test]
async fn malformed_source_plan_fails_before_any_load_or_apply() {
    let counters = Counters::new();
    let app = App::new();
    let path = write_config("manifest-broken", "- id: broken\n  name: broken\n");
    let source = counters.source_with_plan_error("sidecar manifest 解析失败");

    let error = load_with_source(app.root.clone(), config_for(&path, Vec::new()), source)
        .await
        .expect_err("损坏的 manifest 必须让装载失败");
    let message = error.to_string();
    assert!(
        message.contains("第 0 行")
            && message.contains("broken")
            && message.contains("sidecar manifest 解析失败"),
        "错误必须点名行、插件与原因：{message}"
    );
    assert_eq!(counters.loads(), 0, "manifest 损坏时不得调用 source.load");
    assert_eq!(counters.applies(), 0, "manifest 损坏时不得应用任何插件");
}

/// `strictSidecars: false`（默认）：损坏的 sidecar 视为"没有计划"，
/// 整次 manifest 预检保守跳过，逐行装载照常进行。
#[tokio::test]
async fn malformed_source_plan_is_skipped_when_not_strict() {
    let counters = Counters::new();
    let app = App::new();
    let path = write_config("manifest-broken-lenient", "- id: broken\n  name: broken\n");
    let source = counters.source_with_plan_error("sidecar manifest 解析失败");
    let config = LoaderConfig {
        strict_sidecars: false,
        ..config_for(&path, Vec::new())
    };

    let state = load_with_source(app.root.clone(), config, source)
        .await
        .expect("strictSidecars=false 时损坏的 sidecar 不得拖垮装载");
    assert_eq!(state.mounted_labels(), vec!["broken"]);
    assert_eq!(counters.plans(), 1, "每行仍会被询问一次");
    assert_eq!(counters.loads(), 1, "预检跳过后照常加载");
    assert_eq!(counters.applies(), 1, "预检跳过后照常应用");
}

/// 显式 `dependencyPlan` 是**覆盖**：manifest 计划即使会失败也不能被合并进来，
/// 连 `plan_entry` 都不该被调用。
#[tokio::test]
async fn explicit_dependency_plan_overrides_the_manifest_plan() {
    let counters = Counters::new();
    let app = App::new();
    let path = write_config("override", "- id: prod\n  name: prod\n");
    let source =
        counters.source_with_plan(vec![("prod", spec("prod", "1.0.0", &[], &[("svc", "^9")]))]);
    let explicit = vec![spec("prod", "1.0.0", &[("svc", "1.0.0")], &[])];

    let state = load_with_source(app.root.clone(), config_for(&path, explicit), source)
        .await
        .expect("显式计划必须覆盖 manifest 计划");
    assert_eq!(state.mounted_labels(), vec!["prod"]);
    assert_eq!(counters.plans(), 0, "显式计划非空时不得读取 manifest 计划");
    assert_eq!(counters.loads(), 1);
    assert_eq!(counters.applies(), 1);
}

/// 没有任何 manifest 声明 ⇒ 不产生额外预检，既有装载行为不变。
#[tokio::test]
async fn empty_manifest_plan_keeps_existing_loader_behavior() {
    let counters = Counters::new();
    let app = App::new();
    let path = write_config("manifest-empty", "- id: prod\n  name: prod\n");

    let state = load_with_source(
        app.root.clone(),
        config_for(&path, Vec::new()),
        counters.source(),
    )
    .await
    .expect("没有 manifest 计划时照常装载");
    assert_eq!(state.mounted_labels(), vec!["prod"]);
    assert_eq!(counters.plans(), 1, "每行仍会被询问一次");
    assert_eq!(counters.loads(), 1);
    assert_eq!(counters.applies(), 1);
}

// ---------------------------------------------------------------- DiskPluginSource

fn plugin_fixture(tag: &str, manifest: Option<serde_json::Value>) -> (PathBuf, PathBuf, PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "cordis-dependency-plan-{}-{tag}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("创建临时目录");
    let artifact = dir.join("plugin.bin");
    std::fs::write(&artifact, b"not a dynamic library").expect("写假 artifact");
    if let Some(manifest) = manifest {
        std::fs::write(
            cordis_host::source::manifest_path(&artifact),
            serde_json::to_string_pretty(&manifest).expect("manifest JSON"),
        )
        .expect("写 sidecar manifest");
    }
    let config = dir.join("cordis.yml");
    (dir, artifact, config)
}

fn manifest_value(id: &str, version: &str) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "version": version,
        "source_revision": "tree:test",
        "abi_version": 1,
        "sha256": "0".repeat(64),
    })
}

/// sidecar manifest 的 `provides` / `requires` 原样收进计划，再按条目标注解析。
#[test]
fn disk_source_manifest_with_ranges_parses_to_a_plan() {
    let mut manifest = manifest_value("example.plugin", "1.2.3");
    manifest["provides"] = serde_json::json!({ "svc": "1.2.3" });
    manifest["requires"] = serde_json::json!({ "other": "^2" });
    let (_dir, artifact, _config) = plugin_fixture("manifest-ranges", Some(manifest));
    let entry = Entry {
        name: artifact.to_string_lossy().into_owned(),
        ..Entry::default()
    };

    let spec = DiskPluginSource
        .plan_entry(&entry, &[])
        .expect("读取 sidecar 不得失败")
        .expect("有 sidecar 就必须产生计划");
    assert_eq!(spec.id, "example.plugin");
    assert_eq!(spec.version, "1.2.3");
    assert_eq!(spec.provides["svc"], "1.2.3");
    assert_eq!(spec.requires["other"], "^2");

    let plan = spec.into_entry().expect("合法 semver 必须能转成计划");
    assert_eq!(plan.version.to_string(), "1.2.3");
    assert_eq!(plan.provides["svc"].to_string(), "1.2.3");
    assert_eq!(plan.requires["other"].to_string(), "^2");
    assert!(!plan.requires["other"].matches(&plan.provides["svc"]));
}

/// 旧 manifest 不带 `provides` / `requires` 仍然合法，收集结果为空映射。
#[test]
fn disk_source_manifest_without_ranges_defaults_to_empty_maps() {
    let (_dir, artifact, _config) = plugin_fixture(
        "manifest-legacy",
        Some(manifest_value("legacy.plugin", "0.1.0")),
    );
    let entry = Entry {
        name: artifact.to_string_lossy().into_owned(),
        ..Entry::default()
    };

    let spec = DiskPluginSource
        .plan_entry(&entry, &[])
        .expect("旧 manifest 必须可读")
        .expect("有 sidecar 就必须产生计划");
    assert!(spec.provides.is_empty());
    assert!(spec.requires.is_empty());
    spec.into_entry().expect("只有 id/version 也是合法计划");
}

/// 没有 sidecar ⇒ `Ok(None)`；不存在 artifact 也返回 `Ok(None)`（交给 load 报错）。
#[test]
fn disk_source_without_a_sidecar_reports_no_plan() {
    let (_dir, artifact, _config) = plugin_fixture("manifest-absent", None);
    let entry = Entry {
        name: artifact.to_string_lossy().into_owned(),
        ..Entry::default()
    };
    assert!(DiskPluginSource
        .plan_entry(&entry, &[])
        .expect("没有 sidecar 不是错误")
        .is_none());

    let missing = Entry {
        name: "definitely-missing-plugin".into(),
        ..Entry::default()
    };
    assert!(DiskPluginSource
        .plan_entry(&missing, &[])
        .expect("找不到 artifact 交给 load 报错")
        .is_none());
}

/// 损坏的 sidecar JSON 必须报错，并带上 artifact 路径与行 id。
#[test]
fn disk_source_malformed_sidecar_reports_path_and_id() {
    let (_dir, artifact, _config) = plugin_fixture("manifest-malformed", None);
    std::fs::write(cordis_host::source::manifest_path(&artifact), "{ not json")
        .expect("写坏 manifest");
    let entry = Entry {
        id: Some("broken-row".into()),
        name: artifact.to_string_lossy().into_owned(),
        ..Entry::default()
    };

    let error = DiskPluginSource
        .plan_entry(&entry, &[])
        .expect_err("损坏的 sidecar 必须报错");
    assert!(
        error.contains("broken-row") && error.contains("manifest"),
        "错误必须点名行与 manifest：{error}"
    );
}

/// 端到端：sidecar 里的非 semver `provides` 必须在 `source.load` 之前让装载失败。
#[tokio::test]
async fn disk_source_manifest_with_bad_semver_fails_before_load() {
    let mut manifest = manifest_value("broken.plugin", "1.0.0");
    manifest["provides"] = serde_json::json!({ "svc": "not-a-version" });
    let (dir, _artifact, config) = plugin_fixture("manifest-bad-semver", Some(manifest));
    std::fs::write(&config, "- id: broken\n  name: plugin.bin\n").expect("写配置");

    let app = App::new();
    let error = load_with_source(
        app.root.clone(),
        config_for(&config, Vec::new()),
        Arc::new(DiskPluginSource),
    )
    .await
    .expect_err("非 semver 的 provides 必须让装载失败");
    let message = error.to_string();
    assert!(
        message.contains("broken.plugin")
            && message.contains("provides.svc")
            && message.contains("semver"),
        "错误必须点名插件、字段与 semver：{message}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
