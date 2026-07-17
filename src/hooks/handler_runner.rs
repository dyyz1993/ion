//! 5 种 handler 执行引擎 + interpret_exit_code
//!
//! - command: Runtime::execute_command（复用沙箱/权限）
//! - http: reqwest POST（强制 HTTPS，拒绝私网 IP）
//! - prompt: callLLM 单轮（调 ApiRegistry.complete，解析 {block,reason} JSON）
//! - agent: Runtime::spawn_worker（带工具循环的子 Worker）
//! - mcp_tool: McpManager.call_tool（stub，待接入）

use std::time::Duration;

use super::{HookHandler, HookOutcome, HandlerType};

/// 执行上下文（handler 执行时需要的能力）
pub struct HookExecContext {
    /// 项目目录（变量替换用）
    pub project_dir: String,
    /// 事件名
    pub event_name: String,
    /// Runtime（command + agent handler 用）
    pub runtime: Option<std::sync::Arc<dyn crate::runtime::Runtime>>,
    /// ApiRegistry（prompt handler 调 LLM 用）
    pub registry: Option<std::sync::Arc<ion_provider::registry::ApiRegistry>>,
    /// 当前会话模型（prompt handler 用）
    pub model: Option<ion_provider::types::Model>,
}

/// 执行单个 handler，返回 outcome
pub async fn run_handler(
    handler: &HookHandler,
    stdin_data: serde_json::Value,
    ctx: &HookExecContext,
) -> HookOutcome {
    let timeout = handler.timeout.unwrap_or(30) as u64;
    let result = tokio::time::timeout(
        Duration::from_secs(timeout),
        run_handler_inner(handler, stdin_data, ctx),
    ).await;

    match result {
        Ok(outcome) => outcome,
        Err(_) => {
            tracing::warn!("[hooks] handler timeout after {}s (event={})", timeout, ctx.event_name);
            HookOutcome::default() // 超时不阻断
        }
    }
}

async fn run_handler_inner(
    handler: &HookHandler,
    stdin_data: serde_json::Value,
    ctx: &HookExecContext,
) -> HookOutcome {
    match handler.handler_type {
        HandlerType::Command => run_command(handler, stdin_data, ctx).await,
        HandlerType::Http => run_http(handler, stdin_data).await,
        HandlerType::Prompt => run_prompt(handler, stdin_data, ctx).await,
        HandlerType::Agent => run_agent(handler, stdin_data, ctx).await,
        HandlerType::McpTool => run_mcp_tool(handler, stdin_data).await,
    }
}

// ---------------------------------------------------------------------------
// command handler — Runtime::execute_command
// ---------------------------------------------------------------------------

async fn run_command(
    handler: &HookHandler,
    stdin_data: serde_json::Value,
    ctx: &HookExecContext,
) -> HookOutcome {
    let cmd = match &handler.command {
        Some(c) => c.clone(),
        None => return HookOutcome::default(),
    };

    // 变量替换（对齐 pi handler-runner）
    let cmd = cmd
        .replace("$CLAUDE_PROJECT_DIR", &ctx.project_dir)
        .replace("$ION_PROJECT_DIR", &ctx.project_dir);

    // 优先用 Runtime（复用沙箱/权限），否则直接 tokio::spawn bash
    let (stdout, stderr, exit_code) = if let Some(ref rt) = ctx.runtime {
        // Runtime::execute_command 不支持 stdin，我们用环境变量传 hook context
        // （对齐 pi 的 PI_HOOK_* 环境变量方式）
        match rt.execute_command(&cmd, handler.timeout.unwrap_or(30) as u64).await {
            Ok(result) => result,
            Err(e) => {
                tracing::warn!("[hooks] command failed: {e}");
                return HookOutcome::default();
            }
        }
    } else {
        // Fallback: 直接 spawn（开发/测试用）
        // current_dir 透传 ctx.project_dir，让 bash 在项目目录里跑（命令常用相对路径
        // 如 `bash .ion/scripts/x.sh`）。这样不依赖进程级 cwd，并发安全。
        match spawn_command_with_stdin(&cmd, &stdin_data, handler.timeout.unwrap_or(30) as u64, &ctx.project_dir).await {
            Ok(result) => result,
            Err(e) => {
                tracing::warn!("[hooks] command spawn failed: {e}");
                return HookOutcome::default();
            }
        }
    };

    interpret_exit_code(exit_code, &stdout, &stderr, handler)
}

/// 直接 spawn bash + stdin（fallback，不走 Runtime）
///
/// `current_dir` 设为 `ctx.project_dir`：hook 命令常用相对路径（如
/// `bash .ion/scripts/x.sh`），显式指定 cwd 后不再依赖进程级 current_dir，
/// 并发执行（多个 hook / 测试并行）时不会互相踩进程 cwd。
async fn spawn_command_with_stdin(
    cmd: &str,
    stdin_data: &serde_json::Value,
    timeout_secs: u64,
    current_dir: &str,
) -> Result<(String, String, i32), String> {
    use tokio::io::AsyncWriteExt;

    let mut command = tokio::process::Command::new("bash");
    command.arg("-c").arg(cmd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    // 显式指定工作目录（非空时）。空字符串则继承进程 cwd（向后兼容）。
    if !current_dir.is_empty() {
        command.current_dir(current_dir);
    }
    let mut child = command.spawn()
        .map_err(|e| format!("spawn failed: {e}"))?;

    // 写 stdin
    if let Some(mut stdin) = child.stdin.take() {
        let stdin_bytes = serde_json::to_vec(stdin_data).unwrap_or_default();
        let _ = stdin.write_all(&stdin_bytes).await;
    }

    let output = tokio::time::timeout(
        Duration::from_secs(timeout_secs),
        child.wait_with_output(),
    ).await;

    match output {
        Ok(Ok(out)) => {
            let stdout = String::from_utf8_lossy(&out.stdout).to_string();
            let stderr = String::from_utf8_lossy(&out.stderr).to_string();
            let code = out.status.code().unwrap_or(-1);
            Ok((stdout, stderr, code))
        }
        Ok(Err(e)) => Err(format!("wait failed: {e}")),
        Err(_) => Err("timeout".into()),
    }
}

// ---------------------------------------------------------------------------
// http handler — reqwest POST
// ---------------------------------------------------------------------------

async fn run_http(handler: &HookHandler, stdin_data: serde_json::Value) -> HookOutcome {
    let url = match &handler.url {
        Some(u) => u.clone(),
        None => return HookOutcome::default(),
    };

    // 安全校验：必须 https://，拒绝私网 IP
    if let Err(reason) = validate_url(&url) {
        tracing::warn!("[hooks] http handler url rejected: {reason}");
        return HookOutcome { block: true, block_reason: Some(reason), ..Default::default() };
    }

    let timeout = handler.timeout.unwrap_or(30) as u64;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout))
        .build();

    let client = match client {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("[hooks] http client build failed: {e}");
            return HookOutcome::default();
        }
    };

    let resp = client.post(&url).json(&stdin_data).send().await;
    match resp {
        Ok(r) => {
            let status = r.status().as_u16();
            let body = r.text().await.unwrap_or_default();
            match status {
                200 => interpret_stdout(&body),
                403 => HookOutcome {
                    block: true,
                    block_reason: Some(body),
                    ..Default::default()
                },
                _ => HookOutcome::default(),
            }
        }
        Err(e) => {
            tracing::warn!("[hooks] http request failed: {e}");
            HookOutcome::default()
        }
    }
}

fn validate_url(url: &str) -> Result<(), String> {
    if !url.starts_with("https://") {
        return Err("url must be https://".into());
    }
    // 拒绝常见私网 IP（简化检查）
    let host_part = url.trim_start_matches("https://").split('/').next().unwrap_or("");
    let host = host_part.split(':').next().unwrap_or("");
    for blocked in &["127.0.0.1", "localhost", "10.", "172.16.", "172.17.", "172.18.",
                     "172.19.", "172.20.", "172.21.", "172.22.", "172.23.", "172.24.",
                     "172.25.", "172.26.", "172.27.", "172.28.", "172.29.", "172.30.",
                     "172.31.", "192.168.", "169.254."] {
        if host == *blocked || host.starts_with(blocked) {
            return Err(format!("private IP rejected: {host}"));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// prompt handler — callLLM 单轮（调 ApiRegistry.complete，解析 {block,reason} JSON 决策）
// ---------------------------------------------------------------------------

async fn run_prompt(handler: &HookHandler, stdin_data: serde_json::Value, ctx: &HookExecContext) -> HookOutcome {
    let prompt = match &handler.prompt {
        Some(p) => p.clone(),
        None => return HookOutcome::default(),
    };
    let registry = match &ctx.registry {
        Some(r) => r.clone(),
        None => {
            tracing::warn!("[hooks] prompt handler needs ApiRegistry, none available");
            return HookOutcome::default();
        }
    };
    // 用当前会话模型（简化：不查 handler.model，用户要换模型用 command handler + 脚本调 API）
    let model = match &ctx.model {
        Some(m) => m.clone(),
        None => {
            tracing::warn!("[hooks] prompt handler needs a model, none available");
            return HookOutcome::default();
        }
    };

    // 构造 context：system prompt = handler.prompt，user message = stdin 上下文
    let system = format!(
        "{prompt}\n\n---\nHook context ({ }):\n{}",
        ctx.event_name,
        serde_json::to_string_pretty(&stdin_data).unwrap_or_default()
    );
    let context = ion_provider::types::Context {
        system_prompt: Some(system),
        messages: vec![ion_provider::types::Message::User(ion_provider::types::UserMessage {
            role: "user".into(),
            content: vec![ion_provider::types::ContentBlock::Text(ion_provider::types::TextContent {
                text: "Respond with JSON: {\"block\": false} or {\"block\": true, \"reason\": \"...\"}".into(),
                text_signature: None,
            })],
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0),
        })],
        tools: None,
    };

    let options = ion_provider::StreamOptions {
        max_tokens: Some(1024),
        api_key: None,
        reasoning: None,
        timeout_ms: Some((handler.timeout.unwrap_or(30) as u64) * 1000),
        max_retries: None,
        response_format: None,
    };

    // 调 LLM
    match ion_provider::registry::complete(&registry, &model, &context, Some(&options)).await {
        Ok(assistant_msg) => {
            // 提取文本
            let text: String = assistant_msg.content.iter()
                .filter_map(|c| if let ion_provider::types::AssistantContentBlock::Text(t) = c {
                    Some(t.text.clone())
                } else {
                    None
                })
                .collect::<Vec<_>>()
                .join("");
            // 解析 JSON 决策
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                if json.get("block").and_then(|v| v.as_bool()) == Some(true) {
                    let reason = json.get("reason").and_then(|v| v.as_str()).unwrap_or("blocked by prompt hook");
                    return HookOutcome { block: true, block_reason: Some(reason.into()), ..Default::default() };
                }
            }
            // 不是 block 就看有没有 additionalContext
            interpret_stdout(&text)
        }
        Err(e) => {
            tracing::warn!("[hooks] prompt handler LLM call failed: {e}");
            HookOutcome::default()
        }
    }
}

// ---------------------------------------------------------------------------
// agent handler — Runtime::spawn_worker（带工具循环的子 Worker）
// ---------------------------------------------------------------------------

async fn run_agent(
    handler: &HookHandler,
    stdin_data: serde_json::Value,
    ctx: &HookExecContext,
) -> HookOutcome {
    let prompt = match &handler.prompt {
        Some(p) => p.clone(),
        None => return HookOutcome::default(),
    };
    let rt = match &ctx.runtime {
        Some(r) => r.clone(),
        None => {
            tracing::warn!("[hooks] agent handler needs runtime, none available");
            return HookOutcome::default();
        }
    };

    // 组装任务 prompt（注入 stdin 上下文）
    let task = format!(
        "{prompt}\n\n---\nHook context ({ }):\n{}",
        ctx.event_name,
        serde_json::to_string_pretty(&stdin_data).unwrap_or_default()
    );

    // 读当前进程的 hook depth，+1 传给子 Worker（防递归）
    let current_depth = std::env::var("ION_HOOK_DEPTH")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0);

    let req = crate::runtime::SpawnWorkerRequest {
        relation: crate::runtime::SpawnRelation::Child,
        // 用户可在 hooks.json 里配 "agent": "outline-syncer"
        // 指向 .ion/agents/outline-syncer.md（自定义 agent 角色）
        // 不填则用 "default"
        agent: handler.agent.clone().unwrap_or_else(|| "default".to_string()),
        task,
        name: None,
        report_channel: None,
        wait: true, // 阻塞等首轮完成
        worktree: None,
        hook_depth: Some(current_depth + 1),  // 子 Worker depth+1，防 agent handler 递归
    };

    let timeout = handler.timeout.unwrap_or(300) as u64;
    let result = tokio::time::timeout(
        Duration::from_secs(timeout),
        rt.spawn_worker(req),
    ).await;

    match result {
        Ok(Ok(resp)) => {
            // 子 Worker 完成首轮，解析输出判断 block
            let output = resp.first_turn_output.unwrap_or_default();
            interpret_stdout(&output)
        }
        Ok(Err(e)) => {
            tracing::warn!("[hooks] agent handler spawn_worker failed: {e}");
            HookOutcome::default() // 不阻断主流程
        }
        Err(_) => {
            tracing::warn!("[hooks] agent handler timeout after {}s", timeout);
            HookOutcome::default()
        }
    }
}

// ---------------------------------------------------------------------------
// mcp_tool handler — McpManager.call_tool（stub，待接入）
// ---------------------------------------------------------------------------

async fn run_mcp_tool(handler: &HookHandler, stdin_data: serde_json::Value) -> HookOutcome {
    let _server = match &handler.server {
        Some(s) => s.clone(),
        None => return HookOutcome::default(),
    };
    let _tool = match &handler.tool {
        Some(t) => t.clone(),
        None => return HookOutcome::default(),
    };
    // TODO: 需要接入 McpManager（Extension trait 当前未暴露此能力）
    tracing::info!("[hooks] mcp_tool handler (stub, needs mcp_manager): server={:?} tool={:?}", handler.server, handler.tool);
    let _ = stdin_data;
    HookOutcome::default()
}

// ---------------------------------------------------------------------------
// 退出码解释 + stdout 解析（Claude Code 兼容协议）
// ---------------------------------------------------------------------------

/// 按退出码 0/2/3 解释 handler 结果
///
/// - exit 0 + stdout JSON → 解析 additionalContext / updatedInput / decision 等
/// - exit 0 + stdout 纯文本 → 当作 additionalContext（仅 SessionStart/UserPromptSubmit）
/// - exit 2 → block（reason 取 stderr 或 JSON.permissionDecisionReason）
/// - exit 3 → ask（仅 PreToolUse，请求用户确认）
/// - 其他 → 忽略
pub fn interpret_exit_code(exit_code: i32, stdout: &str, stderr: &str, _handler: &HookHandler) -> HookOutcome {
    match exit_code {
        0 => interpret_stdout(stdout),
        2 => {
            // exit 2 = 阻断
            let reason = if let Some(json) = parse_json(stdout) {
                json.get("reason")
                    .or_else(|| json.get("message"))
                    .or_else(|| json.get("permissionDecisionReason"))
                    .and_then(|v| v.as_str())
                    .map(String::from)
            } else {
                None
            }.unwrap_or_else(|| {
                if stderr.is_empty() { "blocked by hook".into() } else { stderr.to_string() }
            });
            HookOutcome { block: true, block_reason: Some(reason), ..Default::default() }
        }
        3 => {
            // exit 3 = 请求确认（仅 PreToolUse）
            HookOutcome { ask: true, block_reason: Some(stderr.to_string()), ..Default::default() }
        }
        _ => HookOutcome::default(), // 其他退出码忽略
    }
}

/// 解析 stdout（exit 0 时）
///
/// 尝试 JSON.parse：
/// - 成功 → 解析 hookSpecificOutput.additionalContext / permissionDecision / updatedInput / decision
/// - 失败 → 当作纯文本 additionalContext
pub fn interpret_stdout(stdout: &str) -> HookOutcome {
    let stdout = stdout.trim();
    if stdout.is_empty() {
        return HookOutcome::default();
    }

    if let Some(json) = parse_json(stdout) {
        // JSON 模式
        let mut outcome = HookOutcome::default();

        // decision: "block" → 阻断
        if json.get("decision").and_then(|v| v.as_str()) == Some("block") {
            outcome.block = true;
            outcome.block_reason = json.get("reason").and_then(|v| v.as_str()).map(String::from);
        }

        // hookSpecificOutput
        if let Some(hso) = json.get("hookSpecificOutput") {
            if let Some(ctx) = hso.get("additionalContext").and_then(|v| v.as_str()) {
                outcome.additional_context = Some(ctx.to_string());
            }
            if let Some(perm) = hso.get("permissionDecision").and_then(|v| v.as_str()) {
                match perm {
                    "deny" => { outcome.block = true; }
                    "ask" => { outcome.ask = true; }
                    _ => {}
                }
            }
            if let Some(ui) = hso.get("updatedInput") {
                outcome.updated_input = Some(ui.clone());
            }
        }

        // 顶层 additionalContext（简写）
        if outcome.additional_context.is_none() {
            if let Some(ctx) = json.get("additionalContext").and_then(|v| v.as_str()) {
                outcome.additional_context = Some(ctx.to_string());
            }
        }

        outcome
    } else {
        // 纯文本模式 → 当作 additionalContext
        HookOutcome {
            additional_context: Some(stdout.to_string()),
            ..Default::default()
        }
    }
}

fn parse_json(s: &str) -> Option<serde_json::Value> {
    serde_json::from_str(s).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_interpret_exit_0_text() {
        let handler = HookHandler {
            handler_type: HandlerType::Command,
            agent: None,
            command: None, url: None, prompt: None, server: None, tool: None,
            input: None, model: None, timeout: None, if_clause: None,
            r#async: false, async_rewake: false, once: false,
            status_message: None, allowed_tools: None, max_turns: None,
        };
        let outcome = interpret_exit_code(0, "hello world", "", &handler);
        assert_eq!(outcome.additional_context.as_deref(), Some("hello world"));
        assert!(!outcome.block);
    }

    #[test]
    fn test_interpret_exit_0_json_block() {
        let handler = HookHandler {
            handler_type: HandlerType::Command,
            agent: None,
            command: None, url: None, prompt: None, server: None, tool: None,
            input: None, model: None, timeout: None, if_clause: None,
            r#async: false, async_rewake: false, once: false,
            status_message: None, allowed_tools: None, max_turns: None,
        };
        let stdout = r#"{"decision":"block","reason":"forbidden"}"#;
        let outcome = interpret_exit_code(0, stdout, "", &handler);
        assert!(outcome.block);
        assert_eq!(outcome.block_reason.as_deref(), Some("forbidden"));
    }

    #[test]
    fn test_interpret_exit_2() {
        let handler = HookHandler {
            handler_type: HandlerType::Command,
            agent: None,
            command: None, url: None, prompt: None, server: None, tool: None,
            input: None, model: None, timeout: None, if_clause: None,
            r#async: false, async_rewake: false, once: false,
            status_message: None, allowed_tools: None, max_turns: None,
        };
        let outcome = interpret_exit_code(2, "", "command not allowed", &handler);
        assert!(outcome.block);
        assert_eq!(outcome.block_reason.as_deref(), Some("command not allowed"));
    }

    #[test]
    fn test_interpret_exit_0_json_additional_context() {
        let handler = HookHandler {
            handler_type: HandlerType::Command,
            agent: None,
            command: None, url: None, prompt: None, server: None, tool: None,
            input: None, model: None, timeout: None, if_clause: None,
            r#async: false, async_rewake: false, once: false,
            status_message: None, allowed_tools: None, max_turns: None,
        };
        let stdout = r#"{"hookSpecificOutput":{"additionalContext":"injected text"}}"#;
        let outcome = interpret_exit_code(0, stdout, "", &handler);
        assert_eq!(outcome.additional_context.as_deref(), Some("injected text"));
    }

    #[test]
    fn test_validate_url() {
        assert!(validate_url("https://example.com/hook").is_ok());
        assert!(validate_url("http://example.com/hook").is_err()); // 非 https
        assert!(validate_url("https://127.0.0.1/hook").is_err()); // 私网
        assert!(validate_url("https://192.168.1.1/hook").is_err()); // 私网
    }
}
