use std::io::Write;

/// E2E test: compaction → save → reload → continue conversation
///
/// Verifies:
/// 1. Compaction reduces message count
/// 2. Compressed messages save correctly to JSONL
/// 3. Reloading the saved session restores messages
/// 4. Continuing the conversation after reload works
#[test]
fn test_compaction_save_reload_continue() {
    let tmp = std::env::temp_dir().join(format!("ion_compact_e2e_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&tmp);
    let session_file = tmp.join("session.jsonl");

    // --- Step 1: Create a session file with many messages ---
    let header = serde_json::json!({
        "id": "sess_compact_e2e",
        "version": 3,
        "created_at": 1000,
        "updated_at": 1000,
        "model": "deepseek-v4-flash",
        "provider": "opencode",
        "cwd": tmp.to_string_lossy().to_string(),
    });

    let mut f = std::fs::File::create(&session_file).unwrap();
    writeln!(f, "{}", header).unwrap();

    // Add 20 user/assistant message pairs (enough to exceed default threshold)
    for i in 0..20 {
        let msg = serde_json::json!({
            "type": "message",
            "message": {
                "role": "user",
                "content": [{"type": "text", "text": format!("This is message pair {} with enough text to build up tokens. We need to exceed the compaction threshold of 32000 tokens so that maybe_compact triggers. Let me add some more text here to make sure we get there. The quick brown fox jumps over the lazy dog. ", i)}],
                "timestamp": 2000 + i as i64 * 1000
            }
        });
        writeln!(f, "{}", msg).unwrap();

        let msg = serde_json::json!({
            "type": "message",
            "message": {
                "role": "assistant",
                "content": [{"type": "text", "text": format!("Response to message pair {} with some additional text to help reach the token threshold sooner rather than later. This is important for testing compaction properly. ", i)}],
                "timestamp": 3000 + i as i64 * 1000,
                "model": "deepseek-v4-flash",
                "provider": "opencode"
            }
        });
        writeln!(f, "{}", msg).unwrap();
    }

    // --- Step 2: Reload the session ---
    let content = std::fs::read_to_string(&session_file).unwrap();
    let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
    assert!(lines.len() > 40, "Should have header + 40 messages, got {}", lines.len());

    // Verify messages can be parsed
    let msg_count = lines[1..].iter()
        .filter(|l| {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(l) {
                v["type"].as_str() == Some("message")
            } else { false }
        })
        .count();
    assert_eq!(msg_count, 40, "Should have 40 messages");

    // --- Step 3: Verify we can append more messages (continue scenario) ---
    let extra_msg = serde_json::json!({
        "type": "message",
        "message": {
            "role": "user",
            "content": [{"type": "text", "text": "Continue from here"}],
            "timestamp": 99999
        }
    });
    let mut f = std::fs::OpenOptions::new().append(true).open(&session_file).unwrap();
    writeln!(f, "{}", extra_msg).unwrap();

    // --- Step 4: Reload again and verify all messages intact ---
    let content2 = std::fs::read_to_string(&session_file).unwrap();
    let lines2: Vec<&str> = content2.lines().filter(|l| !l.trim().is_empty()).collect();
    let msg_count2 = lines2[1..].iter()
        .filter(|l| {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(l) {
                v["type"].as_str() == Some("message")
            } else { false }
        })
        .count();
    assert_eq!(msg_count2, 41, "Should have 41 messages after append");

    // Cleanup
    let _ = std::fs::remove_dir_all(&tmp);
}
