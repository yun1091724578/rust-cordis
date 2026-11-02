//! Production seam: `LoadedPlugin::apply` must carry `PluginDesc.error_format`
//! into the instance so a capable plugin's structured `STATUS_ERR` is decoded,
//! not shown to the user as a raw JSON envelope.

use cordis_core::{App, Config, FiberState, Plugin};
use cordis_host::{loaded_instances, LoadedPlugin};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus};
use std::sync::Arc;
use std::time::{Duration, Instant};

struct TempTarget {
    path: PathBuf,
}

impl TempTarget {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("cordis-abi-error-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        Self { path }
    }
}

impl Drop for TempTarget {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/host 的上两级是仓库根")
        .to_path_buf()
}

fn wait_with_timeout(child: &mut Child, seconds: u64) -> ExitStatus {
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait().expect("等待夹具构建") {
            return status;
        }
        if started.elapsed() >= Duration::from_secs(seconds) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("夹具构建超时");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn find_artifact(root: &Path, name: &str) -> Option<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    let mut found = Vec::new();
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path.file_name() != Some(OsStr::new("incremental")) {
                    stack.push(path);
                }
            } else if path.file_name() == Some(OsStr::new(name)) {
                found.push(path);
            }
        }
    }
    found.into_iter().max_by_key(|path| {
        std::fs::metadata(path)
            .and_then(|meta| meta.modified())
            .ok()
    })
}

fn build_fixture() -> (TempTarget, PathBuf) {
    let manifest = repo_root().join("tests/fixtures/abi-error/Cargo.toml");
    assert!(
        manifest.is_file(),
        "结构化错误夹具必须存在：{}",
        manifest.display()
    );

    let target = TempTarget::new();
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let mut child = Command::new(cargo)
        .args(["build", "--manifest-path"])
        .arg(&manifest)
        .arg("--target-dir")
        .arg(&target.path)
        .args(["--offline", "--locked"])
        .spawn()
        .expect("启动夹具构建");
    assert!(wait_with_timeout(&mut child, 120).success(), "夹具构建失败");

    let name = if cfg!(windows) {
        "abi_error.dll"
    } else if cfg!(target_os = "macos") {
        "libabi_error.dylib"
    } else {
        "libabi_error.so"
    };
    let artifact =
        find_artifact(&target.path, name).unwrap_or_else(|| panic!("夹具产物不存在：{name}"));
    (target, artifact)
}

#[tokio::test]
async fn loaded_plugin_apply_carries_error_format() {
    let (target, artifact) = build_fixture();
    let plugin = LoadedPlugin::open(&artifact).expect("新宿主必须能打开结构化错误夹具");
    assert_eq!(plugin.schema(), Some(r#"{"type":"object"}"#));
    assert_eq!(plugin.schema_error(), None);

    let app = App::new();
    let fiber = app
        .root
        .plugin(Arc::new(plugin) as Arc<dyn Plugin>, Config::Null);
    tokio::time::timeout(Duration::from_secs(10), fiber.settle())
        .await
        .expect("apply 结算超时");

    assert_eq!(fiber.state(), FiberState::Failed);
    let error = fiber.error().expect("失败的 apply 必须留下错误");
    assert!(
        error.contains("apply boom"),
        "结构化错误的人类可读消息必须保留：{error:?}"
    );
    assert!(
        !error.contains("\"code\""),
        "生产 seam 必须解码结构，而不是把 JSON 信封当文本：{error:?}"
    );

    drop(fiber);
    drop(app);
    assert_eq!(loaded_instances(), 0, "失败的 apply 不得泄漏实例");
    drop(target);
}
