//! Formal `/2` seal consumer evidence.
//!
//! Run manually with:
//! `CORDIS_CONSUMER_SEAL_DIR=dist/cordis-0.1.1-formal cargo test --manifest-path tests/consumer-service/Cargo.toml --test formal_seal`
//!
//! CI normally skips this because release seals are external artifacts.

use cordis_core::App;
use cordis_host::loaded_instances;
use cordis_host::loader::{load_with_source, DiskPluginSource, LoaderConfig};
use cordis_host::source::PluginSourcePolicy;
use std::path::PathBuf;

#[tokio::test]
async fn formal_seal_loads_through_its_allow_list() {
    let Some(seal) = std::env::var_os("CORDIS_CONSUMER_SEAL_DIR").map(PathBuf::from) else {
        eprintln!("skipping formal seal consumer: CORDIS_CONSUMER_SEAL_DIR is not set");
        return;
    };
    let allow_list =
        std::fs::read_to_string(seal.join("allow-list.json")).expect("formal seal allow-list.json");
    cordis_host::source::set_allow_unverified(false);
    cordis_host::source::set_active_policy(Some(std::sync::Arc::new(
        PluginSourcePolicy::from_json(&allow_list).expect("parse formal allow-list"),
    )));

    let config_path = std::env::temp_dir().join(format!(
        "cordis-formal-seal-consumer-{}.yml",
        std::process::id()
    ));
    std::fs::write(
        &config_path,
        "- id: greeting\n  name: greeting\n- id: hello\n  name: hello\n",
    )
    .expect("write formal seal consumer config");
    let app = App::new();
    let mut config = LoaderConfig::new(config_path.to_string_lossy().into_owned());
    config.plugin_dir = Some(seal.join("plugins").to_string_lossy().into_owned());
    let state = load_with_source(
        app.root.clone(),
        config,
        std::sync::Arc::new(DiskPluginSource),
    )
    .await
    .expect("formal /2 seal must load through its allow-list");
    assert_eq!(state.mounted_labels(), ["greeting", "hello"]);
    app.close().await;
    assert_eq!(loaded_instances(), 0);
    cordis_host::source::set_active_policy(None);
    let _ = std::fs::remove_file(config_path);
}
