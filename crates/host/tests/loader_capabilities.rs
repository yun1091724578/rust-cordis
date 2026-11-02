//! M5：**能力清单**（`report.capabilities`）与 ABI 侧 provision 的**生产路径**。
//!
//! A5 要的是"挂载**之前**就能拿到每行的 inject/provision/volatile"：
//! - 原生插件：三项都在 `Plugin` 上（`inject`/`provision`/`volatile_paths`）；
//! - ABI 插件：`inject` 与 `provision` 在 **desc** 里（预挂载可读），`volatile`
//!   经 `OP_VOLATILE_PATHS` 跨边界问（约定句柄 0、不需要实例）——所以三项都能在
//!   mount 之前拿到（M5 的 `probe_capabilities` 与 CLI 的 `inspect` 都靠它）。

use cordis_core::{App, Config, Ctx, Inject, Plugin, PluginKey};
use cordis_host::loader::{load_with_source, LoadedFile, LoaderConfig, PluginSource};
use cordis_host::patch::Entry;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;

/// 假插件：把三项声明都摆在明面上（与 `LoadedPlugin` 给 ABI 插件做的完全一致）。
struct Fake {
    name: &'static str,
    inject: Vec<String>,
    provision: Vec<String>,
    volatile: Vec<String>,
}

impl Plugin for Fake {
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
    fn volatile_paths(&self) -> Vec<String> {
        self.volatile.clone()
    }
    fn apply(&self, _ctx: Ctx, _config: Config) -> cordis_core::BoxFuture<cordis_core::Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

struct CapabilitySource;

impl PluginSource for CapabilitySource {
    fn load(
        &self,
        entry: &Entry,
        _search: &[PathBuf],
        _revision: u64,
    ) -> Result<LoadedFile, String> {
        let plugin: Arc<dyn Plugin> = match entry.name.as_str() {
            "prod" => Arc::new(Fake {
                name: "prod",
                inject: vec![],
                provision: vec!["tools".into()],
                volatile: vec!["data".into()],
            }),
            "cons" => Arc::new(Fake {
                name: "cons",
                inject: vec!["tools".into()],
                provision: vec![],
                volatile: vec![],
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

fn report_of(ctx: &Ctx) -> Value {
    let value = ctx.require("loader.report").expect("loader.report");
    value
        .downcast::<cordis_core::ExternValue>()
        .expect("ExternValue")
        .0
        .clone()
}

/// M5-8：`report.capabilities` 把每行的 **inject / provision / volatile** 一次给全，
/// 顺序与行序一致（可逐字断言）。
#[tokio::test]
async fn capabilities_report_aggregates_inject_provision_and_volatile() {
    let app = App::new();
    let path = std::env::temp_dir().join(format!("cordis-caps-{}.yml", std::process::id()));
    std::fs::write(
        &path,
        "- id: prod\n  name: prod\n- id: cons\n  name: cons\n",
    )
    .expect("写配置");
    let config = LoaderConfig {
        path: path.to_string_lossy().into_owned(),
        patches: Vec::new(),
        plugin_dir: None,
        strict: true,
        strict_cycles: false,
        hmr: None,
        js_evaluator: None,
        interpolation: cordis_host::loader::InterpolationMode::Builtin,
    };
    load_with_source(app.root.clone(), config, Arc::new(CapabilitySource))
        .await
        .expect("装载成功");

    let report = report_of(&app.root);
    assert_eq!(
        report["capabilities"],
        json!([
            // 报告按 **label 升序**定序（装载表是哈希表，报告必须自己定序）。
            { "label": "cons", "plugin": "cons", "inject": ["tools"], "provision": [], "volatile": [],
              "js": { "env": [], "host": [], "services": [] } },
            { "label": "prod", "plugin": "prod", "inject": [], "provision": ["tools"], "volatile": ["data"],
              "js": { "env": [], "host": [], "services": [] } },
        ]),
        "每行一条，四项都要在（`js` 是 M9 加的 `!!js` 能力面）：{report}"
    );
}

/// entry-level `{required, optional}`：必需依赖照常门控，可选依赖缺席不阻塞。
#[tokio::test]
async fn loader_accepts_scoped_inject_and_optional_dependency_does_not_block() {
    let app = App::new();
    let path =
        std::env::temp_dir().join(format!("cordis-scoped-inject-{}.yml", std::process::id()));
    std::fs::write(
        &path,
        "- id: prod\n  name: prod\n- id: cons\n  name: cons\n  inject:\n    required: [tools]\n    optional: [nope]\n",
    )
    .expect("写配置");
    let config = LoaderConfig {
        path: path.to_string_lossy().into_owned(),
        patches: Vec::new(),
        plugin_dir: None,
        strict: true,
        strict_cycles: false,
        hmr: None,
        js_evaluator: None,
        interpolation: cordis_host::loader::InterpolationMode::Builtin,
    };
    let state = load_with_source(app.root.clone(), config, Arc::new(CapabilitySource))
        .await
        .expect("required 已提供、optional 缺席时装载必须成功");

    assert_eq!(state.mounted_labels(), vec!["cons", "prod"]);
    let report = report_of(&app.root);
    assert!(
        report["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["label"] == "cons" && row["inject"] == json!(["tools"])),
        "必需依赖仍要进入能力清单：{report}"
    );
}

/// M5-9：ABI 侧的 provision 走**生产路径**——`cordis_plugin!` 与
/// `build_plugin_with_provision` 把它写进 `PluginDesc` 的尾字段，宿主按附录 A 的
/// 规矩读回来（`Capabilities::of_desc` 与 `LoadedPlugin` 是同一条读法）。
#[test]
fn abi_plugin_can_declare_provision_through_the_build_path() {
    use std::mem::offset_of;

    let schema = r#"{"type":"object","required":["name"]}"#;
    let desc = cordis_abi::sdk::build_plugin_with_schema(
        "m5-abi",
        &["greeting"],
        &["data"],
        &["m5.svc", "m5.extra"],
        Some(schema),
        |_ctx, _config| Ok(()),
    );
    // ① desc 自己：尾字段真的写上了（v1 插件读到 None，v2 插件读到 JSON 数组）。
    let raw = unsafe {
        cordis_abi::PluginDesc::field_at::<cordis_abi::Blob>(
            desc,
            offset_of!(cordis_abi::PluginDesc, provision),
        )
        .expect("v2 的 desc 必须声明 size 覆盖 provision")
    };
    assert_eq!(unsafe { raw.as_str() }, r#"["m5.svc","m5.extra"]"#);

    // ② 宿主侧读法（与 `LoadedPlugin` 同一条路）：三项能力面都在，且**没有实例**。
    let caps = unsafe { cordis_host::Capabilities::of_desc(desc) };
    assert_eq!(caps.plugin, "m5-abi");
    assert_eq!(caps.inject, vec!["greeting".to_string()]);
    assert_eq!(
        caps.provision,
        vec!["m5.svc".to_string(), "m5.extra".to_string()]
    );
    assert_eq!(
        caps.volatile,
        vec!["data".to_string()],
        "volatile 走 OP_VOLATILE_PATHS"
    );
    assert_eq!(caps.schema(), Some(schema), "schema 在冻结前缀内可直接读");
    assert_eq!(caps.origin(), "plugin-self-description");
    assert_eq!(
        cordis_host::loaded_instances(),
        0,
        "读能力面不注册实例（pre-mount）"
    );
}
