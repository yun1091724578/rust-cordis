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
    violations.extend(check_consumer_lockfile_sync(repo_root));
    violations.extend(check_markdown_doc_links(repo_root));
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

fn yaml_str<'a>(map: &'a serde_yaml::Mapping, key: &str) -> Option<&'a str> {
    map.get(YamlValue::String(key.to_string()))
        .and_then(YamlValue::as_str)
}

fn yaml_env_var<'a>(map: &'a serde_yaml::Mapping, var: &str) -> Option<&'a str> {
    map.get(YamlValue::String("env".to_string()))
        .and_then(YamlValue::as_mapping)
        .and_then(|env| yaml_str(env, var))
}

fn contains_version_token(haystack: &str, expected_version: &str) -> bool {
    let escaped = expected_version.replace('.', "\\.");
    let needles = [
        format!("rustc {expected_version}"),
        format!("rustc {escaped}"),
    ];
    for needle in &needles {
        let mut rest = haystack;
        while let Some(idx) = rest.find(needle.as_str()) {
            let after = &rest[idx + needle.len()..];
            // 禁止前缀误匹配（例如 1.98.1 误匹配 1.98.10）。
            if !after.as_bytes().first().is_some_and(u8::is_ascii_digit) {
                return true;
            }
            rest = after;
        }
    }
    false
}

fn suppresses_failure(line: &str) -> bool {
    if let Some((_, rhs)) = line.split_once("||") {
        let rhs = rhs.trim();
        if rhs == "true"
            || rhs == ":"
            || rhs.starts_with("true ")
            || rhs.starts_with(": ")
            || rhs.starts_with("echo ")
            || rhs.starts_with("printf ")
        {
            return true;
        }
    }
    false
}

fn strip_inline_comment(line: &str) -> &str {
    let mut in_single = false;
    let mut in_double = false;
    let bytes = line.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'\'' if !in_double => in_single = !in_single,
            b'"' if !in_single => in_double = !in_double,
            b'#' if !in_single && !in_double && (i == 0 || bytes[i - 1].is_ascii_whitespace()) => {
                return &line[..i];
            }
            _ => {}
        }
    }
    line
}

fn is_grep_assertion(rhs: &str, expected_version: &str) -> bool {
    let rhs = rhs.trim();
    let Some(rest) = rhs
        .strip_prefix("grep")
        .filter(|r| r.is_empty() || r.starts_with(char::is_whitespace))
    else {
        return false;
    };
    for token in rest.split_whitespace() {
        if token == "--invert-match" {
            return false;
        }
        if token.starts_with('-') && !token.starts_with("--") && token[1..].contains('v') {
            return false;
        }
        if !token.starts_with('-') {
            break;
        }
    }
    contains_version_token(rest, expected_version)
}

fn step_verifies_rustc_version(run_script: &str, expected_version: &str) -> bool {
    let mut captured_vars: Vec<String> = Vec::new();
    for raw_line in run_script.lines() {
        let line = strip_inline_comment(raw_line).trim();
        if line.is_empty() || suppresses_failure(line) {
            continue;
        }
        // 记录 `VAR=$(rustc --version)` 或 `VAR="$(rustc --version)"` 赋值。
        if let Some((lhs, rhs)) = line.split_once('=') {
            let var = lhs.trim();
            let rhs_trimmed = rhs.trim().trim_matches('"').trim_matches('\'');
            if !var.is_empty()
                && var.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                && (rhs_trimmed == "$(rustc --version)" || rhs_trimmed == "`rustc --version`")
            {
                captured_vars.push(var.to_string());
                continue;
            }
        }
        // 形态 1：`rustc --version | grep ... '<expected>'`（禁止多级管道掩盖退出码，拒绝 echo/注释伪造）。
        if let Some((lhs, rhs)) = line.split_once('|') {
            if !rhs.starts_with('|') {
                let lhs = lhs.trim();
                let grep_stage = rhs.split("||").next().unwrap_or(rhs).trim();
                if lhs == "rustc --version"
                    && !grep_stage.contains('|')
                    && is_grep_assertion(grep_stage, expected_version)
                {
                    return true;
                }
            }
        }
        // 形态 2：shell 条件断言 `[[ "$(rustc --version)" == "rustc <ver>"* ]]` /
        //        `[[ "$ver" == "rustc <ver>"* ]]`。
        let expr = line.split("||").next().unwrap_or(line).trim();
        if (expr.starts_with("[[ ") && expr.ends_with(" ]]"))
            || (expr.starts_with("[ ") && expr.ends_with(" ]"))
            || expr.starts_with("test ")
        {
            let references_rustc = expr.contains("$(rustc --version)")
                || expr.contains("`rustc --version`")
                || captured_vars.iter().any(|v| {
                    expr.contains(&format!("${v}")) || expr.contains(&format!("${{{v}}}"))
                });
            let is_positive_cmp =
                expr.contains("==") || expr.contains(" = ") || expr.contains("=~");
            if references_rustc && is_positive_cmp && contains_version_token(expr, expected_version)
            {
                return true;
            }
        }
    }
    false
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
        let doc = match serde_yaml::from_str::<YamlValue>(&text) {
            Ok(doc) => doc,
            Err(err) => {
                problems.push(format!("{name}: YAML 解析失败（fail closed）: {err}"));
                continue;
            }
        };
        let scanned = scanned_uses(&text);
        for (line_no, action_ref, comment) in &scanned {
            if !action_ref.starts_with("dtolnay/rust-toolchain@") {
                continue;
            }
            if !allowed_ci_toolchains.contains(comment) {
                problems.push(format!(
                    "{name}:{line_no}: dtolnay/rust-toolchain 尾注 `{comment}` 必须与 rust-toolchain.toml (`{pinned_channel}`) 或 workspace rust-version ({msrv_version:?}) 一致"
                ));
            }
        }

        let Some(root_map) = doc.as_mapping() else {
            continue;
        };
        // Rustup 优先级：env.RUSTUP_TOOLCHAIN 高于 rust-toolchain.toml 与 rustup default。
        // 顶层 env 若声明 RUSTUP_TOOLCHAIN，不得偏离 pinned_channel。
        if let Some(top_tc) = yaml_env_var(root_map, "RUSTUP_TOOLCHAIN") {
            if top_tc != pinned_channel {
                problems.push(format!(
                    "{name}: workflow 顶层 `env.RUSTUP_TOOLCHAIN` 为 `{top_tc}`，会覆盖精确锁版 `{pinned_channel}`"
                ));
            }
        }

        let Some(jobs) = root_map
            .get(YamlValue::String("jobs".to_string()))
            .and_then(YamlValue::as_mapping)
        else {
            continue;
        };

        for (job_key, job_val) in jobs {
            let (Some(job_name), Some(job_map)) = (job_key.as_str(), job_val.as_mapping()) else {
                continue;
            };
            let steps = job_map
                .get(YamlValue::String("steps".to_string()))
                .and_then(YamlValue::as_sequence);

            let mut job_toolchain_from_action: Option<String> = None;
            let mut has_rustc_version_check = false;

            if let Some(steps) = steps {
                for step in steps {
                    let Some(step_map) = step.as_mapping() else {
                        continue;
                    };
                    if let Some(uses_val) = yaml_str(step_map, "uses") {
                        if uses_val.starts_with("dtolnay/rust-toolchain@") {
                            let comment = scanned
                                .iter()
                                .find(|(_, r, _)| r == uses_val)
                                .map(|(_, _, c)| c.clone())
                                .unwrap_or_default();
                            if !comment.is_empty() {
                                job_toolchain_from_action = Some(comment.clone());
                            }
                            if let Some(with_tc) = step_map
                                .get(YamlValue::String("with".to_string()))
                                .and_then(YamlValue::as_mapping)
                                .and_then(|w| yaml_str(w, "toolchain"))
                            {
                                let expected = if comment.is_empty() {
                                    pinned_channel.as_str()
                                } else {
                                    comment.as_str()
                                };
                                if with_tc != expected {
                                    problems.push(format!(
                                        "{name}:{job_name}: dtolnay/rust-toolchain 的 `with.toolchain` 为 `{with_tc}`，与预期锁版 `{expected}` 不一致"
                                    ));
                                }
                            }
                        }
                    }
                }

                let expected_job_tc = job_toolchain_from_action
                    .as_deref()
                    .unwrap_or(pinned_channel.as_str());

                if let Some(job_env_tc) = yaml_env_var(job_map, "RUSTUP_TOOLCHAIN") {
                    if job_env_tc != expected_job_tc {
                        problems.push(format!(
                            "{name}:{job_name}: job 级 `env.RUSTUP_TOOLCHAIN` 为 `{job_env_tc}`，与预期工具链 `{expected_job_tc}` 不一致"
                        ));
                    }
                } else if expected_job_tc != pinned_channel {
                    problems.push(format!(
                        "{name}:{job_name}: 使用非默认工具链 `{expected_job_tc}` 的 job 必须设置 `env.RUSTUP_TOOLCHAIN: {expected_job_tc}`，防止被根目录 rust-toolchain.toml (`{pinned_channel}`) 静默覆盖"
                    ));
                }

                for step in steps {
                    let Some(step_map) = step.as_mapping() else {
                        continue;
                    };
                    if let Some(step_env_tc) = yaml_env_var(step_map, "RUSTUP_TOOLCHAIN") {
                        if step_env_tc != expected_job_tc {
                            problems.push(format!(
                                "{name}:{job_name}: step 级 `env.RUSTUP_TOOLCHAIN` 为 `{step_env_tc}`，会覆盖预期工具链 `{expected_job_tc}`"
                            ));
                        }
                    }
                    if let Some(run_script) = yaml_str(step_map, "run") {
                        if step_verifies_rustc_version(run_script, expected_job_tc) {
                            has_rustc_version_check = true;
                        }
                    }
                }

                if name == "ci.yml"
                    && job_toolchain_from_action.is_some()
                    && !has_rustc_version_check
                {
                    problems.push(format!(
                        "{name}:{job_name}: ci.yml 中安装 Rust 工具链的 job 必须显式校验实际生效的 `rustc --version` 为 `{expected_job_tc}`"
                    ));
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
        let mut tokio_dep_keys: BTreeSet<String> = BTreeSet::from(["tokio".to_string()]);
        if let Some(deps) = pkg.get("dependencies").and_then(JsonValue::as_array) {
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
                    if let Some(rename) = dep.get("rename").and_then(JsonValue::as_str) {
                        tokio_dep_keys.insert(rename.to_string());
                    }
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

        // 检查 Cargo `[features]` 转发（如 `feat = ["tokio/full"]`、`["tokio?/full"]` 或重命名别名转发）。
        if let Some(pkg_features) = pkg.get("features").and_then(JsonValue::as_object) {
            for (feat_name, entries) in pkg_features {
                let Some(list) = entries.as_array() else {
                    continue;
                };
                for entry in list.iter().filter_map(JsonValue::as_str) {
                    if let Some((raw_dep, forwarded_feat)) = entry.split_once('/') {
                        let dep_key = raw_dep.strip_suffix('?').unwrap_or(raw_dep);
                        if tokio_dep_keys.contains(dep_key)
                            && rule.forbidden_tokio_features.contains(&forwarded_feat)
                        {
                            violations.push(format!(
                                "{name} (tier {}) 的 Cargo feature `{feat_name}` 转发开启了禁止的 tokio feature `{forwarded_feat}`（`{entry}`）",
                                rule.tier
                            ));
                        }
                    }
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

fn check_consumer_lockfile_sync(repo_root: &Path) -> Vec<String> {
    let root_manifest = match std::fs::read_to_string(repo_root.join("Cargo.toml")) {
        Ok(t) => t,
        Err(err) => return vec![format!("读取根 Cargo.toml 失败: {err}")],
    };
    let expected_version =
        match parse_toml_string_field(&root_manifest, "workspace.package", "version") {
            Ok(Some(v)) => v,
            Ok(None) => return vec!["根 Cargo.toml 缺少 [workspace.package].version".to_string()],
            Err(err) => return vec![format!("解析根 Cargo.toml version 失败: {err}")],
        };
    let lock_path = repo_root
        .join("tests")
        .join("consumer-service")
        .join("Cargo.lock");
    let lock_text = match std::fs::read_to_string(&lock_path) {
        Ok(t) => t,
        Err(err) => {
            return vec![format!(
                "缺少或无法读取独立消费者锁文件（{}）: {err}",
                lock_path.display()
            )];
        }
    };
    check_consumer_lockfile_str(&lock_text, &expected_version)
}

fn check_consumer_lockfile_str(lock_text: &str, expected_version: &str) -> Vec<String> {
    const TRACKED: &[&str] = &["rust-cordis-abi", "rust-cordis-core", "rust-cordis-host"];
    let mut found: BTreeMap<String, String> = BTreeMap::new();
    let mut in_pkg = false;
    let mut cur_name: Option<String> = None;
    let mut cur_ver: Option<String> = None;

    let mut flush = |name: &mut Option<String>, ver: &mut Option<String>| {
        if let (Some(n), Some(v)) = (name.take(), ver.take()) {
            if TRACKED.contains(&n.as_str()) {
                found.insert(n, v);
            }
        } else {
            *name = None;
            *ver = None;
        }
    };

    for raw in lock_text.lines() {
        let line = raw.trim();
        if line == "[[package]]" {
            flush(&mut cur_name, &mut cur_ver);
            in_pkg = true;
            continue;
        }
        if line.starts_with('[') {
            flush(&mut cur_name, &mut cur_ver);
            in_pkg = false;
            continue;
        }
        if !in_pkg {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let val = v.trim().trim_matches('"').to_string();
            match k.trim() {
                "name" => cur_name = Some(val),
                "version" => cur_ver = Some(val),
                _ => {}
            }
        }
    }
    flush(&mut cur_name, &mut cur_ver);

    let mut violations = Vec::new();
    for pkg in TRACKED {
        match found.get(*pkg) {
            Some(ver) if ver == expected_version => {}
            Some(ver) => violations.push(format!(
                "tests/consumer-service/Cargo.lock 中 `{pkg}` 锁定版本为 `{ver}`，落后于 workspace 版本 `{expected_version}`（`cargo xtask consumer` 的 `--locked` 将失败）"
            )),
            None => violations.push(format!(
                "tests/consumer-service/Cargo.lock 中缺少 `{pkg}` 条目"
            )),
        }
    }
    violations
}

fn collect_markdown_files(repo_root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(repo_root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("md") {
                files.push(path);
            }
        }
    }
    let mut stack = vec![repo_root.join("docs")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("md") {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

fn extract_docs_md_refs(text: &str) -> Vec<(usize, String)> {
    let mut refs = Vec::new();
    let mut in_fence = false;
    for (idx, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        let mut cursor = 0usize;
        while let Some(pos) = line[cursor..].find("docs/") {
            let start = cursor + pos;
            // 仅接受词边界后的 `docs/...`，避免命中 URL 路径片段如 `https://example.com/docs/a.md`。
            let prev_ok = start == 0
                || matches!(
                    line.as_bytes()[start - 1],
                    b' ' | b'\t' | b'`' | b'(' | b'[' | b'"' | b'\'' | b':'
                )
                || !line.as_bytes()[start - 1].is_ascii();
            let rest = &line[start..];
            let end_rel = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '-' | '_')))
                .unwrap_or(rest.len());
            let candidate = &rest[..end_rel];
            if prev_ok
                && candidate.ends_with(".md")
                && !candidate.contains('*')
                && !candidate.contains("..")
            {
                let entry = (idx + 1, candidate.to_string());
                if !refs.contains(&entry) {
                    refs.push(entry);
                }
            }
            cursor = start + end_rel.max(5);
        }
    }
    refs
}

fn check_markdown_doc_links(repo_root: &Path) -> Vec<String> {
    let files = collect_markdown_files(repo_root);
    if files.is_empty() {
        return vec!["未在仓库中发现任何 Markdown 文档（fail closed）".to_string()];
    }
    let mut violations = Vec::new();
    for path in files {
        let rel = path
            .strip_prefix(repo_root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for (line_no, target) in extract_docs_md_refs(&text) {
            if !repo_root.join(&target).is_file() {
                violations.push(format!(
                    "{rel}:{line_no}: 引用了不存在的文档路径 `{target}`"
                ));
            }
        }
    }
    violations
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

        // 顶层 env.RUSTUP_TOOLCHAIN: stable 与缺少 rustc --version 校验必须判红。
        let tmp3 = TempDir::new("tc-top-env-override");
        let wf_dir3 = tmp3.path.join(".github").join("workflows");
        std::fs::create_dir_all(&wf_dir3).unwrap();
        std::fs::write(
            tmp3.path.join("rust-toolchain.toml"),
            "[toolchain]\nchannel = \"1.98.1\"\n",
        )
        .unwrap();
        std::fs::write(
            wf_dir3.join("ci.yml"),
            "permissions:\n  contents: read\nenv:\n  RUSTUP_TOOLCHAIN: stable\njobs:\n  lint:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: dtolnay/rust-toolchain@01e2821fc38dbca62c2557c927b33938a2e005fd # 1.98.1\n",
        )
        .unwrap();
        let problems3 = check_toolchain_pin_and_ci_sync(&tmp3.path, &wf_dir3);
        assert!(
            problems3
                .iter()
                .any(|p| p.contains("顶层 `env.RUSTUP_TOOLCHAIN`")),
            "{problems3:?}"
        );
        assert!(
            problems3.iter().any(|p| p.contains("rustc --version")),
            "{problems3:?}"
        );
    }

    #[test]
    fn crate_dag_guard_rejects_feature_forwarded_tokio_full() {
        let root = PathBuf::from("/repo");
        let manifest = |sub: &str| format!("/repo/crates/{sub}/Cargo.toml");
        let forwarded_metadata = json!({
            "packages": [
                { "name": "rust-cordis-abi", "manifest_path": manifest("abi"), "dependencies": [] },
                { "name": "rust-cordis-macros", "manifest_path": manifest("macros"), "dependencies": [] },
                {
                    "name": "rust-cordis-core",
                    "manifest_path": manifest("core"),
                    "dependencies": [
                        { "name": "tokio", "rename": "tk", "kind": null, "features": ["rt"] }
                    ],
                    "features": {
                        "stealth": ["tokio/full"],
                        "stealth_opt": ["tk?/full"]
                    }
                },
                { "name": "rust-cordis-sdk", "manifest_path": manifest("sdk"), "dependencies": [] },
                { "name": "rust-cordis-host", "manifest_path": manifest("host"), "dependencies": [] },
                { "name": "cordis-components", "manifest_path": manifest("components"), "dependencies": [] },
                { "name": "cordis-cli", "manifest_path": manifest("cli"), "dependencies": [] }
            ]
        });
        let violations = check_metadata_dag(&forwarded_metadata, &root, CRATE_RULES);
        assert!(
            violations
                .iter()
                .any(|v| v.contains("stealth") && v.contains("tokio/full")),
            "{violations:?}"
        );
        assert!(
            violations
                .iter()
                .any(|v| v.contains("stealth_opt") && v.contains("tk?/full")),
            "{violations:?}"
        );
    }

    #[test]
    fn consumer_lockfile_and_markdown_link_guards_catch_drift_and_dead_links() {
        let stale_lock = r#"
[[package]]
name = "rust-cordis-abi"
version = "0.1.3"
[[package]]
name = "rust-cordis-core"
version = "0.1.5"
[[package]]
name = "rust-cordis-host"
version = "0.1.3"
"#;
        let lock_v = check_consumer_lockfile_str(stale_lock, "0.1.5");
        assert!(
            lock_v
                .iter()
                .any(|v| v.contains("rust-cordis-abi") && v.contains("0.1.3")),
            "{lock_v:?}"
        );
        assert!(
            lock_v
                .iter()
                .any(|v| v.contains("rust-cordis-host") && v.contains("0.1.3")),
            "{lock_v:?}"
        );

        let tmp = TempDir::new("dead-doc");
        std::fs::create_dir_all(tmp.path.join("docs")).unwrap();
        std::fs::write(
            tmp.path.join("README.md"),
            "见 [`docs/deleted.md`](docs/deleted.md)\n```\ndocs/ignored_in_fence.md\n```\n",
        )
        .unwrap();
        let doc_v = check_markdown_doc_links(&tmp.path);
        assert_eq!(doc_v.len(), 1, "{doc_v:?}");
        assert!(doc_v[0].contains("docs/deleted.md"), "{doc_v:?}");
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

    #[test]
    fn rustc_version_guard_rejects_echo_comments_and_non_failing_steps() {
        // 真实会失败的断言必须放行。
        assert!(step_verifies_rustc_version(
            "rustc --version | grep -q '^rustc 1\\.98\\.1 '",
            "1.98.1"
        ));
        assert!(step_verifies_rustc_version(
            "[[ \"$(rustc --version)\" == \"rustc 1.98.1 \"* ]]",
            "1.98.1"
        ));
        assert!(step_verifies_rustc_version(
            "ver=\"$(rustc --version)\"\n[[ \"$ver\" == \"rustc 1.98.1 \"* ]] || exit 1",
            "1.98.1"
        ));

        // 任何不会失败的提示、注释、吞错、取反或前缀误匹配都必须拒绝。
        for bad in [
            "echo \"hint: rustc --version should be rustc 1.98.1\"",
            "printf 'rustc --version | grep -q \"^rustc 1.98.1 \"\\n'",
            "# rustc --version | grep -q '^rustc 1\\.98\\.1 '",
            "rustc --version | grep -q '^rustc 1\\.98\\.1 ' || true",
            "rustc --version | grep -q '^rustc 1\\.98\\.1 ' | cat",
            "rustc --version | grep -qv '^rustc 1\\.98\\.1 '",
            "rustc --version | grep -q '^rustc 1\\.98\\.10 '",
            "rustc --version; echo \"expected rustc 1.98.1\"",
        ] {
            assert!(
                !step_verifies_rustc_version(bad, "1.98.1"),
                "非失败断言 `{bad}` 必须被工具链守卫拒绝"
            );
        }

        // 端到端 workflow 级反例：ci.yml 仅含 echo 提示必须判红。
        let tmp = TempDir::new("echo-rustc-guard");
        let wf_dir = tmp.path.join(".github").join("workflows");
        std::fs::create_dir_all(&wf_dir).unwrap();
        std::fs::write(
            tmp.path.join("rust-toolchain.toml"),
            "[toolchain]\nchannel = \"1.98.1\"\n",
        )
        .unwrap();
        std::fs::write(
            wf_dir.join("ci.yml"),
            "permissions:\n  contents: read\njobs:\n  lint:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: dtolnay/rust-toolchain@01e2821fc38dbca62c2557c927b33938a2e005fd # 1.98.1\n      - run: 'echo \"hint: rustc --version should be rustc 1.98.1\"'\n",
        )
        .unwrap();
        let problems = check_toolchain_pin_and_ci_sync(&tmp.path, &wf_dir);
        assert!(
            problems.iter().any(|p| p.contains("rustc --version")),
            "仅含 echo 提示的 workflow 必须触发版本校验缺失告警：{problems:?}"
        );
    }
}
