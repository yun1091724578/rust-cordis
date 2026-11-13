//! Real ABI consumer: load greeting/hello cdylibs through public APIs.
//!
//! `cargo xtask consumer` builds the two example plugins and sets
//! `CORDIS_CONSUMER_PLUGIN_DIR`; direct `cargo test` without that env is not a
//! supported consumer path.

use cordis_core::App;
use cordis_host::loaded_instances;
use cordis_host::loader::{load_with_source, DiskPluginSource, LoaderConfig};
use std::path::PathBuf;

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
