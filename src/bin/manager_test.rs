use ion::worker_registry::{WorkerCreateConfig, WorkerRegistry};
use std::sync::Arc;
use tokio::sync::Mutex;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new("info"))
        .with_target(false).try_init().ok();

    let registry = Arc::new(Mutex::new(WorkerRegistry::new()));
    let cwd = std::env::current_dir().unwrap().to_string_lossy().to_string();
    tracing::info!("=== Worktree 隔离测试 ===");
    tracing::info!("项目路径: {}", cwd);

    // 创建带 worktree 隔离的 Worker
    let wid = {
        let mut reg = registry.lock().await;
        let w = reg.create_worker(WorkerCreateConfig {
            session: Some("worktree-test".into()),
            project_path: Some(cwd.clone()),
            worktree: Some(ion::worker_registry::WorktreeConfig { branch: "test-branch".into(), base: None }),
            ..Default::default()
        }).await.unwrap();
        w.worker_id
    };

    // 在 worktree 里执行 git_status
    tracing::info!("在 worktree 里执行 git_status...");
    {
        let mut reg = registry.lock().await;
        match reg.send_to_worker(&wid, "prompt",
            serde_json::json!({"text":"Use git_status to check the status of this repo. Just report the output."})).await {
            Ok(_) => tracing::info!("✅ prompt 发送成功"),
            Err(e) => tracing::warn!("❌ {e}"),
        }
    }

    tokio::time::sleep(std::time::Duration::from_secs(15)).await;

    // 查结果
    {
        let mut reg = registry.lock().await;
        match reg.send_to_worker(&wid, "get_last_assistant_text", serde_json::json!({})).await {
            Ok(r) => tracing::info!("✅ 结果: {}", r.get("data").and_then(|v| v.as_str()).unwrap_or("(empty)")),
            Err(e) => tracing::warn!("❌ {e}"),
        }
        let _ = reg.kill_worker(&wid);
    }

    // 检查 worktree 是否创建
    let home = std::env::var("HOME").unwrap();
    let wt_path = format!("{home}/.ion/worktree/worktree-test/ion");
    if std::path::Path::new(&wt_path).exists() {
        tracing::info!("✅ worktree 创建成功: {}", wt_path);
    } else {
        tracing::warn!("⚠️ worktree 不存在: {}", wt_path);
    }

    tracing::info!("=== 测试完成 ===");
}
