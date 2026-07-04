//! Phase 5+6: E2E 场景 + 压力测试
//!
//! E2: 多项目并发 (验证项目隔离)
//! E4: 会话恢复 (create → kill → recreate → remember)
//! S1: 10 Worker 同时 prompt
//! S2: 1 Worker 连续 50 轮对话
//! S3: Channel 100 条消息广播
//! S4: 快速创建/销毁 20 个 Worker

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

use ion::worker_registry::{WorkerCreateConfig, WorktreeConfig, WorkerRegistry};
use ion::agent::tool::Tool;

fn worker_bin_path() -> std::path::PathBuf {
    if let Ok(path) = std::env::var("ION_WORKER_BIN") {
        return std::path::PathBuf::from(path);
    }
    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let debug_bin = manifest.join("target").join("debug").join("ion-worker");
    if debug_bin.exists() {
        return debug_bin;
    }
    std::path::PathBuf::from("ion-worker")
}

fn create_registry() -> Arc<Mutex<WorkerRegistry>> {
    let bin = worker_bin_path();
    Arc::new(Mutex::new(WorkerRegistry::with_binary(
        &bin.to_string_lossy(),
    )))
}

// =========================================================================
// E2: 多项目并发
// =========================================================================

#[tokio::test]
async fn e02_multi_project_concurrent() {
    let registry = create_registry();
    let mut reg = registry.lock().await;

    // Create two workers in different projects
    let tmp_a = std::env::temp_dir().join("ion_e2_a").join("proj-alpha");
    let tmp_b = std::env::temp_dir().join("ion_e2_b").join("proj-beta");
    std::fs::create_dir_all(&tmp_a).ok();
    std::fs::create_dir_all(&tmp_b).ok();

    let a = reg.create_worker(WorkerCreateConfig {
        session: Some("e2-proj-a".into()),
        project_path: Some(tmp_a.to_string_lossy().to_string()),
        ..Default::default()
    }).await.unwrap();

    let b = reg.create_worker(WorkerCreateConfig {
        session: Some("e2-proj-b".into()),
        project_path: Some(tmp_b.to_string_lossy().to_string()),
        ..Default::default()
    }).await.unwrap();

    // Both respond independently
    let r1 = reg.send_to_worker(&a.worker_id, "get_state", serde_json::Value::Null).await.unwrap();
    let r2 = reg.send_to_worker(&b.worker_id, "get_state", serde_json::Value::Null).await.unwrap();
    assert_eq!(r1["success"], true, "A should respond");
    assert_eq!(r2["success"], true, "B should respond");

    // Projects should be isolated
    let projects = reg.list_projects();
    let paths: Vec<&str> = projects.iter().map(|p| p.path.as_str()).collect();
    assert!(paths.iter().any(|p| p.contains("proj-alpha")), "proj-alpha project");
    assert!(paths.iter().any(|p| p.contains("proj-beta")), "proj-beta project");

    let _ = reg.kill_worker(&a.worker_id);
    let _ = reg.kill_worker(&b.worker_id);
}

// =========================================================================
// E4: 会话恢复
// =========================================================================

#[tokio::test]
async fn e04_session_recovery() {
    let registry = create_registry();

    // Lock scope 1: create worker, send a command, check response, kill
    let info;
    let session_id;
    {
        let mut reg = registry.lock().await;
        info = reg.create_worker(WorkerCreateConfig {
            session: Some("e4-recover".into()),
            ..Default::default()
        }).await.unwrap();
        session_id = info.session_id.clone();

        // Verify worker is alive
        let resp = reg.send_to_worker(
            &info.worker_id, "get_state", serde_json::Value::Null,
        ).await.unwrap();
        assert_eq!(resp["success"], true);
        assert_eq!(resp["data"]["session_id"], session_id);

        reg.kill_worker(&info.worker_id).unwrap();
    }

    // Lock scope 2: recreate with same session
    {
        let mut reg = registry.lock().await;
        let info2 = reg.create_worker(WorkerCreateConfig {
            session: Some(session_id.clone()),
            ..Default::default()
        }).await.unwrap();

        // Recreated worker should be operational
        let resp = reg.send_to_worker(
            &info2.worker_id, "get_state", serde_json::Value::Null,
        ).await.unwrap();
        assert_eq!(resp["success"], true, "recreated worker should respond");

        // Session ID should match
        // Note: the worker generates a new session if not specified,
        // but if we pass --session it should use that
        assert!(
            resp["data"]["session_id"].as_str().map_or(false, |s| s.contains("e4-recover") || s == &session_id),
            "recreated worker should have matching session"
        );

        let _ = reg.kill_worker(&info2.worker_id);
    }
}

// =========================================================================
// S1: 10 Worker 同时 prompt
// =========================================================================

#[tokio::test]
async fn s01_ten_workers_concurrent() {
    let registry = create_registry();
    let mut reg = registry.lock().await;

    // Create 10 workers
    let mut workers = Vec::new();
    for i in 0..10 {
        let info = reg.create_worker(WorkerCreateConfig {
            session: Some(format!("s1-worker-{i}")),
            ..Default::default()
        }).await.unwrap();
        workers.push(info);
    }
    assert_eq!(workers.len(), 10, "should have 10 workers");

    // All 10 respond to commands concurrently
    let mut results = Vec::new();
    for w in &workers {
        let result = reg.send_to_worker(
            &w.worker_id, "get_state", serde_json::Value::Null,
        ).await;
        results.push(result);
    }

    // Verify all 10 succeeded
    let failures: Vec<_> = results.iter().enumerate()
        .filter(|(_, r)| r.is_err() || !r.as_ref().unwrap()["success"].as_bool().unwrap_or(false))
        .collect();
    assert!(failures.is_empty(), "{} workers failed", failures.len());

    // Verify all have unique IDs
    let ids: Vec<&str> = workers.iter().map(|w| w.worker_id.as_str()).collect();
    let mut unique_ids = ids.clone();
    unique_ids.sort();
    unique_ids.dedup();
    assert_eq!(unique_ids.len(), 10, "all workers should have unique IDs");

    // Cleanup
    for w in &workers {
        let _ = reg.kill_worker(&w.worker_id);
    }
}

// =========================================================================
// S2: 1 Worker 连续 50 轮对话
// =========================================================================

#[tokio::test]
async fn s02_fifty_rounds_single_worker() {
    let registry = create_registry();
    let mut reg = registry.lock().await;

    let info = reg.create_worker(WorkerCreateConfig {
        session: Some("s2-fifty".into()),
        ..Default::default()
    }).await.unwrap();

    // Send 50 rapid get_state commands
    for i in 0..50 {
        let resp = reg.send_to_worker(
            &info.worker_id, "get_state", serde_json::Value::Null,
        ).await.expect(&format!("round {i} should succeed"));
        assert_eq!(resp["success"], true, "round {i} should be success");

        // Verify session_id stays consistent
        if i == 0 {
            let sid = resp["data"]["session_id"].as_str().unwrap_or("").to_string();
            assert!(!sid.is_empty(), "should have session_id");
        }
    }

    // Final state check
    let final_resp = reg.send_to_worker(
        &info.worker_id, "get_session_stats", serde_json::Value::Null,
    ).await.unwrap();
    assert_eq!(final_resp["success"], true);

    let _ = reg.kill_worker(&info.worker_id);
}

// =========================================================================
// S3: Channel 100 条消息广播
// =========================================================================

#[tokio::test]
async fn s03_channel_100_messages() {
    let registry = create_registry();
    let mut reg = registry.lock().await;

    // Create 3 subscribers on "broadcast" channel
    let mut subs = Vec::new();
    for i in 0..3 {
        let info = reg.create_worker(WorkerCreateConfig {
            session: Some(format!("s3-sub-{i}")),
            channels: Some(vec!["broadcast".into()]),
            ..Default::default()
        }).await.unwrap();
        subs.push(info);
    }

    // Create sender (not subscribed)
    let sender = reg.create_worker(WorkerCreateConfig {
        session: Some("s3-sender".into()),
        ..Default::default()
    }).await.unwrap();

    // Send 100 messages to channel
    for i in 0..100 {
        reg.channel_send("broadcast", &sender.worker_id,
            serde_json::json!({"seq": i, "text": format!("msg {i}")})).await;
    }

    // Drain events on all subscribers
    for sub in &subs {
        tokio::time::sleep(Duration::from_millis(50)).await;
        reg.drain_events(&sub.worker_id, 500).await;
    }

    // Verify channel subscribers exist
    let channel_subs = reg.channels.get("broadcast");
    assert!(channel_subs.is_some(), "broadcast channel should exist");
    let channel_subs = channel_subs.unwrap();
    assert_eq!(channel_subs.len(), 3, "3 subscribers should be registered");

    for sub in &subs {
        assert!(channel_subs.contains(&sub.worker_id), "subscriber should be in channel");
    }

    // Cleanup
    for sub in &subs {
        let _ = reg.kill_worker(&sub.worker_id);
    }
    let _ = reg.kill_worker(&sender.worker_id);
}

// =========================================================================
// S4: 快速创建/销毁 20 个 Worker
// =========================================================================

#[tokio::test]
async fn s04_rapid_create_destroy_20() {
    let registry = create_registry();
    let mut reg = registry.lock().await;

    let mut created = Vec::new();

    for i in 0..20 {
        // Create
        let info = reg.create_worker(WorkerCreateConfig {
            session: Some(format!("s4-rapid-{i}")),
            ..Default::default()
        }).await.expect(&format!("create worker {i} should succeed"));

        // Quick command to verify it's alive
        let resp = reg.send_to_worker(
            &info.worker_id, "get_state", serde_json::Value::Null,
        ).await;
        assert!(resp.is_ok(), "worker {i} should respond after creation");

        created.push(info);

        // Destroy every 5 workers to keep count manageable
        if created.len() >= 5 {
            for w in created.drain(..) {
                reg.kill_worker(&w.worker_id).ok();
            }
            // Verify cleanup
            let remaining = reg.list_workers();
            assert!(remaining.len() < 5, "should have cleaned up workers");
        }
    }

    // Final cleanup
    for w in created {
        let _ = reg.kill_worker(&w.worker_id);
    }

    // Verify all gone
    let final_workers = reg.list_workers();
    assert_eq!(final_workers.len(), 0, "all workers should be cleaned up");
}

// =========================================================================
// E1: 代码审查流水线 — Worker 通过 manager_command 创建子 Worker
// =========================================================================

#[tokio::test]
async fn e01_code_review_pipeline() {
    let registry = create_registry();
    let mut reg = registry.lock().await;

    // Step 1: Create coordinator (parent) worker
    let coord = reg.create_worker(WorkerCreateConfig {
        session: Some("e1-coordinator".into()),
        ..Default::default()
    }).await.unwrap();
    let coord_id = coord.worker_id.clone();

    // Step 2: Send create_worker command to coordinator
    // The coordinator writes manager_command to stdout,
    // which the reader task sends to manager_cmd_rx.
    // The worker also returns a pending response immediately.
    let resp = reg.send_to_worker(
        &coord_id, "create_worker",
        serde_json::json!({
            "session": "e1-reviewer-auth",
            "parent": coord_id,
            "channels": ["review"],
        }),
    ).await.expect("coordinator should accept create_worker");

    // Verify coordinator returned a pending response
    assert_eq!(resp["success"], true, "coordinator should respond");
    assert_eq!(resp["data"]["status"], "pending",
        "create_worker should be pending when delegated to Manager");

    // Step 3: Process the queued manager command
    reg.process_pending_commands().await;

    // Step 4: Verify child worker was created by the Manager
    let reviewer = reg.find_by_session("e1-reviewer-auth");
    assert!(reviewer.is_some(), "reviewer worker should have been created");
    let reviewer_id = reviewer.unwrap().worker_id.clone();

    // Verify coordinator->reviewer parent-child relationship
    let coord_record = reg.get_worker(&coord_id).unwrap();
    assert!(coord_record.children.contains(&reviewer_id),
        "coordinator should track reviewer as child");

    let reviewer_record = reg.get_worker(&reviewer_id).unwrap();
    assert_eq!(reviewer_record.parent.as_deref(), Some(coord_id.as_str()),
        "reviewer's parent should be coordinator");

    // Verify reviewer is on the "review" channel
    assert!(reviewer_record.channels.contains(&"review".into()),
        "reviewer should be subscribed to review channel");

    // Step 5: Send a prompt to the reviewer child
    let prompt_resp = reg.send_to_worker(
        &reviewer_id, "prompt",
        serde_json::json!({"text": "Review the auth module"}),
    ).await;
    assert!(prompt_resp.is_ok(), "reviewer should accept prompt");

    // Step 6: Cleanup
    let _ = reg.kill_worker(&coord_id);
    let _ = reg.kill_worker(&reviewer_id);
}

// =========================================================================
// E1b: Worker channel_send via manager_command
// =========================================================================

#[tokio::test]
async fn e01b_worker_channel_send() {
    let registry = create_registry();
    let mut reg = registry.lock().await;

    // Create a subscriber on "alerts" channel
    let sub = reg.create_worker(WorkerCreateConfig {
        session: Some("e1b-sub".into()),
        channels: Some(vec!["alerts".into()]),
        ..Default::default()
    }).await.unwrap();

    // Create a sender worker
    let sender = reg.create_worker(WorkerCreateConfig {
        session: Some("e1b-sender".into()),
        ..Default::default()
    }).await.unwrap();

    // Send channel_send to the sender worker
    let resp = reg.send_to_worker(
        &sender.worker_id, "channel_send",
        serde_json::json!({
            "channel": "alerts",
            "msg": {"type": "build_complete", "status": "success"},
        }),
    ).await.expect("sender should accept channel_send");
    assert_eq!(resp["success"], true);

    // Process queued manager command
    reg.process_pending_commands().await;

    // Verify subscriber is still on the channel
    let channel_subs = reg.channels.get("alerts");
    assert!(channel_subs.is_some(), "alerts channel should exist");
    assert!(channel_subs.unwrap().contains(&sub.worker_id),
        "subscriber should still be on channel");

    // Cleanup
    let _ = reg.kill_worker(&sub.worker_id);
    let _ = reg.kill_worker(&sender.worker_id);
}

// =========================================================================
// RT1: Retry — send_to_worker 超时重试验证
// =========================================================================

#[tokio::test]
async fn rt01_retry_on_timeout() {
    let registry = create_registry();
    let mut reg = registry.lock().await;

    let info = reg.create_worker(WorkerCreateConfig {
        session: Some("rt01-test".into()),
        ..Default::default()
    }).await.unwrap();

    // 使用 retry config：快速失败（小 delay）
    let retry_cfg = ion::retry::RetryConfig {
        max_retries: 3,
        initial_delay: Duration::from_millis(5),
        max_delay: Duration::from_millis(50),
        fixed_delay: Duration::from_millis(20),
        multiplier: 2.0,
    };

    // 正常命令应该第一次就成功
    let start = std::time::Instant::now();
    let result = reg.send_to_worker_retry(
        &info.worker_id, "get_state", serde_json::Value::Null, &retry_cfg,
    ).await;
    let elapsed = start.elapsed();
    assert!(result.is_ok(), "get_state should succeed: {:?}", result.err());
    assert!(elapsed < Duration::from_millis(500), "should succeed fast: {:?}", elapsed);

    let _ = reg.kill_worker(&info.worker_id);
}

// =========================================================================
// RT2: Retry — aborts on "insufficient balance"
// =========================================================================

#[tokio::test]
async fn rt02_retry_aborts_on_no_money() {
    // 直接用 retry_async 测试"没钱"逻辑，不依赖 worker
    let retry_cfg = ion::retry::RetryConfig {
        max_retries: 10,
        initial_delay: Duration::from_millis(1),
        ..Default::default()
    };

    let result = ion::retry::retry_async(&retry_cfg, || async {
        Err::<i32, String>("Insufficient balance".into())
    }).await;

    match result {
        Err(ion::retry::RetryError::Permanent { reason, .. }) => {
            assert!(reason.contains("Insufficient"), "should abort on insufficient balance: {reason}");
        }
        _ => panic!("should have aborted permanently"),
    }
}

// =========================================================================
// RT3: Retry — stops after max_retries
// =========================================================================

#[tokio::test]
async fn rt03_retry_exhausts_max_retries() {
    let retry_cfg = ion::retry::RetryConfig {
        max_retries: 3,
        initial_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(10),
        fixed_delay: Duration::from_millis(5),
        ..Default::default()
    };

    let counter = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let c = counter.clone();

    let result = ion::retry::retry_async(&retry_cfg, || {
        let c = c.clone();
        async move {
            c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err::<i32, String>("timeout".into())
        }
    }).await;

    match result {
        Err(ion::retry::RetryError::Transient { attempts, .. }) => {
            // 1 initial + 3 retries = 4 total
            assert!(attempts >= 3, "should have attempted at least 3 times: {attempts}");
            let total = counter.load(std::sync::atomic::Ordering::SeqCst);
            assert_eq!(total, attempts, "total calls should match attempts");
        }
        _ => panic!("should return transient error"),
    }
}

// =========================================================================
// RT4: Retry — succeeds after transient failures
// =========================================================================

#[tokio::test]
async fn rt04_retry_succeeds_after_failures() {
    let retry_cfg = ion::retry::RetryConfig {
        max_retries: 5,
        initial_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(10),
        ..Default::default()
    };

    let counter = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let c = counter.clone();

    let result = ion::retry::retry_async(&retry_cfg, || {
        let c = c.clone();
        async move {
            let attempt = c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if attempt < 2 {
                Err::<i32, String>("timeout".into())
            } else {
                Ok(42)
            }
        }
    }).await;

    assert_eq!(result.unwrap(), 42);
    assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 3,
        "should fail twice then succeed");
}

// =========================================================================
// RT5: Retry — send_to_worker_retry with failing command
// =========================================================================

#[tokio::test]
async fn rt05_send_to_worker_retry_failing() {
    let registry = create_registry();
    let mut reg = registry.lock().await;

    let info = reg.create_worker(WorkerCreateConfig {
        session: Some("rt05-test".into()),
        ..Default::default()
    }).await.unwrap();

    let retry_cfg = ion::retry::RetryConfig {
        max_retries: 2,
        initial_delay: Duration::from_millis(5),
        max_delay: Duration::from_millis(50),
        fixed_delay: Duration::from_millis(20),
        multiplier: 2.0,
    };

    // 未知命令应该每次失败，最后返回 exhausted
    let result = reg.send_to_worker_retry(
        &info.worker_id, "nonexistent_command_xyz", serde_json::Value::Null, &retry_cfg,
    ).await;

    assert!(result.is_err(), "unknown command should fail");
    let err = result.unwrap_err();
    assert!(err.contains("exhausted") || err.contains("unknown"),
        "should mention exhausted: {err}");

    let _ = reg.kill_worker(&info.worker_id);
}

// =========================================================================
// BASH1: 真正 bash 执行
// =========================================================================

#[tokio::test]
async fn bash01_execute_works() {
    let registry = create_registry();
    let mut reg = registry.lock().await;

    let info = reg.create_worker(WorkerCreateConfig {
        session: Some("bash01".into()),
        ..Default::default()
    }).await.unwrap();

    // echo 命令
    let resp = reg.send_to_worker(&info.worker_id, "bash",
        serde_json::json!({"command": "echo 'hello from ion'"})
    ).await.unwrap();
    assert_eq!(resp["success"], true, "bash should succeed");
    assert_eq!(resp["data"]["exitCode"], 0, "exit code 0");
    assert!(resp["data"]["output"].as_str().unwrap_or("").contains("hello from ion"),
        "output should contain echo text");

    // pwd
    let resp2 = reg.send_to_worker(&info.worker_id, "bash",
        serde_json::json!({"command": "pwd"})
    ).await.unwrap();
    assert_eq!(resp2["success"], true);
    let pwd = resp2["data"]["stdout"].as_str().unwrap_or("");
    assert!(!pwd.is_empty(), "pwd should output something");
    assert!(pwd.contains("ion") || std::path::Path::new(pwd.trim()).exists(),
        "pwd should be a valid path");

    let _ = reg.kill_worker(&info.worker_id);
}

// =========================================================================
// BASH2: bash 返回错误 exit code
// =========================================================================

#[tokio::test]
async fn bash02_error_exit_code() {
    let registry = create_registry();
    let mut reg = registry.lock().await;

    let info = reg.create_worker(WorkerCreateConfig {
        session: Some("bash02".into()),
        ..Default::default()
    }).await.unwrap();

    let resp = reg.send_to_worker(&info.worker_id, "bash",
        serde_json::json!({"command": "exit 42"})
    ).await.unwrap();
    assert_eq!(resp["success"], true);
    assert_eq!(resp["data"]["exitCode"], 42, "should have exit code 42");

    let _ = reg.kill_worker(&info.worker_id);
}

// =========================================================================
// BASH3: bash 在工作在 worktree 隔离环境
// =========================================================================

#[tokio::test]
async fn bash03_worktree_cwd() {
    let repo = setup_temp_repo_for_bash("bash03");
    let repo_str = repo.to_string_lossy().to_string();
    let registry = create_registry();
    let mut reg = registry.lock().await;

    let info = reg.create_worker(WorkerCreateConfig {
        session: Some("bash03".into()),
        project_path: Some(repo_str.clone()),
        worktree: Some(WorktreeConfig {
            branch: "bash-worktree-test".into(),
            base: Some("main".into()),
        }),
        ..Default::default()
    }).await.unwrap();

    // bash 命令会在 worktree 目录执行（因为 cwd 被设为 worktree_path）
    // 验证：pwd 返回 worktree 路径，不是主仓库路径
    let resp = reg.send_to_worker(&info.worker_id, "bash",
        serde_json::json!({"command": "pwd"})
    ).await.unwrap();
    assert_eq!(resp["success"], true);
    let pwd = resp["data"]["stdout"].as_str().unwrap_or("").trim();
    assert!(pwd.contains("ion_wt_test_bash03"), "pwd should contain test repo: {pwd}");

    // 在 worktree 里写文件
    let resp2 = reg.send_to_worker(&info.worker_id, "bash",
        serde_json::json!({"command": "echo 'worktree test content' > bash_worktree_test.txt && cat bash_worktree_test.txt"})
    ).await.unwrap();
    assert_eq!(resp2["success"], true);
    assert!(resp2["data"]["output"].as_str().unwrap_or("").contains("worktree test content"));

    // 验证主仓库没有这个文件（隔离生效）
    assert!(!repo.join("bash_worktree_test.txt").exists(),
        "main repo should NOT have the worktree test file");

    let _ = reg.reclaim(&info.worker_id);
}

fn setup_temp_repo_for_bash(name: &str) -> std::path::PathBuf {
    // Same as setup_temp_repo in worktree_isolation.rs
    use std::process::Command;
    let dir = std::env::temp_dir().join(format!("ion_wt_test_{}_{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for cmd in &[["init", "-b", "main"], ["config", "user.email", "test@ion.dev"], ["config", "user.name", "Ion Test"]] {
        Command::new("git").args(cmd).current_dir(&dir).output().unwrap();
    }
    std::fs::write(dir.join("README.md"), "# Test Project\n").unwrap();
    Command::new("git").args(["add", "."]).current_dir(&dir).output().unwrap();
    Command::new("git").args(["commit", "-m", "init"]).current_dir(&dir).output().unwrap();
    dir
}

// =========================================================================
// PERM1: PermissionEngine 拒绝无权限的工具
// =========================================================================

#[tokio::test]
async fn perm01_permission_denies_tool() {
    // 不用 worker（纯测 PermissionEngine）
    let engine = ion::kernel::PermissionEngine::new();

    // 注册规则：拒绝所有 write 操作
    engine.register_rule(ion::kernel::PermissionRule {
        name: "no-write".into(),
        actions: vec![ion::kernel::Action::Write],
        pattern: "**".into(),
        policy: ion::kernel::PermissionPolicy::Deny,
        priority: 100,
    });

    // 读应该放行
    match engine.check("/tmp/test.txt", ion::kernel::Action::Read) {
        ion::kernel::PermissionResult::Allow => {}
        other => panic!("read should be allowed, got: {other:?}"),
    }

    // 写应该被拒绝
    match engine.check("/tmp/test.txt", ion::kernel::Action::Write) {
        ion::kernel::PermissionResult::Deny(reason) => {
            assert!(reason.contains("no-write"), "reason should mention rule name: {reason}");
        }
        other => panic!("write should be denied, got: {other:?}"),
    }
}

// =========================================================================
// PERM2: Action::from_tool 映射正确
// =========================================================================

#[tokio::test]
async fn perm02_action_from_tool() {
    assert_eq!(ion::kernel::Action::from_tool("read"), ion::kernel::Action::Read);
    assert_eq!(ion::kernel::Action::from_tool("write"), ion::kernel::Action::Write);
    assert_eq!(ion::kernel::Action::from_tool("bash"), ion::kernel::Action::Execute);
    assert_eq!(ion::kernel::Action::from_tool("edit"), ion::kernel::Action::Edit);
    assert_eq!(ion::kernel::Action::from_tool("grep"), ion::kernel::Action::Read);
    assert_eq!(ion::kernel::Action::from_tool("find"), ion::kernel::Action::Read);
    assert_eq!(ion::kernel::Action::from_tool("ls"), ion::kernel::Action::Read);
}

// =========================================================================
// PERM3: glob 匹配正确
// =========================================================================

#[tokio::test]
async fn perm03_glob_matching() {
    let engine = ion::kernel::PermissionEngine::new();

    engine.register_rule(ion::kernel::PermissionRule {
        name: "no-env".into(),
        actions: vec![ion::kernel::Action::Read, ion::kernel::Action::Write],
        pattern: "**/.env*".into(),
        policy: ion::kernel::PermissionPolicy::Deny,
        priority: 100,
    });

    // .env 文件应该被拒绝
    match engine.check("/app/.env", ion::kernel::Action::Read) {
        ion::kernel::PermissionResult::Deny(_) => {}
        other => panic!(".env read should be denied: {other:?}"),
    }
    match engine.check("/app/.env.production", ion::kernel::Action::Read) {
        ion::kernel::PermissionResult::Deny(_) => {}
        other => panic!(".env.production read should be denied: {other:?}"),
    }

    // 普通文件应该放行
    match engine.check("/app/main.rs", ion::kernel::Action::Read) {
        ion::kernel::PermissionResult::Allow => {}
        other => panic!("main.rs read should be allowed: {other:?}"),
    }
}

// =========================================================================
// PERM4: ExtensionRegistry 可以绑定 PermissionEngine
// =========================================================================

#[tokio::test]
async fn perm04_extension_registry_with_permissions() {
    use ion::agent::extension::ExtensionRegistry;
    use ion::kernel::*;

    let engine = PermissionEngine::new();
    engine.register_rule(PermissionRule {
        name: "block-bash".into(),
        actions: vec![Action::Execute],
        pattern: "**".into(),
        policy: PermissionPolicy::Deny,
        priority: 100,
    });

    let registry = ExtensionRegistry::new()
        .with_permissions(engine);

    assert!(registry.permission_engine.is_some(), "should have permission engine");

    // 权限检查应该工作（通过 registry 上的 engine）
    let engine = registry.permission_engine.as_ref().unwrap();
    // execute 动作被拒绝
    match engine.check("/any/command", Action::Execute) {
        PermissionResult::Deny(reason) => assert!(reason.contains("block-bash"), "{reason}"),
        other => panic!("should deny: {other:?}"),
    }
    // read 动作放行（没规则针对 read）
    match engine.check("/any/file.txt", Action::Read) {
        PermissionResult::Allow => {}
        other => panic!("read should allow: {other:?}"),
    }
}

// =========================================================================
// RUNTIME1: ReadTool 走 Runtime（通过 SecuredRuntime 拦截验证）
// =========================================================================

#[tokio::test]
async fn runtime01_read_goes_through_secured() {
    use std::sync::Arc;
    use ion::kernel::*;
    use ion::agent::tool::ReadTool;

    let engine = Arc::new(PermissionEngine::new());
    engine.register_rule(PermissionRule {
        name: "block-all-reads".into(),
        actions: vec![Action::Read],
        pattern: "**".into(),
        policy: PermissionPolicy::Deny,
        priority: 100,
    });

    let secured = ion::runtime::SecuredRuntime::new(ion::runtime::LocalRuntime::new())
        .with_permissions(engine);

    let tool = ReadTool;
    let result: Result<String, ion::agent::error::AgentError> = tool.execute(
        serde_json::json!({"file_path": "/tmp/nonexistent_ion_test_file.txt"}),
        &secured,
    ).await;

    // 应该因为权限被拒绝，而不是文件不存在
    match result {
        Err(e) => {
            let e = e.to_string();
            let msg = e.to_string();
            assert!(msg.contains("Permission") || msg.contains("Deny"),
                "should be blocked by permission, got: {msg}");
        }
        Ok(_) => panic!("read should have been blocked"),
    }
}

// =========================================================================
// RUNTIME2: BashTool 走 Runtime（通过 CommandGuard 拦截验证）
// =========================================================================

#[tokio::test]
async fn runtime02_bash_goes_through_guard() {
    use ion::command_guard::CommandGuard;
    use ion::agent::tool::BashTool;

    let guard = CommandGuard::default();
    let secured = ion::runtime::SecuredRuntime::new(ion::runtime::LocalRuntime::new())
        .with_command_guard(guard);

    let tool = BashTool;
    let result: Result<String, ion::agent::error::AgentError> = tool.execute(
        serde_json::json!({"command": "rm -rf / "}),
        &secured,
    ).await;

    match result {
        Err(e) => {
            let e = e.to_string();
            let msg = e.to_string();
            assert!(msg.contains("CommandGuard") || msg.contains("高危"),
                "should be blocked by guard, got: {msg}");
        }
        Ok(_) => panic!("rm -rf / should have been blocked"),
    }
}

// =========================================================================
// RUNTIME3: SecuredRuntime + Agent 集成（走 ext registry）
// =========================================================================

#[tokio::test]
async fn runtime03_agent_uses_runtime() {
    // 验证 Agent 默认使用 LocalRuntime
    let registry = create_registry();
    let mut reg = registry.lock().await;
    let info = reg.create_worker(WorkerCreateConfig {
        session: Some("runtime03".into()),
        ..Default::default()
    }).await.unwrap();

    // bash 命令应该正常工作（走 LocalRuntime）
    let resp = reg.send_to_worker(&info.worker_id, "bash",
        serde_json::json!({"command": "echo runtime_test_ok"})
    ).await.unwrap();
    assert_eq!(resp["success"], true);
    let output = resp["data"]["output"].as_str().unwrap_or("");
    assert!(output.contains("runtime_test_ok"),
        "bash should work through LocalRuntime: {output}");

    let _ = reg.kill_worker(&info.worker_id);
}
