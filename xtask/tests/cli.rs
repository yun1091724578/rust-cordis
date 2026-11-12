//! `cargo xtask docs [path]` 的退出码契约。
//!
//! 单测覆盖检查逻辑；这里只钉住“违规必须变成非零退出”，
//! 否则 `cargo xtask docs && ...` 会在契约破坏时继续往下跑。

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_xtask"))
}

/// 独立构造封存夹具时用：不依赖 xtask 的私有实现。
fn sha256_hex(bytes: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut message = bytes.to_vec();
    let bit_len = (bytes.len() as u64) * 8;
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_be_bytes());
    for chunk in message.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (index, word) in w.iter_mut().take(16).enumerate() {
            let start = index * 4;
            *word = u32::from_be_bytes([
                chunk[start],
                chunk[start + 1],
                chunk[start + 2],
                chunk[start + 3],
            ]);
        }
        for index in 16..64 {
            let s0 = w[index - 15].rotate_right(7)
                ^ w[index - 15].rotate_right(18)
                ^ (w[index - 15] >> 3);
            let s1 = w[index - 2].rotate_right(17)
                ^ w[index - 2].rotate_right(19)
                ^ (w[index - 2] >> 10);
            w[index] = w[index - 16]
                .wrapping_add(s0)
                .wrapping_add(w[index - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for index in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[index])
                .wrapping_add(w[index]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }
    h.iter().map(|word| format!("{word:08x}")).collect()
}

fn write_json(dir: &Path, relative: &str, value: &Value) -> Vec<u8> {
    let bytes = format!("{}\n", serde_json::to_string_pretty(value).unwrap()).into_bytes();
    std::fs::write(dir.join(relative), &bytes).unwrap();
    bytes
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
  "format": "cordis-plugin-seal/2",
  "created_unix": 0,
  "workspace_version": "0.1.0",
  "abi_version": 1,
  "error_format": 1,
  "target": "test",
  "profile": "release",
  "toolchain": {"rustc": "rustc test", "cargo": "cargo test", "rustflags": ""},
  "source": {"kind": "workspace-tree", "sha256": "41cf6794ba4200b839c53531555f0f3998df4cbb01a4d5cb0b94e3ca5e23947d", "files": 1},
  "host": {"workspace_version": "0.1.0", "source_sha256": "41cf6794ba4200b839c53531555f0f3998df4cbb01a4d5cb0b94e3ca5e23947d", "source_files": 1},
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
    let sha = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    let source_revision = "tree:41cf6794ba4200b839c53531555f0f3998df4cbb01a4d5cb0b94e3ca5e23947d";
    let plugins = [
        ("plugins/greeting.bin", "greeting"),
        ("plugins/hello.bin", "hello"),
        ("plugins/harness_tool.bin", "harness-tool"),
    ];
    let manifest_value = |id: &str| {
        let (provides, requires) = match id {
            "greeting" => (json!({ "greeting": "2.0.0" }), json!({})),
            "hello" => (
                json!({ "hello.note": "1.0.0" }),
                json!({ "greeting": "^2" }),
            ),
            "harness-tool" => (
                json!({
                    "sessions": "1.0.0",
                    "tool.harness-echo": "1.0.0",
                    "harness.status": "1.0.0",
                    "harness.stream": "1.0.0",
                }),
                json!({}),
            ),
            _ => (json!({}), json!({})),
        };
        json!({
            "id": id,
            "version": "0.1.0",
            "source_revision": source_revision,
            "abi_version": 1,
            "sha256": sha,
            "provides": provides,
            "requires": requires,
        })
    };
    let mut sidecars = Vec::new();
    for (path, id) in plugins {
        let relative = format!("{path}.manifest.json");
        let bytes = write_json(&dir, &relative, &manifest_value(id));
        sidecars.push((relative, sha256_hex(&bytes)));
    }
    let allowed = plugins
        .iter()
        .map(|(_, id)| manifest_value(id))
        .collect::<Vec<_>>();
    let allow_bytes = write_json(&dir, "allow-list.json", &json!({ "allowed": allowed }));

    let mut sums = String::new();
    for (path, _) in plugins {
        sums.push_str(&format!("{sha}  {path}\n"));
    }
    for (path, hash) in &sidecars {
        sums.push_str(&format!("{hash}  {path}\n"));
    }
    sums.push_str(&format!("{}  allow-list.json\n", sha256_hex(&allow_bytes)));
    sums.push_str(&format!("{sha}  smoke/cordis.yml\n"));
    sums.push_str(&format!("{sha}  smoke/inspect.txt\n"));
    std::fs::write(dir.join("SHA256SUMS"), sums).unwrap();

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
    for args in [
        vec!["seal", "--out", "a", "--verify", "b"],
        vec!["seal", "--root", "a", "--verify", "b"],
    ] {
        let output = Command::new(bin())
            .args(args)
            .output()
            .expect("spawn xtask");
        assert_eq!(output.status.code(), Some(2));
    }
}

#[test]
fn seal_root_requires_a_value_and_may_not_repeat() {
    for args in [
        vec!["seal", "--root"],
        vec!["seal", "--root", "a", "--root", "b"],
    ] {
        let output = Command::new(bin())
            .args(args)
            .output()
            .expect("spawn xtask");
        assert_eq!(output.status.code(), Some(2));
    }
}

#[test]
fn soak_rejects_invalid_or_extra_arguments() {
    for args in [
        vec!["soak", "0"],
        vec!["soak", "many"],
        vec!["soak", "1", "2"],
        vec!["soak", "-1"],
        vec!["soak", "--bogus"],
    ] {
        let output = Command::new(bin())
            .args(args)
            .output()
            .expect("spawn xtask");
        assert_eq!(output.status.code(), Some(2));
    }
    let output = Command::new(bin())
        .env("CORDIS_SOAK_CYCLES", "many")
        .arg("soak")
        .output()
        .expect("spawn xtask");
    assert_eq!(output.status.code(), Some(2));
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
