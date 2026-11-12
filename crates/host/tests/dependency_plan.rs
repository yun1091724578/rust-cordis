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
    load_with_source, InterpolationMode, LoadedFile, LoaderConfig, PluginSource,
};
use cordis_host::patch::Entry;
use cordis_host::plan::PlanEntrySpec;
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

    /// 假源：每次 `load` 计数 +1；返回的插件在 `apply` 时计数 +1。
    fn source(&self) -> Arc<dyn PluginSource> {
        self.source_with(&[])
    }

    /// 同上，但让插件**自述** `provision`（用来证明它不参与显式计划求解）。
    fn source_with(&self, provision: &[&str]) -> Arc<dyn PluginSource> {
        Arc::new(CountingSource {
            counters: self.clone(),
            provision: provision.iter().map(|key| key.to_string()).collect(),
        })
    }
}

struct CountingSource {
    counters: Counters,
    provision: Vec<String>,
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
}

fn config_for(path: &Path, dependency_plan: Vec<PlanEntrySpec>) -> LoaderConfig {
    LoaderConfig {
        path: path.to_string_lossy().into_owned(),
        patches: Vec::new(),
        plugin_dir: None,
        strict: true,
        strict_cycles: false,
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
