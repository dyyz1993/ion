//! Session Switch Hook Harness — 验证 on_session_before_switch 钩子触发 + veto 能力
//!
//! FauxProvider 驱动真实 Agent loop，让 LLM 调 branch_session 工具，
//! 验证钩子在工具执行**前**触发，且扩展返回 Err 时工具被 veto（不执行）。
//!
//! 覆盖：agent_loop.rs 里 branch_session 特判 → on_session_before_switch。

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;

use ion::agent::agent_loop::{Agent, AgentConfig};
use ion::agent::error::AgentResult;
use ion::agent::extension::{Extension, ExtensionRegistry, SessionSwitchContext};
use ion::agent::tool::{BranchSessionTool, ToolRegistry, WriteTool};
use ion_provider::faux;
use ion_provider::registry::ApiRegistry;
use ion_provider::types::*;

/// 生成唯一后缀（测试间隔离）
fn uuid_like() -> String {
    use std::time::SystemTime;
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("{nanos}")
}

/// BranchSessionTool 读进程 cwd（std::env::current_dir），并行测试会互相踩。
/// 用全局 Mutex 串行化所有 set_current_dir 操作，保证确定性。
/// lock 时忽略 poison（某个测试 panic 不影响其他测试拿锁）。
static CWD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn cwd_guard() -> std::sync::MutexGuard<'static, ()> {
    CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// 创建唯一临时目录并 canonicalize（macOS /tmp→/private/tmp，避免符号链接导致
/// seed 的路径和 BranchSessionTool 读 current_dir() 算出的 hash 不一致）。
fn make_cwd(label: &str) -> String {
    let raw = format!("/tmp/ion_{label}_{}_{}", std::process::id(), uuid_like());
    std::fs::remove_dir_all(&raw).ok();
    std::fs::create_dir_all(&raw).unwrap();
    std::fs::canonicalize(&raw)
        .unwrap_or_else(|_| std::path::PathBuf::from(&raw))
        .to_string_lossy()
        .to_string()
}

fn faux_model() -> Model {
    Model {
        id: "faux-test".into(),
        name: "Faux Test".into(),
        api: "faux".into(),
        provider: "faux".into(),
        base_url: "".into(),
        reasoning: false,
        input: vec!["text".into()],
        cost: Cost::default(),
        context_window: 128000,
        max_tokens: 4096,
        compat: None,
        headers: None,
    }
}

fn stop_msg() -> faux::FauxResponseStep {
    faux::FauxResponseStep::Static(faux::faux_assistant_message(
        faux::FauxContent::Text("done".into()),
        faux::FauxMessageOptions {
            stop_reason: Some(StopReason::Stop),
            error_message: None,
        },
    ))
}

/// 构造 branch_session 工具调用响应
fn branch_call(from_entry: &str, name: Option<&str>) -> faux::FauxResponseStep {
    let mut args = serde_json::json!({"from_entry": from_entry});
    if let Some(n) = name {
        args["name"] = serde_json::json!(n);
    }
    faux::FauxResponseStep::Static(faux::faux_assistant_message(
        faux::FauxContent::Single(faux::faux_tool_call("branch_session", args)),
        faux::FauxMessageOptions {
            stop_reason: Some(StopReason::ToolUse),
            error_message: None,
        },
    ))
}

/// 构造 rollback 工具调用响应
fn rollback_call(from_entry: &str, reason: &str) -> faux::FauxResponseStep {
    faux::FauxResponseStep::Static(faux::faux_assistant_message(
        faux::FauxContent::Single(faux::faux_tool_call(
            "branch_session",
            serde_json::json!({"from_entry": from_entry, "is_rollback": true, "reason": reason}),
        )),
        faux::FauxMessageOptions {
            stop_reason: Some(StopReason::ToolUse),
            error_message: None,
        },
    ))
}

/// 统计 on_session_before_switch 触发次数 + 可选 veto
struct SwitchProbe {
    trigger_count: Arc<AtomicUsize>,
    /// 捕获到的最后一次 action
    last_action: Arc<std::sync::Mutex<Option<String>>>,
    /// 是否 veto（返回 Err）
    veto: bool,
}

impl SwitchProbe {
    fn new(veto: bool) -> Self {
        Self {
            trigger_count: Arc::new(AtomicUsize::new(0)),
            last_action: Arc::new(std::sync::Mutex::new(None)),
            veto,
        }
    }
}

#[async_trait]
impl Extension for SwitchProbe {
    fn name(&self) -> &str { "switch_probe" }

    async fn on_session_before_switch(&self, ctx: &SessionSwitchContext) -> AgentResult<()> {
        self.trigger_count.fetch_add(1, Ordering::SeqCst);
        *self.last_action.lock().unwrap() = Some(ctx.action.clone());
        if self.veto {
            Err(ion::agent::error::AgentError::Tool("vetoed by switch_probe".into()))
        } else {
            Ok(())
        }
    }
}

/// 构建测试 agent：注册 BranchSessionTool + WriteTool + SwitchProbe 扩展
fn build_agent(cwd: &str, responses: Vec<faux::FauxResponseStep>, probe: SwitchProbe) -> Agent {
    let mut registry = ApiRegistry::new();
    let faux_handle = faux::register_faux(&mut registry);
    faux_handle.set_responses(responses);

    let mut tools = ToolRegistry::new();
    tools.register(Box::new(BranchSessionTool));
    tools.register(Box::new(WriteTool));

    let mut ext_reg = ExtensionRegistry::new();
    ext_reg.register(Box::new(probe));

    let config = AgentConfig {
        max_turns: Some(5),
        max_retries: 0,
        retry_on_no_tool_use: 0,
        ..Default::default()
    };
    Agent::new(Arc::new(registry), faux_model(), None, tools, config)
        .with_extensions(ext_reg)
        .with_session_cwd(Some(cwd.to_string()))
}

/// 写几条消息到 session 文件（用 ion 的 session_path），造出可分叉的 entry
fn seed_session(cwd: &str) -> String {
    use std::io::Write;
    let path = ion::session_jsonl::session_path(cwd);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let header = serde_json::json!({
        "type": "session", "id": "sess_test", "cwd": cwd,
        "model": "faux", "provider": "faux", "createdAt": "2026-01-01T00:00:00Z",
        "parentSession": null,
    });
    let user_msg = serde_json::json!({
        "type": "message", "id": "entry_1", "parentId": "sess_test",
        "message": {"role": "user", "content": [{"type":"text","text":"hi"}]},
        "timestamp": "2026-01-01T00:00:01Z",
    });
    let mut f = std::fs::File::create(&path).unwrap();
    writeln!(f, "{}", header).unwrap();
    writeln!(f, "{}", user_msg).unwrap();
    "entry_1".to_string()
}

// ──────────────────────────────────────────────────────────
// 测试用例
// ──────────────────────────────────────────────────────────

/// SH1：branch_session 执行前触发 on_session_before_switch（action="branch"）
#[tokio::test]
async fn sh1_branch_triggers_hook() {
    let _guard = cwd_guard();
    let cwd = make_cwd("sh1");
    std::env::set_current_dir(&cwd).ok();
    let entry = seed_session(&cwd);

    let probe = SwitchProbe::new(false); // 不 veto
    let trigger_count = probe.trigger_count.clone();
    let last_action = probe.last_action.clone();

    let responses = vec![branch_call(&entry, Some("alt")), stop_msg(), stop_msg()];
    let mut agent = build_agent(&cwd, responses, probe);

    // run 可能因为 branch_session 的返回格式而继续，用 stop 兜底
    let _ = agent.run("branch from entry_1").await;

    let count = trigger_count.load(Ordering::SeqCst);
    assert!(count >= 1, "on_session_before_switch 应被触发至少 1 次，实际 {count}");

    let action = last_action.lock().unwrap().clone();
    assert_eq!(action.as_deref(), Some("branch"), "action 应为 branch，实际 {action:?}");

    let _ = std::fs::remove_dir_all(&cwd);
}

/// SH2：rollback 时 action="rollback"
#[tokio::test]
async fn sh2_rollback_action_is_rollback() {
    let _guard = cwd_guard();
    let cwd = make_cwd("sh2");
    std::env::set_current_dir(&cwd).ok();
    let entry = seed_session(&cwd);

    let probe = SwitchProbe::new(false);
    let last_action = probe.last_action.clone();

    let responses = vec![rollback_call(&entry, "wrong direction"), stop_msg(), stop_msg()];
    let mut agent = build_agent(&cwd, responses, probe);
    let _ = agent.run("rollback").await;

    let action = last_action.lock().unwrap().clone();
    assert_eq!(action.as_deref(), Some("rollback"), "rollback 时 action 应为 rollback，实际 {action:?}");

    let _ = std::fs::remove_dir_all(&cwd);
}

/// SH3：扩展 veto 时 branch_session 工具不执行（session 文件没有新增 branch entry）
#[tokio::test]
async fn sh3_veto_blocks_branch_execution() {
    let _guard = cwd_guard();
    let cwd = make_cwd("sh3");
    std::env::set_current_dir(&cwd).ok();
    let entry = seed_session(&cwd);

    let probe = SwitchProbe::new(true); // veto
    let trigger_count = probe.trigger_count.clone();

    let responses = vec![branch_call(&entry, Some("blocked")), stop_msg(), stop_msg()];
    let mut agent = build_agent(&cwd, responses, probe);

    let _ = agent.run("try to branch").await;

    // 钩子被触发了
    assert!(trigger_count.load(Ordering::SeqCst) >= 1, "veto 钩子应触发");

    // session 文件里不应该有 branch entry（type=="branch"）
    let content = std::fs::read_to_string(ion::session_jsonl::session_path(&cwd)).unwrap();
    let has_branch = content.lines().any(|l| {
        l.contains("\"type\":\"branch\"") || l.contains("\"type\":\"leaf_pointer\"")
    });
    assert!(!has_branch, "veto 后 session 文件不应有 branch/leaf_pointer entry");

    let _ = std::fs::remove_dir_all(&cwd);
}

/// SH4：不 veto 时 branch 正常执行（session 文件有 leaf_pointer）
#[tokio::test]
async fn sh4_allow_executes_branch() {
    let _guard = cwd_guard();
    let cwd = make_cwd("sh4");
    std::env::set_current_dir(&cwd).ok();
    let entry = seed_session(&cwd);

    let probe = SwitchProbe::new(false); // 不 veto
    let responses = vec![branch_call(&entry, Some("alt-path")), stop_msg(), stop_msg()];
    let mut agent = build_agent(&cwd, responses, probe);

    let _ = agent.run("branch").await;

    let content = std::fs::read_to_string(ion::session_jsonl::session_path(&cwd)).unwrap();
    let has_leaf = content.lines().any(|l| l.contains("\"type\":\"leaf_pointer\""));
    assert!(has_leaf, "不 veto 时应正常执行 branch（有 leaf_pointer entry）");

    let _ = std::fs::remove_dir_all(&cwd);
}

/// SH5：非 branch_session 工具不触发 session 钩子
#[tokio::test]
async fn sh5_other_tools_dont_trigger() {
    let _guard = cwd_guard();
    let cwd = make_cwd("sh5");
    std::env::set_current_dir(&cwd).ok();
    let _ = seed_session(&cwd);

    let probe = SwitchProbe::new(false);
    let trigger_count = probe.trigger_count.clone();

    // 写文件工具调用，不是 branch_session
    let abs = format!("{}/note.txt", cwd.trim_end_matches('/'));
    let write_step = faux::FauxResponseStep::Static(faux::faux_assistant_message(
        faux::FauxContent::Single(faux::faux_tool_call(
            "write",
            serde_json::json!({"file_path": abs, "content": "x"}),
        )),
        faux::FauxMessageOptions {
            stop_reason: Some(StopReason::ToolUse),
            error_message: None,
        },
    ));
    let responses = vec![write_step, stop_msg(), stop_msg()];
    let mut agent = build_agent(&cwd, responses, probe);

    let _ = agent.run("write a note").await;

    assert_eq!(trigger_count.load(Ordering::SeqCst), 0, "非 branch 工具不应触发 session 钩子");

    let _ = std::fs::remove_dir_all(&cwd);
}
