//! M5：**预挂载探测**（A5 的机制）——`probe_capabilities` 在**不挂载**的前提下
//! 拿到一份插件的能力面（inject / provision / volatile）。
//!
//! 为什么这条能进 `cargo test`（D9 只把**真 DLL/HMR** 断言留给 demo/spike）：
//! 探测走的是 [`PluginSource`] seam（M1 的 `set_plugin_source`），
//! 于是"解析 → 暂存 → 打开 → 探活 → 读能力 → 立刻放手"这条链在测试里
//! 用假源跑通；真 DLL 那一半由 demo/spike/CLI 覆盖。
//!
//! 本文件**独占一个测试二进制**：`set_plugin_source` 是进程级设置。

use cordis_core::{Config, Ctx, Inject, Plugin, PluginKey};
use cordis_host::loader::{LoadedFile, PluginSource};
use cordis_host::patch::Entry;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// 假源：只认一个名字，能力面写死在里面（含 provision 与 volatile）。
struct ProbeSource;

impl PluginSource for ProbeSource {
    fn load(
        &self,
        entry: &Entry,
        _search: &[PathBuf],
        _revision: u64,
    ) -> Result<LoadedFile, String> {
        if entry.name.as_str() != "<probe:m5>" {
            return Err(format!("假插件源不认识 \"{}\"", entry.name));
        }
        struct Fake;
        impl Plugin for Fake {
            fn name(&self) -> &str {
                "probe-m5"
            }
            fn key(&self) -> PluginKey {
                Arc::from("probe-m5")
            }
            fn inject(&self) -> Inject {
                Inject(vec![("tools".into(), Config::Null)])
            }
            fn provision(&self) -> Vec<String> {
                vec!["m5.svc".into()]
            }
            fn volatile_paths(&self) -> Vec<String> {
                vec!["data".into()]
            }
            fn config_schema(&self) -> Option<String> {
                Some(r#"{"type":"object","required":["name"]}"#.into())
            }
            fn apply(
                &self,
                _ctx: Ctx,
                _config: Config,
            ) -> cordis_core::BoxFuture<cordis_core::Result<()>> {
                Box::pin(async { Ok(()) })
            }
        }
        let path = PathBuf::from(&entry.name);
        Ok(LoadedFile {
            plugin: Arc::new(Fake),
            source: path.clone(),
            staged: path,
            fingerprint: None,
        })
    }
}

/// M5-10（**A5**）：挂载之前读到 inject / provision / volatile，且**没有**任何实例被注册。
#[tokio::test]
async fn probe_capabilities_reads_provision_without_mounting() {
    cordis_host::set_plugin_source(Arc::new(ProbeSource));

    let caps = cordis_host::probe_capabilities(Path::new("<probe:m5>")).expect("探测成功");
    assert_eq!(caps.plugin, "probe-m5");
    assert_eq!(caps.inject, vec!["tools".to_string()]);
    assert_eq!(caps.provision, vec!["m5.svc".to_string()]);
    assert_eq!(caps.volatile, vec!["data".to_string()]);
    assert_eq!(
        caps.schema(),
        Some(r#"{"type":"object","required":["name"]}"#)
    );
    assert_eq!(caps.schema_error(), None);
    assert_eq!(caps.origin(), "plugin-self-description");

    // "不挂载"的证据：宿主全局实例表空（没有为它注册过实例）。
    assert_eq!(cordis_host::loaded_instances(), 0, "探测不注册实例");
}
