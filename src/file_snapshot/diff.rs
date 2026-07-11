//! Unified diff 生成
//!
//! 使用 similar crate（Myers 算法）做真行级 diff，
//! 用于 get_file_diff / get_batch_diffs。
//! 输出格式与 unified diff 兼容（--- a/path / +++ b/path / @@ hunk）。

use similar::{ChangeTag, TextDiff};

/// 生成 unified diff
/// before/after 是文件的原始内容（UTF-8）
pub fn unified_diff(before: &str, after: &str, path: &str) -> String {
    let diff = TextDiff::from_lines(before, after);

    let mut result = String::new();
    result.push_str(&format!("--- a/{}\n", path));
    result.push_str(&format!("+++ b/{}\n", path));

    // 遍历所有 hunk（similar 自动按变更聚类，默认 context 3 行）
    let unified = diff.unified_diff();
    let output = unified.to_string();

    // similar 的 unified_diff 输出已含 --- / +++ 头，但我们用自己的路径格式
    // 跳过 similar 输出的前两行头（--- / +++），只取 hunk 部分
    let hunk_part = output.lines()
        .skip_while(|l| l.starts_with("---") || l.starts_with("+++"))
        .collect::<Vec<_>>()
        .join("\n");

    if hunk_part.is_empty() {
        // 完全相同，只有头
        return result;
    }

    result.push_str(&hunk_part);
    result.push('\n');
    result
}

/// 统计 diff 的 added/removed 行数
pub fn count_diff(diff: &str) -> (usize, usize) {
    let mut added = 0;
    let mut removed = 0;
    for line in diff.lines() {
        if line.starts_with('+') && !line.starts_with("+++") {
            added += 1;
        } else if line.starts_with('-') && !line.starts_with("---") {
            removed += 1;
        }
    }
    (added, removed)
}

/// 直接从 before/after 内容统计变更行数（不生成 diff 文本，更高效）
pub fn count_changes(before: &str, after: &str) -> (usize, usize) {
    let diff = TextDiff::from_lines(before, after);
    let mut added = 0;
    let mut removed = 0;
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Insert => added += 1,
            ChangeTag::Delete => removed += 1,
            ChangeTag::Equal => {}
        }
    }
    (added, removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_simple_change() {
        let before = "line1\nline2\nline3";
        let after = "line1\nline2 modified\nline3";
        let diff = unified_diff(before, after, "test.txt");
        assert!(diff.contains("-line2"), "应含删除行: {}", diff);
        assert!(diff.contains("+line2 modified"), "应含新增行: {}", diff);
    }

    #[test]
    fn diff_addition() {
        let before = "line1\nline2";
        let after = "line1\nline2\nline3";
        let diff = unified_diff(before, after, "test.txt");
        assert!(diff.contains("+line3"), "应含新增行: {}", diff);
    }

    #[test]
    fn diff_deletion() {
        let before = "line1\nline2\nline3";
        let after = "line1\nline3";
        let diff = unified_diff(before, after, "test.txt");
        assert!(diff.contains("-line2"), "应含删除行: {}", diff);
    }

    #[test]
    fn diff_identical() {
        let before = "line1\nline2";
        let after = "line1\nline2";
        let diff = unified_diff(before, after, "test.txt");
        let (added, removed) = count_diff(&diff);
        assert_eq!(added, 0, "相同内容不应有新增行");
        assert_eq!(removed, 0, "相同内容不应有删除行");
    }

    #[test]
    fn diff_count() {
        let diff = "--- a/x\n+++ b/x\n@@ -1,1 +1,2 @@\n-old\n+new1\n+new2\n";
        let (added, removed) = count_diff(diff);
        assert_eq!(added, 2);
        assert_eq!(removed, 1);
    }

    #[test]
    fn diff_scattered_changes_not_overreported() {
        // 回归测试：分散修改（第2行和第8行各改）不应把中间全标变更
        let before = "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10";
        let after = "l1\nl2-X\nl3\nl4\nl5\nl6\nl7\nl8-X\nl9\nl10";
        let diff = unified_diff(before, after, "test.rs");
        let (added, removed) = count_diff(&diff);
        // 真行级 diff：只删 2 行（l2, l8）、加 2 行（l2-X, l8-X）
        assert_eq!(removed, 2, "分散修改应只报 2 处删除，而非整段。diff:\n{}", diff);
        assert_eq!(added, 2, "分散修改应只报 2 处新增");
    }
}
