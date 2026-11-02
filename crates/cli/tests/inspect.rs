//! A5：`cordis-cli inspect` 在**挂载之前**给出每行的 inject / provision / volatile，
//! 并明说"此刻挂载了几行"（0）——能力清单是装载前就能审阅的东西。
//!
//! 测试走 **library 面**（`cordis_cli::inspect`）而不是子进程：只有进程内才能塞
//! 假 `PluginSource`（`set_plugin_source` 是进程级设置，D9 的 DLL 断言留给 spike）。
//! 本文件独占一个测试二进制。

use cordis_core::{Config, Ctx, Inject, Plugin, PluginKey};
use cordis_host::loader::{LoadedFile, PluginSource};
use cordis_host::patch::Entry;
use std::path::PathBuf;
use std::sync::Arc;

struct FakeSource;

impl PluginSource for FakeSource {
    fn load(
        &self,
        entry: &Entry,
        _search: &[PathBuf],
        _revision: u64,
    ) -> Result<LoadedFile, String> {
        struct Fake {
            name: String,
            provision: Vec<String>,
            volatile: Vec<String>,
            schema: Option<String>,
        }
        impl Plugin for Fake {
            fn name(&self) -> &str {
                &self.name
            }
            fn key(&self) -> PluginKey {
                Arc::from(self.name.as_str())
            }
            fn inject(&self) -> Inject {
                Inject::default()
            }
            fn provision(&self) -> Vec<String> {
                self.provision.clone()
            }
            fn volatile_paths(&self) -> Vec<String> {
                self.volatile.clone()
            }
            fn config_schema(&self) -> Option<String> {
                self.schema.clone()
            }
            fn apply(
                &self,
                _ctx: Ctx,
                _config: Config,
            ) -> cordis_core::BoxFuture<cordis_core::Result<()>> {
                Box::pin(async { Ok(()) })
            }
        }
        let (provision, volatile, schema) = match entry.name.as_str() {
            "agent" => (
                vec!["tools".to_string(), "llm".to_string()],
                vec!["data".to_string()],
                Some(r#"{"type":"object","required":["tools"]}"#.to_string()),
            ),
            // 未声明 provision 的行：**要被列出来**（M11 表里的那一行）。
            "legacy" => (vec![], vec![], None),
            "invalid" => (vec![], vec![], Some(r#"{"type":"object","#.to_string())),
            other => return Err(format!("假插件源不认识 \"{other}\"")),
        };
        let name = entry.name.clone();
        let path = PathBuf::from(format!("<fake:{name}>"));
        Ok(LoadedFile {
            plugin: Arc::new(Fake {
                name,
                provision,
                volatile,
                schema,
            }),
            source: path.clone(),
            staged: path,
            fingerprint: None,
        })
    }
}

const CONFIG: &str = concat!(
    "- id: agent\n",
    "  name: agent\n",
    "  inject:\n",
    "    required: [\"tools\"]\n",
    "    optional: [\"telemetry\"]\n",
    "- id: legacy\n",
    "  name: legacy\n",
    "- id: invalid\n",
    "  name: invalid\n",
);

/// A5：每行的 inject/provision/volatile 都能在装载前读到；`mounted == 0`；
/// 未声明 provision 的行**被列出**。
#[tokio::test]
async fn inspect_lists_capabilities_before_any_mount() {
    cordis_host::set_plugin_source(Arc::new(FakeSource));
    let path = std::env::temp_dir().join(format!("cordis-inspect-{}.yml", std::process::id()));
    std::fs::write(&path, CONFIG).expect("写配置");

    let report = cordis_cli::inspect(&path).expect("inspect 成功");
    assert_eq!(report.mounted, 0, "inspect 永不挂载（A5 的判据）");
    assert_eq!(report.rows.len(), 3);

    let agent = &report.rows[0];
    assert_eq!(agent.label, "agent");
    assert_eq!(agent.plugin, "agent");
    assert_eq!(
        agent.inject_required,
        vec!["tools".to_string()],
        "行级 required 与插件声明合并"
    );
    assert_eq!(
        agent.inject_optional,
        vec!["telemetry".to_string()],
        "optional 必须单独保留，不能被当成服务名 required"
    );
    assert_eq!(
        agent.inject,
        vec!["telemetry".to_string(), "tools".to_string()],
        "联合视图保留全部依赖名（升序）"
    );
    assert_eq!(
        agent.provision,
        vec!["tools".to_string(), "llm".to_string()]
    );
    assert_eq!(agent.volatile, vec!["data".to_string()]);
    assert_eq!(
        agent.schema.as_deref(),
        Some(r#"{"type":"object","required":["tools"]}"#)
    );

    let json = report.to_json();
    assert_eq!(
        json["rows"][0]["inject"]["required"],
        serde_json::json!(["tools"]),
        "JSON 的 required 必须是真实服务名"
    );
    assert_eq!(
        json["rows"][0]["inject"]["optional"],
        serde_json::json!(["telemetry"]),
        "JSON 的 optional 必须单独给出"
    );
    assert_eq!(json["source"], "plugin-self-description");
    assert_eq!(json["inject_source"], "plugin+config-entry");
    assert_eq!(
        json["rows"][0]["schema"], r#"{"type":"object","required":["tools"]}"#,
        "schema 必须出现在装载前 JSON 面"
    );
    assert!(json["rows"][1]["schema"].is_null());
    assert!(json["rows"][2]["schema"].is_null());
    assert!(
        json["rows"][2]["schema_error"]
            .as_str()
            .is_some_and(|e| e.contains("JSON 对象")),
        "JSON 也要保留非法 schema 诊断：{}",
        json["rows"][2]
    );

    let legacy = &report.rows[1];
    assert_eq!(legacy.label, "legacy");
    assert!(legacy.provision.is_empty());
    assert!(legacy.schema.is_none());
    let invalid = &report.rows[2];
    assert_eq!(invalid.label, "invalid");
    assert!(invalid.schema.is_none());
    assert!(
        invalid
            .schema_error
            .as_deref()
            .is_some_and(|e| e.contains("JSON 对象")),
        "非法 schema 不能与未声明混在一起：{invalid:?}"
    );
    assert_eq!(
        report.undeclared,
        vec!["legacy".to_string(), "invalid".to_string()],
        "未声明 provision 的行被单独列出（装载前就能看见「权限面」）"
    );
    // 打印面：CLI 的输出由 `render` 生成，测试断言它包含每行的三项。
    let text = report.render();
    assert!(text.contains("mounted = 0"), "{text}");
    assert!(text.contains("inject required: tools"), "{text}");
    assert!(text.contains("inject optional: telemetry"), "{text}");
    assert!(text.contains("provision: tools, llm"), "{text}");
    assert!(
        text.contains("schema: {\"required\":[\"tools\"],\"type\":\"object\"}"),
        "{text}"
    );
    assert!(
        text.contains("capability source = plugin-self-description (not approval)"),
        "{text}"
    );
    assert!(
        text.contains("inject source = plugin+config-entry"),
        "{text}"
    );
    assert!(text.contains("schema: (invalid:"), "{text}");
    assert!(text.contains("legacy"), "{text}");

    // `mounted == 0` 是 A5 的语义字段（inspect 的返回值），但它是**恒真**的——
    // 单独断言它证明不了"没挂载"。真正的观测面是宿主全局实例表：inspect 不注册
    // 实例。注意探针仍会加载动态库并调用 OP_PING / OP_VOLATILE_PATHS；它不是
    // “只读文件头”，只是不建 fiber/实例。
    assert_eq!(
        cordis_host::loaded_instances(),
        0,
        "inspect 不挂载、不注册实例（A5 的可观测判据）"
    );
}

#[test]
fn inspect_uses_the_loader_stable_labels_for_same_named_rows() {
    cordis_host::set_plugin_source(Arc::new(FakeSource));
    let entries =
        cordis_host::patch::parse_entries("- name: agent\n- name: agent\n").expect("parse");
    let report = cordis_cli::inspect_entries(&entries, &[]).expect("inspect 成功");
    let labels: Vec<&str> = report.rows.iter().map(|row| row.label.as_str()).collect();
    assert_eq!(labels, vec!["agent#0", "agent#1"]);
}

/// A5 守卫：**空组**也是组，不是插件。
///
/// 旧实现按"有没有子条目"判组：`group: true, config: []` 会被当成一个名叫组 id 的
/// 插件去探测（然后必然报"找不到插件"）。判据必须是 `group` 声明本身。
#[test]
fn inspect_empty_group_never_probes_and_reports_no_rows() {
    let entries = cordis_host::patch::parse_entries(
        "- id: empty\n  name: not-a-real-plugin\n  group: true\n  config: []\n",
    )
    .expect("parse");
    assert_eq!(entries.len(), 1);
    assert!(entries[0].group && entries[0].children.is_empty());

    let report =
        cordis_cli::inspect_entries(&entries, &[]).expect("空组不该触发任何探测（否则这里会失败）");
    assert!(report.rows.is_empty(), "空组没有可审阅的行：{report:?}");
    assert!(report.undeclared.is_empty(), "{report:?}");
    assert_eq!(report.mounted, 0, "inspect 永不挂载（A5 的语义字段）");
    assert_eq!(cordis_host::loaded_instances(), 0, "空组也不得注册实例");
}
