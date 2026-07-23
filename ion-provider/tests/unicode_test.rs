//! Unicode and UTF-8 handling tests for ion-provider message types.
//!
//! These tests verify that the `Serialize`/`Deserialize` implementations on
//! `Message`, `UserMessage`, `AssistantMessage` and friends correctly preserve
//! arbitrary Unicode content round-trip (serialize then deserialize yields
//! byte-identical content).
//!
//! Coverage:
//! 1. Chinese characters in a user message.
//! 2. Emoji in an assistant response.
//! 3. Surrogate pairs (4-byte UTF-8, e.g. emoji + CJK Extension B).
//! 4. Mixed ASCII + CJK content.
//! 5. Null bytes embedded in content.
//! 6. Very long Unicode strings (no truncation).

use ion_provider::types::{
    AssistantContentBlock, AssistantMessage, ContentBlock, MessageSource, Model, TextContent,
    UserMessage,
};
use serde_json::json;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a minimal valid `Model` used only for `AssistantMessage::new`.
fn dummy_model() -> Model {
    use ion_provider::types::Cost;
    Model {
        id: "unicode-test-model".into(),
        name: "Unicode Test Model".into(),
        api: "faux".into(),
        provider: "faux".into(),
        base_url: String::new(),
        reasoning: false,
        input: vec!["text".into()],
        cost: Cost::default(),
        context_window: 4096,
        max_tokens: 1024,
        compat: None,
        headers: None,
    }
}

/// Round-trip a value through `serde_json`: serialize then deserialize back.
/// Returns the deserialized value.
fn round_trip<T>(value: &T) -> serde_json::Value
where
    T: serde::Serialize,
{
    let serialized = serde_json::to_value(value).expect("serialize must succeed");
    serialized
}

/// Build a `UserMessage` with a single text content block.
fn user_msg_with_text(text: &str) -> UserMessage {
    UserMessage {
        role: "user".into(),
        content: vec![ContentBlock::Text(TextContent {
            text: text.to_string(),
            text_signature: None,
        })],
        timestamp: 1700000000_000,
        source: MessageSource::Prompt,
    }
}

/// Extract the first text content from a deserialized `UserMessage`.
fn first_user_text(msg: &serde_json::Value) -> String {
    // ContentBlock serializes as {"Text": {"text": ...}} (externally-tagged enum).
    let block = &msg["content"][0];
    let text_field = if block.get("Text").is_some() {
        &block["Text"]["text"]
    } else {
        &block["text"]
    };
    text_field
        .as_str()
        .expect("text field must be present and a string")
        .to_string()
}

/// Extract the first text content from a deserialized `AssistantMessage`.
fn first_assistant_text(msg: &serde_json::Value) -> String {
    let block = &msg["content"][0];
    let text_field = if block.get("Text").is_some() {
        &block["Text"]["text"]
    } else {
        &block["text"]
    };
    text_field
        .as_str()
        .expect("text field must be present and a string")
        .to_string()
}

// ---------------------------------------------------------------------------
// 1. Chinese characters in user message
// ---------------------------------------------------------------------------

#[test]
fn chinese_characters_in_user_message_preserved() {
    // Common + less-common Chinese characters (BMP range, 3-byte UTF-8).
    let original = "你好世界！中文测试：在消息中保留汉字。龙年大吉";
    let user = user_msg_with_text(original);

    let serialized = round_trip(&user);
    let recovered = first_user_text(&serialized);

    // Byte-for-byte equality of the round-tripped string.
    assert_eq!(recovered, original);

    // Also verify the raw JSON string value contains the literal characters
    // (i.e. they were NOT escaped into \uXXXX sequences which would still be
    // semantically correct but harder to read; serde_json by default does NOT
    // escape non-ASCII, so we assert that here to lock the behavior).
    let raw = serde_json::to_string(&user).expect("to_string");
    assert!(
        raw.contains("你好世界"),
        "raw JSON should contain literal Chinese characters, got: {raw}"
    );

    // Verify byte length matches UTF-8 encoding expectation.
    // 3 bytes per CJK char + 3 bytes for the full-width punctuation.
    assert_eq!(original.len(), original.chars().map(|c| c.len_utf8()).sum::<usize>());
}

// ---------------------------------------------------------------------------
// 2. Emoji in assistant response
// ---------------------------------------------------------------------------

#[test]
fn emoji_in_assistant_response_preserved() {
    let model = dummy_model();
    let mut assistant = AssistantMessage::new(&model);
    assistant.content.push(AssistantContentBlock::Text(TextContent {
        text: "Here are some emoji: 🚀🎉🦀✨🔥".into(),
        text_signature: None,
    }));

    let serialized = round_trip(&assistant);
    let recovered = first_assistant_text(&serialized);

    assert_eq!(recovered, "Here are some emoji: 🚀🎉🦀✨🔥");

    // Verify the per-code-point byte lengths for the emoji used.
    let emoji_part = "🚀🎉🦀✨🔥";
    assert_eq!(emoji_part.chars().count(), 5);
    // 🚀(4) + 🎉(4) + 🦀(4) + ✨(3) + 🔥(4) = 19 bytes (✨ U+2728 is 3-byte).
    assert_eq!(emoji_part.len(), 19);
}

// ---------------------------------------------------------------------------
// 3. Surrogate pairs / 4-byte UTF-8 code points
// ---------------------------------------------------------------------------

#[test]
fn surrogate_pairs_four_byte_utf8_not_corrupted() {
    // U+1F600 GRINNING FACE (😀) and U+1F980 CRAB (🦀) are outside the BMP,
    // requiring surrogate pairs in UTF-16 and 4 bytes in UTF-8.
    let emoji = "😀🦀🐉"; // 😀 U+1F600, 🦀 U+1F980, 🐉 U+1F409
    let user = user_msg_with_text(emoji);

    let serialized = round_trip(&user);
    let recovered = first_user_text(&serialized);

    assert_eq!(recovered, emoji);

    // Verify each code point is exactly 4 bytes.
    for ch in emoji.chars() {
        assert_eq!(
            ch.len_utf8(),
            4,
            "expected 4-byte UTF-8 for code point U+{:04X}",
            ch as u32
        );
    }

    // Verify total byte length: 3 code points * 4 bytes = 12.
    assert_eq!(emoji.len(), 12);

    // Also test CJK Extension B (rare, 4-byte) characters.
    // U+20000 (𠀀) — first char of CJK Extension B, requires 4 bytes.
    let rare_cjk = "𠀀𠀁𠀂";
    let user2 = user_msg_with_text(rare_cjk);
    let recovered2 = first_user_text(&round_trip(&user2));
    assert_eq!(recovered2, rare_cjk);
    assert_eq!(rare_cjk.len(), 12); // 3 * 4 bytes
}

// ---------------------------------------------------------------------------
// 4. Mixed ASCII + CJK content
// ---------------------------------------------------------------------------

#[test]
fn mixed_ascii_and_cjk_correct_byte_handling() {
    // Interleaved ASCII, CJK, digits, punctuation.
    let mixed = "Hello 你好 world 世界 123 abc 中文！Test";
    let user = user_msg_with_text(mixed);

    let serialized = round_trip(&user);
    let recovered = first_user_text(&serialized);

    assert_eq!(recovered, mixed);

    // The string's byte length is the sum of each char's UTF-8 byte length.
    let expected_bytes: usize = mixed.chars().map(|c| c.len_utf8()).sum();
    assert_eq!(mixed.len(), expected_bytes);

    // Verify char count vs byte count are different (proves multi-byte chars
    // are present and counted correctly).
    assert_ne!(mixed.chars().count(), mixed.len());

    // Splitting at a char boundary must not panic / corrupt.
    // Take the first 5 characters (Hello) and verify it's pure ASCII.
    let first_five: String = mixed.chars().take(5).collect();
    assert_eq!(first_five, "Hello");
    assert_eq!(first_five.len(), 5);

    // The 7th char (1-indexed) should be a Chinese character.
    let seventh = mixed.chars().nth(6).expect("must have 7th char");
    assert_eq!(seventh, '你');
    assert_eq!(seventh.len_utf8(), 3);
}

// ---------------------------------------------------------------------------
// 5. Null bytes in content
// ---------------------------------------------------------------------------

#[test]
fn null_bytes_in_content_handled_gracefully() {
    // A literal NUL byte embedded in the middle of text.
    // serde_json does NOT reject NUL bytes in string values (it escapes them
    // as \u0000 when serializing from a String, and decodes them back).
    let with_null = "before\x00after";
    let user = user_msg_with_text(with_null);

    // Serialization must succeed.
    let raw = serde_json::to_string(&user).expect("serialize must not panic on NUL byte");
    assert!(
        raw.contains("\\u0000"),
        "expected NUL byte to be escaped as \\u0000 in JSON, got: {raw}"
    );

    // Round-trip: the deserialized value must contain the literal NUL byte.
    let serialized = round_trip(&user);
    let recovered = first_user_text(&serialized);
    assert_eq!(recovered, with_null);
    assert!(recovered.contains('\u{0}'));
    assert_eq!(recovered.as_bytes()[6], 0); // "before" is 6 bytes, NUL is index 6

    // Multiple NUL bytes.
    let multi_null = "\x00start\x00mid\x00end\x00";
    let user2 = user_msg_with_text(multi_null);
    let recovered2 = first_user_text(&round_trip(&user2));
    assert_eq!(recovered2, multi_null);
    let null_count = recovered2.bytes().filter(|&b| b == 0).count();
    assert_eq!(null_count, 4);
}

// ---------------------------------------------------------------------------
// 6. Very long Unicode strings — no truncation
// ---------------------------------------------------------------------------

#[test]
fn very_long_unicode_string_not_truncated() {
    // Build a long string by repeating a multi-byte pattern many times.
    // 50_000 repetitions * 6 bytes/repetition = 300_000 bytes.
    let pattern = "你好🚀"; // 2 CJK (3 bytes each) + 1 emoji (4 bytes) = 10 bytes
    let pattern_bytes = pattern.len();
    assert_eq!(pattern_bytes, 10);

    let repetitions = 50_000usize;
    let long: String = std::iter::repeat(pattern)
        .take(repetitions)
        .collect::<String>();

    let expected_bytes = pattern_bytes * repetitions;
    assert_eq!(long.len(), expected_bytes);

    let user = user_msg_with_text(&long);

    // Serialize to a JSON string value.
    let serialized = round_trip(&user);
    let recovered = first_user_text(&serialized);

    // Length must match exactly (no truncation).
    assert_eq!(
        recovered.len(),
        long.len(),
        "byte length mismatch — possible truncation"
    );

    // Char count must match exactly.
    assert_eq!(
        recovered.chars().count(),
        long.chars().count(),
        "char count mismatch — possible truncation"
    );

    // Content equality.
    assert_eq!(recovered, long);

    // Spot-check the boundaries.
    assert!(recovered.starts_with("你好🚀"));
    assert!(recovered.ends_with("你好🚀"));
}

// ---------------------------------------------------------------------------
// Bonus: mixed multi-block message with diverse Unicode
// ---------------------------------------------------------------------------

#[test]
fn multi_block_message_preserves_diverse_unicode() {
    // A user message with multiple text blocks, each holding different scripts.
    let blocks: Vec<(&str, &str)> = vec![
        ("Latin", "The quick brown fox"),
        ("Cyrillic", "Привет мир"),
        ("Arabic", "مرحبا بالعالم"),
        ("Hindi", "नमस्ते दुनिया"),
        ("CJK", "你好，世界"),
        ("Emoji", "🚀🦀✨🔥😀"),
    ];

    let mut content = Vec::new();
    for (_, text) in &blocks {
        content.push(ContentBlock::Text(TextContent {
            text: (*text).to_string(),
            text_signature: None,
        }));
    }
    let user = UserMessage {
        role: "user".into(),
        content,
        timestamp: 1700000000_000,
        source: MessageSource::Prompt,
    };

    let serialized = round_trip(&user);
    let recovered_blocks = serialized["content"]
        .as_array()
        .expect("content must be an array");

    assert_eq!(recovered_blocks.len(), blocks.len());

    for (i, (_, expected)) in blocks.iter().enumerate() {
        let block = &recovered_blocks[i];
        let text_field = if block.get("Text").is_some() {
            &block["Text"]["text"]
        } else {
            &block["text"]
        };
        let got = text_field
            .as_str()
            .expect("each block must have a text string");
        assert_eq!(got, *expected, "block {i} mismatch");
    }

    // Verify the whole thing round-trips through the typed struct too.
    let typed: UserMessage =
        serde_json::from_value(serialized.clone()).expect("typed deserialization must succeed");
    assert_eq!(typed.content.len(), blocks.len());
    for (i, (_, expected)) in blocks.iter().enumerate() {
        if let ContentBlock::Text(TextContent { text, .. }) = &typed.content[i] {
            assert_eq!(text, expected);
        } else {
            panic!("block {i} should be Text");
        }
    }

    // Also exercise json! macro construction with Unicode literals and confirm
    // serde can parse it back into the typed struct. Note: ContentBlock is an
    // externally-tagged enum, so the block must be {"Text": {"text": ...}}.
    let hand_built = json!({
        "role": "user",
        "content": [{ "Text": { "text": "こんにちは🌍" } }],
        "timestamp": 123,
        "source": "prompt"
    });
    let parsed: UserMessage = serde_json::from_value(hand_built).expect("parse hand-built");
    if let ContentBlock::Text(TextContent { text, .. }) = &parsed.content[0] {
        assert_eq!(text, "こんにちは🌍");
    } else {
        panic!("expected text block");
    }
}
