//! Export an ION session as one self-contained, offline HTML file.
//!
//! ## ION 存储格式与导出渲染格式
//!
//! ION 的 JSONL 使用 Rust enum 序列化形式（externally tagged），导出渲染器使用扁平形式：
//!
//! ION: `{"message": {"Assistant": {"role":"assistant", "content":[{"Text":{"text":"..."}}]}}}`
//! HTML: `{"message": {"role":"assistant",   "content":[{"type":"text", "text":"..."}]}}`
//!
//! Content blocks 也是 enum tagged：
//! - `{Text:{text}}`        → `{"type":"text", "text"}`
//! - `{ToolCall:{id,name,arguments}}` → `{"type":"toolCall", "id", "name", "arguments"}`
//! - `{Image:{data,mimeType}}` → `{"type":"image", "data", "mimeType"}`
//! - `{Thinking:{thinking}}` → `{"type":"thinking", "thinking"}`
//!
//! ToolResult 额外字段 camelCase：
//! - `is_error` → `isError`
//! - `tool_call_id` → `toolCallId`
//! - `tool_name` → `toolName`
//! - `role:"tool"` → `role:"toolResult"`
//!
//! 回合概览从真实消息树派生；文件变化以 parented `custom:step-snapshot` 展示。

use serde_json::{Value, json};
use std::path::Path;

use crate::session_jsonl;
use crate::worker_registry::WorkerRelation;

// ION owns the offline export bundle. pi remains a design reference only;
// exporting must never depend on another checkout or on a local web server.
const EXPORT_TEMPLATE_HTML: &str = include_str!("export_assets/template.html");
const EXPORT_TEMPLATE_CSS: &str = include_str!("export_assets/template.css");
const EXPORT_TEMPLATE_JS: &str = include_str!("export_assets/template.js");
const EXPORT_MARKED_JS: &str = include_str!("export_assets/vendor/marked.min.js");
const EXPORT_HIGHLIGHT_JS: &str = include_str!("export_assets/vendor/highlight.min.js");

/// Tool definition snapshot embedded into the exported HTML.
#[derive(Clone, Debug, serde::Serialize)]
pub struct ExportToolInfo {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Debug)]
struct ExportEntrySelection {
    entries: Vec<Value>,
    active_leaf_id: Option<String>,
    source_entry_count: usize,
    omitted_branch_entry_count: usize,
}

fn entry_id(entry: &Value) -> Option<&str> {
    entry.get("id").and_then(|value| value.as_str())
}

fn entry_parent_id(entry: &Value) -> Option<&str> {
    entry.get("parentId").and_then(|value| value.as_str())
}

fn is_message_entry(entry: &Value) -> bool {
    entry.get("type").and_then(|value| value.as_str()) == Some("message")
}

fn is_descendant_of_entry(
    entries_by_id: &std::collections::HashMap<&str, &Value>,
    id: &str,
    ancestor: &str,
) -> bool {
    let mut current = Some(id);
    let mut visited = std::collections::HashSet::new();
    while let Some(current_id) = current {
        if !visited.insert(current_id) {
            return false;
        }
        if current_id == ancestor {
            return true;
        }
        current = entries_by_id
            .get(current_id)
            .and_then(|entry| entry_parent_id(entry));
    }
    false
}

/// Resolve the conversational leaf used by export.
///
/// `session_tree::resolve_current_leaf` intentionally considers every tree entry.
/// An audit entry such as a rollback `branch_summary` may be appended after a
/// `leaf_pointer`, though, and must not pull the exported conversation back onto
/// the abandoned branch. After the last pointer, only message descendants advance
/// the conversational leaf; the pointer target remains the fallback.
fn resolve_export_active_leaf(entries: &[Value]) -> Option<String> {
    let last_pointer = entries.iter().rposition(|entry| {
        entry.get("type").and_then(|value| value.as_str()) == Some("leaf_pointer")
    });
    let Some(pointer_index) = last_pointer else {
        return crate::session_tree::resolve_current_leaf(entries);
    };

    let pointer_target = entries[pointer_index]
        .get("leafId")
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty());
    let entries_by_id: std::collections::HashMap<&str, &Value> = entries
        .iter()
        .filter_map(|entry| entry_id(entry).map(|id| (id, entry)))
        .collect();

    if let Some(target) = pointer_target {
        let latest_message = entries
            .iter()
            .skip(pointer_index + 1)
            .filter(|entry| is_message_entry(entry))
            .filter_map(entry_id)
            .filter(|id| is_descendant_of_entry(&entries_by_id, id, target))
            .last();
        return latest_message.or(Some(target)).map(str::to_string);
    }

    entries
        .iter()
        .skip(pointer_index + 1)
        .filter(|entry| is_message_entry(entry))
        .filter_map(entry_id)
        .last()
        .map(str::to_string)
}

/// Select the body stream for a standalone HTML export.
///
/// Linear sessions retain every JSONL entry. Branched sessions retain the active
/// root-to-leaf message path, global/audit entries, and one structural record per
/// branch operation (`leaf_pointer`, `label`, `branch_summary`). Content scoped to
/// an abandoned message subtree is omitted.
fn select_entries_for_export(entries: &[Value], session_id: &str) -> ExportEntrySelection {
    use std::collections::{HashMap, HashSet};

    let source_entry_count = entries.len();
    let active_leaf_id = resolve_export_active_leaf(entries);
    let has_leaf_pointer = entries
        .iter()
        .any(|entry| entry.get("type").and_then(|value| value.as_str()) == Some("leaf_pointer"));
    let mut message_children: HashMap<&str, usize> = HashMap::new();
    for entry in entries.iter().filter(|entry| is_message_entry(entry)) {
        let parent = entry_parent_id(entry).unwrap_or("");
        *message_children.entry(parent).or_default() += 1;
    }
    let has_message_fork = message_children.values().any(|count| *count > 1);

    if !has_leaf_pointer && !has_message_fork {
        return ExportEntrySelection {
            entries: entries.to_vec(),
            active_leaf_id,
            source_entry_count,
            omitted_branch_entry_count: 0,
        };
    }

    let Some(active_leaf) = active_leaf_id.as_deref() else {
        return ExportEntrySelection {
            entries: entries.to_vec(),
            active_leaf_id,
            source_entry_count,
            omitted_branch_entry_count: 0,
        };
    };
    let active_path = crate::session_tree::get_branch_path(entries, active_leaf);
    if active_path.is_empty() {
        return ExportEntrySelection {
            entries: entries.to_vec(),
            active_leaf_id,
            source_entry_count,
            omitted_branch_entry_count: 0,
        };
    }

    let active_ids: HashSet<String> = active_path
        .iter()
        .filter_map(entry_id)
        .map(str::to_string)
        .collect();
    let mut active_scope_ids = active_ids.clone();
    active_scope_ids.insert(session_id.to_string());
    let mut selected = Vec::with_capacity(entries.len());

    for entry in entries {
        let entry_type = entry
            .get("type")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        let id = entry_id(entry);
        let parent = entry_parent_id(entry);
        let is_branch_record = matches!(entry_type, "leaf_pointer" | "label" | "branch_summary");
        let is_global =
            parent.is_none_or(|parent_id| parent_id.is_empty() || parent_id == session_id);
        let include = if is_message_entry(entry) {
            id.is_some_and(|entry_id| active_ids.contains(entry_id))
        } else if is_branch_record {
            true
        } else {
            id.is_some_and(|entry_id| active_ids.contains(entry_id))
                || is_global
                || parent.is_some_and(|parent_id| active_scope_ids.contains(parent_id))
        };

        if include {
            selected.push(entry.clone());
            if !is_branch_record && let Some(entry_id) = id {
                active_scope_ids.insert(entry_id.to_string());
            }
        }
    }

    ExportEntrySelection {
        omitted_branch_entry_count: source_entry_count.saturating_sub(selected.len()),
        entries: selected,
        active_leaf_id,
        source_entry_count,
    }
}

/// Export a session to HTML using pi's template system.
///
/// Resolves session by:
/// 1. Looking up session index (if available) for cwd
/// 2. Falling back to flat `sessions/{id}.jsonl` (legacy)
/// 3. Falling back to `sessions/--hash--id--/session.jsonl` (treat id as cwd)
/// 4. Scanning all session dirs for matching header id
///
/// `tools` is optional — when provided (e.g. CLI ran an agent then exports),
/// the HTML shows an "Available Tools" panel. When None (standalone --export),
/// the panel is hidden. This matches pi's `exportSessionToHtml` which takes
/// `state.tools`; pi's standalone `exportFromFile` also has no tools.
pub fn export_session(
    session_id: &str,
    output_path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    export_session_with_tools(session_id, output_path, None)
}

/// Export a session with tools + system prompt extracted from the agent config.
///
/// This is used by the standalone `--export` CLI path (no agent run). It reads
/// the session header to find the agent name, loads the agent's tool list and
/// system prompt, then delegates to export_session_with_tools_and_prompt.
///
/// If the session has no agent name or the agent config is not found, falls
/// back to a plain export with no tools and no system prompt.
pub fn export_session_rich(
    session_id: &str,
    output_path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    // Read session file to get header
    let jsonl_path = resolve_session_file(session_id)?;
    let content = std::fs::read_to_string(&jsonl_path)?;
    let header: Value = content
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|entry| entry.get("type").and_then(Value::as_str) == Some("session"))
        .unwrap_or_else(|| json!({}));

    // Extract agent name from header
    let agent_name = header.get("agent").and_then(|v| v.as_str());

    // Load agent config to get tools + system prompt
    let mut tools: Option<Vec<ExportToolInfo>> = None;
    let mut system_prompt: Option<String> = None;

    if let Some(name) = agent_name
        && let Some(agent_cfg) = crate::agent_config::find_agent(name)
    {
        // Get system prompt from agent config
        let mut sp = agent_cfg.system_prompt.clone().unwrap_or_default();
        // 追加 skill 大纲（对齐 cmd_run 的 system prompt 构建逻辑，让 export 出来的
        // HTML 也能看到 skill 列表）。扫描 ~/.agents/skills + ~/.ion/skills + 项目级 + ZCode plugins。
        let home = std::env::var("HOME").unwrap_or_default();
        let cwd = header.get("cwd").and_then(|v| v.as_str()).unwrap_or(".");
        let mut skill_dirs: Vec<std::path::PathBuf> = vec![
            crate::paths::skills_dir(),
            crate::paths::project_skills_dir(cwd),
            std::path::PathBuf::from(&home)
                .join(".agents")
                .join("skills"),
        ];
        // ZCode plugin skills
        let plugins_cache = std::path::PathBuf::from(&home).join(".zcode/cli/plugins/cache");
        if plugins_cache.exists()
            && let Ok(mp_iter) = std::fs::read_dir(&plugins_cache)
        {
            for mp_entry in mp_iter.flatten() {
                if let Ok(plugin_iter) = std::fs::read_dir(mp_entry.path()) {
                    for plugin_entry in plugin_iter.flatten() {
                        if let Ok(ver_iter) = std::fs::read_dir(plugin_entry.path()) {
                            for ver_entry in ver_iter.flatten() {
                                let sd = ver_entry.path().join("skills");
                                if sd.is_dir() {
                                    skill_dirs.push(sd);
                                }
                            }
                        }
                    }
                }
            }
        }
        // 注入环境信息（cwd/git/最近 commit/最近修改文件）——用 session header 的 cwd
        let session_cwd = header.get("cwd").and_then(|v| v.as_str()).unwrap_or(".");
        sp.push_str(&build_env_info_for_export(session_cwd));

        // 注入 bash tool guide（export 不跑 agent loop，直接注入）
        sp.push_str(&crate::agent::bash::bash_tool_guide());

        // 注入项目 rules（只注入全局 rule 到 system prompt，跟实时 agent on_system_prompt 一致）
        // 路径匹配 rule 在实时 agent 里追加到 tool result（session.jsonl 已记录），不进 system prompt。
        {
            let rules_ext = crate::rules_engine::RulesEngineExtension::with_project_dir(
                std::path::PathBuf::from(session_cwd),
            );
            let rules = rules_ext.load_rules();
            // 只注入全局 rule（applyTo 为空或 **/* 或 **）
            let global_rules: Vec<crate::rules_engine::Rule> = rules
                .iter()
                .filter(|r| {
                    r.apply_to.is_empty() || r.apply_to.iter().any(|p| p == "**/*" || p == "**")
                })
                .cloned()
                .collect();
            if !global_rules.is_empty() {
                sp.push_str(
                    &crate::rules_engine::RulesEngineExtension::format_rules_xml(&global_rules),
                );
            }
        }

        let skill_tool = crate::agent::tool::SkillTool {
            skill_dirs: skill_dirs.clone(),
            disabled: crate::config::IonConfig::load().skills.disabled,
        };
        let outline = skill_tool.list_skills();
        if !outline.contains("No skills available") {
            sp.push_str("\n\n--- available-skills ---\n");
            sp.push_str(&outline);
        }
        // Inject available-agents outline (mirrors cmd_run's system prompt).
        let agents_outline = crate::agent_config::agents_outline();
        if !agents_outline.is_empty() {
            sp.push_str("\n\n--- available-agents ---\n");
            sp.push_str(&agents_outline);
        }
        system_prompt = Some(sp);

        // Reconstruct tool definitions by instantiating all built-in tools,
        // then applying the agent config's allowlist and blocklist.
        let mut registry = crate::agent::tool::ToolRegistry::new();
        registry.register_builtins();
        // SkillTool is skipped by register_builtins() (it requires skill_dirs),
        // so register it explicitly here for the export tools panel.
        registry.register(Box::new(crate::agent::tool::SkillTool {
            skill_dirs: skill_dirs.clone(),
            disabled: crate::config::IonConfig::load().skills.disabled,
        }));
        // Let BashExtension self-describe its tools (bash/get_background_process/
        // bash_send/bash_bg). Uses a dummy instance — register_tools only
        // clones Arc fields, never executes commands during export.
        let bash_ext: &dyn crate::agent::extension::Extension =
            &crate::agent::bash::BashExtension::new_for_export();
        bash_ext.register_tools(&mut registry);

        // Apply allowlist: agent_cfg.tools is a list of tool names
        if let Some(ref allowed) = agent_cfg.tools {
            let allowed_refs: Vec<&str> = allowed.iter().map(|s| s.as_str()).collect();
            registry.filter(allowed_refs);
        }

        // Apply blocklist: agent_cfg.disallowed_tools
        if let Some(ref blocked) = agent_cfg.disallowed_tools {
            for name in blocked {
                registry.remove(name);
            }
        }

        // Convert to ExportToolInfo list（按类型分组 + 字母序，避免 HashMap 随机顺序）
        let mut defs: Vec<ExportToolInfo> = registry
            .tool_defs()
            .into_iter()
            .map(|td| ExportToolInfo {
                name: td.name,
                description: td.description,
                parameters: td.parameters,
            })
            .collect();
        defs.sort_by(|a, b| {
            fn group(name: &str) -> u8 {
                if name.starts_with("mcp__") {
                    6
                } else if name.starts_with("wasm_") {
                    5
                } else if matches!(
                    name,
                    "spawn_worker"
                        | "send_to_worker"
                        | "resume_worker"
                        | "await_worker"
                        | "channel_send"
                        | "kill_worker"
                ) {
                    4
                } else if name.starts_with("goal_") {
                    3
                } else if name == "skill" {
                    2
                } else if name.starts_with("git_") {
                    1
                } else {
                    0
                }
            }
            (group(&a.name), &a.name).cmp(&(group(&b.name), &b.name))
        });

        if !defs.is_empty() {
            tools = Some(defs);
        }
    } else {
        // Default agent (e.g. "build") has no .md config file, so find_agent()
        // returned None and the block above was skipped. Reconstruct the full
        // built-in tool set (no allowlist filter) so the export tools panel
        // still shows the tools the agent actually had available.
        let home = std::env::var("HOME").unwrap_or_default();
        let cwd = header.get("cwd").and_then(|v| v.as_str()).unwrap_or(".");
        let skill_dirs: Vec<std::path::PathBuf> = vec![
            crate::paths::skills_dir(),
            crate::paths::project_skills_dir(cwd),
            std::path::PathBuf::from(&home)
                .join(".agents")
                .join("skills"),
        ];
        let mut registry = crate::agent::tool::ToolRegistry::new();
        registry.register_builtins();
        // SkillTool requires skill_dirs — register explicitly (same as above).
        registry.register(Box::new(crate::agent::tool::SkillTool {
            skill_dirs,
            disabled: crate::config::IonConfig::load().skills.disabled,
        }));
        // BashExtension registers the unified `bash` tool (sync + background).
        let bash_ext: &dyn crate::agent::extension::Extension =
            &crate::agent::bash::BashExtension::new_for_export();
        bash_ext.register_tools(&mut registry);
        let mut defs: Vec<ExportToolInfo> = registry
            .tool_defs()
            .into_iter()
            .map(|td| ExportToolInfo {
                name: td.name,
                description: td.description,
                parameters: td.parameters,
            })
            .collect();
        defs.sort_by(|a, b| a.name.cmp(&b.name));
        if !defs.is_empty() {
            tools = Some(defs);
        }

        // 构造最小 system prompt（含 env_info + bash_tool_guide），让导出 HTML 能显示
        // ION Version / Platform / Git Branch / 最近 commit 等元信息。
        // 之前没这段：standalone ion --export（没 --agent 参数）走这个 else 分支，
        // system_prompt 保持 None，HTML 里 systemPrompt 字段为空，无法追溯生成版本。
        let session_cwd = header.get("cwd").and_then(|v| v.as_str()).unwrap_or(".");
        let mut sp = String::new();
        sp.push_str(&build_env_info_for_export(session_cwd));
        sp.push_str(&crate::agent::bash::bash_tool_guide());
        system_prompt = Some(sp);
    }

    // Delegate to internal export with tools + system_prompt
    export_session_with_tools_and_prompt(session_id, output_path, tools, system_prompt)
}

/// Export with optional tools list and optional system prompt override.
///
/// `override_system_prompt` is injected into the session data as systemPrompt
/// if the session itself does not already contain one (e.g. from fork sub-workers).
pub fn export_session_with_tools_and_prompt(
    session_id: &str,
    output_path: &Path,
    tools: Option<Vec<ExportToolInfo>>,
    override_system_prompt: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    export_session_internal(session_id, output_path, tools, override_system_prompt)
}

/// Export with optional tools list (called by CLI when an Agent has run).
/// 入口函数：自动导出关联的 fork 子 session HTML。
pub fn export_session_with_tools(
    session_id: &str,
    output_path: &Path,
    tools: Option<Vec<ExportToolInfo>>,
) -> Result<(), Box<dyn std::error::Error>> {
    export_session_internal(session_id, output_path, tools, None)
}

/// 内部导出函数（不自动导出子 session，避免递归）。
/// 被外部入口和自动子 session 导出调用。
fn export_session_internal(
    session_id: &str,
    output_path: &Path,
    tools: Option<Vec<ExportToolInfo>>,
    override_system_prompt: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Try to find the session file
    let jsonl_path = resolve_session_file(session_id)?;

    // Read JSONL file
    let content = std::fs::read_to_string(&jsonl_path)?;
    let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.is_empty() {
        return Err("empty session file".into());
    }

    // 找 session header（type=session），不在第一行也行（cmd_run 的 per-session 文件可能没有 header）
    let (header, raw_start): (Value, usize) = {
        let mut found_header: Option<Value> = None;
        let mut header_line = 0;
        for (i, line) in lines.iter().enumerate() {
            if let Ok(val) = serde_json::from_str::<Value>(line) {
                if val.get("type").and_then(|v| v.as_str()) == Some("session") {
                    found_header = Some(val);
                    header_line = i;
                    break;
                }
            }
        }
        match found_header {
            Some(h) => (h, header_line + 1),
            None => {
                // No session header found — create synthetic one from file name
                let sid = jsonl_path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("unknown");
                let ts = session_jsonl::timestamp_iso();
                (
                    json!({"type": "session", "version": 3, "id": sid, "timestamp": ts, "cwd": "."}),
                    0, // include all lines as entries (even if first is session_name)
                )
            }
        }
    };
    let mut raw_entries: Vec<Value> = lines[raw_start..]
        .iter()
        .enumerate()
        .map(|(offset, line)| {
            serde_json::from_str(line).map_err(|error| {
                format!(
                    "invalid session JSONL at {}:{}: {}",
                    jsonl_path.display(),
                    raw_start + offset + 1,
                    error
                )
            })
        })
        .collect::<Result<Vec<_>, _>>()?;

    // ── 合并 fork 子 session 的 entries ──
    // 扫同目录下的 <sid>.jsonl 文件，找 parentSession == 当前 session_id 的，
    // 把它们的 entries 用 system_event 分隔标记追加进来。
    // 这样用户在一个 HTML 里能看到"主 Worker 调 skill fork → 子 Worker 干了什么"。
    // 子 session 只在 export 主 session 时合并（避免循环）。
    let _session_type = header.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let is_main_session = header
        .get("parentSession")
        .and_then(|v| v.as_str())
        .is_none()
        && !header.get("spawnMeta").is_some();
    if is_main_session
        && let Some(parent_dir) = jsonl_path.parent()
        && let Ok(files) = std::fs::read_dir(parent_dir)
    {
        for file in files.flatten() {
            let path = file.path();
            let name = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n,
                None => continue,
            };
            // 只扫 <sid>.jsonl，跳过 session.jsonl（自己）+ memory_agent + input
            if name == "session.jsonl"
                || name.starts_with("sess_memory_agent")
                || name == "input.jsonl"
            {
                continue;
            }
            if !name.ends_with(".jsonl") {
                continue;
            }

            // 读子 session header，检查 parentSession 是否匹配
            let sub_content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let mut sub_lines = sub_content.lines().filter(|l| !l.trim().is_empty());
            let sub_header_line = match sub_lines.next() {
                Some(l) => l,
                None => continue,
            };
            let sub_header: Value = match serde_json::from_str(sub_header_line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let parent_match =
                sub_header.get("parentSession").and_then(|v| v.as_str()) == Some(session_id);
            if !parent_match {
                continue;
            }

            // 匹配！自动导出子 session HTML + 在主 HTML 里放可点击链接
            let sub_sid = sub_header
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("?")
                .to_string();
            let spawn_meta = sub_header.get("spawnMeta").cloned();
            let spawned_by = spawn_meta
                .as_ref()
                .and_then(|m| m.get("spawnedBy"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            // relation 字符串 → WorkerRelation enum（serde lowercase: child/peer/system）
            let relation_enum = spawn_meta
                .as_ref()
                .and_then(|m| m.get("relation"))
                .map(|v| serde_json::from_value::<WorkerRelation>(v.clone()).unwrap_or_default())
                .unwrap_or_default();
            // 根据 (spawned_by, relation) 判定子 Worker 类型（影响文案 + HTML 文件名前缀）
            let (sub_kind_label, sub_kind_en) = match (spawned_by, relation_enum) {
                ("skill_fork", _) => ("Fork 子 Worker（skill fork）", "fork"),
                ("singleton_init", _) | (_, WorkerRelation::System) => {
                    ("System 子 Worker（常驻）", "system")
                }
                (_, WorkerRelation::Peer) => ("Peer 子 Worker（同级异步）", "peer"),
                _ => ("Spawn 子 Worker（派发）", "spawn"),
            };
            let relation = match relation_enum {
                WorkerRelation::Child => "child",
                WorkerRelation::Peer => "peer",
                WorkerRelation::System => "system",
            };

            // 自动导出子 session HTML（跟主 HTML 同目录，文件名 sub_<sid>.html）
            let sub_html_name = sub_html_filename(&sub_sid);
            let sub_html_path = output_path
                .parent()
                .map(|p| p.join(&sub_html_name))
                .unwrap_or_else(|| std::path::PathBuf::from(&sub_html_name));

            // 递归导出子 session（但不深度递归——只导一层子 session）。
            // 用 export_session_rich（而非 export_session_internal）：子 worker session
            // header 现在带 agent 字段（ensure_fork_session_header 写入），rich 路径会
            // 读 header.agent 加载 agent config 的 system prompt + tools，让子 worker HTML
            // 也能显示 system prompt 和工具面板。
            match export_session_rich(&sub_sid, &sub_html_path) {
                Ok(()) => {
                    eprintln!(
                        "[export] auto-exported {sub_kind_en} sub-session → {}",
                        sub_html_path.display()
                    );
                }
                Err(e) => {
                    eprintln!(
                        "[export] WARN: failed to auto-export {sub_kind_en} sub-session {sub_sid}: {e}"
                    );
                }
            }

            // 分隔标记：content 里有可点击的 HTML 链接（指向子 HTML 文件）
            let sub_sid_short = &sub_sid[..12.min(sub_sid.len())];
            let sub_html_file = sub_html_filename(&sub_sid);
            let separator_content = format!(
                "🔗 {sub_kind_label} session（{sub_sid_short}）\n\
                 relation: {relation} | spawnedBy: {spawned_by}\n\
                 子 session ID: {sub_sid}\n\n\
                 👆 点击查看完整子 Worker 执行过程：{sub_html_file}\n\n\
                 （或命令行导出：ion --export sub.html --session {sub_sid}）"
            );
            let separator = json!({
                "type": "custom_message",
                "id": format!("sub-sep-{}", sub_sid),
                "parentId": null,
                "timestamp": sub_header.get("timestamp").cloned().unwrap_or(json!("")),
                "customType": session_jsonl::CUSTOM_TYPE_SUB_SESSION_SEPARATOR,
                "content": separator_content,
                "data": {
                    "subSessionId": sub_sid,
                    "spawnedBy": spawned_by,
                    "relation": relation,
                    "kind": sub_kind_en,
                    "htmlFile": sub_html_file,
                },
                "display": true,
            });
            raw_entries.push(separator);

            // 不追加子 session 的 entries——子 Worker 是独立进程，有独立的 <sid>.jsonl
            // 和独立的 HTML。主 HTML 只显示主 Worker 的对话流程 + 分隔链接。
            // 用户可以用 subSessionId 单独 export 子 session：
            //   ion --export sub.html --session <subSessionId>
        }
    }

    // 先按 Session Tree 选择当前 active branch。线性会话保留 JSONL 的全部数据；
    // 分支会话只保留 root→active leaf 的消息、全局审计 entry，以及每次分叉的
    // leaf_pointer / label / branch_summary 记录。被废弃分支的正文不应重新出现。
    let all_raw_entries = raw_entries.clone();
    let header_session_id = header
        .get("id")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let branch_selection = select_entries_for_export(&raw_entries, header_session_id);
    raw_entries = branch_selection.entries;

    // Preserve the selected branch in its original order and attach a read-only
    // semantic layer for the offline audit UI. `sourceEntries` remains the
    // canonical selected JSONL stream; Timeline and body are derived from the
    // same annotations.
    let source_entries = annotate_export_entries(&raw_entries);
    let flow_summary = build_export_flow_summary(&source_entries);

    // 当前格式没有脱离消息树的内部回合记录；所有所选 entry 都进入正文与 Timeline。
    let (visible_raw_entries, internal_entries) = partition_export_entries(source_entries.clone());
    raw_entries = visible_raw_entries;

    // Convert ION Rust-enum format → pi flat format.
    // Timeline 是所选正文分支的完整可见事件索引：除嵌套在 Assistant 卡片里的
    // ToolResult 外，每条 timeline entry 都必须在正文里有可见卡片和稳定锚点。
    let timeline_entry_count = raw_entries.len();
    let timeline_entries = raw_entries.clone();
    let mut entries: Vec<Value> = raw_entries.iter().map(convert_entry).collect();

    // ★ pi template 的 renderEntry 要求 entry.type === "custom_message" 才渲染 hook-message 卡片。
    // 之前 ION 把 Custom 变体 message 扁平化为 type:"message" + role:"custom" →
    // pi template 不识别 → bash_result / dev_servers / diagnostics 全部不显示。
    // 修复：Custom 变体 → 把 type 改为 "custom_message" + 把 customType / content 提到顶层。
    for e in entries.iter_mut() {
        let is_custom_message = e
            .get("message")
            .and_then(|m| m.get("role"))
            .and_then(|v| v.as_str())
            == Some("custom");
        if !is_custom_message {
            continue;
        }
        if let Some(obj) = e.as_object_mut() {
            // 先 clone 出 message 内层字段（避免 borrow 冲突）
            let (ct, content, display, details) = obj
                .get("message")
                .and_then(|v| v.as_object())
                .map(|m| {
                    (
                        m.get("customType").cloned(),
                        m.get("content").cloned(),
                        m.get("display").cloned(),
                        m.get("details").cloned(),
                    )
                })
                .unwrap_or((None, None, None, None));
            if let Some(ct) = ct {
                obj.insert("customType".to_string(), ct);
            }
            if let Some(content) = content {
                obj.insert("content".to_string(), content);
            }
            if let Some(display) = display {
                obj.insert("display".to_string(), display);
            }
            if let Some(details) = details {
                obj.insert("details".to_string(), details);
            }
            // type 改为 custom_message
            obj.insert("type".to_string(), json!("custom_message"));
        }
    }

    // 重建 parentId 链：让所有 entries 串成一条线。
    // pi template 的主体内容只显示 getPath(leafId) 返回的 parentId 链上的 entries。
    // ION 的 session 可能有多条 parentId 链（审计 entry 的 parentId=None、增量
    // save 的 parentId 不连续），导致部分 entries 不在路径上 → 主体内容看不到。
    // 修复：按原始顺序，每个 entry 的 parentId 指向前一个 entry 的 id，
    // 这样 getPath(leafId) 能返回所有 entries。
    if entries.len() > 1 {
        let header_id = header
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        for i in 0..entries.len() {
            let parent = if i == 0 {
                header_id.clone()
            } else {
                entries[i - 1]
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&header_id)
                    .to_string()
            };
            if let Some(obj) = entries[i].as_object_mut() {
                obj.insert("parentId".to_string(), json!(parent));
            }
        }
    }

    // 找 systemPrompt：优先从 sidecar 文件读（agent loop 运行时缓存的最终版，
    // 含所有扩展动态注入如 <dev_servers>）。Sidecar 是 <sid>.system-prompt.txt，
    // 跟 session JSONL 同目录。Sidecar 不会被 SessionHeader::save 覆盖（独立文件）。
    let system_prompt: Option<String> = {
        // Sidecar: same dir + base name as the session JSONL, but .system-prompt.txt
        // Written by agent_loop after on_system_prompt hooks run (contains the
        // real prompt with all dynamic injections like <dev_servers>).
        let sidecar = jsonl_path.with_extension("system-prompt.txt");
        std::fs::read_to_string(&sidecar)
            .ok()
            .filter(|s| !s.is_empty())
    }
    .or_else(|| {
        // Fallback: legacy — 从 session JSONL 里找 custom entry
        all_raw_entries.iter().rev().find_map(|e| {
            if e.get("type").and_then(|v| v.as_str()) == Some("custom")
                && e.get("customType").and_then(|v| v.as_str())
                    == Some(session_jsonl::CUSTOM_TYPE_SYSTEM_PROMPT)
            {
                e.get("data")
                    .and_then(|d| d.get("systemPrompt"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            } else {
                None
            }
        })
    });

    // Find leaf id — template uses getPath(leafId) 决定主体内容显示哪些 entry。
    // 必须取最后一个 entry（含 sub_session_separator 这种末尾 custom_message），
    // 否则末尾的 separator 不在 getPath 路径上 → 不会渲染。
    // 优先取最后一个 entry；若为空则回退到最后一个 message。
    let leaf_id = entries
        .last()
        .or_else(|| {
            entries
                .iter()
                .rev()
                .find(|e| e.get("type").and_then(|v| v.as_str()) == Some("message"))
        })
        .and_then(|e| e.get("id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    // Build SessionData JSON (matching pi's format)
    // 给 header 注入 ionVersion 字段，让顶部信息卡片能显示生成 HTML 的 ion 版本。
    // 不修改 session.jsonl 落盘的 header（只在 export 时注入到 session_data）。
    // 生成 session name（标题）：
    // 1. 优先从 session.jsonl entries 里找最后一条 session_name custom_message entry
    //    （历史路径：cmd_run 之前会写这种 entry）
    // 2. 找不到 → 从 SessionIndex 读（cmd_run 现在只更新 SessionIndex.set_name，
    //    AutoSessionTitle 扩展也写这里，含 LLM 生成的简短标题）
    // 3. 还找不到 → fallback 到首条 user message（截断到 60 字符）
    let session_id_for_lookup = header.get("id").and_then(|v| v.as_str()).unwrap_or("");
    // SessionIndex 是会话级元信息的权威入口。把命中的快照随导出数据一起带上，
    // 让离线 HTML 和后续可视化无需再次扫描索引文件；消息角色明细等索引中
    // 尚未细分的统计仍由当前 session entries 精确计算。
    let session_index_meta = if session_id_for_lookup.is_empty() {
        None
    } else {
        crate::session_index::SessionIndex::load()
            .sessions
            .get(session_id_for_lookup)
            .cloned()
    };
    // 提取 token 统计（在 session_index_meta 被 move 之前）
    let total_token_input = session_index_meta.as_ref().map(|m| m.token_input).unwrap_or(0);
    let total_token_output = session_index_meta.as_ref().map(|m| m.token_output).unwrap_or(0);
    let session_name = all_raw_entries
        .iter()
        .rev()
        .find_map(|e| {
            // 兼容两种格式：
            //   - 老: type=session_name, name=...
            //   - 新: type=custom_message, customType=session_name, content="📝 Session title: ..."
            let is_session_name_entry = e.get("type").and_then(|v| v.as_str())
                == Some("session_name")
                || (e.get("type").and_then(|v| v.as_str()) == Some("custom_message")
                    && e.get("customType").and_then(|v| v.as_str()) == Some("session_name"));
            if !is_session_name_entry {
                return None;
            }
            // 优先用 name 字段，没有就从 content 解析 "📝 Session title: XXX"
            if let Some(name) = e.get("name").and_then(|v| v.as_str()) {
                return Some(name.to_string());
            }
            if let Some(content) = e.get("content").and_then(|v| v.as_str()) {
                // 去掉 "📝 Session title: " 前缀
                let prefix = "📝 Session title: ";
                if let Some(stripped) = content.strip_prefix(prefix) {
                    return Some(stripped.to_string());
                }
                return Some(content.to_string());
            }
            None
        })
        .or_else(|| {
            // ★ 新增：从 SessionIndex 读 cmd_run / AutoSessionTitle 写的 name
            // （之前只看 session.jsonl 的 session_name entry，但 cmd_run 改成
            // 不写 entry 后这里就拿不到了，导致 HTML title 显示成 prompt 片段）
            if session_id_for_lookup.is_empty() {
                return None;
            }
            session_index_meta
                .as_ref()
                .and_then(|meta| meta.name.clone())
                .filter(|n| !n.trim().is_empty())
        })
        .or_else(|| {
            // Fallback: 从首条 user message 生成
            all_raw_entries.iter().find_map(|e| {
                if e.get("type").and_then(|v| v.as_str()) != Some("message") {
                    return None;
                }
                let msg = e.get("message")?;
                let user_msg = msg.get("User")?;
                let content = user_msg.get("content")?;
                let arr = content.as_array()?;
                for block in arr {
                    if let Some(text) = block
                        .get("Text")
                        .and_then(|t| t.get("text"))
                        .and_then(|t| t.as_str())
                    {
                        let trimmed = text.trim();
                        if !trimmed.is_empty() {
                            let title: String = trimmed
                                .chars()
                                .take(60)
                                .map(|c| if c.is_whitespace() { ' ' } else { c })
                                .collect();
                            let title = title.trim();
                            if !title.is_empty() {
                                return Some(title.to_string());
                            }
                        }
                    }
                }
                None
            })
        })
        .unwrap_or_else(|| {
            header
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("Session")
                .to_string()
        });

    let mut header_for_export = header.clone();
    if let Some(obj) = header_for_export.as_object_mut() {
        let ion_full_version = format!(
            "{}+{} ({})",
            env!("CARGO_PKG_VERSION"),
            env!("ION_GIT_HASH"),
            env!("ION_BUILD_DATE"),
        );
        obj.insert("ionVersion".to_string(), json!(ion_full_version));
        // 注入 session name（从第一条 user message 提取）
        obj.insert("name".to_string(), json!(session_name));
        if let Some(meta) = session_index_meta {
            obj.insert(
                "indexMeta".to_string(),
                serde_json::to_value(meta).unwrap_or(Value::Null),
            );
        }
    }
    let mut session_data = json!({
        "header": header_for_export,
        "entries": entries,
        "timelineEntries": timeline_entries,
        "internalEntries": internal_entries,
        "sourceEntries": source_entries,
        "flowSummary": flow_summary,
        "leafId": leaf_id,
        "activeLeafId": branch_selection.active_leaf_id,
        "sourceEntryCount": branch_selection.source_entry_count,
        "omittedBranchEntryCount": branch_selection.omitted_branch_entry_count,
    });
    // systemPrompt（fork 子 Worker 的 skill 内容，让 HTML 顶部能显示）
    // If the session data already has a system_prompt (from fork sub-workers), use it.
    // Otherwise fall back to the override_system_prompt (from agent config, for main sessions).
    let effective_system_prompt = system_prompt.or(override_system_prompt);
    if let Some(sp) = effective_system_prompt {
        session_data
            .as_object_mut()
            .map(|o| o.insert("systemPrompt".to_string(), json!(sp)));
    }
    // Only include tools when provided (matches pi: undefined → panel hidden)
    if let Some(tools) = tools {
        session_data.as_object_mut().map(|o| {
            o.insert(
                "tools".to_string(),
                serde_json::to_value(&tools).unwrap_or(Value::Null),
            )
        });
    }

    // ── Flatten Rust enum externally-tagged message format for pi template ──
    // ion stores messages as {"message": {"User": {"role":...,"content":[...]}}}
    // (Rust enum externally-tagged serialization), but pi's template.js expects
    // the flat format {"message": {"role":...,"content":[...]}}. Without this
    // flattening, entry.message.role is undefined and template.js renders nothing.
    // We only transform the export output, not the on-disk session format.
    if let Some(entries) = session_data
        .get_mut("entries")
        .and_then(|v| v.as_array_mut())
    {
        const MSG_WRAPPERS: &[&str] = &[
            "User",
            "Assistant",
            "ToolResult",
            "BashExecution",
            "Custom",
            "BranchSummary",
            "CompactionSummary",
        ];
        // content block: {"Text":{"text":...}} -> {"type":"text","text":...}
        const BLOCK_MAP: &[(&str, &str)] = &[
            ("Text", "text"),
            ("ToolUse", "toolCall"),
            ("ToolCall", "toolCall"),
            ("ToolResult", "toolResult"),
            ("Thinking", "thinking"),
            ("Image", "image"),
        ];

        for entry in entries.iter_mut() {
            // Step 1: flatten message wrapper {"User":{role,content}} -> {role,content}
            let is_message = entry.get("type").and_then(|v| v.as_str()) == Some("message");
            if is_message {
                if let Some(msg) = entry.get_mut("message").and_then(|m| m.as_object_mut()) {
                    let wrappers: Vec<String> = msg
                        .keys()
                        .filter(|k| MSG_WRAPPERS.contains(&k.as_str()))
                        .cloned()
                        .collect();
                    for wrapper in wrappers {
                        if let Some(inner) = msg.remove(&wrapper) {
                            if let Some(inner_obj) = inner.as_object() {
                                for (k, v) in inner_obj {
                                    msg.insert(k.clone(), v.clone());
                                }
                            }
                        }
                    }

                    // Step 2: flatten content blocks {Text:{text}} -> {type:"text",text}
                    if let Some(content) = msg.get_mut("content").and_then(|c| c.as_array_mut()) {
                        for block in content.iter_mut() {
                            if let Some(block_obj) = block.as_object_mut() {
                                for (wrapper, type_name) in BLOCK_MAP {
                                    if let Some(inner) = block_obj.remove(*wrapper) {
                                        block_obj.insert(
                                            "type".to_string(),
                                            Value::String(type_name.to_string()),
                                        );
                                        if let Some(inner_obj) = inner.as_object() {
                                            for (k, v) in inner_obj {
                                                block_obj.insert(k.clone(), v.clone());
                                            }
                                        }
                                        break;
                                    }
                                }
                            }
                        }
                    }

                    // Step 3: Custom messages (e.g. <bash_result>) — pi template.js
                    // only renders user/assistant/toolResult/bashExecution roles.
                    // Custom messages are system-injected (like background process
                    // completion), semantically equivalent to user messages.
                    // Remap role=custom -> role=user and wrap string content into
                    // the array format pi expects.
                    let is_custom = msg
                        .get("role")
                        .and_then(|v| v.as_str())
                        .map(|r| r == "custom")
                        .unwrap_or(false);
                    if is_custom {
                        msg.insert("role".to_string(), Value::String("user".into()));
                        // Wrap string content into [{type:"text",text:"..."}]
                        if let Some(content_val) = msg.remove("content") {
                            if !content_val.is_array() {
                                msg.insert(
                                    "content".to_string(),
                                    serde_json::json!([{"type":"text","text":content_val}]),
                                );
                            } else {
                                msg.insert("content".to_string(), content_val);
                            }
                        }
                    }

                    // Step 4: BashExecution 字段名 snake_case → camelCase（pi 模板对齐）。
                    // ion 落盘：exit_code / full_output_path / exclude_from_context
                    // pi 模板读：exitCode / fullOutputPath / excludeFromContext
                    // 不转换的话模板显示 "(exit undefined)"（因为 msg.exitCode === undefined）。
                    let is_bash_exec = msg
                        .get("role")
                        .and_then(|v| v.as_str())
                        .map(|r| r == "bashExecution")
                        .unwrap_or(false);
                    if is_bash_exec {
                        const SNAKE_TO_CAMEL: &[(&str, &str)] = &[
                            ("exit_code", "exitCode"),
                            ("full_output_path", "fullOutputPath"),
                            ("exclude_from_context", "excludeFromContext"),
                        ];
                        for (snake, camel) in SNAKE_TO_CAMEL {
                            if let Some(val) = msg.remove(*snake) {
                                msg.insert(camel.to_string(), val);
                            }
                        }
                    }
                }
            }
        }
    }

    // Base64 encode
    let session_data_json = serde_json::to_string(&session_data)?;
    let session_data_b64 = base64_encode(&session_data_json);

    // Compile-time embedded assets keep the generated HTML portable and make a
    // missing renderer a build error instead of a silent empty export.
    let css = EXPORT_TEMPLATE_CSS.to_string()
        + r#"
/* ION: only collapse tool output when opening it reveals more than three lines. */
.tool-output:not(.expandable)[data-ion-output-foldable="true"]:not(.expanded) {
  max-height: var(--ion-tool-output-preview-height, 100px);
  overflow: hidden;
  position: relative;
  cursor: pointer;
}
.tool-output:not(.expandable)[data-ion-output-foldable="true"]:not(.expanded)::after {
  content: '▼ 查看剩余 ' attr(data-ion-output-hidden-lines) ' 行';
  position: absolute;
  bottom: 0; left: 0; right: 0;
  height: 20px;
  background: linear-gradient(transparent, rgba(246,248,250,0.95) 60%);
  display: flex;
  align-items: center;
  justify-content: center;
  font-size: 11px;
  color: #58a6ff;
}
"#;
    let mut js = EXPORT_TEMPLATE_JS.to_string();
    let marked_js = EXPORT_MARKED_JS;
    let highlight_js = EXPORT_HIGHLIGHT_JS;
    let mut html = EXPORT_TEMPLATE_HTML.to_string();

    // ION 扩展：在 pi template 的 stats 区块（Date/Models/...）最前面插入 Agent 行。
    // session header 已含 agent 字段（ion_worker/ion.rs 写入），缺失时显示 '-'。
    // 不改 pi 源码，运行时字符串替换注入。
    let js_date_anchor = r#"<div class="info-item"><span class="info-label">Date:</span>"#;
    let js_agent_row = r#"<div class="info-item"><span class="info-label">Agent:</span><span class="info-value">${escapeHtml(header?.agent || '-')}</span></div>"#;
    if js.contains(js_date_anchor) {
        js = js.replacen(
            js_date_anchor,
            &format!("{}\n              {}", js_agent_row, js_date_anchor),
            1,
        );
    }

    // ION 扩展：在 Models 行后面追加 ION Version 行，让顶部信息卡片能直接看到生成版本。
    // header.ionVersion 在 export_session_internal 里注入（env!("CARGO_PKG_VERSION")）。
    let js_models_anchor = r#"<div class="info-item"><span class="info-label">Messages:</span>"#;
    let ion_version_str = format!(
        "{}+{} ({})",
        env!("CARGO_PKG_VERSION"),
        env!("ION_GIT_HASH"),
        env!("ION_BUILD_DATE")
    );
    let js_ion_version_row = format!(
        r#"<div class="info-item"><span class="info-label">ION Version:</span><span class="info-value">${{escapeHtml(header?.ionVersion || '{}')}}</span></div>"#,
        ion_version_str
    );
    if js.contains(js_models_anchor) {
        js = js.replacen(
            js_models_anchor,
            &format!("{}\n              {}", js_ion_version_row, js_models_anchor),
            1,
        );
    }

    // ION 扩展：修 Models 显示，没 assistant 消息时 fallback 到 header.model + header.provider。
    // pi 原逻辑：globalStats.models 从 assistant 消息累加，没 assistant 就显示 'unknown'。
    // ion 场景：call_tool 测试/extension_rpc 调试时 session 可能没 assistant 消息，但 header.model 已写。
    let js_models_original = r#"globalStats.models.join(', ') || 'unknown'"#;
    let js_models_fixed = r#"(globalStats.models.size > 0 ? [...globalStats.models].join(', ') : (header?.model ? (header?.provider ? header.provider + '/' + header.model : header.model) : 'unknown'))"#;
    js = js.replace(js_models_original, js_models_fixed);

    // ION: h1 和 title 用 session name（header.name）
    js = js.replace(
        "Session: ${escapeHtml(header?.id || 'unknown')}",
        "${escapeHtml(header?.name || header?.id || 'Session')}",
    );
    // 追加 JS：设置 document.title
    js += "\ntry{document.title=header?.name||header?.id||'Session';}catch(e){}";
    // 追加 JS：给 session_name 卡片加专属 class（让 CSS 能用 .session-name-card 选）
    // pi template 渲染所有 custom_message 都用 .hook-message，没法在 CSS 里区分
    // session_name 跟其他 custom_message（如 sub_session_separator）。
    // 这里在 DOMContentLoaded 后扫描 .hook-type 文本，给 session_name 卡片加 class。
    js += r#"
document.addEventListener('DOMContentLoaded', function() {
  // ── Memory/context injection 折叠 ──
  document.querySelectorAll('.user-message').forEach(function(el) {
    var content = el.querySelector('.markdown-content');
    var text = content ? (content.textContent || '') : (el.textContent || '');
    // 检测 memory/context 注入标记
    if (text.trim().startsWith('<global_memory>') ||
        text.trim().startsWith('<memory_context') ||
        text.trim().startsWith('<goal_feedback>')) {
      el.classList.add('memory-injection');
      el.addEventListener('click', function() {
        el.classList.toggle('expanded');
      });
    }
  });

  document.querySelectorAll('.hook-message, .hook-type').forEach(function(el) {
    var parent = el.classList.contains('hook-message') ? el : el.closest('.hook-message, .custom-message');
    if (!parent) return;
    var typeEl = parent.querySelector('.hook-type');
    if (!typeEl) return;
    // 提取 [xxx] 里的 customType 名（如 [diagnostics] / [bash_result] / [session_name]）
    var t = (typeEl.textContent || '').trim().replace(/^\[|\]$/g, '');
    if (!t) return;
    parent.setAttribute('data-custom-type', t);
    // 如果是 diagnostics，再标 has-errors 属性（区分有错/无错）
    if (t === 'diagnostics') {
      var content = parent.querySelector('.markdown-content');
      var txt = content ? (content.textContent || '') : '';
      var hasErrors = /has_errors="true"|error\[E\d{4}\]|error\(s\)|severity="error"/.test(txt);
      parent.setAttribute('data-has-errors', hasErrors ? 'true' : 'false');
    }
  });
});
"#;

    // pi 只认识少数内置 entry type，遇到 thinking_level_change、agent_change、
    // system_event、deletion 等会直接 return ''。Timeline 需要为完整 entry 流提供
    // 正文锚点，因此把最终 fallback 改成一张紧凑的内置 Entry 卡片。
    // ToolResult 仍由对应 Assistant 的 tool call 卡片承载，避免重复渲染。
    let generic_entry_fallback_old = r#"        return '';
      }

      // ============================================================
      // HEADER / STATS"#;
    let generic_entry_fallback_new = r#"        const genericType = entry.customType || entry.type || 'entry';
        let genericText = '';
        if (entry.type === 'custom' && entry.customType === 'system_prompt') {
          genericText = 'System prompt is rendered in the System Prompt panel above.';
        } else if (typeof entry.content === 'string') {
          genericText = entry.content;
        } else if (typeof entry.summary === 'string' && entry.summary) {
          genericText = entry.summary;
        } else if (entry.data !== undefined) {
          genericText = typeof entry.data === 'string' ? entry.data : JSON.stringify(entry.data, null, 2);
        } else {
          const compactEntry = {...entry};
          delete compactEntry.id;
          delete compactEntry.parentId;
          delete compactEntry.timestamp;
          genericText = JSON.stringify(compactEntry, null, 2);
        }
        genericText = String(genericText || 'No additional details');
        if (genericText.length > 4000) genericText = genericText.slice(0, 4000) + '\n…';
        return `<div class="ion-generic-entry" id="${entryDomId}" data-entry-type="${escapeHtml(genericType)}">${copyBtnHtml}${tsHtml}
          <div class="ion-generic-entry-type">[${escapeHtml(genericType)}]</div>
          <div class="ion-generic-entry-content">${escapeHtml(genericText)}</div>
        </div>`;
      }

      // ============================================================
      // HEADER / STATS"#;
    if js.contains(generic_entry_fallback_old) {
        js = js.replacen(generic_entry_fallback_old, generic_entry_fallback_new, 1);
    } else {
        tracing::warn!("pi export template changed: generic entry fallback anchor not found");
    }

    // ION 扩展：formatExpandableOutput 改成"头 N 行 + 尾 N 行（中间压缩）"。
    // pi 原版只显示头 N 行 + 展开按钮看全部。用户反馈：尾部内容往往更重要
    // （错误信息、最终结果都在末尾），不应该折叠尾部。
    // 新格式：头 maxLines 行 → ...[N lines collapsed]... → 尾 maxLines 行。
    let expandable_old = r#"const displayLines = lines.slice(0, maxLines);
        const remaining = lines.length - maxLines;"#;
    let expandable_new = r#"const headCount = maxLines;
        const minimumHiddenLines = 3;
        const tailCount = Math.min(maxLines, Math.max(0, lines.length - headCount - minimumHiddenLines - 1));
        const displayLines = lines.slice(0, headCount);
        const tailLines = tailCount > 0 ? lines.slice(-tailCount) : [];
        const remaining = lines.length - headCount - tailLines.length;
        const shouldFold = remaining > minimumHiddenLines;"#;
    js = js.replace(expandable_old, expandable_new);
    js = js.replace("if (remaining > 0)", "if (shouldFold)");
    js = js.replace(
        "for (const line of displayLines) {\n          out += `<div>${escapeHtml(replaceTabs(line))}</div>`;\n        }\n        out += '</div>';\n        return out;",
        "for (const line of lines) {\n          out += `<div>${escapeHtml(replaceTabs(line))}</div>`;\n        }\n        out += '</div>';\n        return out;",
    );

    // 在 expandable 输出里追加 tailLines
    let tail_inject_old = r#"<div class="expand-hint">... (${remaining} more lines)</div>"#;
    let tail_inject_new = r#"<div class="expand-hint">... (${remaining} lines collapsed, click to expand)</div></div>
              <div class="output-tail"><pre>${tailLines.length > 0 ? escapeHtml(tailLines.join('\n')) : ''}</pre></div>
              <div style="display:none">"#;
    js = js.replace(tail_inject_old, tail_inject_new);

    // ION 扩展：default 工具不展示参数 JSON（入参对用户没用，只显示工具名 + 结果）。
    let default_args_old = r#"html += `<div class="tool-header"><span class="tool-name">${escapeHtml(name)}</span></div>`;
              html += `<div class="tool-output"><pre>${escapeHtml(JSON.stringify(args, null, 2))}</pre></div>`;
              if (result) {
                const output = getResultText();
                if (output) html += formatExpandableOutput(output, 10);"#;
    let default_args_new = r#"html += `<div class="tool-header"><span class="tool-name">${escapeHtml(name)}</span></div>`;
              if (result) {
                const output = getResultText();
                if (output) html += formatExpandableOutput(output, 10);"#;
    js = js.replace(default_args_old, default_args_new);

    // Replace placeholders
    html = html.replace("{{CSS}}", &css);
    // 用 session name 替换 <title>（template.html 里写死的是 "Session Export"）
    html = html.replace(
        "<title>Session Export</title>",
        &format!("<title>{}</title>", escape_html_text(&session_name)),
    );
    html = html.replace("{{SESSION_DATA}}", &session_data_b64);
    html = html.replace("{{MARKED_JS}}", &marked_js);
    html = html.replace("{{HIGHLIGHT_JS}}", &highlight_js);
    html = html.replace("{{JS}}", &js);
    // pi 的模板依赖一组主题变量。ION 之前把占位符替换成空串，导致大量
    // `var(--text)` / `var(--border)` 声明失效，只能靠浏览器默认值兜底。
    // 导出文件需要离线可读，因此在这里提供一套稳定的浅色主题。
    html = html.replace(
        "{{THEME_VARS}}",
        r#"
      --text: #172033;
      --muted: #667085;
      --dim: #98a2b3;
      --accent: #0e7490;
      --success: #059669;
      --warning: #b45309;
      --error: #dc2626;
      --border: #d9e0e8;
      --borderAccent: #0e7490;
      --selectedBg: #e7f3f5;
      --hover: #f2f6f8;
      --userMessageBg: #eef6ff;
      --userMessageText: #172033;
      --thinkingText: #667085;
      --toolPendingBg: #fff7ed;
      --toolSuccessBg: #ecfdf3;
      --toolErrorBg: #fff1f2;
      --toolOutput: #344054;
      --toolDiffAdded: #067647;
      --toolDiffRemoved: #b42318;
      --toolDiffContext: #667085;
      --customMessageBg: #f4f3ff;
      --customMessageLabel: #6941c6;
      --customMessageText: #344054;
      --mdHeading: #101828;
      --mdLink: #0e7490;
      --mdCode: #344054;
      --mdCodeBlockBorder: #d0d5dd;
      --mdQuote: #475467;
      --mdQuoteBorder: #98a2b3;
      --mdListBullet: #0e7490;
      --mdHr: #e4e7ec;
      --syntaxComment: #667085;
      --syntaxKeyword: #9e165f;
      --syntaxNumber: #175cd3;
      --syntaxString: #067647;
      --syntaxFunction: #6941c6;
      --syntaxType: #b54708;
      --syntaxVariable: #344054;
      --syntaxOperator: #475467;
      --syntaxPunctuation: #667085;
      "#,
    );
    html = html.replace("{{BODY_BG}}", "#fafafa");
    html = html.replace("{{CONTAINER_BG}}", "#ffffff");
    html = html.replace("{{INFO_BG}}", "#f5f5f5");

    // ION 自定义 CSS：给不同角色加淡背景色，提高可读性。
    // 在 </style> 前插入（覆盖 pi template 的默认样式）。
    let ion_custom_css = r#"
    /* ION export shell: self-contained, responsive and optimized for long sessions. */
    :root {
      --ion-shell-max: 1560px;
      --ion-sidebar-width: 320px;
      --ion-radius-sm: 8px;
      --ion-radius-lg: 16px;
      --ion-shadow: 0 1px 2px rgba(16, 24, 40, 0.04), 0 12px 32px rgba(16, 24, 40, 0.06);
      --line-height: 20px;
      --sidebar-width: var(--ion-sidebar-width);
    }
    body {
      min-width: 320px;
      color: var(--text);
      background:
        radial-gradient(circle at 8% 0%, rgba(14, 116, 144, 0.08), transparent 28rem),
        #f3f5f7;
      font-family: "Avenir Next", Avenir, "Segoe UI", sans-serif;
      font-size: 13px;
      line-height: var(--line-height);
    }
    code, pre, .tree-container, .info-value, .tool-output {
      font-family: "SFMono-Regular", "Cascadia Code", Menlo, Consolas, monospace;
    }

    /* Top session masthead */
    #ion-stats-banner {
      color: #f8fafc;
      background:
        radial-gradient(circle at 82% -80%, rgba(45, 212, 191, 0.28), transparent 26rem),
        linear-gradient(112deg, #101828 0%, #17233b 62%, #12394a 100%);
      border-bottom: 1px solid rgba(255, 255, 255, 0.12);
    }
    .ion-stats-inner {
      width: min(100%, var(--ion-shell-max));
      min-height: 112px;
      margin: 0 auto;
      padding: 22px 28px;
      display: grid;
      grid-template-columns: minmax(300px, 1fr) auto;
      gap: 18px 36px;
      align-items: center;
    }
    .ion-title-kicker,
    .ion-overview-kicker {
      display: block;
      margin-bottom: 5px;
      color: #67e8f9;
      font: 700 10px/1.2 "SFMono-Regular", Menlo, monospace;
      letter-spacing: 0.16em;
      text-transform: uppercase;
    }
    .ion-session-title {
      margin: 0;
      color: #fff;
      font-family: Charter, "Iowan Old Style", Georgia, serif;
      font-size: clamp(21px, 2vw, 30px);
      font-weight: 650;
      line-height: 1.25;
      letter-spacing: -0.02em;
      overflow-wrap: anywhere;
    }
    .ion-session-metrics {
      display: flex;
      align-items: stretch;
      gap: 8px;
    }
    .ion-session-metric {
      min-width: 104px;
      padding: 10px 12px;
      border: 1px solid rgba(255, 255, 255, 0.12);
      border-radius: 10px;
      background: rgba(255, 255, 255, 0.06);
      backdrop-filter: blur(8px);
    }
    .ion-session-metric--model { min-width: 180px; }
    .ion-metric-label {
      display: block;
      margin-bottom: 3px;
      color: #9fb0c8;
      font: 700 9px/1.2 "SFMono-Regular", Menlo, monospace;
      letter-spacing: 0.1em;
      text-transform: uppercase;
    }
    .ion-session-metric strong {
      display: block;
      color: #f8fafc;
      font-size: 12px;
      font-weight: 600;
      white-space: nowrap;
    }
    .ion-tool-badges {
      grid-column: 1 / -1;
      display: flex;
      flex-wrap: wrap;
      gap: 6px;
      margin-top: -8px;
    }
    .ion-tool-badge {
      padding: 3px 9px;
      border: 1px solid rgba(103, 232, 249, 0.2);
      border-radius: 999px;
      color: #c8f7ff;
      background: rgba(14, 116, 144, 0.2);
      font: 600 10px/1.4 "SFMono-Regular", Menlo, monospace;
    }

    /* Entry filters sit above an order-proportional, gap-free timeline. */
    #ion-ext-viz {
      width: min(100%, var(--ion-shell-max));
      margin: 18px auto 0;
      padding: 0 28px;
      display: grid;
      grid-template-columns: 1fr;
      gap: 12px;
    }
    .ion-overview-panel {
      min-width: 0;
      padding: 16px 18px;
      border: 1px solid #e1e6ec;
      border-radius: 12px;
      background: rgba(255, 255, 255, 0.92);
      box-shadow: 0 1px 2px rgba(16, 24, 40, 0.03);
    }
    .ion-overview-heading {
      display: flex;
      align-items: baseline;
      gap: 8px;
      margin-bottom: 12px;
    }
    .ion-overview-heading strong {
      color: #101828;
      font-size: 12px;
    }
    .ion-overview-meta {
      margin-left: auto;
      color: var(--muted);
      font: 500 10px/1.4 "SFMono-Regular", Menlo, monospace;
      white-space: nowrap;
    }
    .ion-flow-metrics {
      display: grid;
      grid-template-columns: repeat(6, minmax(96px, 1fr));
      gap: 8px;
    }
    .ion-flow-metric {
      min-width: 0;
      padding: 9px 10px;
      border: 1px solid #e4e7ec;
      border-radius: 9px;
      background: #f8fafc;
    }
    .ion-flow-metric strong {
      display: block;
      color: #101828;
      font: 700 17px/1.2 "SFMono-Regular", Menlo, monospace;
    }
    .ion-flow-metric span {
      display: block;
      margin-top: 3px;
      color: #667085;
      font: 600 9px/1.35 "SFMono-Regular", Menlo, monospace;
      text-transform: uppercase;
      letter-spacing: 0.04em;
    }
    .ion-flow-groups {
      display: grid;
      gap: 8px;
      margin-top: 10px;
    }
    .ion-flow-group {
      display: flex;
      align-items: flex-start;
      flex-wrap: wrap;
      gap: 6px;
    }
    .ion-flow-group-label {
      width: 78px;
      padding-top: 4px;
      color: #667085;
      font: 650 9px/1.4 "SFMono-Regular", Menlo, monospace;
      text-transform: uppercase;
    }
    .ion-flow-chip,
    .ion-entry-provenance-chip {
      display: inline-flex;
      align-items: center;
      gap: 4px;
      max-width: 100%;
      padding: 3px 7px;
      border: 1px solid #d0d5dd;
      border-radius: 999px;
      color: #475467;
      background: #fff;
      font: 600 9px/1.35 "SFMono-Regular", Menlo, monospace;
      overflow-wrap: anywhere;
    }
    .ion-entry-type-controls {
      display: flex;
      flex-wrap: wrap;
      gap: 6px;
    }
    .ion-entry-filter,
    .ion-entry-filter-reset {
      display: inline-flex;
      align-items: center;
      gap: 6px;
      padding: 5px 9px;
      border: 1px solid color-mix(in srgb, var(--entry-color) 48%, white);
      border-radius: 999px;
      color: color-mix(in srgb, var(--entry-color) 82%, #101828);
      background: color-mix(in srgb, var(--entry-color) 9%, white);
      font: 650 10px/1.4 "SFMono-Regular", Menlo, monospace;
      cursor: pointer;
      transition: opacity 120ms ease, background 120ms ease, border-color 120ms ease;
    }
    .ion-entry-filter:hover { background: color-mix(in srgb, var(--entry-color) 16%, white); }
    .ion-entry-filter[aria-pressed="false"] { opacity: 0.36; filter: grayscale(0.6); }
    .ion-entry-filter-count { opacity: 0.64; font-weight: 550; }
    .ion-entry-filter-reset {
      --entry-color: #667085;
      color: #475467;
      border-color: #d0d5dd;
      background: #fff;
    }
    .ion-entry-filter-reset:disabled { opacity: 0.35; cursor: default; }
    .ion-entry-swatch {
      width: 8px;
      height: 8px;
      border-radius: 2px;
      background: var(--entry-color);
      box-shadow: 0 0 0 1px rgba(16, 24, 40, 0.08);
    }
    .ion-timeline-scroll {
      overflow-x: auto;
      overflow-y: hidden;
      padding: 2px 0 4px;
      scrollbar-width: thin;
      scrollbar-color: #cbd5e1 transparent;
    }
    .ion-timeline-track {
      position: relative;
      display: flex;
      width: var(--timeline-content-width, 12px);
      min-width: 12px;
      height: 32px;
      overflow: visible;
      border: 1px solid #e4e7ec;
      border-radius: 7px;
      background: #fbfcfd;
    }
    .ion-timeline-bar {
      position: relative;
      flex: 0 0 12px;
      top: 3px;
      width: 12px;
      height: 26px;
      padding: 0;
      border: 0;
      border-radius: 0;
      background: transparent;
      cursor: pointer;
      z-index: 1;
    }
    .ion-timeline-bar::before {
      content: '';
      position: absolute;
      top: 0;
      bottom: 0;
      left: 0;
      width: 100%;
      border-right: 1px solid rgba(255, 255, 255, 0.72);
      border-radius: 0;
      background: var(--bar-color);
      opacity: 0.9;
      transition: opacity 120ms ease, transform 120ms ease, box-shadow 120ms ease;
    }
    .ion-timeline-bar:hover,
    .ion-timeline-bar:focus-visible {
      z-index: 3;
      outline: none;
    }
    .ion-timeline-bar:hover::before,
    .ion-timeline-bar:focus-visible::before {
      opacity: 1;
      transform: scaleY(1.08);
      box-shadow: 0 0 0 2px #fff, 0 0 0 3px var(--bar-color);
    }
    .ion-timeline-empty {
      display: flex;
      align-items: center;
      height: 100%;
      padding: 0 12px;
      color: var(--muted);
      font-size: 10px;
    }
    .ion-timeline-axis {
      display: flex;
      justify-content: flex-start;
      gap: 12px;
      margin-top: 6px;
      color: #98a2b3;
      font: 500 9px/1.4 "SFMono-Regular", Menlo, monospace;
    }
    .ion-timeline-notice {
      min-height: 16px;
      margin-top: 4px;
      color: #667085;
      font: 500 9px/1.4 "SFMono-Regular", Menlo, monospace;
      text-align: center;
    }
    .ion-entry-jump-target {
      scroll-margin-block: 96px;
      animation: ion-entry-jump-flash 1.8s ease-out;
    }
    @keyframes ion-entry-jump-flash {
      0%, 24% { box-shadow: 0 0 0 4px rgba(6, 182, 212, 0.34); }
      100% { box-shadow: 0 0 0 0 rgba(6, 182, 212, 0); }
    }
    .ion-timeline-tooltip {
      position: fixed;
      z-index: 1000;
      width: min(340px, calc(100vw - 24px));
      padding: 12px 13px;
      border: 1px solid rgba(255, 255, 255, 0.12);
      border-radius: 10px;
      color: #f8fafc;
      background: rgba(16, 24, 40, 0.96);
      box-shadow: 0 16px 40px rgba(16, 24, 40, 0.24);
      backdrop-filter: blur(10px);
      pointer-events: none;
      opacity: 0;
      transform: translateY(4px);
      transition: opacity 100ms ease, transform 100ms ease;
    }
    .ion-timeline-tooltip.is-visible { opacity: 1; transform: translateY(0); }
    .ion-tooltip-type {
      display: inline-flex;
      align-items: center;
      gap: 6px;
      margin-bottom: 6px;
      color: #fff;
      font: 700 10px/1.3 "SFMono-Regular", Menlo, monospace;
      letter-spacing: 0.06em;
      text-transform: uppercase;
    }
    .ion-tooltip-meta {
      margin-bottom: 7px;
      color: #9fb0c8;
      font: 500 9px/1.45 "SFMono-Regular", Menlo, monospace;
    }
    .ion-tooltip-summary {
      color: #e4e7ec;
      font-size: 11px;
      line-height: 1.55;
      overflow-wrap: anywhere;
    }

    /* Center the whole workspace and keep the navigation/content ratio useful. */
    #app {
      width: min(100%, var(--ion-shell-max));
      min-height: 0;
      margin: 0 auto;
      padding: 18px 28px 40px;
      display: grid;
      grid-template-columns: var(--ion-sidebar-width) minmax(0, 1fr);
      gap: 18px;
      align-items: start;
    }
    #sidebar {
      width: 100%;
      min-width: 0;
      max-width: none;
      height: calc(100vh - 36px);
      top: 18px;
      overflow: hidden;
      border: 1px solid #e1e6ec;
      border-radius: var(--ion-radius-lg);
      background: rgba(255, 255, 255, 0.92);
      box-shadow: var(--ion-shadow);
    }
    #sidebar-resizer { display: none; }
    .sidebar-header {
      padding: 18px 14px 8px;
      border-bottom: 1px solid #edf0f3;
    }
    .sidebar-header::before {
      content: "SESSION MAP";
      display: block;
      padding: 0 8px 8px;
      color: #475467;
      font: 700 10px/1.2 "SFMono-Regular", Menlo, monospace;
      letter-spacing: 0.13em;
    }
    .sidebar-controls { padding: 0 8px 5px; }
    .sidebar-search {
      padding: 8px 10px;
      border-color: #d9e0e8;
      border-radius: 8px;
      background: #f8fafc;
    }
    .sidebar-filters { gap: 5px; padding: 5px 8px 8px; }
    .filter-btn { border: 0; border-radius: 6px; padding: 4px 7px; }
    .filter-btn.active { color: #fff; background: #0e7490; }
    .tree-container { padding: 10px 5px; }
    .tree-node { padding: 2px 10px; line-height: 16px; border-radius: 5px; }
    .tree-status { padding: 8px 14px; border-top: 1px solid #edf0f3; }

    #content {
      width: 100%;
      min-width: 0;
      overflow: visible;
      padding: 0;
      align-items: stretch;
    }
    #content > * { width: 100%; max-width: none; }
    .header {
      padding: 20px;
      border: 1px solid #e1e6ec;
      border-radius: var(--ion-radius-lg);
      margin-bottom: 14px;
      background: rgba(255, 255, 255, 0.96);
      box-shadow: var(--ion-shadow);
    }
    .header h1 { display: none; }
    .help-bar {
      margin-bottom: 14px;
      padding-bottom: 14px;
      border-bottom: 1px solid #edf0f3;
      color: var(--muted);
    }
    .help-hint { font: 500 10px/1.4 "SFMono-Regular", Menlo, monospace; }
    .help-actions { gap: 6px; }
    .header-toggle-btn,
    .download-json-btn {
      padding: 5px 9px;
      border-color: #d9e0e8;
      border-radius: 7px;
      color: #344054;
      background: #fff;
    }
    .header-info {
      display: grid;
      grid-template-columns: repeat(2, minmax(0, 1fr));
      gap: 7px;
    }
    .info-item {
      min-width: 0;
      padding: 8px 10px;
      border-radius: 8px;
      background: #f7f9fb;
    }
    .info-label { min-width: 92px; color: #667085; }
    .info-value { min-width: 0; color: #172033; overflow-wrap: anywhere; }
    #messages { gap: 12px; }

    /* Long Entries keep their real rendered content visible for a few lines. */
    #messages > [id^="entry-"].ion-entry-fold {
      position: relative;
      overflow: hidden;
    }
    .ion-entry-fold-content {
      display: block;
      overflow: hidden;
      max-height: var(--ion-entry-preview-height, 132px);
    }
    .ion-entry-fold-hint {
      display: block;
      width: max-content;
      max-width: 100%;
      margin: 7px 0 0;
      padding: 4px 7px;
      border: 0;
      border-radius: 5px;
      color: #667085;
      background: transparent;
      font: italic 500 11px/1.4 "SFMono-Regular", Menlo, monospace;
      text-align: left;
      cursor: pointer;
    }
    .ion-entry-fold-hint:hover { color: #0e7490; background: rgba(14, 116, 144, 0.07); }
    .ion-entry-fold-hint:focus-visible {
      outline: 2px solid var(--ion-entry-accent, #0e7490);
      outline-offset: 1px;
    }
    .ion-entry-fold-hint[hidden] { display: none; }
    .ion-entry-fold[data-ion-entry-expanded="true"] .ion-entry-fold-content {
      max-height: none;
    }
    .ion-entry-fold[data-ion-entry-foldable="false"] .ion-entry-fold-content {
      max-height: none;
    }
    .ion-entry-fold.compaction .compaction-collapsed { display: none !important; }
    .ion-entry-fold.compaction .compaction-content { display: block !important; }

    /* Message roles: quiet cards with an unmistakable edge marker. */
    .user-message {
      background: #edf6ff !important;
      border: 1px solid #d6e8f8 !important;
      border-left: 3px solid #2e90fa !important;
      padding: 14px 16px !important;
      border-radius: 12px !important;
      margin-bottom: 0 !important;
    }
    /* Memory/context injection: collapse by default to avoid visual repetition */
    .user-message.memory-injection {
      background: #f8fafc !important;
      border-left: 3px solid #94a3b8 !important;
      border-color: #e2e8f0 !important;
      max-height: 60px;
      overflow: hidden;
      position: relative;
      cursor: pointer;
    }
    .user-message.memory-injection::after {
      content: '📝 Memory 注入（点击展开）';
      position: absolute;
      bottom: 0; left: 0; right: 0;
      height: 24px;
      background: linear-gradient(transparent, #f8fafc 60%);
      display: flex;
      align-items: flex-end;
      justify-content: center;
      font-size: 11px;
      color: #64748b;
      padding-bottom: 2px;
    }
    .user-message.memory-injection.expanded {
      max-height: none;
    }
    .user-message.memory-injection.expanded::after {
      display: none;
    }
    .user-message::before {
      content: "USER";
      display: block;
      font-size: 11px;
      font-weight: 600;
      color: #175cd3;
      margin-bottom: 6px;
      text-transform: uppercase;
      letter-spacing: 0.5px;
    }
    .assistant-message {
      padding: 14px 16px !important;
      border: 1px solid #dce7e3 !important;
      border-left: 3px solid #12b76a !important;
      background: #fbfefc !important;
      border-radius: 12px !important;
      margin-bottom: 0 !important;
    }
    .assistant-message::before {
      content: "ASSISTANT";
      display: block;
      font-size: 11px;
      font-weight: 600;
      color: #067647;
      margin-bottom: 6px;
      text-transform: uppercase;
      letter-spacing: 0.5px;
    }
    .tool-execution {
      border-left: 3px solid #f79009 !important;
      background: #fffbeb !important;
      padding: 8px 12px !important;
      border-radius: 8px !important;
      margin-bottom: 8px !important;
    }
    .tool-execution::before {
      content: "TOOL RESULT";
      display: block;
      font-size: 11px;
      font-weight: 600;
      color: #f59e0b;
      margin-bottom: 4px;
      text-transform: uppercase;
      letter-spacing: 0.5px;
    }
    /* custom_message（sub_session_separator 等）特殊样式 */
    .custom-message {
      border-left: 3px solid #8b5cf6 !important;
      background: #f5f3ff !important;
      padding: 12px 16px !important;
      border-radius: 10px !important;
      margin-bottom: 8px !important;
    }
    /* 不同 customType 用不同颜色（用户反馈：自定义插入应有视觉区分）*/
    .hook-message[data-custom-type="diagnostics"] {
      border-left-color: #dc2626 !important;
      background: #fef2f2 !important;
    }
    .hook-message[data-custom-type="diagnostics"][data-has-errors="false"] {
      border-left-color: #10b981 !important;
      background: #f0fdf4 !important;
    }
    .hook-message[data-custom-type="bash_result"] {
      border-left-color: #f59e0b !important;
      background: #fffbeb !important;
      font-family: 'Menlo', 'Monaco', monospace !important;
      font-size: 12px !important;
    }
    .hook-message[data-custom-type="dev_servers"] {
      border-left-color: #ec4899 !important;
      background: #fdf2f8 !important;
    }
    .hook-message[data-custom-type="session_name"] {
      border-left-color: #0891b2 !important;
      background: #ecfeff !important;
    }
    /* session_name 卡片：用更显著的渐变背景，区别于普通 custom_message */
    .custom-message[data-custom-type="session_name"],
    .custom-message.session-name-card {
      border-left: 3px solid #0891b2 !important;
      background: linear-gradient(90deg, #ecfeff 0%, #cffafe 100%) !important;
      padding: 10px 16px !important;
      border-radius: 6px !important;
      margin: 8px 0 !important;
      font-size: 13px !important;
    }
    /* tree-view 里 session_name 节点也用青色高亮 */
    .tree-node[data-custom-type="session_name"] .tree-custom {
      color: #0891b2 !important;
      font-weight: 600 !important;
    }
    /* hook_event 审计卡片：判断方式 / Agent 推理过程 */
    .hook-meta {
      display: flex;
      flex-wrap: wrap;
      gap: 8px;
      margin-top: 6px;
      padding-top: 6px;
      border-top: 1px solid #e2e8f0;
    }
    .hook-meta-item {
      font-size: 12px;
      color: #64748b;
      background: #f1f5f9;
      padding: 2px 8px;
      border-radius: 4px;
    }
    .hook-meta-item b {
      color: #475569;
      font-weight: 600;
    }
    .hook-reasoning {
      margin-top: 6px;
    }
    .hook-reasoning summary {
      font-size: 12px;
      color: #6366f1;
      cursor: pointer;
      user-select: none;
    }
    .hook-reasoning summary:hover {
      text-decoration: underline;
    }
    .hook-reasoning-text {
      margin-top: 6px;
      padding: 8px 10px;
      background: #f8fafc;
      border: 1px solid #e2e8f0;
      border-radius: 6px;
      font-size: 12px;
      color: #475569;
      white-space: pre-wrap;
      word-break: break-word;
    }
    .ion-generic-entry {
      position: relative;
      padding: 10px 13px;
      border: 1px solid #e2e8f0;
      border-left: 3px solid #667085;
      border-radius: 9px;
      background: #f8fafc;
      color: #344054;
    }
    .ion-generic-entry-type {
      margin-bottom: 4px;
      color: #475467;
      font: 700 10px/1.35 "SFMono-Regular", Menlo, monospace;
      letter-spacing: 0.03em;
    }
    .ion-generic-entry-content {
      white-space: pre-wrap;
      overflow-wrap: anywhere;
      font: 500 11px/1.55 "SFMono-Regular", Menlo, monospace;
    }
    .ion-entry-provenance {
      display: flex;
      flex-wrap: wrap;
      gap: 5px;
      margin: 7px 0 5px;
      padding-top: 7px;
      border-top: 1px dashed #d0d5dd;
    }
    .ion-entry-provenance-chip[data-kind="phase"] {
      color: #175cd3;
      border-color: #b2ddff;
      background: #eff8ff;
    }
    .ion-entry-provenance-chip[data-kind="source"] {
      color: #6941c6;
      border-color: #d9d6fe;
      background: #f4f3ff;
    }
    .ion-entry-provenance-chip[data-kind="llm"] {
      color: #027a48;
      border-color: #abefc6;
      background: #ecfdf3;
    }
    .ion-entry-provenance-chip[data-kind="hidden"] {
      color: #b54708;
      border-color: #fedf89;
      background: #fffaeb;
    }
    .ion-entry-filter-hidden { display: none !important; }
    .ion-entry-nested-events {
      display: grid;
      gap: 6px;
      margin-top: 8px;
      padding-top: 8px;
      border-top: 1px dashed #d0d5dd;
    }
    .ion-entry-nested-events > .hook-message {
      margin: 0 !important;
      padding: 7px 9px !important;
      border: 1px solid #ddd6fe !important;
      border-left: 3px solid #8b5cf6 !important;
      border-radius: 7px !important;
      background: #f5f3ff !important;
      font-size: 11px !important;
    }
    .ion-entry-nested-events > .hook-message .message-timestamp {
      margin-bottom: 3px;
    }
    .ion-entry-nested-events > .hook-message .hook-type {
      font-size: 9px;
    }
    /* ION: 每个 toolCall 独立一行 + 间距 + 背景色（对标 toolResult） */
    .assistant-message .tool-execution {
      display: block !important;
      margin-bottom: 8px !important;
      padding: 8px 12px !important;
      border-radius: 8px !important;
      border-left: 3px solid #f59e0b !important;
      background: #fffbeb !important;
    }
    .assistant-message .tool-execution.error {
      border-left-color: #dc2626 !important;
      background: #fef2f2 !important;
    }
    .assistant-message .tool-execution.success {
      border-left-color: #10b981 !important;
      background: #f0fdf4 !important;
    }
    /* ION: 输出折叠——头 N 行 + 尾 N 行（中间压缩，不是只折叠尾部） */
    .expandable-output .output-tail {
      display: block !important;
    }
    .tool-output:not(.expandable)[data-ion-output-foldable="true"]:not(.expanded)::after {
      color: #0e7490;
      background: linear-gradient(transparent, rgba(248, 250, 252, 0.98) 56%);
    }

    @media (max-width: 1100px) {
      .ion-stats-inner { grid-template-columns: 1fr; }
      .ion-session-metrics { flex-wrap: wrap; }
      .ion-tool-badges { grid-column: auto; margin-top: -6px; }
      #ion-ext-viz { grid-template-columns: 1fr; }
      #app { grid-template-columns: 280px minmax(0, 1fr); }
    }
    @media (max-width: 900px) {
      .ion-stats-inner { min-height: 0; padding: 18px 20px; }
      .ion-title-block { padding-left: 44px; }
      .ion-session-metrics { display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); }
      .ion-session-metric, .ion-session-metric--model { min-width: 0; }
      #ion-ext-viz { margin-top: 12px; padding: 0 12px; }
      #app { display: block; padding: 12px 12px 28px; }
      #sidebar {
        width: min(var(--sidebar-width), calc(100vw - 24px));
        min-width: min(var(--sidebar-width), calc(100vw - 24px));
        max-width: min(var(--sidebar-width), calc(100vw - 24px));
        height: 100vh;
        top: 0;
        border-radius: 0 14px 14px 0;
      }
      #content { padding: 0; }
      #hamburger {
        top: 12px;
        left: 12px;
        padding: 7px 9px;
        border-color: rgba(255, 255, 255, 0.3);
        color: #fff;
        background: rgba(16, 24, 40, 0.76);
        backdrop-filter: blur(8px);
      }
      .header-info { grid-template-columns: 1fr; }
    }
    @media (max-width: 560px) {
      .ion-session-title { font-size: 20px; }
      .ion-session-metrics { grid-template-columns: 1fr; }
      .ion-overview-panel { padding: 14px; }
      .ion-flow-metrics { grid-template-columns: repeat(3, minmax(86px, 1fr)); }
      .header { padding: 14px; border-radius: 12px; }
      .help-hint { display: none; }
      .help-actions { width: 100%; }
      .header-toggle-btn, .download-json-btn { flex: 1; }
      .info-item { display: block; }
      .info-label { display: block; min-width: 0; margin: 0 0 2px; }
    }
    @media print {
      #ion-stats-banner { background: #fff !important; color: #101828; border-bottom: 2px solid #101828; }
      .ion-session-title, .ion-session-metric strong { color: #101828; }
      #ion-ext-viz, #app { width: 100%; max-width: none; padding-left: 0; padding-right: 0; }
      #ion-ext-viz { display: block; }
      .ion-overview-panel { margin-bottom: 10px; box-shadow: none; }
      #content { display: block; }
      .header, .user-message, .assistant-message { box-shadow: none; break-inside: avoid; }
    }
    "#;
    if let Some(pos) = html.rfind("</style>") {
        html.insert_str(pos, ion_custom_css);
    }

    // Set title
    html = html.replace(
        "<title>Session Export</title>",
        &format!("<title>Session {session_id}</title>"),
    );

    // 如果是 fork 子 session，在页面顶部注入来源标记
    let has_parent = header
        .get("parentSession")
        .and_then(|v| v.as_str())
        .is_some();
    let spawn_meta = header.get("spawnMeta").cloned();
    if has_parent || spawn_meta.is_some() {
        let parent_session = header
            .get("parentSession")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let spawned_by = spawn_meta
            .as_ref()
            .and_then(|m| m.get("spawnedBy"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let relation = spawn_meta
            .as_ref()
            .and_then(|m| m.get("relation"))
            .and_then(|v| v.as_str())
            .unwrap_or("child");

        // 计算主 HTML 的文件名（跟主 session id 同名）
        let _parent_html = "session_export.html"; // fallback

        let origin_banner = format!(
            r#"
<div id="fork-origin-banner" style="
  background: linear-gradient(135deg, #667eea 0%, #764ba2 100%);
  color: white;
  padding: 10px 20px;
  font-size: 13px;
  display: flex;
  align-items: center;
  gap: 12px;
  border-bottom: 2px solid #5a67d8;
">
  <span style="font-size: 18px;">🔗</span>
  <span>
    <strong>Fork 子 Worker Session</strong>
    &nbsp;·&nbsp;
    spawnedBy: <code style="background:rgba(255,255,255,0.2);padding:2px 6px;border-radius:3px;">{spawned_by}</code>
    &nbsp;·&nbsp;
    relation: <code style="background:rgba(255,255,255,0.2);padding:2px 6px;border-radius:3px;">{relation}</code>
    &nbsp;·&nbsp;
    parentSession: <code style="background:rgba(255,255,255,0.2);padding:2px 6px;border-radius:3px;">{parent_session}</code>
  </span>
</div>"#
        );
        // 在 <body> 后插入 banner
        if let Some(pos) = html.find("<body>") {
            html.insert_str(pos + 6, &origin_banner);
        }
    }

    // 统计信息（工具调用次数、模型、session 名称）
    let tool_counts: std::collections::HashMap<String, u32> = {
        let mut counts = std::collections::HashMap::new();
        for e in &entries {
            if e.get("type").and_then(|v| v.as_str()) != Some("message") {
                continue;
            }
            if let Some(content) = e
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())
            {
                for c in content {
                    if let Some(name) = c.get("type").and_then(|v| v.as_str())
                        && name == "toolCall"
                        && let Some(tn) = c.get("name").and_then(|v| v.as_str())
                    {
                        *counts.entry(tn.to_string()).or_insert(0) += 1;
                    }
                }
            }
        }
        counts
    };
    let total_tool_calls: u32 = tool_counts.values().sum();
    // 模型名：只从 assistant message 的 model 字段提取（避免抓到 CSS 里的 emoji）
    let model = entries
        .iter()
        .find_map(|e| {
            let msg = e.get("message")?;
            // 只从 assistant 消息里取
            if msg.get("role").and_then(|v| v.as_str()) != Some("assistant") {
                return None;
            }
            msg.get("model")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| "unknown".to_string());

    // ★ banner title：直接用前面解析的 session_name 变量（line 662），
    // 不再从 header 重新提取。之前这里定义了同名变量覆盖了正确的
    // LLM 生成的标题（从 SessionIndex 拿到的），导致 banner 显示
    // agent name "developer" 而不是真正的 session title。
    // session_name 变量已经 fallback 链完整（session.jsonl entry →
    // SessionIndex → 首条 user message），不需要重复提取。
    let banner_title = session_name.clone();
    let banner_title = if banner_title.is_empty() {
        header
            .get("agent")
            .and_then(|v| v.as_str())
            .unwrap_or("Session")
            .to_string()
    } else {
        banner_title
    };

    // 构造统计 banner HTML
    let mut tool_badges = String::new();
    let mut sorted_tools: Vec<(&String, &u32)> = tool_counts.iter().collect();
    sorted_tools.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    for (name, count) in &sorted_tools {
        tool_badges.push_str(&format!(
            r#"<span class="ion-tool-badge">{} ×{}</span>"#,
            escape_html_text(name),
            count
        ));
    }

    let stats_banner = format!(
        r#"
<header id="ion-stats-banner">
  <div class="ion-stats-inner">
    <div class="ion-title-block">
      <span class="ion-title-kicker">ION / Session export</span>
      <h1 class="ion-session-title">{}</h1>
    </div>
    <div class="ion-session-metrics" role="list" aria-label="Session metrics">
      <div class="ion-session-metric ion-session-metric--model" role="listitem">
        <span class="ion-metric-label">Model</span>
        <strong>{}</strong>
      </div>
      <div class="ion-session-metric" role="listitem">
        <span class="ion-metric-label">Tool calls</span>
        <strong>{}</strong>
      </div>
      <div class="ion-session-metric" role="listitem">
        <span class="ion-metric-label">Entries</span>
        <strong>{}</strong>
      </div>
      <div class="ion-session-metric" role="listitem">
        <span class="ion-metric-label">Tokens ↑↓</span>
        <strong>{} / {}</strong>
      </div>
    </div>
    <div class="ion-tool-badges" aria-label="Tools used">{}</div>
  </div>
</header>"#,
        escape_html_text(&banner_title),
        escape_html_text(&model),
        total_tool_calls,
        timeline_entry_count,
        format_tokens(total_token_input),
        format_tokens(total_token_output),
        tool_badges
    );

    // 在 fork-origin-banner 之后（或 body 开头）插入统计 banner
    if html.contains("fork-origin-banner") {
        // fork 子 session：在 origin banner 后插入
        if let Some(pos) = html.find("</div>\n</div>\n\n") {
            html.insert_str(pos + 6, &stats_banner);
        }
    } else {
        // 主 session：在 body 开头插入
        if let Some(pos) = html.find("<body>") {
            html.insert_str(pos + 6, &stats_banner);
        }
    }

    // ── 完整 Entry Timeline（ion 增强版）──
    // 在 stats-banner 后插入一个容器，页面加载后由 JS 填充：
    //   1. 全量 entry 类型筛选（内核 entry 保留真实 type，Extension custom 统一归类）
    //   2. 每条 entry 独立着色，悬停/聚焦展示时间、ID 与内容概要
    let _legacy_ext_visualization_script = r#"
<script>
(function() {
  function escapeVizHtml(value) {
    return String(value)
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;')
      .replace(/'/g, '&#39;');
  }

  function compactPreview(value, limit) {
    var text = String(value == null ? '' : value).replace(/\s+/g, ' ').trim();
    if (!text) return '';
    return text.length > limit ? text.slice(0, limit - 1) + '…' : text;
  }

  function unwrapMessage(entry) {
    var msg = entry && entry.message ? entry.message : {};
    var wrappers = ['User', 'Assistant', 'ToolResult', 'Custom'];
    for (var i = 0; i < wrappers.length; i++) {
      if (msg[wrappers[i]]) {
        var unwrapped = msg[wrappers[i]];
        if (!unwrapped.role) unwrapped.role = wrappers[i];
        return unwrapped;
      }
    }
    return msg;
  }

  function entryCategory(entry) {
    var rawType = String((entry && entry.type) || 'other');
    var semantic = entry && entry.ionMeta;
    if (semantic && semantic.filterType) return semantic.filterType;
    if (rawType === 'custom' || rawType === 'custom_message') return 'custom';
    if (rawType !== 'message') return rawType;
    var role = String(unwrapMessage(entry).role || 'message').toLowerCase();
    if (role === 'tool' || role === 'toolresult' || role === 'tool_result') return 'toolResult';
    if (role === 'custom') return 'custom';
    if (role === 'user' || role === 'assistant') return role;
    return 'message';
  }

  function entryLabel(category) {
    var labels = {
      user: 'user', assistant: 'assistant', toolResult: 'tool result', custom: 'custom',
      branch_summary: 'branch summary', segment_summary: 'segment summary',
      model_change: 'model change', thinking_level_change: 'thinking change', agent_change: 'agent change',
      session_info: 'session info', system_event: 'system event', active_tools_change: 'tools change',
      leaf_pointer: 'leaf pointer'
    };
    return labels[category] || String(category || 'other').replace(/_/g, ' ');
  }

  function entryColor(category) {
    var colors = {
      user: '#3b82f6', assistant: '#10b981', toolResult: '#f59e0b', custom: '#8b5cf6',
      compaction: '#ef4444', branch_summary: '#d946ef',
      segment_summary: '#c026d3', model_change: '#06b6d4', thinking_level_change: '#0ea5e9',
      agent_change: '#14b8a6', session_info: '#6366f1', system_event: '#f97316',
      active_tools_change: '#a855f7', label: '#eab308', deletion: '#dc2626',
      restoration: '#22c55e', leaf_pointer: '#64748b', message: '#475467', other: '#6b7280'
    };
    if (colors[category]) return colors[category];
    var palette = ['#2563eb', '#0f766e', '#b45309', '#7c3aed', '#be123c', '#4f46e5', '#15803d'];
    var hash = 0;
    for (var i = 0; i < category.length; i++) hash = ((hash << 5) - hash + category.charCodeAt(i)) | 0;
    return palette[Math.abs(hash) % palette.length];
  }

  function previewValue(value, depth) {
    if (value == null || depth > 3) return '';
    if (typeof value === 'string' || typeof value === 'number' || typeof value === 'boolean') {
      return String(value);
    }
    if (Array.isArray(value)) {
      return value.slice(0, 6).map(function(v) { return previewValue(v, depth + 1); }).filter(Boolean).join(' · ');
    }
    if (value.Text) return previewValue(value.Text.text, depth + 1);
    if (value.Thinking) return previewValue(value.Thinking.thinking, depth + 1);
    if (value.ToolCall) {
      return 'tool call ' + (value.ToolCall.name || '') + ' ' + previewValue(value.ToolCall.arguments, depth + 1);
    }
    if (value.Image || value.type === 'image') return '[image]';
    if (value.type === 'toolCall') {
      return 'tool call ' + (value.name || '') + ' ' + previewValue(value.arguments, depth + 1);
    }
    var preferred = ['text', 'thinking', 'summary', 'content', 'label', 'name', 'reason', 'status', 'data', 'details'];
    var parts = [];
    preferred.forEach(function(key) {
      if (Object.prototype.hasOwnProperty.call(value, key)) {
        var part = previewValue(value[key], depth + 1);
        if (part) parts.push(part);
      }
    });
    if (parts.length) return parts.join(' · ');
    return Object.keys(value).filter(function(key) {
      return !['id', 'parentId', 'timestamp', 'type', 'display'].includes(key);
    }).slice(0, 4).map(function(key) {
      var part = previewValue(value[key], depth + 1);
      return part ? key + ': ' + part : '';
    }).filter(Boolean).join(' · ');
  }

  function entrySummary(entry) {
    var category = entryCategory(entry);
    if (entry && entry.customType === 'step-snapshot' && entry.data) {
      var diff = entry.data.diff || {};
      var added = Array.isArray(diff.added) ? diff.added : [];
      var modified = Array.isArray(diff.modified) ? diff.modified : [];
      var deleted = Array.isArray(diff.deleted) ? diff.deleted : [];
      var files = added.concat(modified, deleted).slice(0, 5).join(', ');
      return 'Files +' + added.length + ' ~' + modified.length + ' -' + deleted.length +
        (files ? ' · ' + files : '') + ' · tree ' + String(entry.data.snapshotTreeHash || '').slice(0, 12);
    }
    var source = entry;
    if (entry && entry.type === 'message') source = unwrapMessage(entry);
    var preview = previewValue(source, 0);
    if (!preview && category === 'custom') preview = previewValue(entry.content || entry.data || entry.details, 0);
    return compactPreview(preview || 'No preview available', 240);
  }

  function buildExtVisualization() {
    // pi template 不暴露 SESSION_DATA 到 window，自己解码 session-data script
    var dataEl = document.getElementById('session-data');
    if (!dataEl) return;
    var timelineEntries;
    try {
      var b64 = dataEl.textContent.trim();
      var bin = atob(b64);
      var bytes = new Uint8Array(bin.length);
      for (var i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
      var decoded = JSON.parse(new TextDecoder('utf-8').decode(bytes));
      timelineEntries = Array.isArray(decoded.timelineEntries) ? decoded.timelineEntries : (decoded.entries || []);
    } catch (e) { return; }
    if (!timelineEntries.length) return;

    // Timeline 保留所有 entry。内核 entry 使用真实 type；Extension 产生的
    // custom/custom_message 统一归为 custom，不按 customType 继续细分。
    var n = timelineEntries.length;
    var timelineItems = timelineEntries.map(function(entry, index) {
      var category = entryCategory(entry);
      return {
        index: index,
        entry: entry,
        category: category,
        label: entryLabel(category),
        color: entryColor(category),
        summary: entrySummary(entry)
      };
    });

    var categoryCounts = {};
    timelineItems.forEach(function(item) {
      categoryCounts[item.category] = (categoryCounts[item.category] || 0) + 1;
    });
    var priority = ['user', 'assistant', 'toolResult', 'custom', 'compaction', 'branch_summary'];
    var categories = Object.keys(categoryCounts).sort(function(a, b) {
      var ai = priority.indexOf(a), bi = priority.indexOf(b);
      if (ai === -1) ai = 999;
      if (bi === -1) bi = 999;
      return ai - bi || entryLabel(a).localeCompare(entryLabel(b));
    });
    var hiddenCategories = new Set();

    // 时间范围只用于首尾标签；线条位置按 Entry 顺序紧凑排列，避免空闲时间
    // 在可视化中制造大片空白。
    var ts = timelineEntries.map(function(e) {
      var t = new Date(e.timestamp || 0).getTime();
      return (isNaN(t) || t === 0) ? null : t;
    });
    var validTs = ts.filter(function(t) { return t !== null; });
    var tMin = validTs.length ? Math.min.apply(null, validTs) : 0;
    var tMax = validTs.length ? Math.max.apply(null, validTs) : 0;
    var timeLabel = '';
    var timeStart = '';
    var timeEnd = '';
    if (tMin > 0 && tMax > 0) {
      timeStart = new Date(tMin).toLocaleTimeString();
      timeEnd = new Date(tMax).toLocaleTimeString();
      timeLabel = timeStart + ' → ' + timeEnd;
    }

    var html =
      '<div id="ion-ext-viz" aria-label="Session overview">' +
        '<section class="ion-overview-panel">' +
          '<div class="ion-overview-heading"><div><span class="ion-overview-kicker">Entries</span><strong>Type filters</strong></div>' +
          '<span class="ion-overview-meta">' + categories.length + ' types</span></div>' +
          '<div class="ion-entry-type-controls" id="ion-entry-type-controls"></div>' +
        '</section>' +
        '<section class="ion-overview-panel">' +
          '<div class="ion-overview-heading"><div><span class="ion-overview-kicker">Sequence</span><strong>Complete timeline</strong></div>' +
          '<span class="ion-overview-meta" id="ion-timeline-meta"></span></div>' +
          '<div class="ion-timeline-scroll"><div class="ion-timeline-track" id="ion-timeline-track"></div></div>' +
          '<div class="ion-timeline-axis"><span>' + escapeVizHtml(
            timeStart && timeEnd ? timeStart + ' → ' + timeEnd : 'time unavailable'
          ) + '</span></div>' +
          '<div class="ion-timeline-notice" id="ion-timeline-notice" aria-live="polite">Hover for a summary · click to jump to the entry</div>' +
        '</section>' +
        '<div class="ion-timeline-tooltip" id="ion-timeline-tooltip" role="tooltip"></div>' +
      '</div>';

    var existing = document.getElementById('ion-ext-viz');
    if (existing) existing.remove();
    var banner = document.getElementById('ion-stats-banner');
    if (banner && banner.parentNode) {
      banner.insertAdjacentHTML('afterend', html);
    }

    var root = document.getElementById('ion-ext-viz');
    var controls = document.getElementById('ion-entry-type-controls');
    var track = document.getElementById('ion-timeline-track');
    var meta = document.getElementById('ion-timeline-meta');
    var notice = document.getElementById('ion-timeline-notice');
    var tooltip = document.getElementById('ion-timeline-tooltip');
    if (!root || !controls || !track || !meta || !notice || !tooltip) return;

    function renderControls() {
      controls.innerHTML = categories.map(function(category) {
        var visible = !hiddenCategories.has(category);
        return '<button type="button" class="ion-entry-filter" data-entry-category="' +
          escapeVizHtml(category) + '" aria-pressed="' + visible + '" style="--entry-color:' +
          entryColor(category) + '"><span class="ion-entry-swatch"></span><span>' +
          escapeVizHtml(entryLabel(category)) + '</span><span class="ion-entry-filter-count">' +
          categoryCounts[category] + '</span></button>';
      }).join('') + '<button type="button" class="ion-entry-filter-reset" data-entry-reset' +
        (hiddenCategories.size ? '' : ' disabled') + '>Show all</button>';
    }

    function renderTimeline() {
      var visibleItems = timelineItems.filter(function(item) {
        return !hiddenCategories.has(item.category);
      });
      // 每个 Entry 占一个相邻的固定宽度色块。顺序连续、没有墙钟时间造成的
      // 空白；数据量大时由外层横向滚动，筛选后按原始相对顺序重新紧凑排列。
      track.style.setProperty('--timeline-content-width', Math.max(12, visibleItems.length * 12) + 'px');
      if (!visibleItems.length) {
        track.innerHTML = '<div class="ion-timeline-empty">All entry types are hidden. Use “Show all” to restore them.</div>';
      } else {
        track.innerHTML = visibleItems.map(function(item) {
          var aria = item.label + ', entry ' + (item.index + 1) + ' of ' + n + ', ' + item.summary + ', click to jump';
          return '<button type="button" class="ion-timeline-bar" data-entry-index="' + item.index +
            '" style="--bar-color:' + item.color + '" aria-label="' + escapeVizHtml(aria) + '"></button>';
        }).join('');
      }
      meta.textContent = (visibleItems.length === n ? n : visibleItems.length + ' / ' + n) +
        ' entries' + (timeLabel ? ' · ' + timeLabel : '');
      tooltip.classList.remove('is-visible');
    }

    function tooltipPosition(clientX, clientY) {
      var rect = tooltip.getBoundingClientRect();
      var left = Math.min(clientX + 12, window.innerWidth - rect.width - 8);
      left = Math.max(8, left);
      var top = clientY - rect.height - 12;
      if (top < 8) top = Math.min(window.innerHeight - rect.height - 8, clientY + 16);
      tooltip.style.left = left + 'px';
      tooltip.style.top = Math.max(8, top) + 'px';
    }

    function showTooltip(bar, clientX, clientY) {
      var item = timelineItems[Number(bar.getAttribute('data-entry-index'))];
      if (!item) return;
      var entry = item.entry || {};
      var time = entry.timestamp ? new Date(entry.timestamp) : null;
      var timeText = time && !isNaN(time.getTime()) ? time.toLocaleString() : 'time unknown';
      var idText = entry.id ? ' · ' + String(entry.id).slice(0, 12) : '';
      tooltip.innerHTML =
        '<div class="ion-tooltip-type" style="--entry-color:' + item.color + '">' +
          '<span class="ion-entry-swatch"></span>' + escapeVizHtml(item.label) +
        '</div>' +
        '<div class="ion-tooltip-meta">#' + (item.index + 1) + ' / ' + n + ' · ' +
          escapeVizHtml(timeText + idText) + '</div>' +
        '<div class="ion-tooltip-summary">' + escapeVizHtml(item.summary) + '</div>';
      tooltip.classList.add('is-visible');
      tooltipPosition(clientX, clientY);
    }

    function findEntryTarget(entry) {
      if (!entry) return null;
      var id = entry.id == null ? '' : String(entry.id);
      if (id) {
        var direct = document.getElementById('entry-' + id);
        if (direct) return direct;
      }
      if (entry.type === 'message') {
        var message = unwrapMessage(entry);
        var role = String(message.role || '').toLowerCase();
        if (role === 'toolresult' || role === 'tool_result' || role === 'tool') {
          var toolCallId = message.toolCallId || message.tool_call_id;
          if (toolCallId) return document.getElementById('tool-call-' + toolCallId);
        }
      }
      return null;
    }

    function jumpToEntry(bar) {
      var item = timelineItems[Number(bar.getAttribute('data-entry-index'))];
      if (!item) return;
      var target = findEntryTarget(item.entry);
      if (!target) {
        notice.textContent = 'Entry #' + (item.index + 1) + ' body target is unavailable. The export may be incomplete.';
        return;
      }
      target.classList.remove('ion-entry-jump-target');
      void target.offsetWidth;
      target.classList.add('ion-entry-jump-target');
      target.scrollIntoView({ behavior: 'smooth', block: 'center' });
      notice.textContent = 'Jumped to entry #' + (item.index + 1) + ' · ' + item.label;
      window.setTimeout(function() { target.classList.remove('ion-entry-jump-target'); }, 1900);
    }

    controls.addEventListener('click', function(event) {
      var reset = event.target.closest('[data-entry-reset]');
      if (reset) {
        hiddenCategories.clear();
      } else {
        var button = event.target.closest('[data-entry-category]');
        if (!button) return;
        var category = button.getAttribute('data-entry-category');
        if (hiddenCategories.has(category)) hiddenCategories.delete(category);
        else hiddenCategories.add(category);
      }
      renderControls();
      renderTimeline();
    });

    root.addEventListener('pointerover', function(event) {
      var bar = event.target.closest('.ion-timeline-bar');
      if (bar) showTooltip(bar, event.clientX, event.clientY);
    });
    root.addEventListener('pointermove', function(event) {
      if (event.target.closest('.ion-timeline-bar') && tooltip.classList.contains('is-visible')) {
        tooltipPosition(event.clientX, event.clientY);
      }
    });
    root.addEventListener('pointerout', function(event) {
      if (event.target.closest('.ion-timeline-bar')) tooltip.classList.remove('is-visible');
    });
    root.addEventListener('focusin', function(event) {
      var bar = event.target.closest('.ion-timeline-bar');
      if (!bar) return;
      var rect = bar.getBoundingClientRect();
      showTooltip(bar, rect.left + rect.width / 2, rect.top);
    });
    root.addEventListener('focusout', function(event) {
      if (event.target.closest('.ion-timeline-bar')) tooltip.classList.remove('is-visible');
    });
    root.addEventListener('click', function(event) {
      var bar = event.target.closest('.ion-timeline-bar');
      if (!bar) return;
      tooltip.classList.remove('is-visible');
      jumpToEntry(bar);
    });

    renderControls();
    renderTimeline();
  }
  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', buildExtVisualization);
  } else {
    buildExtVisualization();
  }
})();
</script>
"#;
    let compact_timeline_script = r#"
<script>
(function() {
  function esc(value) {
    return String(value == null ? '' : value)
      .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;').replace(/'/g, '&#39;');
  }
  function compact(value, limit) {
    var text = String(value == null ? '' : value).replace(/\s+/g, ' ').trim();
    return text.length > limit ? text.slice(0, limit - 1) + '…' : text;
  }
  function message(entry) {
    var msg = entry && entry.message ? entry.message : {};
    var variants = ['User', 'Assistant', 'ToolResult', 'Custom'];
    for (var i = 0; i < variants.length; i++) {
      if (msg[variants[i]]) {
        var value = msg[variants[i]];
        if (!value.role) value.role = variants[i];
        return value;
      }
    }
    return msg;
  }
  function category(entry) {
    var type = String((entry && entry.type) || 'other');
    var semantic = entry && entry.ionMeta;
    if (semantic && semantic.filterType) return semantic.filterType;
    if (semantic && semantic.customType) {
      if (semantic.customClass === 'builtin') return 'builtin:' + semantic.displayType;
      return 'custom';
    }
    if (type === 'custom' || type === 'custom_message') return 'custom';
    if (type !== 'message') return type;
    var role = String(message(entry).role || 'message').toLowerCase();
    if (role === 'toolresult' || role === 'tool_result' || role === 'tool') return 'toolResult';
    if (role === 'custom') return 'custom';
    return role === 'user' || role === 'assistant' ? role : 'message';
  }
  function label(type) {
    if (String(type).indexOf('builtin:') === 0) return String(type).slice(8);
    var labels = { toolResult: 'tool result', branch_summary: 'branch summary',
      model_change: 'model change', thinking_level_change: 'thinking change', active_tools_change: 'tools change' };
    return labels[type] || String(type).replace(/_/g, ' ');
  }
  function color(type) {
    var colors = { user:'#3b82f6', assistant:'#10b981', toolResult:'#f59e0b', custom:'#8b5cf6',
      compaction:'#ef4444', branch_summary:'#d946ef', model_change:'#06b6d4',
      thinking_level_change:'#0ea5e9', active_tools_change:'#a855f7', deletion:'#dc2626', restoration:'#22c55e' };
    if (colors[type]) return colors[type];
    var palette = ['#2563eb','#0f766e','#b45309','#7c3aed','#be123c','#4f46e5','#15803d'];
    var hash = 0;
    for (var i = 0; i < type.length; i++) hash = ((hash << 5) - hash + type.charCodeAt(i)) | 0;
    return palette[Math.abs(hash) % palette.length];
  }
  function previewValue(value, depth) {
    if (value == null || depth > 3) return '';
    if (typeof value === 'string' || typeof value === 'number' || typeof value === 'boolean') return String(value);
    if (Array.isArray(value)) return value.slice(0, 6).map(function(v) { return previewValue(v, depth + 1); }).filter(Boolean).join(' · ');
    if (value.Text) return previewValue(value.Text.text, depth + 1);
    if (value.Thinking) return previewValue(value.Thinking.thinking, depth + 1);
    if (value.ToolCall) return 'tool call ' + (value.ToolCall.name || '');
    if (value.type === 'toolCall') return 'tool call ' + (value.name || '');
    var keys = ['text','thinking','summary','content','label','name','reason','status','data','details'];
    var parts = [];
    keys.forEach(function(key) {
      if (Object.prototype.hasOwnProperty.call(value, key)) {
        var part = previewValue(value[key], depth + 1);
        if (part) parts.push(part);
      }
    });
    return parts.join(' · ');
  }
  function summary(entry) {
    if (entry && entry.customType === 'step-snapshot' && entry.data) {
      var diff = entry.data.diff || {};
      var added = Array.isArray(diff.added) ? diff.added : [];
      var modified = Array.isArray(diff.modified) ? diff.modified : [];
      var deleted = Array.isArray(diff.deleted) ? diff.deleted : [];
      var files = added.concat(modified, deleted).slice(0, 5).join(', ');
      return 'Files +' + added.length + ' ~' + modified.length + ' -' + deleted.length +
        (files ? ' · ' + files : '') + ' · tree ' + String(entry.data.snapshotTreeHash || '').slice(0, 12);
    }
    var source = entry && entry.type === 'message' ? message(entry) : entry;
    return compact(previewValue(source, 0) || 'No preview available', 240);
  }
  function semanticSummary(entry) {
    var meta = entry && entry.ionMeta;
    if (!meta) return '';
    var parts = [String(meta.phase || '').replace(/_/g, ' ')];
    if (meta.source && meta.source.name) {
      var confidence = meta.source.confidence && meta.source.confidence !== 'recorded'
        ? ' (' + meta.source.confidence + ')' : '';
      parts.push('source ' + meta.source.name + confidence);
    }
    if (meta.customType) parts.push('custom ' + meta.customType);
    if (meta.audience) {
      var llm = meta.audience.llmContext;
      if (llm === 'input') parts.push('LLM context input');
      else if (llm === 'output') parts.push('LLM output');
      else if (llm === 'excluded') parts.push('excluded from LLM context');
      else parts.push('audit only');
      if (!meta.audience.liveUi) parts.push('hidden in live UI');
    }
    if (meta.llmCall) parts.push('LLM call #' + meta.llmCall);
    return parts.filter(Boolean).join(' · ');
  }
  function flowSummaryHtml(flow) {
    flow = flow || {};
    var metrics = [
      ['Entries', flow.entries || 0],
      ['LLM calls', flow.llmCalls || 0],
      ['Tool calls', flow.toolCalls || 0],
      ['Tool results', flow.toolResults || 0],
      ['Context inserts', flow.contextInjections || 0],
      ['Custom', flow.customEntries || 0]
    ];
    var extensions = Array.isArray(flow.extensions) ? flow.extensions : [];
    var customs = Array.isArray(flow.customTypes) ? flow.customTypes : [];
    var inventory = flow.typeInventory || {};
    var current = inventory.current || {};
    var supported = inventory.supported || {};
    var entryTypeNames = Array.isArray(supported.entryTypeNames) ? supported.entryTypeNames : [];
    var builtInCustomTypeNames = Array.isArray(supported.builtInCustomTypeNames) ? supported.builtInCustomTypeNames : [];
    var currentTypeTitle = [
      'raw Entry: ' + (current.rawEntryTypes || 0),
      'visible: ' + (current.visibleTypes || 0),
      'message roles: ' + (current.messageRoles || 0),
      'Custom: ' + (current.customTypes || 0),
      'Extensions: ' + (current.extensions || 0)
    ].join(' · ');
    return '<section class="ion-overview-panel ion-flow-summary" aria-label="Execution flow summary">' +
      '<div class="ion-overview-heading"><div><span class="ion-overview-kicker">Flow</span><strong>What happened in this session</strong></div>' +
      '<span class="ion-overview-meta">active branch</span></div>' +
      '<div class="ion-flow-metrics">' + metrics.map(function(item) {
        return '<div class="ion-flow-metric"><strong>' + esc(item[1]) + '</strong><span>' + esc(item[0]) + '</span></div>';
      }).join('') + '</div>' +
      '<div class="ion-flow-groups">' +
      '<div class="ion-flow-group"><span class="ion-flow-group-label">Extensions</span>' +
      (extensions.length ? extensions.map(function(item) {
        return '<span class="ion-flow-chip">' + esc(item.name) + ' ×' + esc(item.count) + '</span>';
      }).join('') : '<span class="ion-flow-chip">none recorded</span>') + '</div>' +
      '<div class="ion-flow-group"><span class="ion-flow-group-label">Custom</span>' +
      (customs.length ? customs.map(function(item) {
        var source = item.source && item.source !== 'unknown' ? ' · ' + item.source : '';
        return '<span class="ion-flow-chip" title="' + esc(item.customType) + '">' + esc(item.displayType) + source + ' ×' + esc(item.count) + '</span>';
      }).join('') : '<span class="ion-flow-chip">none</span>') + '</div>' +
      '<div class="ion-flow-group"><span class="ion-flow-group-label">Current types</span>' +
      '<span class="ion-flow-chip" title="' + esc(currentTypeTitle) + '">visible ' + esc(current.visibleTypes || 0) + '</span>' +
      '<span class="ion-flow-chip">raw Entry ' + esc(current.rawEntryTypes || 0) + '</span>' +
      '<span class="ion-flow-chip">message roles ' + esc(current.messageRoles || 0) + '</span>' +
      '<span class="ion-flow-chip">Custom ' + esc(current.customTypes || 0) + '</span>' +
      '<span class="ion-flow-chip">built-in Custom ' + esc(current.builtInCustomTypes || 0) + '</span>' +
      '<span class="ion-flow-chip">Extension Custom ' + esc(current.extensionCustomTypes || 0) + '</span>' +
      '<span class="ion-flow-chip">Extensions ' + esc(current.extensions || 0) + '</span></div>' +
      '<div class="ion-flow-group"><span class="ion-flow-group-label">ION catalog</span>' +
      '<span class="ion-flow-chip" title="' + esc(entryTypeNames.join(', ')) + '">Entry ' + esc(supported.entryTypes || 0) + '</span>' +
      '<span class="ion-flow-chip" title="' + esc(builtInCustomTypeNames.join(', ')) + '">built-in Custom ' + esc(supported.builtInCustomTypes || 0) + '</span>' +
      '<span class="ion-flow-chip">Extension Custom is open-ended</span></div>' +
      '</div></section>';
  }
  function build() {
    var dataEl = document.getElementById('session-data');
    if (!dataEl) return;
    var decoded;
    try {
      var bin = atob(dataEl.textContent.trim());
      var bytes = new Uint8Array(bin.length);
      for (var i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
      decoded = JSON.parse(new TextDecoder('utf-8').decode(bytes));
    } catch (error) { return; }
    var entries = Array.isArray(decoded.timelineEntries) ? decoded.timelineEntries : (decoded.entries || []);
    if (!entries.length) return;
    var n = entries.length;
    var items = entries.map(function(entry, index) {
      var type = category(entry);
      return { entry:entry, index:index, type:type, label:label(type), color:color(type), summary:summary(entry) };
    });
    var counts = {};
    items.forEach(function(item) { counts[item.type] = (counts[item.type] || 0) + 1; });
    var order = ['user','assistant','toolResult','custom','compaction','branch_summary'];
    var types = Object.keys(counts).sort(function(a, b) {
      var ai = order.indexOf(a), bi = order.indexOf(b);
      if (ai < 0) ai = 999;
      if (bi < 0) bi = 999;
      return ai - bi || label(a).localeCompare(label(b));
    });
    var times = entries.map(function(entry) { var t = new Date(entry.timestamp || 0); return isNaN(t.getTime()) ? null : t; }).filter(Boolean);
    var start = times.length ? times[0].toLocaleTimeString() : '';
    var end = times.length ? times[times.length - 1].toLocaleTimeString() : '';
    var hidden = new Set();
    var html = '<div id="ion-ext-viz" aria-label="Session overview">' + flowSummaryHtml(decoded.flowSummary) +
      '<section class="ion-overview-panel"><div class="ion-overview-heading"><div><span class="ion-overview-kicker">Entries</span><strong>Type filters</strong></div>' +
      '<span class="ion-overview-meta">' + types.length + ' types</span></div><div class="ion-entry-type-controls" id="ion-entry-type-controls"></div></section>' +
      '<section class="ion-overview-panel"><div class="ion-overview-heading"><div><span class="ion-overview-kicker">Sequence</span><strong>Complete timeline</strong></div>' +
      '<span class="ion-overview-meta" id="ion-timeline-meta"></span></div><div class="ion-timeline-scroll"><div class="ion-timeline-track" id="ion-timeline-track"></div></div>' +
      '<div class="ion-timeline-axis"><span>' + esc(start && end ? start + ' → ' + end : 'time unavailable') + '</span></div>' +
      '<div class="ion-timeline-notice" id="ion-timeline-notice" aria-live="polite">Hover for a summary · click to jump to the entry</div></section>' +
      '<div class="ion-timeline-tooltip" id="ion-timeline-tooltip" role="tooltip"></div></div>';
    var old = document.getElementById('ion-ext-viz');
    if (old) old.remove();
    var banner = document.getElementById('ion-stats-banner');
    if (!banner) return;
    banner.insertAdjacentHTML('afterend', html);
    var root = document.getElementById('ion-ext-viz');
    var controls = document.getElementById('ion-entry-type-controls');
    var track = document.getElementById('ion-timeline-track');
    var meta = document.getElementById('ion-timeline-meta');
    var notice = document.getElementById('ion-timeline-notice');
    var tooltip = document.getElementById('ion-timeline-tooltip');
    function renderControls() {
      controls.innerHTML = types.map(function(type) {
        return '<button type="button" class="ion-entry-filter" data-entry-category="' + esc(type) + '" aria-pressed="' + (!hidden.has(type)) + '" style="--entry-color:' + color(type) + '">' +
          '<span class="ion-entry-swatch"></span><span>' + esc(label(type)) + '</span><span class="ion-entry-filter-count">' + counts[type] + '</span></button>';
      }).join('') + '<button type="button" class="ion-entry-filter-reset" data-entry-reset' + (hidden.size ? '' : ' disabled') + '>Show all</button>';
    }
    function renderTimeline() {
      var visible = items.filter(function(item) { return !hidden.has(item.type); });
      track.style.setProperty('--timeline-content-width', Math.max(12, visible.length * 12) + 'px');
      track.innerHTML = visible.map(function(item) {
        var aria = item.label + ', entry ' + (item.index + 1) + ' of ' + n + ', ' + item.summary + ', click to jump';
        return '<button type="button" class="ion-timeline-bar" data-entry-index="' + item.index + '" style="--bar-color:' + item.color + '" aria-label="' + esc(aria) + '"></button>';
      }).join('');
      meta.textContent = (visible.length === n ? n : visible.length + ' / ' + n) + ' entries' + (start && end ? ' · ' + start + ' → ' + end : '');
      tooltip.classList.remove('is-visible');
      items.forEach(function(item) {
        var target = targetFor(item.entry, item.index);
        if (target) target.classList.toggle('ion-entry-filter-hidden', hidden.has(item.type));
      });
    }
    function showTooltip(bar, x, y) {
      var item = items[Number(bar.getAttribute('data-entry-index'))];
      if (!item) return;
      var entry = item.entry;
      var time = entry.timestamp ? new Date(entry.timestamp).toLocaleString() : 'time unknown';
      tooltip.innerHTML = '<div class="ion-tooltip-type"><span class="ion-entry-swatch" style="--entry-color:' + item.color + '"></span>' + esc(item.label) + '</div>' +
        '<div class="ion-tooltip-meta">#' + (item.index + 1) + ' / ' + n + ' · ' + esc(time + (entry.id ? ' · ' + String(entry.id).slice(0, 12) : '')) + '</div>' +
        '<div class="ion-tooltip-meta">' + esc(semanticSummary(entry)) + '</div>' +
        '<div class="ion-tooltip-summary">' + esc(item.summary) + '</div>';
      tooltip.classList.add('is-visible');
      var rect = tooltip.getBoundingClientRect();
      tooltip.style.left = Math.max(8, Math.min(x + 12, innerWidth - rect.width - 8)) + 'px';
      tooltip.style.top = Math.max(8, y - rect.height - 12) + 'px';
    }
    function targetFor(entry, index) {
      var direct = entry.id ? document.getElementById('entry-' + entry.id) : null;
      if (direct) return direct;
      if (entry.type === 'message') {
        var msg = message(entry);
        var toolCallId = msg.toolCallId || msg.tool_call_id;
        if (toolCallId) return document.getElementById('tool-call-' + toolCallId);
      }
      return document.getElementById('ion-timeline-entry-' + index);
    }
    controls.addEventListener('click', function(event) {
      if (event.target.closest('[data-entry-reset]')) hidden.clear();
      else {
        var button = event.target.closest('[data-entry-category]');
        if (!button) return;
        var type = button.getAttribute('data-entry-category');
        if (hidden.has(type)) hidden.delete(type); else hidden.add(type);
      }
      renderControls(); renderTimeline();
    });
    root.addEventListener('pointerover', function(event) {
      var bar = event.target.closest('.ion-timeline-bar');
      if (bar) showTooltip(bar, event.clientX, event.clientY);
    });
    root.addEventListener('pointermove', function(event) {
      var bar = event.target.closest('.ion-timeline-bar');
      if (bar && tooltip.classList.contains('is-visible')) showTooltip(bar, event.clientX, event.clientY);
    });
    root.addEventListener('pointerout', function(event) {
      if (event.target.closest('.ion-timeline-bar')) tooltip.classList.remove('is-visible');
    });
    root.addEventListener('focusin', function(event) {
      var bar = event.target.closest('.ion-timeline-bar');
      if (bar) { var rect = bar.getBoundingClientRect(); showTooltip(bar, rect.left, rect.top); }
    });
    root.addEventListener('click', function(event) {
      var bar = event.target.closest('.ion-timeline-bar');
      if (!bar) return;
      var item = items[Number(bar.getAttribute('data-entry-index'))];
      var target = item && targetFor(item.entry, item.index);
      tooltip.classList.remove('is-visible');
      if (!target) { notice.textContent = 'Entry #' + (item.index + 1) + ' body target is unavailable. The export may be incomplete.'; return; }
      var foldedEntry = target.closest('.ion-entry-fold');
      if (foldedEntry && window.ionSetEntryExpanded) window.ionSetEntryExpanded(foldedEntry, true);
      target.classList.remove('ion-entry-jump-target'); void target.offsetWidth;
      target.classList.add('ion-entry-jump-target');
      target.scrollIntoView({ behavior:'smooth', block:'center' });
      notice.textContent = 'Jumped to entry #' + (item.index + 1) + ' · ' + item.label;
      setTimeout(function() { target.classList.remove('ion-entry-jump-target'); }, 1900);
    });
    renderControls(); renderTimeline();
  }
  if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', build); else build();
})();
</script>
"#;
    html = html.replacen(
        "</body>",
        &format!("{}\n</body>", compact_timeline_script),
        1,
    );

    // Guarantee the central contract of the export: every Timeline item resolves
    // to a visible body target. Native types unsupported by pi are rendered by the
    // generic fallback above; ToolResult points to its nested tool call. Correlated
    // Hook events are grouped under that tool target; lifecycle Hooks fall back to
    // the preceding visible card instead of relying on internal turn metadata.
    let complete_entry_body_script = r#"
<script>
document.addEventListener('DOMContentLoaded', function() {
  var messages = document.getElementById('messages');
  var dataEl = document.getElementById('session-data');
  if (!messages || !dataEl) return;

  function decodeData() {
    try {
      var bin = atob(dataEl.textContent.trim());
      var bytes = new Uint8Array(bin.length);
      for (var i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
      return JSON.parse(new TextDecoder('utf-8').decode(bytes));
    } catch (error) { return {}; }
  }
  function message(entry) {
    var msg = entry && entry.message || {};
    return msg.User || msg.Assistant || msg.ToolResult || msg.Custom || msg;
  }
  function customType(entry) {
    var msg = message(entry);
    return entry.customType || entry.custom_type || msg.customType || msg.custom_type || '';
  }
  function toolCallId(entry) {
    var msg = message(entry);
    return msg.toolCallId || msg.tool_call_id || entry.toolCallId || entry.tool_call_id ||
      (entry.data && (entry.data.toolCallId || entry.data.tool_call_id)) ||
      (entry.details && (entry.details.toolCallId || entry.details.tool_call_id)) || '';
  }
  function targetFor(entry, index) {
    var direct = entry && entry.id ? document.getElementById('entry-' + entry.id) : null;
    if (direct) return direct;
    var callId = toolCallId(entry || {});
    if (callId) {
      var tool = document.getElementById('tool-call-' + callId);
      if (tool) return tool;
    }
    return document.getElementById('ion-timeline-entry-' + index);
  }
  function topLevelTarget(node) {
    if (!node) return null;
    var current = node;
    while (current.parentElement && current.parentElement !== messages) current = current.parentElement;
    return current.parentElement === messages ? current : null;
  }
  function compact(value) {
    var text = '';
    if (typeof value === 'string') text = value;
    else if (value !== undefined) {
      try { text = JSON.stringify(value); } catch (error) { text = String(value); }
    }
    text = text.replace(/\s+/g, ' ').trim();
    return text.length > 320 ? text.slice(0, 319) + '…' : text;
  }
  function summary(entry) {
    var msg = message(entry || {});
    return compact(entry.summary || entry.content || msg.content || entry.data || entry.status || 'No additional details');
  }
  function label(entry) {
    var msg = message(entry || {});
    if (entry.type === 'message') {
      var role = String(msg.role || '').toLowerCase();
      if (role === 'toolresult' || role === 'tool') return 'tool result';
      if (role) return role;
    }
    return customType(entry) || entry.type || 'entry';
  }
  function makeFallback(entry, index) {
    var card = document.createElement('div');
    card.className = 'ion-generic-entry';
    card.id = entry.id ? 'entry-' + entry.id : 'ion-timeline-entry-' + index;
    card.setAttribute('data-entry-type', label(entry));
    var typeEl = document.createElement('div');
    typeEl.className = 'ion-generic-entry-type';
    typeEl.textContent = '[' + label(entry) + ']';
    var contentEl = document.createElement('div');
    contentEl.className = 'ion-generic-entry-content';
    contentEl.textContent = summary(entry);
    card.append(typeEl, contentEl);
    return card;
  }

  var decoded = decodeData();
  var entries = Array.isArray(decoded.timelineEntries) ? decoded.timelineEntries : (decoded.entries || []);

  // A missing target usually means an orphan ToolResult. Preserve it as a compact
  // body card instead of leaving a dead Timeline marker.
  entries.forEach(function(entry, index) {
    if (targetFor(entry, index)) return;
    var card = makeFallback(entry, index);
    var nextTop = null;
    for (var next = index + 1; next < entries.length; next++) {
      nextTop = topLevelTarget(targetFor(entries[next], next));
      if (nextTop) break;
    }
    if (nextTop) messages.insertBefore(card, nextTop); else messages.appendChild(card);
  });

  function nestedContainer(owner) {
    var container = owner.querySelector(':scope > .ion-entry-nested-events');
    if (!container) {
      container = document.createElement('div');
      container.className = 'ion-entry-nested-events';
      container.setAttribute('aria-label', 'Related entries');
      owner.appendChild(container);
    }
    return container;
  }

  // Tool-correlated hooks belong to the exact ToolResult. Older lifecycle hooks
  // without correlation data fall back to the preceding visible body card.
  entries.forEach(function(entry, index) {
    if (customType(entry) !== 'hook_event' || !entry.id) return;
    var hook = document.getElementById('entry-' + entry.id);
    if (!hook) return;
    var owner = null;
    var callId = toolCallId(entry);
    if (callId) owner = document.getElementById('tool-call-' + callId);
    if (!owner) {
      for (var previous = index - 1; previous >= 0; previous--) {
        owner = topLevelTarget(targetFor(entries[previous], previous));
        if (owner && owner !== hook) break;
        owner = null;
      }
    }
    if (owner && owner !== hook && !hook.contains(owner)) nestedContainer(owner).appendChild(hook);
  });

  var missing = [];
  entries.forEach(function(entry, index) {
    if (!targetFor(entry, index)) missing.push(index);
  });
  window.ionEntryBodyCoverage = { total: entries.length, resolved: entries.length - missing.length, missing: missing };
  messages.setAttribute('data-ion-body-coverage', missing.length ? 'incomplete' : 'complete');
  document.dispatchEvent(new CustomEvent('ion-body-targets-ready', { detail: window.ionEntryBodyCoverage }));
});
</script>
"#;
    html = html.replacen(
        "</body>",
        &format!("{}\n</body>", complete_entry_body_script),
        1,
    );

    // Render the exact same semantic metadata used by Timeline on each body
    // target. This turns the export into an execution audit: users can see who
    // produced an entry, whether it reached LLM context, whether the live UI hid
    // it, and which provider/model/tool/Extension was involved.
    let entry_semantics_script = r#"
<script>
document.addEventListener('DOMContentLoaded', function() {
  function decodeData() {
    var el = document.getElementById('session-data');
    if (!el) return {};
    try {
      var bin = atob(el.textContent.trim());
      var bytes = new Uint8Array(bin.length);
      for (var i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
      return JSON.parse(new TextDecoder('utf-8').decode(bytes));
    } catch (error) { return {}; }
  }
  function message(entry) {
    var msg = entry && entry.message ? entry.message : {};
    var variants = ['User','Assistant','ToolResult','BashExecution','Custom','BranchSummary','CompactionSummary'];
    for (var i = 0; i < variants.length; i++) if (msg[variants[i]]) return msg[variants[i]];
    return msg;
  }
  function toolCallId(entry) {
    var msg = message(entry || {});
    return msg.toolCallId || msg.tool_call_id || entry.toolCallId || entry.tool_call_id ||
      (entry.details && (entry.details.toolCallId || entry.details.tool_call_id)) || '';
  }
  function targetFor(entry, index) {
    var direct = entry.id ? document.getElementById('entry-' + entry.id) : null;
    if (direct) return direct;
    var callId = toolCallId(entry);
    if (callId) return document.getElementById('tool-call-' + callId);
    return document.getElementById('ion-timeline-entry-' + index);
  }
  function phaseLabel(phase) {
    return ({
      user_input:'User input', context_injection:'Context injection', context_snapshot:'Context snapshot',
      llm_response:'LLM response', tool_request:'LLM tool request', tool_result:'Tool result',
      extension_event:'Extension event', custom_event:'Custom audit', session_control:'Session control'
    })[phase] || String(phase || 'entry').replace(/_/g, ' ');
  }
  function addChip(strip, text, kind, title) {
    if (!text) return;
    var chip = document.createElement('span');
    chip.className = 'ion-entry-provenance-chip';
    chip.dataset.kind = kind || 'meta';
    chip.textContent = text;
    if (title) chip.title = title;
    strip.appendChild(chip);
  }
  function tokenText(usage) {
    if (!usage || typeof usage !== 'object') return '';
    var input = usage.input == null ? '?' : usage.input;
    var output = usage.output == null ? '?' : usage.output;
    return 'tokens ↑' + input + ' ↓' + output;
  }
  function decorate(entry, index) {
    var meta = entry && entry.ionMeta;
    var target = targetFor(entry, index);
    if (!meta || !target || target.querySelector(':scope > .ion-entry-provenance[data-entry-sequence="' + meta.sequence + '"]')) return;
    var strip = document.createElement('div');
    strip.className = 'ion-entry-provenance';
    strip.dataset.entrySequence = String(meta.sequence || index + 1);
    strip.setAttribute('aria-label', 'Entry provenance');
    addChip(strip, phaseLabel(meta.phase), 'phase');
    if (meta.source && meta.source.name) {
      var confidence = meta.source.confidence && meta.source.confidence !== 'recorded'
        ? ' · ' + meta.source.confidence : '';
      addChip(strip, meta.source.kind + ' · ' + meta.source.name + confidence, 'source',
        meta.source.confidence === 'inferred' ? 'Source inferred from an ION built-in Custom type' : 'Source recorded by session data');
    }
    if (meta.customType) addChip(strip, 'customType · ' + meta.customType, 'custom');
    if (meta.audience) {
      var llm = meta.audience.llmContext;
      if (llm === 'input') addChip(strip, 'LLM context · included', 'llm');
      else if (llm === 'output') addChip(strip, 'LLM output', 'llm');
      else if (llm === 'excluded') addChip(strip, 'LLM context · excluded', 'hidden');
      else addChip(strip, 'LLM context · no', 'hidden');
      if (!meta.audience.liveUi) addChip(strip, 'live UI · hidden', 'hidden');
    }
    if (meta.llmCall) addChip(strip, 'LLM call #' + meta.llmCall, 'llm');
    if (meta.llm) {
      var identity = [meta.llm.provider, meta.llm.model].filter(Boolean).join('/');
      addChip(strip, identity, 'llm');
      addChip(strip, tokenText(meta.llm.usage), 'llm');
      if (meta.llm.stopReason) addChip(strip, 'stop · ' + meta.llm.stopReason, 'llm');
    }
    if (Array.isArray(meta.toolCalls) && meta.toolCalls.length) {
      addChip(strip, 'tools · ' + meta.toolCalls.map(function(call) { return call.name || 'tool'; }).join(', '), 'tool');
    }
    target.appendChild(strip);
    target.dataset.ionPhase = meta.phase || '';
    target.dataset.ionSource = meta.source && meta.source.name || '';
    target.dataset.ionLlmContext = meta.audience && meta.audience.llmContext || '';
  }
  var data = decodeData();
  var entries = Array.isArray(data.timelineEntries) ? data.timelineEntries : [];
  entries.forEach(decorate);
  window.ionEntrySemantics = {
    total: entries.length,
    decorated: document.querySelectorAll('.ion-entry-provenance').length
  };
  document.dispatchEvent(new CustomEvent('ion-entry-semantics-ready', { detail: window.ionEntrySemantics }));
});
</script>
"#;
    html = html.replacen(
        "</body>",
        &format!("{}\n</body>", entry_semantics_script),
        1,
    );

    // ION: long Entries show at least three rendered content lines instead of
    // counting role labels, timestamps and card controls as preview lines. Folding is
    // only useful when expanding reveals more than three additional content lines;
    // shorter remainders stay fully visible. The hint reports those hidden visual lines
    // and expands the original DOM in place.
    // A MutationObserver reapplies the wrapper after pi's branch navigation rerenders
    // #messages from cached DOM nodes.
    let entry_fold_script = r#"
<script>
document.addEventListener('DOMContentLoaded', function() {
  var messages = document.getElementById('messages');
  if (!messages) return;
  var minimumVisibleContentLines = 3;
  var minimumHiddenContentLines = 3;

  function entryKind(entry) {
    if (entry.classList.contains('user-message') || entry.classList.contains('skill-user-entry')) return ['User', '#2e90fa'];
    if (entry.classList.contains('assistant-message')) return ['Assistant', '#12b76a'];
    if (entry.classList.contains('tool-execution')) return ['Tool Result', '#f79009'];
    if (entry.classList.contains('hook-message') || entry.classList.contains('custom-message')) return ['Custom', '#8b5cf6'];
    if (entry.classList.contains('compaction')) return ['Compaction', '#ef4444'];
    if (entry.classList.contains('branch-summary')) return ['Branch Summary', '#d946ef'];
    if (entry.classList.contains('model-change')) return ['Model Change', '#06b6d4'];
    return ['Entry', '#667085'];
  }

  function setExpanded(entry, expanded) {
    var hint = entry.querySelector(':scope > .ion-entry-fold-hint');
    if (!hint) return;
    entry.setAttribute('data-ion-entry-expanded', expanded ? 'true' : 'false');
    hint.setAttribute('aria-expanded', expanded ? 'true' : 'false');
    hint.textContent = expanded
      ? '↑ Collapse'
      : '... (' + (hint.dataset.hiddenLines || '1') + ' more lines, click to expand)';
    hint.setAttribute('aria-label', entryKind(entry)[0] + (expanded ? ', collapse content' : ', expand remaining content'));
    if (entry.classList.contains('compaction')) entry.classList.toggle('expanded', expanded);
  }
  window.ionSetEntryExpanded = setExpanded;

  function foldContentRoots(content) {
    var selectors = [
      ':scope > .markdown-content',
      ':scope > .assistant-text',
      ':scope > .thinking-block',
      ':scope > .tool-execution',
      ':scope > .tool-command',
      ':scope > .tool-header',
      ':scope > .tool-output',
      ':scope > .tool-images',
      ':scope > .compaction-content',
      ':scope > .error-text',
      ':scope > .skill-invocation',
      ':scope > .user-message',
      ':scope > .ion-generic-entry-content'
    ];
    var roots = Array.from(content.querySelectorAll(selectors.join(',')));
    return roots.length ? roots : [content];
  }

  function collectVisualLines(content) {
    var rects = [];
    foldContentRoots(content).forEach(function(root) {
      var walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
      while (walker.nextNode()) {
        var node = walker.currentNode;
        if (!node.nodeValue || !node.nodeValue.trim()) continue;
        var parent = node.parentElement;
        if (!parent || parent.closest('button, .copy-link, .expand-hint')) continue;
        var range = document.createRange();
        range.selectNodeContents(node);
        Array.from(range.getClientRects()).forEach(function(rect) {
          if (rect.width > 0 && rect.height > 0) rects.push({ top: rect.top, bottom: rect.bottom });
        });
        range.detach();
      }
    });
    rects.sort(function(a, b) { return a.top - b.top || a.bottom - b.bottom; });
    return rects.reduce(function(lines, rect) {
      var line = lines[lines.length - 1];
      if (!line || Math.abs(line.top - rect.top) > 2) {
        lines.push({ top: rect.top, bottom: rect.bottom });
      } else {
        line.bottom = Math.max(line.bottom, rect.bottom);
      }
      return lines;
    }, []);
  }

  function measureEntry(entry) {
    var content = entry.querySelector(':scope > .ion-entry-fold-content');
    var hint = entry.querySelector(':scope > .ion-entry-fold-hint');
    if (!content || !hint) return;
    var visualLines = collectVisualLines(content);
    var contentRect = content.getBoundingClientRect();
    var previewLine = visualLines[minimumVisibleContentLines - 1];
    var previewHeight = previewLine
      ? Math.ceil(previewLine.bottom - contentRect.top + 2)
      : content.scrollHeight;
    var expanded = entry.getAttribute('data-ion-entry-expanded') === 'true';
    entry.style.setProperty('--ion-entry-preview-height', previewHeight + 'px');
    var hiddenHeight = Math.max(0, content.scrollHeight - previewHeight);
    var hiddenLines = Math.max(0, visualLines.length - minimumVisibleContentLines);
    var foldable = hiddenLines > minimumHiddenContentLines && hiddenHeight > 2;
    entry.setAttribute('data-ion-entry-foldable', foldable ? 'true' : 'false');
    hint.dataset.hiddenLines = String(hiddenLines);
    hint.hidden = !foldable;
    setExpanded(entry, expanded && foldable);
  }

  function configureToolOutput(output) {
    if (!output || output.classList.contains('expandable')) return;
    output.removeAttribute('data-ion-output-foldable');
    output.style.removeProperty('--ion-tool-output-preview-height');
    var visualLines = collectVisualLines(output);
    var previewLine = visualLines[minimumVisibleContentLines - 1];
    var outputRect = output.getBoundingClientRect();
    var previewHeight = previewLine
      ? Math.ceil(previewLine.bottom - outputRect.top + 2)
      : output.scrollHeight;
    var hiddenLines = Math.max(0, visualLines.length - minimumVisibleContentLines);
    var foldable = hiddenLines > minimumHiddenContentLines && output.scrollHeight - previewHeight > 2;
    output.setAttribute('data-ion-output-foldable', foldable ? 'true' : 'false');
    output.setAttribute('data-ion-output-hidden-lines', String(hiddenLines));
    if (foldable) {
      output.style.setProperty('--ion-tool-output-preview-height', previewHeight + 'px');
    } else {
      output.classList.remove('expanded');
    }
  }

  function decorateEntry(entry) {
    if (!entry || entry.dataset.ionEntryFoldReady === 'true') return;
    var kind = entryKind(entry);
    var content = document.createElement('div');
    content.className = 'ion-entry-fold-content';
    content.id = entry.id + '-fold-content';
    while (entry.firstChild) content.appendChild(entry.firstChild);

    var hint = document.createElement('button');
    hint.type = 'button';
    hint.className = 'ion-entry-fold-hint';
    hint.setAttribute('aria-expanded', 'false');
    hint.setAttribute('aria-controls', content.id);
    hint.textContent = '... (more lines, click to expand)';

    entry.removeAttribute('onclick');
    entry.dataset.ionEntryFoldReady = 'true';
    entry.classList.add('ion-entry-fold');
    entry.style.setProperty('--ion-entry-accent', kind[1]);
    entry.append(content, hint);
    requestAnimationFrame(function() { measureEntry(entry); });
  }

  function decorateEntries() {
    messages.querySelectorAll('.tool-output:not(.expandable)').forEach(configureToolOutput);
    messages.querySelectorAll(':scope > [id^="entry-"]').forEach(decorateEntry);
  }

  messages.addEventListener('click', function(event) {
    var hint = event.target.closest('.ion-entry-fold-hint');
    if (hint) {
      event.stopPropagation();
      var entry = hint.closest('.ion-entry-fold');
      setExpanded(entry, entry.getAttribute('data-ion-entry-expanded') !== 'true');
      return;
    }
    var output = event.target.closest('.tool-output:not(.expandable)[data-ion-output-foldable="true"]');
    if (output && !window.getSelection().toString()) {
      output.classList.toggle('expanded');
      var owner = output.closest('.ion-entry-fold');
      if (owner) requestAnimationFrame(function() { measureEntry(owner); });
    }
  });

  decorateEntries();
  new MutationObserver(decorateEntries).observe(messages, { childList: true });
  var resizeTimer;
  window.addEventListener('resize', function() {
    clearTimeout(resizeTimer);
    resizeTimer = setTimeout(function() {
      messages.querySelectorAll('.tool-output:not(.expandable)').forEach(configureToolOutput);
      messages.querySelectorAll(':scope > .ion-entry-fold').forEach(measureEntry);
    }, 120);
  });
});
</script>
"#;
    html = html.replacen("</body>", &format!("{}\n</body>", entry_fold_script), 1);

    // 子 session 的文件名（sub_<sid>.html / 旧版 fork_<sid>.html）在 base64 编码的
    // session-data 里，HTML 写入前替换看不到明文。改为在 HTML 末尾注入一段 JavaScript：
    // 页面加载后，遍历 DOM 把 "sub_xxxxxxxxxxxx.html" / "fork_xxxxxxxxxxxx.html" 文本替换成可点击链接。
    // 注意：正则要同时匹配新前缀 sub_ 和旧前缀 fork_，兼容历史导出的 HTML。
    let fork_link_script = r#"
<script>
(function() {
  function makeForkLinks() {
    var walker = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT, {
      acceptNode: function(n) {
        // 跳过 <a> 内的文本（已转成链接的不重复处理）
        if (n.parentNode && n.parentNode.tagName === 'A') return NodeFilter.FILTER_REJECT;
        // 跳过已标记处理的容器
        if (n.parentNode && n.parentNode.getAttribute && n.parentNode.getAttribute('data-fork-done') === '1') return NodeFilter.FILTER_REJECT;
        return NodeFilter.FILTER_ACCEPT;
      }
    }, false);
    var node;
    var nodesToReplace = [];
    while (node = walker.nextNode()) {
      var text = node.textContent;
      // 同时匹配 sub_<sid>.html（新）和 fork_<sid>.html（旧）
      var match = text.match(/(?:sub|fork)_[0-9a-f-]{8,}\.html/);
      if (match) {
        nodesToReplace.push({node: node, match: match[0]});
      }
    }
    nodesToReplace.forEach(function(item) {
      var text = item.node.textContent;
      var before = text.substring(0, text.indexOf(item.match));
      var after = text.substring(text.indexOf(item.match) + item.match.length);
      var link = document.createElement('a');
      link.href = item.match;
      link.textContent = '🔗 ' + item.match;
      link.style.cssText = 'color:#2563eb;text-decoration:underline;font-weight:bold;';
      var parent = item.node.parentNode;
      // 标记父容器已处理（防 setTimeout 重跑重复加 🔗）
      if (parent.setAttribute) parent.setAttribute('data-fork-done', '1');
      parent.insertBefore(document.createTextNode(before), item.node);
      parent.insertBefore(link, item.node);
      parent.insertBefore(document.createTextNode(after), item.node);
      parent.removeChild(item.node);
    });
  }
  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', makeForkLinks);
  } else {
    makeForkLinks();
  }
  // pi template 异步渲染，需要再等一下
  setTimeout(makeForkLinks, 500);
  setTimeout(makeForkLinks, 2000);
})();
</script>"#;

    // 在 </body> 前插入 fork link script
    if let Some(pos) = html.rfind("</body>") {
        html.insert_str(pos, fork_link_script);
    } else {
        html.push_str(fork_link_script);
    }

    std::fs::write(output_path, html)?;
    tracing::info!(
        "exported {session_id} → {} ({} entries)",
        output_path.display(),
        entries.len()
    );
    Ok(())
}

/// Convert a single ION entry to pi-compatible format.
///
/// 子 session HTML 文件名前缀（旧版导出用 `fork_`，新版统一 `sub_`）。
/// JS 端的链接识别正则会同时匹配两个前缀（见 export_session_internal 末尾的 makeForkLinks）。
const SUB_HTML_PREFIX: &str = "sub_";
/// 旧版前缀，仅用于向后兼容识别历史导出 HTML 里的链接。
#[allow(dead_code)]
const SUB_HTML_LEGACY_PREFIX: &str = "fork_";

/// 生成子 session 的 HTML 文件名：`<prefix><sid 前 12 字符>.html`。
/// 截 12 字符避免文件名过长（sid 是 UUID，12 字符足够区分）。
fn sub_html_filename(sid: &str) -> String {
    let short = &sid[..12.min(sid.len())];
    format!("{SUB_HTML_PREFIX}{short}.html")
}

/// Return the inner message payload for both ION's externally-tagged enum
/// representation and the already-flat representation used by older fixtures.
fn export_message_payload(entry: &Value) -> Option<&Value> {
    let message = entry.get("message")?;
    for wrapper in [
        "User",
        "Assistant",
        "ToolResult",
        "BashExecution",
        "Custom",
        "BranchSummary",
        "CompactionSummary",
    ] {
        if let Some(inner) = message.get(wrapper) {
            return Some(inner);
        }
    }
    Some(message)
}

fn export_message_role(entry: &Value) -> Option<&str> {
    let message = entry.get("message")?;
    if let Some(role) = export_message_payload(entry)
        .and_then(|payload| payload.get("role"))
        .and_then(Value::as_str)
    {
        return Some(role);
    }
    for (wrapper, role) in [
        ("User", "user"),
        ("Assistant", "assistant"),
        ("ToolResult", "toolResult"),
        ("BashExecution", "bashExecution"),
        ("Custom", "custom"),
        ("BranchSummary", "branchSummary"),
        ("CompactionSummary", "compactionSummary"),
    ] {
        if message.get(wrapper).is_some() {
            return Some(role);
        }
    }
    None
}

fn export_custom_type(entry: &Value) -> Option<&str> {
    entry
        .get("customType")
        .or_else(|| entry.get("custom_type"))
        .and_then(Value::as_str)
        .or_else(|| {
            let message = export_message_payload(entry)?;
            message
                .get("customType")
                .or_else(|| message.get("custom_type"))
                .and_then(Value::as_str)
        })
}

/// Custom types owned by ION. The tuple is `(customType, display label, source)`.
/// Runtime Extension types are intentionally absent and therefore render as
/// generic `Custom` while retaining their recorded Extension source.
const BUILT_IN_CUSTOM_TYPE_CATALOG: &[(&str, &str, &str)] = &[
    ("hook_event", "Hook", "hooks"),
    ("hook_handler_executed", "Hook", "hooks"),
    ("diagnostics", "Diagnostics", "lsp"),
    ("bash_result", "Background Bash Result", "bash"),
    ("dev_servers", "Dev Server Context", "dev-server-detector"),
    ("remind", "Budget Reminder", "kernel"),
    ("system_prompt", "System Prompt Snapshot", "kernel"),
    ("session_name", "Session Title", "session"),
    ("sub_session_separator", "Sub-session", "orchestration"),
    ("file-approval", "File Approval", "file-snapshot"),
    ("step-snapshot", "File Snapshot", "file-snapshot"),
    ("approval_deny", "Review Rejection", "review"),
    ("memory_injected", "Memory", "memory"),
    ("memory_saved", "Memory", "memory"),
    ("memory_search", "Memory", "memory"),
    ("memory_searching", "Memory", "memory"),
    ("memory_consolidate", "Memory", "memory"),
    ("memory_search_stat", "Memory", "memory"),
    ("llm_retry", "LLM Retry", "retry"),
    ("retry_exceeded", "LLM Retry", "retry"),
    ("compacting", "Compaction Event", "compaction"),
    ("compaction_done", "Compaction Event", "compaction"),
    ("process_started", "Background Process", "bash"),
    ("process_completed", "Background Process", "bash"),
    ("process_killed", "Background Process", "bash"),
];

/// Known ION-owned Custom types get their own semantic label. Unknown runtime
/// Extension types deliberately remain `Custom`; the renderer may still show a
/// recorded Extension name without inventing a type taxonomy for third parties.
fn built_in_custom_semantics(custom_type: &str) -> Option<(&'static str, &'static str)> {
    BUILT_IN_CUSTOM_TYPE_CATALOG
        .iter()
        .find(|(name, _, _)| *name == custom_type)
        .map(|(_, display, source)| (*display, *source))
}

fn normalize_export_source(source: &str) -> String {
    match source {
        "hook" => "hooks".into(),
        "dev_server" | "dev-server" => "dev-server-detector".into(),
        other => other.to_string(),
    }
}

fn recorded_custom_source(entry: &Value) -> Option<String> {
    let payload = export_message_payload(entry);
    let candidates = [
        entry.get("extension"),
        entry.get("sourceExtension"),
        entry.get("source_extension"),
        payload.and_then(|value| value.get("extension")),
        payload.and_then(|value| value.get("sourceExtension")),
        entry
            .get("details")
            .and_then(|value| value.get("extension")),
        entry
            .get("details")
            .and_then(|value| value.get("sourceExtension")),
        entry.get("details").and_then(|value| value.get("source")),
        payload
            .and_then(|value| value.get("details"))
            .and_then(|value| value.get("extension")),
        payload
            .and_then(|value| value.get("details"))
            .and_then(|value| value.get("sourceExtension")),
        payload
            .and_then(|value| value.get("details"))
            .and_then(|value| value.get("source")),
        entry.get("data").and_then(|value| value.get("extension")),
        entry
            .get("data")
            .and_then(|value| value.get("sourceExtension")),
    ];
    candidates
        .into_iter()
        .flatten()
        .find_map(Value::as_str)
        .filter(|source| !source.trim().is_empty())
        .map(normalize_export_source)
}

fn export_tool_calls(entry: &Value) -> Vec<Value> {
    let Some(content) = export_message_payload(entry)
        .and_then(|message| message.get("content"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    content
        .iter()
        .filter_map(|block| {
            let call = block
                .get("ToolCall")
                .or_else(|| block.get("ToolUse"))
                .or_else(|| {
                    (block.get("type").and_then(Value::as_str) == Some("toolCall")).then_some(block)
                })?;
            Some(json!({
                "id": call.get("id").cloned().unwrap_or(Value::Null),
                "name": call.get("name").cloned().unwrap_or(json!("tool")),
                "arguments": call.get("arguments").cloned().unwrap_or_else(|| json!({})),
            }))
        })
        .collect()
}

fn annotate_export_entries(entries: &[Value]) -> Vec<Value> {
    let mut llm_call_number = 0_u64;
    entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let entry_type = entry.get("type").and_then(Value::as_str).unwrap_or("entry");
            let role = export_message_role(entry).unwrap_or("");
            let custom_type = export_custom_type(entry).unwrap_or("");
            let built_in_custom = built_in_custom_semantics(custom_type);
            let recorded_source = recorded_custom_source(entry);

            let is_custom_message = role.eq_ignore_ascii_case("custom");
            let is_tool_result = role.eq_ignore_ascii_case("toolresult")
                || role.eq_ignore_ascii_case("tool")
                || entry_type == "tool_result";
            let is_assistant = role.eq_ignore_ascii_case("assistant");
            let is_user = role.eq_ignore_ascii_case("user");
            let is_bash_execution = role.eq_ignore_ascii_case("bashExecution");
            let tool_calls = if is_assistant {
                export_tool_calls(entry)
            } else {
                Vec::new()
            };
            if is_assistant {
                llm_call_number += 1;
            }

            let (display_type, phase, actor) = if is_user {
                ("User", "user_input", "user")
            } else if is_assistant && !tool_calls.is_empty() {
                ("LLM Tool Request", "tool_request", "llm")
            } else if is_assistant {
                ("LLM Response", "llm_response", "llm")
            } else if is_tool_result {
                ("Tool Result", "tool_result", "tool")
            } else if is_bash_execution {
                ("Bash Execution", "tool_result", "tool")
            } else if !custom_type.is_empty() {
                let label = built_in_custom.map(|item| item.0).unwrap_or("Custom");
                let phase = if custom_type == "hook_event" || custom_type == "hook_handler_executed" {
                    "extension_event"
                } else if is_custom_message {
                    "context_injection"
                } else if custom_type == "system_prompt" {
                    "context_snapshot"
                } else {
                    "custom_event"
                };
                (label, phase, if is_custom_message { "extension" } else { "audit" })
            } else {
                let label = match entry_type {
                    "compaction" => "Compaction",
                    "branch_summary" => "Branch Summary",
                    "model_change" => "Model Change",
                    "thinking_level_change" => "Thinking Change",
                    "agent_change" => "Agent Change",
                    "active_tools_change" => "Tools Change",
                    "leaf_pointer" => "Branch Pointer",
                    "deletion" => "Deletion",
                    "segment_summary" => "Segment Summary",
                    "system_event" => "System Event",
                    _ => "Entry",
                };
                (label, "session_control", "kernel")
            };

            let (source_kind, source_name, source_confidence) = if let Some(source) = recorded_source {
                ("extension", source, "recorded")
            } else if let Some((_, extension)) = built_in_custom {
                let kind = if extension == "kernel" || extension == "session" {
                    "kernel"
                } else {
                    "extension"
                };
                (kind, extension.to_string(), "inferred")
            } else if is_assistant {
                let message = export_message_payload(entry).unwrap_or(&Value::Null);
                let provider = message.get("provider").and_then(Value::as_str).unwrap_or("llm");
                let model = message.get("model").and_then(Value::as_str).unwrap_or("unknown");
                ("llm", format!("{provider}/{model}"), "recorded")
            } else if is_tool_result || is_bash_execution {
                let message = export_message_payload(entry).unwrap_or(entry);
                let tool_name = message
                    .get("toolName")
                    .or_else(|| message.get("tool_name"))
                    .and_then(Value::as_str)
                    .unwrap_or(if is_bash_execution { "bash" } else { "tool" });
                ("tool", tool_name.to_string(), "recorded")
            } else if is_user {
                let source = export_message_payload(entry)
                    .and_then(|message| message.get("source"))
                    .and_then(Value::as_str)
                    .unwrap_or("prompt");
                ("user", source.to_string(), "recorded")
            } else if !custom_type.is_empty() {
                ("unknown", "unknown".into(), "unknown")
            } else {
                ("kernel", "ion".into(), "inferred")
            };

            let payload = export_message_payload(entry);
            let display = entry
                .get("display")
                .or_else(|| payload.and_then(|value| value.get("display")))
                .and_then(Value::as_bool)
                .unwrap_or(entry_type == "message");
            let excluded = payload
                .and_then(|value| {
                    value
                        .get("excludeFromContext")
                        .or_else(|| value.get("exclude_from_context"))
                })
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let llm_context = if excluded {
                "excluded"
            } else if is_assistant {
                "output"
            } else if entry_type == "message" {
                "input"
            } else {
                "not_in_context"
            };
            let filter_type = if !custom_type.is_empty() {
                if built_in_custom.is_some() {
                    format!("builtin:{display_type}")
                } else {
                    "custom".into()
                }
            } else if is_user {
                "user".into()
            } else if is_assistant {
                "assistant".into()
            } else if is_tool_result {
                "toolResult".into()
            } else if is_bash_execution {
                "bashExecution".into()
            } else {
                entry_type.into()
            };

            let mut meta = json!({
                "sequence": index + 1,
                "phase": phase,
                "displayType": display_type,
                "filterType": filter_type,
                "actor": actor,
                "source": {
                    "kind": source_kind,
                    "name": source_name,
                    "confidence": source_confidence,
                },
                "audience": {
                    "llmContext": llm_context,
                    "liveUi": display,
                    "audit": true,
                },
                "customType": if custom_type.is_empty() { Value::Null } else { json!(custom_type) },
                "customClass": if custom_type.is_empty() {
                    Value::Null
                } else if built_in_custom.is_some() {
                    json!("builtin")
                } else if source_confidence == "recorded" {
                    json!("extension")
                } else {
                    json!("custom")
                },
                "toolCalls": tool_calls,
            });
            if is_assistant {
                let message = export_message_payload(entry).unwrap_or(&Value::Null);
                if let Some(object) = meta.as_object_mut() {
                    object.insert("llmCall".into(), json!(llm_call_number));
                    object.insert(
                        "llm".into(),
                        json!({
                            "provider": message.get("provider").cloned().unwrap_or(Value::Null),
                            "model": message.get("model").cloned().unwrap_or(Value::Null),
                            "api": message.get("api").cloned().unwrap_or(Value::Null),
                            "stopReason": message.get("stopReason").or_else(|| message.get("stop_reason")).cloned().unwrap_or(Value::Null),
                            "usage": message.get("usage").cloned().unwrap_or(Value::Null),
                            "responseId": message.get("responseId").or_else(|| message.get("response_id")).cloned().unwrap_or(Value::Null),
                        }),
                    );
                }
            }

            let mut annotated = entry.clone();
            if let Some(object) = annotated.as_object_mut() {
                object.insert("ionMeta".into(), meta);
            }
            annotated
        })
        .collect()
}

fn build_export_flow_summary(entries: &[Value]) -> Value {
    let mut llm_calls = 0_u64;
    let mut tool_calls = 0_u64;
    let mut tool_results = 0_u64;
    let mut custom_entries = 0_u64;
    let mut context_injections = 0_u64;
    let mut extensions = std::collections::BTreeMap::<String, u64>::new();
    let mut custom_types = std::collections::BTreeMap::<String, (String, String, u64)>::new();
    let mut raw_entry_types = std::collections::BTreeMap::<String, u64>::new();
    let mut visible_types = std::collections::BTreeMap::<String, u64>::new();
    let mut message_roles = std::collections::BTreeMap::<String, u64>::new();
    let mut built_in_custom_types = std::collections::BTreeSet::<String>::new();
    let mut extension_custom_types = std::collections::BTreeSet::<String>::new();
    let mut unknown_custom_types = std::collections::BTreeSet::<String>::new();

    for entry in entries {
        let Some(meta) = entry.get("ionMeta") else {
            continue;
        };
        let raw_type = entry.get("type").and_then(Value::as_str).unwrap_or("entry");
        *raw_entry_types.entry(raw_type.to_string()).or_default() += 1;
        if let Some(filter_type) = meta.get("filterType").and_then(Value::as_str) {
            *visible_types.entry(filter_type.to_string()).or_default() += 1;
        }
        if let Some(role) = export_message_role(entry) {
            *message_roles.entry(role.to_string()).or_default() += 1;
        }
        if meta.get("llmCall").is_some() {
            llm_calls += 1;
        }
        tool_calls += meta
            .get("toolCalls")
            .and_then(Value::as_array)
            .map(|calls| calls.len() as u64)
            .unwrap_or(0);
        if meta.get("phase").and_then(Value::as_str) == Some("tool_result") {
            tool_results += 1;
        }
        if meta.get("phase").and_then(Value::as_str) == Some("context_injection") {
            context_injections += 1;
        }
        let source = meta.get("source");
        if source
            .and_then(|value| value.get("kind"))
            .and_then(Value::as_str)
            == Some("extension")
            && let Some(name) = source
                .and_then(|value| value.get("name"))
                .and_then(Value::as_str)
            && name != "unknown"
        {
            *extensions.entry(name.to_string()).or_default() += 1;
        }
        if let Some(custom_type) = meta.get("customType").and_then(Value::as_str) {
            custom_entries += 1;
            let display = meta
                .get("displayType")
                .and_then(Value::as_str)
                .unwrap_or("Custom")
                .to_string();
            let source_name = source
                .and_then(|value| value.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            let item =
                custom_types
                    .entry(custom_type.to_string())
                    .or_insert((display, source_name, 0));
            item.2 += 1;
            match meta.get("customClass").and_then(Value::as_str) {
                Some("builtin") => {
                    built_in_custom_types.insert(custom_type.to_string());
                }
                Some("extension") => {
                    extension_custom_types.insert(custom_type.to_string());
                }
                _ => {
                    unknown_custom_types.insert(custom_type.to_string());
                }
            }
        }
    }

    let current_custom_type_count = custom_types.len();
    let current_extension_count = extensions.len();
    let raw_entry_type_count = raw_entry_types.len();
    let visible_type_count = visible_types.len();
    let message_role_count = message_roles.len();
    let extension_items = extensions
        .into_iter()
        .map(|(name, count)| json!({"name":name,"count":count}))
        .collect::<Vec<_>>();
    let custom_type_items = custom_types
        .into_iter()
        .map(|(custom_type, (display, source, count))| {
            json!({
                "customType": custom_type,
                "displayType": display,
                "source": source,
                "count": count,
            })
        })
        .collect::<Vec<_>>();
    let raw_entry_type_items = raw_entry_types
        .into_iter()
        .map(|(name, count)| json!({"name":name,"count":count}))
        .collect::<Vec<_>>();
    let visible_type_items = visible_types
        .into_iter()
        .map(|(name, count)| json!({"name":name,"count":count}))
        .collect::<Vec<_>>();
    let message_role_items = message_roles
        .into_iter()
        .map(|(name, count)| json!({"name":name,"count":count}))
        .collect::<Vec<_>>();

    json!({
        "entries": entries.len(),
        "llmCalls": llm_calls,
        "toolCalls": tool_calls,
        "toolResults": tool_results,
        "customEntries": custom_entries,
        "contextInjections": context_injections,
        "extensions": extension_items,
        "customTypes": custom_type_items,
        "typeInventory": {
            "supported": {
                "entryTypes": session_jsonl::SESSION_ENTRY_TYPES.len(),
                "builtInCustomTypes": BUILT_IN_CUSTOM_TYPE_CATALOG.len(),
                "entryTypeNames": session_jsonl::SESSION_ENTRY_TYPES,
                "builtInCustomTypeNames": BUILT_IN_CUSTOM_TYPE_CATALOG
                    .iter()
                    .map(|(name, _, _)| *name)
                    .collect::<Vec<_>>(),
            },
            "current": {
                "rawEntryTypes": raw_entry_type_count,
                "visibleTypes": visible_type_count,
                "messageRoles": message_role_count,
                "customTypes": current_custom_type_count,
                "builtInCustomTypes": built_in_custom_types.len(),
                "extensionCustomTypes": extension_custom_types.len(),
                "unknownCustomTypes": unknown_custom_types.len(),
                "extensions": current_extension_count,
            },
            "entryTypes": raw_entry_type_items,
            "visibleTypes": visible_type_items,
            "messageRoles": message_role_items,
        },
    })
}

/// 保留字段是为了稳定离线 HTML data contract；当前格式没有隐藏的内部 entry。
fn partition_export_entries(entries: Vec<Value>) -> (Vec<Value>, Vec<Value>) {
    (entries, Vec::new())
}

/// Handles:
/// - `message`: unwrap the `Assistant`/`User`/`ToolResult` variant, flatten into pi's `{role, content}` form
/// - others: passed through (already match pi schema)
fn convert_entry(entry: &Value) -> Value {
    let entry_type = entry.get("type").and_then(|v| v.as_str()).unwrap_or("");
    match entry_type {
        "message" => convert_message_entry(entry),
        "tool_result" => convert_tool_result_entry(entry),
        // session_name → 转成 custom_message 让 pi 模板渲染（带 customType=session_name）
        "session_name" => {
            let name = entry.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let mut out = entry.clone();
            if let Some(obj) = out.as_object_mut() {
                obj.insert("type".into(), json!("custom_message"));
                obj.insert("customType".into(), json!("session_name"));
                obj.insert("content".into(), json!(format!("📝 Session title: {name}")));
                obj.insert("display".into(), json!(true));
            }
            out
        }
        _ => entry.clone(),
    }
}

/// Convert `message` entry: unwrap Rust enum variant, flatten to pi format.
fn convert_message_entry(entry: &Value) -> Value {
    let mut out = entry.clone();
    let msg_obj = match entry.get("message") {
        Some(m) if m.is_object() => m,
        _ => return out, // 没有 message 字段，原样返回
    };

    // ION: `{"Assistant": {...}}` / `{"User": {...}}` / `{"ToolResult": {...}}`
    // 找出 variant key（只取第一个 object key）
    let variant_key = msg_obj.as_object().and_then(|o| o.keys().next());
    let variant = match variant_key {
        Some(k) => k.clone(),
        None => return out, // 已经是扁平的（理论上不会，但容错）
    };

    let inner = match msg_obj.get(&variant) {
        Some(v) if v.is_object() => v,
        _ => return out,
    };

    let mut flat = inner.clone();

    // variant → role 兜底（若内部没显式 role）
    let role_for_variant = match variant.as_str() {
        "Assistant" => Some("assistant"),
        "User" => Some("user"),
        "ToolResult" => Some("toolResult"),
        // ★ Custom 变体（bash_result / dev_servers / diagnostics / session_name 等）
        // 之前没设 role → pi template 找不到 role 直接丢弃 → bash_result 完全消失。
        // 设 role="custom" + 保留 customType 让模板能按类型渲染（hook-message 卡片）。
        "Custom" => Some("custom"),
        _ => None,
    };
    if let Some(role) = role_for_variant {
        if let Some(o) = flat.as_object_mut() {
            o.entry("role").or_insert(json!(role));
        }
    }
    // ION ToolResult 存的是 role:"tool"，修正为 pi 的 role:"toolResult"
    if variant == "ToolResult"
        && let Some(obj) = flat.as_object_mut()
        && obj.get("role").and_then(|v| v.as_str()) == Some("tool")
    {
        obj.insert("role".to_string(), json!("toolResult"));
    }

    // ToolResult 字段 camelCase 化
    if variant == "ToolResult"
        && let Some(obj) = flat.as_object_mut()
    {
        rename_key(obj, "is_error", "isError");
        rename_key(obj, "tool_call_id", "toolCallId");
        rename_key(obj, "tool_name", "toolName");
    }

    // Assistant 字段 camelCase 化：stop_reason → stopReason, response_id → responseId, response_model → responseModel
    if variant == "Assistant"
        && let Some(obj) = flat.as_object_mut()
    {
        rename_key(obj, "stop_reason", "stopReason");
        rename_key(obj, "response_id", "responseId");
        rename_key(obj, "response_model", "responseModel");
        // usage 字段：cache_read → cacheRead, cache_write → cacheWrite, total_tokens → totalTokens
        if let Some(usage) = obj.get_mut("usage").and_then(|v| v.as_object_mut()) {
            rename_key(usage, "cache_read", "cacheRead");
            rename_key(usage, "cache_write", "cacheWrite");
            rename_key(usage, "total_tokens", "totalTokens");
        }
    }

    // content blocks: `{"Text":{"text":...}}` → `{"type":"text","text":...}`
    if let Some(content) = flat.get_mut("content") {
        *content = convert_content_blocks(content);
    }

    // pi template 的侧边栏会隐藏"只有 toolCall 没有 text"的 assistant message（filterNodes line 380），
    // 而且侧边栏只显示 text 不显示 toolCall（getTreeNodeDisplayHtml line 659）。
    // ION 的 skill 调用通常只有 toolCall 没 text → 侧边栏看不到 skill 调用。
    // 修复：给这种 message 注入一个描述性 text block（含工具名 + 参数），
    // 这样侧边栏能显示 "skill(context=fork, skill_name=code-audit)" 而不是空。
    if variant == "Assistant"
        && let Some(content) = flat.get("content").and_then(|v| v.as_array())
    {
        let has_text = content.iter().any(|c| {
            c.get("type").and_then(|v| v.as_str()) == Some("text")
                && c.get("text")
                    .and_then(|v| v.as_str())
                    .map(|s| !s.trim().is_empty())
                    .unwrap_or(false)
        });
        if !has_text {
            // 没有有意义的 text —— 从 toolCall 生成描述
            let mut descriptions: Vec<String> = Vec::new();
            for c in content {
                if c.get("type").and_then(|v| v.as_str()) == Some("toolCall") {
                    let name = c.get("name").and_then(|v| v.as_str()).unwrap_or("tool");
                    let args = c.get("arguments").cloned().unwrap_or(json!({}));
                    let args_str = if args.is_object() {
                        let obj = args.as_object().unwrap();
                        if obj.is_empty() {
                            String::new()
                        } else {
                            let pairs: Vec<String> = obj
                                .iter()
                                .map(|(k, v)| {
                                    let val_str = v
                                        .as_str()
                                        .map(|s| s.to_string())
                                        .unwrap_or_else(|| v.to_string());
                                    format!("{}={}", k, val_str)
                                })
                                .collect();
                            format!("({})", pairs.join(", "))
                        }
                    } else {
                        String::new()
                    };
                    descriptions.push(format!("{}{}", name, args_str));
                }
            }
            if !descriptions.is_empty() {
                let placeholder = descriptions.join("; ");
                if let Some(obj) = flat.as_object_mut()
                    && let Some(content) = obj.get_mut("content").and_then(|v| v.as_array_mut())
                {
                    content.insert(0, json!({"type": "text", "text": placeholder}));
                }
            }
        }
    }

    if let Some(o) = out.as_object_mut() {
        o.insert("message".to_string(), flat);
    }
    out
}

/// Convert ION ToolResult entry (when stored as standalone entry, not inside message).
/// ION 通常把 tool result 放在 message.ToolResult 里，但有些路径单独存为 entry。
fn convert_tool_result_entry(entry: &Value) -> Value {
    let mut out = entry.clone();
    if let Some(obj) = out.as_object_mut() {
        rename_key(obj, "is_error", "isError");
        rename_key(obj, "tool_call_id", "toolCallId");
        rename_key(obj, "tool_name", "toolName");
        if let Some(role) = obj.get("role").and_then(|v| v.as_str())
            && role == "tool"
        {
            obj.insert("role".to_string(), json!("toolResult"));
        }
        if let Some(content) = obj.get_mut("content") {
            *content = convert_content_blocks(content);
        }
    }
    out
}

/// Convert content blocks: ION enum-tagged → pi flat.
///
/// - `{Text:{text}}`              → `{"type":"text","text"}`
/// - `{Thinking:{thinking}}`      → `{"type":"thinking","thinking"}`
/// - `{ToolCall:{id,name,...}}`   → `{"type":"toolCall","id","name","arguments"}`
/// - `{Image:{data,mimeType}}`    → `{"type":"image","data","mimeType"}`
/// - `{ToolResult:{content,...}}` → `{"type":"toolResult",...}` (rare)
/// - string passthrough
/// - already-flat (has `type`) passthrough
fn convert_content_blocks(content: &Value) -> Value {
    match content {
        Value::String(_) => content.clone(),
        Value::Array(arr) => Value::Array(arr.iter().map(convert_content_block).collect()),
        _ => content.clone(),
    }
}

fn convert_content_block(block: &Value) -> Value {
    let obj = match block.as_object() {
        Some(o) => o,
        None => return block.clone(), // 不是 object，原样返回
    };
    // 已经是 pi 扁平格式（有 type 字段）
    if obj.contains_key("type") {
        return block.clone();
    }
    // 取 variant key（第一个 key）
    let variant_key = match obj.keys().next() {
        Some(k) => k.clone(),
        None => return block.clone(),
    };
    let inner = match obj.get(&variant_key) {
        Some(v) => v,
        None => return block.clone(),
    };

    match variant_key.as_str() {
        "Text" => {
            let mut out = inner.clone();
            if let Some(o) = out.as_object_mut() {
                o.insert("type".to_string(), json!("text"));
            }
            out
        }
        "Thinking" => {
            let mut out = inner.clone();
            if let Some(o) = out.as_object_mut() {
                o.insert("type".to_string(), json!("thinking"));
            }
            out
        }
        "ToolCall" => {
            let mut out = inner.clone();
            if let Some(o) = out.as_object_mut() {
                o.insert("type".to_string(), json!("toolCall"));
                // ION 字段已是 id/name/arguments，不需要 rename
            }
            out
        }
        "Image" => {
            let mut out = inner.clone();
            if let Some(o) = out.as_object_mut() {
                o.insert("type".to_string(), json!("image"));
                rename_key(o, "mime_type", "mimeType");
            }
            out
        }
        "ToolResult" => {
            let mut out = inner.clone();
            if let Some(o) = out.as_object_mut() {
                o.insert("type".to_string(), json!("toolResult"));
                rename_key(o, "is_error", "isError");
                rename_key(o, "tool_call_id", "toolCallId");
                rename_key(o, "tool_name", "toolName");
                if let Some(content) = o.get_mut("content") {
                    *content = convert_content_blocks(content);
                }
            }
            out
        }
        _ => block.clone(), // 未知 variant，原样返回
    }
}

/// Rename a key in a JSON object (if present).
fn rename_key(obj: &mut serde_json::Map<String, Value>, from: &str, to: &str) {
    if let Some(val) = obj.remove(from) {
        obj.insert(to.to_string(), val);
    }
}

/// Resolve a session file path, trying multiple strategies.
fn resolve_session_file(
    session_id: &str,
) -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    // Strategy 1: Look up session in global index → get cwd → use cwd path
    let index = crate::session_index::SessionIndex::load();
    if let Some(meta) = index.get(session_id)
        && let Some(ref project) = meta.project
    {
        let cwd_path = crate::session_jsonl::session_path(project);
        if cwd_path.exists() {
            // Verify the session file contains this session
            if let Some(file) = crate::session_jsonl::SessionFile::load(project)
                && file.header.id == session_id
            {
                return Ok(cwd_path);
            }
        }
    }

    // Strategy 2: Legacy flat format: sessions/{id}.jsonl
    let legacy_path = crate::paths::sessions_dir().join(format!("{session_id}.jsonl"));
    if legacy_path.exists() {
        return Ok(legacy_path);
    }

    // Strategy 2b: Per-session files: sessions/--hash--name--/{session_id}.jsonl
    // cmd_run 每次运行创建独立 <sid>.jsonl 文件，header 可能不含标准 session 类型
    // 但文件名就是 session_id，直接按文件名匹配。
    let sessions_root = crate::paths::sessions_dir();
    if sessions_root.exists()
        && let Ok(entries) = std::fs::read_dir(&sessions_root)
    {
        for entry in entries.flatten() {
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            let per_session_file = dir.join(format!("{session_id}.jsonl"));
            if per_session_file.exists() {
                return Ok(per_session_file);
            }
        }
    }

    // Strategy 3: Treat session_id as a cwd path (encoded)
    let cwd_path = crate::session_jsonl::session_path(session_id);
    if cwd_path.exists() {
        return Ok(cwd_path);
    }

    // Strategy 4: Scan all session.jsonl AND <session_id>.jsonl files for matching header id.
    // - session.jsonl: 主 Worker 的会话文件
    // - <session_id>.jsonl: fork 子 Worker 的独立会话文件（ION_FORK_CHILD 标记）
    // This handles cases where the index is stale or the session was created
    // in a worktree/temp cwd that wasn't tracked in the global index.
    let sessions_root = crate::paths::sessions_dir();
    if sessions_root.exists()
        && let Ok(entries) = std::fs::read_dir(&sessions_root)
    {
        for entry in entries.flatten() {
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            // 扫目录下所有 .jsonl 文件（session.jsonl + <sid>.jsonl）
            if let Ok(files) = std::fs::read_dir(&dir) {
                for file in files.flatten() {
                    let path = file.path();
                    let name = match path.file_name().and_then(|n| n.to_str()) {
                        Some(n) => n,
                        None => continue,
                    };
                    if !name.ends_with(".jsonl") {
                        continue;
                    }
                    // Read only the first line (header) to check id
                    if let Ok(header_line) = std::fs::read_to_string(&path)
                        && let Some(first_line) = header_line.lines().next()
                        && let Ok(header) = serde_json::from_str::<Value>(first_line)
                        && header.get("id").and_then(|v| v.as_str()) == Some(session_id)
                    {
                        return Ok(path);
                    }
                }
            }
        }
    }

    Err(format!(
        "session file not found for id '{}' (tried index, flat, cwd path, and directory scan)",
        session_id
    )
    .into())
}

/// Escape text inserted into the export shell itself (outside the base64 session payload).
fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

fn escape_html_text(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn base64_encode(input: &str) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = input.as_bytes();
    let mut result = String::new();

    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;

        result.push(CHARS[((triple >> 18) & 0x3F) as usize] as char);
        result.push(CHARS[((triple >> 12) & 0x3F) as usize] as char);
        result.push(if chunk.len() > 1 {
            CHARS[((triple >> 6) & 0x3F) as usize] as char
        } else {
            '='
        });
        result.push(if chunk.len() > 2 {
            CHARS[(triple & 0x3F) as usize] as char
        } else {
            '='
        });
    }

    result
}

/// 构建环境信息（export 用，用 session header 的 cwd 跑 git 命令）。
/// 跟 bin/ion.rs 的 build_env_info 同逻辑，但独立实现（lib 看不到 bin 的函数）。
fn build_env_info_for_export(cwd: &str) -> String {
    use std::process::Command;
    let now_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = now_epoch / 86400;
    let remain = now_epoch % 86400;
    let now_human = format!(
        "day {} ({}:{:02} UTC)",
        days,
        remain / 3600,
        (remain % 3600) / 60
    );

    let project_root = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| cwd.to_string());
    let git_branch = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(cwd)
        .output()
        .ok()
        .and_then(|o| {
            let b = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if b.is_empty() { None } else { Some(b) }
        });
    let git_remote = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(cwd)
        .output()
        .ok()
        .and_then(|o| {
            let r = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if r.is_empty() { None } else { Some(r) }
        });
    let worktree = std::env::var("ION_WORKTREE_ROOT")
        .ok()
        .or_else(|| std::env::var("ION_WORKTREE").ok());

    let mut info = String::from("\n\n--- environment ---\n## Environment\n");
    info.push_str(&format!(
        "- **Current Time**: {} (unix: {})\n",
        now_human, now_epoch
    ));
    info.push_str(&format!("- **Working Directory (cwd)**: `{}`\n", cwd));
    info.push_str(&format!("- **Project Root**: `{}`\n", project_root));
    if let Some(wt) = &worktree {
        info.push_str(&format!("- **Worktree Path**: `{}`\n", wt));
    }
    info.push_str(&format!(
        "- **Platform**: `{} {}`\n",
        std::env::consts::OS,
        std::env::consts::ARCH
    ));
    info.push_str(&format!(
        "- **ION Version**: `{}`\n",
        env!("CARGO_PKG_VERSION")
    ));
    if let Some(b) = &git_branch {
        info.push_str(&format!("- **Git Branch**: `{}`\n", b));
    }
    if let Some(r) = &git_remote {
        info.push_str(&format!("- **Git Remote**: `{}`\n", r));
    }

    // 最近 5 个 commit 主题
    if let Ok(o) = Command::new("git")
        .args(["log", "--oneline", "-5"])
        .current_dir(cwd)
        .output()
        && let Ok(s) = String::from_utf8(o.stdout)
    {
        let s = s.trim();
        if !s.is_empty() {
            info.push_str("\n### Recent Commits (last 5)\n```\n");
            info.push_str(s);
            info.push_str("\n```\n");
        }
    }
    // 最近修改文件（HEAD~1..HEAD + 未提交，前 20）
    let mut recent_files: Vec<String> = Vec::new();
    if let Ok(o) = Command::new("git")
        .args(["diff", "--name-only", "HEAD~1", "HEAD"])
        .current_dir(cwd)
        .output()
        && let Ok(s) = String::from_utf8(o.stdout)
    {
        for line in s.lines() {
            let f = line.trim();
            if !f.is_empty() && !recent_files.contains(&f.to_string()) {
                recent_files.push(f.to_string());
            }
        }
    }
    if let Ok(o) = Command::new("git")
        .args(["status", "--short"])
        .current_dir(cwd)
        .output()
        && let Ok(s) = String::from_utf8(o.stdout)
    {
        let s = s.trim();
        if !s.is_empty() {
            info.push_str("\n### Uncommitted Changes\n```\n");
            info.push_str(s);
            info.push_str("\n```\n");
            for line in s.lines() {
                let f = line
                    .trim_start_matches(|c: char| c.is_uppercase() || c == ' ' || c == '?')
                    .trim();
                if !f.is_empty() && !recent_files.contains(&f.to_string()) {
                    recent_files.push(f.to_string());
                }
            }
        }
    }
    if !recent_files.is_empty() {
        let trunc = if recent_files.len() > 20 {
            format!("\n  (and {} more...)", recent_files.len() - 20)
        } else {
            String::new()
        };
        let list = recent_files
            .iter()
            .take(20)
            .map(|f| format!("  - {}", f))
            .collect::<Vec<_>>()
            .join("\n");
        info.push_str(&format!(
            "\n### Recently Modified Files\n{}\n{}\n",
            list, trunc
        ));
    }
    info
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_export_selects_current_branch_and_keeps_branch_record() {
        let entries = vec![
            json!({"type":"message","id":"m1","parentId":"session-1","message":{"role":"user","content":"root"}}),
            json!({"type":"message","id":"m2","parentId":"m1","message":{"role":"assistant","content":[]}}),
            json!({"type":"message","id":"old-3","parentId":"m2","message":{"role":"user","content":"old branch"}}),
            json!({"type":"message","id":"old-4","parentId":"old-3","message":{"role":"assistant","content":[]}}),
            json!({"type":"leaf_pointer","id":"lp-1","parentId":null,"leafId":"m2"}),
            json!({"type":"message","id":"m5","parentId":"m2","message":{"role":"user","content":"active branch"}}),
            json!({"type":"custom","id":"snap-1","parentId":"m5","customType":"step-snapshot","data":{"baselineTreeHash":"t0","snapshotTreeHash":"t1","diff":{"added":[],"modified":[],"deleted":[]},"turnIndex":1}}),
            json!({"type":"custom_message","id":"hook-1","parentId":null,"customType":"hook_event","content":"active hook"}),
            json!({"type":"custom_message","id":"old-note","parentId":"old-4","customType":"diagnostics","content":"old branch detail"}),
            json!({"type":"branch_summary","id":"bs-1","parentId":"old-4","fromId":"old-4","summary":"abandoned branch"}),
        ];

        let selection = select_entries_for_export(&entries, "session-1");
        let ids: Vec<&str> = selection
            .entries
            .iter()
            .filter_map(|entry| entry.get("id").and_then(|id| id.as_str()))
            .collect();

        assert_eq!(selection.active_leaf_id.as_deref(), Some("m5"));
        assert_eq!(selection.source_entry_count, 10);
        assert_eq!(selection.omitted_branch_entry_count, 3);
        assert_eq!(
            ids,
            vec!["m1", "m2", "lp-1", "m5", "snap-1", "hook-1", "bs-1"]
        );
        assert!(!ids.contains(&"old-3"));
        assert!(!ids.contains(&"old-4"));
        assert!(!ids.contains(&"old-note"));
    }

    #[test]
    fn test_export_keeps_full_linear_session() {
        let entries = vec![
            json!({"type":"message","id":"m1","parentId":"session-1","message":{"role":"user","content":"hello"}}),
            json!({"type":"custom","id":"snap-1","parentId":"m1","customType":"step-snapshot","data":{"baselineTreeHash":"t0","snapshotTreeHash":"t1"}}),
            json!({"type":"message","id":"m2","parentId":"m1","message":{"role":"assistant","content":[]}}),
        ];

        let selection = select_entries_for_export(&entries, "session-1");
        assert_eq!(selection.entries, entries);
        assert_eq!(selection.omitted_branch_entry_count, 0);
    }

    #[test]
    fn test_escape_html_text_for_export_shell() {
        assert_eq!(
            escape_html_text(r#"<session name='one'>& "two""#),
            "&lt;session name=&#39;one&#39;&gt;&amp; &quot;two&quot;"
        );
    }

    #[test]
    fn test_convert_text_content_block() {
        let ion = json!({"Text": {"text": "hello"}});
        let pi = convert_content_block(&ion);
        assert_eq!(pi, json!({"type": "text", "text": "hello"}));
    }

    #[test]
    fn test_convert_tool_call_block() {
        let ion = json!({"ToolCall": {"id": "tc1", "name": "bash", "arguments": {"cmd": "ls"}}});
        let pi = convert_content_block(&ion);
        assert_eq!(
            pi,
            json!({"type": "toolCall", "id": "tc1", "name": "bash", "arguments": {"cmd": "ls"}})
        );
    }

    #[test]
    fn test_already_flat_passthrough() {
        let flat = json!({"type": "text", "text": "already flat"});
        assert_eq!(convert_content_block(&flat), flat);
    }

    #[test]
    fn test_convert_assistant_message() {
        let entry = json!({
            "type": "message",
            "id": "e1",
            "parentId": null,
            "timestamp": "2026-01-01T00:00:00Z",
            "message": {
                "Assistant": {
                    "role": "assistant",
                    "content": [{"Text": {"text": "hello"}}],
                    "stop_reason": "stop",
                    "usage": {"cache_read": 10, "total_tokens": 100}
                }
            }
        });
        let pi = convert_entry(&entry);
        let msg = pi.get("message").unwrap();
        // Flattened: no "Assistant" wrapper
        assert!(msg.get("Assistant").is_none());
        assert_eq!(msg.get("role").unwrap(), &json!("assistant"));
        assert_eq!(
            msg.get("content").unwrap(),
            &json!([{"type": "text", "text": "hello"}])
        );
        assert_eq!(msg.get("stopReason").unwrap(), &json!("stop"));
        assert_eq!(
            msg.get("usage").unwrap().get("cacheRead").unwrap(),
            &json!(10)
        );
    }

    #[test]
    fn test_convert_tool_result_message() {
        let entry = json!({
            "type": "message",
            "id": "e1",
            "parentId": null,
            "timestamp": "2026-01-01T00:00:00Z",
            "message": {
                "ToolResult": {
                    "role": "tool",
                    "content": [{"Text": {"text": "result"}}],
                    "is_error": false,
                    "tool_call_id": "tc1",
                    "tool_name": "bash"
                }
            }
        });
        let pi = convert_entry(&entry);
        let msg = pi.get("message").unwrap();
        assert_eq!(msg.get("role").unwrap(), &json!("toolResult"));
        assert_eq!(msg.get("isError").unwrap(), &json!(false));
        assert_eq!(msg.get("toolCallId").unwrap(), &json!("tc1"));
        assert_eq!(msg.get("toolName").unwrap(), &json!("bash"));
        assert!(msg.get("is_error").is_none());
    }

    // ── Custom variant conversion tests ──
    // Covers commit e388e0d: Custom variant must be flattened to type=custom_message
    // so pi template's renderEntry (which checks entry.type === 'custom_message')
    // actually renders bash_result / dev_servers / diagnostics as hook-message cards.
    // Without this, all Custom messages were silently dropped from exported HTML.

    #[test]
    fn test_convert_custom_variant_sets_role_custom() {
        // Custom variant 必须设 role="custom"，否则 pi template 不识别。
        let entry = json!({
            "type": "message",
            "id": "e1",
            "parentId": null,
            "timestamp": "2026-01-01T00:00:00Z",
            "message": {
                "Custom": {
                    "role": "custom",
                    "customType": "bash_result",
                    "content": "<bash_result bid=\"100000\" exit=\"0\">output</bash_result>",
                    "display": true
                }
            }
        });
        let pi = convert_entry(&entry);
        let msg = pi.get("message").unwrap();
        // role 应该是 "custom"（之前漏了这个分支，导致 role 为 undefined）
        assert_eq!(msg.get("role").unwrap(), &json!("custom"));
        assert!(
            msg.get("Custom").is_none(),
            "variant wrapper should be flattened"
        );
    }

    #[test]
    fn test_convert_custom_bash_result_preserves_content() {
        // bash_result content 不能丢——LLM/用户靠它看输出。
        let entry = json!({
            "type": "message",
            "id": "e1",
            "parentId": null,
            "timestamp": "2026-01-01T00:00:00Z",
            "message": {
                "Custom": {
                    "role": "custom",
                    "customType": "bash_result",
                    "content": "Traceback ... OSError: Address already in use",
                    "display": true
                }
            }
        });
        let pi = convert_entry(&entry);
        let msg = pi.get("message").unwrap();
        assert_eq!(
            msg.get("content").unwrap(),
            &json!("Traceback ... OSError: Address already in use")
        );
        assert_eq!(msg.get("customType").unwrap(), &json!("bash_result"));
        assert_eq!(msg.get("display").unwrap(), &json!(true));
    }

    #[test]
    fn test_convert_custom_dev_servers() {
        // dev_servers XML 注入也是 Custom 变体
        let entry = json!({
            "type": "message",
            "id": "e1",
            "parentId": null,
            "timestamp": "2026-01-01T00:00:00Z",
            "message": {
                "Custom": {
                    "role": "custom",
                    "customType": "dev_servers",
                    "content": "<dev_servers><server port=\"8765\"/></dev_servers>",
                    "display": true
                }
            }
        });
        let pi = convert_entry(&entry);
        let msg = pi.get("message").unwrap();
        assert_eq!(msg.get("role").unwrap(), &json!("custom"));
        assert_eq!(msg.get("customType").unwrap(), &json!("dev_servers"));
        assert!(
            msg.get("content")
                .unwrap()
                .as_str()
                .unwrap()
                .contains("8765")
        );
    }

    #[test]
    fn test_convert_custom_diagnostics() {
        // diagnostics 含 error/warning 分类信息
        let entry = json!({
            "type": "message",
            "id": "e1",
            "parentId": null,
            "timestamp": "2026-01-01T00:00:00Z",
            "message": {
                "Custom": {
                    "role": "custom",
                    "customType": "diagnostics",
                    "content": "<diagnostics count=\"2\" has_errors=\"true\">...</diagnostics>",
                    "display": true
                }
            }
        });
        let pi = convert_entry(&entry);
        let msg = pi.get("message").unwrap();
        assert_eq!(msg.get("customType").unwrap(), &json!("diagnostics"));
        assert!(
            msg.get("content")
                .unwrap()
                .as_str()
                .unwrap()
                .contains("has_errors")
        );
    }

    #[test]
    fn test_step_snapshot_is_visible_export_flow_entry() {
        let step_snapshot = json!({
            "type": "custom",
            "id": "snap1",
            "parentId": "m1",
            "customType": "step-snapshot",
            "data": {
                "baselineTreeHash": "tree0",
                "snapshotTreeHash": "tree1",
                "diff": {"added":["a.rs"],"modified":[],"deleted":[]},
                "turnIndex": 1
            }
        });
        let message = json!({"type": "message", "id": "m1"});
        let compaction = json!({"type": "compaction", "id": "c1"});

        let (visible, internal) = partition_export_entries(vec![
            message.clone(),
            step_snapshot.clone(),
            compaction.clone(),
        ]);

        assert_eq!(visible, vec![message, step_snapshot, compaction]);
        assert!(internal.is_empty());
    }

    #[test]
    fn test_convert_user_message_with_role_fallback() {
        // ION: User variant 但 inner 没 role 字段（少见但可能）
        let entry = json!({
            "type": "message",
            "id": "e1",
            "parentId": null,
            "timestamp": "2026-01-01T00:00:00Z",
            "message": {
                "User": {
                    "content": [{"Text": {"text": "hi"}}],
                    "timestamp": 123
                }
            }
        });
        let pi = convert_entry(&entry);
        let msg = pi.get("message").unwrap();
        assert_eq!(msg.get("role").unwrap(), &json!("user"));
    }

    #[test]
    fn test_non_message_entry_passthrough() {
        let entry = json!({
            "type": "model_change",
            "id": "m1",
            "parentId": null,
            "timestamp": "2026-01-01T00:00:00Z",
            "provider": "openai",
            "modelId": "gpt-4"
        });
        let pi = convert_entry(&entry);
        assert_eq!(pi, entry);
    }

    #[test]
    fn test_string_content_passthrough() {
        let content = json!("plain string content");
        assert_eq!(convert_content_blocks(&content), content);
    }

    #[test]
    fn test_rename_key_no_op_when_absent() {
        let mut obj = serde_json::Map::new();
        obj.insert("foo".to_string(), json!(1));
        rename_key(&mut obj, "bar", "baz");
        assert!(obj.get("foo").is_some());
        assert!(obj.get("baz").is_none());
    }

    #[test]
    fn test_convert_message_with_flat_content_no_message() {
        // Edge case: entry.type==message but no "message" field
        let entry = json!({
            "type": "message",
            "id": "e1",
            "parentId": null,
            "timestamp": "2026-01-01T00:00:00Z"
        });
        let pi = convert_entry(&entry);
        assert_eq!(pi, entry);
    }

    #[test]
    fn test_export_semantics_marks_llm_custom_injection_with_recorded_source() {
        let entries = annotate_export_entries(&[json!({
            "type": "message",
            "id": "custom-1",
            "message": {"Custom": {
                "role": "custom",
                "customType": "diagnostics",
                "content": "<diagnostics />",
                "display": true,
                "details": {"source": "lsp"}
            }}
        })]);
        let meta = &entries[0]["ionMeta"];
        assert_eq!(meta["displayType"], json!("Diagnostics"));
        assert_eq!(meta["phase"], json!("context_injection"));
        assert_eq!(meta["source"]["name"], json!("lsp"));
        assert_eq!(meta["source"]["confidence"], json!("recorded"));
        assert_eq!(meta["audience"]["llmContext"], json!("input"));
        assert_eq!(meta["audience"]["liveUi"], json!(true));
    }

    #[test]
    fn test_export_semantics_marks_hook_as_audit_not_llm_context() {
        let entries = annotate_export_entries(&[json!({
            "type": "custom_message",
            "id": "hook-1",
            "customType": "hook_event",
            "content": "blocked",
            "display": true,
            "details": {"source": "hook", "toolCallId": "call-1"}
        })]);
        let meta = &entries[0]["ionMeta"];
        assert_eq!(meta["displayType"], json!("Hook"));
        assert_eq!(meta["phase"], json!("extension_event"));
        assert_eq!(meta["source"]["name"], json!("hooks"));
        assert_eq!(meta["source"]["confidence"], json!("recorded"));
        assert_eq!(meta["audience"]["llmContext"], json!("not_in_context"));
    }

    #[test]
    fn test_runtime_extension_custom_keeps_generic_label_and_exact_origin() {
        let entries = annotate_export_entries(&[json!({
            "type": "custom_message",
            "id": "weather-1",
            "customType": "forecast_refreshed",
            "extension": "weather",
            "content": "sunny",
            "display": true
        })]);
        let meta = &entries[0]["ionMeta"];
        assert_eq!(meta["displayType"], json!("Custom"));
        assert_eq!(meta["customClass"], json!("extension"));
        assert_eq!(meta["source"]["name"], json!("weather"));
        assert_eq!(meta["source"]["confidence"], json!("recorded"));
    }

    #[test]
    fn test_export_flow_summary_counts_llm_tools_custom_and_extensions() {
        let entries = annotate_export_entries(&[
            json!({"type":"message","id":"u1","message":{"User":{"role":"user","content":[],"source":"prompt"}}}),
            json!({"type":"message","id":"a1","message":{"Assistant":{
                "role":"assistant","provider":"zai","model":"glm-5.2","api":"openai-completions",
                "usage":{"input":10,"output":4},"stop_reason":"tool_use",
                "content":[{"ToolCall":{"id":"call-1","name":"bash","arguments":{"command":"pwd"}}}]
            }}}),
            json!({"type":"message","id":"t1","message":{"ToolResult":{"role":"toolResult","tool_call_id":"call-1","tool_name":"bash","content":[]}}}),
            json!({"type":"message","id":"c1","message":{"Custom":{"role":"custom","customType":"diagnostics","content":"x","display":true,"details":{"source":"lsp"}}}}),
        ]);
        let summary = build_export_flow_summary(&entries);
        assert_eq!(summary["entries"], json!(4));
        assert_eq!(summary["llmCalls"], json!(1));
        assert_eq!(summary["toolCalls"], json!(1));
        assert_eq!(summary["toolResults"], json!(1));
        assert_eq!(summary["customEntries"], json!(1));
        assert_eq!(summary["contextInjections"], json!(1));
        assert_eq!(summary["extensions"][0]["name"], json!("lsp"));
        assert_eq!(
            summary["typeInventory"]["supported"]["entryTypes"],
            json!(17)
        );
        assert_eq!(
            summary["typeInventory"]["supported"]["builtInCustomTypes"],
            json!(25)
        );
        assert_eq!(
            summary["typeInventory"]["current"]["rawEntryTypes"],
            json!(1)
        );
        assert_eq!(
            summary["typeInventory"]["current"]["visibleTypes"],
            json!(4)
        );
        assert_eq!(
            summary["typeInventory"]["current"]["messageRoles"],
            json!(4)
        );
        assert_eq!(summary["typeInventory"]["current"]["customTypes"], json!(1));
        assert_eq!(
            summary["typeInventory"]["current"]["builtInCustomTypes"],
            json!(1)
        );
        assert_eq!(
            summary["typeInventory"]["current"]["extensionCustomTypes"],
            json!(0)
        );
        assert_eq!(summary["typeInventory"]["current"]["extensions"], json!(1));
    }

    #[test]
    fn test_export_assets_are_embedded_and_markdown_escapes_raw_html() {
        assert!(EXPORT_TEMPLATE_HTML.contains("{{SESSION_DATA}}"));
        assert!(EXPORT_TEMPLATE_CSS.len() > 10_000);
        assert!(EXPORT_TEMPLATE_JS.contains("function safeMarkedParse"));
        assert!(EXPORT_TEMPLATE_JS.contains(".replace(/</g, '&lt;')"));
        assert!(!EXPORT_TEMPLATE_JS.contains("PI_EXPORT_DIR"));
        assert!(!EXPORT_MARKED_JS.is_empty());
        assert!(!EXPORT_HIGHLIGHT_JS.is_empty());
    }
}
