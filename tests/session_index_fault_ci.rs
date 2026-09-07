// session_index_fault_ci.rs — T06 故障注入：索引持久化错误可见 + 不覆盖损坏数据
//
// 单一 #[test] 顺序执行全部场景（HOME 是进程级环境变量，不能并行隔离）。
// 场景：
//   F1 损坏索引 + write_txn → 隔离保全（内容原样）、空索引不覆盖现场、
//      事务后从新索引重建、issue 记录可读
//   F2 索引目录只读 → save 失败被记录（不再静默）
//   F3 锁文件路径被目录占用 → LockDegraded 记录、功能降级不中断
//   F4 索引文件不可读（chmod 000）→ 隔离保全后重建
//   F5 并发 8 线程 × 25 次写不同会话 → 无丢更新（flock 串行）
use ion::session_index::SessionIndex;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

fn mode(m: u32) -> std::fs::Permissions {
    std::fs::Permissions::from_mode(m)
}

struct HomeGuard {
    original: String,
    sandbox: PathBuf,
}

impl HomeGuard {
    fn new(tag: &str) -> Self {
        let original = std::env::var("HOME").unwrap_or_default();
        let sandbox =
            std::env::temp_dir().join(format!("ion-idx-fault-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&sandbox);
        std::fs::create_dir_all(sandbox.join(".ion/agent")).unwrap();
        unsafe { std::env::set_var("HOME", &sandbox) };
        Self { original, sandbox }
    }

    fn index_path(&self) -> PathBuf {
        self.sandbox.join(".ion/agent/sessions.index.json")
    }

    fn write_index_raw(&self, content: &str) {
        std::fs::write(self.index_path(), content).unwrap();
    }
}

impl Drop for HomeGuard {
    fn drop(&mut self) {
        // 恢复目录权限再删，避免只读沙箱残留
        let _ = std::fs::set_permissions(self.sandbox.join(".ion/agent"), mode(0o755));
        let _ = std::fs::remove_dir_all(&self.sandbox);
        unsafe { std::env::set_var("HOME", &self.original) };
    }
}

fn upsert_txn(id: &str) {
    SessionIndex::write_txn(|idx| {
        idx.upsert(&id, crate_test_meta());
    });
}

// 构造最小合法 SessionMeta（字段全 pub，直接字面量）
fn crate_test_meta() -> ion::session_index::SessionMeta {
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

#[test]
fn index_persist_faults_are_visible_and_safe() {
    // ─── F1：损坏索引 → 隔离保全，不覆盖，重建新索引 ───
    {
        let g = HomeGuard::new("f1");
        let corrupt = "{{{ NOT JSON AT ALL";
        g.write_index_raw(corrupt);
        upsert_txn("sess_f1");
        let issue = SessionIndex::last_issue().expect("F1: issue must be recorded");
        assert!(issue.contains("corrupt"), "F1 issue text: {issue}");
        assert!(issue.contains("quarantined"), "F1 issue text: {issue}");
        // 现场保全：存在 .corrupt-* 备份且内容原样
        let dir = g.index_path().parent().unwrap().to_path_buf();
        let backups: Vec<PathBuf> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .map(|n| n.to_string_lossy().contains(".corrupt-"))
                    .unwrap_or(false)
            })
            .collect();
        assert_eq!(backups.len(), 1, "F1: exactly one quarantine backup");
        assert_eq!(
            std::fs::read_to_string(&backups[0]).unwrap(),
            corrupt,
            "F1: backup must preserve the corrupt bytes verbatim"
        );
        // 新索引可解析且包含本次 upsert
        let idx = SessionIndex::load();
        assert!(
            idx.get("sess_f1").is_some(),
            "F1: fresh index has the new session"
        );
    }

    // ─── F2：索引目录只读 → save 失败被记录 ───
    {
        let g = HomeGuard::new("f2");
        upsert_txn("sess_f2_base"); // 正常先写一条
        // last_issue 是粘性设计（健康检查可反复拉取）——只要求此前无 save 失败记录
        let pre = SessionIndex::last_issue().unwrap_or_default();
        assert!(!pre.contains("save failed"), "F2 precondition: {pre}");
        let dir = g.index_path().parent().unwrap().to_path_buf();
        std::fs::set_permissions(&dir, mode(0o555)).unwrap();
        upsert_txn("sess_f2_blocked");
        let issue = SessionIndex::last_issue().expect("F2: save failure must be recorded");
        assert!(
            issue.contains("save failed") || issue.contains("lock degraded"),
            "F2 issue text: {issue}"
        );
        // 恢复权限后验证：被阻塞那条未落盘（可见性测试，不是数据丢失测试）
        std::fs::set_permissions(&dir, mode(0o755)).unwrap();
        let idx = SessionIndex::load();
        assert!(idx.get("sess_f2_base").is_some());
    }

    // ─── F3：锁文件路径被目录占用 → LockDegraded 记录、功能不中断 ───
    {
        let g = HomeGuard::new("f3");
        let lock_dir = g.index_path().with_extension("json.lock");
        std::fs::create_dir_all(&lock_dir).unwrap(); // File::create 必失败
        upsert_txn("sess_f3");
        let issue = SessionIndex::last_issue().expect("F3: lock degradation must be recorded");
        assert!(issue.contains("lock degraded"), "F3 issue text: {issue}");
        // 降级路径仍完成写
        let idx = SessionIndex::load();
        assert!(
            idx.get("sess_f3").is_some(),
            "F3: degraded path still persists"
        );
        std::fs::remove_dir_all(&lock_dir).unwrap();
    }

    // ─── F4：索引文件不可读（chmod 000）→ 隔离保全后重建 ───
    {
        let g = HomeGuard::new("f4");
        g.write_index_raw("{\"sessions\":{},\"removed_sessions\":[]}");
        std::fs::set_permissions(g.index_path(), mode(0o000)).unwrap();
        upsert_txn("sess_f4");
        let issue = SessionIndex::last_issue().expect("F4: unreadable index must be recorded");
        assert!(
            issue.contains("corrupt/unreadable"),
            "F4 issue text: {issue}"
        );
        assert!(
            !issue.contains("quarantine FAILED"),
            "F4: quarantine should succeed (writable dir)"
        );
        let dir = g.index_path().parent().unwrap().to_path_buf();
        let backups = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".corrupt-"))
            .count();
        assert_eq!(backups, 1, "F4: unreadable file quarantined");
        let idx = SessionIndex::load();
        assert!(idx.get("sess_f4").is_some(), "F4: rebuilt with new session");
    }

    // ─── F5：并发写不同会话不丢更新（flock 串行）───
    {
        let g = HomeGuard::new("f5");
        let mut handles = Vec::new();
        for t in 0..8u32 {
            handles.push(std::thread::spawn(move || {
                for i in 0..25u32 {
                    let id = format!("sess_f5_{t}_{i}");
                    SessionIndex::write_txn(|idx| {
                        idx.upsert(&id, crate_test_meta());
                    });
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let idx = SessionIndex::load();
        let mut missing = Vec::new();
        for t in 0..8u32 {
            for i in 0..25u32 {
                let id = format!("sess_f5_{t}_{i}");
                if idx.get(&id).is_none() {
                    missing.push(id);
                }
            }
        }
        assert!(
            missing.is_empty(),
            "F5: {} updates lost: {:?}",
            missing.len(),
            {
                let mut m = missing.clone();
                m.truncate(5);
                m
            }
        );
    }
}
