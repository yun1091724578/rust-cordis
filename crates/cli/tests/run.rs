//! CLI 的 `validate` / `run`：先验证“不挂载”，再验证“挂载后能干净关闭”。
//!
//! 独立测试二进制：`set_plugin_source` 是进程级设置。

use cordis_core::{Config, Ctx, Inject, Plugin, PluginKey};
use cordis_host::loader::{LoadedFile, PluginSource};
use cordis_host::patch::Entry;
use std::path::PathBuf;
use std::sync::{Arc, Once};

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
                Inject::default()
            }
            fn apply(
                &self,
                _ctx: Ctx,
                _config: Config,
            ) -> cordis_core::BoxFuture<cordis_core::Result<()>> {
                let fail = self.fail;
                Box::pin(async move {
                    if fail {
                        Err("intentional plugin startup failure".into())
                    } else {
                        Ok(())
                    }
                })
            }
        }
        match entry.name.as_str() {
            "agent" | "legacy" | "boom" => {
                let name = entry.name.clone();
                let path = PathBuf::from(format!("<fake:{name}>"));
                Ok(LoadedFile {
                    plugin: Arc::new(Fake {
                        name,
                        fail: entry.name == "boom",
                    }),
                    source: path.clone(),
                    staged: path,
                    fingerprint: None,
                })
            }
            other => Err(format!("假插件源不认识 \"{other}\"")),
        }
    }
}

static INIT: Once = Once::new();

fn source() -> Arc<dyn PluginSource> {
    INIT.call_once(|| cordis_host::set_plugin_source(Arc::new(FakeSource)));
    Arc::new(FakeSource)
}

fn write_config(name: &str, text: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("cordis-cli-{name}-{}.yml", std::process::id()));
    std::fs::write(&path, text).expect("写配置");
    path
}

#[tokio::test]
async fn validate_applies_patch_layers_without_mounting() {
    let config = write_config("validate", "- id: agent\n  name: agent\n");
    let patch = write_config(
        "validate-patch",
        "- insert:\n    - id: legacy\n      name: legacy\n",
    );

    let report = cordis_cli::validate(&config, &[patch]).expect("validate 成功");
    assert_eq!(report.inspect.mounted, 0, "validate 永不挂载");
    assert_eq!(
        report
            .inspect
            .rows
            .iter()
            .map(|row| row.label.as_str())
            .collect::<Vec<_>>(),
        vec!["agent", "legacy"],
        "patch 插入的行也必须被 validate 审阅"
    );
    assert!(
        report.warnings.is_empty(),
        "合法 patch 不应产生警告: {:?}",
        report.warnings
    );
    assert_eq!(cordis_host::loaded_instances(), 0, "validate 不得注册实例");
}

#[test]
fn validate_rejects_invalid_entry_and_patch() {
    let config = write_config("validate-invalid", "- 42\n");
    let error = cordis_cli::validate(&config, &[]).expect_err("非法条目必须被拒绝");
    assert!(error.contains("第 1 行"), "{error}");
    assert!(error.contains("映射"), "{error}");

    let valid = write_config("validate-valid", "- id: agent\n  name: agent\n");
    let patch = write_config("validate-invalid-patch", "- 42\n");
    let error = cordis_cli::validate(&valid, &[patch]).expect_err("非法 patch 必须被拒绝");
    assert!(error.contains("patch 第 1 条"), "{error}");
}

#[tokio::test]
async fn run_session_mounts_and_shuts_down_without_residue() {
    let config = write_config("run", "- id: agent\n  name: agent\n");
    let session =
        cordis_cli::RunSession::start(&config, cordis_cli::RunOptions::default(), source())
            .await
            .expect("run session 启动");
    assert_eq!(session.mounted_labels(), vec!["agent".to_string()]);

    let report = session.shutdown().await;
    assert_eq!(
        report.mounted,
        vec!["agent".to_string()],
        "报告记录关闭前的装载面"
    );
    assert_eq!(cordis_host::loaded_instances(), 0, "关闭后不得残留宿主实例");
}

#[tokio::test]
async fn run_with_source_returns_after_shutdown_signal() {
    let config = write_config("run-signal", "- id: agent\n  name: agent\n");
    let report = cordis_cli::run_with_source(
        &config,
        cordis_cli::RunOptions::default(),
        source(),
        async {},
    )
    .await
    .expect("run_with_source 成功");
    assert_eq!(report.mounted, vec!["agent".to_string()]);
    assert_eq!(cordis_host::loaded_instances(), 0);
}

#[tokio::test]
async fn invalid_config_is_classified_as_config_error() {
    let config = write_config("invalid", "- 42\n");
    let error =
        match cordis_cli::RunSession::start(&config, cordis_cli::RunOptions::default(), source())
            .await
        {
            Ok(_) => panic!("非法配置不应启动成功"),
            Err(error) => error,
        };
    assert_eq!(error.kind, cordis_cli::RunErrorKind::Config);
    assert_eq!(error.exit_code(), 3);
}

#[tokio::test]
async fn plugin_start_failure_is_classified_as_runtime_error() {
    let config = write_config("boom", "- id: boom\n  name: boom\n");
    let error =
        match cordis_cli::RunSession::start(&config, cordis_cli::RunOptions::default(), source())
            .await
        {
            Ok(_) => panic!("插件 apply 失败不应启动成功"),
            Err(error) => error,
        };
    assert_eq!(error.kind, cordis_cli::RunErrorKind::Runtime);
    assert_eq!(error.exit_code(), 4);
}
