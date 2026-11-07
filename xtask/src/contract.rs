//! 支持契约的“保证索引”检查。
//!
//! `docs/support-contract.md` 里的每一行要么是**有测试兜底的保证**，要么明确标为
//! 待验证 / 声明 / 非目标。这个模块把“保证”与测试的对应关系变成可执行检查，
//! 避免文档悄悄引用一个不存在的测试。
//!
//! 只做一件事：解析表、校验状态与证据。不做代码生成，也不改 CI。

/// 索引里允许出现的四种状态。
pub const STATUSES: [&str; 4] = ["保证", "待验证", "声明", "非目标"];

/// `## 保证索引` 表格里的一行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub line: usize,
    pub guarantee: String,
    pub status: String,
    pub evidence: String,
}

/// 解析 `## 保证索引` 一节。返回的每一行都必须是四列。
pub fn parse_index(text: &str) -> Result<Vec<Row>, String> {
    let mut in_section = false;
    let mut header_seen = false;
    let mut in_fence = false;
    let mut rows = Vec::new();

    // 先扫描整篇：只要出现第二个 `## 保证索引` 就报错，即使中间隔着别的标题。
    // 解析循环在遇到任意标题时会结束本节，不能只靠循环里的相邻检查。
    let mut fenced = false;
    let mut sections = Vec::new();
    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.starts_with("```") || line.starts_with("~~~") {
            fenced = !fenced;
            continue;
        }
        if !fenced && line == "## 保证索引" {
            sections.push(index + 1);
        }
    }
    if sections.len() > 1 {
        return Err(format!("第 {} 行：重复的 `## 保证索引` 一节", sections[1]));
    }

    for (index, raw) in text.lines().enumerate() {
        let line_no = index + 1;
        let line = raw.trim();

        // 围栏在“找到真实标题”之前也要跟踪：标题前可能放一个格式示例，
        // 里面的 `## 保证索引` 不能提前把 `in_section` 置真。
        if line.starts_with("```") || line.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if !in_section {
            if !in_fence && line == "## 保证索引" {
                in_section = true;
            }
            continue;
        }
        if in_fence {
            continue;
        }

        if line == "## 保证索引" {
            return Err(format!("第 {line_no} 行：重复的 `## 保证索引` 一节"));
        }
        // 任何标题都结束本节：`###` 下的表不是保证索引。
        if line.starts_with('#') {
            break;
        }
        if !line.starts_with('|') {
            continue;
        }

        let cells = split_row(line);
        if !header_seen {
            let expected = ["保证", "状态", "证据", "备注"];
            if cells.len() != expected.len()
                || !cells
                    .iter()
                    .map(String::as_str)
                    .eq(expected.iter().copied())
            {
                return Err(format!(
                    "第 {line_no} 行：保证索引表头必须是 `| 保证 | 状态 | 证据 | 备注 |`，实际 {cells:?}"
                ));
            }
            header_seen = true;
            continue;
        }
        if cells
            .iter()
            .all(|cell| cell.is_empty() || is_separator(cell))
        {
            continue;
        }
        if cells.len() != 4 {
            return Err(format!(
                "第 {line_no} 行：保证索引必须是四列（保证 / 状态 / 证据 / 备注），实际 {} 列",
                cells.len()
            ));
        }
        rows.push(Row {
            line: line_no,
            guarantee: cells[0].clone(),
            status: cells[1].clone(),
            evidence: cells[2].clone(),
        });
    }

    if !in_section {
        return Err("缺少 `## 保证索引` 一节".to_string());
    }
    if !header_seen {
        return Err("保证索引缺少表头 `| 保证 | 状态 | 证据 | 备注 |`".to_string());
    }
    if rows.is_empty() {
        return Err("保证索引为空".to_string());
    }
    Ok(rows)
}

/// 按**未转义、代码跨度之外**的 `|` 切分一行表格。
///
/// - `\|` → 字面 `|`；
/// - 反引号内的 `|` 不切分（备注里写 `a | b` 很常见）；
/// - 只去掉首尾各一个空单元（表格边界），不用 `trim_matches`——
///   那会把 `|| a | b | c | d |` 的 5 列悄悄收成 4 列。
pub(crate) fn split_row(line: &str) -> Vec<String> {
    let mut cells = Vec::new();
    let mut current = String::new();
    let mut in_code = false;
    let mut chars = line.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '\\' if chars.peek() == Some(&'|') => {
                current.push('|');
                chars.next();
            }
            '`' => {
                in_code = !in_code;
                current.push(ch);
            }
            '|' if !in_code => {
                cells.push(current.trim().to_string());
                current.clear();
            }
            _ => current.push(ch),
        }
    }
    cells.push(current.trim().to_string());

    if cells.first().is_some_and(|cell| cell.is_empty()) {
        cells.remove(0);
    }
    if cells.last().is_some_and(|cell| cell.is_empty()) {
        cells.pop();
    }
    cells
}

/// 校验已经解析出来的行。
///
/// `exists(path, test_name)` 由调用方注入：生产用文件系统，测试用闭包。
pub fn validate_rows(rows: &[Row], exists: impl Fn(&str, &str) -> bool) -> Vec<String> {
    let mut violations = Vec::new();

    for row in rows {
        if !STATUSES.contains(&row.status.as_str()) {
            violations.push(format!(
                "第 {} 行：未知状态 \"{}\"（允许：{}）",
                row.line,
                row.status,
                STATUSES.join(" / ")
            ));
        }
        if row.guarantee.trim().is_empty() {
            violations.push(format!("第 {} 行：保证文本为空", row.line));
        }

        let references = backticked(&row.evidence);
        if row.status == "保证" && references.is_empty() {
            violations.push(format!(
                "第 {} 行：状态为“保证”但没有证据；请给出 `path::test`，或改成待验证 / 声明 / 非目标",
                row.line
            ));
        }
        for reference in references {
            // 非“保证”行允许引用文档/示例路径；只有形如 `path::test` 的证据才校验。
            if row.status != "保证" && !reference.contains("::") {
                continue;
            }
            let Some((path, name)) = reference.rsplit_once("::") else {
                violations.push(format!(
                    "第 {} 行：证据 `{reference}` 必须是 `path::test` 形式",
                    row.line
                ));
                continue;
            };
            if !exists(path, name) {
                violations.push(format!(
                    "第 {} 行：证据指向不存在的测试 `{reference}`",
                    row.line
                ));
            }
        }
    }

    violations
}

/// 生产入口：解析 + 校验，返回所有违规。空 = 通过。
pub fn check_contract(text: &str) -> Vec<String> {
    match parse_index(text) {
        Ok(rows) => validate_rows(&rows, test_exists),
        Err(err) => vec![err],
    }
}

/// 支持契约的默认路径（与 `cargo xtask` 的当前目录无关）。
pub fn default_contract_path() -> std::path::PathBuf {
    repo_root().join("docs").join("support-contract.md")
}

/// 读取并检查一份契约文档；返回违规（空 = 通过）。
pub fn check_path(path: &std::path::Path) -> Vec<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => check_contract(&text),
        Err(err) => vec![format!("读取 {} 失败: {err}", path.display())],
    }
}

fn is_separator(cell: &str) -> bool {
    !cell.is_empty() && cell.chars().all(|ch| ch == '-' || ch == ':')
}

/// 取出反引号包住的片段：证据列只允许 `path::test`。
fn backticked(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find('`') {
        rest = &rest[start + 1..];
        let Some(end) = rest.find('`') else {
            break;
        };
        out.push(&rest[..end]);
        rest = &rest[end + 1..];
    }
    out
}

/// 仓库根目录。`cargo xtask` 可能在任意子目录里跑，不能依赖 CWD。
fn repo_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask 必须位于仓库子目录")
        .to_path_buf()
}

/// 测试是否存在：文件可读，且目标函数带 `#[test]` / `#[tokio::test]`。
fn test_exists(path: &str, name: &str) -> bool {
    test_exists_at(&repo_root(), path, name)
}

/// 可注入仓库根的版本，便于直接测试。
///
/// 当前实现只做文本包含；红档用例会要求它同时满足：
/// 目标行的 `fn` 必须是**测试函数**（带 `#[test]` / `#[tokio::test]`），
/// 不能是注释、`#[ignore]` 或普通 helper。
pub fn test_exists_at(root: &std::path::Path, path: &str, name: &str) -> bool {
    let normalized = path.replace('\\', "/");
    if normalized.is_empty() || normalized.starts_with('/') || normalized.contains(':') {
        return false;
    }
    let candidate = std::path::Path::new(&normalized);
    if candidate.components().any(|component| {
        matches!(
            component,
            std::path::Component::ParentDir
                | std::path::Component::RootDir
                | std::path::Component::Prefix(_)
        )
    }) {
        return false;
    }

    let Ok(text) = std::fs::read_to_string(root.join(candidate)) else {
        return false;
    };
    let lines: Vec<&str> = text.lines().collect();
    let sync = format!("fn {name}(");
    let asynchronous = format!("async fn {name}(");

    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        if !(trimmed.starts_with(&sync) || trimmed.starts_with(&asynchronous)) {
            continue;
        }

        // 只认**紧邻**的测试属性。空行、普通语句都会终止回溯，
        // 避免把上一个 item 的 `#[test]` 借给一个 helper。
        let mut saw_test = false;
        let mut saw_ignore = false;
        for previous in lines[..index].iter().rev().take(8) {
            let previous = previous.trim();
            if previous.is_empty() {
                break;
            }
            if previous.starts_with("///") || previous.starts_with("//") {
                continue;
            }
            if previous.starts_with("#[") {
                if previous == "#[test]"
                    || previous.starts_with("#[test(")
                    || previous.contains("::test")
                {
                    saw_test = true;
                }
                if previous == "#[ignore]" || previous.starts_with("#[ignore(") {
                    saw_ignore = true;
                }
                continue;
            }
            break;
        }
        if saw_test && !saw_ignore {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn index(rows: &str) -> String {
        format!("## 保证索引\n\n| 保证 | 状态 | 证据 | 备注 |\n|---|---|---|---|\n{rows}\n")
    }

    fn row(guarantee: &str, status: &str, evidence: &str) -> String {
        format!("| {guarantee} | {status} | {evidence} | — |")
    }

    fn fixture(tag: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("cordis-contract-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/lib.rs"),
            r#"
#[test]
fn real_test() {}

fn helper() {}

#[ignore]
#[test]
fn ignored_test() {}

// fn commented_test() {}

#[tokio::test]
async fn async_test() {}
"#,
        )
        .unwrap();
        std::fs::write(root.join("outside.rs"), "#[test]\nfn real_test() {}\n").unwrap();
        root
    }

    #[test]
    fn parses_four_columns() {
        let text = index(&row("A", "保证", "`p.rs::t`"));
        let rows = parse_index(&text).expect("表应当可解析");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].guarantee, "A");
        assert_eq!(rows[0].status, "保证");
        assert_eq!(rows[0].evidence, "`p.rs::t`");
    }

    #[test]
    fn rejects_a_missing_section() {
        assert!(parse_index("没有索引").is_err());
    }

    #[test]
    fn rejects_an_empty_index() {
        let text = "## 保证索引\n\n| 保证 | 状态 | 证据 | 备注 |\n|---|---|---|---|\n";
        let err = parse_index(text).unwrap_err();
        assert!(err.contains("空"), "{err}");
    }

    #[test]
    fn rejects_a_malformed_header() {
        let text =
            "## 保证索引\n\n| 保证 | 状态 | 证据 |\n|---|---|---|\n| A | 保证 | `p.rs::t` |\n";
        let err = parse_index(text).unwrap_err();
        assert!(err.contains("表头"), "{err}");
    }

    #[test]
    fn rejects_wrong_column_count() {
        let text = index(&format!(
            "{}\n| 只有三列 | 保证 | `p.rs::t` |",
            row("A", "保证", "`p.rs::t`")
        ));
        let err = parse_index(&text).unwrap_err();
        assert!(err.contains("四列"), "{err}");
    }

    #[test]
    fn keeps_a_data_row_whose_guarantee_is_the_word_baozheng() {
        let text = index(&row("保证", "保证", "`p.rs::t`"));
        let rows = parse_index(&text).unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
    }

    #[test]
    fn ignores_tables_inside_code_fences() {
        let text = "## 保证索引\n\n| 保证 | 状态 | 证据 | 备注 |\n|---|---|---|---|\n\
                    | A | 保证 | `p.rs::t` | — |\n\n```md\n| B | 待验证 | — | — |\n```\n";
        let rows = parse_index(text).unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
    }

    #[test]
    fn fenced_example_before_the_real_section_is_ignored() {
        let text = "```md\n## 保证索引\n\n| 保证 | 状态 | 证据 | 备注 |\n|---|---|---|---|\n\
                    | EXAMPLE | 保证 | `p.rs::t` | — |\n```\n\n\
                    ## 保证索引\n\n| 保证 | 状态 | 证据 | 备注 |\n|---|---|---|---|\n\
                    | A | 保证 | `p.rs::t` | — |\n";
        let rows = parse_index(text).unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].guarantee, "A");
    }

    #[test]
    fn accepts_escaped_pipes_and_pipes_inside_code() {
        let text = index("| a \\| b | 保证 | `p.rs::t` | 见 `x | y` |");
        let rows = parse_index(&text).unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].guarantee, "a | b");
        assert_eq!(rows[0].evidence, "`p.rs::t`");
    }

    #[test]
    fn rejects_a_duplicate_section() {
        let text = format!(
            "{}{}",
            index(&row("A", "保证", "`p.rs::t`")),
            "## 保证索引\n"
        );
        let err = parse_index(&text).unwrap_err();
        assert!(err.contains("重复"), "{err}");
    }

    #[test]
    fn rejects_a_duplicate_section_after_another_heading() {
        let text = format!(
            "{}## 其他\n\n## 保证索引\n",
            index(&row("A", "保证", "`p.rs::t`"))
        );
        let err = parse_index(&text).unwrap_err();
        assert!(err.contains("重复"), "{err}");
    }

    #[test]
    fn rejects_a_guarantee_without_evidence() {
        let rows = parse_index(&index(&row("A", "保证", "—"))).unwrap();
        let violations = validate_rows(&rows, |_, _| true);
        assert_eq!(violations.len(), 1, "{violations:?}");
        assert!(violations[0].contains("没有证据"), "{violations:?}");
    }

    #[test]
    fn rejects_a_missing_test() {
        let rows = parse_index(&index(&row("A", "保证", "`p.rs::t`"))).unwrap();
        let violations = validate_rows(&rows, |_, _| false);
        assert_eq!(violations.len(), 1, "{violations:?}");
        assert!(violations[0].contains("不存在的测试"), "{violations:?}");
    }

    #[test]
    fn rejects_an_unknown_status() {
        let rows = parse_index(&index(&row("A", "大概", "—"))).unwrap();
        let violations = validate_rows(&rows, |_, _| true);
        assert_eq!(violations.len(), 1, "{violations:?}");
        assert!(violations[0].contains("大概"), "{violations:?}");
    }

    #[test]
    fn accepts_a_non_guarantee_with_a_path_reference() {
        let rows = parse_index(&index("| A | 非目标 | `docs/x.md` | — |")).unwrap();
        assert!(validate_rows(&rows, |_, _| true).is_empty());
    }

    #[test]
    fn test_exists_requires_a_real_test() {
        let root = fixture("exists");
        assert!(test_exists_at(&root, "src/lib.rs", "real_test"));
        assert!(!test_exists_at(&root, "src/lib.rs", "helper"));
        assert!(!test_exists_at(&root, "src/lib.rs", "ignored_test"));
        assert!(!test_exists_at(&root, "src/lib.rs", "commented_test"));
        assert!(test_exists_at(&root, "src/lib.rs", "async_test"));
        assert!(!test_exists_at(&root, "src/lib.rs", "missing"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn test_exists_rejects_paths_outside_the_repo() {
        let root = fixture("escape");
        assert!(!test_exists_at(&root, "../outside.rs", "real_test"));
        assert!(!test_exists_at(&root, "src/../../outside.rs", "real_test"));
        let absolute = root.join("outside.rs");
        assert!(!test_exists_at(
            &root,
            absolute.to_str().unwrap(),
            "real_test"
        ));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn normalizes_windows_separators() {
        let root = fixture("separators");
        assert!(test_exists_at(&root, "src\\lib.rs", "real_test"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn checks_the_real_support_contract() {
        let text = include_str!("../../docs/support-contract.md");
        let rows = parse_index(text).expect("支持契约必须包含保证索引");
        assert_eq!(rows.len(), 26, "索引行数变化必须是一次有意的编辑");
        assert_eq!(
            rows.iter().filter(|row| row.status == "保证").count(),
            17,
            "保证数量的变化必须是一次有意的编辑"
        );
        let panic = rows
            .iter()
            .find(|row| row.guarantee.contains("panic"))
            .expect("panic 行必须存在");
        assert_eq!(panic.status, "保证");
        assert!(panic
            .evidence
            .contains("plugin_panics_become_status_err_at_the_abi_boundary"));
        assert_eq!(check_contract(text), Vec::<String>::new());
    }
}
