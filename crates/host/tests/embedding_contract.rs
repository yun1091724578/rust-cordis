//! Consumer-neutral embedding contract: only public cordis APIs.
//!
//! This test intentionally does not use `crate::` internals or agent concepts.
//! It pins the SDK path an external service runtime would take:
//! App -> LoaderConfig::new -> load_with_source -> mounted lifecycle -> close.

use cordis_core::{plugin_fn, App, Config, Ctx, Inject, Plugin};
use cordis_host::loaded_instances;
use cordis_host::loader::{load_with_source, LoadedFile, LoaderConfig, PluginSource};
use cordis_host::patch::Entry;
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
