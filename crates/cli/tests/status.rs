//! `cordis status`：装载后解释未就绪原因（独立测试二进制，假插件源）。

use cordis_core::{
    BoxFuture, CloseStatus, Config, Ctx, Disposable, ExternService, Inject, PendingReason, Plugin,
    PluginKey,
};
use cordis_host::loader::{LoadedFile, PluginSource};
use cordis_host::patch::Entry;
use cordis_host::{FailureDisposition, StartupPolicy};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;

struct PendingSource;

struct StatusService;

impl ExternService for StatusService {
    fn service(&self) -> &str {
        "status.svc"
    }

    fn version(&self) -> u32 {
        3
    }

    fn methods(&self) -> Vec<String> {
        vec!["beta".to_string(), "alpha".to_string()]
    }

    fn call(
        &self,
        _caller_meta: Option<Config>,
        _method: String,
        _args: Config,
    ) -> BoxFuture<cordis_core::Result<Config>> {
        Box::pin(async { Ok(Config::Null) })
    }
}

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
            iface: bool,
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
            fn config_schema(&self) -> Option<String> {
                self.iface
                    .then(|| r#"{"type":"object","required":["name"]}"#.to_string())
            }
            fn apply(
                &self,
                ctx: Ctx,
                _config: Config,
            ) -> cordis_core::BoxFuture<cordis_core::Result<()>> {
                if self.fail {
                    return Box::pin(async { Err("boom".into()) });
                }
                if self.name == "panic-dispose" {
                    return Box::pin(async move {
                        ctx.effect(|_| Disposable::sync(|| panic!("cli-status-disposer-boom")));
                        Ok(())
                    });
                }
                if self.iface {
                    return Box::pin(async move {
                        ctx.provide_iface("status.svc", Arc::new(StatusService))?;
                        Ok(())
                    });
                }
                Box::pin(async { Ok(()) })
            }
        }
        let name = entry.name.clone();
        let (inject, provision, fail, iface) = match name.as_str() {
            "needs" => (vec!["missing@1"], vec![], false, false),
            "broken" => (vec![], vec![], true, false),
            "broken-provider" => (vec![], vec!["missing@1".to_string()], true, false),
            "panic-dispose" => (vec![], vec![], false, false),
            "iface" => (vec![], vec!["status.svc".to_string()], false, true),
            "a" => (vec!["b"], vec!["a".to_string()], false, false),
            "b" => (vec!["a"], vec!["b".to_string()], false, false),
            other => return Err(format!("假插件源不认识 \"{other}\"")),
        };
        let path = PathBuf::from(format!("<fake:{name}>"));
        Ok(LoadedFile {
            plugin: Arc::new(Fake {
                name,
                inject,
                provision,
                fail,
                iface,
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
        text.contains("provenance source = missing/manifest"),
        "没有 sidecar 的挂载行必须明确标注 missing/manifest：{text}"
    );
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
    assert_eq!(
        json["provenance"][0]["label"], "needs",
        "provenance 必须带行标：{json}"
    );
    assert_eq!(
        json["provenance"][0]["source"], "missing/manifest",
        "没有 sidecar 的挂载行必须在 JSON 里显示 missing/manifest：{json}"
    );
    assert_eq!(cordis_host::loaded_instances(), 0, "status 必须关闭装载树");
}

#[tokio::test]
async fn status_services_methods() {
    let path = std::env::temp_dir().join(format!("cordis-status-iface-{}.yml", std::process::id()));
    std::fs::write(&path, "- id: iface\n  name: iface\n").expect("写配置");

    let report = cordis_cli::status_with_source(
        &path,
        cordis_cli::RunOptions::default(),
        Arc::new(PendingSource),
    )
    .await
    .expect("status 装载成功");

    assert_eq!(report.capabilities.len(), 1, "{report:?}");
    let row = &report.capabilities[0];
    assert_eq!(row.label, "iface");
    assert_eq!(row.plugin, "iface");
    assert_ne!(row.uid, u64::MAX, "活实例的 uid 不能是 UID_DISPOSED");
    assert_eq!(row.sha256, "builtin");
    assert_eq!(row.provision, vec!["status.svc".to_string()]);
    assert!(
        row.schema
            .as_deref()
            .is_some_and(|text| text.contains("required")),
        "capabilities 必须导出插件自述的配置 schema：{row:?}"
    );
    assert_eq!(row.services.len(), 1, "{row:?}");
    assert_eq!(row.services[0].name, "status.svc");
    assert_eq!(row.services[0].version, 3);
    assert_eq!(row.iface_spec_sha256, None);
    assert_eq!(
        row.services[0].methods,
        vec!["alpha".to_string(), "beta".to_string()],
        "方法名必须是排序后的稳定集合"
    );

    let json = report.to_json();
    assert_eq!(
        json["capabilities"][0]["services"][0]["methods"],
        serde_json::json!(["alpha", "beta"]),
        "{json}"
    );
    assert_eq!(json["capabilities"][0]["services"][0]["name"], "status.svc");
    assert_eq!(json["capabilities"][0]["services"][0]["version"], 3);
    assert_eq!(json["capabilities"][0]["iface_spec_sha256"], Value::Null);
    assert_eq!(
        json["capabilities"][0]["schema"].as_str(),
        row.schema.as_deref()
    );
    let text = report.render();
    assert!(text.contains("capabilities:"), "{text}");
    assert!(
        text.contains("status.svc (version 3): alpha, beta"),
        "{text}"
    );
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
        .args([
            "status",
            path.to_str().unwrap(),
            "--no-strict",
            "--json",
            "--allow-unverified",
        ])
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
        .arg("--allow-unverified")
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

#[tokio::test]
async fn status_surfaces_failed_upstream_degraded_persisted_errors_and_close_report() {
    let dir = std::env::temp_dir().join(format!(
        "cordis-status-p24-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("建临时目录");

    // 1) 可选提供者失败 + 下游 Pending：status 必须展示 failedUpstream 与 degraded，且 is_healthy()=true。
    let cfg_opt = dir.join("opt.yml");
    std::fs::write(
        &cfg_opt,
        "- id: prov\n  name: broken-provider\n  optional: true\n- id: cons\n  name: needs\n",
    )
    .expect("写配置");
    let report = cordis_cli::status_with_source(
        &cfg_opt,
        cordis_cli::RunOptions::default(),
        Arc::new(PendingSource),
    )
    .await
    .expect("可选上游失败应降级返回 status");
    assert!(
        report.is_healthy(),
        "仅可选行降级且清理正常时应健康: {report:?}"
    );
    assert!(report.has_degraded(), "必须标记 has_degraded: {report:?}");
    assert_eq!(report.degraded.len(), 1);
    assert_eq!(report.degraded[0].label, "prov");
    assert_eq!(report.correlated_pending.len(), 1);
    assert_eq!(
        report.correlated_pending[0].missing[0].failed_upstream[0].label,
        "prov"
    );
    assert_eq!(report.close_report.status, CloseStatus::Completed);

    let text = report.render();
    assert!(text.contains("failed upstream: prov"), "{text}");
    assert!(text.contains("degraded:"), "{text}");
    let json = report.to_json();
    assert_eq!(
        json["pending"][0]["missing"][0]["failedUpstream"][0]["label"],
        "prov"
    );
    assert_eq!(json["degraded"][0]["label"], "prov");
    assert_eq!(json["close"]["status"], "completed");

    // 2) 损坏 patch + StartupPolicy::with_persisted_state(Degrade)：status 不被 precheck 误拦并上报 persisted_state_errors。
    let bad_patch = dir.join("broken.patch.yml");
    std::fs::write(&bad_patch, "not: [valid: yaml").expect("写坏 patch");
    let cfg_ok = dir.join("ok.yml");
    std::fs::write(&cfg_ok, "- id: iface\n  name: iface\n").expect("写主配置");
    let report_p = cordis_cli::status_with_source(
        &cfg_ok,
        cordis_cli::RunOptions {
            patches: vec![bad_patch.clone()],
            startup_policy: Some(
                StartupPolicy::new().with_persisted_state(FailureDisposition::Degrade),
            ),
            ..cordis_cli::RunOptions::default()
        },
        Arc::new(PendingSource),
    )
    .await
    .expect("PersistedState=Degrade 应跳过坏 patch 启动");
    assert!(report_p.is_healthy());
    assert!(report_p.has_degraded());
    assert_eq!(report_p.persisted_state_errors.len(), 1);
    assert_eq!(
        report_p.persisted_state_errors[0].label,
        bad_patch.display().to_string()
    );
    assert!(report_p.render().contains("persisted_state_errors:"));
    assert_eq!(
        report_p.to_json()["persisted_state_errors"][0]["degraded"],
        true
    );

    // 3) 卸载期 disposer panic：status 必须捕获 close_report 错误并判为不健康。
    let cfg_panic = dir.join("panic.yml");
    std::fs::write(&cfg_panic, "- id: bad-close\n  name: panic-dispose\n").expect("写配置");
    let report_c = cordis_cli::status_with_source(
        &cfg_panic,
        cordis_cli::RunOptions::default(),
        Arc::new(PendingSource),
    )
    .await
    .expect("装载成功、关闭时 disposer panic");
    assert!(!report_c.is_healthy(), "关闭产生清理错误不得判为健康");
    assert_eq!(
        report_c.close_report.status,
        CloseStatus::CompletedWithErrors
    );
    assert!(report_c.render().contains("close: completed_with_errors"));
    assert_eq!(
        report_c.to_json()["close"]["status"],
        "completed_with_errors"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
