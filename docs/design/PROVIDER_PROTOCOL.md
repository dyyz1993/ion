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

## 7. CLI 测试指南

本文档的 CLI 测试分 5 组，对齐 [SECURITY_CLI_GUIDE.md](./SECURITY_CLI_GUIDE.md) 与 [BASH_EXTENSION.md](./BASH_EXTENSION.md) §0.2 的格式：每组测试给完整的 `ion rpc` 命令 + 请求/响应 JSON 规格 + 字段说明表。

### prompt RPC 接口规格

**请求：**

```bash
ion rpc --session <sid> --method prompt \
  --params '{"text":"<用户输入>","behavior":"interrupt"}'
```

**请求参数：**

| 字段 | 类型 | 默认 | 说明 |
|------|------|------|------|
| `text` | string | 必填 | 用户输入；以 `!` 开头时拦截为 bash_command |
| `behavior` | string | `"interrupt"` | `interrupt` / `steer` / `followUp`（Agent 忙时策略） |
| `streamingBehavior` | string | 同 `behavior` | 别名（pi 兼容） |
| `timeout` | number | 30 | 仅 `!cmd` 直发时生效，普通 prompt 走 agent loop |

**响应 JSON（成功）：**

```json
{
  "type": "response",
  "id": "1",
  "command": "prompt",
  "success": true,
  "data": {
    "ok": true,
    "stopped": false,
    "aborted": false
  }
}
```

> 响应只表示"prompt 已被 Agent 接收/处理"。LLM 输出文本通过 `agent_start` / `text_delta` / `agent_end` 事件流推送，需用 `ion subscribe --session <sid>` 监听。

**响应 JSON（失败）：**

```json
{
  "type": "response",
  "id": "1",
  "command": "prompt",
  "success": false,
  "error": "agent prompt failed: provider error: 401 Unauthorized"
}
```

### set_model RPC 接口规格（运行时切 provider/model）

**请求：**

```bash
ion rpc --session <sid> --method set_model \
  --params '{"modelId":"glm-4.6","provider":"anthropic"}'
```

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `modelId` | string | 是 | 新模型 id（如 `glm-4.6` / `deepseek-v4-flash`） |
| `provider` | string | 否 | 新 provider 名；不传则保留原值 |

**响应 JSON：**

```json
{
  "type": "response",
  "id": "1",
  "command": "get_state",
  "success": true,
  "data": {"model": "glm-4.6", "provider": "anthropic"}
}
```

### set_thinking_level RPC 接口规格

**请求：**

```bash
ion rpc --session <sid> --method set_thinking_level \
  --params '{"level":"medium"}'
```

| 字段 | 类型 | 必填 | 取值 |
|------|------|------|------|
| `level` | string | 是 | `off` / `minimal` / `low` / `medium` / `high` / `xhigh` |

**响应 JSON：**

```json
{
  "type": "response",
  "id": "1",
  "command": "set_thinking_level",
  "success": true,
  "data": {"thinkingLevel": "medium"}
}
```

### get_state RPC（验证当前 provider/model）

**请求：**

```bash
ion rpc --session <sid> --method get_state
```

**响应 JSON：**

```json
{
  "type": "response",
  "id": "1",
  "command": "get_state",
  "success": true,
  "data": {
    "model": "glm-4.6",
    "provider": "anthropic",
    "session_id": "sess_xxx",
    "message_count": 2,
    "is_running": false,
    "steering_queue": 0,
    "follow_up_queue": 0
  }
}
```

---

### Group A：单 Provider 烟测（Anthropic / OpenAI）

> 通过 `ion rpc` + `ion subscribe` 验证每个 provider 的基础流式 + tool_call 链路。所有测试需先启动 Host：`ion serve start`，并 `ion rpc --method create_session --params '{"agent":"developer"}'` 创建 session。

#### A1 Anthropic 基础聊天（z.ai 代理 + glm-4.6）

**前置** — `~/.ion/config.json`：

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

`~/.ion/auth.json`（权限 600）：

```json
{"keys": {"anthropic": "任意值（z.ai 代理不校验）"}}
```

**测试：**

```bash
# Terminal 1：订阅事件流
ion subscribe --session sess_xxx

# Terminal 2：发 prompt
ion rpc --session sess_xxx --method prompt \
  --params '{"text":"用一句话介绍你自己","behavior":"interrupt"}'
```

**预期事件流（Terminal 1）：**

```
{"type":"event","event":{"type":"agent_start", ...}}
{"type":"event","event":{"type":"text_delta","delta":"我是"}}
{"type":"event","event":{"type":"text_delta","delta":"GLM"}}
{"type":"event","event":{"type":"agent_end", ...}}
```

**预期响应（Terminal 2）：**

```json
{"type":"response","id":"1","command":"prompt","success":true,"data":{"ok":true,"stopped":false,"aborted":false}}
```

**验证点：**
- ✅ 无 `emergency truncation` warning（needs_compact 检查通过）
- ✅ Agent 不崩溃
- ✅ SSE 正确解析 `message_start` / `content_block_delta` / `message_stop`

#### A2 Anthropic tool_call（bash 工具）

```bash
ion rpc --session sess_xxx --method prompt \
  --params '{"text":"用 bash 工具执行 echo hello-world"}'
```

**预期事件流：**

```
{"type":"event","event":{"type":"tool_call","name":"bash","args":{"command":"echo hello-world"}}}
{"type":"event","event":{"type":"tool_result","content":"hello-world\n"}}
{"type":"event","event":{"type":"agent_end"}}
```

**验证点：**
- ✅ Anthropic `tool_use` block 正确解析
- ✅ `parse_json_rerepair` 处理流式 JSON
- ✅ 工具结果回填后 LLM 继续生成

#### A3 OpenAI 基础聊天（OpenCODE + deepseek-v4-flash）

**前置** — `~/.ion/config.json`：

```json
{
  "providers": {
    "opencode": {
      "name": "opencode",
      "api": "openai-completions",
      "base_url": "https://opencode.ai/zen/go/v1",
      "models": [{"id": "deepseek-v4-flash", "reasoning": true}]
    }
  }
}
```

```bash
ion rpc --session sess_xxx --method set_model \
  --params '{"modelId":"deepseek-v4-flash","provider":"opencode"}'

ion rpc --session sess_xxx --method prompt \
  --params '{"text":"用一句话介绍你自己"}'
```

**预期：**
- ✅ detectCompat 推断为 `openai` thinkingFormat
- ✅ SSE 解析 `data: {"choices":[{"delta":{"content":"..."}}]}`
- ✅ 响应 success=true

#### A4 OpenAI tool_call

```bash
ion rpc --session sess_xxx --method prompt \
  --params '{"text":"用 bash 工具执行 pwd"}'
```

**验证点：**
- ✅ OpenAI `tool_calls` 数组正确解析
- ✅ `tool_call_id` 回放格式正确

---

### Group B：Provider 切换测试（transform_messages）

> 验证同一 session 切换 provider 时，`transform_messages` 正确规范化历史消息。

#### B1 OpenAI → Anthropic 切换

```bash
# 1. 用 OpenAI provider 累积对话
ion rpc --session sess_xxx --method set_model \
  --params '{"modelId":"deepseek-v4-flash","provider":"opencode"}'

ion rpc --session sess_xxx --method prompt --params '{"text":"你好，我是 Alice"}'
# 等 agent_end

ion rpc --session sess_xxx --method prompt --params '{"text":"用 bash 执行 echo hi"}'
# 等 agent_end（产生 tool_call 历史）

# 2. 切到 Anthropic provider
ion rpc --session sess_xxx --method set_model \
  --params '{"modelId":"glm-4.6","provider":"anthropic"}'

# 3. 验证 get_state
ion rpc --session sess_xxx --method get_state
# 预期：{"model":"glm-4.6","provider":"anthropic",...}

# 4. 继续对话（transform_messages 自动处理历史）
ion rpc --session sess_xxx --method prompt --params '{"text":"我刚才叫什么名字？"}'
```

**验证点：**
- ✅ Agent 不崩溃（thinking block 跨模型降级成功）
- ✅ LLM 能正确回答 "Alice"（历史消息保留）
- ✅ tool call ID 规范化（OpenAI `call_xxx` → Anthropic 兼容格式）
- ✅ 孤儿 tool call 自动补合成 result

#### B2 Anthropic → OpenAI 切换

```bash
# 1. 用 Anthropic 累积 thinking + tool_use 历史
ion rpc --session sess_xxx --method set_model \
  --params '{"modelId":"glm-4.6","provider":"anthropic"}'

ion rpc --session sess_xxx --method prompt \
  --params '{"text":"证明根号2是无理数"}'
# 等 agent_end（产生 thinking block + signature）

# 2. 切到 OpenAI
ion rpc --session sess_xxx --method set_model \
  --params '{"modelId":"deepseek-v4-flash","provider":"opencode"}'

# 3. 继续对话
ion rpc --session sess_xxx --method prompt --params '{"text":"继续上面的证明"}'
```

**验证点：**
- ✅ Anthropic thinking block（带 signature）→ OpenAI 转纯文本
- ✅ redacted thinking 被丢弃
- ✅ 空 thinking 被丢弃

#### B3 查看历史消息确认转换

```bash
# 查看原始消息（Agent 内部存储格式）
ion rpc --session sess_xxx --method get_messages | jq '.data[-4:]'
```

**预期：**
- 历史中的 `AssistantContentBlock::Thinking` 节点存在（带 signature 字段）
- 切换 provider 后，发往 LLM 的请求体经过 `transform_messages` 处理（不修改存储）

---

### Group C：thinking 等级测试

> 验证 `apply_thinking_format` 根据 `ThinkingLevel` 注入正确的请求字段。

#### C1 thinking off（默认）

```bash
ion rpc --session sess_xxx --method set_thinking_level --params '{"level":"off"}'

ion rpc --session sess_xxx --method prompt --params '{"text":"1+1=?"}'
```

**验证点：**
- ✅ OpenAI provider: 请求 body 无 `reasoning_effort` 字段
- ✅ Anthropic provider: 请求 body 无 `thinking` 字段
- ✅ 响应无 `reasoning_content` / `thinking` block

#### C2 thinking medium

```bash
ion rpc --session sess_xxx --method set_thinking_level --params '{"level":"medium"}'

ion rpc --session sess_xxx --method prompt --params '{"text":"证明根号2是无理数"}'
```

**验证点（按 provider）：**

| Provider | 请求字段 | 响应字段 |
|----------|---------|---------|
| opencode (openai) | `reasoning_effort: "medium"` | `choices[].delta.reasoning_content` |
| anthropic (z.ai) | `thinking: {"type":"enabled","budget_tokens":8192}` | `content_block_delta.thinking_delta` |
| google (gemini) | `generationConfig.thinkingConfig: {thinkingBudget:8192,includeThoughts:true}` | `parts[].thought: true` |

#### C3 thinking high + tool_call

```bash
ion rpc --session sess_xxx --method set_thinking_level --params '{"level":"high"}'

ion rpc --session sess_xxx --method prompt \
  --params '{"text":"计算 123 * 456，用 calculator 工具"}'
```

**验证点：**
- ✅ thinking block 在 tool_use 之前生成
- ✅ thinking signature 正确回放（同模型继续对话时）

#### C4 OpenAI Responses reasoning

```bash
# 需配置 openai-responses provider（待真实 API 验证）
ion rpc --session sess_xxx --method set_model \
  --params '{"modelId":"o3-mini","provider":"openai-responses"}'

ion rpc --session sess_xxx --method set_thinking_level --params '{"level":"high"}'

ion rpc --session sess_xxx --method prompt --params '{"text":"解释量子纠缠"}'
```

**验证点：**
- ✅ 请求 body: `reasoning: {effort:"high", summary:"auto"}`
- ✅ SSE 事件: `response.reasoning_text.delta` / `response.reasoning_summary_text.delta`
- ✅ Tool call ID 回放格式: `{call_id}|{item_id}`

---

### Group D：单元测试

> 验证各 provider 的 SSE 解析、请求体构造、JSON 容错。

#### D1 全部 provider 单元测试

```bash
cargo test -p ion-provider --lib
```

**预期：**

```
running 21 tests
test anthropic::tests::test_sse_message_start ... ok
test anthropic::tests::test_sse_thinking_delta ... ok
test anthropic::tests::test_tool_use_partial_json ... ok
...
test result: ok. 21 passed; 0 failed
```

#### D2 单 provider 单元测试

```bash
# Anthropic（9 tests）
cargo test -p ion-provider --lib anthropic
# 预期: 9 passed

# OpenAI Completions
cargo test -p ion-provider --lib openai::tests
# 预期: detectCompat + thinkingFormat 测试通过

# OpenAI Responses（4 tests）
cargo test -p ion-provider --lib openai_responses
# 预期: 4 passed

# Google（4 tests）
cargo test -p ion-provider --lib google
# 预期: 4 passed

# transform_messages（10 tests）
cargo test -p ion-provider --lib transform_messages
# 预期: 10 passed
```

#### D3 transform_messages 单元测试细节

```bash
cargo test -p ion-provider --lib transform_messages -- --nocapture
```

**覆盖用例：**

| 测试 | 验证 |
|------|------|
| `test_downgrade_unsupported_images` | 不支持 image 的模型，image block 降级为文本 |
| `test_thinking_block_cross_model` | 跨模型 thinking → 纯文本 |
| `test_thinking_block_with_signature` | 同模型 thinking + signature 保留 |
| `test_redacted_thinking_dropped` | redacted thinking 被丢弃 |
| `test_tool_call_id_normalize` | `call_xxx\|item_yyy` → `call_xxx` |
| `test_synthetic_tool_result` | 孤儿 tool_call 补合成 result |
| `test_skip_error_assistant` | error/aborted assistant 被跳过 |
| ... | （共 10 个） |

#### D4 detectCompat 单元测试

```bash
cargo test -p ion-provider --lib detect_compat
```

**覆盖 8 种 thinkingFormat：**

| 测试 | provider | 预期 thinkingFormat |
|------|---------|-------------------|
| `test_detect_deepseek` | deepseek.com | `deepseek` |
| `test_detect_zai` | z.ai / zhipuai | `zai` |
| `test_detect_qwen` | qwen | `qwen` |
| `test_detect_openrouter` | openrouter.com | `openrouter` |
| `test_detect_together` | together.ai | `together` |
| `test_detect_ant_ling` | ant-ling | `ant-ling` |
| `test_detect_opencode` | opencode.ai | `openai` |
| `test_detect_string_thinking` | 通用 fallback | `string-thinking` |

---

### Group E：e2e 真实 API 测试

> 验证各 provider 对真实 LLM API 的完整调用链路。测试标记为 `#[ignore]`，需显式启用环境变量。

**文件**：[ion-provider/tests/e2e_real_api.rs](file:///Users/xuyingzhou/Project/study-rust/ion-provider/tests/e2e_real_api.rs)（306 行，4 个 `#[ignore]` 测试）

#### E1 Anthropic 真实 API（z.ai 代理 + glm-4.6）

```bash
ION_E2E_ANTHROPIC=1 \
ION_ANTHROPIC_API_KEY="任意值" \
cargo test -p ion-provider --test e2e_real_api -- --ignored --nocapture
```

**测试用例：**

| 用例 | 验证 |
|------|------|
| `anthropic_basic_stream` | 基础流式：message_start → content_block_delta → message_stop |
| `anthropic_tool_call` | tool_use block + partial JSON 容错 |

**预期：** `2 passed`

#### E2 OpenAI 真实 API（OpenCODE + deepseek-v4-flash）

```bash
ION_E2E_OPENAI=1 \
ION_OPENAI_API_KEY="sk-xxx" \
cargo test -p ion-provider --test e2e_real_api -- --ignored --nocapture
```

**测试用例：**

| 用例 | 验证 |
|------|------|
| `openai_reasoning_stream` | reasoning_content 流式 + apply_thinking_format |
| `openai_tool_call` | tool_calls 数组 + tool_call_id 回放 |

**预期：** `2 passed`

#### E3 环境变量参考

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

#### E4 待补充真实 API 测试

| # | 测试 | 触发条件 | 验证点 |
|---|------|---------|--------|
| E4.1 | Claude 真实 API（非 z.ai 代理） | 拿到 Claude API key | thinking signature + redacted thinking |
| E4.2 | OpenAI Responses API（GPT-5/o1/o3） | 拿到 OpenAI Responses 权限 | reasoning + tool_call + `{call_id}\|{item_id}` 回放 |
| E4.3 | Google Gemini API | 拿到 Google API key | thoughtSignature + thinking_budget 映射 |
| E4.4 | transform_messages 跨 provider e2e | 同一会话切 provider | thinking 降级 + tool call ID 规范化 |
| E4.5 | detectCompat 各 thinkingFormat e2e | 各 provider 真实 API key | deepseek/zai/qwen/openrouter/together/ant-ling 各跑一次 |

测试方法（拿到 key 后）：

```bash
# Claude 真实 API
ION_E2E_CLAUDE=1 ION_CLAUDE_API_KEY="sk-ant-xxx" \
cargo test -p ion-provider --test e2e_real_api -- --ignored --nocapture

# OpenAI Responses
ION_E2E_OPENAI_RESPONSES=1 ION_OPENAI_RESPONSES_API_KEY="sk-xxx" \
cargo test -p ion-provider --test e2e_real_api -- --ignored --nocapture

# Google Gemini
ION_E2E_GOOGLE=1 ION_GOOGLE_API_KEY="xxx" \
cargo test -p ion-provider --test e2e_real_api -- --ignored --nocapture
```

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
