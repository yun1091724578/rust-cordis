//! O9-C：**暂存副本替换**——`reload_one` 必须把原始 artifact 的准入结论也用在
//! 暂存副本上（crate 内用例）。
//!
//! # 为什么在 crate 内
//! 它要靠 `#[cfg(test)]` 的 [`AdoptHooks::pass_before_staged_open`] 闸门：在
//! `stage_plugin` 之后、`open_staged_verified` 之前把 `.rN` 换成别的字节。这个钩子
//! 每实例一份、release 构建零成本，所以测试能精确钉住"校验的到底是哪个字节"，
//! 而不是靠 sleep 去赌竞态窗口。

use super::*;
use crate::source::{AllowedPlugin, PluginSourcePolicy};
use cordis_core::App;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "cordis-o9-staged-{}-{}-{tag}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
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

/// 只提供真实文件路径的假源：`reload_one` 的暂存/校验要从 `record.source` 读文件，
/// 但首次装载不需要真的能打开库。
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
    ) -> std::result::Result<LoadedFile, String> {
        Ok(LoadedFile {
            plugin: self.plugin.clone(),
            source: self.source.clone(),
            staged: self.source.clone(),
            fingerprint: fingerprint(&self.source),
        })
    }
}

fn write_manifest(artifact: &Path, sha256: &str) {
    let manifest = json!({
        "id": "example.plugin",
        "version": "0.1.0",
        "source_revision": "abc123",
        "abi_version": cordis_abi::ABI_VERSION,
        "sha256": sha256,
    });
    std::fs::write(crate::source::manifest_path(artifact), manifest.to_string())
        .expect("写 sidecar manifest");
}

fn allow_list(sha256: &str) -> PluginSourcePolicy {
    PluginSourcePolicy::new(vec![AllowedPlugin {
        id: "example.plugin".to_string(),
        version: "0.1.0".to_string(),
        source_revision: "abc123".to_string(),
        abi_version: cordis_abi::ABI_VERSION,
        sha256: sha256.to_string(),
    }])
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
        interpolation: InterpolationMode::Builtin,
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

/// **O9-C**：暂存与校验之间副本被换掉 ⇒ 必须报"暂存副本 SHA256 不匹配"，
/// 而不是把它当成坏库去 `LoadLibrary`。
///
/// 这条钉住两件事：① `reload_one` 把**原始 artifact 的策略**确实传给了
/// `open_staged_verified`（否则走的是不校验的 `open_staged`，错误会变成加载失败）；
/// ② 失败分支零副作用（无 `.rN` 残留、暂存台账回基线、在线实例不动）。
#[tokio::test]
async fn reload_one_rejects_a_staged_copy_with_different_bytes() {
    let app = App::new();
    let dir = TempDir::new("staged-swap");
    let artifact = dir.write("plug.dll", b"not a dynamic library, but the original bytes");
    let sha = crate::source::sha256_file(&artifact).expect("算原始 hash");
    write_manifest(&artifact, &sha);
    let config_path = dir.write("cordis.yml", b"- id: provider\n  name: plug.dll\n");

    let plugin: Arc<dyn Plugin> =
        cordis_core::plugin_fn("plug", Inject::default(), |_ctx: Ctx, _cfg: Config| {
            Box::pin(async { Ok(()) })
        });
    let inner: Arc<dyn PluginSource> = Arc::new(FileSource {
        source: artifact.clone(),
        plugin,
    });
    let source: Arc<dyn PluginSource> =
        Arc::new(PolicyPluginSource::strict(inner, allow_list(&sha)));
    let state = load_with_source(app.root.clone(), config_for(&config_path), source)
        .await
        .expect("首次装载");
    assert_eq!(state.mounted_labels(), ["provider"]);

    // 闸门：暂存之后、打开之前，用**别的字节**覆盖那个 `.rN`。
    let touched = Arc::new(Mutex::new(None::<PathBuf>));
    let seen = touched.clone();
    state.install_before_staged_open_hook(Arc::new(move |staged: &Path| {
        std::fs::write(
            staged,
            b"bytes that are definitely not the verified artifact",
        )
        .expect("覆盖暂存副本");
        *seen.lock().unwrap() = Some(staged.to_path_buf());
    }));
    let leaked = crate::staged_leaked();

    let error = reload_one(&app.root, &state, "provider")
        .await
        .expect_err("暂存副本被换掉必须失败");
    let staged_path = touched
        .lock()
        .unwrap()
        .clone()
        .expect("闸门必须真的被调用过（否则这条用例什么都没钉住）");
    assert!(!staged_path.exists(), "失败分支必须删掉暂存副本");
    assert!(
        error.contains("暂存副本 SHA256 与已校验 artifact 不匹配"),
        "必须报二次校验失败（而不是走到加载器）：{error}"
    );
    assert!(
        !error.contains("LoadLibrary") && !error.contains("dlopen"),
        "策略拒绝必须发生在打开动态库之前：{error}"
    );
    assert_eq!(
        crate::staged_leaked(),
        leaked,
        "失败的暂存副本必须销账（基线 {leaked}）"
    );
    assert!(
        staged_files(dir.path()).is_empty(),
        "不得留下 .rN 残留：{:?}",
        staged_files(dir.path())
    );
    assert_eq!(state.mounted_labels(), ["provider"], "保留当前实例");
    state.clear_hooks();
}
