//! 首发产物封存：构建 release 插件，记录源码树 / 工具链 / ABI / SHA256，
//! 并用**同版本宿主**做一次 `inspect` 加载检查。
//!
//! 封存目录不可覆盖：已有非空目录直接拒绝。这样“首发产物”不会在第二次
//! 运行时被悄悄替换。校验只读 `seal.json` 与 `SHA256SUMS`，不重新构建，
//! 因此可以在后续宿主版本上独立验证。

use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const SEAL_FORMAT: &str = "cordis-plugin-seal/2";
/// 历史格式：没有 sidecar manifest / `allow-list.json`，只校验 artifact 与 smoke。
const SEAL_FORMAT_V1: &str = "cordis-plugin-seal/1";
const SEAL_FILE: &str = "seal.json";
const SUMS_FILE: &str = "SHA256SUMS";
const ALLOW_LIST_FILE: &str = "allow-list.json";
const SMOKE_CONFIG: &str = "smoke/cordis.yml";
const SMOKE_EVIDENCE: &str = "smoke/inspect.txt";
const SMOKE_METHOD: &str = "cordis inspect --plugin-dir plugins --source-policy allow-list.json \
     (PluginSourcePolicy::verify + LoadedPlugin::open + OP_PING / OP_VOLATILE_PATHS; no mount; \
     host identity is recorded under host.*)";

const PLUGINS: &[PluginSpec] = &[
    PluginSpec {
        package: "plugin-greeting",
        krate: "greeting",
        label: "greeting",
        plugin: "greeting",
        provides: &[("greeting", "2.0.0"), ("greeting.text", "1.0.0")],
        requires: &[],
    },
    PluginSpec {
        package: "plugin-hello",
        krate: "hello",
        label: "hello",
        plugin: "hello",
        provides: &[("hello.note", "1.0.0")],
        // hello 对 greeting 是 PENDING 语义，不是硬依赖；不能在预检里硬失败。
        requires: &[],
    },
    PluginSpec {
        package: "plugin-harness-tool",
        krate: "harness_tool",
        label: "harness",
        plugin: "harness-tool",
        provides: &[
            ("sessions@1", "1.0.0"),
            ("tool.harness-echo@1", "1.0.0"),
            ("harness.status@1", "1.0.0"),
            ("harness.stream@1", "1.0.0"),
            ("harness.status", "1.0.0"),
        ],
        // `tools` is provided by the T1 harness host, which is not part of this seal.
        requires: &[],
    },
];

struct PluginSpec {
    package: &'static str,
    krate: &'static str,
    label: &'static str,
    plugin: &'static str,
    provides: &'static [(&'static str, &'static str)],
    requires: &'static [(&'static str, &'static str)],
}

#[derive(Debug)]
struct SealArtifact {
    package: String,
    krate: String,
    path: String,
    sha256: String,
    bytes: u64,
}

#[derive(Debug)]
struct SealManifest {
    workspace_version: String,
    abi_version: u32,
    error_format: u32,
    target: String,
    profile: String,
    created_unix: u64,
    rustc: String,
    cargo: String,
    rustflags: String,
    source_sha256: String,
    source_files: u64,
    host_source_sha256: String,
    host_source_files: u64,
    host_workspace_version: String,
    artifacts: Vec<SealArtifact>,
    smoke_config: String,
    smoke_config_sha256: String,
    smoke_config_bytes: u64,
    smoke_evidence: String,
    smoke_evidence_sha256: String,
    smoke_evidence_bytes: u64,
    smoke_checked: bool,
}

impl SealManifest {
    fn to_json(&self) -> Value {
        json!({
            "format": SEAL_FORMAT,
            "created_unix": self.created_unix,
            "workspace_version": self.workspace_version,
            "abi_version": self.abi_version,
            "error_format": self.error_format,
            "target": self.target,
            "profile": self.profile,
            "toolchain": {
                "rustc": self.rustc,
                "cargo": self.cargo,
                "rustflags": self.rustflags,
            },
            "source": {
                "kind": "workspace-tree",
                "sha256": self.source_sha256,
                "files": self.source_files,
            },
            "host": {
                "workspace_version": self.host_workspace_version,
                "source_sha256": self.host_source_sha256,
                "source_files": self.host_source_files,
            },
            "artifacts": self.artifacts.iter().map(|artifact| json!({
                "package": artifact.package,
                "crate": artifact.krate,
                "path": artifact.path,
                "sha256": artifact.sha256,
                "bytes": artifact.bytes,
            })).collect::<Vec<_>>(),
            "smoke": {
                "config": self.smoke_config,
                "config_sha256": self.smoke_config_sha256,
                "config_bytes": self.smoke_config_bytes,
                "evidence": self.smoke_evidence,
                "evidence_sha256": self.smoke_evidence_sha256,
                "evidence_bytes": self.smoke_evidence_bytes,
                "checked": self.smoke_checked,
                "method": SMOKE_METHOD,
            },
        })
    }

    fn from_json(value: &Value) -> Result<Self, String> {
        let format = value.get("format").and_then(Value::as_str);
        if format != Some(SEAL_FORMAT) && format != Some(SEAL_FORMAT_V1) {
            return Err(format!(
                "封存清单格式不是 {SEAL_FORMAT}（兼容 {SEAL_FORMAT_V1}）"
            ));
        }
        let artifacts = value
            .get("artifacts")
            .and_then(Value::as_array)
            .ok_or("封存清单缺少 artifacts 数组")?
            .iter()
            .map(|artifact| {
                Ok(SealArtifact {
                    package: field_str(artifact, "package")?,
                    krate: field_str(artifact, "crate")?,
                    path: field_str(artifact, "path")?,
                    sha256: field_str(artifact, "sha256")?,
                    bytes: field_u64(artifact, "bytes")?,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let source = value.get("source").ok_or("封存清单缺少 source")?;
        let toolchain = value.get("toolchain").ok_or("封存清单缺少 toolchain")?;
        let smoke = value.get("smoke").ok_or("封存清单缺少 smoke")?;
        Ok(Self {
            workspace_version: field_str(value, "workspace_version")?,
            abi_version: field_u32(value, "abi_version")?,
            error_format: field_u32(value, "error_format")?,
            target: field_str(value, "target")?,
            profile: field_str(value, "profile")?,
            created_unix: field_u64(value, "created_unix")?,
            rustc: field_str(toolchain, "rustc")?,
            cargo: field_str(toolchain, "cargo")?,
            rustflags: toolchain
                .get("rustflags")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            source_sha256: field_str(source, "sha256")?,
            source_files: field_u64(source, "files")?,
            host_source_sha256: value
                .get("host")
                .and_then(|host| host.get("source_sha256"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            host_source_files: value
                .get("host")
                .and_then(|host| host.get("source_files"))
                .and_then(Value::as_u64)
                .unwrap_or(0),
            host_workspace_version: value
                .get("host")
                .and_then(|host| host.get("workspace_version"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            artifacts,
            smoke_config: field_str(smoke, "config")?,
            smoke_config_sha256: field_str(smoke, "config_sha256")?,
            smoke_config_bytes: field_u64(smoke, "config_bytes")?,
            smoke_evidence: field_str(smoke, "evidence")?,
            smoke_evidence_sha256: field_str(smoke, "evidence_sha256")?,
            smoke_evidence_bytes: field_u64(smoke, "evidence_bytes")?,
            smoke_checked: smoke
                .get("checked")
                .and_then(Value::as_bool)
                .ok_or("封存清单缺少 smoke.checked")?,
        })
    }
}

fn field_str(value: &Value, key: &str) -> Result<String, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("封存清单缺少字符串字段 {key}"))
}

fn field_u64(value: &Value, key: &str) -> Result<u64, String> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("封存清单缺少无符号整数字段 {key}"))
}

fn field_u32(value: &Value, key: &str) -> Result<u32, String> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|number| u32::try_from(number).ok())
        .ok_or_else(|| format!("封存清单缺少 u32 字段 {key}"))
}

pub fn command(args: Vec<String>) -> i32 {
    let mut out: Option<PathBuf> = None;
    let mut verify: Option<PathBuf> = None;
    let mut require_v2 = false;
    let mut root: Option<PathBuf> = None;
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--root" => {
                let Some(value) = iter.next() else {
                    eprintln!("seal --root 缺少目录\n\n{}", crate::USAGE);
                    return 2;
                };
                if root.replace(PathBuf::from(value)).is_some() {
                    eprintln!("seal --root 只能出现一次\n\n{}", crate::USAGE);
                    return 2;
                }
            }
            "--out" => {
                let Some(value) = iter.next() else {
                    eprintln!("seal --out 缺少目录\n\n{}", crate::USAGE);
                    return 2;
                };
                if out.replace(PathBuf::from(value)).is_some() {
                    eprintln!("seal --out 只能出现一次\n\n{}", crate::USAGE);
                    return 2;
                }
            }
            "--verify" => {
                let Some(value) = iter.next() else {
                    eprintln!("seal --verify 缺少目录\n\n{}", crate::USAGE);
                    return 2;
                };
                if verify.replace(PathBuf::from(value)).is_some() {
                    eprintln!("seal --verify 只能出现一次\n\n{}", crate::USAGE);
                    return 2;
                }
            }
            "--require-v2" => require_v2 = true,
            "-h" | "--help" => {
                print!("{}", crate::USAGE);
                return 0;
            }
            other => {
                eprintln!("seal 未知参数 \"{other}\"\n\n{}", crate::USAGE);
                return 2;
            }
        }
    }
    if require_v2 && verify.is_none() {
        eprintln!("seal --require-v2 只能与 --verify 同用\n\n{}", crate::USAGE);
        return 2;
    }
    if out.is_some() && verify.is_some() {
        eprintln!("seal --out 与 --verify 只能选一个\n\n{}", crate::USAGE);
        return 2;
    }
    if root.is_some() && verify.is_some() {
        eprintln!(
            "seal --root 只用于生成封存，不能与 --verify 同用\n\n{}",
            crate::USAGE
        );
        return 2;
    }
    if let Some(dir) = verify {
        let dir = crate::resolve_repo_path(dir);
        if require_v2 {
            let format = fs::read_to_string(dir.join(SEAL_FILE))
                .ok()
                .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                .and_then(|value| {
                    value
                        .get("format")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                });
            if format.as_deref() != Some(SEAL_FORMAT) {
                eprintln!(
                    "封存校验失败：{} 的 format 不是 {SEAL_FORMAT}（--require-v2）",
                    dir.display()
                );
                return 1;
            }
        }
        return match verify_dir(&dir) {
            Ok(manifest) => {
                println!("封存校验通过：{}", dir.display());
                print_manifest(&manifest);
                0
            }
            Err(errors) => {
                eprintln!("封存校验失败（{} 条，{}）：", errors.len(), dir.display());
                for error in errors {
                    eprintln!("  - {error}");
                }
                1
            }
        };
    }

    let source_root = root
        .map(crate::resolve_repo_path)
        .unwrap_or_else(crate::workspace_root);
    let out = out
        .map(crate::resolve_repo_path)
        .unwrap_or_else(|| default_output_dir_for(&source_root));
    match seal_to(&out, &source_root) {
        Ok(manifest) => {
            println!("封存完成：{}", out.display());
            print_manifest(&manifest);
            println!("宿主加载检查通过：{SMOKE_METHOD}");
            0
        }
        Err(err) => {
            eprintln!("封存失败：{err}");
            1
        }
    }
}

fn print_manifest(manifest: &SealManifest) {
    println!(
        "  version = {}  abi = {}  error_format = {}",
        manifest.workspace_version, manifest.abi_version, manifest.error_format
    );
    println!(
        "  target = {}  profile = {}",
        manifest.target, manifest.profile
    );
    println!(
        "  source = {}  files = {}",
        manifest.source_sha256, manifest.source_files
    );
    println!(
        "  host = {}  source = {}  files = {}  same_source = {}",
        manifest.host_workspace_version,
        manifest.host_source_sha256,
        manifest.host_source_files,
        manifest.host_source_sha256 == manifest.source_sha256
    );
    println!(
        "  smoke = {}  evidence = {}",
        manifest.smoke_config, manifest.smoke_evidence
    );
    for artifact in &manifest.artifacts {
        println!(
            "  {}  {}  {} bytes",
            artifact.sha256, artifact.path, artifact.bytes
        );
    }
}

fn default_output_dir_for(root: &Path) -> PathBuf {
    let target = host_target(root).unwrap_or_else(|_| "unknown-target".to_string());
    let version = workspace_version(root).unwrap_or_else(|_| env!("CARGO_PKG_VERSION").to_string());
    root.join("dist")
        .join(format!("cordis-plugins-{version}-{target}"))
}

fn abi_constants(root: &Path) -> Result<(u32, u32), String> {
    let path = root.join("crates/abi/src/lib.rs");
    let text =
        fs::read_to_string(&path).map_err(|err| format!("读取 {} 失败：{err}", path.display()))?;
    let read = |name: &str| -> Result<u32, String> {
        text.lines()
            .find_map(|line| {
                let line = line.trim();
                line.strip_prefix(&format!("pub const {name}: u32 ="))
                    .and_then(|rest| rest.trim().trim_end_matches(';').parse().ok())
            })
            .ok_or_else(|| format!("{} 缺少 {name}", path.display()))
    };
    Ok((read("ABI_VERSION")?, read("ERROR_FORMAT_V1")?))
}

fn workspace_version(root: &Path) -> Result<String, String> {
    let text = fs::read_to_string(root.join("Cargo.toml"))
        .map_err(|err| format!("读取 {} 失败：{err}", root.join("Cargo.toml").display()))?;
    text.lines()
        .find_map(|line| {
            line.trim()
                .strip_prefix("version = \"")
                .and_then(|rest| rest.strip_suffix('"'))
                .map(str::to_string)
        })
        .ok_or_else(|| format!("{} 缺少 version 字段", root.join("Cargo.toml").display()))
}

fn cli_supports_source_policy(root: &Path) -> bool {
    ["crates/cli/src/lib.rs", "crates/cli/src/main.rs"]
        .iter()
        .any(|relative| {
            fs::read_to_string(root.join(relative))
                .map(|source| source.contains("--source-policy"))
                .unwrap_or(false)
        })
}

fn seal_to(out: &Path, root: &Path) -> Result<SealManifest, String> {
    // tag 树支持 `--source-policy` 时，宿主 smoke 也来自同一棵树，得到严格
    // 同源证据；历史 tag 不支持时退回当前宿主，manifest 会如实记录跨源。
    let host_root = if cli_supports_source_policy(root) {
        root.to_path_buf()
    } else {
        crate::workspace_root()
    };
    if out.exists() {
        let mut entries =
            fs::read_dir(out).map_err(|err| format!("读取 {} 失败：{err}", out.display()))?;
        if entries.next().is_some() {
            return Err(format!(
                "输出目录已存在且非空：{}（封存产物不可覆盖，请换 --out）",
                out.display()
            ));
        }
    }

    let target = host_target(root)?;
    let rustc = tool_version_in(
        &std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string()),
        root,
    )?;
    let cargo_program = cargo_program();
    let cargo = tool_version_in(&cargo_program, root)?;
    let rustflags = std::env::var("RUSTFLAGS").unwrap_or_default();
    let (source_sha256, source_files) = source_tree_digest(root, out)?;

    let built = build_release_plugins(root, &target)?;
    let (after_sha256, after_files) = source_tree_digest(root, out)?;
    if source_sha256 != after_sha256 || source_files != after_files {
        return Err("构建期间源码树发生变化；请先停掉会改源码的进程，再重新封存".to_string());
    }

    let parent = out.parent().unwrap_or(root);
    fs::create_dir_all(parent).map_err(|err| format!("创建 {} 失败：{err}", parent.display()))?;
    let file_name = out
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or("cordis-plugins");
    let temp = parent.join(format!(
        ".{file_name}.tmp-{}-{}",
        std::process::id(),
        temp_nonce()
    ));
    let mut guard = TempOut {
        path: temp.clone(),
        keep: false,
    };
    fs::create_dir(&temp)
        .map_err(|err| format!("创建临时封存目录 {} 失败：{err}", temp.display()))?;
    fs::create_dir(temp.join("plugins")).map_err(|err| format!("创建临时封存目录失败：{err}"))?;

    let mut artifacts = Vec::new();
    for spec in PLUGINS {
        let source = built
            .get(spec.krate)
            .ok_or_else(|| format!("构建结果缺少 {} 的 cdylib", spec.package))?;
        let name = source
            .file_name()
            .ok_or_else(|| format!("构建产物没有文件名：{}", source.display()))?;
        let destination = temp.join("plugins").join(name);
        fs::copy(source, &destination).map_err(|err| {
            format!(
                "复制 {} 到 {} 失败：{err}",
                source.display(),
                destination.display()
            )
        })?;
        let bytes = fs::read(&destination)
            .map_err(|err| format!("读取 {} 失败：{err}", destination.display()))?;
        artifacts.push(SealArtifact {
            package: spec.package.to_string(),
            krate: spec.krate.to_string(),
            path: format!("plugins/{}", name.to_string_lossy()),
            sha256: sha256_hex(&bytes),
            bytes: bytes.len() as u64,
        });
    }

    // sidecar manifest + allow-list 必须在 smoke 之前落盘：同版本 inspect 会用
    // `--source-policy allow-list.json` 走真实的准入路径。
    let source_version = workspace_version(root)?;
    let (abi_version, error_format) = abi_constants(root)?;
    write_plugin_provenance(
        &temp,
        &source_version,
        &source_sha256,
        abi_version,
        &artifacts,
    )?;

    fs::create_dir_all(temp.join("smoke")).map_err(|err| format!("创建 smoke 目录失败：{err}"))?;
    fs::write(temp.join(SMOKE_CONFIG), smoke_yaml())
        .map_err(|err| format!("写 smoke 配置失败：{err}"))?;
    let inspect_output = run_load_check(&temp, &cargo_program, &target, &host_root)?;
    fs::write(temp.join(SMOKE_EVIDENCE), &inspect_output)
        .map_err(|err| format!("写 smoke 证据失败：{err}"))?;
    let config_bytes =
        fs::read(temp.join(SMOKE_CONFIG)).map_err(|err| format!("读取 smoke 配置失败：{err}"))?;
    let evidence_bytes =
        fs::read(temp.join(SMOKE_EVIDENCE)).map_err(|err| format!("读取 smoke 证据失败：{err}"))?;

    let (host_source_sha256, host_source_files) = source_tree_digest(&host_root, out)?;
    let manifest = SealManifest {
        workspace_version: source_version,
        abi_version,
        error_format,
        target,
        profile: "release".to_string(),
        created_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0),
        rustc,
        cargo,
        rustflags,
        source_sha256,
        source_files,
        host_source_sha256,
        host_source_files,
        host_workspace_version: workspace_version(&host_root)?,
        artifacts,
        smoke_config: SMOKE_CONFIG.to_string(),
        smoke_config_sha256: sha256_hex(&config_bytes),
        smoke_config_bytes: config_bytes.len() as u64,
        smoke_evidence: SMOKE_EVIDENCE.to_string(),
        smoke_evidence_sha256: sha256_hex(&evidence_bytes),
        smoke_evidence_bytes: evidence_bytes.len() as u64,
        smoke_checked: true,
    };
    write_manifest_files(&temp, &manifest)?;
    // 先按只读路径自检一遍，避免把缺文件的目录当成“封存完成”。
    if let Err(errors) = verify_dir(&temp) {
        return Err(format!("封存自检失败：{}", errors.join("；")));
    }
    if out.exists() {
        fs::remove_dir(out).map_err(|err| format!("移除空目录 {} 失败：{err}", out.display()))?;
    }
    fs::rename(&temp, out).map_err(|err| format!("提交封存目录 {} 失败：{err}", out.display()))?;
    guard.keep = true;
    Ok(manifest)
}

struct TempOut {
    path: PathBuf,
    keep: bool,
}

impl Drop for TempOut {
    fn drop(&mut self) {
        if !self.keep {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

fn build_release_plugins(root: &Path, target: &str) -> Result<BTreeMap<String, PathBuf>, String> {
    let cargo = cargo_program();
    let mut command = Command::new(&cargo);
    command.current_dir(root).args([
        "build",
        "--release",
        "--locked",
        "--target",
        target,
        "--message-format=json-render-diagnostics",
    ]);
    for spec in PLUGINS {
        command.arg("-p").arg(spec.package);
    }
    let output = command
        .output()
        .map_err(|err| format!("启动 {cargo} build 失败：{err}"))?;
    if !output.stderr.is_empty() {
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
    }
    if !output.status.success() {
        return Err(format!(
            "release 插件构建失败（退出码 {:?}）",
            output.status.code()
        ));
    }
    let stdout =
        String::from_utf8(output.stdout).map_err(|err| format!("构建输出不是 UTF-8：{err}"))?;
    parse_build_artifacts(&stdout)
}

fn parse_build_artifacts(stdout: &str) -> Result<BTreeMap<String, PathBuf>, String> {
    let mut found = BTreeMap::new();
    for line in stdout.lines().filter(|line| !line.trim().is_empty()) {
        let value: Value = serde_json::from_str(line)
            .map_err(|err| format!("解析 cargo 构建输出失败：{err}\n{line}"))?;
        if value.get("reason").and_then(Value::as_str) != Some("compiler-artifact") {
            continue;
        }
        let target = value.get("target").ok_or("构建输出缺少 target")?;
        let crate_types = target
            .get("crate_types")
            .and_then(Value::as_array)
            .ok_or("构建输出缺少 target.crate_types")?;
        if !crate_types
            .iter()
            .any(|kind| kind.as_str() == Some("cdylib"))
        {
            continue;
        }
        let name = target
            .get("name")
            .and_then(Value::as_str)
            .ok_or("构建输出缺少 target.name")?;
        let Some(spec) = PLUGINS.iter().find(|spec| spec.krate == name) else {
            continue;
        };
        let candidates = value
            .get("filenames")
            .and_then(Value::as_array)
            .ok_or("构建输出缺少 filenames")?
            .iter()
            .filter_map(Value::as_str)
            .map(PathBuf::from)
            .filter(|path| is_plugin_file(path))
            .collect::<Vec<_>>();
        let source = match candidates.as_slice() {
            [path] => path.clone(),
            [] => {
                return Err(format!(
                    "{} 的构建输出没有当前平台的动态库文件",
                    spec.package
                ));
            }
            _ => {
                return Err(format!(
                    "{} 的构建输出有多个动态库候选：{:?}",
                    spec.package, candidates
                ));
            }
        };
        if !source.is_file() {
            return Err(format!("构建产物不存在：{}", source.display()));
        }
        if found.insert(spec.krate.to_string(), source).is_some() {
            return Err(format!("{} 的构建产物出现了两次", spec.package));
        }
    }
    for spec in PLUGINS {
        if !found.contains_key(spec.krate) {
            return Err(format!("构建结果缺少 {} 的 cdylib", spec.package));
        }
    }
    Ok(found)
}

fn is_plugin_file(path: &Path) -> bool {
    let extension = path.extension().and_then(OsStr::to_str).unwrap_or("");
    if cfg!(windows) {
        extension.eq_ignore_ascii_case("dll")
    } else if cfg!(target_os = "macos") {
        extension == "dylib"
    } else {
        extension == "so"
    }
}

fn smoke_yaml() -> String {
    let mut text =
        String::from("# Generated by `cargo xtask seal`. Read-only probe: inspect never mounts.\n");
    for spec in PLUGINS {
        text.push_str(&format!("- id: {}\n  name: {}\n", spec.label, spec.krate));
    }
    text
}

fn run_load_check(
    seal_dir: &Path,
    cargo: &str,
    target: &str,
    host_root: &Path,
) -> Result<String, String> {
    let config = seal_dir.join(SMOKE_CONFIG);
    let plugin_dir = seal_dir.join("plugins");
    let manifest_path = host_root.join("Cargo.toml");
    let output = Command::new(cargo)
        // CWD 就是封存目录：插件解析先试裸文件名，这样不会命中 workspace 根目录里的同名文件。
        .current_dir(&plugin_dir)
        .args([
            "run",
            "--release",
            "--locked",
            "--target",
            target,
            "--manifest-path",
        ])
        .arg(&manifest_path)
        .args(["--quiet", "-p", "cordis-cli", "--", "inspect"])
        .arg(&config)
        .arg("--plugin-dir")
        .arg(&plugin_dir)
        .arg("--source-policy")
        .arg(seal_dir.join(ALLOW_LIST_FILE))
        .output()
        .map_err(|err| format!("启动同版本加载检查失败：{err}"))?;
    if !output.status.success() {
        return Err(format!(
            "同版本加载检查失败（退出码 {:?}）：\n{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    verify_inspect_output(&stdout)?;
    Ok(stdout)
}

fn verify_inspect_output(stdout: &str) -> Result<(), String> {
    if !stdout.lines().any(|line| line.trim() == "mounted = 0") {
        return Err(format!(
            "同版本加载检查没有报告 mounted = 0，inspect 输出：\n{stdout}"
        ));
    }
    for spec in PLUGINS {
        let expected = format!("- {} (plugin: {})", spec.label, spec.plugin);
        if !stdout.lines().any(|line| line.trim() == expected) {
            return Err(format!(
                "同版本加载检查缺少行 \"{expected}\"（或加载到了别的插件），inspect 输出：\n{stdout}"
            ));
        }
    }
    Ok(())
}

/// `source_revision` 的约定：`tree:<source.sha256>`。
///
/// seal 没有 git commit 字段，源码树聚合 SHA256 是唯一稳定、可重建的 revision
/// 标识；它包含未跟踪文件，因此不等同于提交号。格式见 `docs/release-seal.md`。
fn source_revision(source_sha256: &str) -> String {
    format!("tree:{}", source_sha256.to_ascii_lowercase())
}

fn plugin_spec_for(artifact: &SealArtifact) -> Option<&'static PluginSpec> {
    PLUGINS
        .iter()
        .find(|spec| spec.package == artifact.package && spec.krate == artifact.krate)
}

/// seal 写身份字段，并从 [`PluginSpec`] 写入示例插件的 semver 依赖声明。
///
/// 这些声明只用于 loader 的 mount 前预检，不是 ABI 能力或授权；`--verify` 仍要求
/// sidecar 与这里写出的规范字节一致，所以不要手改已封存目录。
fn plugin_manifest_value(
    artifact: &SealArtifact,
    version: &str,
    source_revision: &str,
    abi_version: u32,
) -> Result<Value, String> {
    let spec = plugin_spec_for(artifact).ok_or_else(|| {
        format!(
            "artifact 未登记：package={} crate={}",
            artifact.package, artifact.krate
        )
    })?;
    let provides = spec
        .provides
        .iter()
        .map(|(key, value)| ((*key).to_string(), json!(value)))
        .collect::<serde_json::Map<_, _>>();
    let requires = spec
        .requires
        .iter()
        .map(|(key, value)| ((*key).to_string(), json!(value)))
        .collect::<serde_json::Map<_, _>>();
    Ok(json!({
        "id": spec.plugin,
        "version": version,
        "source_revision": source_revision,
        "abi_version": abi_version,
        "sha256": artifact.sha256.to_ascii_lowercase(),
        "provides": provides,
        "requires": requires,
    }))
}

fn allow_list_value(
    version: &str,
    source_revision: &str,
    abi_version: u32,
    artifacts: &[SealArtifact],
) -> Result<Value, String> {
    let mut allowed = Vec::new();
    for spec in PLUGINS {
        let artifact = artifacts
            .iter()
            .find(|artifact| artifact.package == spec.package && artifact.krate == spec.krate)
            .ok_or_else(|| format!("缺少 artifact：{} / {}", spec.package, spec.krate))?;
        allowed.push(plugin_manifest_value(
            artifact,
            version,
            source_revision,
            abi_version,
        )?);
    }
    Ok(json!({ "allowed": allowed }))
}

fn json_file_bytes(value: &Value) -> Result<Vec<u8>, String> {
    let text = serde_json::to_string_pretty(value)
        .map_err(|err| format!("序列化封存 provenance 失败：{err}"))?;
    Ok(format!("{text}\n").into_bytes())
}

fn sidecar_path(artifact: &SealArtifact) -> String {
    format!("{}.manifest.json", artifact.path)
}

/// 写每个 artifact 的 sidecar manifest 与封存根的 `allow-list.json`，返回
/// `相对路径 -> SHA256`（供 `SHA256SUMS` 与 `--verify` 使用）。
fn write_plugin_provenance(
    dir: &Path,
    version: &str,
    source_sha256: &str,
    abi_version: u32,
    artifacts: &[SealArtifact],
) -> Result<BTreeMap<String, String>, String> {
    let revision = source_revision(source_sha256);
    let mut written = BTreeMap::new();
    for artifact in artifacts {
        let relative = sidecar_path(artifact);
        // 防御恶意/损坏的清单路径：宁可让 `--verify` 报「路径不安全」，也不往
        // 封存目录外写文件（`verify_rejects_path_traversal` 覆盖这条）。
        if !safe_relative_path(&relative) || !relative.starts_with("plugins/") {
            continue;
        }
        // 不一致的清单（缺 artifact / 未登记 artifact）由 `verify_dir` 报告；
        // 写侧只负责产出可生成的那部分，避免测试夹具在自检前就 panic。
        let Ok(value) = plugin_manifest_value(artifact, version, &revision, abi_version) else {
            continue;
        };
        let bytes = json_file_bytes(&value)?;
        fs::write(dir.join(&relative), &bytes)
            .map_err(|err| format!("写插件 manifest {relative} 失败：{err}"))?;
        written.insert(relative, sha256_hex(&bytes));
    }
    if let Ok(value) = allow_list_value(version, &revision, abi_version, artifacts) {
        let bytes = json_file_bytes(&value)?;
        fs::write(dir.join(ALLOW_LIST_FILE), &bytes)
            .map_err(|err| format!("写 {ALLOW_LIST_FILE} 失败：{err}"))?;
        written.insert(ALLOW_LIST_FILE.to_string(), sha256_hex(&bytes));
    }
    Ok(written)
}

fn write_manifest_files(dir: &Path, manifest: &SealManifest) -> Result<(), String> {
    let provenance_sums = write_plugin_provenance(
        dir,
        &manifest.workspace_version,
        &manifest.source_sha256,
        manifest.abi_version,
        &manifest.artifacts,
    )?;
    let text = serde_json::to_string_pretty(&manifest.to_json())
        .map_err(|err| format!("序列化封存清单失败：{err}"))?;
    fs::write(dir.join(SEAL_FILE), format!("{text}\n"))
        .map_err(|err| format!("写 {SEAL_FILE} 失败：{err}"))?;
    let mut sums = String::new();
    for artifact in &manifest.artifacts {
        sums.push_str(&format!("{}  {}\n", artifact.sha256, artifact.path));
        if let Some(hash) = provenance_sums.get(&sidecar_path(artifact)) {
            sums.push_str(&format!("{hash}  {}\n", sidecar_path(artifact)));
        }
    }
    if let Some(hash) = provenance_sums.get(ALLOW_LIST_FILE) {
        sums.push_str(&format!("{hash}  {ALLOW_LIST_FILE}\n"));
    }
    sums.push_str(&format!(
        "{}  {}\n",
        manifest.smoke_config_sha256, manifest.smoke_config
    ));
    sums.push_str(&format!(
        "{}  {}\n",
        manifest.smoke_evidence_sha256, manifest.smoke_evidence
    ));
    fs::write(dir.join(SUMS_FILE), sums).map_err(|err| format!("写 {SUMS_FILE} 失败：{err}"))
}

pub(crate) fn verify_for_consume(dir: &Path) -> Result<(), Vec<String>> {
    verify_dir(dir).map(|_| ())
}

fn verify_dir(dir: &Path) -> Result<SealManifest, Vec<String>> {
    let mut errors = Vec::new();
    let text = match fs::read_to_string(dir.join(SEAL_FILE)) {
        Ok(text) => text,
        Err(err) => return Err(vec![format!("读取 {SEAL_FILE} 失败：{err}")]),
    };
    let value: Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(err) => return Err(vec![format!("解析 {SEAL_FILE} 失败：{err}")]),
    };
    let manifest = match SealManifest::from_json(&value) {
        Ok(manifest) => manifest,
        Err(err) => return Err(vec![err]),
    };
    // `/1` 是加入 provenance 文件之前的历史封存：保留只读复验能力，不要求
    // sidecar / allow-list。新 `/2` 封存必须完整。
    let legacy = value.get("format").and_then(Value::as_str) == Some(SEAL_FORMAT_V1);

    if value.pointer("/source/kind").and_then(Value::as_str) != Some("workspace-tree") {
        errors.push("source.kind 必须是 workspace-tree".to_string());
    }
    if manifest.workspace_version.trim().is_empty() {
        errors.push("workspace_version 为空".to_string());
    }
    if !legacy {
        if manifest.host_workspace_version.trim().is_empty() {
            errors.push("host.workspace_version 为空".to_string());
        }
        if manifest.host_source_sha256.len() != 64 {
            errors.push("host.source_sha256 必须是 64 位十六进制".to_string());
        }
        if manifest.host_source_files == 0 {
            errors.push("host.source_files 必须大于 0".to_string());
        }
    }
    if manifest.abi_version == 0 {
        errors.push("abi_version 必须大于 0".to_string());
    }
    if manifest.error_format == 0 {
        errors.push("error_format 必须大于 0".to_string());
    }
    if manifest.target.trim().is_empty() {
        errors.push("target 不能为空".to_string());
    }
    if manifest.profile != "release" {
        errors.push(format!("profile 必须是 release，实际 {}", manifest.profile));
    }
    if manifest.rustc.trim().is_empty() || manifest.cargo.trim().is_empty() {
        errors.push("rustc / cargo 版本不能为空".to_string());
    }
    if !is_sha256(&manifest.source_sha256)
        || manifest.source_sha256.bytes().all(|byte| byte == b'0')
    {
        errors.push("source.sha256 不是有效的非零 64 位十六进制".to_string());
    }
    if manifest.source_files == 0 {
        errors.push("source.files 必须大于 0".to_string());
    }
    if !manifest.smoke_checked {
        errors.push("smoke.checked 必须为 true".to_string());
    }
    verify_expected_artifacts(&manifest, &mut errors);
    if !is_sha256(&manifest.smoke_config_sha256) || manifest.smoke_config_bytes == 0 {
        errors.push("smoke.config 的 SHA256 / 大小非法".to_string());
    }
    if !is_sha256(&manifest.smoke_evidence_sha256) || manifest.smoke_evidence_bytes == 0 {
        errors.push("smoke.evidence 的 SHA256 / 大小非法".to_string());
    }
    verify_sealed_file(
        dir,
        "smoke.config",
        &manifest.smoke_config,
        &manifest.smoke_config_sha256,
        Some(manifest.smoke_config_bytes),
        &mut errors,
    );
    verify_sealed_file(
        dir,
        "smoke.evidence",
        &manifest.smoke_evidence,
        &manifest.smoke_evidence_sha256,
        Some(manifest.smoke_evidence_bytes),
        &mut errors,
    );

    let mut seen = BTreeSet::new();
    let mut expected_sums = BTreeMap::new();
    for artifact in &manifest.artifacts {
        if artifact.package.trim().is_empty() || artifact.krate.trim().is_empty() {
            errors.push("artifact 的 package / crate 不能为空".to_string());
        }
        if !is_sha256(&artifact.sha256) {
            errors.push(format!("{} 的 SHA256 不是 64 位十六进制", artifact.path));
        }
        if !safe_relative_path(&artifact.path) {
            errors.push(format!("artifact 路径不安全：{}", artifact.path));
            continue;
        }
        if !artifact.path.starts_with("plugins/") {
            errors.push(format!("artifact 必须位于 plugins/ 下：{}", artifact.path));
            continue;
        }
        if !seen.insert(artifact.path.clone()) {
            errors.push(format!("artifact 路径重复：{}", artifact.path));
            continue;
        }
        expected_sums.insert(artifact.path.clone(), artifact.sha256.clone());
        let path = dir.join(&artifact.path);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(err) => {
                errors.push(format!("读取 artifact {} 失败：{err}", artifact.path));
                continue;
            }
        };
        if metadata.file_type().is_symlink() {
            errors.push(format!("artifact 不能是符号链接：{}", artifact.path));
            continue;
        }
        if !metadata.is_file() {
            errors.push(format!("artifact 不是普通文件：{}", artifact.path));
            continue;
        }
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) => {
                errors.push(format!("读取 artifact {} 失败：{err}", artifact.path));
                continue;
            }
        };
        if bytes.len() as u64 != artifact.bytes {
            errors.push(format!(
                "artifact {} 大小不匹配：清单 {}，实际 {}",
                artifact.path,
                artifact.bytes,
                bytes.len()
            ));
        }
        let actual = sha256_hex(&bytes);
        if actual != artifact.sha256.to_ascii_lowercase() {
            errors.push(format!(
                "artifact {} 的 SHA256 不匹配：清单 {}，实际 {actual}",
                artifact.path, artifact.sha256
            ));
        }
    }
    if !legacy {
        verify_plugin_provenance(dir, &manifest, &mut seen, &mut expected_sums, &mut errors);
    }
    if safe_relative_path(&manifest.smoke_config) && manifest.smoke_config.starts_with("smoke/") {
        expected_sums.insert(
            manifest.smoke_config.clone(),
            manifest.smoke_config_sha256.clone(),
        );
    }
    if safe_relative_path(&manifest.smoke_evidence) && manifest.smoke_evidence.starts_with("smoke/")
    {
        expected_sums.insert(
            manifest.smoke_evidence.clone(),
            manifest.smoke_evidence_sha256.clone(),
        );
    }
    verify_exact_files(dir, "plugins", &seen, &mut errors);
    verify_exact_files(
        dir,
        "smoke",
        &BTreeSet::from([
            manifest.smoke_config.clone(),
            manifest.smoke_evidence.clone(),
        ]),
        &mut errors,
    );
    verify_sums(dir, &expected_sums, &mut errors);
    if !legacy {
        verify_root_files(
            dir,
            &BTreeSet::from([
                SEAL_FILE.to_string(),
                SUMS_FILE.to_string(),
                ALLOW_LIST_FILE.to_string(),
            ]),
            &mut errors,
        );
    }

    if errors.is_empty() {
        Ok(manifest)
    } else {
        Err(errors)
    }
}

/// 校验每个 artifact 的 sidecar manifest 与封存根的 `allow-list.json`：
/// 内容必须是本次 seal 生成器会写出的确定字节（id / version / revision /
/// abi_version / sha256 全部匹配），同时把文件纳入 `SHA256SUMS` 的期望集合。
fn verify_plugin_provenance(
    dir: &Path,
    manifest: &SealManifest,
    seen: &mut BTreeSet<String>,
    expected_sums: &mut BTreeMap<String, String>,
    errors: &mut Vec<String>,
) {
    let revision = source_revision(&manifest.source_sha256);
    for artifact in &manifest.artifacts {
        let relative = sidecar_path(artifact);
        if !safe_relative_path(&relative) || !relative.starts_with("plugins/") {
            errors.push(format!(
                "插件 manifest 路径不安全或不在 plugins/ 下：{relative}"
            ));
            continue;
        }
        if !seen.insert(relative.clone()) {
            errors.push(format!("插件 manifest 路径重复：{relative}"));
            continue;
        }
        let expected = match plugin_manifest_value(
            artifact,
            &manifest.workspace_version,
            &revision,
            manifest.abi_version,
        ) {
            Ok(value) => value,
            Err(err) => {
                errors.push(err);
                continue;
            }
        };
        let bytes = match json_file_bytes(&expected) {
            Ok(bytes) => bytes,
            Err(err) => {
                errors.push(err);
                continue;
            }
        };
        let legacy = strip_dependency_fields(expected.clone());
        let legacy_bytes = match json_file_bytes(&legacy) {
            Ok(bytes) => bytes,
            Err(err) => {
                errors.push(err);
                continue;
            }
        };
        if let Some(actual_hash) = accept_legacy_provenance(dir, &relative, &bytes, &legacy_bytes) {
            expected_sums.insert(relative.clone(), actual_hash);
            continue;
        }
        expected_sums.insert(relative.clone(), sha256_hex(&bytes));
        verify_expected_file(dir, "插件 manifest", &relative, &bytes, errors);
    }

    let expected = match allow_list_value(
        &manifest.workspace_version,
        &revision,
        manifest.abi_version,
        &manifest.artifacts,
    ) {
        Ok(value) => value,
        Err(err) => {
            errors.push(err);
            return;
        }
    };
    let bytes = match json_file_bytes(&expected) {
        Ok(bytes) => bytes,
        Err(err) => {
            errors.push(err);
            return;
        }
    };
    let mut legacy = expected.clone();
    if let Some(allowed) = legacy.get_mut("allowed").and_then(Value::as_array_mut) {
        for entry in allowed {
            strip_dependency_fields_in_place(entry);
        }
    }
    let legacy_bytes = match json_file_bytes(&legacy) {
        Ok(bytes) => bytes,
        Err(err) => {
            errors.push(err);
            return;
        }
    };
    if let Some(actual_hash) = accept_legacy_provenance(dir, ALLOW_LIST_FILE, &bytes, &legacy_bytes)
    {
        expected_sums.insert(ALLOW_LIST_FILE.to_string(), actual_hash);
        return;
    }
    expected_sums.insert(ALLOW_LIST_FILE.to_string(), sha256_hex(&bytes));
    verify_expected_file(dir, "来源 allow-list", ALLOW_LIST_FILE, &bytes, errors);
}

fn strip_dependency_fields_in_place(value: &mut Value) {
    if let Some(object) = value.as_object_mut() {
        object.remove("provides");
        object.remove("requires");
    }
}

fn strip_dependency_fields(mut value: Value) -> Value {
    strip_dependency_fields_in_place(&mut value);
    value
}

/// 兼容 26f108b 之前写出的 `/2`：sidecar/allow-list 没有 provides/requires。
/// 只有实际字节等于旧规范且不是符号链接时才接受，并把实际哈希写回 SHA 期望。
fn accept_legacy_provenance(
    dir: &Path,
    relative: &str,
    expected: &[u8],
    legacy: &[u8],
) -> Option<String> {
    let path = dir.join(relative);
    let metadata = fs::symlink_metadata(&path).ok()?;
    if metadata.file_type().is_symlink() {
        return None;
    }
    let actual = fs::read(&path).ok()?;
    if actual == expected || actual == legacy {
        Some(sha256_hex(&actual))
    } else {
        None
    }
}

fn verify_expected_file(
    dir: &Path,
    label: &str,
    relative: &str,
    expected: &[u8],
    errors: &mut Vec<String>,
) {
    let path = dir.join(relative);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(err) => {
            errors.push(format!("读取 {label} {relative} 失败：{err}"));
            return;
        }
    };
    if metadata.file_type().is_symlink() {
        errors.push(format!("{label} 不能是符号链接：{relative}"));
        return;
    }
    if !metadata.is_file() {
        errors.push(format!("{label} 不是普通文件：{relative}"));
        return;
    }
    let content = match fs::read(&path) {
        Ok(content) => content,
        Err(err) => {
            errors.push(format!("读取 {label} {relative} 失败：{err}"));
            return;
        }
    };
    if content != expected {
        errors.push(format!(
            "{label} {relative} 的内容与预期不一致：预期 {} 字节 / SHA256 {}，实际 {} 字节 / SHA256 {}",
            expected.len(),
            sha256_hex(expected),
            content.len(),
            sha256_hex(&content)
        ));
    }
}

fn verify_root_files(seal_dir: &Path, expected: &BTreeSet<String>, errors: &mut Vec<String>) {
    let entries = match fs::read_dir(seal_dir) {
        Ok(entries) => entries,
        Err(err) => {
            errors.push(format!("读取封存根目录失败：{err}"));
            return;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                errors.push(format!("读取封存根目录项失败：{err}"));
                continue;
            }
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(err) => {
                errors.push(format!("读取封存根目录项 {name} 类型失败：{err}"));
                continue;
            }
        };
        if file_type.is_dir() {
            if name != "plugins" && name != "smoke" {
                errors.push(format!("封存根目录多出未登记目录：{name}"));
            }
        } else if !expected.contains(&name) {
            errors.push(format!("封存根目录多出未登记文件：{name}"));
        }
    }
}

fn verify_expected_artifacts(manifest: &SealManifest, errors: &mut Vec<String>) {
    if manifest.artifacts.len() != PLUGINS.len() {
        errors.push(format!(
            "artifact 数量必须是 {}，实际 {}",
            PLUGINS.len(),
            manifest.artifacts.len()
        ));
    }
    let mut by_crate = BTreeMap::new();
    for artifact in &manifest.artifacts {
        if by_crate.insert(artifact.krate.as_str(), artifact).is_some() {
            errors.push(format!("artifact crate 重复：{}", artifact.krate));
        }
        if !PLUGINS
            .iter()
            .any(|spec| spec.krate == artifact.krate && spec.package == artifact.package)
        {
            errors.push(format!(
                "artifact 未登记：package={} crate={}",
                artifact.package, artifact.krate
            ));
        }
    }
    for spec in PLUGINS {
        match by_crate.get(spec.krate) {
            Some(artifact) if artifact.package == spec.package => {}
            Some(artifact) => errors.push(format!(
                "artifact {} 的 package 应为 {}，实际 {}",
                spec.krate, spec.package, artifact.package
            )),
            None => errors.push(format!("缺少 artifact：{} / {}", spec.package, spec.krate)),
        }
    }
}

fn verify_sealed_file(
    dir: &Path,
    label: &str,
    relative: &str,
    sha256: &str,
    bytes: Option<u64>,
    errors: &mut Vec<String>,
) {
    if !safe_relative_path(relative) || !relative.starts_with("smoke/") {
        errors.push(format!("{label} 路径不安全或不在 smoke/ 下：{relative}"));
        return;
    }
    let path = dir.join(relative);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(err) => {
            errors.push(format!("读取 {label} {relative} 失败：{err}"));
            return;
        }
    };
    if metadata.file_type().is_symlink() {
        errors.push(format!("{label} 不能是符号链接：{relative}"));
        return;
    }
    if !metadata.is_file() {
        errors.push(format!("{label} 不是普通文件：{relative}"));
        return;
    }
    let content = match fs::read(&path) {
        Ok(content) => content,
        Err(err) => {
            errors.push(format!("读取 {label} {relative} 失败：{err}"));
            return;
        }
    };
    if let Some(expected) = bytes {
        if content.len() as u64 != expected {
            errors.push(format!(
                "{label} {relative} 大小不匹配：清单 {expected}，实际 {}",
                content.len()
            ));
        }
    }
    let actual = sha256_hex(&content);
    if actual != sha256.to_ascii_lowercase() {
        errors.push(format!(
            "{label} {relative} 的 SHA256 不匹配：清单 {sha256}，实际 {actual}"
        ));
    }
}

fn verify_exact_files(
    seal_dir: &Path,
    dir_name: &str,
    expected: &BTreeSet<String>,
    errors: &mut Vec<String>,
) {
    let root = seal_dir.join(dir_name);
    let metadata = match fs::symlink_metadata(&root) {
        Ok(metadata) => metadata,
        Err(err) => {
            errors.push(format!("读取 {dir_name}/ 失败：{err}"));
            return;
        }
    };
    if metadata.file_type().is_symlink() {
        errors.push(format!("{dir_name}/ 不能是符号链接"));
        return;
    }
    if !metadata.is_dir() {
        errors.push(format!("{dir_name}/ 不是目录"));
        return;
    }
    let mut actual = BTreeSet::new();
    if let Err(err) = collect_relative_files(seal_dir, &root, &mut actual) {
        errors.push(err);
        return;
    }
    for extra in actual.difference(expected) {
        errors.push(format!("{dir_name}/ 多出未登记文件：{extra}"));
    }
}

fn collect_relative_files(
    seal_dir: &Path,
    dir: &Path,
    out: &mut BTreeSet<String>,
) -> Result<(), String> {
    let entries = fs::read_dir(dir).map_err(|err| format!("读取 {} 失败：{err}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|err| format!("读取目录项失败：{err}"))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|err| format!("读取 {} 类型失败：{err}", path.display()))?;
        if file_type.is_dir() {
            collect_relative_files(seal_dir, &path, out)?;
        } else {
            let relative = path
                .strip_prefix(seal_dir)
                .map_err(|err| format!("计算相对路径失败：{err}"))?;
            out.insert(relative.to_string_lossy().replace('\\', "/"));
        }
    }
    Ok(())
}

fn verify_sums(dir: &Path, expected: &BTreeMap<String, String>, errors: &mut Vec<String>) {
    let text = match fs::read_to_string(dir.join(SUMS_FILE)) {
        Ok(text) => text,
        Err(err) => {
            errors.push(format!("读取 {SUMS_FILE} 失败：{err}"));
            return;
        }
    };
    let mut actual = BTreeMap::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let Some((hash, path)) = line.split_once("  ") else {
            errors.push(format!("{SUMS_FILE} 行格式错误：{line}"));
            continue;
        };
        if !is_sha256(hash) {
            errors.push(format!("{SUMS_FILE} 中的 SHA256 非法：{hash}"));
        }
        if !safe_relative_path(path) {
            errors.push(format!("{SUMS_FILE} 中的路径不安全：{path}"));
            continue;
        }
        if actual.insert(path.to_string(), hash.to_string()).is_some() {
            errors.push(format!("{SUMS_FILE} 中的路径重复：{path}"));
        }
    }
    for (path, hash) in expected {
        match actual.get(path) {
            Some(actual_hash) if actual_hash.eq_ignore_ascii_case(hash) => {}
            Some(actual_hash) => errors.push(format!(
                "{SUMS_FILE} 中 {path} 的 SHA256 不匹配：清单 {hash}，校验文件 {actual_hash}"
            )),
            None => errors.push(format!("{SUMS_FILE} 缺少 {path}")),
        }
    }
    for path in actual.keys() {
        if !expected.contains_key(path) {
            errors.push(format!("{SUMS_FILE} 多出未登记的 {path}"));
        }
    }
}

fn safe_relative_path(text: &str) -> bool {
    let path = Path::new(text);
    !text.is_empty()
        && !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn is_sha256(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn host_target(root: &Path) -> Result<String, String> {
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    let output = Command::new(&rustc)
        .arg("-vV")
        .current_dir(root)
        .output()
        .map_err(|err| format!("运行 {rustc} -vV 失败：{err}"))?;
    if !output.status.success() {
        return Err(format!("{rustc} -vV 退出码 {:?}", output.status.code()));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .find_map(|line| line.strip_prefix("host: "))
        .map(|host| host.trim().to_string())
        .ok_or_else(|| format!("{rustc} -vV 没有 host 行"))
}

/// 在目标源码树里执行版本探测：`rust-toolchain.toml`/`.cargo/config` 的
/// 目录语义与真正构建保持一致。
fn tool_version_in(program: &str, root: &Path) -> Result<String, String> {
    let output = Command::new(program)
        .arg("--version")
        .current_dir(root)
        .output()
        .map_err(|err| format!("运行 {program} --version 失败：{err}"))?;
    if !output.status.success() {
        return Err(format!(
            "{program} --version 退出码 {:?}",
            output.status.code()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn cargo_program() -> String {
    std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string())
}

fn temp_nonce() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}

fn source_tree_digest(root: &Path, excluded: &Path) -> Result<(String, u64), String> {
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .map(|path| {
            if path.is_absolute() {
                path
            } else {
                root.join(path)
            }
        })
        .unwrap_or_else(|| root.join("target"));
    let mut files = Vec::new();
    collect_source_files(root, root, excluded, &target, &mut files)?;
    files.sort_by(|left, right| left.0.cmp(&right.0));
    let mut canonical = Vec::new();
    canonical.extend_from_slice(b"cordis-source-tree-v1\0");
    for (relative, path) in &files {
        let bytes =
            fs::read(path).map_err(|err| format!("读取源码 {} 失败：{err}", path.display()))?;
        canonical.extend_from_slice(&(relative.len() as u64).to_le_bytes());
        canonical.extend_from_slice(relative.as_bytes());
        canonical.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
        canonical.extend_from_slice(&bytes);
    }
    Ok((sha256_hex(&canonical), files.len() as u64))
}

fn collect_source_files(
    root: &Path,
    dir: &Path,
    excluded: &Path,
    target: &Path,
    out: &mut Vec<(String, PathBuf)>,
) -> Result<(), String> {
    let entries =
        fs::read_dir(dir).map_err(|err| format!("读取目录 {} 失败：{err}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|err| format!("读取目录项失败：{err}"))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|err| format!("读取 {} 类型失败：{err}", path.display()))?;
        if file_type.is_dir() {
            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            if path == excluded
                || path == target
                || matches!(
                    name.as_str(),
                    ".git" | "target" | "dist" | ".cargo-home" | "node_modules"
                )
                || name.contains(".tmp-")
            {
                continue;
            }
            collect_source_files(root, &path, excluded, target, out)?;
        } else if file_type.is_file() && path != excluded && path != target {
            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            if name == ".git" || name == ".ds_store" || name.ends_with(".swp") {
                continue;
            }
            let relative = path
                .strip_prefix(root)
                .map_err(|err| format!("计算相对路径失败：{err}"))?;
            out.push((
                relative.to_string_lossy().replace('\\', "/"),
                path.to_path_buf(),
            ));
        }
    }
    Ok(())
}

/// 无依赖 SHA-256：封存清单必须能在没有新增哈希依赖的机器上独立校验。
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

#[cfg(test)]
mod tests {
    use super::*;

    struct TempTree {
        path: PathBuf,
    }

    impl TempTree {
        fn new(label: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("cordis-seal-{}-{label}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self { path }
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn expected_inspect() -> String {
        let mut text = String::from("mounted = 0\n");
        for spec in PLUGINS {
            text.push_str(&format!("- {} (plugin: {})\n", spec.label, spec.plugin));
        }
        text
    }

    fn fixture(dir: &Path) -> SealManifest {
        let payload = b"sealed plugin bytes";
        fs::create_dir_all(dir.join("plugins")).unwrap();
        fs::create_dir_all(dir.join("smoke")).unwrap();
        let mut artifacts = Vec::new();
        for (spec, name) in PLUGINS.iter().zip([
            "plugins/greeting.bin",
            "plugins/hello.bin",
            "plugins/harness_tool.bin",
        ]) {
            fs::write(dir.join(name), payload).unwrap();
            artifacts.push(SealArtifact {
                package: spec.package.to_string(),
                krate: spec.krate.to_string(),
                path: name.to_string(),
                sha256: sha256_hex(payload),
                bytes: payload.len() as u64,
            });
        }
        let config = smoke_yaml();
        let evidence = expected_inspect();
        fs::write(dir.join(SMOKE_CONFIG), &config).unwrap();
        fs::write(dir.join(SMOKE_EVIDENCE), &evidence).unwrap();
        let manifest = SealManifest {
            workspace_version: "0.1.0".to_string(),
            abi_version: 1,
            error_format: 1,
            target: "test-target".to_string(),
            profile: "release".to_string(),
            created_unix: 0,
            rustc: "rustc test".to_string(),
            cargo: "cargo test".to_string(),
            rustflags: String::new(),
            source_sha256: sha256_hex(b"source"),
            source_files: 3,
            host_source_sha256: sha256_hex(b"host"),
            host_source_files: 2,
            host_workspace_version: "0.1.0".to_string(),
            artifacts,
            smoke_config: SMOKE_CONFIG.to_string(),
            smoke_config_sha256: sha256_hex(config.as_bytes()),
            smoke_config_bytes: config.len() as u64,
            smoke_evidence: SMOKE_EVIDENCE.to_string(),
            smoke_evidence_sha256: sha256_hex(evidence.as_bytes()),
            smoke_evidence_bytes: evidence.len() as u64,
            smoke_checked: true,
        };
        write_manifest_files(dir, &manifest).unwrap();
        manifest
    }

    #[test]
    fn sha256_matches_known_vectors_including_multi_block_inputs() {
        let block = [b'a'; 64];
        let million = vec![b'a'; 1_000_000];
        let vectors: &[(&[u8], &str)] = &[
            (
                b"",
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            ),
            (
                b"abc",
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            ),
            (
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq",
                "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1",
            ),
            (
                b"abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmnhijklmnoijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu",
                "cf5b16a778af8380036ce59e7b0492370b249b11e8f07a51afac45037afee9d1",
            ),
            (
                block.as_slice(),
                "ffe054fe7ae0cb6dc65c3af9b61d5209f439851db43d0ba5997337df154668eb",
            ),
            (
                million.as_slice(),
                "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0",
            ),
        ];
        for (input, expected) in vectors {
            assert_eq!(&sha256_hex(input), expected);
        }
    }

    #[test]
    fn verify_accepts_a_matching_seal() {
        let tree = TempTree::new("verify-ok");
        fixture(&tree.path);
        verify_dir(&tree.path).expect("完整封存应通过");
    }

    #[test]
    fn verify_still_accepts_a_legacy_v1_seal_without_provenance_files() {
        let tree = TempTree::new("verify-legacy-v1");
        let manifest = fixture(&tree.path);
        let mut value: Value =
            serde_json::from_str(&fs::read_to_string(tree.path.join(SEAL_FILE)).unwrap()).unwrap();
        value["format"] = SEAL_FORMAT_V1.into();
        fs::write(
            tree.path.join(SEAL_FILE),
            format!("{}\n", serde_json::to_string_pretty(&value).unwrap()),
        )
        .unwrap();
        for artifact in &manifest.artifacts {
            fs::remove_file(tree.path.join(sidecar_path(artifact))).unwrap();
        }
        fs::remove_file(tree.path.join(ALLOW_LIST_FILE)).unwrap();
        let sums = fs::read_to_string(tree.path.join(SUMS_FILE))
            .unwrap()
            .lines()
            .filter(|line| !line.contains(".manifest.json") && !line.ends_with("allow-list.json"))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(tree.path.join(SUMS_FILE), format!("{sums}\n")).unwrap();

        verify_dir(&tree.path).expect("历史 /1 封存必须继续可复验");
    }

    #[test]
    fn writes_sidecar_manifests_and_allow_list_that_match_the_artifacts() {
        let tree = TempTree::new("provenance-files");
        let manifest = fixture(&tree.path);

        for artifact in &manifest.artifacts {
            let spec = plugin_spec_for(artifact).expect("artifact 必须对应一个插件规格");
            let text = fs::read_to_string(tree.path.join(sidecar_path(artifact)))
                .expect("sidecar manifest 必须存在");
            let value: Value = serde_json::from_str(&text).expect("sidecar 必须是 JSON");
            assert_eq!(value["id"], spec.plugin);
            assert_eq!(value["version"], manifest.workspace_version);
            assert_eq!(
                value["source_revision"],
                source_revision(&manifest.source_sha256)
            );
            assert_eq!(value["abi_version"], manifest.abi_version);
            assert_eq!(value["sha256"], artifact.sha256);
        }

        let allow: Value = serde_json::from_str(
            &fs::read_to_string(tree.path.join(ALLOW_LIST_FILE)).expect("allow-list 必须存在"),
        )
        .expect("allow-list 必须是 JSON");
        let allowed = allow["allowed"].as_array().expect("allowed 必须是数组");
        assert_eq!(allowed.len(), PLUGINS.len());
        for spec in PLUGINS {
            assert!(allowed.iter().any(|entry| entry["id"] == spec.plugin));
        }
    }

    #[test]
    fn verify_rejects_a_tampered_sidecar_manifest() {
        let tree = TempTree::new("verify-manifest");
        let manifest = fixture(&tree.path);
        let sidecar = tree.path.join(sidecar_path(&manifest.artifacts[0]));
        let mut value: Value =
            serde_json::from_str(&fs::read_to_string(&sidecar).unwrap()).unwrap();
        value["source_revision"] = "tree:tampered".into();
        fs::write(&sidecar, value.to_string()).unwrap();

        let errors = verify_dir(&tree.path).expect_err("篡改 sidecar 必须失败");
        assert!(
            errors.iter().any(|error| error.contains("manifest")),
            "{errors:?}"
        );
    }

    #[test]
    fn verify_rejects_a_missing_sidecar_manifest() {
        let tree = TempTree::new("verify-missing-manifest");
        let manifest = fixture(&tree.path);
        fs::remove_file(tree.path.join(sidecar_path(&manifest.artifacts[0]))).unwrap();

        let errors = verify_dir(&tree.path).expect_err("缺 sidecar 必须失败");
        assert!(
            errors.iter().any(|error| error.contains("manifest")),
            "{errors:?}"
        );
    }

    #[test]
    fn verify_rejects_a_tampered_allow_list() {
        let tree = TempTree::new("verify-allow-list");
        fixture(&tree.path);
        fs::write(tree.path.join(ALLOW_LIST_FILE), "{\"allowed\":[]}\n").unwrap();

        let errors = verify_dir(&tree.path).expect_err("篡改 allow-list 必须失败");
        assert!(
            errors.iter().any(|error| error.contains("allow-list")),
            "{errors:?}"
        );
    }

    #[test]
    fn verify_rejects_tampered_artifact_bytes() {
        let tree = TempTree::new("verify-tamper");
        fixture(&tree.path);
        fs::write(tree.path.join("plugins/greeting.bin"), b"tampered").unwrap();
        let errors = verify_dir(&tree.path).expect_err("篡改必须失败");
        assert!(errors.iter().any(|error| error.contains("SHA256")));
    }

    #[test]
    fn verify_rejects_same_length_artifact_tamper() {
        let tree = TempTree::new("verify-tamper-same-length");
        fixture(&tree.path);
        let mut bytes = fs::read(tree.path.join("plugins/greeting.bin")).unwrap();
        bytes[0] ^= 1;
        fs::write(tree.path.join("plugins/greeting.bin"), bytes).unwrap();
        let errors = verify_dir(&tree.path).expect_err("同长度篡改必须失败");
        assert!(errors.iter().any(|error| error.contains("SHA256")));
    }

    #[test]
    fn verify_rejects_path_traversal() {
        let tree = TempTree::new("verify-traversal");
        let mut manifest = fixture(&tree.path);
        manifest.artifacts[0].path = "../outside.bin".to_string();
        write_manifest_files(&tree.path, &manifest).unwrap();
        let errors = verify_dir(&tree.path).expect_err("路径穿越必须失败");
        assert!(errors.iter().any(|error| error.contains("路径不安全")));
    }

    #[test]
    fn verify_rejects_a_missing_checksum_entry() {
        let tree = TempTree::new("verify-sums");
        fixture(&tree.path);
        fs::write(tree.path.join(SUMS_FILE), "").unwrap();
        let errors = verify_dir(&tree.path).expect_err("缺少校验行必须失败");
        assert!(errors
            .iter()
            .any(|error| error.contains("缺少 plugins/greeting.bin")));
    }

    #[test]
    fn verify_rejects_a_missing_expected_artifact() {
        let tree = TempTree::new("verify-missing-artifact");
        let mut manifest = fixture(&tree.path);
        manifest.artifacts.pop();
        write_manifest_files(&tree.path, &manifest).unwrap();
        let errors = verify_dir(&tree.path).expect_err("缺一个产物必须失败");
        assert!(errors.iter().any(|error| error.contains("artifact 数量")));
    }

    #[test]
    fn verify_rejects_an_extra_plugin_file() {
        let tree = TempTree::new("verify-extra-file");
        fixture(&tree.path);
        fs::write(tree.path.join("plugins/stray.bin"), b"stray").unwrap();
        let errors = verify_dir(&tree.path).expect_err("多出文件必须失败");
        assert!(errors.iter().any(|error| error.contains("多出未登记文件")));
    }

    #[test]
    fn verify_rejects_an_extra_root_file() {
        let tree = TempTree::new("verify-extra-root");
        fixture(&tree.path);
        fs::write(tree.path.join("stray.txt"), b"stray").unwrap();
        let errors = verify_dir(&tree.path).expect_err("根目录多出文件必须失败");
        assert!(
            errors.iter().any(|error| error.contains("根目录多出")),
            "{errors:?}"
        );
    }

    #[test]
    fn verify_rejects_tampered_smoke_config() {
        let tree = TempTree::new("verify-smoke-config");
        fixture(&tree.path);
        fs::write(tree.path.join(SMOKE_CONFIG), "tampered\n").unwrap();
        let errors = verify_dir(&tree.path).expect_err("smoke 配置篡改必须失败");
        assert!(errors.iter().any(|error| error.contains("smoke.config")));
    }

    #[test]
    fn verify_rejects_tampered_inspect_evidence() {
        let tree = TempTree::new("verify-evidence");
        fixture(&tree.path);
        fs::write(tree.path.join(SMOKE_EVIDENCE), "tampered\n").unwrap();
        let errors = verify_dir(&tree.path).expect_err("inspect 证据篡改必须失败");
        assert!(errors.iter().any(|error| error.contains("smoke.evidence")));
    }

    #[test]
    fn inspect_output_must_name_the_expected_plugin() {
        let tree = TempTree::new("inspect-identity");
        fixture(&tree.path);
        let mut output = expected_inspect();
        output = output.replace(
            "- greeting (plugin: greeting)",
            "- greeting (plugin: hello)",
        );
        let errors = verify_inspect_output(&output).expect_err("插件身份错误必须失败");
        assert!(errors.contains("plugin: greeting"));
    }

    #[test]
    fn inspect_output_requires_mounted_zero() {
        let tree = TempTree::new("inspect-mounted");
        fixture(&tree.path);
        let output = expected_inspect().replace("mounted = 0", "mounted = 1");
        let errors = verify_inspect_output(&output).expect_err("mounted 非零必须失败");
        assert!(errors.contains("mounted = 0"));
    }

    #[test]
    fn detects_source_policy_cli_support() {
        let tree = TempTree::new("policy-detect");
        let cli = tree.path.join("crates/cli/src/lib.rs");
        fs::create_dir_all(cli.parent().unwrap()).unwrap();
        fs::write(&cli, "pub const FLAG: &str = \"--source-policy\";").unwrap();
        assert!(cli_supports_source_policy(&tree.path));
        fs::write(&cli, "pub fn no_policy_here() {}").unwrap();
        assert!(!cli_supports_source_policy(&tree.path));

        let main = tree.path.join("crates/cli/src/main.rs");
        fs::write(&main, "const FLAG: &str = \"--source-policy\";").unwrap();
        assert!(cli_supports_source_policy(&tree.path));
    }

    #[test]
    fn source_digest_ignores_generated_directories() {
        let tree = TempTree::new("source-digest");
        fs::create_dir_all(tree.path.join("src")).unwrap();
        fs::create_dir_all(tree.path.join("target")).unwrap();
        fs::create_dir_all(tree.path.join("dist")).unwrap();
        fs::create_dir_all(tree.path.join("out")).unwrap();
        fs::write(tree.path.join("src/lib.rs"), "fn main() {}\n").unwrap();
        fs::write(tree.path.join("target/junk"), "junk").unwrap();
        fs::write(tree.path.join("dist/junk"), "junk").unwrap();
        fs::write(tree.path.join("out/artifact.bin"), "junk").unwrap();
        fs::write(tree.path.join("notes.swp"), "junk").unwrap();
        fs::write(tree.path.join(".git"), "gitdir: /tmp/linked-worktree\n").unwrap();
        let excluded = tree.path.join("out");
        let (first, files) = source_tree_digest(&tree.path, &excluded).unwrap();
        assert_eq!(files, 1);
        fs::write(tree.path.join("target/junk"), "changed").unwrap();
        fs::write(tree.path.join("dist/junk"), "changed").unwrap();
        let (second, _) = source_tree_digest(&tree.path, &excluded).unwrap();
        assert_eq!(first, second);
        fs::write(tree.path.join("src/lib.rs"), "fn main() { println!(); }\n").unwrap();
        let (third, _) = source_tree_digest(&tree.path, &excluded).unwrap();
        assert_ne!(first, third);
    }

    fn plugin_file_name(crate_name: &str) -> String {
        if cfg!(windows) {
            format!("{crate_name}.dll")
        } else if cfg!(target_os = "macos") {
            format!("lib{crate_name}.dylib")
        } else {
            format!("lib{crate_name}.so")
        }
    }

    #[test]
    fn cargo_artifact_output_selects_exactly_the_three_cdylibs() {
        let tree = TempTree::new("cargo-artifacts");
        let plugins = tree.path.join("plugins");
        fs::create_dir_all(&plugins).unwrap();
        let mut output = String::new();
        for spec in PLUGINS {
            let path = plugins.join(plugin_file_name(spec.krate));
            fs::write(&path, b"artifact").unwrap();
            let line = serde_json::json!({
                "reason": "compiler-artifact",
                "target": {"name": spec.krate, "crate_types": ["cdylib"]},
                "filenames": [path.to_string_lossy()],
            });
            output.push_str(&line.to_string());
            output.push('\n');
        }
        output.push_str(
            &serde_json::json!({
                "reason": "compiler-artifact",
                "target": {"name": "not-a-plugin", "crate_types": ["lib"]},
                "filenames": ["ignored.rlib"],
            })
            .to_string(),
        );
        let found = parse_build_artifacts(&output).expect("三个 cdylib 应被识别");
        assert_eq!(found.len(), PLUGINS.len());
        for spec in PLUGINS {
            assert!(found.contains_key(spec.krate));
        }
    }

    #[test]
    fn cargo_artifact_output_must_contain_all_three_cdylibs() {
        let tree = TempTree::new("cargo-missing");
        let path = tree.path.join(plugin_file_name("greeting"));
        fs::write(&path, b"artifact").unwrap();
        let output = format!(
            "{}\n",
            serde_json::json!({
                "reason": "compiler-artifact",
                "target": {"name": "greeting", "crate_types": ["cdylib"]},
                "filenames": [path.to_string_lossy()],
            })
        );
        let error = parse_build_artifacts(&output).expect_err("缺产物必须失败");
        assert!(error.contains("缺少"));
    }

    #[test]
    fn artifact_extension_matches_the_platform() {
        if cfg!(windows) {
            assert!(is_plugin_file(Path::new("greeting.dll")));
            assert!(!is_plugin_file(Path::new("libgreeting.so")));
        } else if cfg!(target_os = "macos") {
            assert!(is_plugin_file(Path::new("libgreeting.dylib")));
        } else {
            assert!(is_plugin_file(Path::new("libgreeting.so")));
        }
    }
}
