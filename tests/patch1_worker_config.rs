//! 补丁 1（HOOKS_AND_OUTLINE_SYNC）：create_worker 能力补齐测试
//!
//! 验证：
//! 1. ExtensionWorkerConfig 新增字段（agent/initial_prompt/allowed_tools/disallowed_tools/max_turns）能序列化
//! 2. ExtensionWorkerConfig → JSON → WorkerCreateConfig 的字段透传完整
//! 3. WorkerCreateConfig 带新字段能正确反序列化
//!
//! 端到端验证（子 Worker 工具真的被限制）留给 hooks_ci.sh CLI harness。

use ion::worker_api::ExtensionWorkerConfig;
use ion::worker_registry::WorkerCreateConfig;

/// ExtensionWorkerConfig 新增字段能正确序列化（补丁 1 核心）
#[test]
fn t01_extension_worker_config_new_fields_serialize() {
    let cfg = ExtensionWorkerConfig {
        session: Some("sess_test".into()),
        model: Some("fast".into()),
        provider: None,
        channels: Some(vec!["main".into()]),
        parent: Some("wkr_parent".into()),
        // 补丁 1 新增字段
        agent: Some("outline-syncer".into()),
        initial_prompt: Some("扫描 docs/ 同步大纲".into()),
        worktree: None,
        relation: Some("child".into()),
        allowed_tools: Some(vec!["read".into(), "write".into(), "edit".into(), "bash".into()]),
        disallowed_tools: Some(vec!["spawn_worker".into()]),
        max_turns: Some(100),
    };

    let json = serde_json::to_value(&cfg).expect("serialize");
    assert_eq!(json["agent"], "outline-syncer");
    assert_eq!(json["initial_prompt"], "扫描 docs/ 同步大纲");
    assert_eq!(json["relation"], "child");
    assert_eq!(json["max_turns"], 100);

    let allowed = json["allowed_tools"].as_array().expect("allowed_tools is array");
    assert_eq!(allowed.len(), 4);
    assert_eq!(allowed[0], "read");

    let disallowed = json["disallowed_tools"].as_array().expect("disallowed_tools is array");
    assert_eq!(disallowed.len(), 1);
    assert_eq!(disallowed[0], "spawn_worker");
}

/// ExtensionWorkerConfig → JSON → WorkerCreateConfig 字段透传完整
/// （模拟 create_worker 实现里的 bridge.send_command 透传链路）
#[test]
fn t02_extension_config_to_worker_config_passthrough() {
    let ext_cfg = ExtensionWorkerConfig {
        session: Some("sess_passthrough".into()),
        model: Some("pro".into()),
        provider: Some("anthropic".into()),
        channels: None,
        parent: Some("wkr_root".into()),
        agent: Some("reviewer".into()),
        initial_prompt: Some("审查代码".into()),
        worktree: None,
        relation: Some("peer".into()),
        allowed_tools: Some(vec!["read".into(), "grep".into()]),
        disallowed_tools: None,
        max_turns: Some(50),
    };

    // 模拟 create_worker 实现里的 serde_json::json!({...}) 透传
    let params = serde_json::json!({
        "session": ext_cfg.session,
        "model": ext_cfg.model,
        "provider": ext_cfg.provider,
        "channels": ext_cfg.channels,
        "parent": ext_cfg.parent,
        "agent": ext_cfg.agent,
        "initial_prompt": ext_cfg.initial_prompt,
        "worktree": ext_cfg.worktree,
        "relation": ext_cfg.relation,
        "allowed_tools": ext_cfg.allowed_tools,
        "disallowed_tools": ext_cfg.disallowed_tools,
        "max_turns": ext_cfg.max_turns,
    });

    // Manager 端反序列化（serde_json::from_value）
    let worker_cfg: WorkerCreateConfig = serde_json::from_value(params).expect("deserialize to WorkerCreateConfig");

    assert_eq!(worker_cfg.session.as_deref(), Some("sess_passthrough"));
    assert_eq!(worker_cfg.model.as_deref(), Some("pro"));
    assert_eq!(worker_cfg.provider.as_deref(), Some("anthropic"));
    assert_eq!(worker_cfg.parent.as_deref(), Some("wkr_root"));
    assert_eq!(worker_cfg.agent.as_deref(), Some("reviewer"));
    assert_eq!(worker_cfg.initial_prompt.as_deref(), Some("审查代码"));
    assert_eq!(worker_cfg.allowed_tools, Some(vec!["read".into(), "grep".into()]));
    assert_eq!(worker_cfg.max_turns, Some(50));
}

/// WorkerCreateConfig 新字段默认值为 None（向后兼容）
#[test]
fn t03_worker_create_config_defaults_none() {
    // 不带新字段的老配置（向后兼容场景）
    let json = serde_json::json!({
        "session": "sess_old",
        "model": "fast",
    });

    let cfg: WorkerCreateConfig = serde_json::from_value(json).expect("deserialize");
    assert_eq!(cfg.session.as_deref(), Some("sess_old"));
    assert_eq!(cfg.allowed_tools, None, "allowed_tools defaults to None");
    assert_eq!(cfg.disallowed_tools, None, "disallowed_tools defaults to None");
    assert_eq!(cfg.max_turns, None, "max_turns defaults to None");
    assert_eq!(cfg.agent, None, "agent defaults to None");
}

/// max_turns=0 的边界（0 表示无限，文档里约定）
#[test]
fn t04_max_turns_zero_means_unlimited() {
    let cfg = ExtensionWorkerConfig {
        max_turns: Some(0),
        ..Default::default()
    };
    let json = serde_json::to_value(&cfg).expect("serialize");
    assert_eq!(json["max_turns"], 0);

    // ion_worker 里 max_turns=0 会被映射成 None（无限）
    // 这里只验证序列化值正确传递，映射逻辑在 ion_worker 里测
}

/// allowed_tools 空数组 = 不限制（等价 None）
#[test]
fn t05_empty_allowed_tools_means_all() {
    let cfg = ExtensionWorkerConfig {
        allowed_tools: Some(vec![]),
        ..Default::default()
    };
    let json = serde_json::to_value(&cfg).expect("serialize");
    assert!(json["allowed_tools"].as_array().unwrap().is_empty());
}
