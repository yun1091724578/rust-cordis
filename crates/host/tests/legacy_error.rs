//! 旧 ABI 插件错误的端到端夹具。
//!
//! 独立测试二进制：`loaded_instances()` 是进程级计数。夹具 `tests/fixtures/abi-err-v1`
//! 是 72B v1 布局，只返回纯文本 `STATUS_ERR`；它不修改冻结的 `abi-v1`。

use cordis_abi::{Blob, PluginDesc, OP_APPLY, OP_CALLBACK};
use cordis_core::{App, Config, FiberState, Plugin};
use cordis_host::{call_plugin_detailed, loaded_instances, LoadedPlugin};
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
        let path = std::env::temp_dir().join(format!("cordis-abi-err-v1-{}", std::process::id()));
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
    let manifest = repo_root().join("tests/fixtures/abi-err-v1/Cargo.toml");
    assert!(
        manifest.is_file(),
        "旧 ABI 错误夹具必须存在：{}",
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
        "abi_err_v1.dll"
    } else if cfg!(target_os = "macos") {
        "libabi_err_v1.dylib"
    } else {
        "libabi_err_v1.so"
    };
    let artifact =
        find_artifact(&target.path, name).unwrap_or_else(|| panic!("夹具产物不存在：{name}"));
    (target, artifact)
}

#[tokio::test]
async fn legacy_plugin_error_is_text_and_unknown_not_not_executed() {
    let (target, artifact) = build_fixture();
    let plugin = LoadedPlugin::open(&artifact).expect("新宿主必须能打开旧 ABI 错误夹具");
    let raw = plugin.raw_desc();
    assert_eq!(
        unsafe { (*raw).size as usize },
        cordis_abi::PLUGIN_DESC_PREFIX_SIZE,
        "夹具必须停在 72B v1 前缀；88B/96B 且 error_format=0 的同形描述符不算旧插件"
    );
    assert_eq!(
        unsafe { PluginDesc::error_format_at(raw) },
        0,
        "旧插件不得被误读为有结构化能力"
    );
    assert!(
        unsafe { PluginDesc::field_at::<Blob>(raw, std::mem::offset_of!(PluginDesc, provision)) }
            .is_none(),
        "v1 描述符不能包含 provision 字段"
    );

    let call = unsafe { (*raw).call };
    // 这里只验证“按插件能力位 0 读文本”的降级分支，不是真实旧宿主进程。
    let payload = unsafe { call_plugin_detailed(call, 0, OP_APPLY, "", 0) }
        .expect_err("旧插件返回 STATUS_ERR 时必须得到错误载荷");
    assert_eq!(payload.error, "legacy apply failed");
    assert_eq!(payload.code, "unknown", "旧文本没有 code，必须标成 unknown");
    assert_eq!(payload.category, "unknown");
    assert_eq!(payload.certainty, "unknown");
    assert_ne!(
        payload.certainty, "not_executed",
        "宿主没有派发前证据，不能声称未执行"
    );

    let callback = unsafe { call_plugin_detailed(call, 0, OP_CALLBACK, "", 0) }
        .expect_err("旧插件的回调错误也必须可读");
    assert_eq!(callback.error, "legacy callback failed");
    assert_eq!(callback.code, "unknown");
    assert_eq!(callback.certainty, "unknown");

    let app = App::new();
    let fiber = app
        .root
        .plugin(Arc::new(plugin) as Arc<dyn Plugin>, Config::Null);
    tokio::time::timeout(Duration::from_secs(10), fiber.settle())
        .await
        .expect("apply 结算超时");
    assert_eq!(fiber.state(), FiberState::Failed);
    let error = fiber.error().expect("失败的 apply 必须留下可读错误");
    assert!(
        error.contains("legacy apply failed"),
        "旧插件的纯文本必须保留：{error:?}"
    );
    assert!(
        !error.contains("\"code\""),
        "旧插件错误不能被伪装成结构化信封：{error:?}"
    );

    drop(fiber);
    drop(app);
    assert_eq!(loaded_instances(), 0, "失败的 apply 不得泄漏实例");
    drop(target);
}
