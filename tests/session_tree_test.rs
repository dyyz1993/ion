//! Session Tree 集成测试 — 验证 branch/rollback/only-append 端到端
//! 直接操作 session 文件，不依赖 CLI/LLM

/// 辅助：构造 message entry JSON
fn msg_entry(id: &str, parent: &str, text: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "message", "id": id, "parentId": parent,
        "timestamp": "2026-07-08T10:00:00Z",
        "message": {"role": "user", "content": text}
    })
}

fn header(id: &str) -> serde_json::Value {
    serde_json::json!({"type": "session", "version": 3, "id": id, "timestamp": "x", "cwd": "/test"})
}

/// 写一个 session 文件（header + entries）
fn write_session(path: &std::path::Path, entries: &[serde_json::Value]) {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().create(true).write(true).truncate(true).open(path).unwrap();
    for e in entries {
        writeln!(f, "{}", serde_json::to_string(e).unwrap()).unwrap();
    }
}

/// 读 session 文件的所有 entries
fn read_session(path: &std::path::Path) -> Vec<serde_json::Value> {
    let content = std::fs::read_to_string(path).unwrap();
    content.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

#[test]
fn branch_appends_leaf_pointer_and_preserves_old_entries() {
    let tmp = std::env::temp_dir().join(format!("st_test_{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let path = tmp.join("session.jsonl");

    // 初始：h → m1 → m2
    let initial = vec![header("h"), msg_entry("m1", "h", "a"), msg_entry("m2", "m1", "b")];
    write_session(&path, &initial);
    let original_content = std::fs::read_to_string(&path).unwrap();

    // branch 到 m1
    let new_entries = ion::session_tree::make_branch("m1", Some("try-x")).unwrap();
    // 模拟 append_raw_entry：追加到文件
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
    for e in &new_entries {
        write!(f, "\n{}", serde_json::to_string(e).unwrap()).unwrap();
    }
    drop(f);

    // only-append 验证：原始行不变
    verify_prefix_unchanged(&path, &original_content);

    // leaf_pointer 写入了
    let after = read_session(&path);
    assert!(after.iter().any(|e| e["type"].as_str() == Some("leaf_pointer")), "leaf_pointer 写入");
    assert!(after.iter().any(|e| e["type"].as_str() == Some("label")), "label 写入");

    // current_leaf 解析正确
    let leaf = ion::session_tree::resolve_current_leaf(&after);
    assert_eq!(leaf.as_deref(), Some("m1"));

    std::fs::remove_dir_all(&tmp).ok();
}

#[test]
fn rollback_appends_tombstone_and_preserves_old_entries() {
    let tmp = std::env::temp_dir().join(format!("st_test_rb_{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let path = tmp.join("session.jsonl");

    let initial = vec![header("h"), msg_entry("m1", "h", "a"), msg_entry("m2", "m1", "b")];
    write_session(&path, &initial);
    let original_content = std::fs::read_to_string(&path).unwrap();

    // rollback 到 m1，带 reason
    let new_entries = ion::session_tree::make_rollback("m1", Some("m2"), Some("走错了")).unwrap();
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
    for e in &new_entries {
        write!(f, "\n{}", serde_json::to_string(e).unwrap()).unwrap();
    }
    drop(f);

    let after = read_session(&path);
    // only-append
    verify_prefix_unchanged(&path, &original_content);

    // tombstone
    assert!(after.iter().any(|e| e["type"].as_str() == Some("branch_summary")), "tombstone 写入");
    let tombstone = after.iter().find(|e| e["type"].as_str() == Some("branch_summary")).unwrap();
    assert!(tombstone["summary"].as_str().unwrap().contains("走错了"));

    std::fs::remove_dir_all(&tmp).ok();
}

#[test]
fn branch_then_new_message_parents_off_leaf() {
    // branch 后，新消息的 parentId 应该是 leaf（m1），不是文件末尾
    let tmp = std::env::temp_dir().join(format!("st_test_chain_{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let path = tmp.join("session.jsonl");

    let initial = vec![header("h"), msg_entry("m1", "h", "a"), msg_entry("m2", "m1", "b")];
    write_session(&path, &initial);

    // branch 到 m1
    let new_entries = ion::session_tree::make_branch("m1", None).unwrap();
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
    for e in &new_entries { write!(f, "\n{}", serde_json::to_string(e).unwrap()).unwrap(); }
    drop(f);

    // 模拟 save_session 的 leaf 感知 append：新消息 parentId = resolve_current_leaf
    let entries = read_session(&path);
    let parent_id = ion::session_tree::resolve_current_leaf(&entries).unwrap();
    assert_eq!(parent_id, "m1", "branch 后 current_leaf = m1");

    // 写一条新消息，parentId = m1（不是 m2）
    let new_msg = msg_entry("m3", &parent_id, "branch msg");
    let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
    write!(f, "\n{}", serde_json::to_string(&new_msg).unwrap()).unwrap();
    drop(f);

    // 验证树：m1 有两个子节点（m2 和 m3）
    let after = read_session(&path);
    let tree = ion::session_tree::get_tree(&after);
    assert_eq!(tree.len(), 1);
    let root = &tree[0];
    assert_eq!(root.entry["id"].as_str(), Some("m1"));
    assert_eq!(root.children.len(), 2, "m1 有两个分支");

    std::fs::remove_dir_all(&tmp).ok();
}

#[test]
fn full_branch_rollback_checkout_sequence_preserves_only_append() {
    let tmp = std::env::temp_dir().join(format!("st_test_seq_{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let path = tmp.join("session.jsonl");

    let initial = vec![header("h"), msg_entry("m1", "h", "a"), msg_entry("m2", "m1", "b")];
    write_session(&path, &initial);
    let original_content = std::fs::read_to_string(&path).unwrap();

    // 一系列操作：branch + label + rollback + checkout
    let ops = vec![
        ion::session_tree::make_branch("m1", Some("branch-a")).unwrap(),
        ion::session_tree::make_rollback("m2", Some("m1"), Some("back")).unwrap(),
    ];
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
    for op_entries in &ops {
        for e in op_entries { write!(f, "\n{}", serde_json::to_string(e).unwrap()).unwrap(); }
    }
    drop(f);

    // checkout branch-a
    let entries = read_session(&path);
    let checkout_entries = ion::session_tree::make_checkout(&entries, "branch-a").unwrap();
    let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
    for e in &checkout_entries { write!(f, "\n{}", serde_json::to_string(e).unwrap()).unwrap(); }
    drop(f);

    // only-append 核心验证
    verify_prefix_unchanged(&path, &original_content);

    // 消息一条没少
    let after = read_session(&path);
    let msg_count = after.iter().filter(|e| e["type"].as_str() == Some("message")).count();
    assert_eq!(msg_count, 2, "消息数不变（2）");

    std::fs::remove_dir_all(&tmp).ok();
}

// ── helpers ──

fn sha256(path: &std::path::Path) -> String {
    // 用文件内容的字节直接 hash（简化版，验证 only-append 用）
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let content = std::fs::read(path).unwrap_or_default();
    let mut h = DefaultHasher::new();
    content.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// 验证文件前 N 行的内容与原始内容一致（only-append 核心证明）
fn verify_prefix_unchanged(path: &std::path::Path, original_full_content: &str) {
    let current = std::fs::read_to_string(path).unwrap();
    let original_lines: Vec<&str> = original_full_content.lines().collect();
    let current_lines: Vec<&str> = current.lines().collect();
    assert!(
        current_lines.len() >= original_lines.len(),
        "行数不应减少: 原 {} 现 {}", original_lines.len(), current_lines.len()
    );
    for (i, orig) in original_lines.iter().enumerate() {
        assert_eq!(
            current_lines[i], *orig,
            "第 {} 行被修改（违反 only-append）\n  原: {}\n  现: {}",
            i + 1, orig, current_lines[i]
        );
    }
}
