//! `cargo xtask docs [path]` 的退出码契约。
//!
//! 单测覆盖检查逻辑；这里只钉住“违规必须变成非零退出”，
//! 否则 `cargo xtask docs && ...` 会在契约破坏时继续往下跑。

use std::path::PathBuf;
use std::process::Command;

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_xtask"))
}

#[test]
fn docs_passes_on_the_real_contract() {
    let output = Command::new(bin())
        .arg("docs")
        .output()
        .expect("spawn xtask");
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn docs_accepts_an_explicit_path_and_fails_on_a_bogus_reference() {
    let dir = std::env::temp_dir().join(format!("cordis-xtask-cli-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let doc = dir.join("contract.md");
    std::fs::write(
        &doc,
        "## 保证索引\n\n| 保证 | 状态 | 证据 | 备注 |\n|---|---|---|---|\n\
         | A | 保证 | `nope.rs::missing` | — |\n",
    )
    .unwrap();

    let output = Command::new(bin())
        .arg("docs")
        .arg(&doc)
        .output()
        .expect("spawn xtask");
    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("不存在的测试"),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unknown_command_exits_two() {
    let output = Command::new(bin())
        .arg("nosuchcommand")
        .output()
        .expect("spawn xtask");
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn inventory_passes_on_the_real_catalog() {
    let output = Command::new(bin())
        .arg("inventory")
        .output()
        .expect("spawn xtask");
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn inventory_fails_on_a_tampered_catalog() {
    let dir = std::env::temp_dir().join(format!("cordis-xtask-inventory-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let doc = dir.join("inventory.md");
    std::fs::write(&doc, "tampered\n").unwrap();

    let output = Command::new(bin())
        .arg("inventory")
        .arg(&doc)
        .output()
        .expect("spawn xtask");
    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("与源码不一致"),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn inventory_write_round_trips() {
    let dir = std::env::temp_dir().join(format!("cordis-xtask-write-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let doc = dir.join("inventory.md");

    let write = Command::new(bin())
        .arg("inventory")
        .arg("--write")
        .arg(&doc)
        .output()
        .expect("spawn xtask");
    assert_eq!(
        write.status.code(),
        Some(0),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&write.stdout),
        String::from_utf8_lossy(&write.stderr)
    );

    let check = Command::new(bin())
        .arg("inventory")
        .arg(&doc)
        .output()
        .expect("spawn xtask");
    assert_eq!(
        check.status.code(),
        Some(0),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr)
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn inventory_explicit_path_is_repo_relative() {
    let output = Command::new(bin())
        .current_dir(std::env::temp_dir())
        .arg("inventory")
        .arg("docs/api-inventory.md")
        .output()
        .expect("spawn xtask");
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn inventory_rejects_conflicting_modes() {
    let output = Command::new(bin())
        .arg("inventory")
        .arg("--write")
        .arg("--check")
        .output()
        .expect("spawn xtask");
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn matrix_passes_on_the_real_documents() {
    let output = Command::new(bin())
        .arg("matrix")
        .output()
        .expect("spawn xtask");
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("平台矩阵结构检查通过"),
        "stdout={}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn matrix_fails_on_a_missing_guarantee() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask 位于仓库根目录下")
        .to_path_buf();
    let dir = std::env::temp_dir().join(format!("cordis-xtask-matrix-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let support = dir.join("support.md");
    let platform = dir.join("platform.md");
    std::fs::copy(root.join("docs/support-contract.md"), &support).unwrap();
    let text = std::fs::read_to_string(root.join("docs/platform-matrix.md")).unwrap();
    let tampered = text
        .lines()
        .filter(|line| !line.contains("撞名的 `provide`"))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&platform, tampered).unwrap();

    let output = Command::new(bin())
        .arg("matrix")
        .arg(&support)
        .arg(&platform)
        .output()
        .expect("spawn xtask");
    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("缺少保证"),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn matrix_rejects_extra_arguments() {
    let output = Command::new(bin())
        .args(["matrix", "a", "b", "c"])
        .output()
        .expect("spawn xtask");
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn seal_verify_accepts_a_complete_seal() {
    let dir = std::env::temp_dir().join(format!("cordis-xtask-seal-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("plugins")).unwrap();
    std::fs::create_dir_all(dir.join("smoke")).unwrap();
    for name in ["greeting.bin", "hello.bin", "harness_tool.bin"] {
        std::fs::write(dir.join("plugins").join(name), b"abc").unwrap();
    }
    std::fs::write(dir.join("smoke/cordis.yml"), b"abc").unwrap();
    std::fs::write(dir.join("smoke/inspect.txt"), b"abc").unwrap();
    std::fs::write(
        dir.join("seal.json"),
        r#"{
  "format": "cordis-plugin-seal/1",
  "created_unix": 0,
  "workspace_version": "0.1.0",
  "abi_version": 1,
  "error_format": 1,
  "target": "test",
  "profile": "release",
  "toolchain": {"rustc": "rustc test", "cargo": "cargo test", "rustflags": ""},
  "source": {"kind": "workspace-tree", "sha256": "41cf6794ba4200b839c53531555f0f3998df4cbb01a4d5cb0b94e3ca5e23947d", "files": 1},
  "artifacts": [
    {"package": "plugin-greeting", "crate": "greeting", "path": "plugins/greeting.bin", "sha256": "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad", "bytes": 3},
    {"package": "plugin-hello", "crate": "hello", "path": "plugins/hello.bin", "sha256": "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad", "bytes": 3},
    {"package": "plugin-harness-tool", "crate": "harness_tool", "path": "plugins/harness_tool.bin", "sha256": "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad", "bytes": 3}
  ],
  "smoke": {
    "config": "smoke/cordis.yml",
    "config_sha256": "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
    "config_bytes": 3,
    "evidence": "smoke/inspect.txt",
    "evidence_sha256": "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
    "evidence_bytes": 3,
    "checked": true,
    "method": "test"
  }
}
"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("SHA256SUMS"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad  plugins/greeting.bin\n\
         ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad  plugins/hello.bin\n\
         ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad  plugins/harness_tool.bin\n\
         ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad  smoke/cordis.yml\n\
         ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad  smoke/inspect.txt\n",
    )
    .unwrap();

    let output = Command::new(bin())
        .arg("seal")
        .arg("--verify")
        .arg(&dir)
        .output()
        .expect("spawn xtask");
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("封存校验通过"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn seal_verify_fails_with_a_readable_error() {
    let dir = std::env::temp_dir().join(format!("cordis-xtask-seal-broken-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let output = Command::new(bin())
        .arg("seal")
        .arg("--verify")
        .arg(&dir)
        .output()
        .expect("spawn xtask");
    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("读取 seal.json 失败"),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn seal_refuses_to_overwrite_a_nonempty_output() {
    let dir = std::env::temp_dir().join(format!("cordis-xtask-seal-out-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("keep.txt"), "keep").unwrap();

    let output = Command::new(bin())
        .arg("seal")
        .arg("--out")
        .arg(&dir)
        .output()
        .expect("spawn xtask");
    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("不可覆盖"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn seal_rejects_conflicting_modes() {
    let output = Command::new(bin())
        .args(["seal", "--out", "a", "--verify", "b"])
        .output()
        .expect("spawn xtask");
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn soak_rejects_invalid_or_extra_arguments() {
    for args in [
        vec!["soak", "0"],
        vec!["soak", "many"],
        vec!["soak", "1", "2"],
    ] {
        let output = Command::new(bin())
            .args(args)
            .output()
            .expect("spawn xtask");
        assert_eq!(output.status.code(), Some(2));
    }
    let output = Command::new(bin())
        .args(["soak", "--help"])
        .output()
        .expect("spawn xtask");
    assert_eq!(output.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&output.stdout).contains("soak [cycles]"));
}

#[test]
fn docs_explicit_path_is_repo_relative() {
    let output = Command::new(bin())
        .current_dir(std::env::temp_dir())
        .arg("docs")
        .arg("docs/support-contract.md")
        .output()
        .expect("spawn xtask");
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
