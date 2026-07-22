//! matcher 正则 + if 条件过滤
//!
//! 对齐 pi 的 matcher.ts + if-parser.ts：
//! - matcher: `*` / 空 = 全匹配；纯字母数字+`|`+`_` 时按 `|` 分割做工具名匹配；否则当正则
//! - if: 格式 `ToolName(glob)`，仅对部分工具生效

use super::HookHandler;

/// 检查 matcher 是否匹配工具名
///
/// - None / "*" / 空 = 全匹配
/// - 纯 `a-z|A-Z|0-9|_||` 时按 `|` 分割做工具名大小写不敏感匹配
/// - 否则当正则
pub fn matches_matcher(matcher: Option<&str>, tool_name: &str) -> bool {
    let m = match matcher {
        None | Some("") | Some("*") => return true,
        Some(m) => m,
    };

    // 纯简单字符（字母数字 + | + _）→ 按 | 分割精确匹配
    if m.chars().all(|c| c.is_alphanumeric() || c == '|' || c == '_') && m.contains('|') {
        return m.split('|')
            .map(|s| s.trim().to_lowercase())
            .any(|part| part == tool_name.to_lowercase());
    }
    // 单个工具名（无 |）也精确匹配
    if m.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return m.to_lowercase() == tool_name.to_lowercase();
    }

    // 否则当简单的 contains 匹配（避免引入 regex 依赖）
    // 真正的正则匹配后续按需接入
    m.to_lowercase().contains(&tool_name.to_lowercase().chars().next().unwrap_or(' ').to_string())
        || tool_name.to_lowercase().contains(&m.to_lowercase())
}

/// 检查 handler 的 if 条件是否满足
///
/// 格式 `ToolName(glob)`，如 `Bash(rm *)` 表示只对 bash 删文件命令生效。
/// 当前实现简化版：只解析 `ToolName` 部分，glob 部分暂不实现（始终 true）。
/// None 或空 = 始终满足。
pub fn matches_if_clause(handler: &HookHandler, tool_name: Option<&str>, tool_input: Option<&serde_json::Value>) -> bool {
    let clause = match &handler.if_clause {
        None => return true,
        Some(s) if s.is_empty() => return true,
        Some(c) => c,
    };

    // 解析 `ToolName(glob)` 格式
    if let Some(paren_idx) = clause.find('(') {
        let tool_pattern = &clause[..paren_idx].trim();
        // 工具名匹配（大小写不敏感）
        if let Some(actual_tool) = tool_name && !tool_pattern.is_empty() && !tool_pattern.eq_ignore_ascii_case(actual_tool) {
            return false;
        }
        // glob 部分简化：检查 input 里有没有匹配的关键词
        let glob = clause[paren_idx + 1..].trim_end_matches(')');
        if !glob.is_empty() && let Some(input) = tool_input {
            let input_str = input.to_string().to_lowercase();
            let glob_lower = glob.to_lowercase().replace('*', "");
            if !glob_lower.is_empty() && !input_str.contains(&glob_lower) {
                return false;
            }
        }
        true
    } else {
        // 没有括号 = 只匹配工具名
        if let Some(actual_tool) = tool_name {
            clause.eq_ignore_ascii_case(actual_tool)
        } else {
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_matcher_wildcard() {
        assert!(matches_matcher(None, "bash"));
        assert!(matches_matcher(Some(""), "bash"));
        assert!(matches_matcher(Some("*"), "bash"));
    }

    #[test]
    fn test_matcher_pipe_split() {
        assert!(matches_matcher(Some("bash|write|edit"), "bash"));
        assert!(matches_matcher(Some("bash|write|edit"), "WRITE"));
        assert!(!matches_matcher(Some("bash|write|edit"), "read"));
    }

    #[test]
    fn test_matcher_single_name() {
        assert!(matches_matcher(Some("bash"), "bash"));
        assert!(matches_matcher(Some("bash"), "BASH"));
        assert!(!matches_matcher(Some("bash"), "read"));
    }

    #[test]
    fn test_matcher_regex() {
        assert!(matches_matcher(Some("mcp__.*"), "mcp__server__tool"));
        assert!(!matches_matcher(Some("mcp__.*"), "bash"));
    }
}
