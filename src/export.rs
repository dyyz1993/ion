//! Export a session to HTML using pi's template system.
//!
//! 引用: /Users/xuyingzhou/Project/temporary/pi-momo-fork/packages/coding-agent/src/core/export-html/
//!
//! ## ION vs pi 格式差异
//!
//! ION 存的是 Rust enum 序列化形式（externally tagged），pi 期望扁平形式：
//!
//! ION: `{"message": {"Assistant": {"role":"assistant", "content":[{"Text":{"text":"..."}}]}}}`
//! pi:  `{"message": {"role":"assistant",   "content":[{"type":"text", "text":"..."}]}}`
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
//! turn_summary（ION 原生）→ custom_message（pi 可识别），让侧边栏树展示有内容。

use serde_json::{json, Value};
use std::path::Path;

/// Paths to pi's export template files
const PI_EXPORT_DIR: &str =
    "/Users/xuyingzhou/Project/temporary/pi-momo-fork/packages/coding-agent/src/core/export-html";

/// Tool info for export (matches pi's ToolDefinition shape: name/description/parameters).
#[derive(Clone, Debug, serde::Serialize)]
pub struct ExportToolInfo {
    pub name: String,
    pub description: String,
    pub parameters: Value,
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
    let first_line = content.lines().next().unwrap_or("{}");
    let header: Value = serde_json::from_str(first_line)?;

    // Extract agent name from header
    let agent_name = header.get("agent").and_then(|v| v.as_str());

    // Load agent config to get tools + system prompt
    let mut tools: Option<Vec<ExportToolInfo>> = None;
    let mut system_prompt: Option<String> = None;

    if let Some(name) = agent_name {
        if let Some(agent_cfg) = crate::agent_config::find_agent(name) {
            // Get system prompt from agent config
            system_prompt = agent_cfg.system_prompt.clone();

            // Reconstruct tool definitions by instantiating all built-in tools,
            // then applying the agent config's allowlist and blocklist.
            let mut registry = crate::agent::tool::ToolRegistry::new();
            registry.register_builtins();

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

            // Convert to ExportToolInfo list
            let defs: Vec<ExportToolInfo> = registry
                .tool_defs()
                .into_iter()
                .map(|td| ExportToolInfo {
                    name: td.name,
                    description: td.description,
                    parameters: td.parameters,
                })
                .collect();

            if !defs.is_empty() {
                tools = Some(defs);
            }
        }
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

    // Split: first line is header, rest are entries
    let header: Value = serde_json::from_str(lines[0])?;
    let mut raw_entries: Vec<Value> = lines[1..]
        .iter()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();

    // ── 合并 fork 子 session 的 entries ──
    // 扫同目录下的 <sid>.jsonl 文件，找 parentSession == 当前 session_id 的，
    // 把它们的 entries 用 system_event 分隔标记追加进来。
    // 这样用户在一个 HTML 里能看到"主 Worker 调 skill fork → 子 Worker 干了什么"。
    // 子 session 只在 export 主 session 时合并（避免循环）。
    let session_type = header.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let is_main_session = header.get("parentSession").and_then(|v| v.as_str()).is_none()
        && !header.get("spawnMeta").is_some();
    if is_main_session {
        if let Some(parent_dir) = jsonl_path.parent() {
            if let Ok(files) = std::fs::read_dir(parent_dir) {
                for file in files.flatten() {
                    let path = file.path();
                    let name = match path.file_name().and_then(|n| n.to_str()) {
                        Some(n) => n,
                        None => continue,
                    };
                    // 只扫 <sid>.jsonl，跳过 session.jsonl（自己）+ memory_agent + input
                    if name == "session.jsonl" || name.starts_with("sess_memory_agent") || name == "input.jsonl" {
                        continue;
                    }
                    if !name.ends_with(".jsonl") { continue; }

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
                    let parent_match = sub_header
                        .get("parentSession")
                        .and_then(|v| v.as_str())
                        == Some(session_id);
                    if !parent_match { continue; }

                    // 匹配！自动导出子 session HTML + 在主 HTML 里放可点击链接
                    let sub_sid = sub_header.get("id").and_then(|v| v.as_str()).unwrap_or("?").to_string();
                    let spawn_meta = sub_header.get("spawnMeta").cloned();
                    let spawned_by = spawn_meta
                        .as_ref()
                        .and_then(|m| m.get("spawnedBy"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");

                    // 自动导出子 session HTML（跟主 HTML 同目录，文件名 fork_<sid>.html）
                    let sub_html_name = format!("fork_{}.html", &sub_sid[..12.min(sub_sid.len())]);
                    let sub_html_path = output_path.parent()
                        .map(|p| p.join(&sub_html_name))
                        .unwrap_or_else(|| std::path::PathBuf::from(&sub_html_name));

                    // 递归导出子 session（但不深度递归——只导一层 fork 子 session）
                    match export_session_internal(&sub_sid, &sub_html_path, None, None) {
                        Ok(()) => {
                            eprintln!("[export] auto-exported fork sub-session → {}", sub_html_path.display());
                        }
                        Err(e) => {
                            eprintln!("[export] WARN: failed to auto-export fork sub-session {sub_sid}: {e}");
                        }
                    }

                    // 分隔标记：content 里有可点击的 HTML 链接（指向子 HTML 文件）
                    let sub_sid_short = &sub_sid[..12.min(sub_sid.len())];
                    let separator_content = format!(
                        "🔗 Fork 子 Worker session（{sub_sid_short}）\n\
                         spawnedBy: {spawned_by}\n\
                         子 session ID: {sub_sid}\n\n\
                         👆 点击查看完整 fork 子 Worker 执行过程：fork_{sub_sid_short}.html\n\n\
                         （或命令行导出：ion --export sub.html --session {sub_sid}）"
                    );
                    let separator = json!({
                        "type": "custom_message",
                        "id": format!("fork-sep-{}", sub_sid),
                        "parentId": null,
                        "timestamp": sub_header.get("timestamp").cloned().unwrap_or(json!("")),
                        "customType": "fork_separator",
                        "content": separator_content,
                        "data": { "subSessionId": sub_sid, "spawnedBy": spawned_by },
                        "display": true,
                    });
                    raw_entries.push(separator);

                    // 不追加子 session 的 entries——fork 是独立进程，应该有独立的 session.jsonl
                    // 和独立的 HTML。主 HTML 只显示主 Worker 的对话流程。
                    // 用户可以用 subSessionId 单独 export 子 session：
                    //   ion --export sub.html --session <subSessionId>
                }
            }
        }
    }

    // Convert ION Rust-enum format → pi flat format.
    // 排除以下 entries（它们是内部记录，不是真正的对话内容）：
    // - system_prompt custom entry（已提取到顶层 systemPrompt 字段）
    // - turn_summary custom_message（ION 内部的 turn 摘要，对用户没意义，
    //   会污染主体内容的"入参→响应值"流程）
    let mut entries: Vec<Value> = raw_entries
        .iter()
        .filter(|e| {
            // 过滤掉 system_prompt custom entry
            if e.get("type").and_then(|v| v.as_str()) == Some("custom")
                && e.get("customType").and_then(|v| v.as_str()) == Some("system_prompt")
            {
                return false;
            }
            // 过滤掉 turn_summary（原始类型）
            if e.get("type").and_then(|v| v.as_str()) == Some("turn_summary") {
                return false;
            }
            // 过滤掉转换后的 turn_summary custom_message
            if e.get("type").and_then(|v| v.as_str()) == Some("custom_message")
                && e.get("customType").and_then(|v| v.as_str()) == Some("turn_summary")
            {
                return false;
            }
            true
        })
        .map(convert_entry)
        .collect();

    // 重建 parentId 链：让所有 entries 串成一条线。
    // pi template 的主体内容只显示 getPath(leafId) 返回的 parentId 链上的 entries。
    // ION 的 session 可能有多条 parentId 链（turn_summary 的 parentId=None 断链、
    // 增量 save 的 parentId 不连续），导致部分 entries 不在路径上 → 主体内容看不到。
    // 修复：按原始顺序，每个 entry 的 parentId 指向前一个 entry 的 id，
    // 这样 getPath(leafId) 能返回所有 entries。
    if entries.len() > 1 {
        let header_id = header.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        for i in 0..entries.len() {
            let parent = if i == 0 {
                header_id.clone()
            } else {
                entries[i - 1].get("id").and_then(|v| v.as_str()).unwrap_or(&header_id).to_string()
            };
            if let Some(obj) = entries[i].as_object_mut() {
                obj.insert("parentId".to_string(), json!(parent));
            }
        }
    }

    // 找 systemPrompt：fork 子 Worker 把它存在 custom entry (customType=system_prompt) 里
    // 主 Worker 没有（system_prompt 是固定的，不需要存）
    let system_prompt: Option<String> = raw_entries.iter().find_map(|e| {
        if e.get("type").and_then(|v| v.as_str()) == Some("custom")
            && e.get("customType").and_then(|v| v.as_str()) == Some("system_prompt")
        {
            e.get("data")
                .and_then(|d| d.get("systemPrompt"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        } else {
            None
        }
    });

    // Find leaf id (last message entry id, or last entry id) — template uses this
    // to scroll to / highlight the latest message.
    let leaf_id = entries
        .iter()
        .rev()
        .find(|e| e.get("type").and_then(|v| v.as_str()) == Some("message"))
        .and_then(|e| e.get("id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    // Build SessionData JSON (matching pi's format)
    let mut session_data = json!({
        "header": header,
        "entries": entries,
        "leafId": leaf_id,
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
        session_data
            .as_object_mut()
            .map(|o| o.insert("tools".to_string(), serde_json::to_value(&tools).unwrap_or(Value::Null)));
    }

    // Base64 encode
    let session_data_json = serde_json::to_string(&session_data)?;
    let session_data_b64 = base64_encode(&session_data_json);

    // Read template files
    let read_file = |name: &str| -> String {
        let path = format!("{PI_EXPORT_DIR}/{name}");
        std::fs::read_to_string(&path).unwrap_or_default()
    };

    let css = read_file("template.css");
    let mut js = read_file("template.js");
    let marked_js = read_file("vendor/marked.min.js");
    let highlight_js = read_file("vendor/highlight.min.js");
    let mut html = read_file("template.html");

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

    // Replace placeholders
    html = html.replace("{{CSS}}", &css);
    html = html.replace("{{SESSION_DATA}}", &session_data_b64);
    html = html.replace("{{MARKED_JS}}", &marked_js);
    html = html.replace("{{HIGHLIGHT_JS}}", &highlight_js);
    html = html.replace("{{JS}}", &js);
    html = html.replace("{{THEME_VARS}}", "");
    html = html.replace("{{BODY_BG}}", "#fafafa");
    html = html.replace("{{CONTAINER_BG}}", "#ffffff");
    html = html.replace("{{INFO_BG}}", "#f5f5f5");

    // ION 自定义 CSS：给不同角色加淡背景色，提高可读性。
    // 在 </style> 前插入（覆盖 pi template 的默认样式）。
    let ion_custom_css = r#"
    /* ION: 角色背景色区分 */
    .user-message {
      background: #eef4fb !important;
      border-left: 3px solid #3b82f6 !important;
      padding: 12px 16px !important;
      border-radius: 4px !important;
      margin-bottom: 8px !important;
    }
    .user-message::before {
      content: "👤 User";
      display: block;
      font-size: 11px;
      font-weight: 600;
      color: #3b82f6;
      margin-bottom: 6px;
      text-transform: uppercase;
      letter-spacing: 0.5px;
    }
    .assistant-message {
      padding: 12px 16px !important;
      border-left: 3px solid #10b981 !important;
      background: #f0fdf4 !important;
      border-radius: 4px !important;
      margin-bottom: 8px !important;
    }
    .assistant-message::before {
      content: "🤖 Assistant";
      display: block;
      font-size: 11px;
      font-weight: 600;
      color: #10b981;
      margin-bottom: 6px;
      text-transform: uppercase;
      letter-spacing: 0.5px;
    }
    .tool-execution {
      border-left: 3px solid #f59e0b !important;
      background: #fffbeb !important;
      padding: 8px 12px !important;
      border-radius: 4px !important;
      margin-bottom: 8px !important;
    }
    .tool-execution::before {
      content: "🔧 Tool Result";
      display: block;
      font-size: 11px;
      font-weight: 600;
      color: #f59e0b;
      margin-bottom: 4px;
      text-transform: uppercase;
      letter-spacing: 0.5px;
    }
    /* custom_message（fork_separator 等）特殊样式 */
    .custom-message {
      border-left: 3px solid #8b5cf6 !important;
      background: #f5f3ff !important;
      padding: 12px 16px !important;
      border-radius: 4px !important;
      margin-bottom: 8px !important;
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
    let has_parent = header.get("parentSession").and_then(|v| v.as_str()).is_some();
    let spawn_meta = header.get("spawnMeta").cloned();
    if has_parent || spawn_meta.is_some() {
        let parent_session = header.get("parentSession").and_then(|v| v.as_str()).unwrap_or("");
        let spawned_by = spawn_meta.as_ref()
            .and_then(|m| m.get("spawnedBy"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let relation = spawn_meta.as_ref()
            .and_then(|m| m.get("relation"))
            .and_then(|v| v.as_str())
            .unwrap_or("child");

        // 计算主 HTML 的文件名（跟主 session id 同名）
        let parent_html = "session_export.html"; // 兜底

        let origin_banner = format!(r#"
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
</div>"#);
        // 在 <body> 后插入 banner
        if let Some(pos) = html.find("<body>") {
            html.insert_str(pos + 6, &origin_banner);
        }
    }

    // 统计信息（工具调用次数、模型、session 名称）
    let tool_counts: std::collections::HashMap<String, u32> = {
        let mut counts = std::collections::HashMap::new();
        for e in &entries {
            if e.get("type").and_then(|v| v.as_str()) != Some("message") { continue; }
            if let Some(content) = e.get("message").and_then(|m| m.get("content")).and_then(|c| c.as_array()) {
                for c in content {
                    if let Some(name) = c.get("type").and_then(|v| v.as_str()) {
                        if name == "toolCall" {
                            if let Some(tn) = c.get("name").and_then(|v| v.as_str()) {
                                *counts.entry(tn.to_string()).or_insert(0) += 1;
                            }
                        }
                    }
                }
            }
        }
        counts
    };
    let total_tool_calls: u32 = tool_counts.values().sum();
    // 模型名：只从 assistant message 的 model 字段提取（避免抓到 CSS 里的 emoji）
    let model = entries.iter()
        .find_map(|e| {
            let msg = e.get("message")?;
            // 只从 assistant 消息里取
            if msg.get("role").and_then(|v| v.as_str()) != Some("assistant") {
                return None;
            }
            msg.get("model").and_then(|v| v.as_str()).map(|s| s.to_string())
        })
        .unwrap_or_else(|| "unknown".to_string());

    // session 名称：优先 header.name > agent > spawnMeta.spawnedBy > cwd 目录名
    let session_name = header.get("name").and_then(|v| v.as_str())
        .or_else(|| header.get("agent").and_then(|v| v.as_str()))
        .or_else(|| header.get("spawnMeta").and_then(|m| m.get("spawnedBy")).and_then(|v| v.as_str()))
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            // 从 cwd 推导：/Users/xxx/ion → "ion"
            header.get("cwd")
                .and_then(|v| v.as_str())
                .and_then(|cwd| cwd.rsplit('/').next())
                .unwrap_or("Session")
                .to_string()
        });

    // 构造统计 banner HTML
    let mut tool_badges = String::new();
    let mut sorted_tools: Vec<(&String, &u32)> = tool_counts.iter().collect();
    sorted_tools.sort_by(|a, b| b.1.cmp(a.1));
    for (name, count) in &sorted_tools {
        tool_badges.push_str(&format!(
            r#"<span style="background:rgba(255,255,255,0.15);padding:2px 8px;border-radius:10px;font-size:11px;">{} ×{}</span>"#,
            name, count
        ));
    }

    let stats_banner = format!(r#"
<div id="ion-stats-banner" style="
  background: #1a1a2e;
  color: white;
  padding: 12px 20px;
  font-size: 13px;
  display: flex;
  align-items: center;
  gap: 16px;
  border-bottom: 2px solid #16213e;
  flex-wrap: wrap;
">
  <span style="font-size: 16px; font-weight: 700;">📋 {}</span>
  <span style="color:#7ec8e3;">🤖 {}</span>
  <span style="color:#aaa;">🔧 {} tool calls</span>
  <span style="color:#aaa;">📝 {} entries</span>
  <span style="display:flex;gap:4px;flex-wrap:wrap;">{}</span>
</div>"#, session_name, model, total_tool_calls, entries.len(), tool_badges);

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

    // fork 子 session 的文件名在 base64 编码的 session-data 里，
    // HTML 写入前替换看不到明文。改为在 HTML 末尾注入一段 JavaScript：
    // 页面加载后，遍历 DOM 把 "fork_xxxxxxxxxxxx.html" 文本替换成可点击链接。
    let fork_link_script = r#"
<script>
(function() {
  function makeForkLinks() {
    var walker = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT, null, false);
    var node;
    var nodesToReplace = [];
    while (node = walker.nextNode()) {
      var text = node.textContent;
      var match = text.match(/fork_[0-9a-f-]{8,}\.html/);
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
/// Handles:
/// - `message`: unwrap the `Assistant`/`User`/`ToolResult` variant, flatten into pi's `{role, content}` form
/// - `turn_summary` (ION-only): rewrite to `custom_message` so pi template renders it in the tree
/// - others: passed through (already match pi schema)
fn convert_entry(entry: &Value) -> Value {
    let entry_type = entry.get("type").and_then(|v| v.as_str()).unwrap_or("");
    match entry_type {
        "message" => convert_message_entry(entry),
        "turn_summary" => convert_turn_summary_entry(entry),
        // tool_result 需要把 is_error/tool_call_id 转成 camelCase（有些实现单独存）
        "tool_result" => convert_tool_result_entry(entry),
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
        _ => None,
    };
    if let Some(role) = role_for_variant {
        flat.as_object_mut().map(|o| {
            o.entry("role").or_insert(json!(role));
        });
    }
    // ION ToolResult 存的是 role:"tool"，修正为 pi 的 role:"toolResult"
    if variant == "ToolResult" {
        if let Some(obj) = flat.as_object_mut() {
            if obj.get("role").and_then(|v| v.as_str()) == Some("tool") {
                obj.insert("role".to_string(), json!("toolResult"));
            }
        }
    }

    // ToolResult 字段 camelCase 化
    if variant == "ToolResult" {
        if let Some(obj) = flat.as_object_mut() {
            rename_key(obj, "is_error", "isError");
            rename_key(obj, "tool_call_id", "toolCallId");
            rename_key(obj, "tool_name", "toolName");
        }
    }

    // Assistant 字段 camelCase 化：stop_reason → stopReason, response_id → responseId, response_model → responseModel
    if variant == "Assistant" {
        if let Some(obj) = flat.as_object_mut() {
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
    if variant == "Assistant" {
        if let Some(content) = flat.get("content").and_then(|v| v.as_array()) {
            let has_text = content.iter().any(|c| {
                c.get("type").and_then(|v| v.as_str()) == Some("text")
                    && c.get("text").and_then(|v| v.as_str()).map(|s| !s.trim().is_empty()).unwrap_or(false)
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
                                let pairs: Vec<String> = obj.iter()
                                    .map(|(k, v)| {
                                        let val_str = v.as_str().map(|s| s.to_string())
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
                    if let Some(obj) = flat.as_object_mut() {
                        if let Some(content) = obj.get_mut("content").and_then(|v| v.as_array_mut()) {
                            content.insert(0, json!({"type": "text", "text": placeholder}));
                        }
                    }
                }
            }
        }
    }

    out.as_object_mut().map(|o| {
        o.insert("message".to_string(), flat);
    });
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
        if let Some(role) = obj.get("role").and_then(|v| v.as_str()) {
            if role == "tool" {
                obj.insert("role".to_string(), json!("toolResult"));
            }
        }
        if let Some(content) = obj.get_mut("content") {
            *content = convert_content_blocks(content);
        }
    }
    out
}

/// Convert ION turn_summary entry → pi custom_message (so the tree shows the summary text).
///
/// pi template 的 `getTreeNodeDisplayHtml` 不认识 `turn_summary`，
/// 但认识 `custom_message`。我们把 summary 当成 custom_message 的 content，
/// 这样侧边栏会显示 "[turn_summary]: <摘要>" 而不是 undefined。
fn convert_turn_summary_entry(entry: &Value) -> Value {
    let summary = entry
        .get("summary")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let status = entry.get("status").and_then(|v| v.as_str()).unwrap_or("");
    let turn_id = entry.get("turnId");

    let mut out = entry.clone();
    if let Some(obj) = out.as_object_mut() {
        obj.insert("type".to_string(), json!("custom_message"));
        obj.insert("customType".to_string(), json!("turn_summary"));
        // content 用 string 形式（pi 支持 string | array）
        let content = if !summary.is_empty() {
            summary.clone()
        } else if !status.is_empty() {
            format!("turn {}: {}", turn_id.map(|v| v.to_string()).unwrap_or_default(), status)
        } else {
            "(empty turn)".to_string()
        };
        obj.insert("content".to_string(), json!(content));
        obj.insert("display".to_string(), json!(true));
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
fn resolve_session_file(session_id: &str) -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    // Strategy 1: Look up session in global index → get cwd → use cwd path
    let index = crate::session_index::SessionIndex::load();
    if let Some(meta) = index.get(session_id) {
        if let Some(ref project) = meta.project {
            let cwd_path = crate::session_jsonl::session_path(project);
            if cwd_path.exists() {
                // Verify the session file contains this session
                if let Some(file) = crate::session_jsonl::SessionFile::load(project) {
                    if file.header.id == session_id {
                        return Ok(cwd_path);
                    }
                }
            }
        }
    }

    // Strategy 2: Legacy flat format: sessions/{id}.jsonl
    let legacy_path = crate::paths::sessions_dir().join(format!("{session_id}.jsonl"));
    if legacy_path.exists() {
        return Ok(legacy_path);
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
    if sessions_root.exists() {
        if let Ok(entries) = std::fs::read_dir(&sessions_root) {
            for entry in entries.flatten() {
                let dir = entry.path();
                if !dir.is_dir() { continue; }
                // 扫目录下所有 .jsonl 文件（session.jsonl + <sid>.jsonl）
                if let Ok(files) = std::fs::read_dir(&dir) {
                    for file in files.flatten() {
                        let path = file.path();
                        let name = match path.file_name().and_then(|n| n.to_str()) {
                            Some(n) => n,
                            None => continue,
                        };
                        if !name.ends_with(".jsonl") { continue; }
                        // Read only the first line (header) to check id
                        if let Ok(header_line) = std::fs::read_to_string(&path) {
                            if let Some(first_line) = header_line.lines().next() {
                                if let Ok(header) = serde_json::from_str::<Value>(first_line) {
                                    if header.get("id").and_then(|v| v.as_str()) == Some(session_id) {
                                        return Ok(path);
                                    }
                                }
                            }
                        }
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

fn base64_encode(input: &str) -> String {
    const CHARS: &[u8] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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

    #[test]
    fn test_convert_turn_summary() {
        let entry = json!({
            "type": "turn_summary",
            "id": "ts1",
            "parentId": null,
            "timestamp": "2026-01-01T00:00:00Z",
            "summary": "Did some work",
            "status": "completed",
            "turnId": 0
        });
        let pi = convert_entry(&entry);
        assert_eq!(pi.get("type").unwrap(), &json!("custom_message"));
        assert_eq!(pi.get("customType").unwrap(), &json!("turn_summary"));
        assert_eq!(pi.get("content").unwrap(), &json!("Did some work"));
        assert_eq!(pi.get("display").unwrap(), &json!(true));
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
    fn test_empty_summary_turn_falls_back_to_status() {
        let entry = json!({
            "type": "turn_summary",
            "id": "ts1",
            "parentId": null,
            "timestamp": "2026-01-01T00:00:00Z",
            "summary": "",
            "status": "aborted",
            "turnId": 3
        });
        let pi = convert_entry(&entry);
        // Empty summary → fall back to status
        let content = pi.get("content").unwrap().as_str().unwrap();
        assert!(content.contains("aborted"));
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
}
