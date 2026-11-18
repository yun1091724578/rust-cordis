//! 真实 `open_staged` soak：每次走 `stage_plugin` → `LoadedPlugin::open_staged`
//! → `OP_PING` → drop，证明真实装载路径反复运行后暂存台账与实例表回到基线。
//!
//! 默认被 `#[ignore]` 跳过；`cargo xtask soak` 会连同 `soak` 一起跑。夹具是
//! `tests/fixtures/abi-v1`，只构建一次。
//!
//! 轮次读 `CORDIS_STAGED_SOAK_CYCLES`（默认 200）；`cargo xtask soak` 会显式读取并
//! 打印它。`0` / 非法值一律 panic——否则"跑 0 轮"会被当成通过。

mod fixture_support;

use cordis_core::Plugin;
use cordis_host::{loaded_instances, stage_plugin, staged_leaked, LoadedPlugin};
use fixture_support::{build_fixture, TempDir};
use std::path::Path;

fn cycles() -> usize {
    let raw = std::env::var("CORDIS_STAGED_SOAK_CYCLES").unwrap_or_else(|_| "200".to_string());
    let cycles = raw
        .parse::<usize>()
        .ok()
        .filter(|cycles| *cycles > 0)
        .unwrap_or_else(|| panic!("CORDIS_STAGED_SOAK_CYCLES 必须是正整数，实际 {raw:?}"));
    eprintln!("open_staged soak: {cycles} cycles");
    cycles
}

#[cfg(target_os = "linux")]
fn rss_bytes() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let resident_pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    Some(resident_pages.saturating_mul(4096))
}

#[cfg(not(target_os = "linux"))]
fn rss_bytes() -> Option<u64> {
    None
}

/// 跑一轮真实装载并断言台账回到基线，返回 `1` 供调用方累加轮次。
///
/// 计数与断言绑在同一处：删掉这里的断言，`opened` 也就没了来源。
fn open_staged_once(source: &Path, cycle: usize) -> usize {
    let staged = stage_plugin(source, cycle as u64 + 1).expect("暂存真实夹具");
    let plugin = LoadedPlugin::open_staged(&staged).expect("打开真实暂存副本");
    assert!(plugin.ping(), "cycle {cycle}: 真实夹具必须通过 OP_PING");
    assert_eq!(plugin.name(), "abi-v1-frozen");
    drop(plugin);
    assert!(
        !staged.exists(),
        "cycle {cycle}: LoadedPlugin drop 后暂存副本必须被删除"
    );
    assert_eq!(staged_leaked(), 0, "cycle {cycle}: 暂存台账必须归零");
    assert_eq!(loaded_instances(), 0, "cycle {cycle}: 不得留下实例");
    1
}

#[test]
#[ignore = "run via cargo xtask soak"]
fn real_open_staged_cycles_return_to_baseline() {
    assert_eq!(loaded_instances(), 0, "soak 开始前不得有实例");
    assert_eq!(staged_leaked(), 0, "soak 开始前暂存台账必须为零");

    let target = TempDir::new("open-staged-target");
    let artifact = build_fixture(target.path());
    let source_dir = TempDir::new("open-staged-source");
    let source = source_dir
        .path()
        .join(artifact.file_name().expect("夹具产物必须有文件名"));
    std::fs::copy(&artifact, &source).expect("复制夹具");

    let rss_baseline = rss_bytes();
    let mut rss_max = rss_baseline;
    let cycles = cycles();
    let mut opened = 0usize;
    for cycle in 0..cycles {
        opened += open_staged_once(&source, cycle);
        if let Some(rss) = rss_bytes() {
            rss_max = Some(rss_max.map_or(rss, |prior| prior.max(rss)));
        }
    }
    // 轮次计数抓两类退化：整个循环被删（opened 停在 0）与过早 break（轮次不足）。
    // 它抓不到"循环还在转、body 里的断言被删干净"——所以自增取自 body 本身：
    // `open_staged_once` 只有全部断言通过后才返回 1，计数与断言没法各自单独删掉。
    assert_eq!(
        opened, cycles,
        "真实装载路径必须跑满 {cycles} 轮，实际 {opened} 轮"
    );

    if let (Some(baseline), Some(max)) = (rss_baseline, rss_max) {
        let growth = max.saturating_sub(baseline);
        assert!(
            growth <= 128 * 1024 * 1024,
            "open_staged soak RSS 增长 {growth} bytes 超过 128 MiB"
        );
    }
    println!("open_staged soak complete: cycles={cycles}");
}
