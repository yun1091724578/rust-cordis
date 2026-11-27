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
use std::collections::{BTreeMap, BTreeSet, VecDeque};
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

enum YamlScalarLookup<'a> {
    Missing,
    Str(&'a str),
    NonString(&'a YamlValue),
    DuplicateCaseKeys,
}

fn yaml_str<'a>(map: &'a serde_yaml::Mapping, key: &str) -> Option<&'a str> {
    map.get(YamlValue::String(key.to_string()))
        .and_then(YamlValue::as_str)
}

fn yaml_lookup_str<'a>(map: &'a serde_yaml::Mapping, key: &str) -> YamlScalarLookup<'a> {
    match map.get(YamlValue::String(key.to_string())) {
        None => YamlScalarLookup::Missing,
        Some(YamlValue::String(s)) => YamlScalarLookup::Str(s.as_str()),
        Some(other) => YamlScalarLookup::NonString(other),
    }
}

/// 大小写不敏感查找 YAML `env` 映射中的变量（防止 Windows runner 上 `rustup_toolchain` 静默覆盖）。
/// 若同一 `env` 映射中存在多个仅大小写不同的同名键，fail-closed 返回 `DuplicateCaseKeys`。
fn yaml_lookup_env_var<'a>(map: &'a serde_yaml::Mapping, var: &str) -> YamlScalarLookup<'a> {
    let Some(env_val) = map.get(YamlValue::String("env".to_string())) else {
        return YamlScalarLookup::Missing;
    };
    let Some(env_map) = env_val.as_mapping() else {
        return YamlScalarLookup::NonString(env_val);
    };
    let mut matched: Option<&'a YamlValue> = None;
    for (k, v) in env_map {
        if k.as_str().is_some_and(|s| s.eq_ignore_ascii_case(var)) && matched.replace(v).is_some() {
            return YamlScalarLookup::DuplicateCaseKeys;
        }
    }
    match matched {
        None => YamlScalarLookup::Missing,
        Some(YamlValue::String(s)) => YamlScalarLookup::Str(s.as_str()),
        Some(other) => YamlScalarLookup::NonString(other),
    }
}

/// 判断 workflow 是否由主 CI / 合并队列 / 可复用 CI 门禁触发（不因改名、merge_group 或
/// reusable workflow 而绕过工具链校验守卫）。
fn workflow_triggers_on_push_or_pr(root_map: &serde_yaml::Mapping) -> bool {
    let on_val = root_map
        .get(YamlValue::String("on".to_string()))
        .or_else(|| root_map.get(YamlValue::Bool(true)));
    let is_ci_event = |s: &str| {
        matches!(
            s,
            "push" | "pull_request" | "pull_request_target" | "merge_group" | "workflow_call"
        )
    };
    match on_val {
        Some(YamlValue::String(s)) => is_ci_event(s.trim()),
        Some(YamlValue::Sequence(seq)) => seq
            .iter()
            .filter_map(YamlValue::as_str)
            .any(|s| is_ci_event(s.trim())),
        Some(YamlValue::Mapping(map)) => map
            .keys()
            .filter_map(YamlValue::as_str)
            .any(|s| is_ci_event(s.trim())),
        _ => false,
    }
}

fn is_standard_ci_shell(shell: &str) -> bool {
    matches!(shell.trim(), "bash" | "sh" | "pwsh" | "cmd")
}

/// 防止 workflow/job 通过 `defaults.run.shell: "true {0}"` 静默吞掉规范步骤。
fn defaults_shell_is_safe(map: &serde_yaml::Mapping) -> bool {
    let Some(defaults) = map.get(YamlValue::String("defaults".to_string())) else {
        return true;
    };
    let Some(defaults_map) = defaults.as_mapping() else {
        return false;
    };
    let Some(run_val) = defaults_map.get(YamlValue::String("run".to_string())) else {
        return true;
    };
    let Some(run_map) = run_val.as_mapping() else {
        return false;
    };
    match run_map.get(YamlValue::String("shell".to_string())) {
        None => true,
        Some(YamlValue::String(s)) => is_standard_ci_shell(s),
        Some(_) => false,
    }
}

fn step_shell_is_safe(step_map: &serde_yaml::Mapping) -> bool {
    match step_map.get(YamlValue::String("shell".to_string())) {
        None => true,
        Some(YamlValue::String(s)) => is_standard_ci_shell(s),
        Some(_) => false,
    }
}

/// 规范单行工具链校验契约：CI job 必须通过 `cargo xtask verify-toolchain <expected_version>`
/// 在运行期执行真实版本断言，彻底消除对任意 bash 脚本做静态启发式分析的猫鼠游戏。
fn step_is_canonical_toolchain_verify(
    step_map: &serde_yaml::Mapping,
    expected_version: &str,
) -> bool {
    if !step_is_unconditional_and_blocking(step_map) || !step_shell_is_safe(step_map) {
        return false;
    }
    let Some(run_script) = yaml_str(step_map, "run") else {
        return false;
    };
    let trimmed = run_script.trim();
    if trimmed.contains('\n') || trimmed.contains('\r') {
        return false;
    }
    trimmed == format!("cargo xtask verify-toolchain {expected_version}")
        || trimmed == format!("cargo xtask verify-toolchain --expected {expected_version}")
}

fn yaml_is_explicit_false(val: &YamlValue) -> bool {
    matches!(val, YamlValue::Bool(false))
}

fn yaml_is_const_false_condition(val: &YamlValue) -> bool {
    match val {
        YamlValue::Bool(false) => true,
        YamlValue::String(s) => {
            let trimmed = s.trim();
            let inner = trimmed
                .strip_prefix("${{")
                .and_then(|rest| rest.strip_suffix("}}"))
                .map(str::trim)
                .unwrap_or(trimmed);
            inner.eq_ignore_ascii_case("false")
        }
        _ => false,
    }
}

fn job_is_blocking_ci(job_map: &serde_yaml::Mapping) -> bool {
    if let Some(cond) = job_map.get(YamlValue::String("if".to_string())) {
        if yaml_is_const_false_condition(cond) {
            return false;
        }
    }
    job_map
        .get(YamlValue::String("continue-on-error".to_string()))
        .map(yaml_is_explicit_false)
        .unwrap_or(true)
}

fn step_is_unconditional_and_blocking(step_map: &serde_yaml::Mapping) -> bool {
    // 任何 `if:` 条件（如 `if: false` / `if: ${{ false }}` / 仅限部分事件或平台）
    // 都可能跳过版本校验，因此一律不计入阻断 CI 的版本断言（fail closed）。
    if step_map.contains_key(YamlValue::String("if".to_string())) {
        return false;
    }
    // `continue-on-error` 缺失或显式为布尔 `false` 才可阻断 CI；
    // `true`、`${{ true }}` 或任何表达式都可能放行失败。
    if let Some(coe) = step_map.get(YamlValue::String("continue-on-error".to_string())) {
        if !yaml_is_explicit_false(coe) {
            return false;
        }
    }
    true
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
        let mut scanned_comments: BTreeMap<String, VecDeque<String>> = BTreeMap::new();
        for (line_no, action_ref, comment) in &scanned {
            if !action_ref.starts_with("dtolnay/rust-toolchain@") {
                continue;
            }
            scanned_comments
                .entry(action_ref.clone())
                .or_default()
                .push_back(comment.clone());
            if !allowed_ci_toolchains.contains(comment) {
                problems.push(format!(
                    "{name}:{line_no}: dtolnay/rust-toolchain 尾注 `{comment}` 必须与 rust-toolchain.toml (`{pinned_channel}`) 或 workspace rust-version ({msrv_version:?}) 一致"
                ));
            }
        }

        let Some(root_map) = doc.as_mapping() else {
            continue;
        };
        let root_shell_safe = defaults_shell_is_safe(root_map);
        // Rustup 优先级：env.RUSTUP_TOOLCHAIN 高于 rust-toolchain.toml 与 rustup default。
        // 顶层 env 若声明 RUSTUP_TOOLCHAIN，不得偏离 pinned_channel，且禁止非字符串/大小写重复键绕过。
        match yaml_lookup_env_var(root_map, "RUSTUP_TOOLCHAIN") {
            YamlScalarLookup::Missing => {}
            YamlScalarLookup::Str(top_tc) => {
                if top_tc != pinned_channel {
                    problems.push(format!(
                        "{name}: workflow 顶层 `env.RUSTUP_TOOLCHAIN` 为 `{top_tc}`，会覆盖精确锁版 `{pinned_channel}`"
                    ));
                }
            }
            YamlScalarLookup::NonString(val) => {
                problems.push(format!(
                    "{name}: workflow 顶层 `env.RUSTUP_TOOLCHAIN` 必须是字符串，实际为非字符串 YAML 值 `{val:?}`（fail closed）"
                ));
            }
            YamlScalarLookup::DuplicateCaseKeys => {
                problems.push(format!(
                    "{name}: workflow 顶层 `env` 中存在多个大小写不同的 `RUSTUP_TOOLCHAIN` 键（fail closed）"
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
                            let comment = scanned_comments
                                .get_mut(uses_val)
                                .and_then(VecDeque::pop_front)
                                .unwrap_or_default();
                            if !comment.is_empty() {
                                job_toolchain_from_action = Some(comment.clone());
                            }
                            if let Some(with_val) =
                                step_map.get(YamlValue::String("with".to_string()))
                            {
                                let Some(with_map) = with_val.as_mapping() else {
                                    problems.push(format!(
                                        "{name}:{job_name}: dtolnay/rust-toolchain 的 `with` 必须是映射，实际为 `{with_val:?}`（fail closed）"
                                    ));
                                    continue;
                                };
                                match yaml_lookup_str(with_map, "toolchain") {
                                    YamlScalarLookup::Missing => {}
                                    YamlScalarLookup::Str(with_tc) => {
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
                                    YamlScalarLookup::NonString(val) => {
                                        problems.push(format!(
                                            "{name}:{job_name}: dtolnay/rust-toolchain 的 `with.toolchain` 必须是字符串，实际为非字符串 YAML 值 `{val:?}`（fail closed）"
                                        ));
                                    }
                                    YamlScalarLookup::DuplicateCaseKeys => {}
                                }
                            }
                        }
                    }
                }

                let expected_job_tc = job_toolchain_from_action
                    .as_deref()
                    .unwrap_or(pinned_channel.as_str());

                match yaml_lookup_env_var(job_map, "RUSTUP_TOOLCHAIN") {
                    YamlScalarLookup::Str(job_env_tc) => {
                        if job_env_tc != expected_job_tc {
                            problems.push(format!(
                                "{name}:{job_name}: job 级 `env.RUSTUP_TOOLCHAIN` 为 `{job_env_tc}`，与预期工具链 `{expected_job_tc}` 不一致"
                            ));
                        }
                    }
                    YamlScalarLookup::NonString(val) => {
                        problems.push(format!(
                            "{name}:{job_name}: job 级 `env.RUSTUP_TOOLCHAIN` 必须是字符串，实际为非字符串 YAML 值 `{val:?}`（fail closed）"
                        ));
                    }
                    YamlScalarLookup::DuplicateCaseKeys => {
                        problems.push(format!(
                            "{name}:{job_name}: job 级 `env` 中存在多个大小写不同的 `RUSTUP_TOOLCHAIN` 键（fail closed）"
                        ));
                    }
                    YamlScalarLookup::Missing => {
                        if expected_job_tc != pinned_channel {
                            problems.push(format!(
                                "{name}:{job_name}: 使用非默认工具链 `{expected_job_tc}` 的 job 必须设置 `env.RUSTUP_TOOLCHAIN: {expected_job_tc}`，防止被根目录 rust-toolchain.toml (`{pinned_channel}`) 静默覆盖"
                            ));
                        }
                    }
                }

                let job_shell_safe = root_shell_safe && defaults_shell_is_safe(job_map);

                for step in steps {
                    let Some(step_map) = step.as_mapping() else {
                        continue;
                    };
                    match yaml_lookup_env_var(step_map, "RUSTUP_TOOLCHAIN") {
                        YamlScalarLookup::Missing => {}
                        YamlScalarLookup::Str(step_env_tc) => {
                            if step_env_tc != expected_job_tc {
                                problems.push(format!(
                                    "{name}:{job_name}: step 级 `env.RUSTUP_TOOLCHAIN` 为 `{step_env_tc}`，会覆盖预期工具链 `{expected_job_tc}`"
                                ));
                            }
                        }
                        YamlScalarLookup::NonString(val) => {
                            problems.push(format!(
                                "{name}:{job_name}: step 级 `env.RUSTUP_TOOLCHAIN` 必须是字符串，实际为非字符串 YAML 值 `{val:?}`（fail closed）"
                            ));
                        }
                        YamlScalarLookup::DuplicateCaseKeys => {
                            problems.push(format!(
                                "{name}:{job_name}: step 级 `env` 中存在多个大小写不同的 `RUSTUP_TOOLCHAIN` 键（fail closed）"
                            ));
                        }
                    }
                    if job_shell_safe
                        && job_is_blocking_ci(job_map)
                        && step_is_canonical_toolchain_verify(step_map, expected_job_tc)
                    {
                        has_rustc_version_check = true;
                    }
                }

                let requires_rustc_check =
                    name == "ci.yml" || workflow_triggers_on_push_or_pr(root_map);
                if requires_rustc_check
                    && job_toolchain_from_action.is_some()
                    && !has_rustc_version_check
                {
                    problems.push(format!(
                        "{name}:{job_name}: CI workflow 中安装 Rust 工具链的 job 必须包含无条件阻断的规范校验步骤 `cargo xtask verify-toolchain {expected_job_tc}`"
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
            problems3
                .iter()
                .any(|p| p.contains("verify-toolchain 1.98.1")),
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
    fn r5_canonical_toolchain_verify_contract_replaces_shell_heuristics() {
        let make_step = |yaml: &str| -> serde_yaml::Mapping {
            serde_yaml::from_str::<YamlValue>(yaml)
                .unwrap()
                .as_mapping()
                .unwrap()
                .clone()
        };

        // 1. Only exact single-line canonical `cargo xtask verify-toolchain <ver>` passes.
        assert!(step_is_canonical_toolchain_verify(
            &make_step("run: cargo xtask verify-toolchain 1.98.1\n"),
            "1.98.1"
        ));
        assert!(step_is_canonical_toolchain_verify(
            &make_step("run: cargo xtask verify-toolchain --expected 1.98.1\n"),
            "1.98.1"
        ));
        assert!(step_is_canonical_toolchain_verify(
            &make_step(
                "continue-on-error: false\nshell: bash\nrun: cargo xtask verify-toolchain 1.98.1\n"
            ),
            "1.98.1"
        ));

        // 2. Arbitrary bash scripts, pipelines, shell bypasses, custom shells, wrong versions,
        //    and conditional/non-blocking steps are ALL structurally rejected.
        for bad_step_yaml in [
            "run: \"rustc --version | grep -q '^rustc 1\\\\.98\\\\.1 '\"\n",
            "run: cargo xtask verify-toolchain 1.98.1 || true\n",
            "run: cargo xtask verify-toolchain 1.98.1; exit 0\n",
            "run: cargo xtask verify-toolchain 1.98.1 &\n",
            "run: cargo xtask verify-toolchain 1.98.10\n",
            "run: |\n  set +e\n  cargo xtask verify-toolchain 1.98.1\n",
            "run: |\n  if false; then\n    cargo xtask verify-toolchain 1.98.1\n  fi\n",
            "shell: true {0}\nrun: cargo xtask verify-toolchain 1.98.1\n",
            "if: false\nrun: cargo xtask verify-toolchain 1.98.1\n",
            "continue-on-error: true\nrun: cargo xtask verify-toolchain 1.98.1\n",
        ] {
            assert!(
                !step_is_canonical_toolchain_verify(&make_step(bad_step_yaml), "1.98.1"),
                "非规范步骤必须被拒绝:\n{bad_step_yaml}"
            );
        }

        // 3. End-to-end workflow checks: non-blocking job/step, custom defaults.run.shell,
        //    case-insensitive `rustup_toolchain`, and renamed/merge_group/workflow_call triggers.
        for (label, job_extra, step_extra, wf_extra) in [
            ("step-if-false", "", "        if: false\n", ""),
            ("step-if-expr-false", "", "        if: ${{ false }}\n", ""),
            (
                "step-if-cond",
                "",
                "        if: github.event_name == 'release'\n",
                "",
            ),
            ("step-coe-true", "", "        continue-on-error: true\n", ""),
            (
                "step-coe-expr",
                "",
                "        continue-on-error: ${{ true }}\n",
                "",
            ),
            ("job-coe-true", "    continue-on-error: true\n", "", ""),
            ("job-if-false", "    if: false\n", "", ""),
            (
                "custom-defaults-shell",
                "    defaults:\n      run:\n        shell: true {0}\n",
                "",
                "",
            ),
            (
                "root-defaults-shell",
                "",
                "",
                "defaults:\n  run:\n    shell: true {0}\n",
            ),
        ] {
            let tmp_nb = TempDir::new(label);
            let wf_nb = tmp_nb.path.join(".github").join("workflows");
            std::fs::create_dir_all(&wf_nb).unwrap();
            std::fs::write(
                tmp_nb.path.join("rust-toolchain.toml"),
                "[toolchain]\nchannel = \"1.98.1\"\n",
            )
            .unwrap();
            let yaml = format!(
                "permissions:\n  contents: read\n{wf_extra}jobs:\n  lint:\n    runs-on: ubuntu-latest\n{job_extra}    steps:\n      - uses: dtolnay/rust-toolchain@01e2821fc38dbca62c2557c927b33938a2e005fd # 1.98.1\n      - run: cargo xtask verify-toolchain 1.98.1\n{step_extra}"
            );
            std::fs::write(wf_nb.join("ci.yml"), yaml).unwrap();
            let nb_problems = check_toolchain_pin_and_ci_sync(&tmp_nb.path, &wf_nb);
            assert!(
                nb_problems
                    .iter()
                    .any(|p| p.contains("verify-toolchain 1.98.1")),
                "非阻断/自定义 shell 夹具 `{label}` 必须被拒绝：{nb_problems:?}"
            );
        }

        // 4. Explicit `continue-on-error: false` with canonical verify-toolchain passes.
        let tmp_ok = TempDir::new("step-coe-false");
        let wf_ok = tmp_ok.path.join(".github").join("workflows");
        std::fs::create_dir_all(&wf_ok).unwrap();
        std::fs::write(
            tmp_ok.path.join("rust-toolchain.toml"),
            "[toolchain]\nchannel = \"1.98.1\"\n",
        )
        .unwrap();
        std::fs::write(
            wf_ok.join("ci.yml"),
            "permissions:\n  contents: read\njobs:\n  lint:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: dtolnay/rust-toolchain@01e2821fc38dbca62c2557c927b33938a2e005fd # 1.98.1\n      - continue-on-error: false\n        run: cargo xtask verify-toolchain 1.98.1\n",
        )
        .unwrap();
        let ok_problems = check_toolchain_pin_and_ci_sync(&tmp_ok.path, &wf_ok);
        assert!(
            ok_problems.is_empty(),
            "显式 continue-on-error: false 的规范步骤应放行：{ok_problems:?}"
        );

        // 5. Renamed CI workflows (push/pull_request/merge_group/workflow_call) enforce the contract;
        //    manual workflow_dispatch does not.
        for (wf_name, trigger) in [
            ("rust-ci.yml", "on: [push, pull_request]"),
            ("merge-group.yml", "on: merge_group"),
            ("reusable.yml", "on: workflow_call"),
        ] {
            let tmp = TempDir::new(wf_name);
            let wf_dir = tmp.path.join(".github").join("workflows");
            std::fs::create_dir_all(&wf_dir).unwrap();
            std::fs::write(
                tmp.path.join("rust-toolchain.toml"),
                "[toolchain]\nchannel = \"1.98.1\"\n",
            )
            .unwrap();
            std::fs::write(
                wf_dir.join(wf_name),
                format!("{trigger}\npermissions:\n  contents: read\njobs:\n  check:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: dtolnay/rust-toolchain@01e2821fc38dbca62c2557c927b33938a2e005fd # 1.98.1\n      - run: cargo test\n"),
            )
            .unwrap();
            let problems = check_toolchain_pin_and_ci_sync(&tmp.path, &wf_dir);
            assert!(
                problems
                    .iter()
                    .any(|p| p.contains(wf_name) && p.contains("verify-toolchain 1.98.1")),
                "{trigger} workflow 必须要求规范 verify-toolchain 步骤：{problems:?}"
            );
        }

        // 6. Case-insensitive `rustup_toolchain` and duplicate-SHA different-comment jobs are caught.
        let tmp_ci = TempDir::new("case-and-dup-sha");
        let wf_ci = tmp_ci.path.join(".github").join("workflows");
        std::fs::create_dir_all(&wf_ci).unwrap();
        std::fs::write(
            tmp_ci.path.join("rust-toolchain.toml"),
            "[toolchain]\nchannel = \"1.98.1\"\n",
        )
        .unwrap();
        std::fs::write(
            tmp_ci.path.join("Cargo.toml"),
            "[workspace.package]\nrust-version = \"1.85\"\n",
        )
        .unwrap();
        std::fs::write(
            wf_ci.join("ci.yml"),
            "permissions:\n  contents: read\nenv:\n  rustup_toolchain: stable\njobs:\n  first:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: dtolnay/rust-toolchain@01e2821fc38dbca62c2557c927b33938a2e005fd # 1.98.1\n      - run: cargo xtask verify-toolchain 1.98.1\n  second:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: dtolnay/rust-toolchain@01e2821fc38dbca62c2557c927b33938a2e005fd # 1.85.0\n      - run: cargo xtask verify-toolchain 1.98.1\n",
        )
        .unwrap();
        let ci_problems = check_toolchain_pin_and_ci_sync(&tmp_ci.path, &wf_ci);
        assert!(
            ci_problems
                .iter()
                .any(|p| p.contains("顶层 `env.RUSTUP_TOOLCHAIN`") && p.contains("stable")),
            "小写 rustup_toolchain 覆写必须被拦截：{ci_problems:?}"
        );
        assert!(
            ci_problems
                .iter()
                .any(|p| p.contains("ci.yml:second") && p.contains("1.85.0")),
            "重复 SHA 的第二个 job 必须匹配其自身尾注 `1.85.0` 而非复用首个 job 尾注：{ci_problems:?}"
        );
    }

    #[test]
    fn r4_p1_toolchain_guard_fails_closed_on_non_string_yaml_scalars() {
        // Non-string YAML scalars (e.g. float `1.90`, bool `true`, `null`) must NEVER be
        // silently ignored by `.as_str()` when checking RUSTUP_TOOLCHAIN or `with.toolchain`.
        let cases = [
            (
                "top-env-float",
                "env:\n  RUSTUP_TOOLCHAIN: 1.90\njobs:\n  lint:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: dtolnay/rust-toolchain@01e2821fc38dbca62c2557c927b33938a2e005fd # 1.98.1\n      - run: cargo xtask verify-toolchain 1.98.1\n",
            ),
            (
                "top-env-bool",
                "env:\n  RUSTUP_TOOLCHAIN: true\njobs:\n  lint:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: dtolnay/rust-toolchain@01e2821fc38dbca62c2557c927b33938a2e005fd # 1.98.1\n      - run: cargo xtask verify-toolchain 1.98.1\n",
            ),
            (
                "job-env-float",
                "jobs:\n  lint:\n    runs-on: ubuntu-latest\n    env:\n      RUSTUP_TOOLCHAIN: 1.90\n    steps:\n      - uses: dtolnay/rust-toolchain@01e2821fc38dbca62c2557c927b33938a2e005fd # 1.98.1\n      - run: cargo xtask verify-toolchain 1.98.1\n",
            ),
            (
                "step-env-null",
                "jobs:\n  lint:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: dtolnay/rust-toolchain@01e2821fc38dbca62c2557c927b33938a2e005fd # 1.98.1\n      - env:\n          RUSTUP_TOOLCHAIN: null\n        run: cargo xtask verify-toolchain 1.98.1\n",
            ),
            (
                "with-toolchain-float",
                "jobs:\n  lint:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: dtolnay/rust-toolchain@01e2821fc38dbca62c2557c927b33938a2e005fd # 1.98.1\n        with:\n          toolchain: 1.90\n      - run: cargo xtask verify-toolchain 1.98.1\n",
            ),
            (
                "with-toolchain-bool",
                "jobs:\n  lint:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: dtolnay/rust-toolchain@01e2821fc38dbca62c2557c927b33938a2e005fd # 1.98.1\n        with:\n          toolchain: true\n      - run: cargo xtask verify-toolchain 1.98.1\n",
            ),
        ];

        for (label, body) in cases {
            let tmp = TempDir::new(label);
            let wf_dir = tmp.path.join(".github").join("workflows");
            std::fs::create_dir_all(&wf_dir).unwrap();
            std::fs::write(
                tmp.path.join("rust-toolchain.toml"),
                "[toolchain]\nchannel = \"1.98.1\"\n",
            )
            .unwrap();
            std::fs::write(
                wf_dir.join("ci.yml"),
                format!("permissions:\n  contents: read\n{body}"),
            )
            .unwrap();
            let problems = check_toolchain_pin_and_ci_sync(&tmp.path, &wf_dir);
            assert!(
                !problems.is_empty(),
                "非字符串 YAML 标量夹具 `{label}` 必须 fail-closed 报错，实际放行"
            );
        }
    }
}
