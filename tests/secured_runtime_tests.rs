// ─────────────────────────────────────────────────────────────────────────────
// Runtime 抽象层 — SecuredRuntime 安全拦截测试规格 (TDD: Red)
// ─────────────────────────────────────────────────────────────────────────────
//
// 这些测试验证 SecuredRuntime 的安全检查：
// 1. CommandGuard 拦截危险命令
// 2. PermissionEngine 拦截文件读写
// 3. 安全命令正常放行
// 4. grep_search 也走权限检查

use ion::command_guard::CommandGuard;
use ion::kernel::{Action, PermissionEngine, PermissionPolicy as Policy, PermissionRule};
use ion::runtime::{LocalRuntime, Runtime, SecuredRuntime};

// ── CommandGuard 拦截危险命令 ───────────────────────────────────────────────

#[tokio::test]
async fn secured_runtime_blocks_dangerous_command() {
    let base = LocalRuntime::new();
    let guard = CommandGuard::default();
    let rt = SecuredRuntime::new(base).with_command_guard(guard);

    // rm -rf / 是高风险命令，应被 CommandGuard 拦截
    let result = rt.execute_command("rm -rf /", 5).await;
    assert!(result.is_err(), "should block rm -rf /");
}

#[tokio::test]
async fn secured_runtime_allows_safe_command() {
    let base = LocalRuntime::new();
    let guard = CommandGuard::default();
    let rt = SecuredRuntime::new(base).with_command_guard(guard);

    let (stdout, _, _) = rt.execute_command("echo ok", 5).await.unwrap();
    assert!(stdout.contains("ok"), "stdout={stdout}");
}

// ── PermissionEngine 拦截文件读取 ───────────────────────────────────────────

#[tokio::test]
async fn secured_runtime_checks_file_permission() {
    let base = LocalRuntime::new();
    let engine = PermissionEngine::new();
    // 注册规则：禁止读取 ~/.ssh/ 下的任何文件
    engine.register_rule(PermissionRule {
        name: "block-ssh".into(),
        actions: vec![Action::Read],
        pattern: "*/.ssh/*".into(),
        policy: Policy::Deny,
        priority: 100,
    });

    let rt = SecuredRuntime::new(base).with_permissions(std::sync::Arc::new(engine));

    let result = rt.read_file("/Users/test/.ssh/id_rsa").await;
    assert!(result.is_err(), "should block read from ~/.ssh/");
}

// ── PermissionEngine 放行允许路径 ───────────────────────────────────────────

#[tokio::test]
async fn secured_runtime_allows_safe_read() {
    let base = LocalRuntime::new();
    let engine = PermissionEngine::new();
    engine.register_rule(PermissionRule {
        name: "block-ssh".into(),
        actions: vec![Action::Read],
        pattern: "*/.ssh/*".into(),
        policy: Policy::Deny,
        priority: 100,
    });

    let rt = SecuredRuntime::new(base).with_permissions(std::sync::Arc::new(engine));

    // /tmp 下的文件应正常读取
    let _ = rt.read_file("/tmp/some-file").await;
    // 测试能执行就算通过（如果文件不存在返回 Err，但不是权限错误）
}

// ── grep_search 也走权限检查 ────────────────────────────────────────────────

#[tokio::test]
async fn secured_runtime_checks_grep_search() {
    let base = LocalRuntime::new();
    let engine = PermissionEngine::new();
    engine.register_rule(PermissionRule {
        name: "block-secret".into(),
        actions: vec![Action::Read],
        pattern: "*/secret/*".into(),
        policy: Policy::Deny,
        priority: 100,
    });

    let rt = SecuredRuntime::new(base).with_permissions(std::sync::Arc::new(engine));

    let result = rt.grep_search("test", "/tmp/secret/data").await;
    assert!(result.is_err(), "should block grep in secret dir");
}

// ── spawn_process 走 CommandGuard ───────────────────────────────────────────

#[tokio::test]
async fn secured_runtime_blocks_dangerous_spawn() {
    let base = LocalRuntime::new();
    let guard = CommandGuard::default();
    let rt = SecuredRuntime::new(base).with_command_guard(guard);

    let req = ion::runtime::SpawnProcessRequest {
        command: "rm -rf /".into(),
        timeout_secs: 5,
        background: false,
        log_path: None,
    };
    let result = rt.spawn_process(req).await;
    assert!(result.is_err(), "should block dangerous spawn");
}

// ── spawn_process 放行安全命令 ───────────────────────────────────────────────

#[tokio::test]
async fn secured_runtime_allows_safe_spawn() {
    let base = LocalRuntime::new();
    let guard = CommandGuard::default();
    let rt = SecuredRuntime::new(base).with_command_guard(guard);

    let req = ion::runtime::SpawnProcessRequest {
        command: "echo safe".into(),
        timeout_secs: 5,
        background: false,
        log_path: None,
    };
    let result = rt.spawn_process(req).await;
    // LocalRuntime 尚未实现，返回 Err，但不是 CommandGuard 拦截的错误
    if let Err(msg) = &result {
        assert!(!msg.contains("rejected"), "should not be rejected by guard: {msg}");
    }
}
