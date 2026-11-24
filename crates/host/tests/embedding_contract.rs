//! Consumer-neutral embedding contract: only public cordis APIs.
//!
//! This test intentionally does not use `crate::` internals or agent concepts.
//! It pins the SDK path an external service runtime would take:
//! App -> LoaderConfig::new -> load_with_source -> mounted lifecycle -> close.

use cordis_core::{plugin_fn, App, CloseStatus, Config, Ctx, Dyn, FiberState, Inject, Plugin};
use cordis_host::loaded_instances;
use cordis_host::loader::{load_with_source, LoadedFile, LoaderConfig, PluginSource};
use cordis_host::patch::Entry;
use cordis_host::{
    lifecycle_policy_for, method_allow_list_for, set_lifecycle_policy_for,
    set_method_allow_list_for, set_startup_policy_for, startup_policy_for, Capabilities,
    DispatchPolicy, FailureCategory, FailureDisposition, HostLimits, LifecyclePolicy,
    MethodAllowList, RowStartup, StartupPolicy,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;

struct OnePluginSource;

impl PluginSource for OnePluginSource {
    fn load(
        &self,
        entry: &Entry,
        _search: &[PathBuf],
        _revision: u64,
    ) -> Result<LoadedFile, String> {
        let plugin: Arc<dyn Plugin> = plugin_fn(
            "embedding-service",
            Inject::default(),
            |_ctx: Ctx, _config: Config| Box::pin(async { Ok(()) }),
        );
        let path = PathBuf::from(format!("<embedding:{}>", entry.name));
        Ok(LoadedFile {
            plugin,
            source: path.clone(),
            staged: path,
            fingerprint: None,
        })
    }
}

fn config_path(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "cordis-embedding-contract-{}-{tag}.yml",
        std::process::id()
    ));
    std::fs::write(&path, "- id: demo\n  name: demo\n").expect("write embedding config");
    path
}

struct ServiceSource;

impl PluginSource for ServiceSource {
    fn load(
        &self,
        entry: &Entry,
        _search: &[PathBuf],
        _revision: u64,
    ) -> Result<LoadedFile, String> {
        let plugin: Arc<dyn Plugin> = match entry.name.as_str() {
            "provider" => plugin_fn("embedding-provider", Inject::default(), |ctx: Ctx, _| {
                Box::pin(async move {
                    ctx.provide("demo.svc", cordis_core::Dyn::new(String::from("value")))?;
                    Ok(())
                })
            }),
            "consumer" => plugin_fn(
                "embedding-consumer",
                Inject::of(&["demo.svc"]),
                |ctx: Ctx, _| {
                    Box::pin(async move {
                        let value = ctx.get("demo.svc").expect("injected service");
                        let text = value.downcast::<String>().expect("service type");
                        assert_eq!(&*text, "value");
                        Ok(())
                    })
                },
            ),
            other => return Err(format!("unexpected embedding plugin {other}")),
        };
        let path = PathBuf::from(format!("<embedding:{}>", entry.name));
        Ok(LoadedFile {
            plugin,
            source: path.clone(),
            staged: path,
            fingerprint: None,
        })
    }
}

struct VersionedPlugin;

impl Plugin for VersionedPlugin {
    fn name(&self) -> &str {
        "versioned-embedding"
    }
    fn inject(&self) -> Inject {
        Inject::default()
    }
    fn version(&self) -> Option<&str> {
        Some("3.4.5")
    }
    fn apply(&self, _ctx: Ctx, _config: Config) -> cordis_core::BoxFuture<cordis_core::Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

#[test]
fn capabilities_expose_optional_plugin_version() {
    let caps = Capabilities::of_plugin(&VersionedPlugin);
    assert_eq!(caps.version.as_deref(), Some("3.4.5"));
    let anonymous = plugin_fn("anonymous-caps", Inject::default(), |_, _| {
        Box::pin(async { Ok(()) })
    });
    assert_eq!(Capabilities::of_plugin(&*anonymous).version, None);
}

#[test]
fn dispatch_policy_constructor_preserves_optional_timeouts() {
    let policy = DispatchPolicy::new(Some(10), None);
    assert_eq!(policy.timeout_ms, Some(10));
    assert_eq!(policy.outbound_timeout_ms, None);
}

#[test]
fn host_limits_constructor_rejects_zero_and_preserves_valid_values() {
    let limits = HostLimits::new(8, 16, 4, 2).expect("valid limits");
    assert_eq!(limits.max_instances_per_app, 8);
    assert_eq!(limits.max_dispatches_per_instance, 16);
    assert_eq!(limits.max_pending_per_instance, 4);
    assert_eq!(limits.max_streams_per_instance, 2);
    assert!(HostLimits::new(0, 16, 4, 2).is_err());
}

/// 0.1 的配置 DTO 是公开可字面量构造的。这个测试故意让未来加
/// `#[non_exhaustive]` 时在这里变红，逼出一次显式迁移决策。
#[test]
fn config_dtos_remain_literal_constructible_in_0_1() {
    let _policy = DispatchPolicy {
        timeout_ms: Some(1),
        outbound_timeout_ms: Some(2),
    };
    let _limits = HostLimits {
        max_instances_per_app: 8,
        max_dispatches_per_instance: 16,
        max_pending_per_instance: 4,
        max_streams_per_instance: 2,
    };
    let _lifecycle = LifecyclePolicy {
        apply_timeout_ms: Some(10),
        drain_timeout_ms: Some(20),
        dispose_timeout_ms: Some(30),
    };
}

#[tokio::test]
async fn lifecycle_policy_and_close_with_report_work_via_public_api() {
    assert!(LifecyclePolicy::new(Some(0), None, None).is_err());
    let policy = LifecyclePolicy::new(Some(1_000), Some(1_000), Some(1_000)).expect("valid policy");
    let app = App::new();
    set_lifecycle_policy_for(&app.root, policy).expect("valid policy accepted");
    assert_eq!(lifecycle_policy_for(&app.root), policy);
    assert!(
        set_lifecycle_policy_for(
            &app.root,
            LifecyclePolicy {
                drain_timeout_ms: Some(0),
                ..LifecyclePolicy::default()
            }
        )
        .is_err(),
        "字面量构造的 Some(0) 预算必须被拒绝"
    );
    assert_eq!(
        lifecycle_policy_for(&app.root),
        policy,
        "拒绝非法预算不得覆盖原生效配置"
    );
    let report = app.close_with_report(None).await;
    assert_eq!(report.status, CloseStatus::Completed);
    assert!(!report.still_running);
    assert!(report.errors.is_empty());
}

struct Tagged {
    inner: Arc<dyn Plugin>,
    version: &'static str,
}

impl Plugin for Tagged {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn key(&self) -> cordis_core::PluginKey {
        self.inner.key()
    }
    fn inject(&self) -> Inject {
        self.inner.inject()
    }
    fn version(&self) -> Option<&str> {
        Some(self.version)
    }
    fn provision(&self) -> Vec<String> {
        self.inner.provision()
    }
    fn volatile_paths(&self) -> Vec<String> {
        self.inner.volatile_paths()
    }
    fn config_schema(&self) -> Option<String> {
        self.inner.config_schema()
    }
    fn validate(&self, raw: Config) -> Result<Config, cordis_core::ValidationError> {
        self.inner.validate(raw)
    }
    fn apply(&self, ctx: Ctx, config: Config) -> cordis_core::BoxFuture<cordis_core::Result<()>> {
        self.inner.apply(ctx, config)
    }
}

#[tokio::test]
async fn plugin_versions_do_not_gate_service_composition() {
    let app = App::new();
    let provider: Arc<dyn Plugin> = Arc::new(Tagged {
        inner: plugin_fn("versioned-provider", Inject::default(), |ctx: Ctx, _| {
            Box::pin(async move {
                ctx.provide("versioned.svc", Dyn::new(7u32))?;
                Ok(())
            })
        }),
        version: "1.0.0",
    });
    let provider_fiber = app.root.plugin(provider, Config::Null);
    tokio::time::timeout(std::time::Duration::from_secs(5), provider_fiber.settle())
        .await
        .expect("provider settle");
    assert_eq!(provider_fiber.state(), FiberState::Active);

    let consumer: Arc<dyn Plugin> = Arc::new(Tagged {
        inner: plugin_fn(
            "versioned-consumer",
            Inject::of(&["versioned.svc"]),
            |ctx: Ctx, _| {
                Box::pin(async move {
                    assert_eq!(
                        ctx.get("versioned.svc")
                            .and_then(|value| value.downcast::<u32>())
                            .map(|value| *value),
                        Some(7)
                    );
                    Ok(())
                })
            },
        ),
        version: "9.9.9",
    });
    let consumer_fiber = app.root.plugin(consumer, Config::Null);
    tokio::time::timeout(std::time::Duration::from_secs(5), consumer_fiber.settle())
        .await
        .expect("consumer settle");
    assert_eq!(
        consumer_fiber.state(),
        FiberState::Active,
        "Plugin::version 差异不得成为服务组合门禁"
    );
    app.close().await;
}

#[tokio::test]
async fn public_api_composes_two_services_without_agent_concepts() {
    let path = config_path("services");
    std::fs::write(
        &path,
        "- id: provider\n  name: provider\n- id: consumer\n  name: consumer\n",
    )
    .expect("write service config");
    let app = App::new();
    let state = load_with_source(
        app.root.clone(),
        LoaderConfig::new(path.to_string_lossy().into_owned()),
        Arc::new(ServiceSource),
    )
    .await
    .expect("service composition must load");

    assert_eq!(state.mounted_labels(), ["consumer", "provider"]);
    app.close().await;
    let _ = std::fs::remove_file(Path::new(&path));
}

#[tokio::test]
async fn public_api_can_load_embed_and_close() {
    let path = config_path("basic");
    let app = App::new();
    let config = LoaderConfig::new(path.to_string_lossy().into_owned());
    let state = load_with_source(app.root.clone(), config, Arc::new(OnePluginSource))
        .await
        .expect("public loader path must work");

    assert_eq!(state.mounted_labels(), ["demo"]);
    // 进程内 fake plugin 不经过 ABI 实例表；实例计数只适用于 cdylib 路径。
    assert_eq!(loaded_instances(), 0);

    app.close().await;
    assert_eq!(loaded_instances(), 0, "close must release plugin instances");
    let _ = std::fs::remove_file(Path::new(&path));
}

#[tokio::test]
async fn host_startup_and_method_allow_list_public_contract() {
    // 0.1 兼容性约束：Entry 字面量构造不得被新增字段破坏（ADR 0006）。
    let entry = Entry {
        id: Some("demo".into()),
        name: "demo".into(),
        config: None,
        group: false,
        disabled: false,
        disabled_expr: None,
        inject: None,
        isolate: None,
        intercept: None,
        children: Vec::new(),
    };
    assert_eq!(entry.name, "demo");

    let app = App::new();
    let startup = StartupPolicy::new()
        .with_source(FailureDisposition::Fail)
        .with_integrity(FailureDisposition::Fail)
        .with_persisted_state(FailureDisposition::Degrade)
        .with_optional_row("opt");
    assert_eq!(startup.source(), FailureDisposition::Fail);
    assert_eq!(startup.integrity(), FailureDisposition::Fail);
    assert_eq!(startup.persisted_state(), FailureDisposition::Degrade);
    assert_eq!(startup.row_startup("opt", None), Some(RowStartup::Optional));
    assert_eq!(FailureCategory::Source.as_str(), "source");
    set_startup_policy_for(&app.root, Some(startup.clone()));
    assert_eq!(startup_policy_for(&app.root), Some(startup));

    let allow = MethodAllowList::new().allow_row_method("demo", "svc", "ping");
    set_method_allow_list_for(&app.root, Some(allow.clone()));
    assert_eq!(method_allow_list_for(&app.root), Some(allow));

    app.close().await;
}
