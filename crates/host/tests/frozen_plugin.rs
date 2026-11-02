//! #7：真实旧插件夹具（冻结 ABI v1 的 cdylib）。
//!
//! 夹具是 `tests/fixtures/abi-v1` 的独立 crate：它复制了当前 `cordis-abi`
//! 的 ABI 类型，编译成真正的 cdylib，**不链接 workspace 的 cordis-abi**。
//! 结构体是真正的 72 字节 v1 形态（只到 `call`，没有 `provision`），并通过
//! `size` 判界读取宿主函数表；现在它是“冻结快照”，下一次 ABI 变更后就是旧版。
//!
//! 本文件必须保持**单测试**：`loaded_instances` / `staged_leaked` /
//! 进程级 `PLUGIN_SOURCE` 都是全局量，第二个测试会互相踩。
//! 这是仓库内测试，依赖仓库根的 `tests/fixtures/abi-v1`；发布包的
//! `cargo package --no-verify` 不会运行它。

use cordis_abi::PLUGIN_DESC_PREFIX_SIZE;
use cordis_core::{App, Config, ExternValue, FiberState};
use cordis_host::loader::{DiskPluginSource, PluginSource};
use cordis_host::patch::Entry;
use cordis_host::{loaded_instances, probe_capabilities, staged_leaked, LoadedPlugin};
use serde_json::json;
use std::ffi::OsStr;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus};
use std::time::{Duration, Instant};

/// 临时 target 目录的守卫：断言失败时也会在 Drop 里清理。
struct TempTarget {
    path: PathBuf,
}

impl TempTarget {
    fn new(label: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("cordis-abi-fixture-{}-{label}", std::process::id()));
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

/// 每个 await 都要有期限：卡住应当是失败，不是挂死 CI。
async fn within<T>(what: &str, future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), future)
        .await
        .unwrap_or_else(|_| panic!("{what} 超时"))
}

fn wait_with_timeout(child: &mut Child, seconds: u64, what: &str) -> ExitStatus {
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait().expect("等待夹具构建") {
            return status;
        }
        if started.elapsed() >= Duration::from_secs(seconds) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("{what} 构建超时");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// target 里可能有 `debug/` 或 `debug/<triple>/`（`CARGO_BUILD_TARGET`），
/// 递归找同名产物即可；临时目录是干净的，不会歧义。
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

fn build_fixture(label: &str, features: &[&str]) -> (TempTarget, PathBuf) {
    let manifest = repo_root().join("tests/fixtures/abi-v1/Cargo.toml");
    assert!(
        manifest.is_file(),
        "夹具源码必须存在（冻结的 ABI v1 crate）：{}",
        manifest.display()
    );

    let target = TempTarget::new(label);
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let mut command = Command::new(cargo);
    command
        .args(["build", "--manifest-path"])
        .arg(&manifest)
        .arg("--target-dir")
        .arg(&target.path)
        .arg("--offline")
        .arg("--locked");
    for feature in features {
        command.arg("--features").arg(feature);
    }

    let mut child = command.spawn().expect("启动 cargo 构建冻结夹具");
    let status = wait_with_timeout(&mut child, 120, label);
    assert!(status.success(), "冻结夹具构建失败（{label}）");

    let name = if cfg!(windows) {
        "abi_v1_frozen.dll"
    } else if cfg!(target_os = "macos") {
        "libabi_v1_frozen.dylib"
    } else {
        "libabi_v1_frozen.so"
    };
    let artifact = find_artifact(&target.path, name)
        .unwrap_or_else(|| panic!("冻结夹具产物不存在：{name}（{label}）"));
    (target, artifact)
}

/// 无依赖 SHA-256：只用于校验夹具与 README 的冻结哈希。
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

/// 从 README 的 provenance 表里取某个文件的 SHA256。
fn readme_hash(readme: &str, file: &str) -> String {
    let needle = format!("`{file}`");
    readme
        .lines()
        .filter(|line| line.contains(&needle))
        .find_map(|line| {
            line.split('`')
                .find(|part| part.len() == 64 && part.chars().all(|ch| ch.is_ascii_hexdigit()))
                .map(str::to_ascii_lowercase)
        })
        .unwrap_or_else(|| panic!("README 缺少 {file} 的 SHA256"))
}

#[tokio::test]
async fn a_frozen_abi_v1_cdylib_loads_and_calls() {
    // SHA-256 自检：冻结哈希不能建立在错误的实现上。
    assert_eq!(
        sha256_hex(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        "SHA-256 自检失败"
    );

    // 冻结纪律：夹具源码/清单必须与 README 记录一致。
    let fixture = repo_root().join("tests/fixtures/abi-v1");
    let readme = std::fs::read_to_string(fixture.join("README.md")).expect("读取夹具 README");
    let lib_hash = sha256_hex(&std::fs::read(fixture.join("src/lib.rs")).expect("读取夹具源码"));
    assert_eq!(
        lib_hash,
        readme_hash(&readme, "src/lib.rs"),
        "夹具源码变了：必须有意更新 README 的哈希（永不跟随 HEAD）"
    );
    let toml_hash = sha256_hex(&std::fs::read(fixture.join("Cargo.toml")).expect("读取夹具清单"));
    assert_eq!(
        toml_hash,
        readme_hash(&readme, "Cargo.toml"),
        "夹具清单变了：必须有意更新 README 的哈希"
    );

    let (valid_target, artifact) = build_fixture("valid", &[]);

    // 1. 能力面：真 v1 结构体没有 provision；volatile 走 {"ret":[...]}。
    let capabilities = probe_capabilities(&artifact).expect("新宿主必须能探测冻结夹具");
    assert_eq!(capabilities.plugin, "abi-v1-frozen");
    assert!(capabilities.inject.is_empty());
    assert!(
        capabilities.provision.is_empty(),
        "真正的 v1 结构体没有 provision；宿主不得越界读"
    );
    assert_eq!(
        capabilities.volatile,
        vec!["data".to_string()],
        "volatile 必须走 {{\"ret\":[...]}} 回包"
    );
    assert_eq!(capabilities.schema(), None, "v1 夹具没有 schema");
    assert_eq!(capabilities.schema_error(), None, "未声明不是非法声明");

    // 2. 尺寸可证伪：直接读原始 desc，确认它声明的就是 v1 尺寸。
    let raw = LoadedPlugin::open(&artifact).expect("新宿主必须能直接打开冻结夹具");
    assert_eq!(raw.schema(), None);
    assert_eq!(raw.schema_error(), None);
    let size = unsafe { (*raw.raw_desc()).size };
    assert_eq!(
        size as usize, PLUGIN_DESC_PREFIX_SIZE,
        "夹具必须声明真正的 v1 尺寸"
    );
    drop(raw);

    // 3. 真装载路径的拒绝半边：版本不兼容、尺寸不足都必须 Err。
    //    坏夹具走 `LoadedPlugin::open`（不经过 stage）：这里只钉 ABI 门禁。
    //    `DiskPluginSource` 的装载失败按 `open_staged` 的约定把副本留在泄漏
    //    台账里，由 `stagedLeaked` 暴露，不在本测试里伪装成“已清理”。
    for (label, features, needle) in [
        ("bad-version", &["bad-version"][..], "ABI 主版本"),
        ("bad-size", &["bad-size"][..], "小于冻结前缀"),
    ] {
        let (bad_target, bad_artifact) = build_fixture(label, features);
        let error = match LoadedPlugin::open(&bad_artifact) {
            Ok(_) => panic!("{label} 坏夹具必须被拒绝"),
            Err(error) => error.to_string(),
        };
        assert!(
            error.contains(needle),
            "{label} 拒绝原因必须包含 {needle:?}，实际：{error}"
        );
        drop(bad_target);
    }

    // 4. 装载 / 暂存 / 挂载 / 回调 / 卸载 / 先卸库后删文件。
    let source = DiskPluginSource;
    let entry = Entry {
        name: artifact.to_string_lossy().into_owned(),
        ..Entry::default()
    };
    let loaded = source
        .load(&entry, &[artifact.parent().unwrap().to_path_buf()], 1)
        .expect("新宿主必须能加载冻结夹具");
    assert!(loaded.staged.exists(), "装载成功后暂存副本必须还在");
    assert_eq!(staged_leaked(), 0, "认领后暂存台账必须归零");
    let plugin = loaded.plugin.clone();

    let marker = valid_target.path.join("disposed.marker");
    let _ = std::fs::remove_file(&marker);
    std::env::set_var("CORDIS_FROZEN_MARKER", &marker);

    let app = App::new();
    let fiber = app.root.plugin(plugin, Config::Null);
    within("settle", fiber.settle()).await;
    assert_eq!(fiber.state(), FiberState::Active);
    assert_eq!(loaded_instances(), 1, "挂载后必须恰好有一个实例");

    let value = within("bail", app.root.bail("fixture/event", vec![]))
        .await
        .expect("冻结夹具的监听器必须应答");
    let value = value
        .downcast::<ExternValue>()
        .expect("宿主侧的值必须是 ExternValue");
    assert_eq!(value.0, json!("frozen-v1"));

    within("dispose", fiber.dispose()).await;
    assert!(marker.exists(), "OP_DISPOSE 必须真的送达（夹具写标记文件）");
    assert_eq!(loaded_instances(), 0, "卸载后实例必须回收");

    let staged = loaded.staged.clone();
    drop(fiber);
    drop(app);
    drop(loaded);
    // Windows 上删除被加载的 DLL 会失败，所以这条断言在 Windows 才有判别力；
    // Unix 上 unlink 已映射的 .so 恒成功。
    assert!(
        !staged.exists(),
        "插件对象全部 drop 后暂存副本必须删除（先卸库、后删文件）：{}",
        staged.display()
    );
    assert_eq!(staged_leaked(), 0, "卸载后暂存台账必须归零");

    std::env::remove_var("CORDIS_FROZEN_MARKER");
    drop(valid_target);
}
