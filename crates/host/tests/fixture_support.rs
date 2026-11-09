//! Shared integration-test fixture builder for the frozen ABI v1 cdylib.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new(label: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("cordis-fixture-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("创建临时目录");
        Self(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/host 的上两级是仓库根")
        .to_path_buf()
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

pub fn build_fixture(target: &Path) -> PathBuf {
    let manifest = repo_root().join("tests/fixtures/abi-v1/Cargo.toml");
    assert!(
        manifest.is_file(),
        "冻结夹具源码不存在：{}",
        manifest.display()
    );
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let started = Instant::now();
    let mut child = Command::new(cargo)
        .args(["build", "--manifest-path"])
        .arg(&manifest)
        .arg("--target-dir")
        .arg(target)
        .args(["--offline", "--locked"])
        .spawn()
        .expect("启动冻结夹具构建");
    let status = loop {
        if let Some(status) = child.try_wait().expect("等待冻结夹具构建") {
            break status;
        }
        if started.elapsed() >= Duration::from_secs(180) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("冻结夹具构建超时");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(status.success(), "冻结夹具构建失败");

    let name = if cfg!(windows) {
        "abi_v1_frozen.dll"
    } else if cfg!(target_os = "macos") {
        "libabi_v1_frozen.dylib"
    } else {
        "libabi_v1_frozen.so"
    };
    find_artifact(target, name).unwrap_or_else(|| panic!("冻结夹具产物不存在：{name}"))
}
