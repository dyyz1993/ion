// goal_cost_ci.rs — T04：goal 费用防线真实接线验证
//
// 通过真实 SessionIndex（HOME 沙箱）注入 token 计量，验证 sync_cost_from_index：
//   C1 首次结算 = 基线，不计费
//   C2 差分计费：Δin×单价in + Δout×单价out（重试产生的 token 同样在索引里，天然计入）
//   C3 无新增时重复结算不重复计费（单调差分防重复）
//   C4 索引 token 回落（重启/GC 场景）→ 差分为 0，不倒扣
//   C5 budget_valid 且费用达限 → check_guards 触发 max_cost
//   C6 无单价 → budget_valid 降级为 false，且 max_cost 不误杀
//
// 单一 #[test] 顺序执行（HOME 是进程级状态）。
use ion::goal_supervisor_extension::{
    GoalPricing, GoalState, GoalStatus, GoalSupervisorConfig, GoalSupervisorExtension,
};

fn mode(m: u32) -> std::fs::Permissions {
    use std::os::unix::fs::PermissionsExt;
    std::fs::Permissions::from_mode(m)
}

fn session_meta() -> ion::session_index::SessionMeta {
    ion::session_index::SessionMeta {
        name: None,
        first_name: None,
        project: None,
        project_name: None,
        worktree: false,
        branch: None,
        model: "faux".into(),
        agent: "default".into(),
        provider: "faux".into(),
        token_input: 0,
        token_output: 0,
        token_cache_read: 0,
        token_cache_write: 0,
        user_prompt_count: 0,
        llm_request_count: 0,
        total_duration_ms: 0,
        compress_count: 0,
        message_count: 0,
        turn_count: 0,
        created_at: 1,
        updated_at: 1,
        error_count: 0,
        last_thinking_level: None,
        last_active_tools: None,
        last_entry_id: None,
        parent_session: None,
        parent_type: None,
        initial_cwd: None,
        last_cwd: None,
        extra_cwds: Vec::new(),
        tier_models: None,
        security_profile: None,
        workspace_path: None,
        workspace_status: None,
        goal_status: None,
        goal_deadline_ms: None,
    }
}

/// 会话累计 token 注入（模拟 agent_loop 的 increment_turn_stats 累计口径）
fn set_session_tokens(tok_in: u64, tok_out: u64) {
    ion::session_index::SessionIndex::write_txn(|idx| {
        idx.upsert("sess_cost", session_meta());
        if let Some(m) = idx.sessions.get_mut("sess_cost") {
            m.token_input = tok_in;
            m.token_output = tok_out;
        }
    });
}

fn goal_state(budget_valid: bool) -> GoalState {
    GoalState {
        goal_id: "goal-cost-1".into(),
        objective: "test cost wiring".into(),
        checks: vec![],
        status: GoalStatus::Running,
        iteration_count: 1,
        // 用当前时间，避免 max_duration 守卫先于被测的 max_cost 触发
        started_at: format!(
            "epoch:{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs()
        ),
        total_cost_usd: 0.0,
        last_seen_tokens: None,
        budget_valid,
        cost_basis: None,
        last_action_plan: None,
        recent_tools: vec![],
        goal_plan: Default::default(),
    }
}

fn cost_of(ext: &GoalSupervisorExtension) -> f64 {
    ext.state
        .lock()
        .unwrap()
        .as_ref()
        .map(|s| s.total_cost_usd)
        .unwrap_or(f64::NAN)
}

fn budget_valid_of(ext: &GoalSupervisorExtension) -> bool {
    ext.state.lock().unwrap().as_ref().unwrap().budget_valid
}

#[test]
fn goal_cost_accrues_from_index_tokens() {
    let orig_home = std::env::var("HOME").unwrap_or_default();
    let sandbox = std::env::temp_dir().join(format!("ion-goal-cost-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&sandbox);
    std::fs::create_dir_all(sandbox.join(".ion/agent")).unwrap();
    // SAFETY: 本测试二进制内无并发线程访问 HOME
    unsafe { std::env::set_var("HOME", &sandbox) };

    let shared: ion::goal_supervisor_extension::SharedGoalState =
        std::sync::Arc::new(std::sync::Mutex::new(Some(goal_state(true))));
    let ext = GoalSupervisorExtension::new()
        .with_shared_state(shared.clone())
        .with_session_id("sess_cost")
        .with_pricing("faux", 1.0, 2.0) // USD / 1M tokens
        .with_config(GoalSupervisorConfig {
            max_total_cost_usd: 10.0,
            ..Default::default()
        });

    // C1：基线（目标设定前已有 1M/2M token 使用）→ 首次结算不计费
    set_session_tokens(1_000_000, 2_000_000);
    ext.sync_cost_from_index();
    assert!(
        (cost_of(&ext) - 0.0).abs() < 1e-9,
        "C1 baseline not charged"
    );
    assert!(budget_valid_of(&ext), "C1 budget stays valid with pricing");
    assert!(
        shared
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .cost_basis
            .is_some(),
        "C1 cost_basis recorded"
    );

    // C2：新增 2M in / 3M out（含一次重试的 token）→ 2*1.0 + 3*2.0 = 8.0
    set_session_tokens(3_000_000, 5_000_000);
    ext.sync_cost_from_index();
    assert!(
        (cost_of(&ext) - 8.0).abs() < 1e-9,
        "C2 diff pricing: got {}",
        cost_of(&ext)
    );

    // C3：无新增重复结算 → 不重复计费
    ext.sync_cost_from_index();
    assert!((cost_of(&ext) - 8.0).abs() < 1e-9, "C3 no double charge");

    // C4：索引回落（会话重建/GC）→ 差分 0，不倒扣
    set_session_tokens(100, 100);
    ext.sync_cost_from_index();
    assert!(
        (cost_of(&ext) - 8.0).abs() < 1e-9,
        "C4 no clawback on index reset"
    );

    // C5：费用已达上限 → max_cost 守卫触发（先把 token 恢复高位并再结算一点）
    set_session_tokens(4_000_000, 5_000_000);
    ext.sync_cost_from_index(); // +1.0 → 9.0；再把上限压到 9.0 之下
    {
        let mut guard = shared.lock().unwrap();
        guard.as_mut().unwrap().total_cost_usd = 10.5; // 越过上限 10.0
    }
    let tripped = ext.check_guards(None);
    assert_eq!(
        tripped.as_deref(),
        Some("max_cost"),
        "C5 guard trips over budget"
    );

    // C6：无单价扩展 → budget_valid 降级 false，且超限也不误杀
    let shared2: ion::goal_supervisor_extension::SharedGoalState =
        std::sync::Arc::new(std::sync::Mutex::new(Some(goal_state(true))));
    let ext_noprice = GoalSupervisorExtension::new()
        .with_shared_state(shared2.clone())
        .with_session_id("sess_cost")
        .with_config(GoalSupervisorConfig {
            max_total_cost_usd: 0.001,
            ..Default::default()
        });
    ext_noprice.sync_cost_from_index();
    assert!(
        !budget_valid_of(&ext_noprice),
        "C6 budget degrades without pricing"
    );
    {
        let mut guard = shared2.lock().unwrap();
        guard.as_mut().unwrap().total_cost_usd = 999.0; // 远超上限
    }
    let no_trip = ext_noprice.check_guards(None);
    assert_ne!(
        no_trip.as_deref(),
        Some("max_cost"),
        "C6 no false kill without pricing"
    );

    let _ = std::fs::set_permissions(sandbox.join(".ion/agent"), mode(0o755));
    let _ = std::fs::remove_dir_all(&sandbox);
    // SAFETY: 同上
    unsafe { std::env::set_var("HOME", &orig_home) };
    let _ = GoalPricing {
        model_id: String::new(),
        input_per_1m: 0.0,
        output_per_1m: 0.0,
    }; // 保持类型导入使用
}
