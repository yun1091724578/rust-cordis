//! P0-1a：CLI 的 `--source-policy` 在 **dlopen 之前**执行来源准入，并把
//! artifact / SHA256 / sidecar manifest 的 provenance 放进 inspect / status 输出。
//!
//! 真 DLL 来自冻结的 ABI v1 夹具（`tests/fixtures/abi-v1`）：策略校验必须先于
//! 动态库加载，因此成功路径必须用真正可加载的 cdylib，而不是假字节。

use serde_json::{json, Value};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/cli 的上两级是仓库根")
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

/// 构建冻结夹具一次；返回真实 cdylib 路径。
fn fixture_artifact() -> &'static Path {
    static ARTIFACT: OnceLock<PathBuf> = OnceLock::new();
    ARTIFACT.get_or_init(|| {
        let manifest = repo_root().join("tests/fixtures/abi-v1/Cargo.toml");
        assert!(
            manifest.is_file(),
            "夹具源码必须存在：{}",
            manifest.display()
        );
        let target_root = option_env!("CARGO_TARGET_TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join(format!("cordis-cli-source-policy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&target_root);
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
        let status = Command::new(cargo)
            .args(["build", "--manifest-path"])
            .arg(&manifest)
            .arg("--target-dir")
            .arg(&target_root)
            .args(["--offline", "--locked", "--quiet"])
            .status()
            .expect("启动冻结夹具构建");
        assert!(status.success(), "冻结夹具构建失败");
        let name = if cfg!(windows) {
            "abi_v1_frozen.dll"
        } else if cfg!(target_os = "macos") {
            "libabi_v1_frozen.dylib"
        } else {
            "libabi_v1_frozen.so"
        };
        find_artifact(&target_root, name).unwrap_or_else(|| panic!("冻结夹具产物不存在：{name}"))
    })
}

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "cordis-cli-source-policy-{}-{label}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(path.join("plugins")).unwrap();
        Self { path }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

struct Fixture {
    dir: TempDir,
    plugin_stem: String,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let dir = TempDir::new(label);
        let source = fixture_artifact();
        let name = source.file_name().expect("夹具产物有文件名");
        let destination = dir.path.join("plugins").join(name);
        std::fs::copy(source, &destination).expect("复制夹具");
        let plugin_stem = destination
            .file_stem()
            .and_then(OsStr::to_str)
            .expect("夹具产物有 stem")
            .trim_start_matches("lib")
            .to_string();
        let sha = cordis_host::source::sha256_file(&destination).expect("夹具 SHA256");
        write_manifest(&destination, &sha);
        let allow_list = json!({
            "allowed": [{
                "id": "abi-v1-frozen",
                "version": "0.1.1",
                "source_revision": "tree:test",
                "abi_version": 1,
                "sha256": sha,
            }]
        });
        std::fs::write(
            dir.path.join("allow-list.json"),
            format!("{}\n", serde_json::to_string_pretty(&allow_list).unwrap()),
        )
        .unwrap();
        std::fs::write(
            dir.path.join("cordis.yml"),
            format!("- id: frozen\n  name: {plugin_stem}\n"),
        )
        .unwrap();
        Self { dir, plugin_stem }
    }

    fn config(&self) -> PathBuf {
        self.dir.path.join("cordis.yml")
    }

    fn plugin_dir(&self) -> PathBuf {
        self.dir.path.join("plugins")
    }

    fn allow_list(&self) -> PathBuf {
        self.dir.path.join("allow-list.json")
    }

    fn artifact(&self) -> PathBuf {
        let name = if cfg!(windows) {
            format!("{}.dll", self.plugin_stem)
        } else if cfg!(target_os = "macos") {
            format!("lib{}.dylib", self.plugin_stem)
        } else {
            format!("lib{}.so", self.plugin_stem)
        };
        self.plugin_dir().join(name)
    }

    fn tamper(&self) {
        let path = self.artifact();
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[0] ^= 0x01;
        std::fs::write(&path, bytes).unwrap();
    }
}

fn write_manifest(artifact: &Path, sha: &str) {
    let manifest = json!({
        "id": "abi-v1-frozen",
        "version": "0.1.1",
        "source_revision": "tree:test",
        "abi_version": 1,
        "sha256": sha,
    });
    std::fs::write(
        cordis_host::source::manifest_path(artifact),
        format!("{}\n", serde_json::to_string_pretty(&manifest).unwrap()),
    )
    .unwrap();
}

fn run(args: &[&OsStr]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cordis"))
        .args(args)
        .output()
        .expect("启动 cordis")
}

fn inspect(fixture: &Fixture, json: bool) -> Output {
    let config = fixture.config();
    let plugin_dir = fixture.plugin_dir();
    let allow_list = fixture.allow_list();
    let mut args: Vec<&OsStr> = vec![
        OsStr::new("inspect"),
        config.as_os_str(),
        OsStr::new("--plugin-dir"),
        plugin_dir.as_os_str(),
        OsStr::new("--source-policy"),
        allow_list.as_os_str(),
    ];
    if json {
        args.push(OsStr::new("--json"));
    }
    run(&args)
}

fn status(fixture: &Fixture, json: bool) -> Output {
    let config = fixture.config();
    let plugin_dir = fixture.plugin_dir();
    let allow_list = fixture.allow_list();
    let mut args: Vec<&OsStr> = vec![
        OsStr::new("status"),
        config.as_os_str(),
        OsStr::new("--plugin-dir"),
        plugin_dir.as_os_str(),
        OsStr::new("--source-policy"),
        allow_list.as_os_str(),
    ];
    if json {
        args.push(OsStr::new("--json"));
    }
    run(&args)
}

fn output_text(output: &Output) -> String {
    format!(
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn inspect_without_policy_or_dev_flag_denies_by_default() {
    let config = std::env::temp_dir().join(format!(
        "cordis-source-policy-default-deny-{}.yml",
        std::process::id()
    ));
    std::fs::write(&config, "[]\n").unwrap();
    let output = run(&[OsStr::new("inspect"), config.as_os_str()]);
    let text = output_text(&output);
    assert_eq!(output.status.code(), Some(3), "{text}");
    assert!(text.contains("未配置插件来源策略"), "{text}");
    let _ = std::fs::remove_file(config);
}

#[test]
fn inspect_with_source_policy_shows_manifest_provenance() {
    let fixture = Fixture::new("inspect-ok");
    let output = inspect(&fixture, false);
    let text = output_text(&output);
    assert_eq!(output.status.code(), Some(0), "{text}");
    assert!(text.contains("(plugin: abi-v1-frozen)"), "{text}");
    assert!(text.contains("provenance source = manifest"), "{text}");
    assert!(text.contains("source_revision: tree:test"), "{text}");
    assert!(
        text.contains("manifest: id=abi-v1-frozen version=0.1.1 abi_version=1"),
        "{text}"
    );
    let sha = cordis_host::source::sha256_file(&fixture.artifact()).unwrap();
    assert!(text.contains("admission: enforced"), "{text}");
    assert!(text.contains(&format!("sha256: {sha}")), "{text}");
    assert!(text.contains(&format!("manifest sha256: {sha}")), "{text}");
    assert!(text.contains("hash matches manifest: true"), "{text}");
    assert!(
        text.contains(&format!("artifact: {}", fixture.artifact().display())),
        "provenance 必须给出原始 artifact 路径：{text}"
    );

    let output = inspect(&fixture, true);
    let text = output_text(&output);
    assert_eq!(output.status.code(), Some(0), "{text}");
    let json: Value = serde_json::from_slice(&output.stdout).expect("stdout 必须是 JSON");
    let provenance = &json["rows"][0]["provenance"];
    assert_eq!(provenance["source"], "manifest", "{json}");
    assert_eq!(provenance["admission"], "enforced", "{json}");
    assert_eq!(provenance["sha256"], sha, "{json}");
    assert_eq!(provenance["manifest_sha256"], sha, "{json}");
    assert_eq!(provenance["manifest_hash_matches"], true, "{json}");
    assert_eq!(provenance["source_revision"], "tree:test", "{json}");
    assert_eq!(provenance["manifest"]["id"], "abi-v1-frozen", "{json}");
    assert_eq!(provenance["manifest"]["version"], "0.1.1", "{json}");
    assert_eq!(provenance["manifest"]["abi_version"], 1, "{json}");
}

#[test]
fn status_with_source_policy_shows_manifest_provenance() {
    let fixture = Fixture::new("status-ok");
    let output = status(&fixture, false);
    let text = output_text(&output);
    assert_eq!(output.status.code(), Some(0), "{text}");
    assert!(text.contains("mounted = 1"), "{text}");
    assert!(text.contains("provenance source = manifest"), "{text}");
    assert!(text.contains("admission: enforced"), "{text}");
    assert!(text.contains("hash matches manifest: true"), "{text}");
    assert!(text.contains("- frozen"), "{text}");
    assert!(text.contains("source_revision: tree:test"), "{text}");

    let output = status(&fixture, true);
    let text = output_text(&output);
    assert_eq!(output.status.code(), Some(0), "{text}");
    let json: Value = serde_json::from_slice(&output.stdout).expect("stdout 必须是 JSON");
    assert_eq!(json["mounted"], 1, "{json}");
    assert_eq!(json["provenance"][0]["label"], "frozen", "{json}");
    assert_eq!(json["provenance"][0]["source"], "manifest", "{json}");
    assert_eq!(json["provenance"][0]["admission"], "enforced", "{json}");
    assert_eq!(
        json["provenance"][0]["manifest_hash_matches"], true,
        "{json}"
    );
    assert_eq!(
        json["provenance"][0]["manifest"]["id"], "abi-v1-frozen",
        "{json}"
    );
}

#[test]
fn inspect_with_source_policy_rejects_a_tampered_plugin() {
    let fixture = Fixture::new("inspect-tampered");
    fixture.tamper();
    let output = inspect(&fixture, false);
    let text = output_text(&output);
    assert_eq!(output.status.code(), Some(3), "{text}");
    assert!(text.contains("SHA256"), "{text}");
    assert!(
        !text.contains("LoadLibrary") && !text.contains("dlopen"),
        "策略拒绝必须发生在加载动态库之前：{text}"
    );
}

#[test]
fn status_with_source_policy_rejects_a_tampered_plugin() {
    let fixture = Fixture::new("status-tampered");
    fixture.tamper();
    let output = Command::new(env!("CARGO_BIN_EXE_cordis"))
        .arg("status")
        .arg(fixture.config())
        .arg("--plugin-dir")
        .arg(fixture.plugin_dir())
        .arg("--source-policy")
        .arg(fixture.allow_list())
        .arg("--no-strict")
        .output()
        .expect("启动 cordis status");
    let text = output_text(&output);
    assert_eq!(output.status.code(), Some(4), "{text}");
    assert!(text.contains("SHA256"), "{text}");
    assert!(
        !text.contains("LoadLibrary") && !text.contains("dlopen"),
        "策略拒绝必须发生在加载动态库之前：{text}"
    );
}
