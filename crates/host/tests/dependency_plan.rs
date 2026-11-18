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
    PolicyPluginSource,
};
use cordis_host::patch::Entry;
use cordis_host::plan::PlanEntrySpec;
use serde_json::{json, Value};
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

/// 本地开发准入的磁盘源：只给这个来源放行，**不写**进程级开发标志（O9）。
///
/// 这一节的用例只关心"sidecar 怎么读成计划"，artifact 是随手写的假字节（manifest
/// 里的 sha256 也是假的），既没有 allow-list、也不该去动全局开关——`dev_unverified`
/// 正好是"这个来源不校验"的局部说法，顺便让 `plan_entry` / `load` 走同一个读口。
fn local_dev_source() -> PolicyPluginSource {
    PolicyPluginSource::dev_unverified(Arc::new(DiskPluginSource))
}

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

    let spec = local_dev_source()
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

    let spec = local_dev_source()
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
    assert!(local_dev_source()
        .plan_entry(&entry, &[])
        .expect("没有 sidecar 不是错误")
        .is_none());

    let missing = Entry {
        name: "definitely-missing-plugin".into(),
        ..Entry::default()
    };
    assert!(local_dev_source()
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

    let error = local_dev_source()
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
        // O9：准入跟着来源走。这里要看的是**声明**预检（非 semver 的 provides），
        // 不是来源准入，所以给这个来源一个局部开发放行，不去动全局开关。
        Arc::new(local_dev_source()),
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

/// 声明走真实的 [`DiskPluginSource`]（因此读**进程级**准入），装载走假源。
///
/// 把两件事分开，才能单独观察预检的准入结论：否则"预检被默认拒绝挡住"与
/// "这一行根本装不上"混在同一个错误里。
struct DiskPlanSource {
    disk: DiskPluginSource,
}

impl PluginSource for DiskPlanSource {
    fn load(
        &self,
        entry: &Entry,
        _search: &[PathBuf],
        _revision: u64,
    ) -> Result<LoadedFile, String> {
        let plugin: Arc<dyn Plugin> = plugin_fn(
            "disk-plan",
            Inject::default(),
            |_ctx: Ctx, _config: Config| Box::pin(async { Ok(()) }),
        );
        let path = PathBuf::from(format!("<disk-plan:{}>", entry.name));
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
        search: &[PathBuf],
    ) -> Result<Option<PlanEntrySpec>, String> {
        self.disk.plan_entry(entry, search)
    }
}

/// 前提断言：进程级准入是**默认拒绝**（无策略、也没开开发模式）。
fn assert_default_denied_admission() {
    assert!(
        !cordis_host::source::allow_unverified(),
        "前提：进程级开发模式必须是关的"
    );
    assert!(
        cordis_host::source::active_policy().is_none(),
        "前提：进程级不得有来源策略"
    );
}

/// O9：**默认拒绝** + 有 sidecar ⇒ 预检失败，且错误点名**准入**原因。
///
/// sidecar 这里是**损坏**的：顺序是有意的——同样的配置在一台"配了策略"的机器上报
/// 策略错误、在另一台机器上报"manifest 无效"，首错就不稳定。准入结论必须先于
/// manifest 结论给出（`plan_entry_with` 先 `resolve` 再读 sidecar）。
#[tokio::test]
async fn default_denied_admission_fails_the_preflight_and_names_admission() {
    assert_default_denied_admission();
    let (dir, artifact, config) = plugin_fixture("preflight-default-denied", None);
    // 有 sidecar，但它是坏的：准入必须先说话。
    std::fs::write(cordis_host::source::manifest_path(&artifact), "{ not json")
        .expect("写坏 manifest");
    std::fs::write(&config, "- id: blocked\n  name: plugin.bin\n").expect("写配置");

    let app = App::new();
    let error = load_with_source(
        app.root.clone(),
        // strict_sidecars: true（`config_for` 的缺省）
        config_for(&config, Vec::new()),
        Arc::new(DiskPlanSource {
            disk: DiskPluginSource,
        }),
    )
    .await
    .expect_err("默认拒绝下声明预检必须失败");
    let message = error.to_string();
    assert!(
        message.contains("未配置插件来源策略"),
        "错误必须点名准入原因：{message}"
    );
    assert!(message.contains("blocked"), "错误必须点名行：{message}");
    assert!(
        !message.contains("manifest"),
        "准入原因必须优先于 manifest 结论：{message}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// O9：同一条默认拒绝，`strict_sidecars=false` ⇒ 降级为 `downgraded`，逐行保留原因。
///
/// 宽松模式不等于放行：整次预检**跳过**（`skipped=true`），但原始错误必须留在报告里，
/// 否则用户只能从日志里猜"这次预检为什么什么都没检查"。sidecar 同样是坏的，
/// 报告里的原因仍应是**准入**（而不是 manifest）：同一条配置的首错不随机器漂移。
#[tokio::test]
async fn default_denied_admission_downgrades_the_preflight_when_lenient() {
    assert_default_denied_admission();
    let (dir, artifact, config) = plugin_fixture("preflight-default-denied-lenient", None);
    std::fs::write(cordis_host::source::manifest_path(&artifact), "{ not json")
        .expect("写坏 manifest");
    std::fs::write(&config, "- id: blocked\n  name: plugin.bin\n").expect("写配置");

    let app = App::new();
    let loader_config = LoaderConfig {
        strict_sidecars: false,
        ..config_for(&config, Vec::new())
    };
    let state = load_with_source(
        app.root.clone(),
        loader_config,
        Arc::new(DiskPlanSource {
            disk: DiskPluginSource,
        }),
    )
    .await
    .expect("宽松模式只降级，不失败");
    assert_eq!(state.mounted_labels(), ["blocked"], "装载照常进行");

    let preflight = state.report()["preflight"].clone();
    assert_eq!(preflight["verdict"], json!("downgraded"), "{preflight}");
    assert_eq!(preflight["skipped"], json!(true), "{preflight}");
    assert_eq!(
        preflight["declaration"],
        json!("sidecarManifest"),
        "{preflight}"
    );
    let rows = preflight["rows"]
        .as_array()
        .unwrap_or_else(|| panic!("rows 必须是数组：{preflight}"));
    assert_eq!(rows.len(), 1, "{preflight}");
    assert_eq!(rows[0]["label"], json!("blocked"), "{preflight}");
    assert_eq!(rows[0]["verdict"], json!("downgraded"), "{preflight}");
    assert!(
        rows[0]["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("未配置插件来源策略")),
        "降级原因必须点名准入：{preflight}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------- 声明预检报告（O7）
//
// ADR 0007：`loader.report.preflight` 是**声明**结论，不是运行时事实。
// 它不改任何门禁结果（Ok/Err 与 O7 之前逐字一致），只回答"这次预检走到了哪一步、
// 每个相关行说了什么、哪些行其实没有声明"。因此报告的字段一律用
// declaration / runtimeVerified=false / realmModel=notModeled 标注它的适用边界，
// 不出现 verified / bound / satisfied 这类暗示运行时绑定的词。

/// 声明与运行期**分开给**的假源：`plan` 决定预检看到什么，
/// `behavior`（行名 → (inject, provide)）决定真正 `apply` 时做什么。
///
/// 两者必须能分开，O7 的核心判据正是"声明通过 ≠ 运行期绑定成功"。
struct BoundSource {
    counters: Counters,
    plan: BTreeMap<String, PlanEntrySpec>,
    inject: BTreeMap<String, Vec<String>>,
    provides: BTreeMap<String, Vec<String>>,
}

impl BoundSource {
    fn build(
        plan: Vec<(&str, PlanEntrySpec)>,
        behavior: &[(&str, &[&str], &[&str])],
    ) -> (Counters, Arc<dyn PluginSource>) {
        let counters = Counters::new();
        let source = Self {
            counters: counters.clone(),
            plan: plan
                .into_iter()
                .map(|(name, spec)| (name.to_string(), spec))
                .collect(),
            inject: behavior
                .iter()
                .map(|(name, inject, _)| {
                    (
                        name.to_string(),
                        inject.iter().map(|key| key.to_string()).collect(),
                    )
                })
                .collect(),
            provides: behavior
                .iter()
                .map(|(name, _, provides)| {
                    (
                        name.to_string(),
                        provides.iter().map(|key| key.to_string()).collect(),
                    )
                })
                .collect(),
        };
        (counters, Arc::new(source))
    }
}

impl PluginSource for BoundSource {
    fn load(
        &self,
        entry: &Entry,
        _search: &[PathBuf],
        _revision: u64,
    ) -> Result<LoadedFile, String> {
        self.counters.loads.fetch_add(1, Ordering::SeqCst);
        let inject = self.inject.get(&entry.name).cloned().unwrap_or_default();
        let provides = self.provides.get(&entry.name).cloned().unwrap_or_default();
        let applies = self.counters.applies.clone();
        let plugin: Arc<dyn Plugin> = plugin_fn(
            "bound",
            Inject(
                inject
                    .iter()
                    .map(|key| (key.as_str().into(), Config::Null))
                    .collect(),
            ),
            move |ctx: Ctx, _config: Config| {
                let applies = applies.clone();
                let provides = provides.clone();
                Box::pin(async move {
                    applies.fetch_add(1, Ordering::SeqCst);
                    for key in provides {
                        ctx.provide(&key, cordis_core::Dyn::new(key.clone()))?;
                    }
                    Ok(())
                })
            },
        );
        let path = PathBuf::from(format!("<bound:{}>", entry.name));
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
        Ok(self.plan.get(&entry.name).cloned())
    }
}

/// `loader.report` 的当前值（`ctx` 上已 provide；装载失败时也还在）。
fn report_from(ctx: &Ctx) -> Value {
    let value = ctx.require("loader.report").expect("loader.report");
    let exposed = value
        .downcast::<cordis_core::ExternValue>()
        .expect("ExternValue");
    exposed.0.clone()
}

/// 报告里某一行未就绪的缺失键（排序无关的粗判）。
fn pending_keys(report: &Value, label: &str) -> Vec<String> {
    report["pending"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter(|row| row["label"] == json!(label))
                .flat_map(|row| row["missing"].as_array().cloned().unwrap_or_default())
                .filter_map(|item| item["key"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// 报告数组里某一行（按 label）的条目。
fn preflight_row<'a>(preflight: &'a Value, label: &str) -> &'a Value {
    preflight["rows"]
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["label"] == json!(label)))
        .unwrap_or_else(|| panic!("报告里缺少 {label} 行：{preflight}"))
}

/// 每一份预检报告都必须显式标注自己的适用边界（ADR 0007），
/// 并用 `skipped` 直说"整次预检是否被跳过"（`incomplete`/`downgraded` 为真）。
fn assert_declaration_scope(preflight: &Value, skipped: bool) {
    assert_eq!(
        preflight["scope"],
        json!("declaration"),
        "报告必须声明自己是声明级结论：{preflight}"
    );
    assert_eq!(
        preflight["runtimeVerified"],
        json!(false),
        "声明通过不是运行时 verified：{preflight}"
    );
    assert_eq!(
        preflight["realmModel"],
        json!("notModeled"),
        "本任务的预检不做 realm 感知：{preflight}"
    );
    assert_eq!(
        preflight["skipped"],
        json!(skipped),
        "`skipped` 必须直说整次预检是否被跳过：{preflight}"
    );
}

/// 非 `checked` 分类里**不得**出现 `declaredProviders`（不存在或为空）。
///
/// 求解器没跑出结论意味着"没有任何提供者被选出来"。这个键一旦出现在
/// `incomplete`/`downgraded`/`notApplicable` 下面，读报告的人会以为求解真的发生过
/// ——把"什么都没检查"误报成"检查后选出了这些"。
fn assert_no_declared_providers(preflight: &Value) {
    let providers = match preflight.get("declaredProviders") {
        None | Some(Value::Null) => return,
        Some(value) => value
            .as_array()
            .unwrap_or_else(|| panic!("declaredProviders 必须是数组：{preflight}")),
    };
    assert!(
        providers.is_empty(),
        "非 checked 分类不得报出 declaredProviders：{preflight}"
    );
}

/// O7-a：混合来源（一行有 sidecar、一行 `Ok(None)`）⇒ 整次预检保守跳过，
/// 报告分类为 `incomplete` 并**点名**没有声明的那一行；装载行为照旧。
#[tokio::test]
async fn preflight_reports_incomplete_on_mixed_sources() {
    let counters = Counters::new();
    let app = App::new();
    let path = write_config(
        "preflight-incomplete",
        "- id: provider\n  name: provider\n- id: consumer\n  name: consumer\n",
    );
    let source = counters.source_with_plan(vec![(
        "provider",
        spec("provider.plugin", "1.0.0", &[("svc", "1.0.0")], &[]),
    )]);

    let state = load_with_source(app.root.clone(), config_for(&path, Vec::new()), source)
        .await
        .expect("混合来源照旧装载成功");

    assert_eq!(state.mounted_labels(), vec!["consumer", "provider"]);
    assert_eq!(counters.loads(), 2, "预检跳过不影响逐行装载");

    let report = state.report();
    let preflight = &report["preflight"];
    assert_eq!(preflight["verdict"], json!("incomplete"), "{report}");
    assert_declaration_scope(preflight, true);
    assert_no_declared_providers(preflight);
    let missing = preflight_row(preflight, "consumer");
    assert_eq!(
        missing["verdict"],
        json!("noDeclaration"),
        "无声明行必须被点名：{report}"
    );
    assert_eq!(missing["plugin"], json!("consumer"));
    assert_eq!(
        preflight_row(preflight, "provider")["verdict"],
        json!("declared")
    );
}

/// O7-b：`strict_sidecars=false` 下损坏的 sidecar ⇒ 整次跳过，但**原始错误文本**、
/// 行号与 label 必须留在报告里（旧实现把 `Err` 整个丢掉了）。
#[tokio::test]
async fn preflight_reports_downgraded_err_under_lenient_sidecars() {
    let counters = Counters::new();
    let app = App::new();
    let path = write_config("preflight-downgraded", "- id: broken\n  name: broken\n");
    let source = counters.source_with_plan_error("sidecar manifest 解析失败");
    let config = LoaderConfig {
        strict_sidecars: false,
        ..config_for(&path, Vec::new())
    };

    let state = load_with_source(app.root.clone(), config, source)
        .await
        .expect("strictSidecars=false 时损坏的 sidecar 不得拖垮装载");
    assert_eq!(state.mounted_labels(), vec!["broken"]);
    assert_eq!(counters.loads(), 1, "预检跳过后照常加载");

    let report = state.report();
    let preflight = &report["preflight"];
    assert_eq!(preflight["verdict"], json!("downgraded"), "{report}");
    assert_declaration_scope(preflight, true);
    assert_no_declared_providers(preflight);
    let row = preflight_row(preflight, "broken");
    assert_eq!(row["index"], json!(0), "行号必须保留：{report}");
    assert_eq!(row["verdict"], json!("downgraded"));
    assert_eq!(
        row["reason"],
        json!("sidecar manifest 解析失败"),
        "宽松降级必须保留原始错误文本：{report}"
    );
}

/// O7-c：两份 manifest 声明同一服务键的不同版本 ⇒ 求解器选出被声明版本最高的
/// 提供者；这份结论必须进报告（旧实现把 `ResolvedPlan` 直接丢掉了）。
#[tokio::test]
async fn preflight_reports_the_resolved_provider() {
    let counters = Counters::new();
    let app = App::new();
    let path = write_config(
        "preflight-checked",
        "- id: a\n  name: a\n- id: b\n  name: b\n- id: consumer\n  name: consumer\n",
    );
    let source = counters.source_with_plan(vec![
        ("a", spec("a.plugin", "1.0.0", &[("svc", "1.0.0")], &[])),
        ("b", spec("b.plugin", "2.0.0", &[("svc", "1.2.0")], &[])),
        (
            "consumer",
            spec("consumer.plugin", "3.0.0", &[], &[("svc", "^1")]),
        ),
    ]);

    let state = load_with_source(app.root.clone(), config_for(&path, Vec::new()), source)
        .await
        .expect("声明可满足时必须放行");

    let report = state.report();
    let preflight = &report["preflight"];
    assert_eq!(preflight["verdict"], json!("checked"), "{report}");
    assert_eq!(
        preflight["declaration"],
        json!("sidecarManifest"),
        "checked 结论来自 sidecar manifest：{report}"
    );
    assert_declaration_scope(preflight, false);
    assert_eq!(
        preflight["entries"],
        json!(3),
        "参与求解的计划条目数：{report}"
    );
    let providers = preflight["declaredProviders"]
        .as_array()
        .expect("declaredProviders 必须是数组");
    assert_eq!(providers.len(), 1, "{report}");
    assert_eq!(providers[0]["key"], json!("svc"));
    assert_eq!(
        providers[0]["plugin"],
        json!("b.plugin"),
        "选出的是声明版本最高的提供者：{report}"
    );
    assert_eq!(providers[0]["version"], json!("1.2.0"));
    assert_eq!(counters.plans(), 3, "预检逐行询问 plan_entry");
}

/// O7-d：显式 `dependencyPlan` ⇒ 预检不适用（`explicitPlan`），且不读 manifest。
#[tokio::test]
async fn preflight_is_not_applicable_for_explicit_plan() {
    let counters = Counters::new();
    let app = App::new();
    let path = write_config("preflight-explicit", "- id: prod\n  name: prod\n");
    let explicit = vec![spec("prod", "1.0.0", &[("svc", "1.0.0")], &[])];
    let source = counters.source_with_plan_error("显式计划不得读到这里");

    let state = load_with_source(app.root.clone(), config_for(&path, explicit), source)
        .await
        .expect("显式计划覆盖 manifest 计划");

    assert_eq!(counters.plans(), 0, "显式计划不得调用 plan_entry");
    let report = state.report();
    let preflight = &report["preflight"];
    assert_eq!(preflight["verdict"], json!("notApplicable"), "{report}");
    assert_eq!(preflight["reason"], json!("explicitPlan"));
    assert_eq!(
        preflight["declaration"],
        json!("explicitPlan"),
        "显式计划的声明来源就是它自己：{report}"
    );
    assert_declaration_scope(preflight, false);
    assert_no_declared_providers(preflight);
}

/// O7-d'：没有任何可检查的启用行 ⇒ `notApplicable` + `noEnabledRows`，且
/// `declaration` 是中性值——这一轮**没读过** sidecar manifest，报 `sidecarManifest`
/// 会让人以为读过它（错误归因到"声明不可用"）。
#[tokio::test]
async fn preflight_reports_no_declaration_when_no_rows_are_enabled() {
    let counters = Counters::new();
    let app = App::new();
    let path = write_config(
        "preflight-no-enabled-rows",
        "- id: dormant\n  name: dormant\n  disabled: true\n",
    );
    let source = counters.source_with_plan_error("没有启用行时不得读取 sidecar");

    let state = load_with_source(app.root.clone(), config_for(&path, Vec::new()), source)
        .await
        .expect("全静态禁用的配置照旧装载成功");
    assert_eq!(counters.plans(), 0, "没有启用行 ⇒ 不询问 plan_entry");

    let report = state.report();
    let preflight = &report["preflight"];
    assert_eq!(preflight["verdict"], json!("notApplicable"), "{report}");
    assert_eq!(preflight["reason"], json!("noEnabledRows"));
    assert_eq!(
        preflight["declaration"],
        json!("none"),
        "没读 sidecar 就不得声称结论来自 sidecarManifest：{report}"
    );
    assert_declaration_scope(preflight, false);
    assert_no_declared_providers(preflight);
}

/// O7-e（联动 1）：同域不同版本 ⇒ **声明上不是歧义**（预检 `checked`），
/// 但运行期同一默认域只能有一个实现 ⇒ 第二个 `provide` 失败。
///
/// 这条是 ADR 0007 的核心证据：声明通过**不等于**运行期绑定成功。
#[tokio::test]
async fn preflight_checked_does_not_imply_runtime_binding() {
    let (counters, source) = BoundSource::build(
        vec![
            ("one", spec("one.plugin", "1.0.0", &[("svc", "1.0.0")], &[])),
            ("two", spec("two.plugin", "2.0.0", &[("svc", "2.0.0")], &[])),
        ],
        &[("one", &[], &["svc"]), ("two", &[], &["svc"])],
    );
    let app = App::new();
    let path = write_config(
        "preflight-binding-same-realm",
        "- id: one\n  name: one\n- id: two\n  name: two\n",
    );

    let error = load_with_source(app.root.clone(), config_for(&path, Vec::new()), source)
        .await
        .expect_err("同域两个提供者必须让装载失败");
    let message = error.to_string();
    assert!(
        message.contains("已被") && message.contains("注册"),
        "错误必须点出同域重复注册：{message}"
    );
    assert_eq!(counters.loads(), 2, "预检放行后两行都尝试过加载");

    let report = report_from(&app.root);
    assert_eq!(
        report["preflight"]["verdict"],
        json!("checked"),
        "版本不同不算歧义，声明预检通过：{report}"
    );
    assert_eq!(report["preflight"]["entries"], json!(2));
    // 边界标注必须齐全：声明级、未做运行时校验、不建模 realm、整次没被跳过。
    assert_declaration_scope(&report["preflight"], false);
}

/// O7-f（联动 2）：跨 realm 同键同版本。**当前实现会误报**——预检只看声明，
/// 不知道 realm（报告里 `realmModel=notModeled` 就是这条局限的显式标注）。
///
/// 运行期两个私有域各自合法；声明阶段却被当成"同一服务键同版本被多个插件提供"。
/// 本任务不引入 realm 感知求解，只把现状记录成测试。
#[tokio::test]
async fn preflight_misreports_cross_realm_same_key_as_ambiguous() {
    let cross_realm = concat!(
        "- id: left\n  name: left\n  isolate:\n    svc: true\n",
        "- id: right\n  name: right\n  isolate:\n    svc: true\n",
    );
    let path = write_config("preflight-cross-realm", cross_realm);

    // ① 记录当前结论：同键同版本（不同插件）⇒ 预检判歧义、装载在 load 之前失败。
    let (counters, source) = BoundSource::build(
        vec![
            (
                "left",
                spec("left.plugin", "1.0.0", &[("svc", "1.0.0")], &[]),
            ),
            (
                "right",
                spec("right.plugin", "1.0.0", &[("svc", "1.0.0")], &[]),
            ),
        ],
        &[("left", &[], &["svc"]), ("right", &[], &["svc"])],
    );
    let app = App::new();
    let error = load_with_source(app.root.clone(), config_for(&path, Vec::new()), source)
        .await
        .expect_err("现状：预检把跨 realm 同键同版本判成歧义");
    let message = error.to_string();
    assert!(
        message.contains("多个插件提供") && message.contains("svc"),
        "记录当前结论（误报）：{message}"
    );
    // 误报必须能自我解释：错误文本里带一句"声明预检不建模 realm"，否则用户只能
    // 自己去猜"两个私有域明明互不干扰，为什么被判歧义"。
    assert!(
        message.contains("声明预检不建模 realm"),
        "跨 realm 误报必须附带提示：{message}"
    );
    assert_eq!(counters.loads(), 0, "预检失败发生在任何 source.load 之前");

    // ② 报告显式写着 `realmModel=notModeled`：上面那条误报是**已声明的局限**，
    //    不是本任务要修的缺陷。把同键同版本换成不同版本，预检就放行，
    //    运行期两个私有域依旧各自合法。
    let (_counters, source) = BoundSource::build(
        vec![
            (
                "left",
                spec("left.plugin", "1.0.0", &[("svc", "1.0.0")], &[]),
            ),
            (
                "right",
                spec("right.plugin", "1.0.1", &[("svc", "1.0.1")], &[]),
            ),
        ],
        &[("left", &[], &["svc"]), ("right", &[], &["svc"])],
    );
    let app = App::new();
    let state = load_with_source(app.root.clone(), config_for(&path, Vec::new()), source)
        .await
        .expect("不同版本 ⇒ 预检放行；两个私有域里各自合法");
    assert_eq!(state.mounted_labels(), vec!["left", "right"]);
    let report = state.report();
    assert_eq!(report["preflight"]["verdict"], json!("checked"), "{report}");
    // 已声明的局限：预检不建模 realm，所以它无法区分"同域撞名"与"跨域同名"。
    assert_declaration_scope(&report["preflight"], false);
}

/// O7-g（联动 3）：`disabled` 表达式运行期为真的提供者仍**参与声明预检**
/// （`checked`），但运行期不挂 ⇒ 消费者停在 PENDING。
/// 报告必须写明它没有做运行时校验。
#[tokio::test]
async fn preflight_checked_while_dynamic_disabled_provider_is_dormant() {
    let (_counters, source) = BoundSource::build(
        vec![
            (
                "provider",
                spec("provider.plugin", "1.0.0", &[("svc", "1.0.0")], &[]),
            ),
            (
                "consumer",
                spec("consumer.plugin", "2.0.0", &[], &[("svc", "^1")]),
            ),
        ],
        &[("provider", &[], &["svc"]), ("consumer", &["svc"], &[])],
    );
    let app = App::new();
    let path = write_config(
        "preflight-dynamic-disabled",
        concat!(
            "- id: provider\n  name: provider\n  disabled: !!js \"1 == 1\"\n",
            "- id: consumer\n  name: consumer\n",
        ),
    );

    let state = load_with_source(app.root.clone(), config_for(&path, Vec::new()), source)
        .await
        .expect("动态 disabled 行在声明阶段保守纳入，装载整体成功");

    assert_eq!(
        state.mounted_labels(),
        vec!["consumer"],
        "运行期 dormant 的提供者不挂"
    );
    assert_eq!(
        state.fiber_of("consumer").expect("consumer 已挂").state(),
        cordis_core::FiberState::Pending,
        "提供者没挂 ⇒ 消费者停在 PENDING"
    );

    let report = state.report();
    assert_eq!(
        report["preflight"]["verdict"],
        json!("checked"),
        "预检把动态 disabled 提供者算进来了：{report}"
    );
    assert_eq!(
        report["preflight"]["entries"],
        json!(2),
        "两行都进了计划：{report}"
    );
    assert_declaration_scope(&report["preflight"], false);
    assert_eq!(
        pending_keys(&report, "consumer"),
        vec!["svc".to_string()],
        "报告必须报出消费者缺什么：{report}"
    );
}

/// O7-h（联动 4）：混合来源下整次预检跳过，消费者**停在 PENDING** 而不是让装载失败。
#[tokio::test]
async fn mixed_source_preflight_skip_leaves_consumer_pending_without_failing_the_load() {
    let (_counters, source) = BoundSource::build(
        vec![(
            "provider",
            spec("provider.plugin", "1.0.0", &[("svc", "1.0.0")], &[]),
        )],
        // 运行期的提供者什么都没提供：声明与绑定在这里不同步。
        &[("provider", &[], &[]), ("consumer", &["svc"], &[])],
    );
    let app = App::new();
    let path = write_config(
        "preflight-mixed-pending",
        "- id: provider\n  name: provider\n- id: consumer\n  name: consumer\n",
    );

    let state = load_with_source(app.root.clone(), config_for(&path, Vec::new()), source)
        .await
        .expect("混合来源只让预检跳过，整个装载不失败");

    assert_eq!(state.mounted_labels(), vec!["consumer", "provider"]);
    assert_eq!(
        state.fiber_of("consumer").expect("consumer 已挂").state(),
        cordis_core::FiberState::Pending
    );
    let report = state.report();
    assert_eq!(
        report["preflight"]["verdict"],
        json!("incomplete"),
        "{report}"
    );
    assert_declaration_scope(&report["preflight"], true);
    assert_no_declared_providers(&report["preflight"]);
    assert_eq!(
        preflight_row(&report["preflight"], "consumer")["verdict"],
        json!("noDeclaration")
    );
    assert_eq!(pending_keys(&report, "consumer"), vec!["svc".to_string()]);
}
