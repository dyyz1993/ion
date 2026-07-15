//! Hooks 内核引擎集成测试（给内核开发者，不是给用户）
//!
//! 这些测试验证 Rust 内核的 HooksConfig / handler_runner / matcher API 不被改坏。
//! 它们是 `cargo test` 的一部分，和 lib tests 里的其他单元测试性质相同。
//!
//! 用户验证自己的 hooks.json 配置请用 `scripts/hooks_test.sh`（纯 bash，不写 Rust）。
//!
//! 验证链路：HooksConfig::load_fresh → handlers_for_event → matcher → run_handler(command) → interpret → HookOutcome

use ion::hooks::handler_runner::{self, HookExecContext};
use ion::hooks::matcher;
use ion::hooks::HooksConfig;
use std::path::PathBuf;
use std::sync::Arc;

/// 创建临时测试目录 + hooks.json + 脚本
fn setup_test_dir() -> PathBuf {
    let dir = PathBuf::from("/tmp/ion_hooks_e2e_test");
    let ion_dir = dir.join(".ion");
    let scripts_dir = ion_dir.join("scripts");
    std::fs::create_dir_all(&scripts_dir);

    // hooks.json：PreToolUse + matcher=bash + command handler
    std::fs::write(ion_dir.join("hooks.json"), r#"{
  "version": 1,
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "bash",
        "hooks": [
          {
            "type": "command",
            "command": "bash .ion/scripts/block_no_verify.sh",
            "timeout": 5
          }
        ]
      }
    ],
    "UserPromptSubmit": [
      {
        "type": "command",
        "command": "echo '项目约定：用 Rust 写代码'",
        "timeout": 3
      }
    ]
  }
}"#).unwrap();

    // 拦截脚本
    std::fs::write(scripts_dir.join("block_no_verify.sh"), r#"#!/bin/bash
set -euo pipefail
INPUT=$(cat)
COMMAND=$(echo "$INPUT" | jq -r '.tool_input.command // ""' 2>/dev/null || echo "$INPUT" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d.get('tool_input',{}).get('command',''))" 2>/dev/null || echo "")
if echo "$COMMAND" | grep -qi "git.*--no-verify"; then
    echo '{"decision":"block","reason":"禁止使用 --no-verify"}'
    exit 2
fi
exit 0
"#).unwrap();
    // 设权限
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(scripts_dir.join("block_no_verify.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    dir
}

#[tokio::test]
async fn e2e_config_loads_from_project_dir() {
    let dir = setup_test_dir();
    let config = HooksConfig::load_fresh(Some(&dir));

    assert!(!config.is_empty(), "应该加载到 hooks");
    assert!(!config.disable_all_hooks);
    assert_eq!(config.event_count(), 2, "应该有 2 个事件");
    assert!(config.hooks.contains_key("PreToolUse"));
    assert!(config.hooks.contains_key("UserPromptSubmit"));
}

#[tokio::test]
async fn e2e_handler_count() {
    let dir = setup_test_dir();
    let config = HooksConfig::load_fresh(Some(&dir));
    assert_eq!(config.handler_count(), 2, "PreToolUse 1 + UserPromptSubmit 1");
}

#[tokio::test]
async fn e2e_command_handler_blocks_no_verify() {
    let dir = setup_test_dir();
    let config = HooksConfig::load_fresh(Some(&dir));

    // 找到 PreToolUse 的 handler
    let handlers = config.handlers_for_event("PreToolUse");
    assert_eq!(handlers.len(), 1);
    let matcher_str = handlers[0].0;
    let handler = handlers[0].1;
    assert_eq!(matcher_str, Some("bash"));

    // matcher 应该匹配 bash
    assert!(matcher::matches_matcher(matcher_str, "bash"));

    // 模拟 --no-verify 的 stdin
    let stdin = serde_json::json!({
        "tool_name": "bash",
        "tool_input": {"command": "git commit --no-verify -m test"},
        "tool_use_id": "test-001",
        "session_id": "sess_test",
        "cwd": dir.to_string_lossy().to_string(),
        "hook_event_name": "PreToolUse",
    });

    // 在测试目录里执行（脚本用相对路径）
    std::env::set_current_dir(&dir).ok();
    let ctx = HookExecContext {
        project_dir: dir.to_string_lossy().to_string(),
        event_name: "PreToolUse".into(),
        runtime: None, // 用 spawn fallback
        registry: None,
        model: None,
    };

    let outcome = handler_runner::run_handler(handler, stdin, &ctx).await;

    // ⭐ 核心验证：应该 block
    assert!(outcome.block, "git --no-verify 应该被拦截");
    assert_eq!(
        outcome.block_reason.as_deref(),
        Some("禁止使用 --no-verify"),
        "block reason 应该来自脚本的 JSON"
    );
}

#[tokio::test]
async fn e2e_command_handler_allows_normal_commit() {
    let dir = setup_test_dir();
    let config = HooksConfig::load_fresh(Some(&dir));
    let handlers = config.handlers_for_event("PreToolUse");
    let handler = handlers[0].1;

    // 正常的 git commit（不带 --no-verify）
    let stdin = serde_json::json!({
        "tool_name": "bash",
        "tool_input": {"command": "git commit -m test"},
        "session_id": "sess_test",
        "cwd": dir.to_string_lossy().to_string(),
        "hook_event_name": "PreToolUse",
    });

    std::env::set_current_dir(&dir).ok();
    let ctx = HookExecContext {
        project_dir: dir.to_string_lossy().to_string(),
        event_name: "PreToolUse".into(),
        runtime: None,
            registry: None,
            model: None,
    };

    let outcome = handler_runner::run_handler(handler, stdin, &ctx).await;

    // ⭐ 核心验证：不应该 block
    assert!(!outcome.block, "正常 git commit 不应该被拦截");
}

#[tokio::test]
async fn e2e_user_prompt_submit_injects_context() {
    let dir = setup_test_dir();
    let config = HooksConfig::load_fresh(Some(&dir));
    let handlers = config.handlers_for_event("UserPromptSubmit");
    assert_eq!(handlers.len(), 1);
    let handler = handlers[0].1;

    let stdin = serde_json::json!({
        "prompt": "帮我写个函数",
        "session_id": "sess_test",
        "cwd": dir.to_string_lossy().to_string(),
        "hook_event_name": "UserPromptSubmit",
    });

    std::env::set_current_dir(&dir).ok();
    let ctx = HookExecContext {
        project_dir: dir.to_string_lossy().to_string(),
        event_name: "UserPromptSubmit".into(),
        runtime: None,
            registry: None,
            model: None,
    };

    let outcome = handler_runner::run_handler(handler, stdin, &ctx).await;

    // ⭐ 核心验证：不 block，但应该有 additionalContext（echo 的输出）
    assert!(!outcome.block, "UserPromptSubmit 不应该 block");
    assert!(
        outcome.additional_context.as_deref().unwrap_or("").contains("项目约定"),
        "应该注入 echo 的输出作为 additionalContext，实际: {:?}",
        outcome.additional_context
    );
}

#[tokio::test]
async fn e2e_hot_reload_picks_up_changes() {
    let dir = setup_test_dir();

    // 第一次读
    let config1 = HooksConfig::load_fresh(Some(&dir));
    assert_eq!(config1.event_count(), 2);

    // 修改 hooks.json，加一个 Stop 事件
    std::fs::write(dir.join(".ion").join("hooks.json"), r#"{
  "version": 1,
  "hooks": {
    "PreToolUse": [{"matcher":"bash","hooks":[{"type":"command","command":"echo hi"}]}],
    "Stop": [{"type":"command","command":"echo bye"}]
  }
}"#).unwrap();

    // 第二次读——不用重启，load_fresh 重新读文件
    let config2 = HooksConfig::load_fresh(Some(&dir));
    assert_eq!(config2.event_count(), 2, "改完应该立即反映");
    assert!(config2.hooks.contains_key("Stop"), "新加的 Stop 应该出现");
    assert!(config2.hooks.contains_key("PreToolUse"), "PreToolUse 还在");
}

// ── B.3 验证：Stop 事件 block + loop_limit ──

/// Stop 事件：脚本 exit 2 → block + reason（模拟测试失败）
#[tokio::test]
async fn e2e_stop_event_blocks_with_reason() {
    let dir = PathBuf::from("/tmp/ion_hooks_b3_test");
    let ion_dir = dir.join(".ion");
    let scripts_dir = ion_dir.join("scripts");
    std::fs::create_dir_all(&scripts_dir);

    // Stop 事件 + loop_limit=3
    std::fs::write(ion_dir.join("hooks.json"), r#"{
  "version": 1,
  "hooks": {
    "Stop": [
      {
        "loop_limit": 3,
        "hooks": [
          {"type":"command","command":"bash .ion/scripts/check_tests.sh","timeout":60}
        ]
      }
    ]
  }
}"#).unwrap();

    // 模拟测试失败的脚本（FAIL 文件存在 = 失败）
    std::fs::write(scripts_dir.join("check_tests.sh"), r#"#!/bin/bash
if [ -f .ion/FAIL ]; then
    echo '{"decision":"block","reason":"测试失败，请修复"}'
    exit 2
fi
exit 0
"#).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(scripts_dir.join("check_tests.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    // 创建 FAIL 文件模拟测试失败
    std::fs::write(ion_dir.join("FAIL"), "1").unwrap();

    let config = HooksConfig::load_fresh(Some(&dir));
    let handlers = config.handlers_for_event("Stop");
    assert_eq!(handlers.len(), 1);
    let handler = handlers[0].1;

    let stdin = serde_json::json!({
        "last_assistant_message": "我做完了",
        "session_id": "sess_b3",
        "cwd": dir.to_string_lossy().to_string(),
        "hook_event_name": "Stop",
    });

    std::env::set_current_dir(&dir).ok();
    let ctx = HookExecContext {
        project_dir: dir.to_string_lossy().to_string(),
        event_name: "Stop".into(),
        runtime: None,
            registry: None,
            model: None,
    };

    let outcome = handler_runner::run_handler(handler, stdin, &ctx).await;

    // ⭐ 核心验证：Stop 被 block，reason 来自脚本
    assert!(outcome.block, "Stop 应该被 block（测试失败）");
    assert_eq!(
        outcome.block_reason.as_deref(),
        Some("测试失败，请修复"),
        "reason 应该来自脚本的 JSON"
    );

    // 清理
    std::fs::remove_file(ion_dir.join("FAIL")).ok();
}

/// Stop 事件：测试通过时不 block（exit 0）
#[tokio::test]
async fn e2e_stop_event_passes_when_tests_ok() {
    let dir = PathBuf::from("/tmp/ion_hooks_b3_pass");
    let ion_dir = dir.join(".ion");
    let scripts_dir = ion_dir.join("scripts");
    std::fs::create_dir_all(&scripts_dir);

    std::fs::write(ion_dir.join("hooks.json"), r#"{
  "version": 1,
  "hooks": {
    "Stop": [{"loop_limit":3,"hooks":[{"type":"command","command":"bash .ion/scripts/check_tests.sh","timeout":60}]}]
  }
}"#).unwrap();

    // 同样的脚本，但不创建 FAIL 文件（测试通过）
    std::fs::write(scripts_dir.join("check_tests.sh"), r#"#!/bin/bash
if [ -f .ion/FAIL ]; then
    echo '{"decision":"block","reason":"测试失败"}'
    exit 2
fi
exit 0
"#).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(scripts_dir.join("check_tests.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    // 不创建 FAIL 文件

    let config = HooksConfig::load_fresh(Some(&dir));
    let handler = config.handlers_for_event("Stop")[0].1;

    let stdin = serde_json::json!({"last_assistant_message":"done","session_id":"sess","cwd":dir.to_string_lossy().to_string(),"hook_event_name":"Stop"});
    std::env::set_current_dir(&dir).ok();
    let ctx = HookExecContext { project_dir: dir.to_string_lossy().to_string(), event_name: "Stop".into(), runtime: None, registry: None, model: None };

    let outcome = handler_runner::run_handler(handler, stdin, &ctx).await;
    assert!(!outcome.block, "测试通过时 Stop 不应该 block");
}

// ── agent handler 验证 ──

/// agent handler：有 runtime 时不会 panic（即使 spawn 失败也优雅降级）
///
/// LocalRuntime 的 spawn_worker 默认返回 Err（单进程不支持多 Worker），
/// 所以这个测试验证的是"agent handler 被调用 → spawn 失败 → 不 panic → 返回默认 outcome"。
/// 真正的 spawn 成功测试需要 WorkerRuntime + ManagerBridge（host 级集成测试）。
#[tokio::test]
async fn e2e_agent_handler_with_runtime_does_not_panic() {
    use ion::hooks::{HandlerType, HookHandler};

    let handler = HookHandler {
        handler_type: HandlerType::Agent,
        command: None, url: None,
        prompt: Some("读文件并报告".into()),
        agent: Some("default".into()),
        server: None, tool: None,
        input: None, model: None, timeout: Some(10),
        if_clause: None,
        r#async: false, async_rewake: false, once: false,
        status_message: None,
        allowed_tools: Some(vec!["read".into()]),
        max_turns: Some(5),
    };

    let stdin = serde_json::json!({
        "session_id": "sess_agent_test",
        "hook_event_name": "SubagentStop",
        "last_assistant_message": "做完了",
    });

    // 用 LocalRuntime（spawn_worker 返回 Err）
    let rt: Arc<dyn ion::runtime::Runtime> = Arc::new(ion::runtime::LocalRuntime::new());
    let ctx = HookExecContext {
        project_dir: "/tmp".into(),
        event_name: "SubagentStop".into(),
        runtime: Some(rt),
            registry: None,
            model: None,
    };

    // ⭐ 核心验证：不 panic，返回默认 outcome（不 block）
    let outcome = handler_runner::run_handler(&handler, stdin, &ctx).await;
    assert!(!outcome.block, "agent handler spawn 失败时不应 block 主流程");
    // LocalRuntime 的 spawn_worker 返回 Err，所以 outcome 是默认空值
    assert!(outcome.additional_context.is_none(), "spawn 失败不应有 additionalContext");
}

/// agent handler：没配 prompt 时返回默认值（不 panic）
#[tokio::test]
async fn e2e_agent_handler_no_prompt_returns_default() {
    use ion::hooks::{HandlerType, HookHandler};

    let handler = HookHandler {
        handler_type: HandlerType::Agent,
        command: None, url: None,
        prompt: None, // 没配 prompt
        agent: None,
        server: None, tool: None,
        input: None, model: None, timeout: None,
        if_clause: None,
        r#async: false, async_rewake: false, once: false,
        status_message: None, allowed_tools: None, max_turns: None,
    };

    let rt: Arc<dyn ion::runtime::Runtime> = Arc::new(ion::runtime::LocalRuntime::new());
    let ctx = HookExecContext {
        project_dir: "/tmp".into(),
        event_name: "Stop".into(),
        runtime: Some(rt),
            registry: None,
            model: None,
    };

    let outcome = handler_runner::run_handler(&handler, serde_json::json!({}), &ctx).await;
    assert!(!outcome.block, "没配 prompt 的 agent handler 不应 block");
}
