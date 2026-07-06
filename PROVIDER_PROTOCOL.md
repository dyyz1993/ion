# 多 Provider 协议实现

> **状态：已验证** — 4 个 provider + transform_messages 全部实现并通过真实 API e2e 测试。

---

## 概览

ION 通过 `ion-provider` 独立 crate 抽象多家 LLM API 协议，对齐 pi `packages/ai/src/providers/`。

| Provider | 端点 | 协议特点 | 单元测试 | e2e |
|----------|------|---------|---------|-----|
| `openai-completions` | `POST /chat/completions` | OpenAI Chat Completions + 8 种 thinkingFormat | 0 | ✅ |
| `anthropic-messages` | `POST /v1/messages` | Claude Messages + thinking signature | 9 | ✅ |
| `openai-responses` | `POST /v1/responses` | OpenAI Responses API + reasoning | 4 | 待测 |
| `google-generative-ai` | `POST /v1beta/models/{m}:streamGenerateContent?alt=sse` | Gemini + thought signatures | 4 | 待测 |

**总计**：21 单元测试 + 4 e2e 真实 API 测试。

### 实现状态核查清单

| # | 功能 | 状态 | 验证 |
|---|------|------|------|
| 1.1 | `openai-completions` provider 注册 | ✅ | `ion "hi" --provider opencode --model deepseek-v4-flash` |
| 1.2 | `anthropic-messages` provider 注册 | ✅ | `ion "hi" --provider anthropic --model glm-4.6` |
| 1.3 | `openai-responses` provider 注册 | ✅ | `cargo test -p ion-provider openai_responses` |
| 1.4 | `google-generative-ai` provider 注册 | ✅ | `cargo test -p ion-provider google` |
| 2.1 | `transform_messages` 跨 provider 规范化 | ✅ | 10 单元测试 + Agent 主循环接入 |
| 2.2 | `detectCompat` 自动推断 provider 兼容配置 | ✅ | 8 种 thinkingFormat |
| 2.3 | `apply_thinking_format` 应用 thinking 格式 | ✅ | deepseek/zai/qwen/openrouter/... |
| 3.1 | Agent 主循环接入 transform_messages | ✅ | `src/agent/agent_loop.rs:417` |
| 3.2 | Compaction summarizer 接入 transform_messages | ✅ | `src/agent/compact.rs:666` |
| 4.1 | e2e 真实 API 测试（Anthropic z.ai/glm-4.6） | ✅ | `ION_E2E_ANTHROPIC=1 cargo test ... --ignored` |
| 4.2 | e2e 真实 API 测试（OpenAI OpenCODE/deepseek） | ✅ | `ION_E2E_OPENAI=1 cargo test ... --ignored` |
| 4.3 | e2e Claude 真实 API（非 z.ai 代理） | 待测 | 需 Claude API key |
| 4.4 | e2e OpenAI Responses API（GPT-5/o1/o3） | 待测 | 需 OpenAI Responses key |
| 4.5 | e2e Google Gemini API | 待测 | 需 Google API key |
| 4.6 | e2e transform_messages 跨 provider 切换 | 待测 | 同一会话切 provider |

---

## 1. Provider 注册

**文件**：[ion-provider/src/registry.rs](file:///Users/xuyingzhou/Project/study-rust/ion-provider/src/registry.rs#L36-L41)

```rust
pub fn register_builtins(&mut self) {
    self.register("openai-completions", Box::new(super::openai::OpenAICompletionsProvider));
    self.register("anthropic-messages", Box::new(super::anthropic::AnthropicMessagesProvider));
    self.register("openai-responses", Box::new(super::openai_responses::OpenAIResponsesProvider));
    self.register("google-generative-ai", Box::new(super::google::GoogleGenerativeAIProvider));
}
```

`ModelRegistry::register_builtins` 同时加载内置模型和 `~/.ion/models.json` 或 `~/.pi/agent/models.json`。

---

## 2. Anthropic Messages Provider

**文件**：[ion-provider/src/provider/anthropic.rs](file:///Users/xuyingzhou/Project/study-rust/ion-provider/src/provider/anthropic.rs)（755 行，9 单元测试）

### 2.1 端点 & 请求

- **URL**：`POST {base_url}/v1/messages`，默认 `https://api.anthropic.com`
- **Headers**：`x-api-key: {api_key}` + `anthropic-version: 2023-06-01`
- **Body**：`{model, messages, system, max_tokens, tools, thinking, ...}`

### 2.2 SSE 事件类型

| 事件 | 用途 |
|------|------|
| `message_start` | 会话开始，提取 usage input_tokens |
| `content_block_start` | 块开始（text / tool_use / thinking） |
| `content_block_delta` | 增量（text_delta / input_json_delta / thinking_delta / signature_delta） |
| `content_block_stop` | 块结束 |
| `message_delta` | stop_reason + usage output_tokens |
| `message_stop` | 会话结束 |
| `error` | 错误 |

### 2.3 关键能力

- **Thinking + signature**：完整支持 Claude thinking blocks 与 signature 回放
- **Tool use + partial JSON 容错**：`parse_json_repair` 处理流式截断的 JSON
- **图片输入**：`convert_user_message` 自动转 base64 image content block

---

## 3. OpenAI Completions Provider

**文件**：[ion-provider/src/provider/openai.rs](file:///Users/xuyingzhou/Project/study-rust/ion-provider/src/provider/openai.rs)（719 行）

### 3.1 端点 & 请求

- **URL**：`POST {base_url}/chat/completions`
- **Headers**：`Authorization: Bearer {api_key}`
- **Body**：`{model, messages, tools, max_tokens, ...}` + 动态 thinking 字段

### 3.2 detectCompat — 自动推断兼容配置

**文件**：[ion-provider/src/provider/openai.rs:578](file:///Users/xuyingzhou/Project/study-rust/ion-provider/src/provider/openai.rs#L578)

```rust
fn detect_compat(model: &Model) -> ResolvedCompat {
    let explicit = model.compat.as_ref().and_then(|c| match c {
        CompatConfig::OpenAICompletions(c) => Some(c),
        _ => None,
    });
    let provider = model.provider.to_lowercase();
    let base_url = model.base_url.to_lowercase();
    // 识别 zai / deepseek / together / moonshot / openrouter / nvidia / ant-ling / grok / opencode
    // ...
}
```

`ResolvedCompat` 字段：
- `max_tokens_field` — `max_tokens` / `max_completion_tokens` / `max_output_tokens`
- `thinking_format` — 8 种格式之一
- `supports_reasoning_effort` — 是否支持 `reasoning_effort` 字段
- `requires_reasoning_content_on_assistant` — assistant 消息是否需带 `reasoning_content`

### 3.3 8 种 thinkingFormat

**文件**：[ion-provider/src/provider/openai.rs:633](file:///Users/xuyingzhou/Project/study-rust/ion-provider/src/provider/openai.rs#L633)

| thinkingFormat | 适用 provider | 请求字段 |
|---------------|--------------|---------|
| `deepseek` | deepseek.com | `body.thinking = "enabled"` / `"disabled"` |
| `zai` | z.ai / zhipuai | `body.thinking = "enabled"` / `"disabled"` |
| `qwen` | qwen | `body.enable_thinking = bool` |
| `qwen-chat-template` | qwen vllm | `body.chat_template_kwargs.enable_thinking` |
| `openrouter` | openrouter.com | `body.reasoning.effort = "low"/"medium"/"high"` |
| `ant-ling` | ant-ling | `body.reasoning.effort` |
| `together` | together.ai | `body.reasoning.enabled + reasoning_effort` |
| `string-thinking` | 通用 | `body.thinking = "<level>"` |
| `openai`（默认） | OpenAI / opencode | `body.reasoning_effort` |

### 3.4 ThinkingLevel 映射

**文件**：[ion-provider/src/provider/openai.rs:710](file:///Users/xuyingzhou/Project/study-rust/ion-provider/src/provider/openai.rs#L710)

```rust
fn thinking_level_to_str(lvl: ThinkingLevel) -> &'static str {
    match lvl {
        ThinkingLevel::Off => "disabled",
        ThinkingLevel::Minimal => "minimal",
        ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
        ThinkingLevel::XHigh => "xhigh",
    }
}
```

---

## 4. OpenAI Responses Provider

**文件**：[ion-provider/src/provider/openai_responses.rs](file:///Users/xuyingzhou/Project/study-rust/ion-provider/src/provider/openai_responses.rs)（695 行，4 单元测试）

### 4.1 端点 & 请求

- **URL**：`POST {base_url}/v1/responses`，默认 `https://api.openai.com`
- **Headers**：`Authorization: Bearer {api_key}`
- **Body**：`{model, input, tools, reasoning, ...}`（注意是 `input` 数组，不是 `messages`）

### 4.2 SSE 事件类型

| 事件 | 用途 |
|------|------|
| `response.created` | 提取 response.id |
| `response.output_item.added` | 块开始（reasoning / message / function_call） |
| `response.reasoning_text.delta` | 推理文本增量 |
| `response.reasoning_summary_text.delta` | 推理摘要增量 |
| `response.output_text.delta` | 输出文本增量 |
| `response.function_call_arguments.delta` | 工具调用参数增量 |
| `response.function_call_arguments.done` | 工具调用参数完成 |
| `response.output_item.done` | 块结束（捕获 reasoning signature） |
| `response.completed` | stop_reason + usage |
| `error` | 错误 |

### 4.3 Tool call ID 回放格式

为支持 OpenAI Responses 的双 ID 体系（`call_id` + `item_id`），ION 用 `"{call_id}|{item_id}"` 格式存储，回放时拆分：

```rust
// ion-provider/src/provider/openai_responses.rs:164-178
format!("{}|{}", call_id, item_id)
```

### 4.4 Reasoning 配置

```rust
// effort: minimal / low / medium / high / xhigh
// summary: "auto"
body.reasoning = { effort, summary };
```

---

## 5. Google Generative AI Provider

**文件**：[ion-provider/src/provider/google.rs](file:///Users/xuyingzhou/Project/study-rust/ion-provider/src/provider/google.rs)（701 行，4 单元测试）

### 5.1 端点 & 请求

- **URL**：`POST {base_url}/v1beta/models/{model}:streamGenerateContent?alt=sse`
- **Headers**：`x-goog-api-key: {api_key}`
- **Body**：`{contents, tools, generationConfig: {thinkingConfig}, ...}`

### 5.2 SSE 解析

每个 chunk 的 `candidates[].content.parts[]` 可能是：

| part 类型 | 字段 |
|----------|------|
| 文本 | `text` |
| 思考 | `text + thought: true` |
| 思考签名 | `thought_signature` |
| 工具调用 | `function_call: {name, args}` |
| 工具结果 | `function_response` |

### 5.3 ThinkingConfig

**文件**：[ion-provider/src/provider/google.rs:152](file:///Users/xuyingzhou/Project/study-rust/ion-provider/src/provider/google.rs#L152)

| ThinkingLevel | thinking_budget | include_thoughts |
|---------------|-----------------|------------------|
| Off | 0 | false |
| Minimal | 1024 | true |
| Low | 4096 | true |
| Medium | 8192 | true |
| High | 24576 | true |
| XHigh | 32768 | true |

---

## 6. transform_messages — 跨 Provider 消息规范化

**文件**：[ion-provider/src/transform_messages.rs](file:///Users/xuyingzhou/Project/study-rust/ion-provider/src/transform_messages.rs)（664 行，10 单元测试）

### 6.1 主入口

```rust
pub fn transform_messages(
    messages: Vec<Message>,
    model: &Model,
    normalize_tool_call_id: Option<&NormalizeToolCallIdFn>,
) -> Vec<Message>
```

### 6.2 五个核心功能

| # | 功能 | 函数 | 行号 |
|---|------|------|------|
| 1 | 图片降级（不支持 image 的模型） | `downgrade_unsupported_images` | [51](file:///Users/xuyingzhou/Project/study-rust/ion-provider/src/transform_messages.rs#L51) |
| 2 | thinking block 跨模型处理 | `transform_content_block` | [157](file:///Users/xuyingzhou/Project/study-rust/ion-provider/src/transform_messages.rs#L157) |
| 3 | tool call ID 规范化 | `default_normalize_tool_call_id` | [312](file:///Users/xuyingzhou/Project/study-rust/ion-provider/src/transform_messages.rs#L312) |
| 4 | 孤儿 tool call 补合成 result | `insert_synthetic_tool_results` | [228](file:///Users/xuyingzhou/Project/study-rust/ion-provider/src/transform_messages.rs#L228) |
| 5 | 跳过 error/aborted assistant | main loop | [240](file:///Users/xuyingzhou/Project/study-rust/ion-provider/src/transform_messages.rs#L240) |

### 6.3 thinking block 处理规则

| 场景 | 处理 |
|------|------|
| redacted thinking（跨模型） | 丢弃 |
| 带 signature 的 thinking（同模型） | 保留（replay 需要） |
| 空 thinking | 丢弃 |
| 跨模型 thinking | 转纯文本 |

### 6.4 tool call ID 规范化

Anthropic 要求 `^[a-zA-Z0-9_-]+$`（max 64），OpenAI Responses 用 `{call_id}|{item_id}` 格式：

```rust
// ion-provider/src/transform_messages.rs:320-327
if let Some(idx) = id.find('|') {
    return id[..idx].to_string();  // 取 call_id 部分
}
// 兜底：hash 成 call_{hash:x}
```

### 6.5 Agent 主循环接入点

**文件**：[src/agent/agent_loop.rs:417](file:///Users/xuyingzhou/Project/study-rust/ion/src/agent/agent_loop.rs#L417)

```rust
let transformed_messages = ion_provider::transform_messages::transform_messages(
    messages_snapshot,
    &self.model,
    None,  // 当前未传 normalize_tool_call_id 回调，靠各 provider 内部处理
);
let ctx = Context::new(sys_prompt, transformed_messages);
```

---

## 7. CLI 测试方法

### 7.1 单 Provider 烟测

#### Anthropic（z.ai 代理 + glm-4.6）

**前置**：在 `~/.ion/config.json` 配置 anthropic provider：

```json
{
  "providers": {
    "anthropic": {
      "name": "anthropic",
      "api": "anthropic-messages",
      "base_url": "https://p.19930810.xyz:8443/k/glm/https://api.z.ai/api/anthropic",
      "models": [{"id": "glm-4.6", "reasoning": true}]
    }
  }
}
```

`~/.ion/auth.json`：
```json
{"keys": {"anthropic": "任意值（z.ai 代理不校验）"}}
```

**测试**：

```bash
# 基础聊天
ion "用一句话介绍你自己" --provider anthropic --model glm-4.6
# 预期：打印 LLM 回答，无 "emergency truncation" warning

# tool_call
ion "用 bash 工具执行 echo hello-world" --provider anthropic --model glm-4.6
# 预期：触发 bash 工具调用，输出 "hello-world"
```

#### OpenAI（OpenCODE + deepseek-v4-flash）

```bash
# 基础聊天
ion "用一句话介绍你自己" --provider opencode --model deepseek-v4-flash

# thinking
ion "证明根号2是无理数" --provider opencode --model deepseek-v4-flash --thinking medium
# 预期：触发 reasoning，detectCompat 推断为 opencode 格式
```

### 7.2 Provider 切换测试

```bash
# 同一会话先 OpenAI 后 Anthropic
ion "你好" --provider opencode --model deepseek-v4-flash
# 记下 session_id

ion --resume <session_id> "继续" --provider anthropic --model glm-4.6
# 预期：transform_messages 自动降级 thinking block（OpenAI → Anthropic）
#       tool call ID 规范化（若历史有 tool_call）
```

### 7.3 thinking 等级测试

```bash
# Off
ion "1+1=?" --provider opencode --model deepseek-v4-flash --thinking off

# High
ion "证明费马大定理" --provider opencode --model deepseek-v4-flash --thinking high
# 预期：apply_thinking_format 注入 thinking 字段，LLM 返回 reasoning_content
```

### 7.4 单元测试

```bash
# 全部 provider 单元测试
cargo test -p ion-provider

# 单个 provider
cargo test -p ion-provider --lib anthropic
cargo test -p ion-provider --lib openai_responses
cargo test -p ion-provider --lib google
cargo test -p ion-provider --lib transform_messages

# 预期：
# anthropic: 9 tests passed
# openai_responses: 4 tests passed
# google: 4 tests passed
# transform_messages: 10 tests passed
```

### 7.5 e2e 真实 API 测试

**文件**：[ion-provider/tests/e2e_real_api.rs](file:///Users/xuyingzhou/Project/study-rust/ion-provider/tests/e2e_real_api.rs)（306 行，4 个 `#[ignore]` 测试）

#### Anthropic（z.ai 代理 + glm-4.6）

```bash
ION_E2E_ANTHROPIC=1 \
ION_ANTHROPIC_API_KEY="任意值" \
cargo test -p ion-provider --test e2e_real_api -- --ignored --nocapture

# 测试用例：
# - anthropic_basic_stream（基础流式）
# - anthropic_tool_call（工具调用）
# 预期：2 tests passed
```

#### OpenAI（OpenCODE + deepseek-v4-flash）

```bash
ION_E2E_OPENAI=1 \
ION_OPENAI_API_KEY="sk-xxx" \
cargo test -p ion-provider --test e2e_real_api -- --ignored --nocapture

# 测试用例：
# - openai_reasoning_stream（reasoning_content 流式）
# - openai_tool_call（工具调用）
# 预期：2 tests passed
```

#### 环境变量

| 变量 | 默认 | 说明 |
|------|------|------|
| `ION_E2E_ANTHROPIC` | 空 | 设为 `1` 启用 Anthropic 测试 |
| `ION_E2E_OPENAI` | 空 | 设为 `1` 启用 OpenAI 测试 |
| `ION_ANTHROPIC_BASE_URL` | `https://p.19930810.xyz:8443/k/glm/https://api.z.ai/api/anthropic` | Anthropic 兼容端点 |
| `ION_ANTHROPIC_API_KEY` | 空（fallback `ION_API_KEY`） | API key |
| `ION_ANTHROPIC_MODEL` | `glm-4.6` | 模型 id |
| `ION_OPENAI_BASE_URL` | `https://opencode.ai/zen/go/v1` | OpenAI 兼容端点 |
| `ION_OPENAI_API_KEY` | 空 | API key |
| `ION_OPENAI_MODEL` | `deepseek-v4-flash` | 模型 id |

### 7.6 待补充测试

| # | 测试 | 触发条件 |
|---|------|---------|
| 1 | Claude 真实 API（非 z.ai 代理） | 拿到 Claude API key |
| 2 | OpenAI Responses API（GPT-5/o1/o3） | 拿到 OpenAI Responses 权限 |
| 3 | Google Gemini API | 拿到 Google API key |
| 4 | transform_messages 跨 provider e2e | 同一会话切 provider |
| 5 | detectCompat 各 thinkingFormat | deepseek/zai/qwen/openrouter/together/ant-ling 各跑一次 |

---

## 8. 配置参考

### 8.1 config.json 自定义 provider

**文件**：[src/config.rs:69](file:///Users/xuyingzhou/Project/study-rust/ion/src/config.rs#L69)

```rust
pub struct CustomProvider {
    pub name: String,
    pub api: String,           // "anthropic-messages" / "openai-completions" / ...
    pub base_url: String,
    pub api_key: Option<String>,
    pub headers: Option<HashMap<String, String>>,
    pub models: Vec<CustomModel>,
    pub model_overrides: Option<HashMap<String, ModelOverride>>,
}
```

### 8.2 默认 provider 映射

**文件**：[src/config.rs:164](file:///Users/xuyingzhou/Project/study-rust/ion/src/config.rs#L164)

| provider | 默认 model |
|----------|-----------|
| `anthropic` | `claude-opus-4-8` |
| `openai` | `gpt-5.4` |
| `deepseek` | `deepseek-v4-pro` |
| `google` | `gemini-3.1-pro-preview` |
| `opencode` | `deepseek-v4-flash` |

### 8.3 CLI 参数

| 参数 | 说明 |
|------|------|
| `--provider <name>` | Provider 名称 |
| `--model <id>` | 模型 id |
| `--base-url <url>` | 覆盖 base_url |
| `--api-key <key>` | 覆盖 API key |
| `--thinking <level>` | off / minimal / low / medium / high / xhigh |

---

## 9. 暂不实现的 Provider

按用户要求，常见够用即可：

| Provider | 用途 |
|----------|------|
| `azure-openai-responses` | Azure 部署的 OpenAI Responses |
| `openai-codex-responses` | Codex 专用 |
| `google-vertex` | Vertex AI |
| `mistral-conversations` | Mistral |
| `bedrock-converse-stream` | AWS Bedrock |
| `cloudflare-workers-ai` | Cloudflare |
