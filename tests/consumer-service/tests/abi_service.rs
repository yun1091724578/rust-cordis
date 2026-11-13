//! Real ABI consumer: load greeting/hello cdylibs through public APIs.
//!
//! `cargo xtask consumer` builds the two example plugins and sets
//! `CORDIS_CONSUMER_PLUGIN_DIR`; direct `cargo test` without that env is not a
//! supported consumer path.

use cordis_core::App;
use cordis_host::loaded_instances;
use cordis_host::loader::{load_with_source, DiskPluginSource, LoaderConfig};
use cordis_host::source::{AllowedPlugin, PluginSourcePolicy};
use std::path::{Path, PathBuf};

fn write_sidecar(artifact: &Path, id: &str) -> AllowedPlugin {
    let sha256 = cordis_host::source::sha256_file(artifact).expect("hash artifact");
    let manifest = serde_json::json!({
        "id": id,
        "version": "0.1.1",
        "source_revision": "tree:consumer-service-test",
        "abi_version": 1,
        "sha256": sha256.clone(),
    });
    let sidecar = cordis_host::source::manifest_path(artifact);
    std::fs::write(&sidecar, serde_json::to_string_pretty(&manifest).unwrap())
        .expect("write sidecar");
    AllowedPlugin {
        id: id.to_string(),
        version: "0.1.1".to_string(),
        source_revision: "tree:consumer-service-test".to_string(),
        abi_version: 1,
        sha256,
    }
}

#[tokio::test]
async fn real_abi_plugins_pass_source_policy_and_compose() {
    let plugin_dir = PathBuf::from(
        std::env::var("CORDIS_CONSUMER_PLUGIN_DIR")
            .expect("run through `cargo xtask consumer` (plugin dir env missing)"),
    );
    let greeting = cordis_host::resolve_plugin_path("greeting", std::slice::from_ref(&plugin_dir))
        .expect("resolve greeting");
    let hello = cordis_host::resolve_plugin_path("hello", std::slice::from_ref(&plugin_dir))
        .expect("resolve hello");
    let allowed = vec![
        write_sidecar(&greeting, "greeting"),
        write_sidecar(&hello, "hello"),
    ];
    cordis_host::source::set_allow_unverified(false);
    cordis_host::source::set_active_policy(Some(std::sync::Arc::new(PluginSourcePolicy::new(
        allowed,
    ))));

    let config_path =
        std::env::temp_dir().join(format!("cordis-consumer-policy-{}.yml", std::process::id()));
    std::fs::write(
        &config_path,
        "- id: greeting\n  name: greeting\n- id: hello\n  name: hello\n",
    )
    .expect("write policy consumer config");
    let app = App::new();
    let mut config = LoaderConfig::new(config_path.to_string_lossy().into_owned());
    config.plugin_dir = Some(plugin_dir.to_string_lossy().into_owned());
    let state = load_with_source(
        app.root.clone(),
        config,
        std::sync::Arc::new(DiskPluginSource),
    )
    .await
    .expect("source-policy ABI service composition must load");
    assert_eq!(state.mounted_labels(), ["greeting", "hello"]);
    app.close().await;
    assert_eq!(loaded_instances(), 0);
    cordis_host::source::set_active_policy(None);
    let _ = std::fs::remove_file(config_path);
}

#[tokio::test]
async fn real_abi_plugins_compose_services_and_close() {
    let plugin_dir = PathBuf::from(
        std::env::var("CORDIS_CONSUMER_PLUGIN_DIR")
            .expect("run through `cargo xtask consumer` (plugin dir env missing)"),
    );
    let config_path =
        std::env::temp_dir().join(format!("cordis-consumer-abi-{}.yml", std::process::id()));
    std::fs::write(
        &config_path,
        "- id: greeting\n  name: greeting\n- id: hello\n  name: hello\n",
    )
    .expect("write ABI consumer config");

    cordis_host::source::set_allow_unverified(true);
    let app = App::new();
    let mut config = LoaderConfig::new(config_path.to_string_lossy().into_owned());
    config.plugin_dir = Some(plugin_dir.to_string_lossy().into_owned());
    let state = load_with_source(
        app.root.clone(),
        config,
        std::sync::Arc::new(DiskPluginSource),
    )
    .await
    .expect("real ABI service composition must load");
    assert_eq!(state.mounted_labels(), ["greeting", "hello"]);
    assert!(
        loaded_instances() >= 2,
        "both ABI plugins must have instances"
    );

    app.close().await;
    assert_eq!(loaded_instances(), 0, "close must release ABI instances");
    cordis_host::source::set_allow_unverified(false);
    let _ = std::fs::remove_file(config_path);
}
