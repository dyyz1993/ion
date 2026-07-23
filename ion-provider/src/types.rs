use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ===========================================================================
// Core domain types following @dyyz1993/pi-ai reference
// ===========================================================================

// ---------------------------------------------------------------------------
// Identifiers
// ---------------------------------------------------------------------------

pub type Api = String;
pub type Provider = String;

// ---------------------------------------------------------------------------
// Content blocks
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TextContent {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_signature: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ThinkingContent {
    pub thinking: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_signature: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redacted: Option<bool>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImageContent {
    pub data: String,       // base64
    pub mime_type: String,  // "image/jpeg" | "image/png"
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ContentBlock {
    Text(TextContent),
    Image(ImageContent),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum AssistantContentBlock {
    Text(TextContent),
    Thinking(ThinkingContent),
    ToolCall(ToolCall),
}

// ---------------------------------------------------------------------------
// Tool
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value, // JSON Schema
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolCall {
    #[serde(rename = "type")]
    pub call_type: String,
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thought_signature: Option<String>,
}

// ---------------------------------------------------------------------------
// Message
// ---------------------------------------------------------------------------

/// 消息来源标记——区分正常 prompt / steer 插队 / followUp 追加 / interrupt 打断。
/// UI 用它渲染差异化样式（打断红色 / 插队黄色 / 追加蓝色）。
/// 详见 docs/design/MESSAGE_SOURCE_TAG.md
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum MessageSource {
    /// 正常发起新对话（prompt RPC 空闲时，无 behavior）
    Prompt,
    /// steer 插队：等当前 turn 结束，下个 turn 开始时注入（drain_steering）
    Steer,
    /// followUp 追加：等 agent_end 后消费（outer_loop 循环）
    FollowUp,
    /// interrupt 强行打断：abort 当前 agent + 立即新 run
    Interrupt,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UserMessage {
    pub role: String, // "user"
    pub content: Vec<ContentBlock>,
    pub timestamp: i64,
    pub source: MessageSource,
}

impl Default for UserMessage {
    fn default() -> Self {
        UserMessage {
            role: "user".into(),
            content: vec![],
            timestamp: 0,
            source: MessageSource::Prompt,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AssistantMessage {
    pub role: String, // "assistant"
    pub content: Vec<AssistantContentBlock>,
    pub api: String,
    pub provider: String,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
    pub usage: Usage,
    pub stop_reason: StopReason,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    pub timestamp: i64,
}

impl AssistantMessage {
    pub fn new(model: &Model) -> Self {
        Self {
            role: "assistant".into(),
            content: Vec::new(),
            api: model.api.clone(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            response_model: None,
            response_id: None,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: now_ms(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolResultMessage {
    pub role: String, // "toolResult"
    pub tool_call_id: String,
    pub tool_name: String,
    pub content: Vec<ContentBlock>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
    pub is_error: bool,
    pub timestamp: i64,
}

// ---------------------------------------------------------------------------
// Custom message roles (对齐 pi AgentMessage)
// ---------------------------------------------------------------------------

/// CustomMessage.content 可以是字符串或 ContentBlock 数组
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CustomContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

/// Bash 执行结果（用户 `!cmd` 直发，或 bash 工具结果）
/// 对齐 pi BashExecutionMessage
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BashExecutionMessage {
    pub role: String, // "bashExecution"
    pub command: String,
    pub output: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub cancelled: bool,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full_output_path: Option<String>,
    pub timestamp: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exclude_from_context: Option<bool>,
}

/// 扩展自定义消息（content + display + details）
/// 对齐 pi CustomMessage<T>
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CustomMessage {
    pub role: String, // "custom"
    #[serde(rename = "customType")]
    pub custom_type: String,
    pub content: CustomContent,
    pub display: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
    pub timestamp: i64,
}

/// 分支摘要（回到主线时插入）
/// 对齐 pi BranchSummaryMessage
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BranchSummaryMessage {
    pub role: String, // "branchSummary"
    pub summary: String,
    #[serde(rename = "fromId")]
    pub from_id: String,
    pub timestamp: i64,
}

/// 压缩摘要（compaction 后插入）
/// 对齐 pi CompactionSummaryMessage
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CompactionSummaryMessage {
    pub role: String, // "compactionSummary"
    pub summary: String,
    #[serde(rename = "tokensBefore")]
    pub tokens_before: u64,
    pub timestamp: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Message {
    User(UserMessage),
    Assistant(AssistantMessage),
    ToolResult(ToolResultMessage),
    BashExecution(BashExecutionMessage),
    Custom(CustomMessage),
    BranchSummary(BranchSummaryMessage),
    CompactionSummary(CompactionSummaryMessage),
}

// ---------------------------------------------------------------------------
// Context
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
pub struct Context {
    pub system_prompt: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Option<Vec<ToolDef>>,
}

impl Context {
    pub fn new(system_prompt: Option<String>, messages: Vec<Message>) -> Self {
        Self {
            system_prompt,
            messages,
            tools: None,
        }
    }

    pub fn with_tools(mut self, tools: Vec<ToolDef>) -> Self {
        self.tools = Some(tools);
        self
    }
}

// ---------------------------------------------------------------------------
// Model — the central routing object
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Model {
    pub id: String,
    pub name: String,
    pub api: String,         // routes to ApiRegistry key
    pub provider: String,    // used for env var lookup
    pub base_url: String,    // API endpoint
    pub reasoning: bool,
    pub input: Vec<String>,  // ["text"] | ["text", "image"]
    pub cost: Cost,
    pub context_window: u64,
    pub max_tokens: u64,
    pub compat: Option<CompatConfig>,
    pub headers: Option<HashMap<String, String>>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Cost {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

// ---------------------------------------------------------------------------
// CompatConfig
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum CompatConfig {
    #[serde(rename = "openai-completions")]
    OpenAICompletions(OpenAICompletionsCompat),
    #[serde(rename = "openai-responses")]
    OpenAIResponses(OpenAIResponsesCompat),
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct OpenAICompletionsCompat {
    pub max_tokens_field: Option<String>,
    pub requires_tool_result_name: Option<bool>,
    pub requires_assistant_after_tool_result: Option<bool>,
    pub requires_thinking_as_text: Option<bool>,
    pub requires_reasoning_content_on_assistant_messages: Option<bool>,
    pub thinking_format: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct OpenAIResponsesCompat {
    pub supports_developer_role: Option<bool>,
}

// ---------------------------------------------------------------------------
// Usage
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub total_tokens: u64,
}

// ---------------------------------------------------------------------------
// Thinking
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThinkingLevel {
    Off,
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
}

// ---------------------------------------------------------------------------
// StopReason
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StopReason {
    Stop,
    Length,
    ToolUse,
    Error,
    Aborted,
}

impl std::fmt::Display for StopReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

// ---------------------------------------------------------------------------
// StreamEvent — the protocol between Provider and Consumer
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub enum StreamEvent {
    Start {
        partial: AssistantMessage,
    },
    TextStart {
        content_index: usize,
        partial: AssistantMessage,
    },
    TextDelta {
        content_index: usize,
        delta: String,
        partial: AssistantMessage,
    },
    TextEnd {
        content_index: usize,
        content: String,
        partial: AssistantMessage,
    },
    ThinkingStart {
        content_index: usize,
        partial: AssistantMessage,
    },
    ThinkingDelta {
        content_index: usize,
        delta: String,
        partial: AssistantMessage,
    },
    ThinkingEnd {
        content_index: usize,
        content: String,
        partial: AssistantMessage,
    },
    ToolCallStart {
        content_index: usize,
        partial: AssistantMessage,
    },
    ToolCallDelta {
        content_index: usize,
        delta: String,
        partial: AssistantMessage,
    },
    ToolCallEnd {
        content_index: usize,
        tool_call: ToolCall,
        partial: AssistantMessage,
    },
    Done {
        reason: StopReason,
        message: AssistantMessage,
    },
    Error {
        reason: StopReason,
        message: AssistantMessage,
    },
}

// ---------------------------------------------------------------------------
// StreamOptions
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct StreamOptions {
    pub max_tokens: Option<u64>,
    pub api_key: Option<String>,
    pub reasoning: Option<ThinkingLevel>,
    pub timeout_ms: Option<u64>,
    pub max_retries: Option<u32>,
    /// Force JSON output: Some("object") or Some("schema")
    pub response_format: Option<String>,
}

// ---------------------------------------------------------------------------
// Helper
// ---------------------------------------------------------------------------

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

// ---------------------------------------------------------------------------
// ToolResult — used in hook system (after_tool_call)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct ToolResult {
    pub tool_call_id: String,
    pub output: String,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_bashexecution_serde() {
        let msg = BashExecutionMessage {
            role: "bashExecution".into(),
            command: "ls -la".into(),
            output: "total 42\n-rw-r--r--  1 user  staff  1234 Jul  5  2026 Cargo.toml".into(),
            exit_code: Some(0),
            cancelled: false,
            truncated: false,
            full_output_path: None,
            timestamp: 1712345678000,
            exclude_from_context: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""role":"bashExecution""#));
        assert!(json.contains(r#""command":"ls -la""#));
        assert!(json.contains(r#""exit_code":0"#));

        // Round-trip
        let des: BashExecutionMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(des.role, "bashExecution");
        assert_eq!(des.command, "ls -la");
        assert_eq!(des.exit_code, Some(0));
    }

    #[test]
    fn test_message_custom_serde() {
        let msg = CustomMessage {
            role: "custom".into(),
            custom_type: "note".into(),
            content: CustomContent::Text("记住这个：用户喜欢 Rust".into()),
            display: true,
            details: Some(serde_json::json!({"source": "memory"})),
            timestamp: 1712345678000,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""role":"custom""#));
        assert!(json.contains(r#""customType":"note""#));

        let des: CustomMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(des.custom_type, "note");
        assert!(des.display);
        match des.content {
            CustomContent::Text(s) => assert!(s.contains("Rust")),
            _ => panic!("expected Text"),
        }
    }

    #[test]
    fn test_message_branchsummary_serde() {
        let msg = BranchSummaryMessage {
            role: "branchSummary".into(),
            summary: "Reimplemented the parser in nom".into(),
            from_id: "sess_abc123".into(),
            timestamp: 1712345678000,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""role":"branchSummary""#));
        assert!(json.contains(r#""fromId":"sess_abc123""#));

        let des: BranchSummaryMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(des.from_id, "sess_abc123");
    }

    #[test]
    fn test_message_compactionsummary_serde() {
        let msg = CompactionSummaryMessage {
            role: "compactionSummary".into(),
            summary: "Discussed architecture, decided on trait-based approach".into(),
            tokens_before: 42000,
            timestamp: 1712345678000,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""role":"compactionSummary""#));
        assert!(json.contains(r#""tokensBefore":42000"#));

        let des: CompactionSummaryMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(des.tokens_before, 42000);
    }

    #[test]
    fn test_message_enum_bashexecution_roundtrip() {
        let msg = Message::BashExecution(BashExecutionMessage {
            role: "bashExecution".into(),
            command: "cargo test".into(),
            output: "test result: ok. 90 passed".into(),
            exit_code: Some(0),
            cancelled: false,
            truncated: false,
            full_output_path: None,
            timestamp: 1712345678000,
            exclude_from_context: None,
        });
        let json = serde_json::to_string(&msg).unwrap();
        // externally-tagged: {"BashExecution":{...}}
        assert!(json.starts_with(r#"{"BashExecution":"#));
        assert!(json.contains(r#""command":"cargo test""#));

        let des: Message = serde_json::from_str(&json).unwrap();
        match des {
            Message::BashExecution(b) => assert_eq!(b.command, "cargo test"),
            _ => panic!("expected BashExecution"),
        }
    }

    #[test]
    fn test_message_enum_custom_roundtrip() {
        let msg = Message::Custom(CustomMessage {
            role: "custom".into(),
            custom_type: "alert".into(),
            content: CustomContent::Text("test".into()),
            display: false,
            details: None,
            timestamp: 1712345678000,
        });
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.starts_with(r#"{"Custom":"#));

        let des: Message = serde_json::from_str(&json).unwrap();
        match des {
            Message::Custom(c) => assert_eq!(c.custom_type, "alert"),
            _ => panic!("expected Custom"),
        }
    }

    #[test]
    fn test_message_enum_branchsummary_roundtrip() {
        let msg = Message::BranchSummary(BranchSummaryMessage {
            role: "branchSummary".into(),
            summary: "branch summary text".into(),
            from_id: "sess_x".into(),
            timestamp: 1712345678000,
        });
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.starts_with(r#"{"BranchSummary":"#));

        let des: Message = serde_json::from_str(&json).unwrap();
        match des {
            Message::BranchSummary(b) => assert_eq!(b.from_id, "sess_x"),
            _ => panic!("expected BranchSummary"),
        }
    }

    #[test]
    fn test_message_enum_compactionsummary_roundtrip() {
        let msg = Message::CompactionSummary(CompactionSummaryMessage {
            role: "compactionSummary".into(),
            summary: "compacted text".into(),
            tokens_before: 50000,
            timestamp: 1712345678000,
        });
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.starts_with(r#"{"CompactionSummary":"#));

        let des: Message = serde_json::from_str(&json).unwrap();
        match des {
            Message::CompactionSummary(c) => assert_eq!(c.tokens_before, 50000),
            _ => panic!("expected CompactionSummary"),
        }
    }

    #[test]
    fn test_customcontent_text() {
        let cc = CustomContent::Text("hello".into());
        let json = serde_json::to_string(&cc).unwrap();
        assert_eq!(json, r#""hello""#);

        let des: CustomContent = serde_json::from_str(&json).unwrap();
        match des {
            CustomContent::Text(s) => assert_eq!(s, "hello"),
            _ => panic!("expected Text"),
        }
    }

    #[test]
    fn test_customcontent_blocks() {
        let cc = CustomContent::Blocks(vec![
            ContentBlock::Text(TextContent { text: "hello".into(), text_signature: None }),
        ]);
        let json = serde_json::to_string(&cc).unwrap();
        assert!(json.contains(r#"{"text":"hello"}"#));

        let des: CustomContent = serde_json::from_str(&json).unwrap();
        match des {
            CustomContent::Blocks(blocks) => assert_eq!(blocks.len(), 1),
            _ => panic!("expected Blocks"),
        }
    }
}
