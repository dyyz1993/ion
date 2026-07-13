//! File Snapshot Harness — FauxProvider 驱动真实 Agent loop 验证文件快照 + 审批闭环
//!
//! 参照 AGENTS.md「测试验证规范」：Harness 层验证 agent 真实行为（工具调用、hook 触发、多轮交互）
//! 不调真 LLM，用 FauxProvider Static 控制每轮响应。
//!
//! 注意：测试间用唯一 cwd（纳秒后缀 → 唯一 project_key）隔离，不依赖 set_current_dir（并发不安全）。
//! file_path 用绝对路径传给 write 工具。

use std::sync::Arc;

use ion::agent::agent_loop::{Agent, AgentConfig};
use ion::agent::extension::ExtensionRegistry;
use ion::agent::tool::{ToolRegistry, WriteTool};
use ion::agent::messages::Message;
use ion::file_snapshot::{FileSnapshotExtension, ApprovalExtension, ApprovalManager, ApprovalStatus};
use ion::storage_context::StorageContext;
use ion_provider::registry::ApiRegistry;
use ion_provider::types::*;
use ion_provider::faux;

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

/// 生成唯一临时 cwd（纳秒后缀 → 唯一 project_key，避免测试间冲突）
fn tmp_cwd(label: &str) -> String {
    let id = format!(
        "fs_harness_{}_{}_{}",
        label,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    );
    let work_dir = std::env::temp_dir().join(&id);
    std::fs::create_dir_all(&work_dir).unwrap();
    work_dir.to_string_lossy().to_string()
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

/// 构造 write 工具调用响应（绝对路径）
fn write_call(cwd: &str, filename: &str, content: &str) -> faux::FauxResponseStep {
    let abs_path = format!("{}/{}", cwd.trim_end_matches('/'), filename);
    faux::FauxResponseStep::Static(faux::faux_assistant_message(
        faux::FauxContent::Single(faux::faux_tool_call(
            "write",
            serde_json::json!({"file_path": abs_path, "content": content}),
        )),
        faux::FauxMessageOptions {
            stop_reason: Some(StopReason::ToolUse),
            error_message: None,
        },
    ))
}

/// 构建带快照+审批的 agent
fn build_agent(
    cwd: &str,
    responses: Vec<faux::FauxResponseStep>,
) -> (
    Agent,
    Arc<ion::file_snapshot::SnapshotStore>,
    Arc<ApprovalManager>,
) {
    let mut registry = ApiRegistry::new();
    let faux_handle = faux::register_faux(&mut registry);
    faux_handle.set_responses(responses);

    let mut tools = ToolRegistry::new();
    tools.register(Box::new(WriteTool));

    let (fs_ext, store) = FileSnapshotExtension::new_pair_with_cwd(cwd);
    let storage = StorageContext::new(cwd, "fs_harness_sess", cwd);
    let mgr = Arc::new(ApprovalManager::new(store.clone(), storage));
    let mut ext_reg = ExtensionRegistry::new();
    ext_reg.register(Box::new(fs_ext));
    ext_reg.register(Box::new(ApprovalExtension::new(mgr.clone())));

    let config = AgentConfig {
        max_turns: Some(5),
        max_retries: 0,
        retry_on_no_tool_use: 0,
        ..Default::default()
    };
    let agent = Agent::new(Arc::new(registry), faux_model(), None, tools, config)
        .with_extensions(ext_reg)
        .with_session_cwd(Some(cwd.to_string()));

    (agent, store, mgr)
}

/// H1：write 工具执行后 SnapshotStore 有记录（采集链路）
#[tokio::test]
async fn h1_write_creates_snapshot() {
    let cwd = tmp_cwd("h1");

    let (mut agent, store, _mgr) =
        build_agent(&cwd, vec![write_call(&cwd, "test.txt", "hello world"), stop_msg(), stop_msg()]);

    agent.run("write test.txt").await.unwrap();

    let has_write = agent
        .messages()
        .iter()
        .any(|m| matches!(m, Message::ToolResult(tr) if tr.tool_name == "write"));
    assert!(has_write, "应执行了 write 工具");

    let snaps = store.load_all_tool_snapshots();
    assert!(
        snaps.iter().any(|s| s.path.contains("test.txt")),
        "write 应被采集到 ToolSnapshot"
    );

    let _ = std::fs::remove_dir_all(&cwd);
}

/// H2：agent Stop 时 on_gate_check 触发，pending 列表有变更
#[tokio::test]
async fn h2_gate_check_triggers_pending() {
    let cwd = tmp_cwd("h2");

    let (mut agent, _store, mgr) =
        build_agent(&cwd, vec![write_call(&cwd, "gate.txt", "gate test"), stop_msg(), stop_msg()]);

    agent.run("write gate.txt").await.unwrap();

    let pending = mgr.compute_pending();
    assert!(
        pending.iter().any(|p| p.path.contains("gate.txt")),
        "on_gate_check 应触发，pending 应含 gate.txt。实际 pending: {:?}",
        pending.iter().map(|p| &p.path).collect::<Vec<_>>()
    );

    let _ = std::fs::remove_dir_all(&cwd);
}

/// H3：approve 单文件 → 不在 pending + 状态 approved
#[tokio::test]
async fn h3_approve_removes_from_pending() {
    let cwd = tmp_cwd("h3");

    let (mut agent, _store, mgr) = build_agent(
        &cwd,
        vec![write_call(&cwd, "approve.txt", "v1"), stop_msg(), stop_msg()],
    );

    agent.run("write approve.txt").await.unwrap();

    let pending_before = mgr.compute_pending();
    assert!(
        pending_before.iter().any(|p| p.path.contains("approve.txt")),
        "approve 前应在 pending"
    );

    mgr.approve("approve.txt").unwrap();

    let pending = mgr.compute_pending();
    assert!(
        !pending.iter().any(|p| p.path.contains("approve.txt")),
        "approve 后不应在 pending"
    );

    let approved = mgr.approvals_list(Some(&ApprovalStatus::Approved));
    assert!(approved.iter().any(|a| a.path == "approve.txt"));

    let _ = std::fs::remove_dir_all(&cwd);
}

/// H4：reject 单文件 → 文件被回滚 + 状态 rejected
#[tokio::test]
async fn h4_reject_rolls_back_file() {
    let cwd = tmp_cwd("h4");
    let abs_path = format!("{}/reject.txt", cwd.trim_end_matches('/'));

    let (mut agent, _store, mgr) = build_agent(
        &cwd,
        vec![
            write_call(&cwd, "reject.txt", "will be rejected"),
            stop_msg(),
            stop_msg(),
        ],
    );

    agent.run("write reject.txt").await.unwrap();

    assert!(std::path::Path::new(&abs_path).exists(), "write 后文件应存在");

    let result = mgr.reject("reject.txt").unwrap();
    assert_eq!(result.action, "deleted", "reject 应回滚删除新文件");
    assert!(!std::path::Path::new(&abs_path).exists(), "reject 后文件应被删除");

    let rejected = mgr.approvals_list(Some(&ApprovalStatus::Rejected));
    assert!(rejected.iter().any(|a| a.path == "reject.txt"));

    let _ = std::fs::remove_dir_all(&cwd);
}

/// H5：approve_all 批量审批全部 pending
#[tokio::test]
async fn h5_approve_all_batch() {
    let cwd = tmp_cwd("h5");

    let (mut agent, _store, mgr) = build_agent(
        &cwd,
        vec![
            write_call(&cwd, "a.txt", "file a"),
            write_call(&cwd, "b.txt", "file b"),
            stop_msg(),
            stop_msg(),
            stop_msg(),
        ],
    );

    agent.run("write a.txt and b.txt").await.unwrap();

    let pending = mgr.compute_pending();
    assert!(
        !pending.is_empty(),
        "应有 pending 文件。实际: {:?}",
        pending.iter().map(|p| &p.path).collect::<Vec<_>>()
    );

    let results = mgr.approve_all();
    assert!(results.iter().all(|r| r.is_ok()), "approve_all 应全成功");

    let pending_after = mgr.compute_pending();
    assert!(pending_after.is_empty(), "approve_all 后 pending 应清空");

    let _ = std::fs::remove_dir_all(&cwd);
}

/// E1：真实 LLM 审批闭环（标 #[ignore]，需 ION_E2E=1 + API key）
#[tokio::test]
#[ignore]
async fn e1_real_agent_approval_workflow() {
    // 运行方式：
    // ION_E2E=1 ION_API_KEY="sk-xxx" \
    //   cargo test --test file_snapshot_harness -- --ignored --nocapture
    //
    // 1. 给 agent 一个真实任务："在当前目录创建一个 hello.rs"
    // 2. agent 真的 write 文件 → Stop → on_gate_check 触发
    // 3. 验证 review_pending 有 hello.rs
    // 4. approve → 验证文件保留
    // 5. reject → 验证回滚
    //
    // TODO: 实现时需要从 config 加载真实 provider/model
}
