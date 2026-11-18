//! O9：局部来源策略落到**热重载**路径上（`reload_one`）。
//!
//! 和 `source_policy.rs` 分文件是因为这里有 `.await`：那边用 `std::sync::Mutex`
//! 串行化进程级策略，持着它跨 await 既过不了 clippy（`await_holding_lock`），
//! 也真的会卡住 current-thread 运行时。这里用 `tokio::sync::Mutex` 的同一套写法。
//!
//! # 头号判据
//! 显式严格的来源**永不被**全局开发标志放宽：即使另一个 App 调了
//! `set_allow_unverified(true)`，这个来源的新构建仍然要按 allow-list 拒绝，
//! 且拒绝必须发生在暂存/打开之前（错误里不出现 `LoadLibrary` / `dlopen`，
//! 也不留 `.rN` 残留）。

use cordis_core::{App, Config, Ctx, Inject, Plugin};
use cordis_host::loader::{
    load_with_source, LoadedFile, LoaderConfig, LoaderState, PluginSource, PolicyPluginSource,
};
use cordis_host::patch::Entry;
use cordis_host::plan::PlanEntrySpec;
use cordis_host::source::{
    set_active_policy, set_allow_unverified, AllowedPlugin, PluginSourcePolicy,
};
use cordis_host::staged_leaked;
use std::path::{Path, PathBuf};
use std::sync::Arc;

// ---------------------------------------------------------------- 夹具

/// `staged_leaked()` / 进程级准入都是全局量：同一二进制里的用例必须串行。
async fn ledger_serial() -> tokio::sync::MutexGuard<'static, ()> {
    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    LOCK.lock().await
}

/// 进用例先复位进程级准入，出用例再复位一次（含 panic 路径）。
struct GlobalsGuard;

impl GlobalsGuard {
    fn new() -> Self {
        set_active_policy(None);
        set_allow_unverified(false);
        Self
    }
}

impl Drop for GlobalsGuard {
    fn drop(&mut self) {
        set_active_policy(None);
        set_allow_unverified(false);
    }
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "cordis-o9-reload-{}-{}-{tag}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("建临时目录");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, bytes).expect("写文件");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn file_fingerprint(path: &Path) -> Option<(u64, u64)> {
    let metadata = std::fs::metadata(path).ok()?;
    let mtime = metadata
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos() as u64;
    Some((mtime, metadata.len()))
}

fn write_manifest(artifact: &Path, sha256: &str, revision: &str) {
    let manifest = serde_json::json!({
        "id": "example.plugin",
        "version": "0.1.0",
        "source_revision": revision,
        "abi_version": cordis_abi::ABI_VERSION,
        "sha256": sha256,
    });
    std::fs::write(
        cordis_host::source::manifest_path(artifact),
        manifest.to_string(),
    )
    .expect("写 sidecar manifest");
}

fn allowed(sha256: &str) -> AllowedPlugin {
    AllowedPlugin {
        id: "example.plugin".to_string(),
        version: "0.1.0".to_string(),
        source_revision: "abc123".to_string(),
        abi_version: cordis_abi::ABI_VERSION,
        sha256: sha256.to_string(),
    }
}

/// 只提供**真实文件路径**的假源（与 `loader_reconcile::FileSource` 同形状）：
/// 首次装载不需要真的能打开库（插件对象直接给），但 `reload_one` 会从
/// `record.source` 读文件做准入与暂存——那正是本文件要观察的那一步。
struct FileSource {
    source: PathBuf,
    plugin: Arc<dyn Plugin>,
}

impl PluginSource for FileSource {
    fn load(
        &self,
        _entry: &Entry,
        _search: &[PathBuf],
        _revision: u64,
    ) -> Result<LoadedFile, String> {
        Ok(LoadedFile {
            plugin: self.plugin.clone(),
            source: self.source.clone(),
            staged: self.source.clone(),
            fingerprint: file_fingerprint(&self.source),
        })
    }
}

fn file_source(path: &Path) -> Arc<dyn PluginSource> {
    let plugin: Arc<dyn Plugin> =
        cordis_core::plugin_fn("plug", Inject::default(), |_ctx: Ctx, _cfg: Config| {
            Box::pin(async { Ok(()) })
        });
    Arc::new(FileSource {
        source: path.to_path_buf(),
        plugin,
    })
}

fn config_for(path: &Path) -> LoaderConfig {
    LoaderConfig {
        path: path.to_string_lossy().into_owned(),
        patches: Vec::new(),
        plugin_dir: None,
        strict: true,
        strict_cycles: false,
        strict_sidecars: false,
        hmr: None,
        js_evaluator: None,
        interpolation: cordis_host::loader::InterpolationMode::Builtin,
        dispatch_policy: None,
        host_limits: None,
        dependency_plan: Vec::new(),
    }
}

/// 暂存副本必须以 `.r<digits>` 结尾（`stage_plugin` 的命名约定）。
fn staged_files(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .expect("读临时目录")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| {
            name.rsplit_once(".r").is_some_and(|(_, tail)| {
                !tail.is_empty() && tail.chars().next().unwrap().is_ascii_digit()
            })
        })
        .collect()
}

// ---------------------------------------------------------------- 用例

/// O9-B（**行为红**）：显式严格的来源在热重载时也必须按 allow-list 拒绝，
/// 即使另一个 App 已经把全局开发模式打开。
///
/// 旧实现在这里读的是 `crate::source::admission()`：全局 dev ⇒ `Ok(None)` ⇒
/// 跳过校验 ⇒ 直接暂存并 `LoadLibrary`，于是错误变成"打不开动态库"——
/// 一个**未登记的构建**被当成"坏构建"，而且真的走到了加载器里。
#[tokio::test]
async fn reload_one_honours_the_sources_own_strict_policy_even_when_global_dev_is_on() {
    let _serial = ledger_serial().await;
    let _globals = GlobalsGuard::new();
    let app = App::new();
    let dir = TempDir::new("strict-policy");
    let plugin_path = dir.write("plug.dll", b"old artifact bytes");
    let old_sha = cordis_host::source::sha256_file(&plugin_path).expect("算旧 hash");
    write_manifest(&plugin_path, &old_sha, "abc123");
    let config_path = dir.write("cordis.yml", b"- id: provider\n  name: plug.dll\n");

    // 旧 hash 在 allow-list 里 ⇒ 挂载成功（挂载只经过假源的 load）。
    let policy = PluginSourcePolicy::new(vec![allowed(&old_sha)]);
    let source: Arc<dyn PluginSource> = Arc::new(PolicyPluginSource::strict(
        file_source(&plugin_path),
        policy,
    ));
    let state: Arc<LoaderState> =
        load_with_source(app.root.clone(), config_for(&config_path), source)
            .await
            .expect("首次装载");
    assert_eq!(state.mounted_labels(), ["provider"]);
    let leaked = staged_leaked();

    // 原地换成新字节 + 新 manifest，新 hash **不在** allow-list 里。
    dir.write("plug.dll", b"brand new artifact bytes, deliberately longer");
    let new_sha = cordis_host::source::sha256_file(&plugin_path).expect("算新 hash");
    write_manifest(&plugin_path, &new_sha, "def456");
    assert_eq!(
        state.stale_labels(),
        ["provider"],
        "文件变了 ⇒ 这一行必须是陈旧的"
    );

    // 另一个 App 的便利开关：不得放宽这个来源。
    set_allow_unverified(true);

    let error = cordis_host::loader::reload_one(&app.root, &state, "provider")
        .await
        .expect_err("显式严格的来源必须拒绝未列入 allow-list 的新构建");
    assert!(
        error.contains("allow-list"),
        "必须报准入失败（旧实现在全局 dev 下会走到加载失败）：{error}"
    );
    assert!(
        !error.contains("LoadLibrary") && !error.contains("dlopen"),
        "拒绝必须发生在暂存/打开之前：{error}"
    );
    assert!(
        state.stale_labels().is_empty(),
        "被拒绝的构建也要推进指纹（否则 watcher 每轮重试同一份拒绝）"
    );
    assert_eq!(staged_leaked(), leaked, "不得新增暂存泄漏（基线 {leaked}）");
    assert!(
        staged_files(dir.path()).is_empty(),
        "策略拒绝时不得留下 .rN 残留：{:?}",
        staged_files(dir.path())
    );
    assert_eq!(
        state.mounted_labels(),
        ["provider"],
        "预检失败不得动正在跑的实例"
    );
}

/// O9：**默认拒绝**（没有策略、也没开开发模式）在 `reload_one` 里同样要
/// "记录失败 + 推进指纹 + publish_report"，而不是 `?` 早退。
///
/// 早退的后果是只坏一半：错误返回了，但指纹没推进 ⇒ watcher 每 500ms 重打一次
/// 同一条日志，`loader.report` 也不刷新（读报告的人看到的是上一轮）。
#[tokio::test]
async fn denied_admission_advances_the_fingerprint_instead_of_retrying_forever() {
    let _serial = ledger_serial().await;
    let _globals = GlobalsGuard::new();
    let app = App::new();
    let dir = TempDir::new("denied-admission");
    let plugin_path = dir.write("plug.dll", b"old artifact bytes");
    let config_path = dir.write("cordis.yml", b"- id: provider\n  name: plug.dll\n");

    // 假源的 `load` 不看准入，所以首次挂载在"默认拒绝"下也能成立。
    let state: Arc<LoaderState> = load_with_source(
        app.root.clone(),
        config_for(&config_path),
        file_source(&plugin_path),
    )
    .await
    .expect("首次装载");
    assert_eq!(state.mounted_labels(), ["provider"]);

    dir.write("plug.dll", b"changed bytes, also longer than before");
    assert_eq!(state.stale_labels(), ["provider"]);
    let leaked = staged_leaked();

    let error = cordis_host::loader::reload_one(&app.root, &state, "provider")
        .await
        .expect_err("默认拒绝下热重载必须失败");
    assert!(
        error.contains("未配置插件来源策略"),
        "失败原因必须可诊断：{error}"
    );
    assert!(
        state.stale_labels().is_empty(),
        "准入拒绝也要推进指纹（否则 watcher 每轮重打日志）"
    );
    assert_eq!(staged_leaked(), leaked, "拒绝发生在暂存之前");
    assert!(
        staged_files(dir.path()).is_empty(),
        "不得留下 .rN 残留：{:?}",
        staged_files(dir.path())
    );
    assert_eq!(state.mounted_labels(), ["provider"]);
}

/// 读 sidecar 的假源：`load` 直接给插件，`plan_entry` **真读** artifact 旁边的 manifest。
///
/// 于是"sidecar 损坏"是文件系统上的事实，而不是一句被注入口令的错误串：
/// `strict_sidecars=true` 下它会真的把预检打挂。
struct SidecarSource {
    source: PathBuf,
    plugin: Arc<dyn Plugin>,
}

impl PluginSource for SidecarSource {
    fn load(
        &self,
        _entry: &Entry,
        _search: &[PathBuf],
        _revision: u64,
    ) -> Result<LoadedFile, String> {
        Ok(LoadedFile {
            plugin: self.plugin.clone(),
            source: self.source.clone(),
            staged: self.source.clone(),
            fingerprint: file_fingerprint(&self.source),
        })
    }

    fn plan_entry(
        &self,
        _entry: &Entry,
        _search: &[PathBuf],
    ) -> Result<Option<PlanEntrySpec>, String> {
        if !cordis_host::source::manifest_path(&self.source).is_file() {
            return Ok(None);
        }
        let manifest = cordis_host::source::read_manifest(&self.source)?;
        Ok(Some(PlanEntrySpec {
            id: manifest.id,
            version: manifest.version,
            provides: manifest.provides,
            requires: manifest.requires,
        }))
    }
}

fn sidecar_source(path: &Path) -> Arc<dyn PluginSource> {
    let plugin: Arc<dyn Plugin> =
        cordis_core::plugin_fn("plug", Inject::default(), |_ctx: Ctx, _cfg: Config| {
            Box::pin(async { Ok(()) })
        });
    Arc::new(SidecarSource {
        source: path.to_path_buf(),
        plugin,
    })
}

/// O9（**行为红**）：`reload_one` 的**预检**失败分支必须与准入/校验拒绝分支同构。
///
/// 旧实现只 `Err` 早退：
/// - **指纹没推进** ⇒ 这一行永远是 stale ⇒ watcher 每 500ms 重跑整树预检与逐行
///   SHA-256，并且每轮再打一条同样的 Error；
/// - 它还用 `ReconcileOutcome::default()` 覆盖 `last`，把**上一轮真正执行过的**
///   对账计数（`report.reconcile.applied`）一起清零。
///
/// 这里的判据是"下一轮不再重试" + "计数不被抹掉" + "失败仍可在报告里看见"。
#[tokio::test]
async fn reload_one_preflight_failure_advances_the_fingerprint_and_keeps_the_counts() {
    let _serial = ledger_serial().await;
    let _globals = GlobalsGuard::new();
    let app = App::new();
    let dir = TempDir::new("preflight-failure-fingerprint");
    let plugin_path = dir.write("plug.dll", b"old artifact bytes");
    let config_path = dir.write("cordis.yml", b"- id: provider\n  name: plug.dll\n");
    write_manifest(
        &plugin_path,
        &cordis_host::source::sha256_file(&plugin_path).expect("算 hash"),
        "abc123",
    );

    let state: Arc<LoaderState> = load_with_source(
        app.root.clone(),
        LoaderConfig {
            strict_sidecars: true,
            ..config_for(&config_path)
        },
        sidecar_source(&plugin_path),
    )
    .await
    .expect("首次装载");
    assert_eq!(state.mounted_labels(), ["provider"]);
    let applied = state.report()["reconcile"]["applied"].clone();
    assert_eq!(applied, serde_json::json!(1), "初次装载挂了一行");
    assert_eq!(
        state.report()["preflight"]["verdict"],
        serde_json::json!("checked"),
        "初装预检是合法的"
    );

    // 换字节（这一行变陈旧）+ 把 sidecar 打坏（下一轮预检必失败）。
    dir.write("plug.dll", b"brand new artifact bytes, deliberately longer");
    std::fs::write(
        cordis_host::source::manifest_path(&plugin_path),
        "{ not json",
    )
    .expect("写坏 manifest");
    assert_eq!(state.stale_labels(), ["provider"], "文件变了 ⇒ 这一行陈旧");

    let error = cordis_host::loader::reload_one(&app.root, &state, "provider")
        .await
        .expect_err("sidecar 损坏 + strict_sidecars ⇒ 预检必须让 reload 失败");
    assert!(
        error.contains("依赖预检失败（reload 未执行）"),
        "失败文案保持既有前缀：{error}"
    );
    assert!(
        error.contains("manifest"),
        "失败原因必须点名损坏的 sidecar：{error}"
    );

    assert!(
        state.stale_labels().is_empty(),
        "预检失败也要推进指纹（否则 watcher 每轮重跑整树预检与逐行 SHA-256）"
    );
    assert_eq!(
        state.report()["reconcile"]["applied"],
        applied,
        "上一轮真正执行过的对账计数不得被一次未执行的重载清零：{}",
        state.report()
    );
    assert!(
        state.report()["failed"]
            .as_array()
            .is_some_and(|lines| lines.iter().any(|line| line
                .as_str()
                .is_some_and(|text| text.contains("依赖预检失败")))),
        "热重载被预检挡住这件事必须进 report.failed：{}",
        state.report()
    );
    assert_eq!(
        state.report()["preflight"]["verdict"],
        serde_json::json!("checked"),
        "预检失败的一轮什么都没检查，不得覆盖上一份结论：{}",
        state.report()
    );
    assert_eq!(
        state.mounted_labels(),
        ["provider"],
        "预检失败不得动正在跑的实例"
    );
}
