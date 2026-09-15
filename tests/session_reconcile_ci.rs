// session_reconcile_ci.rs — H2 索引逆向对账（TDD）
//
// 背景：sessions.index.json 只会"落后于 JSONL"（tmp+rename 原子写），但此前
// 没有逆向对账——索引里缺的会话在 UI list_sessions 永远不可见（GC 只做
// 索引→文件单向删除）。场景：索引损坏/被删/落后大量条目时，磁盘上的会话
// 成为幽灵。
//
// 本测试验证 `session_gc::reconcile_missing`（磁盘 → 索引单向补缺）：
//   R1 补缺：磁盘 3 条、索引只有 1 条 → 补齐 2 条且字段从 header 派生
//   R2 幂等：二次跑零变化（不触发写事务，索引内容逐字节不变）
//   R3 不覆盖：索引已有条目的自定义字段（name/model 等）不被重置
//   R4 墓碑：removed_sessions 里的 id 不重建（防幽灵复活机制不被对账绕过）
//   R5 容错：首行非 JSON 的 .jsonl 计入 unreadable，不 panic、不进索引
//
// 隔离：HOME 沙箱（SessionIndex::path() 是 HOME 派生）+ ION_SESSION_DIR
// 沙箱。两个 env 都是进程级状态——单 #[test] 顺序执行全部场景，不并行。
use ion::session_gc::reconcile_missing;
use ion::session_index::{SessionIndex, SessionMeta};
use std::path::{Path, PathBuf};

// ── 沙箱 ─────────────────────────────────────────────────────────────

struct Sandbox {
    home: PathBuf,
    sessions: PathBuf,
    original_home: String,
    original_session_dir: Option<String>,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        let original_home = std::env::var("HOME").unwrap_or_default();
        let original_session_dir = std::env::var("ION_SESSION_DIR").ok();
        let home = std::env::temp_dir().join(format!(
            "ion-reconcile-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        let sessions = home.join("agent/sessions");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&sessions).unwrap();
        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("ION_SESSION_DIR", &sessions);
        }
        Self {
            home,
            sessions,
            original_home,
            original_session_dir,
        }
    }

    fn index_path(&self) -> PathBuf {
        self.home.join(".ion/agent/sessions.index.json")
    }

    /// 写一个会话 JSONL（header 行 + 若干 body 行）。
    fn write_session(&self, cwd_dir: &str, sid: &str, header: &str) -> PathBuf {
        let dir = self.sessions.join(cwd_dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{sid}.jsonl"));
        std::fs::write(&path, format!("{header}\n{{\"type\":\"message\"}}\n")).unwrap();
        path
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.home);
        unsafe {
            std::env::set_var("HOME", &self.original_home);
            match &self.original_session_dir {
                Some(v) => std::env::set_var("ION_SESSION_DIR", v),
                None => std::env::remove_var("ION_SESSION_DIR"),
            }
        }
    }
}

// ── 测试数据 ─────────────────────────────────────────────────────────

fn full_meta(name: Option<&str>, model: &str) -> SessionMeta {
    SessionMeta {
        name: name.map(String::from),
        first_name: name.map(String::from),
        project: None,
        project_name: None,
        worktree: false,
        branch: None,
        model: model.into(),
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
        message_count: 7,
        turn_count: 2,
        created_at: 111,
        updated_at: 222,
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

/// 标准 header 行（对齐 session_jsonl::ensure_session_header 的产出形状）。
fn header_json(sid: &str, cwd: &str, ts: &str, extra: &str) -> String {
    format!(
        r#"{{"type":"session","version":3,"id":"{sid}","timestamp":"{ts}","cwd":"{cwd}"{extra}}}"#
    )
}

fn read_index_ids(sbx: &Sandbox) -> Vec<String> {
    let raw = std::fs::read_to_string(sbx.index_path()).unwrap_or_default();
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap_or_default();
    v["sessions"]
        .as_object()
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default()
}

// ── 全部场景（单测试顺序执行，env 进程级）───────────────────────────

#[test]
fn reconcile_rebuilds_missing_index_entries_from_disk() {
    // ═══ R1 补缺：磁盘 3 条、索引只有 1 条 → 补齐 2 条（header 派生）═══
    {
        let sbx = Sandbox::new("r1");

        let cwd_a = "/tmp/proj-a";
        let cwd_b = "/tmp/proj-b";
        // 磁盘 3 条：sess_a（在索引）、sess_b / sess_c（缺失，sess_c 带 parentSession）
        sbx.write_session(
            "--h1--a--",
            "sess_a",
            &header_json("sess_a", cwd_a, "2025-06-01T12:00:00.000Z", ""),
        );
        sbx.write_session(
            "--h2--b--",
            "sess_b",
            &header_json(
                "sess_b",
                cwd_b,
                "2025-06-01T12:00:00.000Z",
                r#","agent":"reviewer","model":"glm-5.2","provider":"zai""#,
            ),
        );
        sbx.write_session(
            "--h2--b--",
            "sess_c",
            &header_json(
                "sess_c",
                cwd_b,
                "2025-06-01T12:00:00.000Z",
                r#","parentSession":"sess_a""#,
            ),
        );

        // 索引先只有 sess_a（带自定义字段，供 R3 断言不被覆盖）
        SessionIndex::write_txn(|idx| {
            idx.upsert("sess_a", full_meta(Some("custom-name-a"), "custom-model-a"));
        });

        let before = read_index_ids(&sbx);
        assert_eq!(before, vec!["sess_a".to_string()], "前置：索引只有 sess_a");

        let report = reconcile_missing(&sbx.sessions_dir_as_path());

        assert_eq!(report.disk_files, 3, "R1: 磁盘扫描到 3 个 jsonl");
        assert_eq!(report.rebuilt, 2, "R1: 应重建缺失的 2 条");
        assert_eq!(report.skipped_tombstoned, 0, "R1: 无墓碑命中");

        let ids = read_index_ids(&sbx);
        assert_eq!(ids.len(), 3, "R1: 索引补齐后应有 3 条");
        assert!(ids.contains(&"sess_b".to_string()));
        assert!(ids.contains(&"sess_c".to_string()));

        // 字段正确性（header 派生）
        let idx = SessionIndex::load();
        let b = idx.get("sess_b").expect("R1: sess_b 应在索引");
        assert_eq!(b.project.as_deref(), Some(cwd_b), "R1: project 来自 header.cwd");
        assert_eq!(
            b.project_name.as_deref(),
            Some("proj-b"),
            "R1: project_name 是 cwd 末段"
        );
        assert_eq!(b.model, "glm-5.2", "R1: model 来自 header");
        assert_eq!(b.provider, "zai", "R1: provider 来自 header");
        assert_eq!(b.agent, "reviewer", "R1: agent 来自 header");
        assert_eq!(b.name, None, "R1: header 无 name → 留空");
        assert_eq!(
            b.created_at, 1748779200000,
            "R1: created_at 由 header.timestamp 解析（2025-06-01T12:00:00Z UTC）"
        );
        let c = idx.get("sess_c").expect("R1: sess_c 应在索引");
        assert_eq!(
            c.parent_session.as_deref(),
            Some("sess_a"),
            "R1: parent_session 来自 header.parentSession"
        );
        assert_eq!(c.agent, "default", "R1: header 无 agent → default 兜底");
    }

    // ═══ R2 幂等 + R3 不覆盖已有 ═══
    {
        let sbx = Sandbox::new("r2");

        sbx.write_session(
            "--h1--a--",
            "sess_a",
            &header_json("sess_a", "/tmp/proj-a", "2025-06-01T12:00:00.000Z", ""),
        );
        sbx.write_session(
            "--h1--a--",
            "sess_b",
            &header_json("sess_b", "/tmp/proj-a", "2025-06-01T12:00:00.000Z", ""),
        );
        SessionIndex::write_txn(|idx| {
            idx.upsert("sess_a", full_meta(Some("custom-name-a"), "custom-model-a"));
        });

        let r1 = reconcile_missing(&sbx.sessions_dir_as_path());
        assert_eq!(r1.rebuilt, 1, "R2 前置：只补 sess_b");

        let index_raw_after_first = std::fs::read_to_string(sbx.index_path()).unwrap();

        // R3：sess_a 的自定义字段未被对账重置
        let idx = SessionIndex::load();
        let a = idx.get("sess_a").unwrap();
        assert_eq!(a.name.as_deref(), Some("custom-name-a"), "R3: name 不被覆盖");
        assert_eq!(a.model, "custom-model-a", "R3: model 不被覆盖");
        assert_eq!(a.message_count, 7, "R3: 已有统计不被清零");

        // R2：二次跑零变化
        let r2 = reconcile_missing(&sbx.sessions_dir_as_path());
        assert_eq!(r2.rebuilt, 0, "R2: 二次跑重建数为 0");
        assert_eq!(r2.disk_files, r1.disk_files, "R2: 磁盘扫描数一致");
        let index_raw_after_second = std::fs::read_to_string(sbx.index_path()).unwrap();
        assert_eq!(
            index_raw_after_first, index_raw_after_second,
            "R2: 二次跑索引文件逐字节不变（零差异不触发写事务）"
        );
    }

    // ═══ R4 墓碑尊重：removed_sessions 里的 id 不重建 ═══
    {
        let sbx = Sandbox::new("r4");

        sbx.write_session(
            "--h1--a--",
            "sess_tomb",
            &header_json("sess_tomb", "/tmp/proj-a", "2025-06-01T12:00:00.000Z", ""),
        );
        sbx.write_session(
            "--h1--a--",
            "sess_live",
            &header_json("sess_live", "/tmp/proj-a", "2025-06-01T12:00:00.000Z", ""),
        );
        // 墓碑：sess_tomb 被 session_remove 显式删除（文件已删但磁盘上又出现，
        // 或对账扫描时墓碑仍在）——对账必须跳过，不能让幽灵复活
        SessionIndex::write_txn(|idx| {
            idx.removed_sessions.insert("sess_tomb".to_string());
        });

        let report = reconcile_missing(&sbx.sessions_dir_as_path());

        assert_eq!(
            report.skipped_tombstoned, 1,
            "R4: 墓碑命中的候选应被跳过并计数"
        );
        assert_eq!(report.rebuilt, 1, "R4: 只重建非墓碑的 sess_live");
        let ids = read_index_ids(&sbx);
        assert!(
            !ids.contains(&"sess_tomb".to_string()),
            "R4: 墓碑 id 不得进入索引（防幽灵复活）"
        );
        assert!(ids.contains(&"sess_live".to_string()));
    }

    // ═══ R5 容错：坏 header 计 unreadable，不 panic ═══
    {
        let sbx = Sandbox::new("r5");

        sbx.write_session("--h1--a--", "sess_bad", "this is not json at all");
        sbx.write_session(
            "--h1--a--",
            "sess_good",
            &header_json("sess_good", "/tmp/proj-a", "2025-06-01T12:00:00.000Z", ""),
        );

        let report = reconcile_missing(&sbx.sessions_dir_as_path());
        assert_eq!(report.unreadable, 1, "R5: 坏 header 计入 unreadable");
        assert_eq!(report.rebuilt, 1, "R5: 好文件正常重建");
        let ids = read_index_ids(&sbx);
        assert!(!ids.contains(&"sess_bad".to_string()), "R5: 坏文件不进索引");
    }
}

impl Sandbox {
    fn sessions_dir_as_path(&self) -> &Path {
        &self.sessions
    }
}
