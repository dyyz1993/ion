//! SSE stream parsing tests.
//!
//! These tests mirror how `ion-provider/src/provider/openai.rs` parses an SSE
//! byte stream from an OpenAI-compatible `/chat/completions` endpoint:
//!
//!   1. Bytes are buffered into a `String`.
//!   2. The buffer is split on `\n\n` (or `\r\n\r\n`) into discrete SSE events.
//!   3. For each line in an event, lines not starting with `data: ` are skipped.
//!   4. A `data: [DONE]` payload terminates the stream cleanly.
//!   5. Any other `data: ` payload is deserialized into a `Chunk` (choices +
//!      optional usage). Malformed JSON is skipped (no crash).
//!   6. On `finish_reason`, token usage is extracted from the final chunk.
//!
//! Because `read_sse` consumes a live `reqwest::Response`, these tests
//! re-implement the *parsing* portion against raw byte slices, exercising the
//! exact same algorithm so behavior stays in lock-step with production code.

use serde::Deserialize;

// ---------------------------------------------------------------------------
// Minimal mirror of the private structs in openai.rs (Chunk / Delta / Usage).
// Kept field-for-field compatible so the same JSON payloads deserialize.
// ---------------------------------------------------------------------------

#[derive(Deserialize, Debug)]
struct Chunk {
    #[serde(default)]
    choices: Vec<ChunkChoice>,
    #[serde(default)]
    usage: Option<UsageData>,
}

#[derive(Deserialize, Debug)]
struct UsageData {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    #[serde(default)]
    total_tokens: u64,
}

#[derive(Deserialize, Debug, Default)]
struct ChunkChoice {
    #[serde(default)]
    finish_reason: Option<String>,
    #[serde(default)]
    delta: Delta,
}

#[derive(Deserialize, Debug, Default)]
struct Delta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
}

// ---------------------------------------------------------------------------
// Parsed SSE event representation used by assertions.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum ParsedEvent {
    /// A normal data chunk with accumulated text deltas and optional usage.
    Data {
        text_deltas: Vec<String>,
        reasoning_deltas: Vec<String>,
        finish_reason: Option<String>,
        usage: Option<(u64, u64, u64)>,
    },
    /// The `[DONE]` sentinel — stream should terminate cleanly.
    Done,
}

/// Parse raw SSE bytes into a list of `ParsedEvent`s.
///
/// This is a faithful re-implementation of the parsing loop inside
/// `read_sse` in `openai.rs`: split on blank-line boundaries, skip non-`data:`
/// lines, treat `[DONE]` as terminator, and silently skip malformed JSON.
fn parse_sse(raw: &[u8]) -> Vec<ParsedEvent> {
    let mut events: Vec<ParsedEvent> = Vec::new();
    let buffer = String::from_utf8_lossy(raw).to_string();

    let mut rest = buffer.as_str();
    loop {
        // Find the next event boundary: `\n\n` or `\r\n\r\n`.
        let pos = match rest.find("\n\n").or_else(|| rest.find("\r\n\r\n")) {
            Some(p) => p,
            None => break,
        };
        let event_str = &rest[..pos];
        // Advance past the boundary. Both `\n\n` and `\r\n\r\n` are at least
        // 2 bytes; for `\r\n\r\n` we additionally skip the leading `\r\n`.
        let skip = if rest[pos..].starts_with("\r\n\r\n") { 4 } else { 2 };
        rest = &rest[pos + skip..];
        if event_str.trim().is_empty() {
            continue;
        }

        let mut text_deltas: Vec<String> = Vec::new();
        let mut reasoning_deltas: Vec<String> = Vec::new();
        let mut finish_reason: Option<String> = None;
        let mut usage: Option<(u64, u64, u64)> = None;
        let mut saw_done = false;
        let mut saw_any_data = false;

        for line in event_str.lines() {
            let line = line.trim();
            if !line.starts_with("data: ") {
                continue;
            }
            let json_str = &line[6..];
            saw_any_data = true;
            if json_str == "[DONE]" {
                saw_done = true;
                continue;
            }

            // Malformed JSON is silently skipped, exactly like production code.
            let chunk: Chunk = match serde_json::from_str(json_str) {
                Ok(c) => c,
                Err(_) => continue,
            };

            if let Some(u) = &chunk.usage {
                usage = Some((u.prompt_tokens, u.completion_tokens, u.total_tokens));
            }
            for choice in chunk.choices {
                if let Some(c) = choice.delta.content.filter(|c| !c.is_empty()) {
                    text_deltas.push(c);
                }
                if let Some(r) = choice.delta.reasoning_content.filter(|r| !r.is_empty()) {
                    reasoning_deltas.push(r);
                }
                if let Some(reason) = choice.finish_reason {
                    finish_reason = Some(reason);
                }
            }
        }

        if saw_done {
            events.push(ParsedEvent::Done);
        } else if saw_any_data {
            events.push(ParsedEvent::Data {
                text_deltas,
                reasoning_deltas,
                finish_reason,
                usage,
            });
        }
    }

    events
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Test 1: Parse a single SSE event with one text delta and verify fields.
#[test]
fn parse_single_sse_event() {
    let raw = b"data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n";

    let events = parse_sse(raw);

    assert_eq!(events.len(), 1, "exactly one event expected");
    match &events[0] {
        ParsedEvent::Data { text_deltas, finish_reason, usage, .. } => {
            assert_eq!(text_deltas, &vec!["Hello".to_string()]);
            assert!(finish_reason.is_none(), "no finish_reason on a mid-stream chunk");
            assert!(usage.is_none(), "no usage on a mid-stream chunk");
        }
        other => panic!("expected Data event, got {other:?}"),
    }
}

/// Test 2: Parse multiple SSE events in sequence and verify all are parsed.
#[test]
fn parse_multiple_sse_events() {
    let raw = b"\
data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"},\"finish_reason\":null}]}\n\
\n\
data: {\"choices\":[{\"delta\":{\"content\":\"lo\"},\"finish_reason\":null}]}\n\
\n\
data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\
\n";

    let events = parse_sse(raw);

    assert_eq!(events.len(), 3, "all three events should be parsed");
    // First two chunks carry text deltas.
    for ev in &events[0..2] {
        match ev {
            ParsedEvent::Data { text_deltas, finish_reason, .. } => {
                assert!(!text_deltas.is_empty(), "expected a text delta");
                assert!(finish_reason.is_none());
            }
            _ => panic!("expected Data event"),
        }
    }
    // Final chunk carries the stop reason.
    match &events[2] {
        ParsedEvent::Data { finish_reason, .. } => {
            assert_eq!(finish_reason.as_deref(), Some("stop"));
        }
        _ => panic!("expected Data event with finish_reason"),
    }
}

/// Test 3: Parse an SSE stream terminated by `[DONE]` and verify clean end.
#[test]
fn parse_done_terminator() {
    let raw = b"\
data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\
\n\
data: [DONE]\n\
\n";

    let events = parse_sse(raw);

    assert_eq!(events.len(), 2);
    assert!(matches!(events[0], ParsedEvent::Data { .. }));
    assert_eq!(events[1], ParsedEvent::Done, "stream should end cleanly on [DONE]");
}

/// Test 4: Parse malformed SSE (missing `data:` prefix and bad JSON) and
/// verify it is handled gracefully without panicking.
#[test]
fn parse_malformed_sse_no_crash() {
    // Event 1: comment line with no `data:` prefix → skipped.
    // Event 2: `data:` line with invalid JSON → skipped.
    // Event 3: a valid event so we can confirm parsing continued.
    let raw = b"\
: this is a heartbeat comment\n\
\n\
data: {not valid json\n\
\n\
data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\
\n";

    let events = parse_sse(raw);

    // The comment-only event yields nothing (no `data:` line).
    // The invalid-JSON event is emitted as an empty Data event (mirrors
    // production, which simply `continue`s on parse error). Only the third,
    // valid event carries an actual text delta.
    let meaningful: Vec<&ParsedEvent> = events
        .iter()
        .filter(|e| match e {
            ParsedEvent::Done => true,
            ParsedEvent::Data { text_deltas, reasoning_deltas, finish_reason, usage } => {
                !text_deltas.is_empty()
                    || !reasoning_deltas.is_empty()
                    || finish_reason.is_some()
                    || usage.is_some()
            }
        })
        .collect();
    assert_eq!(meaningful.len(), 1, "only the valid event should carry content");
    match meaningful[0] {
        ParsedEvent::Data { text_deltas, .. } => {
            assert_eq!(*text_deltas, vec!["ok".to_string()]);
        }
        _ => panic!("expected the single valid Data event"),
    }
}

/// Test 5: Parse SSE with usage info and verify token counts are extracted.
#[test]
fn parse_sse_with_usage_info() {
    // The final chunk in an OpenAI stream typically carries `usage` together
    // with an empty `choices` array (when `stream_options.include_usage` is on).
    let raw = b"\
data: {\"choices\":[{\"delta\":{\"content\":\"answer\"},\"finish_reason\":null}]}\n\
\n\
data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":42,\"completion_tokens\":7,\"total_tokens\":49}}\n\
\n";

    let events = parse_sse(raw);

    assert_eq!(events.len(), 2);
    match &events[1] {
        ParsedEvent::Data { usage, finish_reason, .. } => {
            assert_eq!(finish_reason.as_deref(), Some("stop"));
            let (prompt, completion, total) = usage
                .expect("usage should be present on the final chunk");
            assert_eq!(prompt, 42, "prompt_tokens mismatch");
            assert_eq!(completion, 7, "completion_tokens mismatch");
            assert_eq!(total, 49, "total_tokens mismatch");
        }
        _ => panic!("expected Data event with usage"),
    }
}

/// Test 6: Parse an empty SSE stream and verify no crash / no events.
#[test]
fn parse_empty_sse_stream() {
    let events = parse_sse(b"");
    assert!(events.is_empty(), "empty input must yield zero events");

    // Whitespace-only input should also be safe and yield nothing.
    let events_ws = parse_sse(b"\n\n\r\n\r\n   ");
    assert!(events_ws.is_empty(), "whitespace-only input must yield zero events");
}
