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

/// H6：reject_all 批量拒绝 + 批量回滚（文档 V6）
#[tokio::test]
async fn h6_reject_all_rolls_back_files() {
    let cwd = tmp_cwd("h6");
    let abs_a = format!("{}/a.txt", cwd.trim_end_matches('/'));
    let abs_b = format!("{}/b.txt", cwd.trim_end_matches('/'));

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

    assert!(std::path::Path::new(&abs_a).exists(), "write 后 a.txt 应存在");
    assert!(std::path::Path::new(&abs_b).exists(), "write 后 b.txt 应存在");

    let results = mgr.reject_all();
    assert!(
        results.iter().all(|r| r.is_ok()),
        "reject_all 应全成功。结果: {:?}",
        results.iter().map(|r| r.as_ref().map(|rf| &rf.action).map_err(|e| e.as_str())).collect::<Vec<_>>()
    );

    // 两个新文件都应被删除（action=deleted）
    assert!(!std::path::Path::new(&abs_a).exists(), "reject_all 后 a.txt 应被删除");
    assert!(!std::path::Path::new(&abs_b).exists(), "reject_all 后 b.txt 应被删除");

    // 两文件状态都变 rejected
    let rejected = mgr.approvals_list(Some(&ApprovalStatus::Rejected));
    assert!(rejected.iter().any(|a| a.path == "a.txt"), "a.txt 应为 rejected");
    assert!(rejected.iter().any(|a| a.path == "b.txt"), "b.txt 应为 rejected");

    // pending 清空
    let pending = mgr.compute_pending();
    assert!(pending.is_empty(), "reject_all 后 pending 应清空");

    let _ = std::fs::remove_dir_all(&cwd);
}

/// H7：review_approvals 状态查询 + 按 status 过滤（文档 V7）
#[tokio::test]
async fn h7_approvals_list_filter_by_status() {
    let cwd = tmp_cwd("h7");

    let (mut agent, _store, mgr) = build_agent(
        &cwd,
        vec![
            write_call(&cwd, "keep.txt", "approved file"),
            write_call(&cwd, "drop.txt", "rejected file"),
            stop_msg(),
            stop_msg(),
            stop_msg(),
        ],
    );

    agent.run("write keep.txt and drop.txt").await.unwrap();

    // 分别 approve 和 reject
    mgr.approve("keep.txt").unwrap();
    mgr.reject("drop.txt").unwrap();

    // 查全部 → 应含 keep (approved) + drop (rejected)
    let all = mgr.approvals_list(None);
    assert!(all.iter().any(|a| a.path == "keep.txt" && a.status == ApprovalStatus::Approved),
        "全部查询应含 keep.txt (approved)。实际: {:?}", all);
    assert!(all.iter().any(|a| a.path == "drop.txt" && a.status == ApprovalStatus::Rejected),
        "全部查询应含 drop.txt (rejected)。实际: {:?}", all);

    // 只查 approved → 只含 keep
    let approved_only = mgr.approvals_list(Some(&ApprovalStatus::Approved));
    assert!(approved_only.iter().all(|a| a.status == ApprovalStatus::Approved),
        "approved 过滤应只含 approved。实际: {:?}", approved_only);
    assert!(approved_only.iter().any(|a| a.path == "keep.txt"));

    // 只查 rejected → 只含 drop
    let rejected_only = mgr.approvals_list(Some(&ApprovalStatus::Rejected));
    assert!(rejected_only.iter().all(|a| a.status == ApprovalStatus::Rejected),
        "rejected 过滤应只含 rejected。实际: {:?}", rejected_only);
    assert!(rejected_only.iter().any(|a| a.path == "drop.txt"));

    // approved 的有 approved_tree_hash，rejected 的为 None
    let keep_record = all.iter().find(|a| a.path == "keep.txt").unwrap();
    assert!(keep_record.approved_tree_hash.is_some(), "approved 文件应有 baseline tree hash");
    let drop_record = all.iter().find(|a| a.path == "drop.txt").unwrap();
    assert!(drop_record.approved_tree_hash.is_none(), "rejected 文件应无 approved_tree_hash");

    let _ = std::fs::remove_dir_all(&cwd);
}

/// H8：reject 已有文件 → action=restored（文件回退旧内容，不是删除）（文档 V4）
#[tokio::test]
async fn h8_reject_existing_file_restores_content() {
    let cwd = tmp_cwd("h8");
    let abs_path = format!("{}/existing.txt", cwd.trim_end_matches('/'));

    // 先在 cwd 放一个已有文件（session_start 会捕获进 baseline tree）
    std::fs::write(&abs_path, "original content").unwrap();

    let (mut agent, _store, mgr) = build_agent(
        &cwd,
        vec![
            // agent write 覆盖已有文件（从 "original content" → "modified by agent"）
            write_call(&cwd, "existing.txt", "modified by agent"),
            stop_msg(),
            stop_msg(),
        ],
    );

    agent.run("overwrite existing.txt").await.unwrap();

    // 确认 agent 改了文件
    let content_after_write = std::fs::read_to_string(&abs_path).unwrap();
    assert_eq!(content_after_write, "modified by agent", "agent write 后内容应改变");

    // reject → 应走 restored 分支（baseline tree 有该文件）
    let result = mgr.reject("existing.txt").unwrap();
    assert_eq!(
        result.action, "restored",
        "reject 已有文件应 action=restored（不是 deleted）。实际 action: {}",
        result.action
    );

    // 文件仍存在，内容恢复成原始
    assert!(std::path::Path::new(&abs_path).exists(), "restored 后文件应仍存在");
    let content_after_reject = std::fs::read_to_string(&abs_path).unwrap();
    assert_eq!(
        content_after_reject, "original content",
        "reject 后内容应恢复成原始。实际: {}", content_after_reject
    );

    // 状态变 rejected
    let rejected = mgr.approvals_list(Some(&ApprovalStatus::Rejected));
    assert!(rejected.iter().any(|a| a.path == "existing.txt"));

    let _ = std::fs::remove_dir_all(&cwd);
}

/// H9：re-approval 重置（已批准文件再改 → 自动回 pending，baseline 锚定保留）（文档 L2）
#[tokio::test]
async fn h9_re_approval_resets_to_pending_keeps_baseline() {
    let cwd = tmp_cwd("h9");

    let (mut agent, _store, mgr) = build_agent(
        &cwd,
        vec![
            write_call(&cwd, "a.txt", "v1"),
            stop_msg(),
            stop_msg(),
        ],
    );

    agent.run("write a.txt").await.unwrap();

    // approve → 记录 baseline tree hash
    mgr.approve("a.txt").unwrap();
    let approved_record = mgr.approvals_list(Some(&ApprovalStatus::Approved))
        .into_iter()
        .find(|a| a.path == "a.txt")
        .expect("approve 后应有 approved 记录");
    let baseline_hash = approved_record.approved_tree_hash.clone()
        .expect("approved 文件应有 baseline tree hash");

    // 模拟 on_turn_end 检测到同文件再改 → check_re_approval
    mgr.check_re_approval(&["a.txt".to_string()]);

    // 状态应重置回 pending
    let pending = mgr.approvals_list(Some(&ApprovalStatus::Pending));
    assert!(
        pending.iter().any(|a| a.path == "a.txt"),
        "re-approval 后 a.txt 应回 pending。实际 pending: {:?}", pending
    );

    // baseline tree hash 应保留（锚定不丢）
    let pending_record = pending.into_iter().find(|a| a.path == "a.txt").unwrap();
    assert_eq!(
        pending_record.approved_tree_hash,
        Some(baseline_hash),
        "re-approval 重置后 approved_tree_hash 应保留（baseline 锚定不丢）"
    );

    let _ = std::fs::remove_dir_all(&cwd);
}

/// E1：真实 LLM 审批闭环（标 #[ignore]）
///
/// **已由 CI Group L 覆盖**（tests/file_snapshot_ci.sh 的 Group L）。
/// Group L 用 `ion serve` + 真实 host 走完整 RPC 链路，比 harness 更真实。
///
/// 运行方式：
/// ```bash
/// ION_E2E=1 bash tests/file_snapshot_ci.sh   # 跑 Group L（L1-L5）
/// ```
///
/// 这个 Rust 测试保留作为占位，未来如果需要在 harness 层（不走 host）
/// 验证真实 LLM，可以在这里实现。当前 Group L 已足够。
#[tokio::test]
#[ignore]
async fn e1_real_agent_approval_workflow() {
    // 已由 CI Group L 覆盖（tests/file_snapshot_ci.sh）
    // ION_E2E=1 bash tests/file_snapshot_ci.sh → 跑 L1-L5
}
