//! `cordis status`：装载后解释未就绪原因（独立测试二进制，假插件源）。

use cordis_core::{Config, Ctx, Inject, PendingReason, Plugin, PluginKey};
use cordis_host::loader::{LoadedFile, PluginSource};
use cordis_host::patch::Entry;
use std::path::PathBuf;
use std::sync::Arc;

struct PendingSource;

impl PluginSource for PendingSource {
    fn load(
        &self,
        entry: &Entry,
        _search: &[PathBuf],
        _revision: u64,
    ) -> Result<LoadedFile, String> {
        struct Fake {
            name: String,
            inject: Vec<&'static str>,
            provision: Vec<String>,
            fail: bool,
        }
        impl Plugin for Fake {
            fn name(&self) -> &str {
                &self.name
            }
            fn key(&self) -> PluginKey {
                Arc::from(self.name.as_str())
            }
            fn inject(&self) -> Inject {
                Inject::of(&self.inject)
            }
            fn provision(&self) -> Vec<String> {
                self.provision.clone()
            }
            fn apply(
                &self,
                _ctx: Ctx,
                _config: Config,
            ) -> cordis_core::BoxFuture<cordis_core::Result<()>> {
                if self.fail {
                    return Box::pin(async { Err("boom".into()) });
                }
                Box::pin(async { Ok(()) })
            }
        }
        let name = entry.name.clone();
        let (inject, provision, fail) = match name.as_str() {
            "needs" => (vec!["missing@1"], vec![], false),
            "broken" => (vec![], vec![], true),
            "a" => (vec!["b"], vec!["a".to_string()], false),
            "b" => (vec!["a"], vec!["b".to_string()], false),
            other => return Err(format!("假插件源不认识 \"{other}\"")),
        };
        let path = PathBuf::from(format!("<fake:{name}>"));
        Ok(LoadedFile {
            plugin: Arc::new(Fake {
                name,
                inject,
                provision,
                fail,
            }),
            source: path.clone(),
            staged: path,
            fingerprint: None,
        })
    }
}

#[tokio::test]
async fn status_reports_the_precise_missing_inject_after_mounting() {
    let path = std::env::temp_dir().join(format!("cordis-status-{}.yml", std::process::id()));
    std::fs::write(&path, "- id: needs\n  name: needs\n").expect("写配置");

    let report = cordis_cli::status_with_source(
        &path,
        cordis_cli::RunOptions::default(),
        Arc::new(PendingSource),
    )
    .await
    .expect("status 装载成功");

    assert_eq!(report.mounted, vec!["needs".to_string()]);
    assert!(report.disabled.is_empty(), "{report:?}");
    assert!(report.failed.is_empty(), "{report:?}");
    assert!(report.cycles.is_empty(), "{report:?}");
    assert_eq!(report.pending.len(), 1, "{report:?}");
    assert_eq!(report.pending[0].label, "needs");
    assert_eq!(report.pending[0].missing[0].key, "missing@1");
    assert_eq!(report.pending[0].missing[0].reason, PendingReason::Missing);
    assert!(report.is_healthy());

    let text = report.render();
    assert!(text.contains("mounted = 1"), "{text}");
    assert!(text.contains("disabled = (none)"), "{text}");
    assert!(text.contains("cycles = (none)"), "{text}");
    assert!(text.contains("missing missing@1: missing"), "{text}");
    assert!(text.contains("failed = (none)"), "{text}");
    assert!(
        text.contains("capability source = plugin-self-description (not approval)"),
        "{text}"
    );

    let json = report.to_json();
    assert_eq!(json["source"], "plugin-self-description");
    assert_eq!(json["inject_source"], "plugin+config-entry");
    assert_eq!(json["mounted"], 1);
    assert_eq!(json["mounted_labels"], serde_json::json!(["needs"]));
    assert_eq!(json["pending"][0]["missing"][0]["reason"], "missing");
    assert_eq!(json["disabled"], serde_json::json!([]));
    assert_eq!(json["failed"], serde_json::json!([]));
    assert_eq!(cordis_host::loaded_instances(), 0, "status 必须关闭装载树");
}

#[tokio::test]
async fn status_reports_declaration_cycle_paths() {
    let path = std::env::temp_dir().join(format!("cordis-status-cycle-{}.yml", std::process::id()));
    std::fs::write(&path, "- id: a\n  name: a\n- id: b\n  name: b\n").expect("写配置");

    let report = cordis_cli::status_with_source(
        &path,
        cordis_cli::RunOptions::default(),
        Arc::new(PendingSource),
    )
    .await
    .expect("status 装载成功");

    assert_eq!(report.cycles, vec![vec!["a", "b", "a"]]);
    let text = report.render();
    assert!(text.contains("cycles:"), "{text}");
    assert!(text.contains("a -> b -> a"), "{text}");
    assert_eq!(cordis_host::loaded_instances(), 0, "status 必须关闭装载树");
}

#[tokio::test]
async fn status_reports_failed_rows_under_no_strict() {
    let path =
        std::env::temp_dir().join(format!("cordis-status-failed-{}.yml", std::process::id()));
    std::fs::write(&path, "- id: broken\n  name: broken\n").expect("写配置");

    let report = cordis_cli::status_with_source(
        &path,
        cordis_cli::RunOptions {
            strict: false,
            ..cordis_cli::RunOptions::default()
        },
        Arc::new(PendingSource),
    )
    .await
    .expect("--no-strict 下应返回诊断报告");

    assert!(report.mounted.is_empty(), "{report:?}");
    assert!(!report.is_healthy(), "{report:?}");
    assert_eq!(report.failed.len(), 1, "{report:?}");
    assert!(report.failed[0].contains("boom"), "{report:?}");
    assert!(report.render().contains("failed:"), "{report:?}");
    assert_eq!(cordis_host::loaded_instances(), 0, "失败行也要清理");
}

#[tokio::test]
async fn status_lists_disabled_rows_without_failing() {
    let path =
        std::env::temp_dir().join(format!("cordis-status-disabled-{}.yml", std::process::id()));
    std::fs::write(&path, "- id: sleep\n  name: needs\n  disabled: true\n").expect("写配置");

    let report = cordis_cli::status_with_source(
        &path,
        cordis_cli::RunOptions::default(),
        Arc::new(PendingSource),
    )
    .await
    .expect("休眠行不是装载失败");

    assert!(report.mounted.is_empty(), "{report:?}");
    assert_eq!(report.disabled, vec!["sleep".to_string()]);
    assert!(report.failed.is_empty(), "{report:?}");
    assert!(report.is_healthy());
    let text = report.render();
    assert!(text.contains("disabled:"), "{text}");
    assert!(text.contains("- sleep"), "{text}");
    assert_eq!(cordis_host::loaded_instances(), 0);
}

#[test]
fn cli_status_json_reports_failed_rows_and_exits_nonzero() {
    let dir = std::env::temp_dir().join(format!(
        "cordis-status-json-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("建临时目录");
    let path = dir.join("cordis.yml");
    std::fs::write(&path, "- id: broken\n  name: no-such-plugin\n").expect("写配置");

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_cordis"))
        .args(["status", path.to_str().unwrap(), "--no-strict", "--json"])
        .output()
        .expect("启动 cordis status --json");
    assert_eq!(output.status.code(), Some(4));
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout 必须是 JSON");
    assert_eq!(json["inject_source"], "plugin+config-entry");
    assert!(
        json["failed"]
            .as_array()
            .is_some_and(|rows| !rows.is_empty()),
        "JSON 必须保留失败行：{json}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cli_status_exits_nonzero_when_a_row_fails_under_no_strict() {
    let dir = std::env::temp_dir().join(format!(
        "cordis-status-cli-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("建临时目录");
    let path = dir.join("cordis.yml");
    std::fs::write(&path, "- id: broken\n  name: no-such-plugin\n").expect("写配置");

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_cordis"))
        .arg("status")
        .arg(&path)
        .arg("--no-strict")
        .output()
        .expect("启动 cordis status");
    assert_eq!(
        output.status.code(),
        Some(4),
        "失败行必须让 status 返回运行时错误码；stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("failed:"),
        "stdout 应列出失败行：{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let _ = std::fs::remove_dir_all(&dir);
}
