//! Memory 插件 E2E 集成测试
//!
//! 直接测试 MemoryStore 数据层 + MemoryExtension 生命周期钩子，
//! 不依赖 Manager 进程（通过 Rust API 直接调用）。

use std::sync::Arc;
use tokio::sync::Mutex;
use ion::agent::memory::{MemoryStore, MemoryExtension, MemoryEntry};
use ion::agent::extension::Extension;

/// 测试用 JSON 文件存储（不走全局 SQLite，避免测试间数据污染）
fn ensure_test_mode() {
    // SAFETY: 测试单线程运行
    unsafe { std::env::set_var("ION_MEMORY_NO_GLOBAL", "1"); }
}

/// 测试用临时目录（同时清理 ~/.ion/agent/project-data 下对应的测试数据）
fn tmp_dir(name: &str) -> String {
    ensure_test_mode();
    let p = std::env::temp_dir().join(format!("ion_mem_test_{name}"));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    // 清理之前测试可能留下的 project data
    let ion_home = std::path::Path::new(&std::env::var("HOME").unwrap_or_default())
        .join(".ion").join("agent").join("project-data");
    if ion_home.exists() {
        if let Ok(entries) = std::fs::read_dir(&ion_home) {
            for entry in entries.flatten() {
                let dir_name = entry.file_name().to_string_lossy().to_string();
                if dir_name.contains(name) {
                    let _ = std::fs::remove_dir_all(entry.path());
                }
            }
        }
    }
    p.to_string_lossy().to_string()
}

/// 测试用 session ID
fn sess_id() -> String { "test_sess_001".to_string() }

#[tokio::test]
async fn test_store_save_and_search() {
    let root = tmp_dir("save_search");
    let store = MemoryStore::new(&root, &sess_id());

    // 保存 3 条记忆
    let id1 = store.save_entry("用户喜欢 Rust", "语言偏好", "编程", &["rust".into(), "lang".into()], "auto");
    let id2 = store.save_entry("用户喜欢 TypeScript", "语言偏好", "编程", &["ts".into(), "lang".into()], "auto");
    store.save_entry("项目使用 Tokio", "技术选型", "技术", &["tokio".into(), "async".into()], "tech");

    // 验证 ID 正确
    assert_eq!(id1, "mem_1", "first entry id");
    assert_eq!(id2, "mem_2", "second entry id");

    // 搜索 tag "rust" 命中
    let r = store.search("用 Rust 写代码", None);
    assert_eq!(r.len(), 1, "search rust should find 1");
    assert_eq!(r[0].content, "用户喜欢 Rust");

    // 搜索 tag "tokio" 命中
    let r = store.search("Tokio 框架怎么用", None);
    assert_eq!(r.len(), 1, "search tokio should find 1");

    // 搜索无匹配
    let r = store.search("Python 怎么样", None);
    assert_eq!(r.len(), 0, "search python should find 0");

    // 空 query 返回全部
    let r = store.search("", None);
    assert_eq!(r.len(), 3, "empty query returns all");

    // 按 outline 过滤
    let r = store.search("rust", Some("tech"));
    assert_eq!(r.len(), 0, "search rust in tech outline should find 0");
}

#[tokio::test]
async fn test_store_forget_soft_delete() {
    let root = tmp_dir("forget");
    let store = MemoryStore::new(&root, &sess_id());

    store.save_entry("用户喜欢 Rust", "", "", &["rust".into()], "auto");
    store.save_entry("用户喜欢 TS", "", "", &["ts".into()], "auto");

    // forget → archived
    let mut all = store.read_outline("auto");
    assert_eq!(all.len(), 2);

    // 手动 forget
    for e in &mut all {
        if e.id == "mem_1" { e.archived = true; }
    }
    store.write_outline("auto", &all);

    // search 过滤 archived
    let r = store.search("rust", None);
    assert_eq!(r.len(), 0, "forgotten entry not in search");

    // inspect 能查到 archived
    let entries = store.read_outline("auto");
    let archived = entries.iter().find(|e| e.id == "mem_1").unwrap();
    assert!(archived.archived, "forgotten entry has archived=true");
}

#[tokio::test]
async fn test_store_content_hash() {
    let root = tmp_dir("hash");
    let store = MemoryStore::new(&root, &sess_id());

    // 初始 hash
    store.save_entry("内容 A", "", "", &["a".into()], "auto");
    let h1 = store.content_hash("auto");

    // 新增条目 → hash 变化
    store.save_entry("内容 B", "", "", &["b".into()], "auto");
    let h2 = store.content_hash("auto");
    assert_ne!(h1, h2, "adding entry changes hash");

    // 相同内容 → hash 不变（幂等）
    let h3 = store.content_hash("auto");
    assert_eq!(h2, h3, "no change means same hash");
}

#[tokio::test]
async fn test_extension_on_system_prompt() {
    let root = tmp_dir("sysprompt");
    let ext = MemoryExtension::new(&root, &sess_id());

    // 无记忆 → 不注入
    let mut prompt = "你是助手。".to_string();
    let r = ext.on_system_prompt(&mut prompt).await;
    assert!(r.is_ok());
    assert!(!prompt.contains("<memory_outline>"), "no memory, no outline");

    // save 一条记忆
    let store = ext.store.lock().await;
    store.save_entry("用户偏好", "", "", &["pref".into()], "auto");
    drop(store);

    // 有记忆 → 注入 outline
    let mut prompt = "你是助手。".to_string();
    let r = ext.on_system_prompt(&mut prompt).await;
    assert!(r.is_ok());
    assert!(prompt.contains("<memory_outline>"), "has memory, has outline");
    assert!(prompt.contains("auto"), "outline id visible");
}

#[tokio::test]
async fn test_store_pending_and_inject() {
    let root = tmp_dir("inject");
    let store = MemoryStore::new(&root, &sess_id());
    let store = Arc::new(Mutex::new(store));

    // 保存记忆
    {
        let mut s = store.lock().await;
        s.save_entry("偏好 Rust 编程", "语言偏好", "技术", &["rust".into(), "lang".into()], "auto");
    }

    // 模拟 on_input 的搜索 + pending 逻辑
    {
        let mut s = store.lock().await;
        s.turn_count += 1;
        let results = s.search("用 Rust 写代码要注意什么？", None);
        assert!(!results.is_empty(), "search should find memories");
        let mut by_outline: std::collections::HashMap<String, Vec<ion::agent::memory::MemoryEntry>> = std::collections::HashMap::new();
        for e in results { by_outline.entry(e.outline.clone()).or_default().push(e); }
        for (oid, entries) in &by_outline {
            let xml = s.build_context_xml(oid, entries);
            s.pending.push(ion::agent::memory::PendingInject { outline: oid.clone(), xml });
        }
        assert_eq!(s.pending.len(), 1, "should add to pending");
        assert!(s.pending[0].xml.contains("<memory_context"));
    }

    // 模拟 on_context
    {
        let mut s = store.lock().await;
        let n = s.pending.len();
        // 消费 pending
        while let Some(p) = s.pending.pop() {
            // 在真实场景这里会 push 到 messages
            assert!(p.xml.contains("<memory_context"));
            // 更新 injected.json
            s.write_injected(&[ion::agent::memory::InjectRecord {
                outline: p.outline.clone(),
                file_hash: "test_hash".into(),
                last_injected_turn: s.turn_count,
                last_injected_at: 0,
            }]);
        }
        assert_eq!(s.pending.len(), 0, "pending consumed");

        // 验证 injected
        let records = s.read_injected();
        assert_eq!(records.len(), 1, "injected record created");
        assert_eq!(records[0].outline, "auto");
    }
}

#[tokio::test]
async fn test_outline_sanitization() {
    let root = tmp_dir("sanitize");
    let store = MemoryStore::new(&root, &sess_id());

    // 合法 outline
    store.save_entry("内容", "", "", &[], "valid-outline_123");
    let idx = store.read_index();
    assert!(idx.iter().any(|i| i.id == "valid-outline_123"), "valid outline accepted");

    // 非法 outline（含路径穿越）→ 文字被净化，只保留字母数字_-
    store.save_entry("另一个内容", "", "", &[], "../../../etc/passwd");
    let idx = store.read_index();
    // 净化后斜杠和点被移除，无法穿越目录
    // "etcpasswd" 作为一个纯字母 outline 是合法的
    let has_clean = idx.iter().any(|i| i.id == "etcpasswd");
    assert!(has_clean, "sanitized outline should be accepted");
    let has_dirty = idx.iter().any(|i| i.id.contains('/') || i.id.contains('.'));
    assert!(!has_dirty, "no path separators in outline names");
}
