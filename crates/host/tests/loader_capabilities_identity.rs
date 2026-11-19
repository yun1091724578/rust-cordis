//! WP2：`loader.report().capabilities[]` 的变体身份（uid + sha256）。
//!
//! 用假 `PluginSource` 控制暂存副本的内容与 mtime：
//! ① 同内容不同文件（不同 mtime）→ sha256 相同；
//! ② 改一字节 → sha256 不同；
//! ③ 同一 label 重挂 → uid 变化，sha256 随内容变化。

use cordis_core::{unique_key, App, BoxFuture, Config, Ctx, FnPlugin, Inject, Result};
use cordis_host::loader::{load_with_source, LoadedFile, LoaderConfig, PluginSource};
use cordis_host::patch::{parse_entries, Entry};
use cordis_host::source::SourceAdmission;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

fn set_modified(path: &Path, seconds: u64) {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("打开临时文件");
    file.set_times(
        std::fs::FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(seconds)),
    )
    .expect("设置 mtime");
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "cordis-cap-identity-{}-{label}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("创建临时目录");
        Self(path)
    }

    fn file(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, bytes).expect("写临时文件");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct IdentitySource {
    /// 原始来源路径：内容故意与暂存副本不同，用来证明报告取的是 `staged`。
    source: PathBuf,
    staged: Mutex<PathBuf>,
}

impl IdentitySource {
    fn set(&self, path: PathBuf) {
        *self.staged.lock().unwrap() = path;
    }
}

impl PluginSource for IdentitySource {
    fn load(
        &self,
        entry: &Entry,
        _search: &[PathBuf],
        _revision: u64,
    ) -> std::result::Result<LoadedFile, String> {
        if entry.name.as_str() == "builtin" {
            let plugin: Arc<dyn cordis_core::Plugin> = Arc::new(FnPlugin {
                name: "builtin",
                key: unique_key("builtin"),
                inject: Inject::default(),
                validator: None,
                f: Arc::new(|_ctx: Ctx, _config: Config| -> BoxFuture<Result<()>> {
                    Box::pin(async { Ok(()) })
                }),
            });
            return Ok(LoadedFile {
                plugin,
                source: PathBuf::from("<builtin>"),
                staged: PathBuf::from("<builtin>"),
                fingerprint: None,
            });
        }
        if entry.name.as_str() != "alpha" {
            return Err(format!("假源不认识 {}", entry.name));
        }
        let staged = self.staged.lock().unwrap().clone();
        let meta = std::fs::metadata(&staged).map_err(|err| err.to_string())?;
        let mtime = meta
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos() as u64)
            .unwrap_or(0);
        let plugin: Arc<dyn cordis_core::Plugin> = Arc::new(FnPlugin {
            name: "alpha",
            key: unique_key("alpha"),
            inject: Inject::default(),
            validator: None,
            f: Arc::new(|_ctx: Ctx, _config: Config| -> BoxFuture<Result<()>> {
                Box::pin(async { Ok(()) })
            }),
        });
        Ok(LoadedFile {
            plugin,
            source: self.source.clone(),
            staged,
            fingerprint: Some((mtime, meta.len())),
        })
    }

    fn admission(&self) -> SourceAdmission {
        SourceAdmission::dev_unverified()
    }
}

struct BrokenHashSource {
    source: PathBuf,
    staged: PathBuf,
}

impl PluginSource for BrokenHashSource {
    fn load(
        &self,
        entry: &Entry,
        _search: &[PathBuf],
        _revision: u64,
    ) -> std::result::Result<LoadedFile, String> {
        if entry.name.as_str() != "alpha" {
            return Err(format!("假源不认识 {}", entry.name));
        }
        let plugin: Arc<dyn cordis_core::Plugin> = Arc::new(FnPlugin {
            name: "alpha",
            key: unique_key("alpha"),
            inject: Inject::default(),
            validator: None,
            f: Arc::new(|_ctx: Ctx, _config: Config| -> BoxFuture<Result<()>> {
                Box::pin(async { Ok(()) })
            }),
        });
        Ok(LoadedFile {
            plugin,
            source: self.source.clone(),
            staged: self.staged.clone(),
            // 文件来源（有指纹），但暂存副本读不到 ⇒ 报告必须是 unavailable。
            fingerprint: Some((1, 1)),
        })
    }

    fn admission(&self) -> SourceAdmission {
        SourceAdmission::dev_unverified()
    }
}

fn row_by_label<'a>(report: &'a Value, label: &str) -> &'a Value {
    let rows = report["capabilities"]
        .as_array()
        .expect("capabilities 是数组");
    rows.iter()
        .find(|row| row["label"].as_str() == Some(label))
        .unwrap_or_else(|| panic!("缺少 {label} 行：{rows:?}"))
}

fn uid(row: &Value) -> u64 {
    row["uid"].as_u64().expect("uid 是 u64")
}

fn sha(row: &Value) -> &str {
    row["sha256"].as_str().expect("sha256 是字符串")
}

async fn mount(
    dir: &TempDir,
    source: Arc<IdentitySource>,
) -> (App, Arc<cordis_host::loader::LoaderState>) {
    let config = dir.file(
        "cordis.yml",
        b"- id: alpha\n  name: alpha\n- id: builtin\n  name: builtin\n- id: beta\n  name: beta\n  disabled: true\n",
    );
    let app = App::new();
    let state = load_with_source(
        app.root.clone(),
        LoaderConfig::new(config.to_string_lossy().into_owned()),
        source,
    )
    .await
    .expect("装载成功");
    (app, state)
}

#[tokio::test]
async fn report_capabilities_carry_uid_and_content_sha256() {
    let dir = TempDir::new("identity");
    let a1 = dir.file("alpha-a1.bin", b"same-alpha-body");
    let a2 = dir.file("alpha-a2.bin", b"same-alpha-body");
    let b = dir.file("alpha-b.bin", b"same-alpha-bodz");
    set_modified(&a1, 1_000);
    set_modified(&a2, 2_000);
    assert_ne!(
        std::fs::metadata(&a1).unwrap().modified().unwrap(),
        std::fs::metadata(&a2).unwrap().modified().unwrap(),
        "两个副本的 mtime 必须不同"
    );

    let source_file = dir.file("alpha-source.bin", b"source-not-staged");
    let source = Arc::new(IdentitySource {
        source: source_file.clone(),
        staged: Mutex::new(a1.clone()),
    });
    let (_app, state) = mount(&dir, source.clone()).await;

    let report = state.report();
    assert_eq!(
        report["capabilities"].as_array().unwrap().len(),
        2,
        "只应报告两条启用行（disabled 不计）"
    );
    let builtin = row_by_label(&report, "builtin");
    assert_eq!(sha(builtin), "builtin", "非文件来源填 builtin");
    assert_ne!(uid(builtin), u64::MAX);
    let first = row_by_label(&report, "alpha");
    let uid1 = uid(first);
    assert_ne!(uid1, u64::MAX, "活实例的 uid 不能是 UID_DISPOSED");
    // 独立字面量：不拿被测实现自己的 sha256_file 做期望值（否则实现错也全绿）。
    const HASH_A1: &str = "65ff5e296b3bed831eeee2589652aa17f9268567601f69520a885268650dc0eb";
    const HASH_SOURCE: &str = "7cebda985d7e2e0589f8228e39e260295c8dbde73f340135733784ea4f4b5fa3";
    assert_eq!(sha(first), HASH_A1);
    assert_eq!(sha(first).len(), 64);
    assert_ne!(
        sha(first),
        HASH_SOURCE,
        "必须哈希暂存副本，不能哈希原始来源路径"
    );

    // ① 同内容、不同 mtime 的副本：sha 相同，但重挂后 uid 变化。
    source.set(a2.clone());
    let replaced = state
        .reconcile_once(
            parse_entries(
                "- id: alpha\n  name: alpha\n  isolate:\n    svc: true\n- id: builtin\n  name: builtin\n- id: beta\n  name: beta\n  disabled: true\n",
            )
            .expect("解析新树"),
        )
        .await;
    assert_eq!(
        replaced.replaced,
        vec!["alpha".to_string()],
        "必须走 Replace"
    );
    let report = state.report();
    let second = row_by_label(&report, "alpha");
    let uid2 = uid(second);
    assert!(
        uid2 > uid1,
        "重挂后同一 label 的 uid 必须单调增大（新 fiber）"
    );
    assert_eq!(sha(second), HASH_A1, "同内容不同 mtime 的 sha256 必须相同");

    // ②/③ 改一字节：sha 不同，uid 再变。
    source.set(b.clone());
    let replaced = state
        .reconcile_once(
            parse_entries(
                "- id: alpha\n  name: alpha\n  isolate:\n    svc: r2\n- id: builtin\n  name: builtin\n- id: beta\n  name: beta\n  disabled: true\n",
            )
            .expect("解析新树"),
        )
        .await;
    assert_eq!(
        replaced.replaced,
        vec!["alpha".to_string()],
        "必须再次 Replace"
    );
    let report = state.report();
    let third = row_by_label(&report, "alpha");
    assert!(uid(third) > uid2, "再次重挂后 uid 必须单调增大");
    const HASH_B: &str = "ff3b634d44bb1fe1045b27c2245c17562588e2614d04c0f7a9f833da8be0aec1";
    assert_ne!(sha(third), HASH_A1, "改一字节后 sha256 必须不同");
    assert_eq!(sha(third), HASH_B);
}

#[tokio::test]
async fn hash_failure_is_reported_as_unavailable_not_builtin() {
    let dir = TempDir::new("unavailable");
    let source = dir.file("alpha-source.bin", b"source-bytes");
    let missing = dir.0.join("missing-staged.bin");
    let config = dir.file("cordis.yml", b"- id: alpha\n  name: alpha\n");
    let app = App::new();
    let state = load_with_source(
        app.root.clone(),
        LoaderConfig::new(config.to_string_lossy().into_owned()),
        Arc::new(BrokenHashSource {
            source,
            staged: missing,
        }),
    )
    .await
    .expect("挂载成功（hash 失败不应让挂载失败）");

    let report = state.report();
    let row = row_by_label(&report, "alpha");
    assert!(
        sha(row).starts_with("unavailable:"),
        "读不到暂存副本时必须报 unavailable，不能伪装 builtin：{row}"
    );
    assert_ne!(sha(row), "builtin");
}
