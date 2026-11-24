//! 可执行架构与供应链门禁：`cargo xtask arch`。
//!
//! 把“依赖分层、纯度边界、工具链精确锁版、GitHub Actions SHA 钉死、工作流最小权限”
//! 从文档约定升级成会自动变红的静态门禁。
//!
//! 设计纪律：
//! 1. Fail-closed：解析器看到的每一条 `uses:` 必须与行扫描一一对应；空目录、缺失文件、
//!    未登记的 `crates/*` 新包一律报错，绝不静默放行。
//! 2. Allowlist 活性绑定（liveness pinning）：写权限白名单里只要有一条不再对应真实
//!    workflow 声明，守卫立即变红，防止历史豁免永久滞留。
//! 3. 每条规则都有负例反证单测（"A guard that can't go red is worse than none"）。

use serde_json::Value as JsonValue;
use serde_yaml::Value as YamlValue;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

/// 允许持有 workflow 级 `write` 权限的显式白名单：`(workflow 文件名, permission scope)`。
const WORKFLOW_WRITE_GRANTS: &[(&str, &str)] = &[
    // nightly-soak 上传长期压测日志到 rolling GitHub Release。
    ("nightly-soak.yml", "contents"),
];

/// 允许持有 job 级 `write` 权限的显式白名单：`(workflow 文件名, job 名, permission scope)`。
const JOB_WRITE_GRANTS: &[(&str, &str, &str)] = &[];

#[derive(Debug, Clone, Copy)]
struct CrateRule {
    name: &'static str,
    tier: u8,
    forbidden_deps: &'static [&'static str],
    forbidden_tokio_features: &'static [&'static str],
}

/// `crates/*` 的显式分层与纯度契约。任何新增在 `crates/` 下的包都必须先在此登记层级。
const CRATE_RULES: &[CrateRule] = &[
    CrateRule {
        name: "rust-cordis-abi",
        tier: 0,
        forbidden_deps: &[
            "tokio",
            "reqwest",
            "rust-cordis-core",
            "rust-cordis-host",
            "rust-cordis-sdk",
            "rust-cordis-macros",
            "cordis-components",
            "cordis-cli",
        ],
        forbidden_tokio_features: &["full"],
    },
    CrateRule {
        name: "rust-cordis-macros",
        tier: 0,
        forbidden_deps: &[
            "tokio",
            "reqwest",
            "rust-cordis-abi",
            "rust-cordis-core",
            "rust-cordis-host",
            "rust-cordis-sdk",
            "cordis-components",
            "cordis-cli",
        ],
        forbidden_tokio_features: &["full"],
    },
    CrateRule {
        name: "rust-cordis-core",
        tier: 1,
        forbidden_deps: &[
            "reqwest",
            "rust-cordis-abi",
            "rust-cordis-host",
            "rust-cordis-sdk",
            "rust-cordis-macros",
            "cordis-components",
            "cordis-cli",
        ],
        forbidden_tokio_features: &["full"],
    },
    CrateRule {
        name: "rust-cordis-sdk",
        tier: 2,
        forbidden_deps: &[
            "tokio",
            "reqwest",
            "rust-cordis-host",
            "cordis-components",
            "cordis-cli",
        ],
        forbidden_tokio_features: &["full"],
    },
    CrateRule {
        name: "rust-cordis-host",
        tier: 2,
        forbidden_deps: &[
            "reqwest",
            "rust-cordis-sdk",
            "cordis-components",
            "cordis-cli",
        ],
        forbidden_tokio_features: &["full"],
    },
    CrateRule {
        name: "cordis-components",
        tier: 3,
        forbidden_deps: &["reqwest", "rust-cordis-host", "cordis-cli"],
        forbidden_tokio_features: &["full"],
    },
    CrateRule {
        name: "cordis-cli",
        tier: 3,
        forbidden_deps: &["reqwest", "rust-cordis-sdk"],
        forbidden_tokio_features: &["full"],
    },
];

/// 运行全部架构与供应链守卫，返回所有违规项（空列表表示通过）。
pub fn check_repo(repo_root: &Path) -> Vec<String> {
    let mut violations = Vec::new();
    let workflows_dir = repo_root.join(".github").join("workflows");
    let workflow_allow: BTreeSet<(String, String)> = WORKFLOW_WRITE_GRANTS
        .iter()
        .map(|(f, s)| ((*f).to_string(), (*s).to_string()))
        .collect();
    let job_allow: BTreeSet<(String, String, String)> = JOB_WRITE_GRANTS
        .iter()
        .map(|(f, j, s)| ((*f).to_string(), (*j).to_string(), (*s).to_string()))
        .collect();

    violations.extend(check_workflow_action_pins(&workflows_dir));
    violations.extend(check_workflow_permissions(
        &workflows_dir,
        &workflow_allow,
        &job_allow,
    ));
    violations.extend(check_toolchain_pin_and_ci_sync(repo_root, &workflows_dir));
    violations.extend(check_workspace_crate_dag(repo_root));
    violations
}

fn workflow_files(directory: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| {
            path.is_file()
                && matches!(
                    path.extension().and_then(|ext| ext.to_str()),
                    Some("yml" | "yaml")
                )
        })
        .collect();
    files.sort();
    files
}

fn parsed_uses(doc: &YamlValue) -> Vec<String> {
    let mut found = Vec::new();
    let mut stack = vec![doc];
    while let Some(node) = stack.pop() {
        match node {
            YamlValue::Mapping(map) => {
                for (key, value) in map {
                    if key.as_str() == Some("uses") {
                        if let Some(s) = value.as_str() {
                            found.push(s.to_string());
                        }
                    }
                    stack.push(value);
                }
            }
            YamlValue::Sequence(seq) => {
                for item in seq {
                    stack.push(item);
                }
            }
            _ => {}
        }
    }
    found
}

fn scanned_uses(text: &str) -> Vec<(usize, String, String)> {
    let mut out = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') {
            continue;
        }
        let after_dash = if let Some(rest) = trimmed.strip_prefix('-') {
            if rest.is_empty() || rest.starts_with(char::is_whitespace) {
                rest.trim_start()
            } else {
                continue;
            }
        } else {
            trimmed
        };
        let Some(rest) = after_dash.strip_prefix("uses") else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(rhs) = rest.strip_prefix(':') else {
            continue;
        };
        let rhs = rhs.trim();
        if rhs.is_empty() {
            continue;
        }
        let (raw_ref, comment) = match rhs.split_once('#') {
            Some((r, c)) => (r.trim(), c.trim()),
            None => (rhs, ""),
        };
        let cleaned_ref = raw_ref
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .or_else(|| {
                raw_ref
                    .strip_prefix('\'')
                    .and_then(|s| s.strip_suffix('\''))
            })
            .unwrap_or(raw_ref);
        out.push((idx + 1, cleaned_ref.to_string(), comment.to_string()));
    }
    out
}

fn is_lower_hex(s: &str, expected_len: usize) -> bool {
    s.len() == expected_len
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn is_sha_pinned(action_ref: &str) -> bool {
    let Some((repo, sha)) = action_ref.split_once('@') else {
        return false;
    };
    !repo.is_empty()
        && !repo.contains(char::is_whitespace)
        && !repo.contains('@')
        && is_lower_hex(sha, 40)
}

fn is_digest_pinned(action_ref: &str) -> bool {
    let Some(rest) = action_ref.strip_prefix("docker://") else {
        return false;
    };
    let Some((image, digest)) = rest.split_once("@sha256:") else {
        return false;
    };
    !image.is_empty() && !image.contains(char::is_whitespace) && is_lower_hex(digest, 64)
}

fn is_version_comment(comment: &str) -> bool {
    let s = comment.strip_prefix('v').unwrap_or(comment);
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_digit() {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-'))
}

pub(crate) fn check_workflow_file_pins(path: &Path) -> Vec<String> {
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(err) => return vec![format!("{name}: 无法读取 workflow 文件: {err}")],
    };
    let doc: YamlValue = match serde_yaml::from_str(&text) {
        Ok(d) => d,
        Err(err) => return vec![format!("{name}: YAML 解析失败: {err}")],
    };

    let scanned = scanned_uses(&text);
    let mut parsed = parsed_uses(&doc);
    parsed.sort();
    let mut scanned_refs: Vec<String> = scanned.iter().map(|(_, r, _)| r.clone()).collect();
    scanned_refs.sort();

    let mut problems = Vec::new();
    if scanned_refs != parsed {
        problems.push(format!(
            "{name}: 行扫描无法覆盖解析器看到的全部 `uses:` 键（scanned={scanned_refs:?}, parsed={parsed:?}）— fail closed"
        ));
    }

    for (line_no, action_ref, comment) in scanned {
        if action_ref.starts_with("./") {
            continue;
        }
        let where_ref = format!("{name}:{line_no}");
        if action_ref.starts_with("docker://") {
            if !is_digest_pinned(&action_ref) {
                problems.push(format!(
                    "{where_ref}: {action_ref} 未钉死到镜像摘要（`@sha256:<64 hex>`）"
                ));
                continue;
            }
        } else if !is_sha_pinned(&action_ref) {
            problems.push(format!(
                "{where_ref}: {action_ref} 未钉死到完整 40 位 commit SHA"
            ));
            continue;
        }
        if !is_version_comment(&comment) {
            let shown = if comment.is_empty() {
                "(none)"
            } else {
                &comment
            };
            problems.push(format!(
                "{where_ref}: {action_ref} 缺少规范的尾注 `# <version>`（实际: {shown}）"
            ));
        }
    }
    problems
}

pub(crate) fn check_workflow_action_pins(directory: &Path) -> Vec<String> {
    let workflows = workflow_files(directory);
    if workflows.is_empty() {
        return vec![format!(
            "未在 {} 下发现任何 workflow 文件",
            directory.display()
        )];
    }
    let mut total_uses = 0usize;
    let mut problems = Vec::new();
    for path in &workflows {
        if let Ok(text) = std::fs::read_to_string(path) {
            total_uses += scanned_uses(&text).len();
        }
        problems.extend(check_workflow_file_pins(path));
    }
    if total_uses == 0 && problems.is_empty() {
        problems.push(format!(
            "{} 下的 workflow 未匹配到任何 `uses:` 步骤（扫描器可能失效）",
            directory.display()
        ));
    }
    problems
}

fn permission_write_scopes(
    value: &YamlValue,
    where_label: &str,
) -> (BTreeSet<String>, Vec<String>) {
    let mut writes = BTreeSet::new();
    let mut problems = Vec::new();
    let Some(map) = value.as_mapping() else {
        problems.push(format!(
            "{where_label}: permissions 必须是显式 scope 映射（禁止 read-all / write-all 简写）"
        ));
        return (writes, problems);
    };
    for (k, v) in map {
        let Some(scope) = k.as_str() else {
            problems.push(format!("{where_label}: permission scope 键必须是字符串"));
            continue;
        };
        let Some(level) = v.as_str() else {
            problems.push(format!(
                "{where_label}: permission `{scope}` 的值必须是 none/read/write"
            ));
            continue;
        };
        match level {
            "none" | "read" => {}
            "write" => {
                writes.insert(scope.to_string());
            }
            other => {
                problems.push(format!(
                    "{where_label}: permission `{scope}` 的值必须为 none/read/write，实际 `{other}`"
                ));
            }
        }
    }
    (writes, problems)
}

pub(crate) fn check_workflow_permissions(
    directory: &Path,
    allowed_workflow_writes: &BTreeSet<(String, String)>,
    allowed_job_writes: &BTreeSet<(String, String, String)>,
) -> Vec<String> {
    let workflows = workflow_files(directory);
    if workflows.is_empty() {
        return vec![format!(
            "未在 {} 下发现任何 workflow 文件",
            directory.display()
        )];
    }

    let mut problems = Vec::new();
    let mut workflow_grants: BTreeSet<(String, String)> = BTreeSet::new();
    let mut job_grants: BTreeSet<(String, String, String)> = BTreeSet::new();

    for path in workflows {
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(err) => {
                problems.push(format!("{name}: 读取失败: {err}"));
                continue;
            }
        };
        let doc: YamlValue = match serde_yaml::from_str(&text) {
            Ok(d) => d,
            Err(err) => {
                problems.push(format!("{name}: YAML 解析失败: {err}"));
                continue;
            }
        };
        let Some(root) = doc.as_mapping() else {
            problems.push(format!("{name}: workflow 顶层必须是 YAML 映射"));
            continue;
        };
        match root.get(YamlValue::String("permissions".to_string())) {
            None => problems.push(format!("{name}: 必须在 workflow 顶层显式声明 permissions")),
            Some(perms) => {
                let (writes, p) = permission_write_scopes(perms, &name);
                problems.extend(p);
                for scope in writes {
                    workflow_grants.insert((name.clone(), scope));
                }
            }
        }
        let Some(jobs_val) = root.get(YamlValue::String("jobs".to_string())) else {
            problems.push(format!("{name}: 缺少 jobs 映射"));
            continue;
        };
        let Some(jobs) = jobs_val.as_mapping() else {
            problems.push(format!("{name}: jobs 必须是映射"));
            continue;
        };
        for (job_key, job_val) in jobs {
            let (Some(job_name), Some(job_map)) = (job_key.as_str(), job_val.as_mapping()) else {
                problems.push(format!("{name}: 非法 job 定义"));
                continue;
            };
            if let Some(job_perms) = job_map.get(YamlValue::String("permissions".to_string())) {
                let (writes, p) = permission_write_scopes(job_perms, &format!("{name}:{job_name}"));
                problems.extend(p);
                for scope in writes {
                    job_grants.insert((name.clone(), job_name.to_string(), scope));
                }
            }
        }
    }

    for (wf, scope) in workflow_grants.difference(allowed_workflow_writes) {
        problems.push(format!("未入白名单的 workflow 写权限: {wf}:{scope}"));
    }
    for (wf, scope) in allowed_workflow_writes.difference(&workflow_grants) {
        problems.push(format!(
            "白名单中的 workflow 写权限已过期（workflow 中已不存在）: {wf}:{scope}"
        ));
    }
    for (wf, job, scope) in job_grants.difference(allowed_job_writes) {
        problems.push(format!("未入白名单的 job 写权限: {wf}:{job}:{scope}"));
    }
    for (wf, job, scope) in allowed_job_writes.difference(&job_grants) {
        problems.push(format!(
            "白名单中的 job 写权限已过期（workflow 中已不存在）: {wf}:{job}:{scope}"
        ));
    }
    problems
}

fn is_exact_semver_patch(version: &str) -> bool {
    let parts: Vec<&str> = version.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
}

fn parse_toml_string_field(text: &str, section: &str, key: &str) -> Result<Option<String>, String> {
    let mut in_section = false;
    let mut found: Option<String> = None;
    for (idx, raw_line) in text.lines().enumerate() {
        let line = raw_line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            let name = line[1..line.len() - 1].trim();
            in_section = name == section;
            continue;
        }
        if !in_section {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            if k.trim() == key {
                let v = v.trim();
                let unquoted = v
                    .strip_prefix('"')
                    .and_then(|s| s.strip_suffix('"'))
                    .or_else(|| v.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
                    .ok_or_else(|| {
                        format!("第 {} 行 `{key}` 必须是引号字符串，实际 `{v}`", idx + 1)
                    })?;
                if found.replace(unquoted.to_string()).is_some() {
                    return Err(format!("`[{section}].{key}` 重复声明"));
                }
            }
        }
    }
    Ok(found)
}

fn normalize_msrv(raw: &str) -> String {
    let parts: Vec<&str> = raw.split('.').collect();
    if parts.len() == 2 {
        format!("{raw}.0")
    } else {
        raw.to_string()
    }
}

pub(crate) fn check_toolchain_pin_and_ci_sync(
    repo_root: &Path,
    workflows_dir: &Path,
) -> Vec<String> {
    let mut problems = Vec::new();
    let toolchain_path = repo_root.join("rust-toolchain.toml");
    let toolchain_text = match std::fs::read_to_string(&toolchain_path) {
        Ok(t) => t,
        Err(err) => {
            return vec![format!(
                "缺少或无法读取 rust-toolchain.toml（{}）: {err}",
                toolchain_path.display()
            )];
        }
    };
    let pinned_channel = match parse_toml_string_field(&toolchain_text, "toolchain", "channel") {
        Ok(Some(ch)) if is_exact_semver_patch(&ch) => ch,
        Ok(Some(ch)) => {
            return vec![format!(
                "rust-toolchain.toml 的 channel 必须精确到三段补丁号（如 1.98.1），实际 `{ch}`"
            )];
        }
        Ok(None) => {
            return vec!["rust-toolchain.toml 缺少 [toolchain].channel 字段".to_string()];
        }
        Err(err) => return vec![format!("rust-toolchain.toml 解析失败: {err}")],
    };

    let msrv_version = std::fs::read_to_string(repo_root.join("Cargo.toml"))
        .ok()
        .and_then(|t| {
            parse_toml_string_field(&t, "workspace.package", "rust-version")
                .ok()
                .flatten()
        })
        .map(|v| normalize_msrv(&v));

    let mut allowed_ci_toolchains = BTreeSet::new();
    allowed_ci_toolchains.insert(pinned_channel.clone());
    if let Some(ref msrv) = msrv_version {
        allowed_ci_toolchains.insert(msrv.clone());
    }

    for path in workflow_files(workflows_dir) {
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(doc) = serde_yaml::from_str::<YamlValue>(&text) else {
            continue;
        };
        let scanned = scanned_uses(&text);
        let mut non_pinned_versions: BTreeSet<String> = BTreeSet::new();
        for (line_no, action_ref, comment) in &scanned {
            if !action_ref.starts_with("dtolnay/rust-toolchain@") {
                continue;
            }
            if !allowed_ci_toolchains.contains(comment) {
                problems.push(format!(
                    "{name}:{line_no}: dtolnay/rust-toolchain 尾注 `{comment}` 必须与 rust-toolchain.toml (`{pinned_channel}`) 或 workspace rust-version ({msrv_version:?}) 一致"
                ));
            } else if comment != &pinned_channel {
                non_pinned_versions.insert(comment.clone());
            }
        }

        // 如果 workflow 中有 MSRV job 使用与 rust-toolchain.toml 不同的版本，该 job 必须显式设置
        // env.RUSTUP_TOOLCHAIN，否则 rust-toolchain.toml 会静默覆盖 dtolnay/rust-toolchain！
        if !non_pinned_versions.is_empty() {
            if let Some(jobs) = doc
                .as_mapping()
                .and_then(|m| m.get(YamlValue::String("jobs".to_string())))
                .and_then(YamlValue::as_mapping)
            {
                for (job_key, job_val) in jobs {
                    let (Some(job_name), Some(job_map)) = (job_key.as_str(), job_val.as_mapping())
                    else {
                        continue;
                    };
                    let matched_msrv = job_map
                        .get(YamlValue::String("steps".to_string()))
                        .and_then(YamlValue::as_sequence)
                        .and_then(|steps| {
                            steps.iter().find_map(|step| {
                                let u = step
                                    .as_mapping()?
                                    .get(YamlValue::String("uses".to_string()))?
                                    .as_str()?;
                                if !u.starts_with("dtolnay/rust-toolchain@") {
                                    return None;
                                }
                                scanned.iter().find_map(|(_, r, c)| {
                                    (r == u && non_pinned_versions.contains(c)).then(|| c.clone())
                                })
                            })
                        });
                    let Some(expected_msrv) = matched_msrv else {
                        continue;
                    };
                    let env_tc = job_map
                        .get(YamlValue::String("env".to_string()))
                        .and_then(YamlValue::as_mapping)
                        .and_then(|e| e.get(YamlValue::String("RUSTUP_TOOLCHAIN".to_string())))
                        .and_then(YamlValue::as_str);
                    if env_tc != Some(expected_msrv.as_str()) {
                        problems.push(format!(
                            "{name}:{job_name}: 使用非默认工具链 `{expected_msrv}` 的 job 必须设置 `env.RUSTUP_TOOLCHAIN: {expected_msrv}`，防止被根目录 rust-toolchain.toml (`{pinned_channel}`) 静默覆盖"
                        ));
                    }
                }
            }
        }
    }
    problems
}

fn check_workspace_crate_dag(repo_root: &Path) -> Vec<String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let output = match Command::new(&cargo)
        .args(["metadata", "--no-deps", "--format-version", "1", "--locked"])
        .current_dir(repo_root)
        .output()
    {
        Ok(o) => o,
        Err(err) => return vec![format!("执行 cargo metadata 失败: {err}")],
    };
    if !output.status.success() {
        return vec![format!(
            "cargo metadata 退出失败 ({:?}): {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        )];
    }
    let metadata: JsonValue = match serde_json::from_slice(&output.stdout) {
        Ok(v) => v,
        Err(err) => return vec![format!("解析 cargo metadata JSON 失败: {err}")],
    };
    check_metadata_dag(&metadata, repo_root, CRATE_RULES)
}

fn is_under_crates_dir(manifest_path: &str, repo_root: &Path) -> bool {
    let norm_manifest = manifest_path.replace('\\', "/").to_ascii_lowercase();
    let norm_crates = repo_root
        .join("crates")
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase();
    let prefix = if norm_crates.ends_with('/') {
        norm_crates
    } else {
        format!("{norm_crates}/")
    };
    norm_manifest.starts_with(&prefix)
}

fn check_metadata_dag(metadata: &JsonValue, repo_root: &Path, rules: &[CrateRule]) -> Vec<String> {
    let mut violations = Vec::new();
    let rule_map: BTreeMap<&str, &CrateRule> = rules.iter().map(|r| (r.name, r)).collect();
    let Some(packages) = metadata.get("packages").and_then(JsonValue::as_array) else {
        return vec!["cargo metadata 缺少 packages 数组".to_string()];
    };

    let mut seen_crates: BTreeSet<String> = BTreeSet::new();
    let mut adj: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for pkg in packages {
        let Some(name) = pkg.get("name").and_then(JsonValue::as_str) else {
            continue;
        };
        let Some(manifest_path) = pkg.get("manifest_path").and_then(JsonValue::as_str) else {
            continue;
        };
        if !is_under_crates_dir(manifest_path, repo_root) {
            continue;
        }
        seen_crates.insert(name.to_string());
        let Some(rule) = rule_map.get(name) else {
            violations.push(format!(
                "crates/* 下存在未登记分层的 workspace crate `{name}`（{manifest_path}）— 必须在 xtask/src/arch.rs 的 CRATE_RULES 中显式声明层级"
            ));
            continue;
        };
        let Some(deps) = pkg.get("dependencies").and_then(JsonValue::as_array) else {
            continue;
        };
        for dep in deps {
            // 只约束常规与构建依赖（kind == null 或 "build"）；dev-dependencies 允许测试用 tokio 等。
            let kind = dep.get("kind").and_then(JsonValue::as_str);
            if kind == Some("dev") {
                continue;
            }
            let Some(dep_name) = dep.get("name").and_then(JsonValue::as_str) else {
                continue;
            };
            if rule.forbidden_deps.contains(&dep_name) {
                violations.push(format!("{name} (tier {}) 禁止依赖 `{dep_name}`", rule.tier));
            }
            if dep_name == "tokio" {
                if let Some(features) = dep.get("features").and_then(JsonValue::as_array) {
                    for feat in features.iter().filter_map(JsonValue::as_str) {
                        if rule.forbidden_tokio_features.contains(&feat) {
                            violations.push(format!(
                                "{name} (tier {}) 禁止开启 tokio 的 `{feat}` feature",
                                rule.tier
                            ));
                        }
                    }
                }
            }
            if let Some(target_rule) = rule_map.get(dep_name) {
                adj.entry(name.to_string())
                    .or_default()
                    .push(dep_name.to_string());
                if target_rule.tier > rule.tier {
                    violations.push(format!(
                        "逆向跨层依赖：{name} (tier {}) 不得依赖 {dep_name} (tier {})",
                        rule.tier, target_rule.tier
                    ));
                }
            }
        }
    }

    for registered in rule_map.keys() {
        if !seen_crates.contains(*registered) {
            violations.push(format!(
                "CRATE_RULES 中的 `{registered}` 在 workspace `crates/` 下不存在（请清理过期条目）"
            ));
        }
    }

    // 显式环检测（即使在合成 metadata 夹具里也能报错）。
    let mut visiting = BTreeSet::new();
    let mut visited = BTreeSet::new();
    for node in adj.keys() {
        if !visited.contains(node) {
            detect_cycle(node, &adj, &mut visiting, &mut visited, &mut violations);
        }
    }

    violations
}

fn detect_cycle(
    node: &str,
    adj: &BTreeMap<String, Vec<String>>,
    visiting: &mut BTreeSet<String>,
    visited: &mut BTreeSet<String>,
    violations: &mut Vec<String>,
) {
    visiting.insert(node.to_string());
    if let Some(neighbors) = adj.get(node) {
        for next in neighbors {
            if visiting.contains(next) {
                violations.push(format!("检测到 workspace crate 依赖环: {node} -> {next}"));
            } else if !visited.contains(next) {
                detect_cycle(next, adj, visiting, visited, violations);
            }
        }
    }
    visiting.remove(node);
    visited.insert(node.to_string());
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static TEMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(label: &str) -> Self {
            let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "cordis-arch-test-{label}-{}-{n}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    const PINNED: &str = "actions/checkout@11d5960a326750d5838078e36cf38b85af677262";

    #[test]
    fn real_repo_passes_all_arch_guards() {
        let root = crate::workspace_root();
        let violations = check_repo(&root);
        assert!(
            violations.is_empty(),
            "真实仓库 arch 守卫失败:\n{}",
            violations.join("\n")
        );
    }

    #[test]
    fn workflow_uses_guard_catches_mutable_and_malformed_refs() {
        let tmp = TempDir::new("uses");
        let sha64 = "a".repeat(64);
        let cases: &[(&str, Option<&str>)] = &[
            (
                "      - uses: actions/checkout@v4",
                Some("未钉死到完整 40 位"),
            ),
            (
                "      - uses: dtolnay/rust-toolchain@stable",
                Some("未钉死到完整 40 位"),
            ),
            (
                "        uses: actions/checkout@11d5960a3267 # v4.4.0",
                Some("未钉死到完整 40 位"),
            ),
            (
                "      - uses: owner/action@main # v1.0.0",
                Some("未钉死到完整 40 位"),
            ),
            (
                "      - uses : owner/action@main",
                Some("未钉死到完整 40 位"),
            ),
            (
                "      - uses: \"owner/action@main\"",
                Some("未钉死到完整 40 位"),
            ),
            (&format!("      - uses: {PINNED}"), Some("缺少规范的尾注")),
            (
                &format!("      - uses: {PINNED} # pin me later"),
                Some("缺少规范的尾注"),
            ),
            // Flow style 是合法 YAML，行扫描看不到，必须 fail-closed 报错。
            (&format!("      - {{uses: {PINNED}}}"), Some("fail closed")),
            (&format!("      - uses: {PINNED} # v4.4.0"), None),
            (&format!("      - uses: \"{PINNED}\" # v4.4.0"), None),
            (&format!("      - uses : {PINNED} # v4.4.0"), None),
            ("      - uses: ./.github/workflows/ci.yml", None),
            (
                "      - uses: docker://alpine:3.20",
                Some("未钉死到镜像摘要"),
            ),
            (
                &format!("      - uses: docker://alpine@sha256:{sha64}"),
                Some("缺少规范的尾注"),
            ),
            (
                &format!("      - uses: docker://alpine@sha256:{sha64} # 3.20"),
                None,
            ),
        ];

        for (i, (step, expected)) in cases.iter().enumerate() {
            let wf = tmp.path.join(format!("probe-{i}.yml"));
            std::fs::write(&wf, format!("jobs:\n  build:\n    steps:\n{step}\n")).unwrap();
            let problems = check_workflow_file_pins(&wf);
            match expected {
                None => assert!(
                    problems.is_empty(),
                    "case {i} ({step}) 应通过，实际: {problems:?}"
                ),
                Some(needle) => assert!(
                    problems.iter().any(|p| p.contains(needle)),
                    "case {i} ({step}) 应包含 `{needle}`，实际: {problems:?}"
                ),
            }
        }
    }

    #[test]
    fn workflow_uses_guard_scans_both_yml_and_yaml_and_fails_on_empty_dir() {
        let empty = TempDir::new("empty-wf");
        let problems = check_workflow_action_pins(&empty.path);
        assert!(problems.iter().any(|p| p.contains("未在")), "{problems:?}");

        let tmp = TempDir::new("both-ext");
        for name in ["a.yml", "b.yaml"] {
            std::fs::write(
                tmp.path.join(name),
                "jobs:\n  b:\n    steps:\n      - uses: owner/evil@main\n",
            )
            .unwrap();
        }
        std::fs::write(tmp.path.join("notes.txt"), "uses: owner/evil@main\n").unwrap();
        let problems = check_workflow_action_pins(&tmp.path);
        assert_eq!(problems.len(), 2, "{problems:?}");
    }

    #[test]
    fn workflow_permissions_guard_catches_violations_and_stale_allowlist() {
        let cases: &[(&str, &str)] = &[
            (
                "jobs:\n  build:\n    runs-on: ubuntu-latest\n",
                "显式声明 permissions",
            ),
            (
                "permissions: write-all\njobs:\n  build:\n    runs-on: ubuntu-latest\n",
                "显式 scope 映射",
            ),
            (
                "permissions: read-all\njobs:\n  build:\n    runs-on: ubuntu-latest\n",
                "显式 scope 映射",
            ),
            (
                "permissions:\n  contents: admin\njobs:\n  build:\n    runs-on: ubuntu-latest\n",
                "必须为 none/read/write",
            ),
            (
                "permissions:\n  contents: write\njobs:\n  build:\n    runs-on: ubuntu-latest\n",
                "未入白名单的 workflow 写权限",
            ),
            (
                "permissions:\n  contents: read\njobs:\n  build:\n    runs-on: ubuntu-latest\n    permissions:\n      contents: write\n",
                "未入白名单的 job 写权限",
            ),
        ];
        for (i, (doc, expected)) in cases.iter().enumerate() {
            let tmp = TempDir::new(&format!("perm-{i}"));
            std::fs::write(tmp.path.join("probe.yml"), doc).unwrap();
            let problems =
                check_workflow_permissions(&tmp.path, &BTreeSet::new(), &BTreeSet::new());
            assert!(
                problems.iter().any(|p| p.contains(expected)),
                "case {i} 应包含 `{expected}`，实际: {problems:?}"
            );
        }

        // 活性校验：过期白名单必须变红。
        let stale = TempDir::new("perm-stale");
        std::fs::write(
            stale.path.join("probe.yml"),
            "permissions:\n  contents: read\njobs:\n  build:\n    runs-on: ubuntu-latest\n",
        )
        .unwrap();
        let wf_allow = BTreeSet::from([("probe.yml".to_string(), "contents".to_string())]);
        let job_allow = BTreeSet::from([(
            "probe.yml".to_string(),
            "build".to_string(),
            "contents".to_string(),
        )]);
        let problems = check_workflow_permissions(&stale.path, &wf_allow, &job_allow);
        assert!(
            problems
                .iter()
                .any(|p| p.contains("workflow 写权限已过期") && p.contains("probe.yml:contents")),
            "{problems:?}"
        );
        assert!(
            problems
                .iter()
                .any(|p| p.contains("job 写权限已过期") && p.contains("probe.yml:build:contents")),
            "{problems:?}"
        );
    }

    #[test]
    fn toolchain_pin_guard_rejects_floating_channel_and_unshielded_msrv_job() {
        let tmp = TempDir::new("tc-floating");
        let wf_dir = tmp.path.join(".github").join("workflows");
        std::fs::create_dir_all(&wf_dir).unwrap();
        std::fs::write(
            tmp.path.join("rust-toolchain.toml"),
            "[toolchain]\nchannel = \"stable\"\n",
        )
        .unwrap();
        let problems = check_toolchain_pin_and_ci_sync(&tmp.path, &wf_dir);
        assert!(
            problems.iter().any(|p| p.contains("三段补丁号")),
            "{problems:?}"
        );

        let tmp2 = TempDir::new("tc-msrv-shadow");
        let wf_dir2 = tmp2.path.join(".github").join("workflows");
        std::fs::create_dir_all(&wf_dir2).unwrap();
        std::fs::write(
            tmp2.path.join("rust-toolchain.toml"),
            "[toolchain]\nchannel = \"1.98.1\"\n",
        )
        .unwrap();
        std::fs::write(
            tmp2.path.join("Cargo.toml"),
            "[workspace.package]\nrust-version = \"1.85\"\n",
        )
        .unwrap();
        std::fs::write(
            wf_dir2.join("ci.yml"),
            "permissions:\n  contents: read\njobs:\n  msrv:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: dtolnay/rust-toolchain@9d8ce8d04e0f46cc2c817f1369e4bd480df4ccc6 # 1.85.0\n",
        )
        .unwrap();
        let problems = check_toolchain_pin_and_ci_sync(&tmp2.path, &wf_dir2);
        assert!(
            problems.iter().any(|p| p.contains("env.RUSTUP_TOOLCHAIN")),
            "{problems:?}"
        );
    }

    #[test]
    fn crate_dag_guard_rejects_forbidden_deps_upward_edges_tokio_full_and_unregistered() {
        let root = PathBuf::from("/repo");
        let manifest = |sub: &str| format!("/repo/crates/{sub}/Cargo.toml");
        let bad_metadata = json!({
            "packages": [
                {
                    "name": "rust-cordis-abi",
                    "manifest_path": manifest("abi"),
                    "dependencies": [
                        { "name": "tokio", "kind": null, "features": [] }
                    ]
                },
                {
                    "name": "rust-cordis-macros",
                    "manifest_path": manifest("macros"),
                    "dependencies": []
                },
                {
                    "name": "rust-cordis-core",
                    "manifest_path": manifest("core"),
                    "dependencies": [
                        { "name": "tokio", "kind": null, "features": ["full"] },
                        { "name": "rust-cordis-host", "kind": null, "features": [] }
                    ]
                },
                {
                    "name": "rust-cordis-sdk",
                    "manifest_path": manifest("sdk"),
                    "dependencies": [
                        { "name": "rust-cordis-host", "kind": null, "features": [] }
                    ]
                },
                {
                    "name": "rust-cordis-host",
                    "manifest_path": manifest("host"),
                    "dependencies": []
                },
                {
                    "name": "cordis-components",
                    "manifest_path": manifest("components"),
                    "dependencies": []
                },
                {
                    "name": "cordis-cli",
                    "manifest_path": manifest("cli"),
                    "dependencies": []
                },
                {
                    "name": "cordis-rogue",
                    "manifest_path": manifest("rogue"),
                    "dependencies": []
                }
            ]
        });

        let violations = check_metadata_dag(&bad_metadata, &root, CRATE_RULES);
        assert!(
            violations
                .iter()
                .any(|v| v.contains("rust-cordis-abi") && v.contains("tokio")),
            "{violations:?}"
        );
        assert!(
            violations
                .iter()
                .any(|v| v.contains("rust-cordis-core") && v.contains("full")),
            "{violations:?}"
        );
        assert!(
            violations
                .iter()
                .any(|v| v.contains("逆向跨层依赖") && v.contains("rust-cordis-core")),
            "{violations:?}"
        );
        assert!(
            violations
                .iter()
                .any(|v| v.contains("rust-cordis-sdk") && v.contains("rust-cordis-host")),
            "{violations:?}"
        );
        assert!(
            violations
                .iter()
                .any(|v| v.contains("未登记分层") && v.contains("cordis-rogue")),
            "{violations:?}"
        );
    }
}
