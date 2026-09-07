// goal_restore_ci.rs — T05：Goal 状态经会话 JSONL 往返 + 恢复取最后一条
//
// 覆盖：
//   R1 persist→restore 全字段往返（objective/status/iteration/started_at/goal_id）
//   R2 多次快照后 restore 必须取最后一条（迭代语义连续）
//   R3 索引小摘要（goal_status）随 persist 写入 SessionIndex（HOME 沙箱验证）
//
// 单一 #[test] 顺序执行：set_session_file_override 与 HOME 都是进程级状态。
use ion::goal_supervisor_extension::{
    GoalPlan, GoalState, GoalStatus, persist_goal_state, restore_goal_state,
};

fn sample_goal(iteration_count: u32, status: GoalStatus) -> GoalState {
    GoalState {
        goal_id: "goal-rt-0001".into(),
        objective: "make CI honest".into(),
        checks: vec![],
        status,
        iteration_count,
        started_at: "epoch:1788810000".into(),
        total_cost_usd: 0.25,
        last_seen_tokens: Some((100, 200)),
        budget_valid: true,
        cost_basis: Some("faux in=1.0/1M out=2.0/1M".into()),
        last_action_plan: Some("fix aggregator".into()),
        recent_tools: vec![("edit".into(), "scripts/aggregate_ci_results.sh".into())],
        goal_plan: GoalPlan::default(),
    }
}

#[test]
fn goal_state_roundtrips_and_restores_last() {
    // HOME 沙箱：persist 会顺带 patch_meta 写 SessionIndex（按 HOME 落位）
    let orig_home = std::env::var("HOME").unwrap_or_default();
    let sandbox = std::env::temp_dir().join(format!("ion-goal-rt-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&sandbox);
    std::fs::create_dir_all(sandbox.join(".ion/agent")).unwrap();
    // SAFETY: 本测试二进制进程内无并发线程访问 HOME
    unsafe { std::env::set_var("HOME", &sandbox) };

    let session = sandbox.join("sess_goalrt.jsonl");
    std::fs::write(
        &session,
        "{\"type\":\"session\",\"id\":\"sess_goalrt\",\"cwd\":\"/tmp\"}\n",
    )
    .unwrap();
    ion::session_jsonl::set_session_file_override(Some(session.clone()));

    // R1：首条快照
    let g1 = sample_goal(3, GoalStatus::Running);
    persist_goal_state(&g1);
    // R2：迭代后第二条快照（模拟 worker 中断前的最后一次 journal）
    let g2 = sample_goal(7, GoalStatus::Exhausted);
    persist_goal_state(&g2);

    let restored = restore_goal_state("/any/cwd").expect("restore must find goal_state");
    assert_eq!(restored.goal_id, g1.goal_id, "R1 goal_id");
    assert_eq!(restored.objective, g1.objective, "R1 objective");
    assert_eq!(
        restored.started_at, g1.started_at,
        "R1 started_at（截止一致性来源）"
    );
    assert_eq!(restored.total_cost_usd, g1.total_cost_usd, "R1 cost");
    assert_eq!(restored.recent_tools.len(), 1, "R1 recent_tools");
    assert_eq!(
        restored.iteration_count, 7,
        "R2 must restore the LAST snapshot"
    );
    assert_eq!(
        restored.last_seen_tokens,
        Some((100, 200)),
        "R2 T04 last_seen_tokens"
    );
    assert!(restored.budget_valid, "R2 T04 budget_valid");
    assert!(restored.cost_basis.is_some(), "R2 T04 cost_basis");
    assert!(
        matches!(restored.status, GoalStatus::Exhausted),
        "R2 status from last snapshot"
    );

    // R3：索引小摘要
    let idx = ion::session_index::SessionIndex::load();
    assert_eq!(
        idx.get("sess_goalrt")
            .and_then(|m| m.goal_status.clone())
            .as_deref(),
        Some("Exhausted"),
        "R3 index goal_status summary"
    );

    ion::session_jsonl::set_session_file_override(None);
    // SAFETY: 同上
    unsafe { std::env::set_var("HOME", &orig_home) };
    let _ = std::fs::remove_dir_all(&sandbox);
}
