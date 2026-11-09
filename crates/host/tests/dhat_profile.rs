#![cfg(feature = "dhat-heap")]

//! Optional dhat heap profile for the real staged-load path.
//!
//! Run manually:
//! `cargo test -p rust-cordis-host --test dhat_profile -- --ignored --nocapture`
//!
//! The test uses the frozen v1 cdylib, performs 100 real stage/open/drop cycles,
//! and asserts that the final live heap stays bounded. dhat is a dev-only
//! allocator; it is intentionally not part of normal CI.

mod fixture_support;

use cordis_core::Plugin;
use cordis_host::{loaded_instances, stage_plugin, staged_leaked, LoadedPlugin};
use fixture_support::{build_fixture, TempDir};

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

#[test]
#[ignore = "run manually for heap profiling"]
fn real_open_staged_path_keeps_live_heap_bounded() {
    let _profiler = dhat::Profiler::builder().testing().build();
    let target = TempDir::new("dhat-target");
    let artifact = build_fixture(target.path());
    let source_dir = TempDir::new("dhat-source");
    let source = source_dir
        .path()
        .join(artifact.file_name().expect("夹具产物必须有文件名"));
    std::fs::copy(&artifact, &source).expect("复制夹具");

    for cycle in 0..100 {
        let staged = stage_plugin(&source, cycle as u64 + 1).expect("暂存真实夹具");
        let plugin = LoadedPlugin::open_staged(&staged).expect("打开真实暂存副本");
        assert!(plugin.ping());
        drop(plugin);
        assert!(!staged.exists());
    }
    assert_eq!(staged_leaked(), 0);
    assert_eq!(loaded_instances(), 0);

    let stats = dhat::HeapStats::get();
    eprintln!(
        "dhat: total_blocks={} total_bytes={} max_bytes={} curr_blocks={} curr_bytes={}",
        stats.total_blocks, stats.total_bytes, stats.max_bytes, stats.curr_blocks, stats.curr_bytes
    );
    dhat::assert!(stats.total_blocks > 100, "真实装载路径应产生多次分配");
    dhat::assert!(stats.curr_bytes < 8 * 1024 * 1024, "最终存活堆超过 8 MiB");
}
