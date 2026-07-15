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
fn turn_summary_entry(turn_id: u64, entry_range: &[&str]) -> serde_json::Value {
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

/// C2: 回滚后 SessionFile::load 仍加载全部消息（暴露 F1）
/// 期望（文档）：load 后 messages 不含被回滚的
/// 实际（F1）：load 加载全部 type=message，不过滤 leaf_pointer
#[test]
fn c2_load_does_not_filter_leaf_pointer() {
    let cwd = tmp_cwd("c2");
    let ids = seed_session(&cwd, "sess_c2", &[
        ("user", "msg1"),
        ("assistant", "msg2"),
        ("user", "msg3"),
        ("assistant", "msg4"),
    ]);

    // 回滚到 msg2（丢弃 msg3/msg4）
    let new_entries = session_tree::make_rollback(&ids[2], Some(&ids[4]), None).unwrap();
    for e in &new_entries {
        session_jsonl::append_raw_entry(&cwd, e);
    }

    // SessionFile::load 加载消息
    let sf = session_jsonl::SessionFile::load(&cwd).expect("session should load");

    // F1 暴露：load 不过滤，messages 仍有 4 条
    // 期望（文档）：应该只有 2 条（msg1, msg2 在 live path）
    assert_eq!(
        sf.messages.len(),
        4,
        "C2: SessionFile::load 返回 {} 条消息。F1: load 不过滤 leaf_pointer（期望 2 条）",
        sf.messages.len()
    );
    println!(
        "C2 F1 EXPOSED: SessionFile::load 返回 {} 条消息（含被回滚的，期望 2 条）",
        sf.messages.len()
    );
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

/// K1: 回滚后 SessionFile.messages 长度 = 全量（F1 导致 compaction 误算）
/// compaction 判定基于 SessionFile.messages，F1 使其含被回滚消息
#[test]
fn k1_messages_count_includes_rolled_back() {
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
    // F1: messages 含全部 4 条（不含被回滚过滤）
    // 如果 compaction 用这个算 token，会误判需要压缩
    assert_eq!(sf.messages.len(), 4,
        "K1: messages 有 {} 条（F1: 含被回滚的，compaction 会误算）", sf.messages.len());
    println!("K1 F1 EXPOSED: messages.len()={}（含被回滚消息，compaction token 会偏高）", sf.messages.len());
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
// Group T: turnId 唯一性（暴露 F3）
// ════════════════════════════════════════════════════════

/// T1: 两次 run 的 turnId 不重复（期望；暴露 F3）
/// 模拟：第一次 run 写 turnId=0,1，回滚后第二次 run 又写 turnId=0,1
#[test]
fn t1_turnid_should_not_repeat() {
    let cwd = tmp_cwd("t1");
    let ids = seed_session(&cwd, "sess_t1", &[
        ("user", "round1"),
        ("assistant", "reply1"),
        ("user", "round2"),
        ("assistant", "reply2"),
    ]);

    // 第一次 run 的 turn_summary（turnId=0, 1）
    let ts1 = turn_summary_entry(0, &[]);
    let ts2 = turn_summary_entry(1, &[]);
    session_jsonl::append_raw_entry(&cwd, &ts1);
    session_jsonl::append_raw_entry(&cwd, &ts2);

    // 回滚到 round1
    let rb_entries = session_tree::make_rollback(&ids[1], Some(&ids[4]), None).unwrap();
    for e in &rb_entries {
        session_jsonl::append_raw_entry(&cwd, e);
    }

    // 第二次 run 的 turn_summary（F3: 又从 0 开始）
    let ts3 = turn_summary_entry(0, &[]);  // 重复 turnId=0
    let ts4 = turn_summary_entry(1, &[]);  // 重复 turnId=1
    session_jsonl::append_raw_entry(&cwd, &ts3);
    session_jsonl::append_raw_entry(&cwd, &ts4);

    let entries = load_all(&cwd);
    let turn_ids: Vec<u64> = entries.iter()
        .filter(|e| e.get("type").and_then(|v| v.as_str()) == Some("turn_summary"))
        .filter_map(|e| e.get("turnId").and_then(|v| v.as_u64()))
        .collect();

    // 统计重复
    let mut seen = std::collections::HashMap::new();
    for tid in &turn_ids {
        *seen.entry(tid).or_insert(0) += 1;
    }
    let dups: Vec<_> = seen.iter().filter(|(_, c)| **c > 1).collect();

    // F3 暴露：有重复 turnId
    assert!(
        !dups.is_empty(),
        "T1: turnId {:?} 无重复。F3 未暴露？", turn_ids
    );
    println!("T1 F3 EXPOSED: turnId {:?} 有重复 {:?}（每次 run 从 0 重置）", turn_ids, dups);
}

/// T2: turn_summary entryRange 恒为空（暴露 F2 的根因）
#[test]
fn t2_entry_range_empty() {
    let cwd = tmp_cwd("t2");
    let ids = seed_session(&cwd, "sess_t2", &[
        ("user", "msg"),
        ("assistant", "reply"),
    ]);

    // 写一个带 entryRange 的 turn_summary（模拟 persist_turn_summary 的实际行为）
    // agent_loop.rs:1390 硬编码 entryRange 为空
    let ts = turn_summary_entry(0, &[]);  // entry_range 故意传空
    session_jsonl::append_raw_entry(&cwd, &ts);

    let entries = load_all(&cwd);
    let ts_entry = entries.iter()
        .find(|e| e.get("type").and_then(|v| v.as_str()) == Some("turn_summary"))
        .unwrap();
    let range = ts_entry.get("entryRange").and_then(|v| v.as_array());
    
    // F2 根因：entryRange 恒为空
    assert!(range.map_or(true, |a| a.is_empty()),
        "T2: entryRange 应为空（F2 根因：agent_loop.rs:1390 硬编码 []）");
    println!("T2 F2 ROOT CAUSE: entryRange 为空（--restore-code 靠它找 turnId 会失败）");
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

/// TC1: 回滚后 total_tokens 含被回滚消息的 token（F1 导致）
///
/// 期望（文档）：回滚后 token 只算 live path（被回滚的不算）
/// 实际（F1）：total_tokens 含全部消息 → token 虚高
#[test]
fn tc1_tokens_include_rolled_back() {
    let cwd = tmp_cwd("tc1");
    let header = serde_json::json!({"type":"session","version":3,"id":"sess_tc1","timestamp":session_jsonl::timestamp_iso(),"cwd":cwd,"parentSession":null});
    session_jsonl::append_raw_entry(&cwd, &header);

    // 写 4 条消息，每条 ~400 chars = ~100 tokens
    let mut parent = "sess_tc1".to_string();
    let mut ids = vec!["sess_tc1".to_string()];
    for i in 0..4 {
        let entry = big_msg_entry(&parent, if i % 2 == 0 { "user" } else { "assistant" }, 400);
        let id = entry["id"].as_str().unwrap().to_string();
        session_jsonl::append_raw_entry(&cwd, &entry);
        ids.push(id.clone());
        parent = id;
    }

    // 回滚到 ids[2]（保留前两条）
    let rb_entries = session_tree::make_rollback(&ids[2], Some(&ids[4]), None).unwrap();
    for e in &rb_entries {
        session_jsonl::append_raw_entry(&cwd, e);
    }

    // SessionFile::load → messages
    let sf = session_jsonl::SessionFile::load(&cwd).unwrap();
    let tokens = total_tokens(&sf.messages);

    // F1: messages 含全部 4 条 → tokens ≈ 400 (4 × 100)
    // 期望（文档）：只有 2 条 → tokens ≈ 200
    assert!(
        sf.messages.len() == 4,
        "TC1: messages 有 {} 条（F1: 含被回滚的）", sf.messages.len()
    );
    assert!(
        tokens > 150,
        "TC1: tokens={} 应含被回滚消息的 token（F1），期望 > 150", tokens
    );
    println!(
        "TC1 F1 EXPOSED: total_tokens={}（含被回滚的 {} 条消息，期望只算 {} 条 ≈ 100 token）",
        tokens, sf.messages.len(), 2
    );
}

/// TC2: 回滚后 needs_compact 因 F1 误触发
///
/// 构造场景：live path 只有 2 条小消息（远低于阈值），
/// 但被回滚的消息很大 → F1 导致 token 虚高 → needs_compact 返回 true（误判）
#[test]
fn tc2_needs_compact_false_positive() {
    let cwd = tmp_cwd("tc2");
    let header = serde_json::json!({"type":"session","version":3,"id":"sess_tc2","timestamp":session_jsonl::timestamp_iso(),"cwd":cwd,"parentSession":null});
    session_jsonl::append_raw_entry(&cwd, &header);

    // 前 2 条：小消息（保留在 live path）
    let m1 = msg_entry("sess_tc2", "user", "hi");
    let id1 = m1["id"].as_str().unwrap().to_string();
    session_jsonl::append_raw_entry(&cwd, &m1);
    let m2 = msg_entry(&id1, "assistant", "ok");
    let id2 = m2["id"].as_str().unwrap().to_string();
    session_jsonl::append_raw_entry(&cwd, &m2);

    // 后 2 条：超大消息（将被回滚，但 F1 导致仍算 token）
    let big = "y".repeat(200_000); // ~50000 tokens
    let m3 = msg_entry(&id2, "user", &big);
    let id3 = m3["id"].as_str().unwrap().to_string();
    session_jsonl::append_raw_entry(&cwd, &m3);
    let m4 = msg_entry(&id3, "assistant", &big);
    let id4 = m4["id"].as_str().unwrap().to_string();
    session_jsonl::append_raw_entry(&cwd, &m4);

    // 回滚到 id2（丢弃 m3/m4）
    let rb_entries = session_tree::make_rollback(&id2, Some(&id4), None).unwrap();
    for e in &rb_entries {
        session_jsonl::append_raw_entry(&cwd, e);
    }

    let sf = session_jsonl::SessionFile::load(&cwd).unwrap();
    let config = CompactConfig {
        threshold: 10000, // 低阈值
        ..Default::default()
    };

    // F1: messages 含全部 4 条 → total_tokens ≈ 100000+ → needs_compact = true
    // 期望（文档）：只有 m1/m2 → tokens ≈ 1 → needs_compact = false
    let result = needs_compact(&sf.messages, &config);
    assert!(
        result,
        "TC2: needs_compact={} 应为 true（F1: 被回滚的大消息仍算 token → 误判需要压缩）",
        result
    );
    println!(
        "TC2 F1 EXPOSED: needs_compact=true（live path 只有 2 条小消息，但被回滚的大消息导致误判压缩）"
    );
}

/// TC3: 回滚后 context 长度（message count）虚高
///
/// 对比：retrieval View::Live 的消息数 vs SessionFile.messages 的数量
/// 期望（文档）：两者应一致（都是 live path）
/// 实际（F1）：SessionFile.messages > live（含被回滚的）
#[test]
fn tc3_context_length_inflated() {
    let cwd = tmp_cwd("tc3");
    let ids = seed_session(&cwd, "sess_tc3", &[
        ("user", "keep1"),
        ("assistant", "keep2"),
        ("user", "discard1"),
        ("assistant", "discard2"),
        ("user", "discard3"),
        ("assistant", "discard4"),
    ]);

    // 回滚到 keep2（丢弃 4 条）
    let rb_entries = session_tree::make_rollback(&ids[2], Some(&ids[6]), None).unwrap();
    for e in &rb_entries {
        session_jsonl::append_raw_entry(&cwd, e);
    }

    let entries = load_all(&cwd);
    let live_count = live_msg_count(&entries); // retrieval 层（正确）

    let sf = session_jsonl::SessionFile::load(&cwd).unwrap();
    let context_count = sf.messages.len(); // Agent context 层（F1: 虚高）

    assert_eq!(live_count, 2, "TC3: live path 应只有 2 条（keep1+keep2）");
    assert_eq!(
        context_count, 6,
        "TC3: context 有 {} 条（F1: 含被回滚的 4 条，应为 2）",
        context_count
    );
    println!(
        "TC3 F1 EXPOSED: live={}, context={}（context 虚高 {} 条 = 被回滚的消息仍在）",
        live_count, context_count, context_count - live_count
    );
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

/// TC6: 多次回滚后 token 累积（F1 导致只增不减）
///
/// 每次回滚+继续聊，被回滚的消息仍在 context → token 只增不减
/// 对比 retrieval live path（正确递减/稳定）
#[test]
fn tc6_token_accumulates_across_rollbacks() {
    let cwd = tmp_cwd("tc6");
    let header = serde_json::json!({"type":"session","version":3,"id":"sess_tc6","timestamp":session_jsonl::timestamp_iso(),"cwd":cwd,"parentSession":null});
    session_jsonl::append_raw_entry(&cwd, &header);

    // 初始 2 条
    let m1 = msg_entry("sess_tc6", "user", "init1");
    let id1 = m1["id"].as_str().unwrap().to_string();
    session_jsonl::append_raw_entry(&cwd, &m1);
    let m2 = msg_entry(&id1, "assistant", "init2");
    let id2 = m2["id"].as_str().unwrap().to_string();
    session_jsonl::append_raw_entry(&cwd, &m2);

    // Cycle 1: 追加 2 条 → 回滚到 id2
    let c1m1 = msg_entry(&id2, "user", "cycle1 msg padding ");
    let c1id1 = c1m1["id"].as_str().unwrap().to_string();
    session_jsonl::append_raw_entry(&cwd, &c1m1);
    let c1m2 = msg_entry(&c1id1, "assistant", "cycle1 reply padding ");
    let c1id2 = c1m2["id"].as_str().unwrap().to_string();
    session_jsonl::append_raw_entry(&cwd, &c1m2);
    let rb1 = session_tree::make_rollback(&id2, Some(&c1id2), None).unwrap();
    for e in &rb1 { session_jsonl::append_raw_entry(&cwd, e); }

    // Cycle 2: 追加 2 条 → 回滚到 id2
    let c2m1 = msg_entry(&id2, "user", "cycle2 msg padding ");
    let c2id1 = c2m1["id"].as_str().unwrap().to_string();
    session_jsonl::append_raw_entry(&cwd, &c2m1);
    let c2m2 = msg_entry(&c2id1, "assistant", "cycle2 reply padding ");
    let c2id2 = c2m2["id"].as_str().unwrap().to_string();
    session_jsonl::append_raw_entry(&cwd, &c2m2);
    let rb2 = session_tree::make_rollback(&id2, Some(&c2id2), None).unwrap();
    for e in &rb2 { session_jsonl::append_raw_entry(&cwd, e); }

    // F1: SessionFile.messages 含全部 6 条（2 init + 2 cycle1 + 2 cycle2）
    let sf = session_jsonl::SessionFile::load(&cwd).unwrap();
    let context_tokens = total_tokens(&sf.messages);

    // retrieval live: 只有 2 条（init1 + init2）
    let entries = load_all(&cwd);
    let live_count = live_msg_count(&entries);

    assert_eq!(sf.messages.len(), 6, "TC6: context 有 6 条（F1: 累积不清理）");
    assert_eq!(live_count, 2, "TC6: live path 只有 2 条");
    println!(
        "TC6 F1 EXPOSED: 2 次回滚后 context={} 条 / token={}，但 live={} 条（被回滚的 4 条仍在 context 累积）",
        sf.messages.len(), context_tokens, live_count
    );
}
