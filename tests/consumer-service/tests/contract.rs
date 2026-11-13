//! External consumer contract: only public path dependencies.
//!
//! This package is deliberately a separate workspace. It must not use
//! `cordis_host::...` test helpers or repository-internal modules.

use cordis_core::{plugin_fn, App, Config, Ctx, Inject, Plugin};
use cordis_host::loader::{load_with_source, LoadedFile, LoaderConfig, PluginSource};
use cordis_host::patch::Entry;
use std::path::PathBuf;
use std::sync::Arc;

struct Source;

impl PluginSource for Source {
    fn load(
        &self,
        entry: &Entry,
        _search: &[PathBuf],
        _revision: u64,
    ) -> Result<LoadedFile, String> {
        let plugin: Arc<dyn Plugin> = plugin_fn(
            "consumer-service",
            Inject::default(),
            |_ctx: Ctx, _config: Config| Box::pin(async { Ok(()) }),
        );
        let path = PathBuf::from(format!("<consumer:{}>", entry.name));
        Ok(LoadedFile {
            plugin,
            source: path.clone(),
            staged: path,
            fingerprint: None,
        })
    }
}

#[tokio::test]
async fn external_service_consumer_loads_and_closes() {
    let path = std::env::temp_dir().join(format!(
        "cordis-consumer-service-{}.yml",
        std::process::id()
    ));
    std::fs::write(&path, "- id: demo\n  name: demo\n").expect("write config");
    let app = App::new();
    let state = load_with_source(
        app.root.clone(),
        LoaderConfig::new(path.to_string_lossy().into_owned()),
        Arc::new(Source),
    )
    .await
    .expect("external consumer must load");
    assert_eq!(state.mounted_labels(), ["demo"]);
    app.close().await;
    let _ = std::fs::remove_file(path);
}
