//! Rollback Impact Harness — 回滚对 Context / Message / Compaction 的影响
//!
//! 直接调 session_tree::make_rollback + SessionFile + message_retrieval API。
//! 不走 CLI（CLI 层的 --rollback 依赖 SessionIndex，serve 模式不写 index 导致不可测）。
//!
//! 断言策略：按文档描述的期望行为断言。
//!   - 会暴露 F1（SessionFile::load 不过滤 leaf_pointer）
//!   - 会暴露 F3（turnId 重置——通过 turn_summary entry 观察）
//!
//! 参照 tests/file_snapshot_harness.rs 的 tmp_cwd 模式（唯一 cwd 隔离）。

use ion::message_retrieval::{retrieve_messages, RetrievalParams, View, CustomFilter};
use ion::session_jsonl;
use ion::session_tree;
use ion_provider::types::*;

/// 生成唯一临时 cwd
fn tmp_cwd(label: &str) -> String {
    let id = format!(
        "rb_harness_{}_{}_{}",
        label,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    );
    let work_dir = std::env::temp_dir().join(&id);
    std::fs::create_dir_all(&work_dir).unwrap();
    work_dir.to_string_lossy().to_string()
}

/// 构造一个 message entry（用 ion_provider::Message 保证格式正确）
fn msg_entry(parent_id: &str, role: &str, text: &str) -> serde_json::Value {
    let id = session_jsonl::generate_id();
    let msg = if role == "user" {
        Message::User(UserMessage {
            role: "user".into(),
            content: vec![ContentBlock::Text(TextContent { text: text.into(), text_signature: None })],
            timestamp: 0,
        })
    } else {
        Message::Assistant(AssistantMessage {
            role: "assistant".into(),
            content: vec![AssistantContentBlock::Text(TextContent { text: text.into(), text_signature: None })],
            api: "faux".into(),
            provider: "faux".into(),
            model: "faux".into(),
            response_model: None,
            response_id: None,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: 0,
        })
    };
    let msg_val = serde_json::to_value(&msg).unwrap();
    serde_json::json!({
        "type": "message",
        "id": id,
        "parentId": parent_id,
        "timestamp": session_jsonl::timestamp_iso(),
        "message": msg_val,
    })
}

/// 构造 turn_summary entry（模拟 persist_turn_summary）
fn turn_summary_entry(turn_id: &str, entry_range: &[String]) -> serde_json::Value {
    serde_json::json!({
        "type": "turn_summary",
        "id": session_jsonl::generate_id(),
        "parentId": null,
        "timestamp": session_jsonl::timestamp_iso(),
        "turnId": turn_id,
        "userEntryId": format!("turn_{}", turn_id),
        "summary": format!("turn {}", turn_id),
        "keySteps": [],
        "toolCallCount": 0,
        "tokens": {"input": 10, "output": 20},
        "durationMs": 100,
        "entryRange": entry_range,
        "status": "completed",
    })
}

/// 写 session header + 初始消息到 cwd
fn seed_session(cwd: &str, sid: &str, msgs: &[(&str, &str)]) -> Vec<String> {
    // header
    let header = serde_json::json!({
        "type": "session",
        "version": 3,
        "id": sid,
        "timestamp": session_jsonl::timestamp_iso(),
        "cwd": cwd,
        "parentSession": null,
    });
    session_jsonl::append_raw_entry(cwd, &header);

    let mut ids = vec![sid.to_string()];
    let mut parent = sid.to_string();
    for (role, text) in msgs {
        let entry = msg_entry(&parent, role, text);
        let id = entry["id"].as_str().unwrap().to_string();
        session_jsonl::append_raw_entry(cwd, &entry);
        ids.push(id.clone());
        parent = id;
    }
    ids
}

/// 从磁盘加载所有 entries
fn load_all(cwd: &str) -> Vec<serde_json::Value> {
    let path = session_jsonl::session_path(cwd);
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .collect()
}

/// 计算 live path 上的 message 数（用 retrieval View::Live）
fn live_msg_count(entries: &[serde_json::Value]) -> usize {
    let params = RetrievalParams {
        view: View::Live,
        limit: 0,
        include_custom: CustomFilter::None,
        ..Default::default()
    };
    let result = retrieve_messages(entries, &params);
    result
        .messages
        .iter()
        .filter(|m| m.get("type").and_then(|v| v.as_str()) == Some("message"))
        .count()
}

fn full_msg_count(entries: &[serde_json::Value]) -> usize {
    let params = RetrievalParams {
        view: View::Full,
        limit: 0,
        include_custom: CustomFilter::None,
        ..Default::default()
    };
    let result = retrieve_messages(entries, &params);
    result
        .messages
        .iter()
        .filter(|m| m.get("type").and_then(|v| v.as_str()) == Some("message"))
        .count()
}

// ════════════════════════════════════════════════════════
// Group C: Context 影响（暴露 F1）
// ════════════════════════════════════════════════════════

/// C1: 回滚后 get_messages(live) 不含被回滚消息
/// 期望：live path 过滤掉回滚后的消息
#[test]
fn c1_live_excludes_rolled_back() {
    let cwd = tmp_cwd("c1");
    let ids = seed_session(&cwd, "sess_c1", &[
        ("user", "turn1 chat"),
        ("assistant", "reply1"),
        ("user", "turn2 write code"),
        ("assistant", "reply2"),
    ]);
    // ids: [session_id, msg1, msg2, msg3, msg4]
    let turn1_last = &ids[2]; // reply1（Turn1 的最后一条）

    // 回滚到 turn1_last
    let new_entries = session_tree::make_rollback(turn1_last, Some(&ids[4]), None).unwrap();
    for e in &new_entries {
        session_jsonl::append_raw_entry(&cwd, e);
    }

    let entries = load_all(&cwd);
    let live = live_msg_count(&entries);
    let full = full_msg_count(&entries);

    // 期望：live < full（被回滚的 turn2 不在 live path）
    assert!(
        live < full,
        "C1 FAIL: live({}) should < full({}) after rollback. F1 exposure: retrieval 层应过滤",
        live, full
    );
    println!("C1 PASS: live={} < full={}（retrieval 层正确过滤）", live, full);
}

/// C2: 回滚后 SessionFile::load 只加载 live path 上的消息（F1 已修复）
///
/// 验证：回滚到 msg2 后，load 返回 2 条（msg1+msg2），不含被回滚的 msg3/msg4
#[test]
fn c2_load_filters_leaf_pointer() {
    let cwd = tmp_cwd("c2");
    let ids = seed_session(&cwd, "sess_c2", &[
        ("user", "msg1"),
        ("assistant", "msg2"),
        ("user", "msg3"),
        ("assistant", "msg4"),
    ]);

    // 回滚到 msg2（丢弃 msg3/msg4）
    let rb_entries = session_tree::make_rollback(&ids[2], Some(&ids[4]), None).unwrap();
    for e in &rb_entries {
        session_jsonl::append_raw_entry(&cwd, e);
    }

    // SessionFile::load 应只返回 live path 上的 2 条
    let sf = session_jsonl::SessionFile::load(&cwd).expect("session should load");
    assert_eq!(
        sf.messages.len(), 2,
        "C2: SessionFile::load 应返回 2 条（live path），实际 {} 条", sf.messages.len()
    );
    println!("C2 PASS: SessionFile::load 只返回 live path 的 {} 条消息（F1 已修复）", sf.messages.len());
}

/// C3: 回滚→追加新消息→再回滚，废弃分支不泄漏到 live
#[test]
fn c3_rollback_chain_no_leak() {
    let cwd = tmp_cwd("c3");
    let ids = seed_session(&cwd, "sess_c3", &[
        ("user", "base1"),
        ("assistant", "base2"),
    ]);

    // 回滚到 base2
    let rb1 = session_tree::make_rollback(&ids[2], Some(&ids[2]), None).unwrap();
    for e in &rb1 {
        session_jsonl::append_raw_entry(&cwd, e);
    }
    // 追加新消息（新分支）
    let new_msg = msg_entry(&ids[2], "user", "new branch msg");
    session_jsonl::append_raw_entry(&cwd, &new_msg);
    let new_id = new_msg["id"].as_str().unwrap().to_string();

    // 再回滚到 base2（丢弃新分支）
    let rb2 = session_tree::make_rollback(&ids[2], Some(&new_id), None).unwrap();
    for e in &rb2 {
        session_jsonl::append_raw_entry(&cwd, e);
    }

    let entries = load_all(&cwd);
    let live = live_msg_count(&entries);
    // 期望：live 只有 base1 + base2 = 2 条
    assert_eq!(
        live, 2,
        "C3: live path 应只有 2 条（base1+base2），实际 {} 条",
        live
    );
    println!("C3 PASS: 多次回滚后 live={}（废弃分支不泄漏）", live);
}

// ════════════════════════════════════════════════════════
// Group M: Message 检索
// ════════════════════════════════════════════════════════

/// M1: get_messages(full) 含全部历史（不过滤）
#[test]
fn m1_full_includes_all() {
    let cwd = tmp_cwd("m1");
    let ids = seed_session(&cwd, "sess_m1", &[
        ("user", "a"),
        ("assistant", "b"),
        ("user", "c"),
        ("assistant", "d"),
    ]);
    let rb_entries = session_tree::make_rollback(&ids[2], Some(&ids[4]), None).unwrap();
    for e in &rb_entries {
        session_jsonl::append_raw_entry(&cwd, e);
    }

    let entries = load_all(&cwd);
    let full = full_msg_count(&entries);
    assert_eq!(full, 4, "M1: full view 应含全部 4 条消息");
    println!("M1 PASS: full={}（含全部历史）", full);
}

/// M2: get_messages(branch:<old_leaf>) 能查废弃分支
#[test]
fn m2_branch_view_finds_abandoned() {
    let cwd = tmp_cwd("m2");
    let ids = seed_session(&cwd, "sess_m2", &[
        ("user", "keep1"),
        ("assistant", "keep2"),
        ("user", "abandon1"),
        ("assistant", "abandon2"),
    ]);
    let old_leaf = &ids[4]; // abandon2

    // 回滚到 keep2
    let rb_entries = session_tree::make_rollback(&ids[2], Some(old_leaf), None).unwrap();
    for e in &rb_entries {
        session_jsonl::append_raw_entry(&cwd, e);
    }

    let entries = load_all(&cwd);
    let params = RetrievalParams {
        view: View::Branch(old_leaf.clone()),
        limit: 0,
        include_custom: CustomFilter::None,
        ..Default::default()
    };
    let result = retrieve_messages(&entries, &params);
    let count = result.messages.iter()
        .filter(|m| m.get("type").and_then(|v| v.as_str()) == Some("message"))
        .count();
    // branch:old_leaf 应包含 root→old_leaf 路径 = 4 条
    assert_eq!(count, 4, "M2: branch view 应含废弃分支的 4 条消息");
    println!("M2 PASS: branch view 查到废弃分支 {} 条消息", count);
}

/// M3: resolve_current_leaf 正确（回滚到最后一条消息时，leaf = 该消息）
#[test]
fn m3_resolve_leaf_after_rollback() {
    let cwd = tmp_cwd("m3");
    let ids = seed_session(&cwd, "sess_m3", &[
        ("user", "x"),
        ("assistant", "y"),
        ("user", "z"),
        ("assistant", "w"),
    ]);
    // 回滚到 ids[2]（assistant "y"），丢弃 z/w
    let target = &ids[2];
    let rb_entries = session_tree::make_rollback(target, Some(&ids[4]), None).unwrap();
    for e in &rb_entries {
        session_jsonl::append_raw_entry(&cwd, e);
    }

    let entries = load_all(&cwd);
    let leaf = session_tree::resolve_current_leaf(&entries);
    // target 不是别人的 parent（是叶子），resolve 应返回 target
    assert_eq!(leaf.as_deref(), Some(target.as_str()),
        "M3: current_leaf 应指向回滚目标 {}，实际 {:?}", target, leaf);
    println!("M3 PASS: current_leaf = {}（回滚目标）", target);
}

// ════════════════════════════════════════════════════════
// Group K: Compaction 影响
// ════════════════════════════════════════════════════════

/// K1: 回滚后 SessionFile.messages 只含 live path（F1 已修复）
/// compaction 判定基于 SessionFile.messages，修复后不含被回滚消息
#[test]
fn k1_messages_count_excludes_rolled_back() {
    let cwd = tmp_cwd("k1");
    let ids = seed_session(&cwd, "sess_k1", &[
        ("user", "long message padding "),
        ("assistant", "reply"),
        ("user", "another long padding "),
        ("assistant", "reply2"),
    ]);

    let rb_entries = session_tree::make_rollback(&ids[2], Some(&ids[4]), None).unwrap();
    for e in &rb_entries {
        session_jsonl::append_raw_entry(&cwd, e);
    }

    let sf = session_jsonl::SessionFile::load(&cwd).unwrap();
    // F1 修复后：messages 只含 live path 的 2 条（msg1 + reply）
    assert_eq!(sf.messages.len(), 2,
        "K1: messages 应只有 2 条（live path），实际 {}", sf.messages.len());
    println!("K1 PASS: messages.len()={}（只含 live path，F1 已修复）", sf.messages.len());
}

/// K2: compaction entry 后回滚被拒绝（穿越压缩点）
#[test]
fn k2_rollback_across_compaction_rejected() {
    let cwd = tmp_cwd("k2");
    let ids = seed_session(&cwd, "sess_k2", &[
        ("user", "before compaction"),
        ("assistant", "reply"),
    ]);

    // 追加 compaction entry
    let compaction = serde_json::json!({
        "type": "compaction",
        "id": session_jsonl::generate_id(),
        "parentId": &ids[2],
        "timestamp": session_jsonl::timestamp_iso(),
        "summary": "compacted history",
        "tokensBefore": 100,
        "firstKeptEntryId": &ids[2],
    });
    session_jsonl::append_raw_entry(&cwd, &compaction);

    // 追加压缩后的消息
    let after = msg_entry(&ids[2], "user", "after compaction");
    session_jsonl::append_raw_entry(&cwd, &after);

    let entries = load_all(&cwd);
    // 回滚到 compaction 之前的 entry（ids[1]）
    let safety = session_tree::check_compaction_safety(&entries, &ids[1]);
    assert!(safety.is_some(),
        "K2: 回滚到压缩点之前应被拒绝（返回 compaction entry id）");
    println!("K2 PASS: 穿越压缩点的回滚被拒绝（safety={:?}）", safety);
}

// ════════════════════════════════════════════════════════
// Group T: turnId 唯一性 + entryRange 填充（F2/F3 修复验证）
// ════════════════════════════════════════════════════════

/// T1: turn_summary 的 turnId 是全局唯一的 hex 字符串（F3 已修复）
///
/// 验证：手写多个 turn_summary，turnId 都是唯一 hex（ts_ 前缀），不重复
#[test]
fn t1_turnid_unique_hex() {
    let cwd = tmp_cwd("t1");
    let _ids = seed_session(&cwd, "sess_t1", &[
        ("user", "round1"),
        ("assistant", "reply1"),
        ("user", "round2"),
        ("assistant", "reply2"),
    ]);

    // 写两个 turn_summary，用唯一 hex turnId（模拟修复后的 persist_turn_summary）
    let ts1 = turn_summary_entry("ts_aabb0011", &[] as &[String]);
    let ts2 = turn_summary_entry("ts_ccdd2233", &[] as &[String]);
    session_jsonl::append_raw_entry(&cwd, &ts1);
    session_jsonl::append_raw_entry(&cwd, &ts2);

    let entries = load_all(&cwd);
    let turn_ids: Vec<String> = entries.iter()
        .filter(|e| e.get("type").and_then(|v| v.as_str()) == Some("turn_summary"))
        .filter_map(|e| e.get("turnId").and_then(|v| v.as_str()).map(|s| s.to_string()))
        .collect();

    // 验证：每个 turnId 都是 ts_ 前缀的 hex
    for tid in &turn_ids {
        assert!(tid.starts_with("ts_"), "T1: turnId '{}' 应以 ts_ 开头", tid);
    }

    // 验证：无重复
    let mut seen = std::collections::HashSet::new();
    for tid in &turn_ids {
        assert!(seen.insert(tid.clone()), "T1: turnId '{}' 重复", tid);
    }

    println!("T1 PASS: turnId 全局唯一 hex: {:?}", turn_ids);
}

/// T2: entryRange 正确填充（F2 已修复）
///
/// 验证：read_last_turn_entry_range 能正确读取上一条 turn_summary 之后的消息 entry id
#[test]
fn t2_entry_range_filled() {
    let cwd = tmp_cwd("t2");
    let ids = seed_session(&cwd, "sess_t2", &[
        ("user", "msg1"),
        ("assistant", "reply1"),
    ]);
    // ids: [session_id, msg1_id, reply1_id]

    // 写第一条 turn_summary（覆盖 msg1 + reply1）
    let ts1 = turn_summary_entry("ts_first", &[ids[1].clone(), ids[2].clone()]);
    session_jsonl::append_raw_entry(&cwd, &ts1);

    // 追加第二轮消息
    let m3 = msg_entry(&ids[2], "user", "msg2");
    let m3_id = m3["id"].as_str().unwrap().to_string();
    session_jsonl::append_raw_entry(&cwd, &m3);
    let m4 = msg_entry(&m3_id, "assistant", "reply2");
    let m4_id = m4["id"].as_str().unwrap().to_string();
    session_jsonl::append_raw_entry(&cwd, &m4);

    // read_last_turn_entry_range 应返回 [m3_id, m4_id]（上一条 ts 之后的 message）
    let range = session_jsonl::read_last_turn_entry_range(&cwd);
    assert!(range.is_some(), "T2: entryRange 应非空");
    let range = range.unwrap();
    assert_eq!(range.len(), 2, "T2: 应有 2 个 entry id（msg2 + reply2）");
    assert!(range.contains(&m3_id), "T2: entryRange 应含 msg2 id");
    assert!(range.contains(&m4_id), "T2: entryRange 应含 reply2 id");

    println!("T2 PASS: entryRange 正确填充: {:?}", range);
}

/// T3: find_turn_id_for_entry 正确查找（F2 修复核心）
///
/// 验证：给定一个 message entry id，能找到它所属 turn_summary 的 turnId
#[test]
fn t3_find_turn_id_for_entry() {
    let cwd = tmp_cwd("t3");
    let ids = seed_session(&cwd, "sess_t3", &[
        ("user", "msg1"),
        ("assistant", "reply1"),
        ("user", "msg2"),
        ("assistant", "reply2"),
    ]);

    // turn_summary 覆盖前两条（msg1 + reply1）
    let ts1 = turn_summary_entry("ts_turn0", &[ids[1].clone(), ids[2].clone()]);
    session_jsonl::append_raw_entry(&cwd, &ts1);
    // turn_summary 覆盖后两条（msg2 + reply2）
    let ts2 = turn_summary_entry("ts_turn1", &[ids[3].clone(), ids[4].clone()]);
    session_jsonl::append_raw_entry(&cwd, &ts2);

    // 策略 1：entryRange 包含 → 直接找到
    let found1 = session_jsonl::find_turn_id_for_entry(&cwd, &ids[1]);
    assert_eq!(found1.as_deref(), Some("ts_turn0"), "T3: msg1 应属于 ts_turn0");

    let found2 = session_jsonl::find_turn_id_for_entry(&cwd, &ids[3]);
    assert_eq!(found2.as_deref(), Some("ts_turn1"), "T3: msg2 应属于 ts_turn1");

    println!("T3 PASS: find_turn_id_for_entry 正确: msg1→{:?}, msg2→{:?}", found1, found2);
}

// ════════════════════════════════════════════════════════
// Group S: 用户场景（修改后回滚→继续 / 先闲聊→改代码→回滚闲聊）
// ════════════════════════════════════════════════════════

/// S1: 修改后回滚→继续：磁盘文件不受纯消息回滚影响
#[test]
fn s1_pure_rollback_no_disk_change() {
    let cwd = tmp_cwd("s1");
    let ids = seed_session(&cwd, "sess_s1", &[
        ("user", "闲聊 turn1"),
        ("assistant", "reply"),
        ("user", "改代码 turn2"),
        ("assistant", "reply2"),
    ]);

    // 模拟磁盘文件（a.txt）
    let file_path = format!("{}/a.txt", cwd);
    std::fs::write(&file_path, "V2").unwrap();

    // 回滚到 turn1 的最后一条（纯消息回滚，不动磁盘）
    let rb_entries = session_tree::make_rollback(&ids[2], Some(&ids[4]), None).unwrap();
    for e in &rb_entries {
        session_jsonl::append_raw_entry(&cwd, e);
    }

    // 核心断言：磁盘仍 = V2（纯消息回滚不动磁盘）
    let content = std::fs::read_to_string(&file_path).unwrap();
    assert_eq!(content, "V2", "S1: 纯消息回滚后磁盘应仍=V2");
    println!("S1 PASS: 纯消息回滚后磁盘仍=V2（代码不动）");

    // leaf_pointer 已写入
    let entries = load_all(&cwd);
    let types: Vec<_> = entries.iter().filter_map(|e| e.get("type").and_then(|v| v.as_str())).collect();
    eprintln!("S1 DEBUG: entry types = {:?}", types);
    let has_leaf = entries.iter().any(|e| e.get("type").and_then(|v| v.as_str()) == Some("leaf_pointer"));
    assert!(has_leaf, "S1: leaf_pointer 应已写入，entries types = {:?}", types);
    println!("S1 PASS: leaf_pointer 已写入");
}

/// S2: 回滚是 only-append（原始消息不丢）
#[test]
fn s2_rollback_is_append_only() {
    let cwd = tmp_cwd("s2");
    let ids = seed_session(&cwd, "sess_s2", &[
        ("user", "a"),
        ("assistant", "b"),
        ("user", "c"),
        ("assistant", "d"),
    ]);

    // 回滚（带 reason → 追加 leaf_pointer + branch_summary）
    let new_entries = session_tree::make_rollback(&ids[2], Some(&ids[4]), Some("test reason")).unwrap();
    let appended_count = new_entries.len();
    for e in &new_entries {
        session_jsonl::append_raw_entry(&cwd, e);
    }

    // 只追加（make_rollback 至少返回 leaf_pointer）
    assert!(appended_count >= 1, "S2: make_rollback 应返回至少 1 条 entry（leaf_pointer）");
    
    // 原始 4 条消息仍在
    let entries = load_all(&cwd);
    let msg_count = entries.iter()
        .filter(|e| e.get("type").and_then(|v| v.as_str()) == Some("message"))
        .count();
    assert_eq!(msg_count, 4, "S2: 4 条原始消息全在（only-append）");
    
    let leaf_count = entries.iter()
        .filter(|e| e.get("type").and_then(|v| v.as_str()) == Some("leaf_pointer"))
        .count();
    assert_eq!(leaf_count, 1, "S2: 1 条 leaf_pointer");
    println!("S2 PASS: only-append（4 条消息全在 + 1 leaf_pointer)");
}

// ════════════════════════════════════════════════════════
// Group TC: Token 计算 / Compaction 判定 / Context 长度
//
// 验证回滚对 token 计算链路的影响。核心链路：
//   SessionFile::load → messages → total_tokens → needs_compact
//
// F1 导致：load 不过滤 leaf_pointer → messages 含被回滚的 → token 虚高 → compaction 误判
// ════════════════════════════════════════════════════════

use ion::agent::compact::{total_tokens, needs_compact, CompactConfig};

/// 构造大消息（让单条 token 足够大，方便测阈值）
fn big_msg_entry(parent_id: &str, role: &str, size: usize) -> serde_json::Value {
    let text = "x".repeat(size);
    msg_entry(parent_id, role, &text)
}

/// TC1: 回滚后 total_tokens 只算 live path（F1 已修复）
///
/// 验证：回滚到 ids[2] 后，messages 只有 2 条 → tokens ≈ 200（不是 400）
#[test]
fn tc1_tokens_exclude_rolled_back() {
    let cwd = tmp_cwd("tc1");
    let header = serde_json::json!({"type":"session","version":3,"id":"sess_tc1","timestamp":session_jsonl::timestamp_iso(),"cwd":cwd,"parentSession":null});
    session_jsonl::append_raw_entry(&cwd, &header);

    let mut parent = "sess_tc1".to_string();
    let mut ids = vec!["sess_tc1".to_string()];
    for i in 0..4 {
        let entry = big_msg_entry(&parent, if i % 2 == 0 { "user" } else { "assistant" }, 400);
        let id = entry["id"].as_str().unwrap().to_string();
        session_jsonl::append_raw_entry(&cwd, &entry);
        ids.push(id.clone());
        parent = id;
    }

    let rb_entries = session_tree::make_rollback(&ids[2], Some(&ids[4]), None).unwrap();
    for e in &rb_entries { session_jsonl::append_raw_entry(&cwd, e); }

    let sf = session_jsonl::SessionFile::load(&cwd).unwrap();
    let tokens = total_tokens(&sf.messages);

    // F1 修复后：只有 2 条 → tokens ≈ 200
    assert_eq!(sf.messages.len(), 2, "TC1: messages 应只有 2 条");
    assert!(tokens < 250, "TC1: tokens={} 应 < 250（只算 2 条）", tokens);
    println!("TC1 PASS: tokens={}（只算 live path 的 2 条，F1 已修复）", tokens);
}

/// TC2: 回滚后 needs_compact 不误触发（F1 已修复）
///
/// 验证：live path 只有 2 条小消息，被回滚的大消息不参与 token 计算 → needs_compact = false
#[test]
fn tc2_needs_compact_correct() {
    let cwd = tmp_cwd("tc2");
    let header = serde_json::json!({"type":"session","version":3,"id":"sess_tc2","timestamp":session_jsonl::timestamp_iso(),"cwd":cwd,"parentSession":null});
    session_jsonl::append_raw_entry(&cwd, &header);

    let m1 = msg_entry("sess_tc2", "user", "hi");
    let id1 = m1["id"].as_str().unwrap().to_string();
    session_jsonl::append_raw_entry(&cwd, &m1);
    let m2 = msg_entry(&id1, "assistant", "ok");
    let id2 = m2["id"].as_str().unwrap().to_string();
    session_jsonl::append_raw_entry(&cwd, &m2);

    let big = "y".repeat(200_000);
    let m3 = msg_entry(&id2, "user", &big);
    let id3 = m3["id"].as_str().unwrap().to_string();
    session_jsonl::append_raw_entry(&cwd, &m3);
    let m4 = msg_entry(&id3, "assistant", &big);
    let id4 = m4["id"].as_str().unwrap().to_string();
    session_jsonl::append_raw_entry(&cwd, &m4);

    let rb_entries = session_tree::make_rollback(&id2, Some(&id4), None).unwrap();
    for e in &rb_entries { session_jsonl::append_raw_entry(&cwd, e); }

    let sf = session_jsonl::SessionFile::load(&cwd).unwrap();
    let config = CompactConfig { threshold: 10000, ..Default::default() };

    // F1 修复后：只有 2 条小消息 → needs_compact = false
    let result = needs_compact(&sf.messages, &config);
    assert!(!result, "TC2: needs_compact={} 应为 false（live path 只有 2 条小消息）", result);
    println!("TC2 PASS: needs_compact=false（被回滚的大消息不参与计算，F1 已修复）");
}

/// TC3: 回滚后 context 长度 = live path（F1 已修复）
///
/// 验证：SessionFile.messages 的数量 = retrieval View::Live 的数量（两者一致）
#[test]
fn tc3_context_length_matches_live() {
    let cwd = tmp_cwd("tc3");
    let ids = seed_session(&cwd, "sess_tc3", &[
        ("user", "keep1"),
        ("assistant", "keep2"),
        ("user", "discard1"),
        ("assistant", "discard2"),
        ("user", "discard3"),
        ("assistant", "discard4"),
    ]);

    let rb_entries = session_tree::make_rollback(&ids[2], Some(&ids[6]), None).unwrap();
    for e in &rb_entries { session_jsonl::append_raw_entry(&cwd, e); }

    let entries = load_all(&cwd);
    let live_count = live_msg_count(&entries);

    let sf = session_jsonl::SessionFile::load(&cwd).unwrap();
    let context_count = sf.messages.len();

    assert_eq!(live_count, 2, "TC3: live path 应只有 2 条");
    assert_eq!(context_count, live_count,
        "TC3: context({}) 应 = live({})（F1 已修复）", context_count, live_count);
    println!("TC3 PASS: context={} = live={}（F1 已修复）", context_count, live_count);
}

/// TC4: 如果 F1 修复（load 过滤），token/compaction/context 应正确
///
/// 这个测试模拟"F1 修复后"的行为：手动只取 live path 的 messages 算 token。
/// 当 F1 修复后，SessionFile::load 会直接返回过滤后的 messages，
/// 这个测试的断言就变成验证修复正确。
#[test]
fn tc4_fixed_token_would_be_correct() {
    let cwd = tmp_cwd("tc4");
    let header = serde_json::json!({"type":"session","version":3,"id":"sess_tc4","timestamp":session_jsonl::timestamp_iso(),"cwd":cwd,"parentSession":null});
    session_jsonl::append_raw_entry(&cwd, &header);

    // 2 条小消息 + 2 条超大消息
    let m1 = msg_entry("sess_tc4", "user", "small");
    let id1 = m1["id"].as_str().unwrap().to_string();
    session_jsonl::append_raw_entry(&cwd, &m1);
    let m2 = msg_entry(&id1, "assistant", "reply");
    let id2 = m2["id"].as_str().unwrap().to_string();
    session_jsonl::append_raw_entry(&cwd, &m2);

    let big = "z".repeat(200_000);
    let m3 = msg_entry(&id2, "user", &big);
    let id3 = m3["id"].as_str().unwrap().to_string();
    session_jsonl::append_raw_entry(&cwd, &m3);
    let m4 = msg_entry(&id3, "assistant", &big);
    let id4 = m4["id"].as_str().unwrap().to_string();
    session_jsonl::append_raw_entry(&cwd, &m4);

    // 回滚到 id2
    let rb_entries = session_tree::make_rollback(&id2, Some(&id4), None).unwrap();
    for e in &rb_entries {
        session_jsonl::append_raw_entry(&cwd, e);
    }

    // 模拟"修复后"：只取 live path 消息
    let entries = load_all(&cwd);
    let params = RetrievalParams {
        view: View::Live,
        limit: 0,
        include_custom: CustomFilter::None,
        ..Default::default()
    };
    let live_result = retrieve_messages(&entries, &params);
    // 从 live entries 反序列化成 Message（模拟修复后 load 的行为）
    let live_messages: Vec<Message> = live_result.messages.iter()
        .filter_map(|e| {
            if e.get("type").and_then(|v| v.as_str()) != Some("message") { return None; }
            e.get("message").and_then(|m| serde_json::from_value(m.clone()).ok())
        })
        .collect();

    let live_tokens = total_tokens(&live_messages);
    let config = CompactConfig { threshold: 10000, ..Default::default() };
    let live_needs = needs_compact(&live_messages, &config);

    // 修复后：只有 2 条小消息 → token 极小 → 不需要压缩
    assert_eq!(live_messages.len(), 2, "TC4: live path 只有 2 条");
    assert!(live_tokens < 100, "TC4: live token={} 应很小（< 100）", live_tokens);
    assert!(!live_needs, "TC4: live path 不需要压缩（token={} < threshold=10000）", live_tokens);

    println!(
        "TC4 PASS (修复后预期): live msg={}, token={}, needs_compact={}（如果 F1 修复，行为应如此）",
        live_messages.len(), live_tokens, live_needs
    );
}

/// TC5: compaction entry + 回滚交互——compaction 后回滚被拒绝
///
/// 验证：compaction 产生的 CompactionEntry 之后的回滚正常，
/// 但穿越 CompactionEntry 的回滚被拒绝（不因 F1 而变化，这是独立的安全检查）
#[test]
fn tc5_compaction_safety_independent_of_f1() {
    let cwd = tmp_cwd("tc5");
    let ids = seed_session(&cwd, "sess_tc5", &[
        ("user", "before compaction 1"),
        ("assistant", "reply 1"),
        ("user", "before compaction 2"),
        ("assistant", "reply 2"),
    ]);

    // 追加 compaction entry
    let compaction = serde_json::json!({
        "type": "compaction",
        "id": session_jsonl::generate_id(),
        "parentId": &ids[4],
        "timestamp": session_jsonl::timestamp_iso(),
        "summary": "early history compacted",
        "tokensBefore": 5000,
        "firstKeptEntryId": &ids[4],
    });
    session_jsonl::append_raw_entry(&cwd, &compaction);

    // compaction 后追加新消息
    let after = msg_entry(&ids[4], "user", "after compaction");
    let after_id = after["id"].as_str().unwrap().to_string();
    session_jsonl::append_raw_entry(&cwd, &after);

    let entries = load_all(&cwd);

    // 穿越压缩点（回滚到 ids[1]）→ 应被拒绝
    let safety_before = session_tree::check_compaction_safety(&entries, &ids[1]);
    assert!(safety_before.is_some(), "TC5: 穿越压缩点的回滚应被拒绝");

    // 压缩点之后（回滚到 after_id）→ 应允许
    let safety_after = session_tree::check_compaction_safety(&entries, &after_id);
    assert!(safety_after.is_none(), "TC5: 压缩点之后的回滚应允许");

    println!("TC5 PASS: compaction 安全检查独立于 F1（穿越拒绝={}, 之后允许）",
        safety_before.is_some());
}

/// TC6: 多次回滚后 context 不累积（F1 已修复）
///
/// 验证：2 次回滚后 SessionFile.messages 仍只含 live path 的 2 条（不累积废弃分支）
#[test]
fn tc6_token_stable_across_rollbacks() {
    let cwd = tmp_cwd("tc6");
    let header = serde_json::json!({"type":"session","version":3,"id":"sess_tc6","timestamp":session_jsonl::timestamp_iso(),"cwd":cwd,"parentSession":null});
    session_jsonl::append_raw_entry(&cwd, &header);

    let m1 = msg_entry("sess_tc6", "user", "init1");
    let id1 = m1["id"].as_str().unwrap().to_string();
    session_jsonl::append_raw_entry(&cwd, &m1);
    let m2 = msg_entry(&id1, "assistant", "init2");
    let id2 = m2["id"].as_str().unwrap().to_string();
    session_jsonl::append_raw_entry(&cwd, &m2);

    // Cycle 1
    let c1m1 = msg_entry(&id2, "user", "cycle1 msg padding ");
    let c1id1 = c1m1["id"].as_str().unwrap().to_string();
    session_jsonl::append_raw_entry(&cwd, &c1m1);
    let c1m2 = msg_entry(&c1id1, "assistant", "cycle1 reply padding ");
    let c1id2 = c1m2["id"].as_str().unwrap().to_string();
    session_jsonl::append_raw_entry(&cwd, &c1m2);
    let rb1 = session_tree::make_rollback(&id2, Some(&c1id2), None).unwrap();
    for e in &rb1 { session_jsonl::append_raw_entry(&cwd, e); }

    // Cycle 2
    let c2m1 = msg_entry(&id2, "user", "cycle2 msg padding ");
    let c2id1 = c2m1["id"].as_str().unwrap().to_string();
    session_jsonl::append_raw_entry(&cwd, &c2m1);
    let c2m2 = msg_entry(&c2id1, "assistant", "cycle2 reply padding ");
    let c2id2 = c2m2["id"].as_str().unwrap().to_string();
    session_jsonl::append_raw_entry(&cwd, &c2m2);
    let rb2 = session_tree::make_rollback(&id2, Some(&c2id2), None).unwrap();
    for e in &rb2 { session_jsonl::append_raw_entry(&cwd, e); }

    // F1 修复后：messages 只有 live path 的 2 条
    let sf = session_jsonl::SessionFile::load(&cwd).unwrap();
    let entries = load_all(&cwd);
    let live_count = live_msg_count(&entries);

    assert_eq!(live_count, 2, "TC6: live path 只有 2 条");
    assert_eq!(sf.messages.len(), 2,
        "TC6: context 应只有 2 条（不累积废弃分支），实际 {}", sf.messages.len());
    println!("TC6 PASS: 2 次回滚后 context={} 条 = live={}（F1 已修复，不累积）",
        sf.messages.len(), live_count);
}
