//! Worktree 隔离 E2E 测试
//!
//! project_path 可选，默认 current_dir()。测试传 project_path 为了并行隔离。

use std::process::Command;
use std::sync::Arc;
use tokio::sync::Mutex;

use ion::worker_registry::{WorkerCreateConfig, WorktreeConfig, WorkerRegistry};

fn worker_bin_path() -> std::path::PathBuf {
    if let Ok(path) = std::env::var("ION_WORKER_BIN") {
        return std::path::PathBuf::from(path);
    }
    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let debug_bin = manifest.join("target").join("debug").join("ion-worker");
    if debug_bin.exists() { return debug_bin; }
    std::path::PathBuf::from("ion-worker")
}

fn create_registry() -> Arc<Mutex<WorkerRegistry>> {
    let bin = worker_bin_path();
    Arc::new(Mutex::new(WorkerRegistry::with_binary(&bin.to_string_lossy())))
}

fn setup_temp_repo(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("ion_wt_test_{}_{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for cmd in &[
        ["init", "-b", "main"],
        ["config", "user.email", "test@ion.dev"],
        ["config", "user.name", "Ion Test"],
    ] { Command::new("git").args(cmd).current_dir(&dir).output().unwrap(); }
    std::fs::write(dir.join("README.md"), "# Test Project\n").unwrap();
    Command::new("git").args(["add", "."]).current_dir(&dir).output().unwrap();
    Command::new("git").args(["commit", "-m", "init"]).current_dir(&dir).output().unwrap();
    dir
}

fn branch_exists(repo: &str, branch: &str) -> bool {
    let o = Command::new("git").args(["branch", "--list", branch]).current_dir(repo).output().unwrap();
    String::from_utf8_lossy(&o.stdout).contains(branch)
}

fn list_worktrees(repo: &str) -> Vec<String> {
    let o = Command::new("git").args(["worktree", "list", "--porcelain"]).current_dir(repo).output().unwrap();
    String::from_utf8_lossy(&o.stdout).lines().filter(|l| l.starts_with("worktree "))
        .map(|l| l.strip_prefix("worktree ").unwrap_or(l).to_string()).collect()
}

// WT1: worktree 创建 + cwd 隔离
#[tokio::test]
async fn wt01_worktree_creates_isolated_cwd() {
    let repo = setup_temp_repo("wt01");
    let repo_str = repo.to_string_lossy().to_string();
    let registry = create_registry();
    let mut reg = registry.lock().await;

    let info = reg.create_worker(WorkerCreateConfig {
        session: Some("wt01-session".into()),
        project_path: Some(repo_str.clone()),
        worktree: Some(WorktreeConfig { branch: "feature-wt01".into(), base: Some("main".into()) }),
        ..Default::default()
    }).await.unwrap();

    let wt = reg.get_worker(&info.worker_id).unwrap().worktree.as_ref().unwrap().clone();
    assert_eq!(wt.branch, "feature-wt01");
    assert!(std::path::Path::new(&wt.path).exists(), "worktree should exist");
    assert!(branch_exists(&repo_str, "feature-wt01"));
    assert!(list_worktrees(&repo_str).iter().any(|w| *w == wt.path));
    reg.kill_worker(&info.worker_id).unwrap();
}

// WT2: reclaim → worktree 清理 + 分支保留
#[tokio::test]
async fn wt02_reclaim_cleans_worktree_preserves_branch() {
    let repo = setup_temp_repo("wt02");
    let repo_str = repo.to_string_lossy().to_string();
    let registry = create_registry();
    let mut reg = registry.lock().await;

    let info = reg.create_worker(WorkerCreateConfig {
        session: Some("wt02-session".into()),
        project_path: Some(repo_str.clone()),
        worktree: Some(WorktreeConfig { branch: "feature-wt02".into(), base: Some("main".into()) }),
        ..Default::default()
    }).await.unwrap();

    let wt_path = reg.get_worker(&info.worker_id).unwrap().worktree.as_ref().unwrap().path.clone();
    assert!(std::path::Path::new(&wt_path).exists());
    reg.reclaim(&info.worker_id).unwrap();

    assert!(reg.get_worker(&info.worker_id).is_none());
    assert!(!std::path::Path::new(&wt_path).exists(), "worktree cleaned");
    assert!(branch_exists(&repo_str, "feature-wt02"), "branch preserved");
    assert!(!list_worktrees(&repo_str).iter().any(|w| *w == wt_path), "git worktree removed");
}

// WT3: 两个 worker 并行隔离
#[tokio::test]
async fn wt03_parallel_workers_isolated() {
    let repo = setup_temp_repo("wt03");
    let repo_str = repo.to_string_lossy().to_string();
    let registry = create_registry();
    let mut reg = registry.lock().await;

    let a = reg.create_worker(WorkerCreateConfig {
        session: Some("wt03-a".into()), project_path: Some(repo_str.clone()),
        worktree: Some(WorktreeConfig { branch: "feature-A".into(), base: Some("main".into()) }),
        ..Default::default()
    }).await.unwrap();
    let b = reg.create_worker(WorkerCreateConfig {
        session: Some("wt03-b".into()), project_path: Some(repo_str.clone()),
        worktree: Some(WorktreeConfig { branch: "feature-B".into(), base: Some("main".into()) }),
        ..Default::default()
    }).await.unwrap();

    let wa = reg.get_worker(&a.worker_id).unwrap().worktree.as_ref().unwrap().path.clone();
    let wb = reg.get_worker(&b.worker_id).unwrap().worktree.as_ref().unwrap().path.clone();
    assert_ne!(wa, wb);
    assert!(std::path::Path::new(&wa).exists() && std::path::Path::new(&wb).exists());
    assert!(branch_exists(&repo_str, "feature-A") && branch_exists(&repo_str, "feature-B"));

    let r1 = reg.send_to_worker(&a.worker_id, "get_state", serde_json::Value::Null).await;
    let r2 = reg.send_to_worker(&b.worker_id, "get_state", serde_json::Value::Null).await;
    assert!(r1.is_ok() && r2.is_ok());

    reg.reclaim(&a.worker_id).unwrap(); reg.reclaim(&b.worker_id).unwrap();
    assert!(!std::path::Path::new(&wa).exists() && !std::path::Path::new(&wb).exists());
    assert!(branch_exists(&repo_str, "feature-A") && branch_exists(&repo_str, "feature-B"));
}

// WT4: kill_worker 也清理 worktree
#[tokio::test]
async fn wt04_kill_cleans_worktree() {
    let repo = setup_temp_repo("wt04");
    let repo_str = repo.to_string_lossy().to_string();
    let registry = create_registry();
    let mut reg = registry.lock().await;

    let info = reg.create_worker(WorkerCreateConfig {
        session: Some("wt04-session".into()), project_path: Some(repo_str.clone()),
        worktree: Some(WorktreeConfig { branch: "feature-wt04".into(), base: Some("main".into()) }),
        ..Default::default()
    }).await.unwrap();

    let wt_path = reg.get_worker(&info.worker_id).unwrap().worktree.as_ref().unwrap().path.clone();
    assert!(std::path::Path::new(&wt_path).exists());
    reg.kill_worker(&info.worker_id).unwrap();
    assert!(!std::path::Path::new(&wt_path).exists(), "kill cleans worktree");
    assert!(branch_exists(&repo_str, "feature-wt04"), "branch preserved");
}

// WT5: 不用 worktree 时不受影响
#[tokio::test]
async fn wt05_no_worktree_default_behavior() {
    let _repo = setup_temp_repo("wt05");
    let registry = create_registry();
    let mut reg = registry.lock().await;
    let info = reg.create_worker(WorkerCreateConfig {
        session: Some("wt05-plain".into()), worktree: None, ..Default::default()
    }).await.unwrap();
    assert!(reg.get_worker(&info.worker_id).unwrap().worktree.is_none());
    assert!(reg.send_to_worker(&info.worker_id, "get_state", serde_json::Value::Null).await.is_ok());
    reg.kill_worker(&info.worker_id).unwrap();
}

// WT6: 并发隔离开发 E2E — 5 个隔离 worktree + 开发 + 验证清理
#[tokio::test]
async fn wt06_concurrent_development() {
    let repo = setup_temp_repo("wt06");
    let repo_str = repo.to_string_lossy().to_string();
    let registry = create_registry();
    let mut reg = registry.lock().await;

    // 创建 5 个隔离 Worker
    let n = 5;
    let mut infos: Vec<(String, String, String)> = Vec::new();
    for i in 0..n {
        let branch = format!("feature-concurrent-{}", i);
        let info = reg.create_worker(WorkerCreateConfig {
            session: Some(format!("wt06-wkr-{}", i)),
            project_path: Some(repo_str.clone()),
            worktree: Some(WorktreeConfig { branch: branch.clone(), base: Some("main".into()) }),
            ..Default::default()
        }).await.unwrap();
        let wt_path = reg.get_worker(&info.worker_id).unwrap().worktree.as_ref().unwrap().path.clone();
        infos.push((info.worker_id, branch, wt_path));
    }

    // 并发开发：每个 worktree 写文件 + git commit
    for (i, (_, branch, wt_path)) in infos.iter().enumerate() {
        let task_file = format!("task-{}.md", i);
        let content = format!("# Task {}\n\nCompleted on branch {}.\n", i, branch);
        std::fs::write(std::path::Path::new(wt_path).join(&task_file), &content).unwrap();
        let add = Command::new("git").args(["add", &task_file]).current_dir(wt_path).output().unwrap();
        assert!(add.status.success(), "git add {} failed", i);
        let commit = Command::new("git").args(["commit", "-m", &format!("task {} done", i)]).current_dir(wt_path).output().unwrap();
        assert!(commit.status.success(), "git commit {} failed", i);
    }

    // 验证：主分支没有 task 文件（隔离有效）
    let main_tasks = std::fs::read_dir(&repo).unwrap().filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("task-")).count();
    assert_eq!(main_tasks, 0, "main should NOT have task files");

    // 验证：每个分支有对应文件且内容正确
    for (i, (_, branch, _)) in infos.iter().enumerate() {
        let out = Command::new("git").args(["show", &format!("{}:task-{}.md", branch, i)])
            .current_dir(&repo_str).output().unwrap();
        assert!(out.status.success(), "branch {} missing task-{}.md", branch, i);
        assert!(String::from_utf8_lossy(&out.stdout).contains(&format!("# Task {}", i)));
    }

    // 回收所有 Worker
    for (wid, _, _) in &infos { reg.reclaim(wid).unwrap(); }

    // 验证：所有 worktree 目录已清理
    for (i, (_, _, wt_path)) in infos.iter().enumerate() {
        assert!(!std::path::Path::new(wt_path).exists(), "worktree {} still exists", i);
    }
    // 验证：git worktree list 无残留
    let wts = list_worktrees(&repo_str);
    for (_, _, wt_path) in &infos { assert!(!wts.iter().any(|w| *w == *wt_path), "git has worktree"); }
    // 验证：所有分支保留
    for (_, branch, _) in &infos { assert!(branch_exists(&repo_str, branch), "branch {} missing", branch); }
    // 验证：主仓库不变
    assert_eq!(std::fs::read_to_string(repo.join("README.md")).unwrap().trim(), "# Test Project");
}
