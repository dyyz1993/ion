//! Unified diff 生成
//!
//! 简单的行级 diff，用于 get_file_diff / get_batch_diffs

/// 生成 unified diff
/// before/after 是文件的原始内容（UTF-8）
pub fn unified_diff(before: &str, after: &str, path: &str) -> String {
    let before_lines: Vec<&str> = before.lines().collect();
    let after_lines: Vec<&str> = after.lines().collect();

    let mut result = String::new();
    result.push_str(&format!("--- a/{}\n", path));
    result.push_str(&format!("+++ b/{}\n", path));

    // 简单 diff：找公共前缀和后缀，中间部分标记 +/- 
    let prefix_len = common_prefix_len(&before_lines, &after_lines);
    let suffix_len = common_suffix_len(&before_lines, &after_lines, prefix_len);

    let before_mid = &before_lines[prefix_len..before_lines.len().saturating_sub(suffix_len)];
    let after_mid = &after_lines[prefix_len..after_lines.len().saturating_sub(suffix_len)];

    if before_mid.is_empty() && after_mid.is_empty() {
        // 完全相同
        return result;
    }

    // 输出 hunk
    result.push_str(&format!("@@ -{},{} +{},{} @@\n",
        prefix_len + 1, before_mid.len() + suffix_len.min(1),
        prefix_len + 1, after_mid.len() + suffix_len.min(1)));

    // 保留的前缀（1 行上下文）
    if prefix_len > 0 {
        result.push_str(&format!(" {}\n", before_lines[prefix_len - 1]));
    }

    // 删除的行
    for line in before_mid {
        result.push_str(&format!("-{}\n", line));
    }
    // 新增的行
    for line in after_mid {
        result.push_str(&format!("+{}\n", line));
    }

    result
}

/// 公共前缀长度
fn common_prefix_len(before: &[&str], after: &[&str]) -> usize {
    let mut i = 0;
    while i < before.len() && i < after.len() && before[i] == after[i] {
        i += 1;
    }
    i
}

/// 公共后缀长度（不超过前缀）
fn common_suffix_len(before: &[&str], after: &[&str], prefix: usize) -> usize {
    let mut i = 0;
    while i < before.len() - prefix && i < after.len() - prefix
        && before[before.len() - 1 - i] == after[after.len() - 1 - i]
    {
        i += 1;
    }
    i
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
        // 完全相同应无变更行（只有 --- +++ 头）
        // 统计 added/removed 行
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
}
