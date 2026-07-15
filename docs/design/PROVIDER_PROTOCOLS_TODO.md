# 缺失 Provider 协议 规划文档

> **状态：待定** — pi 支持 9 种 API 协议，ION 实现 4 种。本文档规划剩余 5 种的补齐方案。
>
> 对标 pi 的 `packages/ai/src/providers/`。

---

## 何时使用这个文档

- 要接入 Mistral / Azure OpenAI Responses / Codex / Vertex AI / Bedrock 的模型时
- 要给 ion-provider crate 加新协议实现时

**前置阅读**：[PROVIDER_PROTOCOL.md](./PROVIDER_PROTOCOL.md)

---

## 1. 现状

### ION 已实现（4 + 2 内部）

| 协议 | 文件 | 状态 |
|------|------|------|
| `openai-completions` | `ion-provider/src/provider/openai.rs` | ✅ |
| `anthropic-messages` | `ion-provider/src/provider/anthropic.rs` | ✅ |
| `google-generative-ai` | `ion-provider/src/provider/google.rs` | ✅ |
| `openai-responses` | `ion-provider/src/provider/openai_responses.rs` | ✅ |
| `faux`（测试 mock） | `ion-provider/src/faux.rs` | ✅ 内部 |
| `replay`（录制回放） | `ion-provider/src/replay.rs` | ✅ 内部 |

### pi 有但 ION 缺（5 种）

| 协议 | 用途 | 严重程度 | 需要 API key |
|------|------|---------|-------------|
| `mistral-conversations` | Mistral AI（Codestral / Large 等） | 🔴 | Mistral API key |
| `azure-openai-responses` | Azure 部署的 OpenAI Responses API | 🔴 | Azure endpoint + key |
| `openai-codex-responses` | OpenAI Codex 专用（gpt-5.5-codex） | 🔴 | OpenAI Codex key |
| `google-vertex` | Google Vertex AI（区别于 Generative AI） | 🟡 | GCP service account |
| `bedrock-converse-stream` | Amazon Bedrock（Claude / Llama 等） | 🔴 | AWS credentials |

## 2. 每个协议的实现规划

### 2.1 `mistral-conversations`

**Mistral Conversations API** — 类似 OpenAI 但有差异。

**关键差异**：
- endpoint: `https://api.mistral.ai/v1/chat/completions`
- 认证：`Authorization: Bearer <api_key>`
- 消息格式：基本兼容 OpenAI，但 `system` role 处理不同（Mistral 用特定前缀）
- tool_call 格式：与 OpenAI 略有差异
- thinking/reasoning：Codestral 支持 `thinking` 参数

**实现文件**：`ion-provider/src/provider/mistral.rs`（新建）

**参照**：`openai.rs`（Mistral 大部分兼容 OpenAI，可继承复用 SSE 解析逻辑）

**改动**：
```rust
// ion-provider/src/provider/mod.rs 注册
registry.register("mistral-conversations", Box::new(MistralProvider));

// ion-provider/src/provider/mistral.rs
pub struct MistralProvider;

#[async_trait]
impl ApiProvider for MistralProvider {
    async fn stream(&self, registry, model, context, options) -> ProviderResult<EventStream> {
        // 1. transform_messages：OpenAI 格式 + Mistral 的 system 处理
        // 2. 构造请求 body（基本同 OpenAI，加 Mistral 专属参数）
        // 3. POST https://api.mistral.ai/v1/chat/completions
        // 4. 解析 SSE 流（复用 openai.rs 的 SSE 解析）
    }
}
```

**估计**：~250 行（大部分复用 openai.rs）

### 2.2 `azure-openai-responses`

**Azure OpenAI Responses API** — OpenAI Responses 的 Azure 部署版。

**关键差异**：
- endpoint: `https://{resource}.cognitiveservices.azure.com/openai/v1/responses`
- 认证：`api-key: <azure_key>` header（不是 Bearer）
- URL 含 deployment name
- 支持 Microsoft Entra ID（Azure AD）认证

**实现文件**：`ion-provider/src/provider/azure_openai.rs`（新建）

**参照**：`openai_responses.rs`（协议几乎一样，只改 endpoint + 认证 header）

**估计**：~150 行（继承 openai_responses，覆写 endpoint + auth）

### 2.3 `openai-codex-responses`

**OpenAI Codex** — 代码生成专用。

**关键差异**：
- endpoint: `https://api.openai.com/v1/responses`（同 openai-responses）
- 认证：Bearer token（同 OpenAI）
- 专属 header：`OpenAI-Beta: codex=...`
- 专属参数：`codex` reasoning effort
- 额外的 GitHub Copilot token 交换流程

**实现文件**：`ion-provider/src/provider/codex.rs`（新建）

**参照**：`openai_responses.rs`（协议一样，加 header）

**估计**：~150 行

### 2.4 `google-vertex`

**Google Vertex AI** — 区别于 Generative AI（不同的认证和 endpoint）。

**关键差异**：
- endpoint: `https://{region}-aiplatform.googleapis.com/v1/projects/{project}/locations/{region}/publishers/google/models/{model}:streamGenerateContent`
- 认证：GCP service account JWT（不是 API key）
- 请求格式：基本同 Generative AI，但 URL 结构完全不同
- 需要区域路由

**实现文件**：`ion-provider/src/provider/google_vertex.rs`（新建）

**参照**：`google.rs`（消息格式一样，认证和 endpoint 不同）

**估计**：~300 行（JWT 认证是主要工作量）

### 2.5 `bedrock-converse-stream`

**Amazon Bedrock Converse API** — AWS 的多模型 API。

**关键差异**：
- endpoint: AWS SigV4 签名的 region-specific URL
- 认证：AWS Signature V4（access key + secret key + region）
- 请求格式：完全不同于 OpenAI/Anthropic（Bedrock 自己的 Converse 格式）
- 流式：event stream（AWS EventStream 编码，不是 SSE）
- 支持 Anthropic / Meta / Mistral / Amazon 等多家模型

**实现文件**：`ion-provider/src/provider/bedrock.rs`（新建）

**参照**：无（格式完全独立，是最复杂的实现）

**额外依赖**：AWS SigV4 签名库（或手写）+ EventStream 解析

**估计**：~400 行（SigV4 + EventStream 是主要工作量）

## 3. 注册 + models.json 配置

每个协议在 `ion-provider/src/provider/mod.rs` 的 `register_builtins` 里注册：

```rust
pub fn register_builtins(registry: &mut ModelRegistry) {
    // 现有
    registry.register("openai-completions", ...);
    registry.register("anthropic-messages", ...);
    registry.register("google-generative-ai", ...);
    registry.register("openai-responses", ...);
    // 新增
    registry.register("mistral-conversations", Box::new(MistralProvider));
    registry.register("azure-openai-responses", Box::new(AzureOpenAIProvider));
    registry.register("openai-codex-responses", Box::new(CodexProvider));
    registry.register("google-vertex", Box::new(GoogleVertexProvider));
    registry.register("bedrock-converse-stream", Box::new(BedrockProvider));
}
```

用户在 `~/.pi/agent/models.json`（ION 兼容读取）里配 provider + model：

```json
{
  "providers": {
    "mistral": {
      "baseUrl": "https://api.mistral.ai/v1",
      "api": "mistral-conversations",
      "models": {
        "codestral-latest": { "id": "codestral-latest", "name": "Codestral" }
      }
    }
  }
}
```

## 4. 认证环境变量

| 协议 | 环境变量 | 格式 |
|------|---------|------|
| mistral | `MISTRAL_API_KEY` | `Bearer <key>` |
| azure-openai | `AZURE_OPENAI_API_KEY` + `AZURE_OPENAI_ENDPOINT` | `api-key: <key>` |
| codex | `OPENAI_API_KEY`（或 `COPILOT_GITHUB_TOKEN`） | `Bearer <key>` |
| google-vertex | `GOOGLE_APPLICATION_CREDENTIALS`（JSON 文件路径） | JWT |
| bedrock | `AWS_ACCESS_KEY_ID` + `AWS_SECRET_ACCESS_KEY` + `AWS_REGION` | SigV4 |

## 5. 实现优先级建议

| 优先级 | 协议 | 理由 |
|--------|------|------|
| 1 | `mistral-conversations` | 最简单（复用 OpenAI），Codestral 是热门代码模型 |
| 2 | `azure-openai-responses` | 简单（复用 openai_responses），Azure 企业用户多 |
| 3 | `openai-codex-responses` | 简单（复用 openai_responses），Codex 用户 |
| 4 | `bedrock-converse-stream` | 复杂但重要（AWS 用户多，多模型统一接口） |
| 5 | `google-vertex` | 复杂（JWT），用户少于 Generative AI |

## 6. 并行开发注意事项

- **5 个协议互相独立**，可 5 个会话并行
- 都改 `ion-provider/src/provider/` 目录，但文件不同（各自新建一个 .rs）
- 注册在 `mod.rs` 的 `register_builtins`——**并行时注意 git merge**，各自加一行 `registry.register(...)` 不会冲突
- 每个协议需要对应的 API key 才能做真实 e2e 测试（标 `ION_E2E_<PROVIDER>=1`）
- 单元测试用 FauxProvider 验证消息转换逻辑，不调真实 API

## 7. 改动文件清单（每个协议）

| 文件 | 内容 | 行数 |
|------|------|------|
| `ion-provider/src/provider/<name>.rs` | ApiProvider 实现 | 150-400 |
| `ion-provider/src/provider/mod.rs` | 注册 | +1 行 |
| `ion-provider/tests/e2e_real_api.rs` | 真实 API 测试（#[ignore]） | ~30 |
| **每个协议总计** | | **180-430** |
