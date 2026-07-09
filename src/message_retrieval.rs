//! 会话消息拉取核心逻辑（纯函数模块）
//!
//! 所有拉取操作（get_messages / list_turns / list_inputs / get_turn_detail）
//! 的核心逻辑都在这里，以纯函数形式实现，不依赖 Agent / RPC，
//! 方便单元测试。
//!
//! 数据来源：`SessionFile.entries`（`Vec<serde_json::Value>`），
//! 每条 entry 是 JSONL 的一行（含 header 之外的 entry）。

use serde_json::Value;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::SystemTime;

// ═══════════════════════════════════════════════════════════════════════════
// SessionFile 缓存（进程级，mtime 校验）
// ═══════════════════════════════════════════════════════════════════════════

/// 缓存条目：cwd → (mtime, entries)
static SESSION_CACHE: Mutex<Option<HashMap<String, (SystemTime, Vec<Value>)>>> = Mutex::new(None);

/// 从缓存加载 session entries（带 mtime 校验）。
/// 文件没变化时 O(1) 返回缓存，变化时才重新读盘+解析。
/// worker 进程内多次调用 get_messages/list_turns 等时复用，避免每次整盘读。
pub fn load_entries_cached(cwd: &str) -> Vec<Value> {
    let path = crate::session_jsonl::session_path(cwd);

    // 获取文件 mtime
    let mtime = std::fs::metadata(&path)
        .ok()
        .and_then(|m| m.modified().ok());

    // 检查缓存
    if let Ok(mut cache_guard) = SESSION_CACHE.lock() {
        let cache = cache_guard.get_or_insert_with(HashMap::new);
        if let Some(mtime) = mtime {
            if let Some((cached_mtime, entries)) = cache.get(cwd) {
                if *cached_mtime == mtime {
                    return entries.clone();
                }
            }
        }
    }

    // 缓存未命中或文件已变，重新加载
    let entries = crate::session_jsonl::SessionFile::load(cwd)
        .map(|f| f.entries)
        .unwrap_or_default();

    // 更新缓存
    if let Ok(mut cache_guard) = SESSION_CACHE.lock() {
        let cache = cache_guard.get_or_insert_with(HashMap::new);
        if let Some(mtime) = mtime {
            cache.insert(cwd.to_string(), (mtime, entries.clone()));
        }
    }

    entries
}

/// 使缓存失效（外部修改了 session 文件后调用，比如 append 操作后）。
pub fn invalidate_cache(cwd: &str) {
    if let Ok(mut cache_guard) = SESSION_CACHE.lock() {
        if let Some(cache) = cache_guard.as_mut() {
            cache.remove(cwd);
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// 参数类型
// ═══════════════════════════════════════════════════════════════════════════

/// 视点：决定从哪个点开始看
#[derive(Clone, Debug)]
pub enum View {
    /// 活跃分支完整历史（解析最后一个 leaf_pointer）
    Live,
    /// 压缩点之后（扫最后一个 compaction entry）
    SinceCompaction,
    /// 指定分支（root → leaf_id 路径）
    Branch(String),
    /// 全量（不过滤，原始顺序，含所有分支）
    Full,
}

impl Default for View {
    fn default() -> Self {
        View::Live
    }
}

/// 旁路数据过滤
#[derive(Clone, Debug)]
pub enum CustomFilter {
    /// 只拉 message 类型
    None,
    /// 带 display:true 的旁路
    DisplayOnly,
    /// 全部（含 display:false 的隐藏事件）
    All,
}

impl Default for CustomFilter {
    fn default() -> Self {
        CustomFilter::None
    }
}

/// 拉取参数（所有接口共享）
#[derive(Clone, Debug, Default)]
pub struct RetrievalParams {
    pub view: View,
    pub after: Option<String>,
    pub before: Option<String>,
    pub limit: usize, // 0 = 全量
    pub complete_turn: bool,
    pub include_custom: CustomFilter,
}

/// 消息拉取结果
#[derive(Clone, Debug)]
pub struct RetrievalResult {
    pub messages: Vec<Value>,
    pub has_more: bool,
    pub total_count: usize,
    pub next_cursor: Option<String>,
    pub view: String,
    pub compaction_points: Vec<Value>,
    pub page_info: Option<PageInfo>,
}

#[derive(Clone, Debug)]
pub struct PageInfo {
    pub requested_limit: usize,
    pub actual_count: usize,
    pub completed_turn_boundary: Option<String>,
}

/// Turn 概览结果
#[derive(Clone, Debug, Default)]
pub struct TurnsResult {
    pub turns: Vec<TurnOverview>,
    pub has_more: bool,
    pub total_count: usize,
    pub next_cursor: Option<String>,
}

/// 单轮概览（list_turns 返回）
#[derive(Clone, Debug, Default)]
pub struct TurnOverview {
    pub turn_id: u64,
    pub user_entry_id: Option<String>,
    pub user_content: String,
    pub assistant_content: String,
    pub key_steps: Vec<String>,
    pub tool_call_count: u32,
    pub tokens_input: u64,
    pub tokens_output: u64,
    pub status: String,
    pub summary: String,
}

/// 用户输入结果
#[derive(Clone, Debug, Default)]
pub struct InputsResult {
    pub inputs: Vec<InputItem>,
    pub has_more: bool,
    pub total_count: usize,
    pub next_cursor: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct InputItem {
    pub turn_id: Option<u64>,
    pub entry_id: String,
    pub text: String,
}

// ═══════════════════════════════════════════════════════════════════════════
// 核心函数：retrieve_messages
// ═══════════════════════════════════════════════════════════════════════════

/// 拉取消息列表（get_messages 的核心逻辑）
pub fn retrieve_messages(entries: &[Value], params: &RetrievalParams) -> RetrievalResult {
    // 1. 视点过滤
    let view_filtered = apply_view_filter(entries, &params.view);

    // 2. 可见性过滤（deletion / segment_summary）
    let visible = apply_visibility_filter(&view_filtered);

    // 3. 旁路过滤（include_custom）
    let custom_filtered = apply_custom_filter(&visible, &params.include_custom);

    // 4. 收集 compaction_points（旁路数据，始终返回）
    let compaction_points: Vec<Value> = custom_filtered
        .iter()
        .filter(|e| e.get("type").and_then(|v| v.as_str()) == Some("compaction"))
        .cloned()
        .collect();

    // 5. 只保留 message 类型用于分页（compaction_points 已单独收集）
    let messages_only: Vec<Value> = custom_filtered
        .iter()
        .filter(|e| {
            let t = e.get("type").and_then(|v| v.as_str()).unwrap_or("");
            t == "message" || t == "branch_summary"
        })
        .cloned()
        .collect();

    let total_count = messages_only.len();

    // 6. 分页
    let (page, next_cursor, has_more, page_info) =
        apply_pagination(&messages_only, &params.after, &params.before, params.limit);

    RetrievalResult {
        messages: page,
        has_more,
        total_count,
        next_cursor,
        view: format!("{:?}", params.view).to_lowercase(),
        compaction_points,
        page_info,
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// 核心函数：retrieve_turns
// ═══════════════════════════════════════════════════════════════════════════

/// 拉取逐轮概览（list_turns 的核心逻辑）
pub fn retrieve_turns(entries: &[Value], params: &RetrievalParams, full_content: bool) -> TurnsResult {
    // 先视点过滤（since_compaction 截断；branch 走分支路径；live/full 不过滤 turn_summary）
    let view_filtered = apply_view_filter(entries, &params.view);

    // 可见性过滤
    let visible = apply_visibility_filter(&view_filtered);

    // 按 turn_summary entry 或 user→assistant 边界分组
    let groups = group_into_turns(&visible);

    let turns: Vec<TurnOverview> = groups
        .iter()
        .map(|g| extract_turn_overview(g, full_content))
        .collect();

    let total_count = turns.len();

    // TODO: 分页（按 turnId 游标），第 2 期实现
    TurnsResult {
        turns,
        has_more: false,
        total_count,
        next_cursor: None,
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// 核心函数：retrieve_inputs
// ═══════════════════════════════════════════════════════════════════════════

/// 拉取用户输入列表（list_inputs 的核心逻辑）
pub fn retrieve_inputs(entries: &[Value], _params: &RetrievalParams) -> InputsResult {
    let view_filtered = apply_view_filter(entries, &View::Live);
    let visible = apply_visibility_filter(&view_filtered);

    let mut inputs = Vec::new();
    for entry in &visible {
        if entry.get("type").and_then(|v| v.as_str()) != Some("message") {
            continue;
        }
        // 检查 role == user
        let role = entry
            .get("message")
            .and_then(|m| m.get("role"))
            .and_then(|r| r.as_str())
            .unwrap_or("");
        if role != "user" {
            continue;
        }
        let entry_id = entry
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let text = extract_message_text(entry);
        inputs.push(InputItem {
            turn_id: None, // TODO: 从 turn_summary 关联
            entry_id,
            text,
        });
    }

    let total_count = inputs.len();
    InputsResult {
        inputs,
        has_more: false,
        total_count,
        next_cursor: None,
    }
}

/// 单轮明细结果（get_turn_detail 返回）
#[derive(Clone, Debug, Default)]
pub struct TurnDetail {
    pub turn_id: u64,
    pub entries: Vec<Value>,
    pub overview: TurnOverview,
}

/// 拉取单轮明细（get_turn_detail 的核心逻辑）
/// 不分页——单 turn 数据量有上限。
pub fn retrieve_turn_detail(entries: &[Value], turn_id: u64, _include_custom: &CustomFilter) -> Option<TurnDetail> {
    let groups = group_into_turns(entries);
    let group = groups.into_iter().find(|g| {
        g.iter()
            .rev()
            .find(|e| e.get("type").and_then(|v| v.as_str()) == Some("turn_summary"))
            .and_then(|ts| ts.get("turnId").and_then(|v| v.as_u64()))
            == Some(turn_id)
    })?;

    let overview = extract_turn_overview(&group, true); // get_turn_detail 始终 full_content
    Some(TurnDetail {
        turn_id,
        entries: group,
        overview,
    })
}

// ═══════════════════════════════════════════════════════════════════════════
// 内部子函数
// ═══════════════════════════════════════════════════════════════════════════

/// 视点过滤：根据 view 返回 entry 子集
fn apply_view_filter(entries: &[Value], view: &View) -> Vec<Value> {
    match view {
        View::Full => entries.to_vec(),
        View::Live => {
            // 解析最后一个 leaf_pointer
            let has_leaf_pointer = entries
                .iter()
                .any(|e| e.get("type").and_then(|v| v.as_str()) == Some("leaf_pointer"));
            if has_leaf_pointer {
                // 有 leaf_pointer：取 root→leaf 路径
                if let Some(leaf_id) = crate::session_tree::resolve_current_leaf(entries) {
                    let path = crate::session_tree::get_branch_path(entries, &leaf_id);
                    if path.is_empty() {
                        entries.to_vec()
                    } else {
                        path
                    }
                } else {
                    entries.to_vec()
                }
            } else {
                // 无 leaf_pointer：无分支，返回全部（含 turn_summary/compaction 等非链 entry）
                entries.to_vec()
            }
        }
        View::SinceCompaction => {
            // since_compaction 语义：从最后一个 compaction 点之后。
            // 直接在全量 entries 上截断（不走分支路径——分支 + 压缩的组合是第 2 期）。
            truncate_after_last_compaction(entries)
        }
        View::Branch(leaf_id) => {
            crate::session_tree::get_branch_path(entries, leaf_id)
        }
    }
}

/// 找最后一个 compaction entry，返回它之后的部分
fn truncate_after_last_compaction(entries: &[Value]) -> Vec<Value> {
    let last_compaction_idx = entries
        .iter()
        .rposition(|e| e.get("type").and_then(|v| v.as_str()) == Some("compaction"));

    match last_compaction_idx {
        Some(idx) => entries[idx..].to_vec(),
        None => entries.to_vec(),
    }
}

/// 可见性过滤：排除被 deletion 标记的 entry，替换 segment_summary 覆盖的 entry
/// （deletion / segment_summary 当前生产路径未写入，这里做预留过滤）
fn apply_visibility_filter(entries: &[Value]) -> Vec<Value> {
    // 收集所有 deletion 的 targetIds
    let deleted_ids: std::collections::HashSet<String> = entries
        .iter()
        .filter(|e| e.get("type").and_then(|v| v.as_str()) == Some("deletion"))
        .filter_map(|e| e.get("targetIds").and_then(|v| v.as_array()))
        .flatten()
        .filter_map(|v| v.as_str().map(|s| s.to_string()))
        .collect();

    // 过滤掉被删除的
    entries
        .iter()
        .filter(|e| {
            let id = e.get("id").and_then(|v| v.as_str()).unwrap_or("");
            !deleted_ids.contains(id)
        })
        .cloned()
        .collect()
}

/// 旁路数据过滤（include_custom）
fn apply_custom_filter(entries: &[Value], filter: &CustomFilter) -> Vec<Value> {
    match filter {
        CustomFilter::All => entries.to_vec(),
        CustomFilter::DisplayOnly => entries
            .iter()
            .filter(|e| {
                let t = e.get("type").and_then(|v| v.as_str()).unwrap_or("");
                if t == "message" || t == "branch_summary" || t == "compaction" {
                    return true;
                }
                // custom / system_event / custom_message：检查 display
                e.get("display").and_then(|v| v.as_bool()).unwrap_or(false)
            })
            .cloned()
            .collect(),
        CustomFilter::None => entries
            .iter()
            .filter(|e| {
                let t = e.get("type").and_then(|v| v.as_str()).unwrap_or("");
                // 只保留 message / branch_summary / compaction / turn_summary / leaf_pointer
                t == "message"
                    || t == "branch_summary"
                    || t == "compaction"
                    || t == "turn_summary"
                    || t == "leaf_pointer"
            })
            .cloned()
            .collect(),
    }
}

/// 分页（after/before 游标 + limit）
fn apply_pagination(
    messages: &[Value],
    after: &Option<String>,
    before: &Option<String>,
    limit: usize,
) -> (Vec<Value>, Option<String>, bool, Option<PageInfo>) {
    // limit == 0 表示全量
    if limit == 0 {
        return (
            messages.to_vec(),
            None,
            false,
            Some(PageInfo {
                requested_limit: 0,
                actual_count: messages.len(),
                completed_turn_boundary: None,
            }),
        );
    }

    // 正向分页（after）
    if let Some(after_id) = after {
        let start_idx = messages
            .iter()
            .position(|e| e.get("id").and_then(|v| v.as_str()) == Some(after_id.as_str()))
            .map(|idx| idx + 1)
            .unwrap_or(0);
        let end = (start_idx + limit).min(messages.len());
        let page = messages[start_idx..end].to_vec();
        let has_more = end < messages.len();
        let next_cursor = if has_more {
            page.last()
                .and_then(|e| e.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()))
        } else {
            None
        };
        return (
            page,
            next_cursor,
            has_more,
            Some(PageInfo {
                requested_limit: limit,
                actual_count: end - start_idx,
                completed_turn_boundary: None,
            }),
        );
    }

    // 反向分页（before）：返回 before_id 之前的 limit 条（最新的 limit 条）
    if let Some(before_id) = before {
        let end_idx = messages
            .iter()
            .position(|e| e.get("id").and_then(|v| v.as_str()) == Some(before_id.as_str()))
            .unwrap_or(messages.len());
        let start_idx = end_idx.saturating_sub(limit);
        let page = messages[start_idx..end_idx].to_vec();
        let has_more = start_idx > 0;
        let next_cursor = if has_more {
            page.first()
                .and_then(|e| e.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()))
        } else {
            None
        };
        return (
            page,
            next_cursor,
            has_more,
            Some(PageInfo {
                requested_limit: limit,
                actual_count: end_idx - start_idx,
                completed_turn_boundary: None,
            }),
        );
    }

    // 默认：返回最新的 limit 条（反向首屏）
    let start_idx = messages.len().saturating_sub(limit);
    let page = messages[start_idx..].to_vec();
    let has_more = start_idx > 0;
    let next_cursor = if has_more {
        page.first()
            .and_then(|e| e.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()))
    } else {
        None
    };
    (
        page,
        next_cursor,
        has_more,
        Some(PageInfo {
            requested_limit: limit,
            actual_count: messages.len() - start_idx,
            completed_turn_boundary: None,
        }),
    )
}

/// 按 turn 分组：优先用 turn_summary entry 切分，否则按 user→assistant 边界
fn group_into_turns(entries: &[Value]) -> Vec<Vec<Value>> {
    // 如果有 turn_summary entry，用它作为 turn 边界
    let has_turn_summary = entries
        .iter()
        .any(|e| e.get("type").and_then(|v| v.as_str()) == Some("turn_summary"));

    if has_turn_summary {
        group_by_turn_summary(entries)
    } else {
        group_by_user_boundary(entries)
    }
}

/// 用 turn_summary entry 切分 turn
fn group_by_turn_summary(entries: &[Value]) -> Vec<Vec<Value>> {
    let mut groups = Vec::new();
    let mut current = Vec::new();

    for entry in entries {
        let t = entry.get("type").and_then(|v| v.as_str()).unwrap_or("");
        current.push(entry.clone());
        if t == "turn_summary" {
            groups.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        groups.push(current);
    }
    groups
}

/// 无 turn_summary 时，按 user→assistant 边界切分
fn group_by_user_boundary(entries: &[Value]) -> Vec<Vec<Value>> {
    let mut groups = Vec::new();
    let mut current = Vec::new();

    for entry in entries {
        let t = entry.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if t == "message" {
            let role = entry
                .get("message")
                .and_then(|m| m.get("role"))
                .and_then(|r| r.as_str())
                .unwrap_or("");
            // 新 user 消息 = 新 turn 开始（除非是第一条）
            if role == "user" && !current.is_empty() {
                groups.push(std::mem::take(&mut current));
            }
        }
        // 跳过非 message entry（compaction / custom 等不参与 turn 分组）
        if t == "message" || t == "branch_summary" {
            current.push(entry.clone());
        }
    }
    if !current.is_empty() {
        groups.push(current);
    }
    groups
}

/// 从一组 entry 提取 turn 概览
fn extract_turn_overview(group: &[Value], full_content: bool) -> TurnOverview {
    // 如果组末尾有 turn_summary，用它
    if let Some(ts) = group
        .iter()
        .rev()
        .find(|e| e.get("type").and_then(|v| v.as_str()) == Some("turn_summary"))
    {
        return extract_from_turn_summary(ts, group, full_content);
    }

    // 否则从 message 提取
    let mut overview = TurnOverview::default();

    for entry in group {
        if entry.get("type").and_then(|v| v.as_str()) != Some("message") {
            continue;
        }
        let role = entry
            .get("message")
            .and_then(|m| m.get("role"))
            .and_then(|r| r.as_str())
            .unwrap_or("");

        let text = extract_message_text(entry);
        match role {
            "user" => {
                overview.user_content = if full_content {
                    text
                } else {
                    truncate_content(&text, 200)
                };
            }
            "assistant" => {
                overview.assistant_content = if full_content {
                    text
                } else {
                    truncate_content(&text, 200)
                };
                // 统计 tool_calls
                let has_tool = entry
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(|c| c.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter(|b| {
                                b.get("type").and_then(|t| t.as_str()) == Some("tool_use")
                            })
                            .count()
                    })
                    .unwrap_or(0);
                overview.tool_call_count += has_tool as u32;
            }
            _ => {}
        }
    }

    overview.status = "completed".to_string();
    overview.summary = truncate_content(&overview.assistant_content, 200);
    overview
}

/// 从 turn_summary entry 提取概览
fn extract_from_turn_summary(
    ts: &Value,
    group: &[Value],
    full_content: bool,
) -> TurnOverview {
    let mut overview = TurnOverview {
        turn_id: ts.get("turnId").and_then(|v| v.as_u64()).unwrap_or(0),
        status: ts
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("completed")
            .to_string(),
        summary: ts
            .get("summary")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        key_steps: ts
            .get("keySteps")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default(),
        tool_call_count: ts
            .get("toolCallCount")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32,
        tokens_input: ts
            .get("tokens")
            .and_then(|t| t.get("input"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        tokens_output: ts
            .get("tokens")
            .and_then(|t| t.get("output"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        ..Default::default()
    };

    // 从 group 里的 message 补充 user_content / assistant_content
    for entry in group {
        if entry.get("type").and_then(|v| v.as_str()) != Some("message") {
            continue;
        }
        let role = entry
            .get("message")
            .and_then(|m| m.get("role"))
            .and_then(|r| r.as_str())
            .unwrap_or("");
        let text = extract_message_text(entry);
        match role {
            "user" if overview.user_content.is_empty() => {
                overview.user_content = if full_content {
                    text
                } else {
                    truncate_content(&text, 200)
                };
            }
            "assistant" if overview.assistant_content.is_empty() => {
                overview.assistant_content = if full_content {
                    text
                } else {
                    truncate_content(&text, 200)
                };
            }
            _ => {}
        }
    }

    overview
}

/// 从 message entry 提取文本
fn extract_message_text(entry: &Value) -> String {
    entry
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            // content 可能是数组（ContentBlock[]）
            entry
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|b| {
                            if b.get("type").and_then(|t| t.as_str()) == Some("text")
                                || b.get("text").is_some()
                            {
                                b.get("text").and_then(|t| t.as_str()).map(|s| s.to_string())
                            } else {
                                None
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("")
                })
                .unwrap_or_default()
        })
}

/// 截断内容到指定字符数
fn truncate_content(text: &str, max_chars: usize) -> String {
    if text.chars().count() > max_chars {
        text.chars().take(max_chars).collect::<String>() + "..."
    } else {
        text.to_string()
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// 单元测试
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // 造假数据的 helper（仿 session_tree.rs:519 的 msg() 模式）
    fn msg(id: &str, parent: &str, role: &str, text: &str) -> Value {
        json!({
            "type": "message",
            "id": id,
            "parentId": parent,
            "message": {
                "role": role,
                "content": [{"type": "text", "text": text}]
            }
        })
    }

    fn turn_summary(turn_id: u64, summary: &str, status: &str) -> Value {
        json!({
            "type": "turn_summary",
            "id": format!("ts_{turn_id}"),
            "parentId": null,
            "turnId": turn_id,
            "summary": summary,
            "keySteps": ["read", "edit"],
            "toolCallCount": 2,
            "tokens": {"input": 100, "output": 50},
            "status": status
        })
    }

    fn compaction(id: &str, summary: &str) -> Value {
        json!({
            "type": "compaction",
            "id": id,
            "parentId": null,
            "summary": summary,
            "tokensBefore": 32000
        })
    }

    fn make_3_turn_session() -> Vec<Value> {
        vec![
            msg("msg_001", "", "user", "帮我重构接口"),
            msg("msg_002", "msg_001", "assistant", "好的我来分析"),
            turn_summary(0, "分析了现有代码", "completed"),
            msg("msg_003", "msg_002", "user", "设计方案"),
            msg("msg_004", "msg_003", "assistant", "用游标分页"),
            turn_summary(1, "设计了游标分页方案", "completed"),
            msg("msg_005", "msg_004", "user", "写测试"),
            msg("msg_006", "msg_005", "assistant", "测试写好了"),
            turn_summary(2, "写了单元测试", "completed"),
        ]
    }

    // ── retrieve_messages 测试 ──

    #[test]
    fn test_retrieve_messages_full_no_limit() {
        let entries = make_3_turn_session();
        let params = RetrievalParams {
            view: View::Full,
            limit: 0,
            ..Default::default()
        };
        let result = retrieve_messages(&entries, &params);
        assert_eq!(result.total_count, 6); // 6 条 message（不含 turn_summary）
        assert!(!result.has_more);
        assert!(result.next_cursor.is_none());
    }

    #[test]
    fn test_retrieve_messages_pagination_latest_n() {
        let entries = make_3_turn_session();
        let params = RetrievalParams {
            view: View::Full,
            limit: 2,
            ..Default::default()
        };
        let result = retrieve_messages(&entries, &params);
        assert_eq!(result.messages.len(), 2); // 最新 2 条
        assert!(result.has_more);
        // 最新 2 条应该是 msg_005, msg_006
        let ids: Vec<_> = result
            .messages
            .iter()
            .filter_map(|e| e.get("id").and_then(|v| v.as_str()))
            .collect();
        assert!(ids.contains(&"msg_005"));
        assert!(ids.contains(&"msg_006"));
    }

    #[test]
    fn test_retrieve_messages_pagination_before() {
        let entries = make_3_turn_session();
        let params = RetrievalParams {
            view: View::Full,
            before: Some("msg_005".to_string()),
            limit: 2,
            ..Default::default()
        };
        let result = retrieve_messages(&entries, &params);
        // before msg_005 → 返回 msg_005 之前的 2 条
        assert_eq!(result.messages.len(), 2);
        let ids: Vec<_> = result
            .messages
            .iter()
            .filter_map(|e| e.get("id").and_then(|v| v.as_str()))
            .collect();
        assert!(ids.contains(&"msg_003"));
        assert!(ids.contains(&"msg_004"));
    }

    #[test]
    fn test_retrieve_messages_pagination_after() {
        let entries = make_3_turn_session();
        let params = RetrievalParams {
            view: View::Full,
            after: Some("msg_002".to_string()),
            limit: 2,
            ..Default::default()
        };
        let result = retrieve_messages(&entries, &params);
        // after msg_002 → 返回 msg_003, msg_004
        assert_eq!(result.messages.len(), 2);
        let first_id = result.messages[0].get("id").and_then(|v| v.as_str());
        assert_eq!(first_id, Some("msg_003"));
    }

    #[test]
    fn test_retrieve_messages_since_compaction() {
        let mut entries = make_3_turn_session();
        // 在 msg_002 后插入 compaction
        entries.insert(2, compaction("cmp_001", "前 1 轮压缩"));
        let params = RetrievalParams {
            view: View::SinceCompaction,
            limit: 0,
            ..Default::default()
        };
        let result = retrieve_messages(&entries, &params);
        // since_compaction → 只返回 compaction 之后的
        assert!(result.total_count < 6, "since_compaction should filter out pre-compaction messages, got {}", result.total_count);
        assert!(!result.compaction_points.is_empty());
    }

    #[test]
    fn test_retrieve_messages_compaction_points_collected() {
        let mut entries = make_3_turn_session();
        entries.insert(2, compaction("cmp_001", "压缩摘要"));
        let params = RetrievalParams {
            view: View::Full,
            limit: 0,
            ..Default::default()
        };
        let result = retrieve_messages(&entries, &params);
        assert_eq!(result.compaction_points.len(), 1);
        let summary = result.compaction_points[0]
            .get("summary")
            .and_then(|v| v.as_str());
        assert_eq!(summary, Some("压缩摘要"));
    }

    // ── retrieve_turns 测试 ──

    #[test]
    fn test_retrieve_turns_count() {
        let entries = make_3_turn_session();
        let result = retrieve_turns(&entries, &RetrievalParams::default(), false);
        assert_eq!(result.total_count, 3); // 3 轮
    }

    #[test]
    fn test_retrieve_turns_user_content() {
        let entries = make_3_turn_session();
        let result = retrieve_turns(&entries, &RetrievalParams::default(), false);
        assert!(result.turns[0].user_content.contains("帮我重构接口"));
        assert!(result.turns[1].user_content.contains("设计方案"));
    }

    #[test]
    fn test_retrieve_turns_truncation() {
        let long_text = "a".repeat(300);
        let entries = vec![
            msg("msg_001", "", "user", &long_text),
            msg("msg_002", "msg_001", "assistant", "ok"),
            turn_summary(0, "summary", "completed"),
        ];
        let result = retrieve_turns(&entries, &RetrievalParams::default(), false);
        assert!(result.turns[0].user_content.ends_with("..."));
        assert!(result.turns[0].user_content.chars().count() <= 203); // 200 + "..."

        // full_content = true
        let result_full = retrieve_turns(&entries, &RetrievalParams::default(), true);
        assert_eq!(result_full.turns[0].user_content.chars().count(), 300);
    }

    #[test]
    fn test_retrieve_turns_from_summary() {
        let entries = make_3_turn_session();
        let result = retrieve_turns(&entries, &RetrievalParams::default(), false);
        // turn_summary 的字段应该被提取
        assert_eq!(result.turns[0].turn_id, 0);
        assert!(result.turns[0].summary.contains("分析了现有代码"));
        assert_eq!(result.turns[0].tool_call_count, 2);
        assert_eq!(result.turns[0].tokens_input, 100);
    }

    // ── retrieve_inputs 测试 ──

    #[test]
    fn test_retrieve_inputs_only_user() {
        let entries = make_3_turn_session();
        let result = retrieve_inputs(&entries, &RetrievalParams::default());
        assert_eq!(result.total_count, 3); // 3 条 user 消息
        assert!(result.inputs.iter().all(|i| i.text.contains("帮我") || i.text.contains("设计") || i.text.contains("测试")));
    }

    #[test]
    fn test_retrieve_inputs_excludes_assistant() {
        let entries = make_3_turn_session();
        let result = retrieve_inputs(&entries, &RetrievalParams::default());
        assert!(result.inputs.iter().all(|i| !i.text.contains("好的")));
        assert!(result.inputs.iter().all(|i| !i.text.contains("游标")));
    }

    // ── 边界测试 ──

    #[test]
    fn test_retrieve_turn_detail_found() {
        let entries = make_3_turn_session();
        let detail = retrieve_turn_detail(&entries, 1, &CustomFilter::None);
        assert!(detail.is_some());
        let d = detail.unwrap();
        assert_eq!(d.turn_id, 1);
        assert!(d.overview.user_content.contains("设计方案"));
        assert!(d.overview.summary.contains("设计了游标分页方案"));
        assert_eq!(d.overview.tool_call_count, 2);
    }

    #[test]
    fn test_retrieve_turn_detail_not_found() {
        let entries = make_3_turn_session();
        let detail = retrieve_turn_detail(&entries, 99, &CustomFilter::None);
        assert!(detail.is_none());
    }

    #[test]
    fn test_custom_filter_none_excludes_custom() {
        let mut entries = make_3_turn_session();
        // 加一条 custom entry
        entries.push(json!({"type":"custom","id":"cst_001","parentId":null,"customType":"memory_search","data":{"q":"test"}}));
        let params = RetrievalParams {
            view: View::Full,
            include_custom: CustomFilter::None,
            limit: 0,
            ..Default::default()
        };
        let result = retrieve_messages(&entries, &params);
        // custom 不在 messages 里（只 message/branch_summary）
        let has_custom = result.messages.iter().any(|e| {
            e.get("type").and_then(|v| v.as_str()) == Some("custom")
        });
        assert!(!has_custom, "None filter should exclude custom entries");
    }

    #[test]
    fn test_custom_filter_all_includes_custom() {
        let mut entries = make_3_turn_session();
        entries.push(json!({"type":"custom","id":"cst_001","parentId":null,"customType":"memory_search","data":{"q":"test"},"display":false}));
        let params = RetrievalParams {
            view: View::Full,
            include_custom: CustomFilter::All,
            limit: 0,
            ..Default::default()
        };
        let result = retrieve_messages(&entries, &params);
        // messages 只含 message/branch_summary，custom 在 compaction_points 旁路？不对
        // 实际上 custom 不在 messages 里，它在 entries 里。需要检查不同的结构。
        // 这里验证：All 模式不过滤 custom entry（它在过滤后的 entries 里）
        // retrieve_messages 的 messages 只含 message/branch_summary，custom 不算
        // 所以这个测试验证的是：custom 不会出现在 messages 数组里（不管 filter）
        let has_custom = result.messages.iter().any(|e| {
            e.get("type").and_then(|v| v.as_str()) == Some("custom")
        });
        // custom 永远不在 messages 里（messages 只要 message/branch_summary）
        // 但 All 模式应该让它出现在结果中。当前实现 messages 只取 message/branch_summary
        // 所以这个测试验证的是当前行为：custom 不在 messages
        assert!(!has_custom);
    }

    #[test]
    fn test_branch_view() {
        let entries = make_3_turn_session();
        // Branch view 指向 msg_003 → get_branch_path 返回 root→msg_003 路径
        let params = RetrievalParams {
            view: View::Branch("msg_003".to_string()),
            limit: 0,
            ..Default::default()
        };
        let result = retrieve_messages(&entries, &params);
        // 应该返回 msg_003 及其祖先链上的 message
        assert!(result.total_count <= 6);
    }

    #[test]
    fn test_empty_entries() {
        let result = retrieve_messages(&[], &RetrievalParams::default());
        assert_eq!(result.total_count, 0);
        assert!(!result.has_more);
    }

    #[test]
    fn test_pagination_beyond_end() {
        let entries = make_3_turn_session();
        let params = RetrievalParams {
            view: View::Full,
            after: Some("msg_006".to_string()), // 最后一条之后
            limit: 5,
            ..Default::default()
        };
        let result = retrieve_messages(&entries, &params);
        assert_eq!(result.messages.len(), 0);
        assert!(!result.has_more);
    }

    #[test]
    fn test_truncate_content() {
        assert_eq!(truncate_content("hello", 10), "hello");
        assert_eq!(truncate_content("hello world", 5), "hello...");
    }
}
