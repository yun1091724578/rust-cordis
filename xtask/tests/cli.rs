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
    let evidence = concat!(
        "mounted = 0\n",
        "- greeting (plugin: greeting)\n",
        "- hello (plugin: hello)\n",
        "- harness (plugin: harness-tool)\n",
    );
    let evidence_sha = sha256_hex(evidence.as_bytes());
    let evidence_bytes = evidence.len();
    std::fs::write(dir.join("smoke/inspect.txt"), evidence).unwrap();
    std::fs::write(
        dir.join("seal.json"),
        format!(
            r#"{{
  "format": "cordis-plugin-seal/2",
  "created_unix": 0,
  "workspace_version": "0.1.0",
  "abi_version": 1,
  "error_format": 1,
  "target": "test",
  "profile": "release",
  "toolchain": {{"rustc": "rustc test", "cargo": "cargo test", "rustflags": ""}},
  "source": {{"kind": "workspace-tree", "sha256": "41cf6794ba4200b839c53531555f0f3998df4cbb01a4d5cb0b94e3ca5e23947d", "files": 1}},
  "host": {{"workspace_version": "0.1.0", "source_sha256": "41cf6794ba4200b839c53531555f0f3998df4cbb01a4d5cb0b94e3ca5e23947d", "source_files": 1}},
  "artifacts": [
    {{"package": "plugin-greeting", "crate": "greeting", "path": "plugins/greeting.bin", "sha256": "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad", "bytes": 3}},
    {{"package": "plugin-hello", "crate": "hello", "path": "plugins/hello.bin", "sha256": "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad", "bytes": 3}},
    {{"package": "plugin-harness-tool", "crate": "harness_tool", "path": "plugins/harness_tool.bin", "sha256": "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad", "bytes": 3}}
  ],
  "smoke": {{
    "config": "smoke/cordis.yml",
    "config_sha256": "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
    "config_bytes": 3,
    "evidence": "smoke/inspect.txt",
    "evidence_sha256": "{evidence_sha}",
    "evidence_bytes": {evidence_bytes},
    "checked": true,
    "method": "test"
  }}
}}
"#
        ),
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
            "greeting" => (
                json!({ "greeting": "2.0.0", "greeting.text": "1.0.0" }),
                json!({}),
            ),
            "hello" => (json!({ "hello.note": "1.0.0" }), json!({})),
            "harness-tool" => (
                json!({
                    "sessions@1": "1.0.0",
                    "tool.harness-echo@1": "1.0.0",
                    "harness.status@1": "1.0.0",
                    "harness.stream@1": "1.0.0",
                    "harness.status": "1.0.0",
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
    sums.push_str(&format!("{evidence_sha}  smoke/inspect.txt\n"));
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
fn seal_consume_verifies_integrity_before_consumer() {
    let dir =
        std::env::temp_dir().join(format!("cordis-xtask-invalid-seal-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("plugins")).unwrap();
    std::fs::write(dir.join("seal.json"), "{}").unwrap();
    std::fs::write(dir.join("allow-list.json"), "{\"allowed\":[]}").unwrap();

    let output = Command::new(bin())
        .arg("seal-consume")
        .arg(&dir)
        .output()
        .expect("spawn xtask");
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("封存校验失败"),
        "must fail integrity before trying the consumer: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn seal_consume_requires_one_existing_seal() {
    let output = Command::new(bin())
        .arg("seal-consume")
        .output()
        .expect("spawn xtask");
    assert_eq!(
        output.status.code(),
        Some(2),
        "missing dir is a usage error"
    );

    let missing =
        std::env::temp_dir().join(format!("cordis-xtask-missing-seal-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&missing);
    let output = Command::new(bin())
        .arg("seal-consume")
        .arg(&missing)
        .output()
        .expect("spawn xtask");
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("seal.json"),
        "error must name the missing required file"
    );
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
fn soak_rejects_invalid_staged_cycles_env() {
    // `soak` 无位置参数时只读环境变量：非法值必须在拉起 cargo 之前就被拒。
    for raw in ["0", "-1", "abc"] {
        let output = Command::new(bin())
            .env("CORDIS_STAGED_SOAK_CYCLES", raw)
            .arg("soak")
            .output()
            .expect("spawn xtask");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            output.status.code(),
            Some(2),
            "CORDIS_STAGED_SOAK_CYCLES={raw}: stdout={} stderr={stderr}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(
            stderr.contains("CORDIS_STAGED_SOAK_CYCLES") && stderr.contains("正整数"),
            "CORDIS_STAGED_SOAK_CYCLES={raw} 的错误必须点名变量与取值要求：stderr={stderr}"
        );
    }
}

#[test]
fn dhat_rejects_an_unknown_argument() {
    let output = Command::new(bin())
        .args(["dhat", "--bogus"])
        .output()
        .expect("spawn xtask");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(2),
        "stdout={} stderr={stderr}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        stderr.contains("dhat 不接受参数"),
        "未知参数必须可读：stderr={stderr}"
    );
}

#[test]
fn dhat_help_exits_zero_and_names_dhat() {
    let output = Command::new(bin())
        .args(["dhat", "--help"])
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
        String::from_utf8_lossy(&output.stdout).contains("dhat"),
        "stdout={}",
        String::from_utf8_lossy(&output.stdout)
    );
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

#[test]
fn package_check_help_lists_the_new_command() {
    let output = Command::new(bin())
        .args(["package-check", "--help"])
        .output()
        .expect("spawn xtask");
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("package-check"), "stdout={stdout}");
    // 三个旋钮必须在用法里看得见，否则"失败保留目录"就没人知道。
    // `--reuse-dir` 是调试口，同样写进用法（否则"改坏产物必红"的证据没人能复现）。
    for needle in [
        "--keep",
        "--online",
        "CORDIS_PACKAGE_CONSUMER_DIR",
        "--reuse-dir",
    ] {
        assert!(stdout.contains(needle), "用法缺少 {needle}: {stdout}");
    }
}

#[test]
fn package_check_rejects_bad_arguments_before_packaging() {
    // 参数错误必须是 usage 错误（2）：此时一行 cargo 命令都还没跑。
    for args in [
        vec!["package-check", "--bogus"],
        vec!["package-check", "positional"],
        vec!["package-check", "--dir"],
        vec!["package-check", "--dir", "relative/path"],
        vec!["package-check", "--dir", "/a", "--dir", "/b"],
    ] {
        let output = Command::new(bin())
            .args(&args)
            .output()
            .expect("spawn xtask");
        assert_eq!(
            output.status.code(),
            Some(2),
            "{args:?}: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn package_check_refuses_a_root_inside_the_repo_or_a_non_ascii_root() {
    // 两个前置判据在拉起 cargo 之前就要拦住：目录必须 ASCII 且在仓库之外。
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask 位于仓库根目录下")
        .to_path_buf();
    let inside = repo.join("target").join("package-check-should-not-exist");
    let output = Command::new(bin())
        .args(["package-check", "--dir"])
        .arg(&inside)
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
        String::from_utf8_lossy(&output.stderr).contains("不能位于仓库内"),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!inside.exists(), "被拒绝的目录不该被创建");
}

#[test]
#[cfg(windows)]
fn package_check_refuses_a_non_ascii_root() {
    let output = Command::new(bin())
        .args(["package-check", "--dir", "C:/科迪斯/包检查"])
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
        String::from_utf8_lossy(&output.stderr).contains("ASCII"),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !Path::new("C:/科迪斯/包检查").exists(),
        "被拒绝的目录不该被创建"
    );
}

#[test]
fn package_check_refuses_to_wipe_a_foreign_directory() {
    // `--dir` 打错一个字符就是递归删除：非空且没有归属标记的目录必须拒绝清空。
    let dir = std::env::temp_dir().join(format!(
        "cordis-package-check-foreign-{}",
        std::process::id()
    ));
    if !dir.to_string_lossy().is_ascii() {
        return; // 非 ASCII 临时目录会先被 ASCII 判据拦下，这个用例无从验证。
    }
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("keep.txt"), b"keep").unwrap();

    let output = Command::new(bin())
        .args(["package-check", "--dir"])
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
        String::from_utf8_lossy(&output.stderr).contains("拒绝清空"),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(dir.join("keep.txt").exists(), "拒绝清空的目录必须原样保留");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn package_check_rejects_a_dir_with_a_parent_component() {
    // `..` 在 canonicalize 之前无法判定落点，必须在拉起任何 cargo 之前就拒。
    let sneaky = std::env::temp_dir()
        .join("cordis-package-check-sneaky")
        .join("..")
        .join("elsewhere");
    let output = Command::new(bin())
        .args(["package-check", "--dir"])
        .arg(&sneaky)
        .output()
        .expect("spawn xtask");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(2),
        "含 `..` 的 --dir 是用法错误：stdout={} stderr={stderr}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(stderr.contains("不能含 `..`"), "stderr={stderr}");
    assert!(
        !std::env::temp_dir()
            .join("cordis-package-check-sneaky")
            .join("..")
            .join("elsewhere")
            .exists(),
        "被拒绝的目录不该被创建"
    );
}

#[test]
fn package_check_rejects_a_drive_root() {
    // 驱动器根没有父目录，而 `remove_owned_dir` 允许删"空目录"：一旦放过就是整盘
    // `remove_dir_all`。这个形状永远不是临时目录，必须在解析阶段拒（连目录都不碰）。
    let drive_root = if cfg!(windows) { "C:\\" } else { "/" };
    let output = Command::new(bin())
        .args(["package-check", "--dir", drive_root])
        .output()
        .expect("spawn xtask");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(2),
        "驱动器根是用法错误：stdout={} stderr={stderr}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(stderr.contains("驱动器根"), "stderr={stderr}");
}

#[test]
#[cfg(windows)]
fn package_check_refuses_a_case_variant_of_the_repo_path() {
    // 盘符/目录大小写不同仍是同一个目录：canonicalize 之后必须认出它在仓库里。
    // 否则消费者会继承仓库的 .cargo/config.toml，cleanup 还会递归删仓库内目录。
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask 位于仓库根目录下")
        .to_path_buf();
    let dir = PathBuf::from(repo.to_string_lossy().to_uppercase())
        .join("target")
        .join("package-check-case-variant");
    let output = Command::new(bin())
        .args(["package-check", "--dir"])
        .arg(&dir)
        .output()
        .expect("spawn xtask");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout={} stderr={stderr}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(stderr.contains("不能位于仓库内"), "stderr={stderr}");
    assert!(!dir.exists(), "被拒绝的目录不该被创建");
}

#[test]
fn package_check_reuse_dir_requires_an_existing_marked_root() {
    // `--reuse-dir` 只复用带自己标记的目录：既不能凭空造，也不能指向别人的目录。
    let missing = std::env::temp_dir().join(format!(
        "cordis-package-reuse-missing-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&missing);
    let output = Command::new(bin())
        .args(["package-check", "--reuse-dir", "--dir"])
        .arg(&missing)
        .output()
        .expect("spawn xtask");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout={} stderr={stderr}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        stderr.contains("--reuse-dir 要求目录已存在"),
        "stderr={stderr}"
    );
    assert!(!missing.exists(), "被拒绝的目录不该被创建");

    let foreign = std::env::temp_dir().join(format!(
        "cordis-package-reuse-foreign-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&foreign);
    std::fs::create_dir_all(&foreign).unwrap();
    std::fs::write(foreign.join("keep.txt"), b"keep").unwrap();
    let output = Command::new(bin())
        .args(["package-check", "--reuse-dir", "--dir"])
        .arg(&foreign)
        .output()
        .expect("spawn xtask");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "stderr={stderr}");
    assert!(
        stderr.contains("只复用 package-check 自己的目录"),
        "stderr={stderr}"
    );
    assert!(
        foreign.join("keep.txt").exists(),
        "复用被拒时不能动别人的目录"
    );
    let _ = std::fs::remove_dir_all(&foreign);
}

/// 解包目录里 host 的源码文件（`unpacked/rust-cordis-host-<版本>/src/lib.rs`）。
fn unpacked_host_source(root: &Path) -> PathBuf {
    let unpacked = root.join("unpacked");
    let dir = std::fs::read_dir(&unpacked)
        .unwrap_or_else(|err| panic!("读取 {} 失败：{err}", unpacked.display()))
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.is_dir()
                && path
                    .file_name()
                    .map(|name| name.to_string_lossy().starts_with("rust-cordis-host-"))
                    .unwrap_or(false)
        })
        .unwrap_or_else(|| panic!("{} 里没有 rust-cordis-host 的解包目录", unpacked.display()));
    dir.join("src").join("lib.rs")
}

/// `--reuse-dir` 的负向证据：改坏**解包产物**之后，检查必须变红。
///
/// 为什么要标 `#[ignore]`：第一次运行要先跑一次完整的 `package-check --keep`
/// （四个 crate 打包 + 隔离消费者构建，分钟级），放进 `cargo test -p xtask`
/// 会把秒级套件变成分钟级；而证据本身仍然可重复——
/// `cargo test -p xtask --test cli -- --ignored reuse_dir_flags`。
///
/// 复用的目录：`CORDIS_PACKAGE_CONSUMER_DIR`（与 package-check 共用），否则
/// `<temp>/cordis-package-reuse`。目录里已有 package-check 标记就跳过准备阶段。
///
/// 恢复文件用"仓库外备份 + SHA256 双校验"：备份放在 root（仓库之外）里，
/// 恢复后再核一次 SHA256，任何一步对不上就直接红，不留半截改动。
#[test]
#[ignore = "慢：需要先跑一次 package-check --keep；用 --ignored 手动执行"]
fn reuse_dir_flags_a_broken_unpacked_artifact() {
    let root = match std::env::var_os("CORDIS_PACKAGE_CONSUMER_DIR") {
        Some(value) => PathBuf::from(value),
        None => std::env::temp_dir().join("cordis-package-reuse"),
    };
    if !root.to_string_lossy().is_ascii() {
        eprintln!(
            "跳过：目录不是纯 ASCII（{}），package-check 会先拒绝它",
            root.display()
        );
        return;
    }
    if !root.join("package-check-root.marker").exists() {
        let output = Command::new(bin())
            .args(["package-check", "--keep", "--dir"])
            .arg(&root)
            .output()
            .expect("spawn xtask");
        assert_eq!(
            output.status.code(),
            Some(0),
            "准备阶段（package-check --keep）必须通过：stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let source = unpacked_host_source(&root);
    let original = std::fs::read(&source).unwrap();
    let digest = sha256_hex(&original);
    let backup = root.join("reuse-backup-host-lib.rs");
    std::fs::write(&backup, &original).unwrap();
    assert_eq!(
        sha256_hex(&std::fs::read(&backup).unwrap()),
        digest,
        "备份必须与原始文件逐字节一致"
    );

    // 一处**沉默**的语义改动：实例计数多算一个。编译照过，只有判据能抓住。
    const NEEDLE: &str = "instances().lock().unwrap().live()";
    let text = String::from_utf8(original.clone()).expect("解包产物里的 host 源码应是 UTF-8");
    assert_eq!(
        text.matches(NEEDLE).count(),
        1,
        "解包产物里应恰好有一处 `{NEEDLE}`；源码结构变了就更新这个用例"
    );
    let mutated = text.replacen(NEEDLE, "instances().lock().unwrap().live() + 1", 1);
    std::fs::write(&source, mutated.as_bytes()).unwrap();

    let output = Command::new(bin())
        .args(["package-check", "--reuse-dir", "--dir"])
        .arg(&root)
        .output()
        .expect("spawn xtask");

    // 先恢复，再断言：无论检查结果如何，仓库外的解包产物都要回到原状。
    std::fs::write(&source, &original).unwrap();
    let restored = std::fs::read(&source).unwrap();
    assert_eq!(
        sha256_hex(&restored),
        digest,
        "恢复后的 SHA256 必须等于原始值"
    );
    assert_eq!(restored, original, "恢复后必须逐字节等于原始文件");
    let _ = std::fs::remove_file(&backup);

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(1),
        "改坏解包产物必须判红：stdout={stdout} stderr={stderr}"
    );
    assert!(
        !stdout.contains("[package-check] --- package"),
        "`--reuse-dir` 必须跳过打包阶段（否则红可能只是重新打包造成的）：stdout={stdout}"
    );
    assert!(
        stderr.contains("失败于阶段 \"run\""),
        "失败必须落在 run 阶段：stderr={stderr}"
    );
    assert!(
        stderr.contains("装载后应有 1 个 ABI 实例，实际 2"),
        "失败原因必须可读（来自产物侧断言的原文）：stderr={stderr}"
    );
}

#[test]
fn arch_passes_on_the_real_repo() {
    let output = Command::new(bin())
        .arg("arch")
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
fn docs_consistency_and_reference_index_pass() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let sdk_guide = std::fs::read_to_string(root.join("SDK指南.md")).unwrap();
    let readme = std::fs::read_to_string(root.join("README.md")).unwrap();

    assert!(
        !sdk_guide.contains("第三方组件（受审计）"),
        "SDK指南.md 不应与 README / ADR 0005 冲突（T2 不承载第三方插件）"
    );
    assert!(
        !sdk_guide.contains("计划中（ADR，M8）"),
        "SDK指南.md 不应与 ADR 0001 冲突（T3 已决定不实现）"
    );
    assert!(
        !readme.contains("dist/cordis-0.1.1-formal"),
        "README.md 不应引用仓库中不存在的 dist/cordis-0.1.1-formal 路径"
    );
    let ref_index = root.join("docs/references-and-milestones.md");
    assert!(
        ref_index.is_file(),
        "缺少里程碑代号与上游参考索引文档: {}",
        ref_index.display()
    );
}
