//! 保证 × 平台矩阵的结构检查。
//!
//! 它**不**证明某个平台真的通过；只保证：
//! - 支持契约里的每一条“保证”在矩阵里恰好有一行；
//! - 三个平台列都是 `通过` / `失败` / `待验证`；
//! - 证据列非空。
//!
//! 真实结果仍由平台 CI / 手工执行填写，禁止把“待验证”当成保证。

use std::collections::HashSet;
use std::path::Path;

const SECTION: &str = "## 平台矩阵";
const PLATFORMS: [&str; 4] = ["Windows GNU", "Windows MSVC", "Linux", "macOS"];
const HEADER: [&str; 6] = [
    "保证",
    "Windows GNU",
    "Windows MSVC",
    "Linux",
    "macOS",
    "证据",
];
const STATUSES: [&str; 3] = ["通过", "失败", "待验证"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub guarantee: String,
    pub platforms: [String; 4],
    pub evidence: String,
}

pub fn default_support_path() -> std::path::PathBuf {
    crate::contract::default_contract_path()
}

pub fn default_matrix_path() -> std::path::PathBuf {
    crate::inventory::repo_root().join("docs/platform-matrix.md")
}

pub fn check_paths(support: &Path, matrix: &Path) -> Vec<String> {
    let contract_errors = crate::contract::check_path(support);
    if !contract_errors.is_empty() {
        return contract_errors
            .into_iter()
            .map(|error| format!("支持契约检查失败: {error}"))
            .collect();
    }
    let support_text = match std::fs::read_to_string(support) {
        Ok(text) => text,
        Err(err) => return vec![format!("读取 {} 失败: {err}", support.display())],
    };
    let matrix_text = match std::fs::read_to_string(matrix) {
        Ok(text) => text,
        Err(err) => return vec![format!("读取 {} 失败: {err}", matrix.display())],
    };
    let support_rows = match crate::contract::parse_index(&support_text) {
        Ok(rows) => rows,
        Err(err) => return vec![format!("支持契约解析失败: {err}")],
    };
    let guarantees: Vec<String> = support_rows
        .into_iter()
        .filter(|row| row.status == "保证")
        .map(|row| row.guarantee)
        .collect();
    let rows = match parse(&matrix_text) {
        Ok(rows) => rows,
        Err(err) => return vec![err],
    };

    let mut errors = Vec::new();
    if rows.len() != guarantees.len() {
        errors.push(format!(
            "矩阵有 {} 行，支持契约有 {} 条保证",
            rows.len(),
            guarantees.len()
        ));
    }
    let expected: HashSet<&str> = guarantees.iter().map(String::as_str).collect();
    let mut seen = HashSet::new();
    for row in &rows {
        if !seen.insert(row.guarantee.as_str()) {
            errors.push(format!("矩阵重复保证：{}", row.guarantee));
        }
        if !expected.contains(row.guarantee.as_str()) {
            errors.push(format!("矩阵多出未知保证：{}", row.guarantee));
        }
        if row.evidence.is_empty() || matches!(row.evidence.as_str(), "—" | "-" | "TODO" | "todo")
        {
            errors.push(format!("{} 的证据列为空或占位符", row.guarantee));
        } else {
            let root = crate::inventory::repo_root();
            let paths: Vec<&str> = row
                .evidence
                .split_whitespace()
                .map(|token| {
                    token.trim_matches(|ch: char| ch == '`' || ch == '/' || ch == ',' || ch == ';')
                })
                .filter(|token| token.ends_with(".rs"))
                .collect();
            if paths.is_empty() {
                errors.push(format!(
                    "{} 的证据必须引用测试文件（.rs）：{}",
                    row.guarantee, row.evidence
                ));
            }
            for path in paths {
                if !root.join(path).is_file() {
                    errors.push(format!("{} 的证据文件不存在：{}", row.guarantee, path));
                }
            }
        }
    }
    for guarantee in guarantees {
        if !seen.contains(guarantee.as_str()) {
            errors.push(format!("矩阵缺少保证：{guarantee}"));
        }
    }
    errors
}

fn parse(text: &str) -> Result<Vec<Row>, String> {
    // 先做一次 fence-aware 扫描，记住**唯一的真实标题行**；第二遍从它开始。
    // 不能像旧实现那样在 `!in_section` 时先 `continue`，否则标题前代码围栏里的
    // 示例会提前把 `in_section` 置真，最终校验的是示例表。
    let mut sections = 0;
    let mut section_line = None;
    let mut fenced = false;
    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.starts_with("```") || line.starts_with("~~~") {
            fenced = !fenced;
        } else if !fenced && line == SECTION {
            sections += 1;
            section_line.get_or_insert(index);
        }
    }
    if sections == 0 {
        return Err(format!("缺少 `{SECTION}` 一节"));
    }
    if sections > 1 {
        return Err(format!("重复的 `{SECTION}` 一节"));
    }
    let start = section_line.expect("sections == 1 implies a line");

    let mut in_fence = false;
    let mut header_seen = false;
    let mut separator_seen = false;
    let mut rows = Vec::new();
    for (index, raw) in text.lines().enumerate().skip(start) {
        let line_no = index + 1;
        let line = raw.trim();
        if index == start {
            continue;
        }
        if line.starts_with("```") || line.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        if line.starts_with('#') {
            break;
        }
        if !line.starts_with('|') {
            continue;
        }
        let cells = crate::contract::split_row(line);
        if !header_seen {
            if cells.len() != HEADER.len()
                || !cells.iter().map(String::as_str).eq(HEADER.iter().copied())
            {
                return Err(format!(
                    "第 {line_no} 行：平台矩阵表头必须是 `| 保证 | Windows GNU | Windows MSVC | Linux | macOS | 证据 |`，实际 {cells:?}"
                ));
            }
            header_seen = true;
            continue;
        }
        if !separator_seen {
            if cells.len() != HEADER.len() || !cells.iter().all(|cell| is_separator(cell)) {
                return Err(format!(
                    "第 {line_no} 行：表头后必须紧跟 {} 列分隔行",
                    HEADER.len()
                ));
            }
            separator_seen = true;
            continue;
        }
        if cells.iter().all(|cell| is_separator(cell)) {
            return Err(format!("第 {line_no} 行：数据行中不允许分隔行"));
        }
        if cells.len() != HEADER.len() {
            return Err(format!(
                "第 {line_no} 行：平台矩阵必须是 {} 列，实际 {} 列",
                HEADER.len(),
                cells.len()
            ));
        }
        let platforms = [
            cells[1].clone(),
            cells[2].clone(),
            cells[3].clone(),
            cells[4].clone(),
        ];
        for (platform, status) in PLATFORMS.iter().zip(&platforms) {
            if !STATUSES.contains(&status.as_str()) {
                return Err(format!(
                    "第 {line_no} 行：{platform} 状态必须是 通过 / 失败 / 待验证，实际 `{status}`"
                ));
            }
        }
        rows.push(Row {
            guarantee: cells[0].clone(),
            platforms,
            evidence: cells[5].clone(),
        });
    }
    if !header_seen {
        return Err("平台矩阵缺少表头".to_string());
    }
    if !separator_seen {
        return Err("平台矩阵缺少表头后的分隔行".to_string());
    }
    if rows.is_empty() {
        return Err("平台矩阵为空".to_string());
    }
    Ok(rows)
}

fn is_separator(cell: &str) -> bool {
    !cell.is_empty() && cell.chars().all(|ch| ch == '-' || ch == ':')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn support() -> &'static str {
        "## 保证索引\n\n| 保证 | 状态 | 证据 | 备注 |\n|---|---|---|---|\n\
         | A | 保证 | `crates/core/tests/weak_audit.rs::failed_provide_does_not_leave_recovery_entries` | |\n\
         | B | 保证 | `crates/core/tests/weak_audit.rs::manually_released_effects_do_not_accumulate` | |\n\
         | C | 声明 | — | |\n"
    }

    fn matrix() -> &'static str {
        "## 平台矩阵\n\n| 保证 | Windows GNU | Windows MSVC | Linux | macOS | 证据 |\n|---|---|---|---|---|---|\n\
         | A | 通过 | 待验证 | 待验证 | 待验证 | crates/core/tests/weak_audit.rs |\n\
         | B | 通过 | 待验证 | 待验证 | 待验证 | crates/core/tests/weak_audit.rs |\n"
    }

    #[test]
    fn accepts_a_complete_matrix() {
        let rows = parse(matrix()).expect("parse");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].platforms[0], "通过");
        assert_eq!(rows[0].platforms[1], "待验证");
    }

    #[test]
    fn rejects_duplicate_section() {
        let text = format!("{}\n{SECTION}\n", matrix());
        assert!(parse(&text).unwrap_err().contains("重复"));
    }

    #[test]
    fn fenced_example_before_the_real_section_is_ignored() {
        let text = format!(
            "```md\n{SECTION}\n\n| 保证 | Windows GNU | Windows MSVC | Linux | macOS | 证据 |\n\
             |---|---|---|---|---|---|\n| EXAMPLE | 通过 | 通过 | 通过 | 通过 | fake.rs |\n```\n\n{}",
            matrix()
        );
        let rows = parse(&text).expect("fenced example must not become the section");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].guarantee, "A");
    }

    #[test]
    fn rejects_unknown_platform_status() {
        let text = matrix().replace("待验证", "也许");
        assert!(parse(&text).unwrap_err().contains("状态"));
    }

    #[test]
    fn rejects_wrong_header_order() {
        let text = matrix().replace(
            "| 保证 | Windows GNU | Windows MSVC | Linux | macOS | 证据 |",
            "| 保证 | Linux | Windows GNU | Windows MSVC | macOS | 证据 |",
        );
        assert!(parse(&text).unwrap_err().contains("表头"));
    }

    #[test]
    fn rejects_short_row() {
        let text = matrix().replace(
            "| A | 通过 | 待验证 | 待验证 | 待验证 | crates/core/tests/weak_audit.rs |",
            "| A | 通过 | 待验证 | 待验证 | crates/core/tests/weak_audit.rs |",
        );
        assert!(parse(&text).unwrap_err().contains("列"));
    }

    #[test]
    fn rejects_missing_separator_row() {
        let text = matrix().replace("|---|---|---|---|---|---|\n", "");
        assert!(parse(&text).unwrap_err().contains("分隔行"));
    }

    #[test]
    fn check_reports_missing_and_extra_guarantees() {
        let support_path =
            std::env::temp_dir().join(format!("cordis-matrix-support-{}.md", std::process::id()));
        let matrix_path =
            std::env::temp_dir().join(format!("cordis-matrix-{}.md", std::process::id()));
        std::fs::write(&support_path, support()).unwrap();
        std::fs::write(
            &matrix_path,
            "## 平台矩阵\n\n| 保证 | Windows GNU | Windows MSVC | Linux | macOS | 证据 |\n\
             |---|---|---|---|---|---|\n\
             | A | 通过 | 待验证 | 待验证 | 待验证 | crates/core/tests/weak_audit.rs |\n\
             | X | 通过 | 待验证 | 待验证 | 待验证 | crates/core/tests/weak_audit.rs |\n",
        )
        .unwrap();
        let errors = check_paths(&support_path, &matrix_path);
        assert!(
            errors.iter().any(|err| err.contains("缺少保证：B")),
            "{errors:?}"
        );
        assert!(
            errors.iter().any(|err| err.contains("多出未知保证：X")),
            "{errors:?}"
        );
        let _ = std::fs::remove_file(support_path);
        let _ = std::fs::remove_file(matrix_path);
    }

    #[test]
    fn check_rejects_empty_evidence_and_invalid_contract() {
        let support_path = std::env::temp_dir().join(format!(
            "cordis-matrix-evidence-support-{}.md",
            std::process::id()
        ));
        let matrix_path =
            std::env::temp_dir().join(format!("cordis-matrix-evidence-{}.md", std::process::id()));
        std::fs::write(&support_path, support()).unwrap();
        std::fs::write(
            &matrix_path,
            "## 平台矩阵\n\n| 保证 | Windows GNU | Windows MSVC | Linux | macOS | 证据 |\n\
             |---|---|---|---|---|---|\n\
             | A | 通过 | 待验证 | 待验证 | 待验证 | TODO |\n\
             | B | 通过 | 待验证 | 待验证 | 待验证 | crates/core/tests/weak_audit.rs |\n",
        )
        .unwrap();
        let errors = check_paths(&support_path, &matrix_path);
        assert!(
            errors.iter().any(|err| err.contains("占位符")),
            "{errors:?}"
        );
        let _ = std::fs::remove_file(support_path);
        let _ = std::fs::remove_file(matrix_path);
    }
}
