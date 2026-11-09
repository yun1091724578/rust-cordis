//! M5：**依赖环诊断**（A6）——声明级的环检查：诊断 + 带环路径，**默认不拒挂**。
//!
//! 论文 §6.5 是 *"a runtime **can** report it"*（能力非义务）；Thm 73 的 acyclicity
//! 是**前提**（经 Lemma 77 进入 Thm 80），不是"必须报告"的义务来源。
//! 本仓因此把环检查做成**声明级诊断**：
//!
//! - 图是**声明**构成的：行 `inject` 的名字 ∩ 别的行**声明提供**的 key ⇒ 一条边。
//!   运行时 `provide()` 出来的东西在挂载前不可知，不参与（Thm 73 的前提也是声明级的）；
//! - 环进 `report.cycles`（每个 SCC 一条**带环路径**，首尾同名）；
//! - **默认只报告**：行照旧挂上（自然停在 PENDING，由内核的依赖机制兜着）；
//! - `strict` 只负责**逐行**装载失败；环由独立的 `strict_cycles` 控制（默认 `false`，
//!   本仓扩展：把它打开才把环升级成装载失败）。
//!
//! 全部无 DLL（D9）：`PluginSource` seam 用假插件顶替加载。

use cordis_core::{App, Config, Ctx, Inject, Plugin, PluginKey};
use cordis_host::loader::{load_with_source, LoadedFile, LoaderConfig, PluginSource};
use cordis_host::patch::Entry;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// 假插件：`inject` 里的每个名字都"等"一个服务；`provision` 与声明一起由测试给。
struct Fake {
    name: &'static str,
    provision: Vec<String>,
    inject: Vec<String>,
}

impl cordis_core::Plugin for Fake {
    fn name(&self) -> &str {
        self.name
    }
    fn key(&self) -> PluginKey {
        Arc::from(format!("fake:{}", self.name).as_str())
    }
    fn inject(&self) -> Inject {
        Inject(
            self.inject
                .iter()
                .map(|name| (name.as_str().into(), Config::Null))
                .collect(),
        )
    }
    fn provision(&self) -> Vec<String> {
        self.provision.clone()
    }
    fn apply(&self, ctx: Ctx, _config: Config) -> cordis_core::BoxFuture<cordis_core::Result<()>> {
        // 真实插件的行为：**声明什么就安装什么**（`provision` 与 `provide` 一一对应）。
        let keys = self.provision.clone();
        Box::pin(async move {
            for key in keys {
                ctx.provide(&key, cordis_core::Dyn::new(key.clone()))?;
            }
            Ok(())
        })
    }
}

/// 按行名分派假插件：行名 = 插件名，声明写在插件里（与 ABI 的 desc 同一条思路）。
struct CycleSource;

impl PluginSource for CycleSource {
    fn load(
        &self,
        entry: &Entry,
        _search: &[PathBuf],
        _revision: u64,
    ) -> Result<LoadedFile, String> {
        let plugin: Arc<dyn Plugin> = match entry.name.as_str() {
            // a 声明提供 x、等 y；b 声明提供 y、等 x ⇒ 互为前提（环）。
            "a" => Arc::new(Fake {
                name: "a",
                provision: vec!["x".into()],
                inject: vec!["y".into()],
            }),
            "b" => Arc::new(Fake {
                name: "b",
                provision: vec!["y".into()],
                inject: vec!["x".into()],
            }),
            // 无依赖的旁观者：环之外的部分必须照常挂上。
            "c" => Arc::new(Fake {
                name: "c",
                provision: vec![],
                inject: vec![],
            }),
            // 无环的一对：提供者在前、消费者在后（守卫用）。
            "prov" => Arc::new(Fake {
                name: "prov",
                provision: vec!["svc".into()],
                inject: vec![],
            }),
            "cons" => Arc::new(Fake {
                name: "cons",
                provision: vec![],
                inject: vec!["svc".into()],
            }),
            other => return Err(format!("假插件源不认识 \"{other}\"")),
        };
        let path = PathBuf::from(format!("<fake:{0}>", entry.name));
        Ok(LoadedFile {
            plugin,
            source: path.clone(),
            staged: path,
            fingerprint: None,
        })
    }
}

fn temp_config(tag: &str, yaml: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("cordis-cycles-{}-{tag}.yml", std::process::id()));
    std::fs::write(&path, yaml).expect("写配置");
    path
}

fn config_for(path: &Path, strict: bool, strict_cycles: bool) -> LoaderConfig {
    LoaderConfig {
        path: path.to_string_lossy().into_owned(),
        patches: Vec::new(),
        plugin_dir: None,
        strict,
        strict_cycles,
        hmr: None,
        js_evaluator: None,
        interpolation: cordis_host::loader::InterpolationMode::Builtin,
        dispatch_policy: None,
    }
}

fn report_of(ctx: &Ctx) -> Value {
    let value = ctx.require("loader.report").expect("loader.report");
    value
        .downcast::<cordis_core::ExternValue>()
        .expect("ExternValue")
        .0
        .clone()
}

/// M5-4：环被诊断，而且**路径**看得出来（首尾同名，一眼知道谁在等谁）。
#[tokio::test]
async fn dependency_cycle_is_reported_with_its_path() {
    let app = App::new();
    let path = temp_config("reported", "- id: a\n  name: a\n- id: b\n  name: b\n");
    let state = load_with_source(
        app.root.clone(),
        config_for(&path, false, false),
        Arc::new(CycleSource),
    )
    .await
    .expect("默认只报告：环不该让装载失败");

    let report = report_of(&app.root);
    assert_eq!(
        report["cycles"],
        json!([["a", "b", "a"]]),
        "一个环一条路径，首尾同名：{report}"
    );
    // 默认不拒挂：两行都挂上了（自然停在 PENDING，由内核的依赖机制兜着）。
    assert_eq!(report["mounted"], json!(2));
    let a = state.fiber_of("a").expect("a 已挂");
    assert_eq!(
        a.state(),
        cordis_core::FiberState::Pending,
        "互为前提 ⇒ 自然停在 PENDING"
    );
    let b = state.fiber_of("b").expect("b 已挂");
    assert_eq!(b.state(), cordis_core::FiberState::Pending);
    let pending = report["pending"].as_array().expect("pending 必须是数组");
    assert_eq!(pending.len(), 2, "{report}");
    assert!(
        pending.iter().any(|row| {
            row["label"] == "a"
                && row["missing"][0]["key"] == "y"
                && row["missing"][0]["reason"] == "missing"
        }),
        "装载报告必须说明 a 缺 y：{report}"
    );
    assert!(
        pending.iter().any(|row| {
            row["label"] == "b"
                && row["missing"][0]["key"] == "x"
                && row["missing"][0]["reason"] == "missing"
        }),
        "装载报告必须说明 b 缺 x：{report}"
    );
}

/// M5-5：环**不阻止**树里其余部分挂上（手动驱动 `reconcile_once`，A6 的判据）。
#[tokio::test]
async fn cycle_does_not_prevent_the_rest_of_the_tree_from_mounting() {
    let app = App::new();
    let path = temp_config(
        "rest",
        "- id: a\n  name: a\n- id: b\n  name: b\n- id: c\n  name: c\n",
    );
    let state = load_with_source(
        app.root.clone(),
        config_for(&path, false, false),
        Arc::new(CycleSource),
    )
    .await
    .expect("装载成功");

    // 手动驱动一次对账：环还在，别的行照旧（幂等，不产生额外动作）。
    let outcome = state.reconcile_once(state.tree_snapshot()).await;
    assert!(
        outcome.failures.is_empty(),
        "没有失败：{:?}",
        outcome.failures
    );

    let report = report_of(&app.root);
    assert_eq!(
        report["mounted"],
        json!(3),
        "环内两行 + 旁观者都挂上：{report}"
    );
    assert_eq!(report["cycles"], json!([["a", "b", "a"]]));
    let c = state.fiber_of("c").expect("c 已挂");
    assert_eq!(
        c.state(),
        cordis_core::FiberState::Active,
        "环之外的部分必须照常激活"
    );
}

/// M5-6（**本仓扩展**，默认关）：`strict_cycles` 打开时环变成装载失败，错误里带环路径。
#[tokio::test]
async fn strict_mode_turns_a_cycle_into_a_load_failure() {
    let app = App::new();
    let path = temp_config("strict", "- id: a\n  name: a\n- id: b\n  name: b\n");
    let error = load_with_source(
        app.root.clone(),
        config_for(&path, true, true),
        Arc::new(CycleSource),
    )
    .await
    .expect_err("strict_cycles 模式下环不许上线");
    let message = error.to_string();
    assert!(message.contains("环"), "必须说清是环：{message}");
    assert!(
        message.contains("a") && message.contains("b"),
        "必须带环路径：{message}"
    );
}

/// M5-7（**守卫**）：无环的声明**永不**被报成环，而且能正常激活。
#[tokio::test]
async fn acyclic_precedence_is_never_reported() {
    let app = App::new();
    let path = temp_config(
        "acyclic",
        "- id: prov\n  name: prov\n- id: cons\n  name: cons\n",
    );
    let state = load_with_source(
        app.root.clone(),
        config_for(&path, true, true),
        Arc::new(CycleSource),
    )
    .await
    .expect("提供者在前、消费者在后 ⇒ 无环 ⇒ strict 也照样成功");

    let report = report_of(&app.root);
    assert_eq!(report["cycles"], json!([]), "无环就不该有条目：{report}");
    assert_eq!(report["mounted"], json!(2));
    assert_eq!(
        state.fiber_of("cons").expect("cons 已挂").state(),
        cordis_core::FiberState::Active
    );
}

/// **默认语义**（P1 修复的判据）：`strict` 只负责**逐行**失败；环由独立的
/// `strict_cycles` 控制，默认 `false` ⇒ 两行都挂上、只报告路径。
///
/// 旧实现把两者混成一个开关：默认配置一遇到环就 boot 失败，与 A6"默认只报告"
/// 的立场相反。
#[tokio::test]
async fn strict_rows_do_not_escalate_cycles_unless_strict_cycles_is_on() {
    let app = App::new();
    let path = temp_config("row-strict", "- id: a\n  name: a\n- id: b\n  name: b\n");
    let state = load_with_source(
        app.root.clone(),
        config_for(&path, true, false),
        Arc::new(CycleSource),
    )
    .await
    .expect("strict=true 只负责逐行失败：环默认只报告");

    let report = report_of(&app.root);
    assert_eq!(
        report["cycles"],
        json!([["a", "b", "a"]]),
        "环必须被报告：{report}"
    );
    assert_eq!(report["mounted"], json!(2), "默认两行都必须挂上：{report}");
    assert!(state.fiber_of("a").is_some() && state.fiber_of("b").is_some());
}

/// `strict_cycles=true` **单独**就能把环升级成装载失败（不依赖 `strict`），错误带路径。
#[tokio::test]
async fn strict_cycles_alone_turns_a_cycle_into_a_load_failure() {
    let app = App::new();
    let path = temp_config("only-cycles", "- id: a\n  name: a\n- id: b\n  name: b\n");
    let error = load_with_source(
        app.root.clone(),
        config_for(&path, false, true),
        Arc::new(CycleSource),
    )
    .await
    .expect_err("strict_cycles=true 时环不许上线");
    let message = error.to_string();
    assert!(message.contains("环"), "必须说清是环：{message}");
    assert!(message.contains("a → b → a"), "必须带完整环路径：{message}");
}

/// 配置字段的**默认值**：`strict_cycles` 缺省为 `false`（`serde(default)`）。
#[test]
fn strict_cycles_defaults_to_false() {
    let config: LoaderConfig = serde_json::from_value(json!({
        "path": "cordis.yml",
        "strict": true,
    }))
    .expect("最小配置必须能解析");
    assert!(config.strict);
    assert!(!config.strict_cycles, "strict_cycles 默认 false：环只报告");
}
