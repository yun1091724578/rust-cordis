//! O9：`probe_capabilities` 必须用**同一份准入快照**完成校验与打开。
//!
//! 探测是"解析 → 暂存 → 打开 → 探活 → 读能力面 → 立刻放手"。旧实现读两次准入：
//! `source.admission().resolve()` 校验一次，接着 `source.load(...)` 里再过一次来源
//! 自己的读口——**两步之间全局被改**，打开就会走未校验的分支，暂存副本也不再与
//! 已验证的 hash 绑定。修法是让探测把结论**显式**交给 `load_admitted`。
//!
//! 本文件**独占一个测试二进制**：`set_plugin_source`（进程级来源）与
//! `set_active_policy` / `allow_unverified`（进程级准入）都是全局量；装进去的来源
//! 拿不回来，所以两段断言必须在同一个测试里按顺序跑（先裸源、后安装的来源）。

use cordis_core::{plugin_fn, Config, Ctx, Inject, Plugin};
use cordis_host::loader::{LoadedFile, PluginSource, PolicyPluginSource};
use cordis_host::patch::Entry;
use cordis_host::source::{
    set_active_policy, set_allow_unverified, AllowedPlugin, PluginSourcePolicy, SourceAdmission,
};
use cordis_host::staged_leaked;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const SHA_EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "cordis-probe-admission-{}-{tag}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("建临时目录");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn write_artifact(dir: &TempDir, bytes: &[u8]) -> PathBuf {
    let path = dir.path().join("plugin.dll");
    std::fs::write(&path, bytes).expect("写 artifact");
    path
}

fn write_manifest(artifact: &Path, sha256: &str) {
    let manifest = serde_json::json!({
        "id": "example.plugin",
        "version": "0.1.0",
        "source_revision": "abc123",
        "abi_version": cordis_abi::ABI_VERSION,
        "sha256": sha256,
    });
    std::fs::write(
        cordis_host::source::manifest_path(artifact),
        manifest.to_string(),
    )
    .expect("写 sidecar manifest");
}

fn allowed(source_revision: &str, sha256: &str) -> AllowedPlugin {
    AllowedPlugin {
        id: "example.plugin".to_string(),
        version: "0.1.0".to_string(),
        source_revision: source_revision.to_string(),
        abi_version: cordis_abi::ABI_VERSION,
        sha256: sha256.to_string(),
    }
}

/// 进用例先复位进程级准入，出用例再复位一次（含 panic 路径）。
///
/// 来源本身（`set_plugin_source`）没有清除入口，所以本文件只有一个用例。
struct GlobalsGuard;

impl GlobalsGuard {
    fn new() -> Self {
        reset();
        Self
    }
}

impl Drop for GlobalsGuard {
    fn drop(&mut self) {
        reset();
    }
}

fn reset() {
    set_active_policy(None);
    set_allow_unverified(false);
}

/// 暂存副本以 `.r<digits>` 结尾（`stage_plugin` 的命名约定）。
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

fn fake_plugin() -> Arc<dyn Plugin> {
    plugin_fn("probe-o9", Inject::default(), |_ctx: Ctx, _cfg: Config| {
        Box::pin(async { Ok(()) })
    })
}

/// **O9 之前的 `DiskPluginSource` 形状**：`load` 自己再读一次进程级准入。
///
/// 这是缺陷的形状本身——同一个来源对象声明了严格准入（`admission()`），但
/// `load` 走的是 fallback（全局）。探测若在两步之间各读一次，就会"验证按局部、
/// 打开按全局"。`load_admitted` 是唯一的正确读口（见 `PluginSource` 的文档）。
struct NaiveSource {
    artifact: PathBuf,
    policy: Arc<PluginSourcePolicy>,
}

impl PluginSource for NaiveSource {
    fn admission(&self) -> SourceAdmission {
        SourceAdmission::strict(self.policy.clone())
    }

    fn load(&self, entry: &Entry, search: &[PathBuf], revision: u64) -> Result<LoadedFile, String> {
        // 朴素实现：不看自己的 `admission()`，直接回落进程级全局。
        let admission = SourceAdmission::fallback();
        self.load_admitted(entry, search, revision, &admission)
    }

    fn load_admitted(
        &self,
        _entry: &Entry,
        _search: &[PathBuf],
        _revision: u64,
        admission: &SourceAdmission,
    ) -> Result<LoadedFile, String> {
        if let Some(policy) = admission.resolve()? {
            policy.verify(&self.artifact)?;
        }
        Ok(LoadedFile {
            plugin: fake_plugin(),
            source: self.artifact.clone(),
            staged: self.artifact.clone(),
            fingerprint: None,
        })
    }
}

/// O9（**行为红**）：探测的校验与打开必须共用**同一份**准入结论。
///
/// 旧实现：`source.admission().resolve()`（严格，合法 artifact 通过）→
/// `source.load(...)`（朴素读口 → 回落全局 → 默认拒绝）⇒ 一个合法 artifact 探测失败，
/// 报的却是"未配置插件来源策略"。同一份来源声明在一句话里给出两个结论，正是要修的东西。
///
/// 第二段是反向判据：裸 `DiskPluginSource` + 全拒绝 allow-list 探测**被篡改**的
/// artifact，必须拿到**准入**错误（而不是"打不开动态库"），且不留暂存副本。
#[test]
fn probe_uses_one_admission_snapshot_for_verify_and_open() {
    let _globals = GlobalsGuard::new();

    // ① 裸源（进程级准入）下的反向判据：篡改的 artifact 必须在打开之前被准入拒绝。
    let tampered_dir = TempDir::new("bare-tampered");
    let tampered = write_artifact(&tampered_dir, b"abc");
    // manifest 自述的是"空文件"的 hash，实际字节是 "abc" ⇒ hash 不匹配。
    write_manifest(&tampered, SHA_EMPTY);
    set_active_policy(Some(Arc::new(PluginSourcePolicy::new(vec![allowed(
        "abc123", SHA_EMPTY,
    )]))));
    let leaked = staged_leaked();
    let error = cordis_host::probe_capabilities_in(&tampered, &[])
        .expect_err("被篡改的 artifact 不得探测成功")
        .to_string();
    assert!(
        error.contains("SHA256") || error.contains("allow-list"),
        "必须报准入错误：{error}"
    );
    assert!(
        !error.contains("LoadLibrary") && !error.contains("dlopen"),
        "拒绝必须发生在打开动态库之前：{error}"
    );
    assert_eq!(
        staged_leaked(),
        leaked,
        "被拒绝的 artifact 不得产生暂存副本"
    );
    assert!(
        staged_files(tampered_dir.path()).is_empty(),
        "不得留下 .rN 残留：{:?}",
        staged_files(tampered_dir.path())
    );
    reset();

    // ② 局部严格来源探测一个**合法** artifact：必须成功。
    let dir = TempDir::new("strict-legal");
    let artifact = write_artifact(&dir, b"abc");
    let sha = cordis_host::source::sha256_file(&artifact).expect("算 hash");
    write_manifest(&artifact, &sha);
    let policy = Arc::new(PluginSourcePolicy::new(vec![allowed("abc123", &sha)]));

    // 朴素来源自己声明严格准入，但 `load` 会把结论丢给全局（见 `NaiveSource`）。
    cordis_host::set_plugin_source(Arc::new(NaiveSource {
        artifact: artifact.clone(),
        policy: policy.clone(),
    }));
    let caps = cordis_host::probe_capabilities(&artifact)
        .expect("显式严格来源的合法 artifact 必须探测成功（旧实现回落到全局默认拒绝）");
    assert_eq!(caps.plugin, "probe-o9");

    // ③ 同一件事在 `PolicyPluginSource` 上的正例（改前改后都绿）：包装层把**自己的**
    //    结论经 `load_admitted` 交下去，于是"验证"与"打开"天然是同一份快照。
    cordis_host::set_plugin_source(Arc::new(PolicyPluginSource::strict(
        Arc::new(NaiveSource {
            artifact: artifact.clone(),
            policy,
        }),
        PluginSourcePolicy::new(vec![allowed("abc123", &sha)]),
    )));
    let caps =
        cordis_host::probe_capabilities(&artifact).expect("包装来源的合法 artifact 必须探测成功");
    assert_eq!(caps.plugin, "probe-o9");
}
