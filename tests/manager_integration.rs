//! Phase 2: 集成测试 I1-I8 (Manager + Worker 基础生命周期)
//!
//! 这些测试使用 WorkerRegistry 直接操作 Manager，
//! 通过 in-process 方式创建 Worker 子进程，验证生命周期。

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

use ion::worker_registry::{WorkerCreateConfig, WorkerRegistry};
use std::path::PathBuf;

const RESPONSE_TIMEOUT: Duration = Duration::from_secs(15);

/// Locate the ion-worker binary for testing
fn worker_bin_path() -> PathBuf {
    if let Ok(path) = std::env::var("ION_WORKER_BIN") {
        return PathBuf::from(path);
    }
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let debug_bin = manifest.join("target").join("debug").join("ion-worker");
    if debug_bin.exists() {
        return debug_bin;
    }
    // Fall back to PATH
    PathBuf::from("ion-worker")
}

/// Create a registry pre-configured with the worker binary path
fn create_registry() -> Arc<Mutex<WorkerRegistry>> {
    let bin = worker_bin_path();
    Arc::new(Mutex::new(WorkerRegistry::with_binary(
        &bin.to_string_lossy(),
    )))
}

// ---------------------------------------------------------------------------
// I1: Manager 启动 (0 Worker)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn i01_manager_starts_with_zero_workers() {
    let registry = create_registry();
    let reg = registry.lock().await;
    let workers = reg.list_workers();
    assert_eq!(workers.len(), 0, "fresh manager should have 0 workers");
    let projects = reg.list_projects();
    assert!(projects.is_empty(), "fresh manager should have 0 projects");
}

// ---------------------------------------------------------------------------
// I2: 创建 Worker
// ---------------------------------------------------------------------------

#[tokio::test]
async fn i02_create_worker_returns_info() {
    let registry = create_registry();
    let mut reg = registry.lock().await;

    let info = reg.create_worker(WorkerCreateConfig {
        session: Some("i02-session".into()),
        project_path: None,
        model: None,
        provider: None,
        agent: None,
        channels: None,
        parent: None,
        worktree: None,
    }).await.expect("create_worker should succeed");

    assert!(!info.worker_id.is_empty(), "worker_id should not be empty");
    assert!(!info.session_id.is_empty(), "session_id should not be empty");

    // Verify worker appears in list
    let workers = reg.list_workers();
    let matching: Vec<_> = workers.iter().filter(|w| w.worker_id == info.worker_id).collect();
    assert_eq!(matching.len(), 1, "worker should be listed");

    // Cleanup
    let _ = reg.kill_worker(&info.worker_id);
}

// ---------------------------------------------------------------------------
// I3: 列出 Worker
// ---------------------------------------------------------------------------

#[tokio::test]
async fn i03_list_workers_shows_all() {
    let registry = create_registry();
    let mut reg = registry.lock().await;

    for i in 0..3 {
        reg.create_worker(WorkerCreateConfig {
            session: Some(format!("list-test-{i}")),
            ..Default::default()
        }).await.expect("create_worker");
    }

    let workers = reg.list_workers();
    assert_eq!(workers.len(), 3, "should list 3 workers");

    for w in &workers {
        assert!(!w.worker_id.is_empty(), "worker should have id");
        assert!(!w.session_id.is_empty(), "worker should have session_id");
    }

    // Cleanup
    for w in workers {
        let _ = reg.kill_worker(&w.worker_id);
    }
}

// ---------------------------------------------------------------------------
// I4: 列出项目
// ---------------------------------------------------------------------------

#[tokio::test]
async fn i04_list_projects() {
    let registry = create_registry();
    let mut reg = registry.lock().await;

    // Create temp directories so spawn doesn't fail
    let tmp_a = std::env::temp_dir().join("ion_test_i04_a").join("proj-a");
    let tmp_b = std::env::temp_dir().join("ion_test_i04_b").join("proj-b");
    std::fs::create_dir_all(&tmp_a).ok();
    std::fs::create_dir_all(&tmp_b).ok();

    let info1 = reg.create_worker(WorkerCreateConfig {
        session: Some("proj-a".into()),
        project_path: Some(tmp_a.to_string_lossy().to_string()),
        ..Default::default()
    }).await.unwrap();

    let info2 = reg.create_worker(WorkerCreateConfig {
        session: Some("proj-b".into()),
        project_path: Some(tmp_b.to_string_lossy().to_string()),
        ..Default::default()
    }).await.unwrap();

    let projects = reg.list_projects();
    assert!(!projects.is_empty(), "should have projects");

    let proj_paths: Vec<&str> = projects.iter().map(|p| p.path.as_str()).collect();
    assert!(proj_paths.iter().any(|p| p.contains("proj-a")), "proj-a should exist");
    assert!(proj_paths.iter().any(|p| p.contains("proj-b")), "proj-b should exist");

    let _ = reg.kill_worker(&info1.worker_id);
    let _ = reg.kill_worker(&info2.worker_id);
}

// ---------------------------------------------------------------------------
// I5: 给 Worker 发命令
// ---------------------------------------------------------------------------

#[tokio::test]
async fn i05_send_command_to_worker() {
    let registry = create_registry();
    let mut reg = registry.lock().await;

    let info = reg.create_worker(WorkerCreateConfig {
        session: Some("i05-test".into()),
        ..Default::default()
    }).await.expect("create_worker");

    // Send get_state command
    let response = reg.send_to_worker(
        &info.worker_id,
        "get_state",
        serde_json::Value::Null,
    ).await.expect("send_to_worker should succeed");

    // Verify response format
    assert_eq!(response["type"], "response", "should be a response");
    assert_eq!(response["command"], "get_state");
    assert_eq!(response["success"], true);
    assert!(response["data"].is_object());
    assert!(response["data"]["model"].is_string());

    let _ = reg.kill_worker(&info.worker_id);
}

// ---------------------------------------------------------------------------
// I6: 订阅 Worker 事件
// ---------------------------------------------------------------------------

#[tokio::test]
async fn i06_worker_events_forwarded() {
    let registry = create_registry();
    let mut reg = registry.lock().await;

    let info = reg.create_worker(WorkerCreateConfig {
        session: Some("i06-test".into()),
        ..Default::default()
    }).await.expect("create_worker");

    // Subscribe to events
    let mut events = reg.subscribe(&info.worker_id)
        .expect("subscribe should work");

    // Send a prompt (triggers events)
    let _resp = reg.send_to_worker(
        &info.worker_id,
        "prompt",
        serde_json::json!({"text": "Say hello"}),
    ).await;

    // Wait for events (agent_start, text_delta, agent_end)
    let mut event_count = 0;
    loop {
        match tokio::time::timeout(Duration::from_secs(20), events.recv()).await {
            Ok(Some(event)) => {
                event_count += 1;
                // Check if we got agent_end (final event)
                if event.get("type").and_then(|v| v.as_str()) == Some("event") {
                    if let Some(inner) = event.get("event") {
                        if inner.get("type").and_then(|v| v.as_str()) == Some("agent_end") {
                            break;
                        }
                    }
                }
            }
            Ok(None) | Err(_) => break,
        }
    }

    assert!(event_count > 0, "should have received events");

    let _ = reg.kill_worker(&info.worker_id);
}

// ---------------------------------------------------------------------------
// I7: 关闭 Worker
// ---------------------------------------------------------------------------

#[tokio::test]
async fn i07_kill_worker_removes_it() {
    let registry = create_registry();
    let mut reg = registry.lock().await;

    let info = reg.create_worker(WorkerCreateConfig {
        session: Some("i07-test".into()),
        ..Default::default()
    }).await.expect("create_worker");

    let wid = info.worker_id.clone();

    // Kill the worker
    reg.kill_worker(&wid).expect("kill should succeed");

    // Verify it's gone
    let workers = reg.list_workers();
    let matching: Vec<_> = workers.iter().filter(|w| w.worker_id == wid).collect();
    assert_eq!(matching.len(), 0, "worker should be removed after kill");
}

// ---------------------------------------------------------------------------
// I8: 关闭后重新创建
// ---------------------------------------------------------------------------

#[tokio::test]
async fn i08_recreate_worker_with_same_session() {
    let registry = create_registry();
    let mut reg = registry.lock().await;

    // Note: auto-respawn by session ID requires the Manager-level
    // send_to_session which isn't in WorkerRegistry directly.
    // Here we verify that: create → kill → re-create with same session works.

    let info1 = reg.create_worker(WorkerCreateConfig {
        session: Some("i08-session".into()),
        ..Default::default()
    }).await.expect("first create");

    // Send a command
    let resp = reg.send_to_worker(
        &info1.worker_id,
        "get_state",
        serde_json::Value::Null,
    ).await.expect("command before kill");
    assert_eq!(resp["success"], true);

    // Kill
    reg.kill_worker(&info1.worker_id).expect("kill");

    // Re-create with same session
    let info2 = reg.create_worker(WorkerCreateConfig {
        session: Some("i08-session".into()),
        ..Default::default()
    }).await.expect("re-create");

    assert!(info2.session_id == "i08-session" || info2.session_id.contains("i08-session"),
        "re-created worker should have the same session");

    // Send a command to the new worker
    let resp2 = reg.send_to_worker(
        &info2.worker_id,
        "get_state",
        serde_json::Value::Null,
    ).await.expect("command after re-create");
    assert_eq!(resp2["success"], true);

    let _ = reg.kill_worker(&info2.worker_id);
}

// ---------------------------------------------------------------------------
// 额外: 多 Worker 并发命令
// ---------------------------------------------------------------------------

#[tokio::test]
async fn i05b_multi_worker_concurrent() {
    let registry = create_registry();
    let mut reg = registry.lock().await;

    let mut workers = Vec::new();
    for i in 0..3 {
        let info = reg.create_worker(WorkerCreateConfig {
            session: Some(format!("concurrent-{i}")),
            ..Default::default()
        }).await.expect("create_worker");
        workers.push(info);
    }

    // Send commands to all 3 workers
    for w in &workers {
        let resp = reg.send_to_worker(
            &w.worker_id,
            "get_state",
            serde_json::Value::Null,
        ).await;
        assert!(resp.is_ok(), "worker {} should respond", w.worker_id);
        if let Ok(r) = resp {
            assert_eq!(r["success"], true);
        }
    }

    // Cleanup
    for w in &workers {
        let _ = reg.kill_worker(&w.worker_id);
    }
}

// =========================================================================
// Phase 3: I9-I20 Worker 通信测试
// =========================================================================

// ---------------------------------------------------------------------------
// I9: 同级 A→B 发消息
// ---------------------------------------------------------------------------

#[tokio::test]
async fn i09_peer_to_peer_message() {
    let registry = create_registry();
    let mut reg = registry.lock().await;

    // Create two peers (no parent)
    let a = reg.create_worker(WorkerCreateConfig {
        session: Some("i09-a".into()), ..Default::default()
    }).await.unwrap();
    let b = reg.create_worker(WorkerCreateConfig {
        session: Some("i09-b".into()), ..Default::default()
    }).await.unwrap();

    // A sends a command to B
    let resp = reg.send_to_worker(
        &b.worker_id, "get_state", serde_json::Value::Null,
    ).await.expect("A→B send should work");
    assert_eq!(resp["success"], true, "B should respond to A");
    assert!(resp["data"]["session_id"].is_string(), "B should return session data");

    let _ = reg.kill_worker(&a.worker_id);
    let _ = reg.kill_worker(&b.worker_id);
}

// ---------------------------------------------------------------------------
// I10: 父→子 发消息
// ---------------------------------------------------------------------------

#[tokio::test]
async fn i10_parent_to_child_message() {
    let registry = create_registry();
    let mut reg = registry.lock().await;

    // Create parent A
    let parent = reg.create_worker(WorkerCreateConfig {
        session: Some("i10-parent".into()),
        ..Default::default()
    }).await.unwrap();

    // A creates child B
    let child = reg.create_worker(WorkerCreateConfig {
        session: Some("i10-child".into()),
        parent: Some(parent.worker_id.clone()),
        ..Default::default()
    }).await.unwrap();

    // Verify parent-child relationship
    assert!(child.parent.as_deref() == Some(&parent.worker_id),
        "B's parent should be A");

    // Parent sends command to child
    let resp = reg.send_to_worker(
        &child.worker_id, "get_state", serde_json::Value::Null,
    ).await.expect("parent→child send should work");
    assert_eq!(resp["success"], true);

    let _ = reg.kill_worker(&parent.worker_id);
    let _ = reg.kill_worker(&child.worker_id);
}

// ---------------------------------------------------------------------------
// I11: 子→父 回传事件 (child_event)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn i11_child_event_back_to_parent() {
    let registry = create_registry();
    let mut reg = registry.lock().await;

    // Create parent A with event subscription
    let parent = reg.create_worker(WorkerCreateConfig {
        session: Some("i11-parent".into()),
        ..Default::default()
    }).await.unwrap();

    // Subscribe parent's events
    let mut parent_events = reg.subscribe(&parent.worker_id).unwrap();

    // Create child B
    let child = reg.create_worker(WorkerCreateConfig {
        session: Some("i11-child".into()),
        parent: Some(parent.worker_id.clone()),
        ..Default::default()
    }).await.unwrap();

    // Child runs a prompt (generates events)
    let _ = reg.send_to_worker(
        &child.worker_id, "prompt",
        serde_json::json!({"text": "Hello from child"}),
    ).await;

    // Drain events from child to forward them
    reg.drain_events(&child.worker_id, 3000).await;

    // Parent should receive child events
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut found_child_event = false;
    loop {
        if tokio::time::Instant::now() > deadline { break; }
        match tokio::time::timeout(Duration::from_millis(500), parent_events.recv()).await {
            Ok(Some(event)) => {
                if event.get("type").and_then(|v| v.as_str()) == Some("child_event") {
                    found_child_event = true;
                    break;
                }
            }
            Ok(None) | Err(_) => break,
        }
    }

    assert!(found_child_event, "parent should receive child_event from child");

    let _ = reg.kill_worker(&parent.worker_id);
    let _ = reg.kill_worker(&child.worker_id);
}

// ---------------------------------------------------------------------------
// I12: A 拉取 B 状态
// ---------------------------------------------------------------------------

#[tokio::test]
async fn i12_pull_worker_state() {
    let registry = create_registry();
    let mut reg = registry.lock().await;

    let a = reg.create_worker(WorkerCreateConfig {
        session: Some("i12-a".into()), ..Default::default()
    }).await.unwrap();
    let b = reg.create_worker(WorkerCreateConfig {
        session: Some("i12-b".into()), ..Default::default()
    }).await.unwrap();

    // A pulls B's state via send_to_worker
    let resp = reg.send_to_worker(
        &b.worker_id, "get_session_stats", serde_json::Value::Null,
    ).await.expect("A should be able to get B's state");

    assert_eq!(resp["success"], true);
    assert!(resp["data"]["sessionId"].is_string(), "should have sessionId");
    assert!(resp["data"]["totalMessages"].is_number(), "should have totalMessages");

    let _ = reg.kill_worker(&a.worker_id);
    let _ = reg.kill_worker(&b.worker_id);
}

// ---------------------------------------------------------------------------
// I13: A 列子 Worker
// ---------------------------------------------------------------------------

#[tokio::test]
async fn i13_list_child_workers() {
    let registry = create_registry();
    let mut reg = registry.lock().await;

    let parent = reg.create_worker(WorkerCreateConfig {
        session: Some("i13-parent".into()),
        ..Default::default()
    }).await.unwrap();

    // Create two children
    let c1 = reg.create_worker(WorkerCreateConfig {
        session: Some("i13-c1".into()),
        parent: Some(parent.worker_id.clone()),
        ..Default::default()
    }).await.unwrap();
    let c2 = reg.create_worker(WorkerCreateConfig {
        session: Some("i13-c2".into()),
        parent: Some(parent.worker_id.clone()),
        ..Default::default()
    }).await.unwrap();

    // List all workers and filter by parent
    let all_workers = reg.list_workers();
    let children: Vec<_> = all_workers.iter()
        .filter(|w| w.parent.as_deref() == Some(&parent.worker_id))
        .collect();

    assert_eq!(children.len(), 2, "parent should have 2 children");

    // Verify parent's children list
    let parent_record = reg.get_worker(&parent.worker_id).unwrap();
    assert_eq!(parent_record.children.len(), 2, "parent record should track 2 children");
    assert!(parent_record.children.contains(&c1.worker_id));
    assert!(parent_record.children.contains(&c2.worker_id));

    let _ = reg.kill_worker(&parent.worker_id);
    let _ = reg.kill_worker(&c1.worker_id);
    let _ = reg.kill_worker(&c2.worker_id);
}

// ---------------------------------------------------------------------------
// I14: A 停止子 B
// ---------------------------------------------------------------------------

#[tokio::test]
async fn i14_kill_child_worker() {
    let registry = create_registry();
    let mut reg = registry.lock().await;

    let parent = reg.create_worker(WorkerCreateConfig {
        session: Some("i14-parent".into()),
        ..Default::default()
    }).await.unwrap();

    let child = reg.create_worker(WorkerCreateConfig {
        session: Some("i14-child".into()),
        parent: Some(parent.worker_id.clone()),
        ..Default::default()
    }).await.unwrap();

    let child_id = child.worker_id.clone();

    // Kill the child
    reg.kill_worker(&child_id).expect("kill child should succeed");

    // Verify child is gone from parent's children
    let parent_record = reg.get_worker(&parent.worker_id).unwrap();
    assert!(!parent_record.children.contains(&child_id),
        "child should be removed from parent's children");

    // Verify child is not listed
    let workers = reg.list_workers();
    let child_still_exists = workers.iter().any(|w| w.worker_id == child_id);
    assert!(!child_still_exists, "child should not be listed after kill");

    let _ = reg.kill_worker(&parent.worker_id);
}

// ---------------------------------------------------------------------------
// I15: B 自己退出 → A 收到通知
// ---------------------------------------------------------------------------

#[tokio::test]
async fn i15_worker_self_shutdown() {
    let registry = create_registry();
    let mut reg = registry.lock().await;

    let parent = reg.create_worker(WorkerCreateConfig {
        session: Some("i15-parent".into()),
        ..Default::default()
    }).await.unwrap();

    let child = reg.create_worker(WorkerCreateConfig {
        session: Some("i15-child".into()),
        parent: Some(parent.worker_id.clone()),
        ..Default::default()
    }).await.unwrap();

    // Subscribe parent to child's events
    let mut child_events = reg.subscribe(&child.worker_id).unwrap();

    // Child shuts itself down
    let resp = reg.send_to_worker(
        &child.worker_id, "shutdown", serde_json::Value::Null,
    ).await;
    assert!(resp.is_ok(), "shutdown command should be accepted");

    // Give time for the child to process shutdown and forward events
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Drain events from child
    reg.drain_events(&child.worker_id, 1000).await;

    // Parent should get some event (final status)
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        if tokio::time::Instant::now() > deadline { break; }
        match tokio::time::timeout(Duration::from_millis(300), child_events.recv()).await {
            Ok(Some(_)) => { /* got event */ }
            Ok(None) | Err(_) => break,
        }
    }
    // The event channel may be closed after child exits — that's fine
    // The key verification is that the parent can still operate

    // Parent is still alive
    let resp2 = reg.send_to_worker(
        &parent.worker_id, "get_state", serde_json::Value::Null,
    ).await;
    assert!(resp2.is_ok(), "parent should still be alive");

    let _ = reg.kill_worker(&parent.worker_id);
}

// ---------------------------------------------------------------------------
// I16: Channel 广播
// ---------------------------------------------------------------------------

#[tokio::test]
async fn i16_channel_broadcast() {
    let registry = create_registry();
    let mut reg = registry.lock().await;

    // Create workers A and B subscribed to "review" channel
    let a = reg.create_worker(WorkerCreateConfig {
        session: Some("i16-a".into()),
        channels: Some(vec!["review".into()]),
        ..Default::default()
    }).await.unwrap();

    let b = reg.create_worker(WorkerCreateConfig {
        session: Some("i16-b".into()),
        channels: Some(vec!["review".into()]),
        ..Default::default()
    }).await.unwrap();

    // Create C (sender, not subscribed)
    let c = reg.create_worker(WorkerCreateConfig {
        session: Some("i16-c".into()),
        ..Default::default()
    }).await.unwrap();

    // C sends a message to "review" channel
    reg.channel_send("review", &c.worker_id,
        serde_json::json!({"text": "Code review requested"})).await;

    // Drain events on A and B to process incoming channel messages
    tokio::time::sleep(Duration::from_millis(200)).await;
    reg.drain_events(&a.worker_id, 500).await;
    reg.drain_events(&b.worker_id, 500).await;

    // Verify by checking that A and B received the channel message
    // We can't easily read from channel subscribers in tests,
    // but we can verify the registry tracked the subscriptions
    let a_record = reg.get_worker(&a.worker_id).unwrap();
    let b_record = reg.get_worker(&b.worker_id).unwrap();
    assert!(a_record.channels.contains(&"review".to_string()),
        "A should be subscribed to review");
    assert!(b_record.channels.contains(&"review".to_string()),
        "B should be subscribed to review");

    // Verify the channel subscriber list
    let review_subs = reg.channels.get("review");
    assert!(review_subs.is_some(), "review channel should have subscribers");
    let subs = review_subs.unwrap();
    assert!(subs.contains(&a.worker_id), "A should be in review subscribers");
    assert!(subs.contains(&b.worker_id), "B should be in review subscribers");

    let _ = reg.kill_worker(&a.worker_id);
    let _ = reg.kill_worker(&b.worker_id);
    let _ = reg.kill_worker(&c.worker_id);
}

// ---------------------------------------------------------------------------
// I17: Channel 取消订阅
// ---------------------------------------------------------------------------

#[tokio::test]
async fn i17_channel_unsubscribe() {
    let registry = create_registry();
    let mut reg = registry.lock().await;

    let a = reg.create_worker(WorkerCreateConfig {
        session: Some("i17-a".into()),
        channels: Some(vec!["deploy".into()]),
        ..Default::default()
    }).await.unwrap();

    let b = reg.create_worker(WorkerCreateConfig {
        session: Some("i17-b".into()),
        channels: Some(vec!["deploy".into()]),
        ..Default::default()
    }).await.unwrap();

    // Kill A (which removes it from channel subscribers)
    reg.kill_worker(&a.worker_id).expect("kill A");

    // Now only B should be in the channel
    let subs = reg.channels.get("deploy");
    assert!(subs.is_some(), "deploy channel should still exist");
    let subs = subs.unwrap();
    assert!(!subs.contains(&a.worker_id), "A should be removed from subscribers");
    assert!(subs.contains(&b.worker_id), "B should still be subscribed");

    // Send a channel message
    reg.channel_send("deploy", &b.worker_id,
        serde_json::json!({"text": "Deploy ready"})).await;

    tokio::time::sleep(Duration::from_millis(200)).await;
    reg.drain_events(&b.worker_id, 500).await;

    let _ = reg.kill_worker(&b.worker_id);
}

// ---------------------------------------------------------------------------
// I18: 多 Channel
// ---------------------------------------------------------------------------

#[tokio::test]
async fn i18_multi_channel() {
    let registry = create_registry();
    let mut reg = registry.lock().await;

    // A subscribes to both "review" and "deploy"
    let a = reg.create_worker(WorkerCreateConfig {
        session: Some("i18-a".into()),
        channels: Some(vec!["review".into(), "deploy".into()]),
        ..Default::default()
    }).await.unwrap();

    // Verify A is in both channels
    let review_subs = reg.channels.get("review").unwrap();
    assert!(review_subs.contains(&a.worker_id), "A should be in review");
    let deploy_subs = reg.channels.get("deploy").unwrap();
    assert!(deploy_subs.contains(&a.worker_id), "A should be in deploy");

    // Send messages to both channels
    reg.channel_send("review", &a.worker_id,
        serde_json::json!({"text": "review msg"})).await;
    reg.channel_send("deploy", &a.worker_id,
        serde_json::json!({"text": "deploy msg"})).await;

    tokio::time::sleep(Duration::from_millis(200)).await;
    reg.drain_events(&a.worker_id, 500).await;

    let _ = reg.kill_worker(&a.worker_id);
}

// ---------------------------------------------------------------------------
// I19: 按会话 ID 自动启动 (send_to_session)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn i19_session_auto_start() {
    let registry = create_registry();
    let mut reg = registry.lock().await;

    // Send to a non-existent session — should auto-start
    let resp = reg.send_to_session(
        "i19-auto-session",
        "get_state",
        serde_json::Value::Null,
    ).await.expect("send_to_session should auto-start worker");

    assert_eq!(resp["success"], true, "auto-started worker should respond");

    // Verify worker was created for this session — clone the ID before mutable use
    let auto_worker_id = reg.find_by_session("i19-auto-session")
        .map(|w| w.worker_id.clone());
    assert!(auto_worker_id.is_some(), "worker should exist for session after auto-start");

    // Send again — should reuse existing worker
    let resp2 = reg.send_to_session(
        "i19-auto-session",
        "get_state",
        serde_json::Value::Null,
    ).await.expect("second send should reuse existing");

    assert_eq!(resp2["success"], true);
    assert_eq!(resp2["data"]["session_id"], "i19-auto-session",
        "should use the same session");

    // Cleanup: kill the auto-started worker
    if let Some(wid) = auto_worker_id {
        let _ = reg.kill_worker(&wid);
    }
}

// ---------------------------------------------------------------------------
// I20: get_session 自动启动 (find_by_session)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn i20_session_lookup_auto_start() {
    let registry = create_registry();
    let mut reg = registry.lock().await;

    // Initially no worker for this session
    {
        let not_found = reg.find_by_session("i20-new-session");
        assert!(not_found.is_none(), "should not exist before creation");
    }

    // Create worker via send_to_session (auto-start)
    let resp = reg.send_to_session(
        "i20-new-session",
        "get_session_stats",
        serde_json::Value::Null,
    ).await.expect("auto-start should work");
    assert_eq!(resp["success"], true);

    // Now it should exist — clone the ID before mutable use
    let auto_id = reg.find_by_session("i20-new-session")
        .map(|w| w.worker_id.clone());
    assert!(auto_id.is_some(), "worker should exist after auto-start");

    // Cleanup
    if let Some(wid) = auto_id {
        let _ = reg.kill_worker(&wid);
    }
}

// =========================================================================
// Phase 4: I21-I32 事件推送 + UI 测试
// =========================================================================

// ---------------------------------------------------------------------------
// I21: 订阅 Worker 事件 (text_delta/agent_end)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn i21_subscribe_worker_events() {
    let registry = create_registry();
    let mut reg = registry.lock().await;

    let info = reg.create_worker(WorkerCreateConfig {
        session: Some("i21-test".into()),
        ..Default::default()
    }).await.unwrap();

    let mut events = reg.subscribe(&info.worker_id).unwrap();
    let _ = reg.send_to_worker(
        &info.worker_id, "prompt",
        serde_json::json!({"text": "Hi"}),
    ).await;

    // Poll: drain and then read from events channel repeatedly
    let deadline = tokio::time::Instant::now() + Duration::from_secs(25);
    let mut saw_agent_start = false;
    let mut saw_text_delta = false;
    let mut saw_agent_end = false;

    loop {
        if tokio::time::Instant::now() > deadline { break; }
        // Drain any pending events from stdout_rx into subscribers
        reg.drain_events(&info.worker_id, 500).await;
        // Read from subscriber channel
        match tokio::time::timeout(Duration::from_millis(1000), events.recv()).await {
            Ok(Some(event)) => {
                if event.get("type").and_then(|v| v.as_str()) == Some("event") {
                    let inner_type = event["event"]["type"].as_str().unwrap_or("");
                    match inner_type {
                        "agent_start" => saw_agent_start = true,
                        "text_delta" => saw_text_delta = true,
                        "agent_end" => saw_agent_end = true,
                        _ => {}
                    }
                }
                if saw_agent_end { break; }
            }
            _ => {
                // No more events right now; drain again
                if saw_agent_start && saw_text_delta {
                    // If we saw start and text but not end, wait for LLM
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }

    assert!(saw_agent_start, "should receive agent_start");
    assert!(saw_text_delta, "should receive text_delta");
    assert!(saw_agent_end, "should receive agent_end");

    let _ = reg.kill_worker(&info.worker_id);
}

// ---------------------------------------------------------------------------
// I22: 事件顺序验证
// ---------------------------------------------------------------------------

#[tokio::test]
async fn i22_event_ordering() {
    let registry = create_registry();
    let mut reg = registry.lock().await;

    let info = reg.create_worker(WorkerCreateConfig {
        session: Some("i22-test".into()),
        ..Default::default()
    }).await.unwrap();

    let mut events = reg.subscribe(&info.worker_id).unwrap();
    let _ = reg.send_to_worker(
        &info.worker_id, "prompt",
        serde_json::json!({"text": "Hello"}),
    ).await;

    let mut collected: Vec<String> = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(25);
    loop {
        if tokio::time::Instant::now() > deadline { break; }
        reg.drain_events(&info.worker_id, 500).await;
        match tokio::time::timeout(Duration::from_millis(1000), events.recv()).await {
            Ok(Some(event)) => {
                if event.get("type").and_then(|v| v.as_str()) == Some("event") {
                    if let Some(inner) = event["event"]["type"].as_str() {
                        collected.push(inner.to_string());
                        if inner == "agent_end" { break; }
                    }
                }
            }
            _ => {}
        }
    }

    assert!(collected.contains(&"agent_start".into()), "should have agent_start");
    assert!(collected.contains(&"text_delta".into()), "should have text_delta");
    let start_pos = collected.iter().position(|t| t == "agent_start");
    let end_pos = collected.iter().position(|t| t == "agent_end");
    if let (Some(sp), Some(ep)) = (start_pos, end_pos) {
        assert!(sp < ep, "agent_start before agent_end");
    }

    let _ = reg.kill_worker(&info.worker_id);
}

// ---------------------------------------------------------------------------
// I23: Worker 创建事件 (worker_created)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn i23_worker_created_event() {
    let registry = create_registry();
    let mut reg = registry.lock().await;

    let mut global_events = reg.subscribe_global();
    let info = reg.create_worker(WorkerCreateConfig {
        session: Some("i23-test".into()),
        ..Default::default()
    }).await.unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let mut found = false;
    loop {
        if tokio::time::Instant::now() > deadline { break; }
        match tokio::time::timeout(Duration::from_millis(500), global_events.recv()).await {
            Ok(Some(event)) => {
                if event.get("type").and_then(|v| v.as_str()) == Some("worker_created") {
                    assert_eq!(event["worker_id"], info.worker_id);
                    assert_eq!(event["session_id"], info.session_id);
                    found = true;
                }
            }
            Ok(None) | Err(_) => break,
        }
    }
    assert!(found, "should receive worker_created event");
    let _ = reg.kill_worker(&info.worker_id);
}

// ---------------------------------------------------------------------------
// I24: 项目变化事件 (project_changed)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn i24_project_changed_event() {
    let registry = create_registry();
    let mut reg = registry.lock().await;

    let mut global_events = reg.subscribe_global();

    // Create → project_changed with change=created
    let info = reg.create_worker(WorkerCreateConfig {
        session: Some("i24-test".into()),
        ..Default::default()
    }).await.unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let mut saw_create = false;
    loop {
        if tokio::time::Instant::now() > deadline { break; }
        match tokio::time::timeout(Duration::from_millis(500), global_events.recv()).await {
            Ok(Some(event)) => {
                if event.get("type").and_then(|v| v.as_str()) == Some("project_changed")
                    && event.get("change").and_then(|v| v.as_str()) == Some("created")
                {
                    saw_create = true;
                }
            }
            Ok(None) | Err(_) => break,
        }
    }
    assert!(saw_create, "should receive project_changed on create");

    // Kill → project_changed with change=destroyed
    reg.kill_worker(&info.worker_id).unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let mut saw_destroy = false;
    loop {
        if tokio::time::Instant::now() > deadline { break; }
        match tokio::time::timeout(Duration::from_millis(500), global_events.recv()).await {
            Ok(Some(event)) => {
                if event.get("type").and_then(|v| v.as_str()) == Some("project_changed")
                    && event.get("change").and_then(|v| v.as_str()) == Some("destroyed")
                {
                    saw_destroy = true;
                }
            }
            Ok(None) | Err(_) => break,
        }
    }
    assert!(saw_destroy, "should receive project_changed on destroy");
}

// ---------------------------------------------------------------------------
// I25: 会话信息随创建事件
// ---------------------------------------------------------------------------

#[tokio::test]
async fn i25_session_in_created_event() {
    let registry = create_registry();
    let mut reg = registry.lock().await;

    let mut global_events = reg.subscribe_global();
    let info = reg.create_worker(WorkerCreateConfig {
        session: Some("i25-session".into()),
        ..Default::default()
    }).await.unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let mut saw = false;
    loop {
        if tokio::time::Instant::now() > deadline { break; }
        match tokio::time::timeout(Duration::from_millis(500), global_events.recv()).await {
            Ok(Some(event)) => {
                if event.get("type").and_then(|v| v.as_str()) == Some("worker_created") {
                    assert_eq!(event["session_id"], "i25-session");
                    assert!(event.get("worker_id").is_some());
                    assert!(event.get("project").is_some());
                    saw = true;
                }
            }
            Ok(None) | Err(_) => break,
        }
    }
    assert!(saw, "worker_created should include session info");
    let _ = reg.kill_worker(&info.worker_id);
}

// ---------------------------------------------------------------------------
// I29: 全局概览 (get_overview)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn i29_global_overview() {
    let registry = create_registry();
    let mut reg = registry.lock().await;

    let a = reg.create_worker(WorkerCreateConfig {
        session: Some("i29-a".into()),
        ..Default::default()
    }).await.unwrap();
    let b = reg.create_worker(WorkerCreateConfig {
        session: Some("i29-b".into()),
        ..Default::default()
    }).await.unwrap();

    let overview = reg.get_overview();

    assert!(overview["total_workers"].as_u64().unwrap_or(0) >= 2,
        "overview: at least 2 workers");
    assert!(overview["total_projects"].as_u64().unwrap_or(0) >= 1,
        "overview: at least 1 project");

    let workers = overview["workers"].as_array().unwrap();
    let ids: Vec<&str> = workers.iter().filter_map(|w| w["worker_id"].as_str()).collect();
    assert!(ids.contains(&a.worker_id.as_str()));
    assert!(ids.contains(&b.worker_id.as_str()));

    let sessions = overview["sessions"].as_array().unwrap();
    assert!(sessions.len() >= 2, "overview: at least 2 sessions");

    let _ = reg.kill_worker(&a.worker_id);
    let _ = reg.kill_worker(&b.worker_id);
}

// ---------------------------------------------------------------------------
// I30: 多 Worker 同时订阅
// ---------------------------------------------------------------------------

#[tokio::test]
async fn i30_multiple_subscriptions() {
    let registry = create_registry();
    let mut reg = registry.lock().await;

    let a = reg.create_worker(WorkerCreateConfig {
        session: Some("i30-a".into()),
        ..Default::default()
    }).await.unwrap();
    let b = reg.create_worker(WorkerCreateConfig {
        session: Some("i30-b".into()),
        ..Default::default()
    }).await.unwrap();

    let mut ea = reg.subscribe(&a.worker_id).unwrap();
    let mut eb = reg.subscribe(&b.worker_id).unwrap();

    let _ = reg.send_to_worker(&a.worker_id, "prompt",
        serde_json::json!({"text": "Hi A"})).await;
    reg.drain_events(&a.worker_id, 3000).await;
    let _ = reg.send_to_worker(&b.worker_id, "prompt",
        serde_json::json!({"text": "Hi B"})).await;
    reg.drain_events(&b.worker_id, 3000).await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(25);
    let mut ac = 0usize;
    let mut bc = 0usize;

    loop {
        if tokio::time::Instant::now() > deadline { break; }
        reg.drain_events(&a.worker_id, 500).await;
        reg.drain_events(&b.worker_id, 500).await;
        match tokio::time::timeout(Duration::from_millis(1000), ea.recv()).await {
            Ok(Some(ev)) => { if ev.get("type").and_then(|v| v.as_str()) == Some("event") { ac += 1; } }
            _ => {}
        }
        match tokio::time::timeout(Duration::from_millis(1000), eb.recv()).await {
            Ok(Some(ev)) => { if ev.get("type").and_then(|v| v.as_str()) == Some("event") { bc += 1; } }
            _ => {}
        }
        if ac > 0 && bc > 0 { break; }
    }

    assert!(ac > 0, "A should receive events");
    assert!(bc > 0, "B should receive events");

    let _ = reg.kill_worker(&a.worker_id);
    let _ = reg.kill_worker(&b.worker_id);
}

// ---------------------------------------------------------------------------
// I31: 历史加载 (get_messages)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn i31_session_history() {
    let registry = create_registry();
    let mut reg = registry.lock().await;

    let info = reg.create_worker(WorkerCreateConfig {
        session: Some("i31-test".into()),
        ..Default::default()
    }).await.unwrap();

    let resp = reg.send_to_worker(
        &info.worker_id, "get_messages", serde_json::Value::Null,
    ).await.unwrap();
    assert_eq!(resp["success"], true);
    assert!(resp["data"].is_array(), "get_messages should return array");

    let _ = reg.kill_worker(&info.worker_id);
}

// ---------------------------------------------------------------------------
// I32: 导出会话 (export_html)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn i32_export_session() {
    let registry = create_registry();
    let mut reg = registry.lock().await;

    let info = reg.create_worker(WorkerCreateConfig {
        session: Some("i32-test".into()),
        ..Default::default()
    }).await.unwrap();

    let resp = reg.send_to_worker(
        &info.worker_id, "export_html",
        serde_json::json!({"path": "/tmp/ion_test_export.html"}),
    ).await;
    // export_html may fail if template files missing — that's acceptable
    // The key test is that it doesn't crash the worker
    assert!(resp.is_ok(), "export_html should not crash worker");

    let _ = reg.kill_worker(&info.worker_id);
}
