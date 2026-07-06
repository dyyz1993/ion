// ─────────────────────────────────────────────────────────────────────────────
// Runtime 抽象层 — 进程管理测试规格 (TDD: Red)
// ─────────────────────────────────────────────────────────────────────────────
//
// 这些测试验证 LocalRuntime 的进程管理方法 (spawn_process / kill_process /
// send_stdin)。当前 LocalRuntime 返回 Err，测试会 FAIL（红），
// 供小模型实现时作为验证目标。
//
// 通过条件：
// 1. spawn_process(background=false) 返回完整 stdout/stderr/exit_code
// 2. spawn_process(background=true)  立即返回，kill_process 能终止
// 3. send_stdin 向进程 stdin 写入数据

use ion::runtime::{LocalRuntime, ProcessHandle, Runtime, SpawnProcessRequest};

// ── 前台进程：echo ──────────────────────────────────────────────────────────

#[tokio::test]
async fn local_runtime_spawn_foreground_echo() {
    let rt = LocalRuntime::new();
    let req = SpawnProcessRequest {
        command: "echo hello-world".into(),
        timeout_secs: 5,
        background: false,
        log_path: None,
    };
    let handle: ProcessHandle = rt.spawn_process(req).await.unwrap();
    assert!(handle.stdout.contains("hello-world"), "stdout={}", handle.stdout);
    assert_eq!(handle.exit_code, Some(0));
}

// ── 前台进程：非零退出码 ──────────────────────────────────────────────────────

#[tokio::test]
async fn local_runtime_spawn_foreground_exit_code() {
    let rt = LocalRuntime::new();
    let req = SpawnProcessRequest {
        command: "exit 42".into(),
        timeout_secs: 5,
        background: false,
        log_path: None,
    };
    let handle = rt.spawn_process(req).await.unwrap();
    assert_eq!(handle.exit_code, Some(42));
}

// ── 后台进程：spawn + kill ──────────────────────────────────────────────────

#[tokio::test]
async fn local_runtime_spawn_background_and_kill() {
    let rt = LocalRuntime::new();
    let req = SpawnProcessRequest {
        command: "sleep 60".into(),
        timeout_secs: 5,
        background: true,
        log_path: None,
    };
    let handle = rt.spawn_process(req).await.unwrap();
    assert!(handle.os_pid > 0, "os_pid should be > 0, got {}", handle.os_pid);
    assert!(!handle.bid.is_empty(), "bid should not be empty");
    // 后台模式：exit_code 应为 None（进程仍在跑）
    assert!(handle.exit_code.is_none(), "background: exit_code should be None");

    // 终止进程
    rt.kill_process(handle.os_pid).await.unwrap();
}

// ── send_stdin: 向 cat 进程写入数据 ─────────────────────────────────────────

#[tokio::test]
async fn local_runtime_send_stdin() {
    let rt = LocalRuntime::new();
    // cat 从 stdin 读取并输出到 stdout
    let req = SpawnProcessRequest {
        command: "cat".into(),
        timeout_secs: 10,
        background: true,
        log_path: None,
    };
    let handle = rt.spawn_process(req).await.unwrap();

    // 向 cat 的 stdin 写入数据
    rt.send_stdin(handle.os_pid, "hello-stdin\n").await.unwrap();

    // 给 cat 一点时间处理，再 kill
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    rt.kill_process(handle.os_pid).await.unwrap();
}

// ── 后台进程：timeout 自动结束 ───────────────────────────────────────────────

#[tokio::test]
async fn local_runtime_spawn_background_timeout() {
    let rt = LocalRuntime::new();
    let req = SpawnProcessRequest {
        command: "echo done && sleep 10".into(),
        timeout_secs: 2,  // 2 秒超时
        background: true,
        log_path: None,
    };
    let handle = rt.spawn_process(req).await.unwrap();
    // 等 3 秒（超时后会结束）
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    // kill 应该仍然工作（即使进程已经结束了）
    rt.kill_process(handle.os_pid).await.unwrap();
}
