use crate::error::ProviderResult;
use crate::event_stream::EventStream;
use crate::types::*;
use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

/// Every provider implements `stream()`.
///
/// `cancel` 是可选的 HTTP 取消令牌：调用方在 abort 时 `cancel.cancel()`，
/// provider 内部用 `select!` 包 `.send().await`，cancel 时立刻返回 Aborted 错误，
/// 并 drop reqwest Response 关 TCP 连接。None 表示不支持取消（兼容旧调用）。
#[async_trait]
pub trait ApiProvider: Send + Sync {
    async fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&StreamOptions>,
        cancel: Option<CancellationToken>,
    ) -> ProviderResult<EventStream>;
}

/// Factory for creating fresh provider instances by API name.
/// Used by RecordingProvider to wrap a real provider without cloning.
pub trait ProviderFactory: Send + Sync {
    fn create(&self, api: &str) -> Option<Box<dyn ApiProvider>>;
}

/// Built-in provider factory — knows how to construct each builtin.
pub struct BuiltinProviderFactory;

impl ProviderFactory for BuiltinProviderFactory {
    fn create(&self, api: &str) -> Option<Box<dyn ApiProvider>> {
        match api {
            "openai-completions" => Some(Box::new(super::openai::OpenAICompletionsProvider)),
            "anthropic-messages" => Some(Box::new(super::anthropic::AnthropicMessagesProvider)),
            "openai-responses" => Some(Box::new(super::openai_responses::OpenAIResponsesProvider)),
            "openai-codex-responses" => Some(Box::new(super::codex::CodexResponsesProvider)),
            "azure-openai-responses" => Some(Box::new(super::azure::AzureOpenAIResponsesProvider::default())),
            "google-generative-ai" => Some(Box::new(super::google::GoogleGenerativeAIProvider)),
            "google-vertex" => Some(Box::new(super::vertex::GoogleVertexProvider)),
            "mistral-conversations" => Some(Box::new(super::mistral::MistralProvider)),
            "cloudflare-workers-ai" => Some(Box::new(super::cloudflare::CloudflareWorkersAIProvider)),
            "bedrock-converse-stream" => Some(Box::new(super::bedrock::BedrockConverseProvider)),
            _ => None,
        }
    }
}

use std::collections::HashMap;

pub struct ApiRegistry {
    providers: HashMap<String, Box<dyn ApiProvider>>,
}

impl ApiRegistry {
    pub fn new() -> Self {
        Self { providers: HashMap::new() }
    }

    pub fn register(&mut self, api: &str, provider: Box<dyn ApiProvider>) {
        self.providers.insert(api.to_string(), provider);
    }

    pub fn get(&self, api: &str) -> Option<&dyn ApiProvider> {
        self.providers.get(api).map(|p| p.as_ref())
    }

    pub fn register_builtins(&mut self) {
        self.register("openai-completions", Box::new(super::openai::OpenAICompletionsProvider));
        self.register("anthropic-messages", Box::new(super::anthropic::AnthropicMessagesProvider));
        self.register("openai-responses", Box::new(super::openai_responses::OpenAIResponsesProvider));
        self.register("openai-codex-responses", Box::new(super::codex::CodexResponsesProvider));
        self.register("azure-openai-responses", Box::new(super::azure::AzureOpenAIResponsesProvider::default()));
        self.register("google-generative-ai", Box::new(super::google::GoogleGenerativeAIProvider));
        self.register("google-vertex", Box::new(super::vertex::GoogleVertexProvider));
        self.register("mistral-conversations", Box::new(super::mistral::MistralProvider));
        self.register("cloudflare-workers-ai", Box::new(super::cloudflare::CloudflareWorkersAIProvider));
        self.register("bedrock-converse-stream", Box::new(super::bedrock::BedrockConverseProvider));
    }
}

pub struct ModelRegistry {
    models: HashMap<String, HashMap<String, Model>>,
}

impl ModelRegistry {
    pub fn new() -> Self {
        Self { models: HashMap::new() }
    }

    pub fn register(&mut self, model: Model) {
        self.models.entry(model.provider.clone()).or_default().insert(model.id.clone(), model);
    }

    pub fn get_model(&self, provider: &str, model_id: &str) -> Option<&Model> {
        self.models.get(provider)?.get(model_id)
    }

    pub fn find_model(&self, model_id: &str) -> Option<&Model> {
        for models in self.models.values() {
            if let Some(model) = models.get(model_id) {
                return Some(model);
            }
        }
        None
    }

    /// 列出所有已注册模型（get_available_models RPC 用）
    pub fn list_models(&self) -> Vec<&Model> {
        self.models.values().flat_map(|m| m.values()).collect()
    }

    /// 按 provider 列出模型（cycle_model 切换用）
    pub fn models_by_provider(&self, provider: &str) -> Vec<&Model> {
        self.models.get(provider)
            .map(|m| m.values().collect())
            .unwrap_or_default()
    }

    pub fn register_builtins(&mut self) {
        for m in builtin_models() { self.register(m); }
        // Also load from models.json
        if let Ok(content) = std::fs::read_to_string(Self::models_path()) {
            if let Ok(models_file) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(providers) = models_file.get("providers").and_then(|v| v.as_object()) {
                    for (provider_name, provider_cfg) in providers {
                        let base_url = provider_cfg.get("baseUrl").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let api = provider_cfg.get("api").and_then(|v| v.as_str()).unwrap_or("openai-completions").to_string();
                        let _api_key = provider_cfg.get("apiKey").and_then(|v| v.as_str()).map(|s| s.to_string());
                        let headers_map: Option<std::collections::HashMap<String, String>> = provider_cfg.get("headers")
                            .and_then(|v| v.as_object())
                            .map(|obj| obj.iter().map(|(k,v)| (k.clone(), v.as_str().unwrap_or("").to_string())).collect());
                        if let Some(models) = provider_cfg.get("models").and_then(|v| v.as_array()) {
                            for m in models {
                                if let Some(id) = m.get("id").and_then(|v| v.as_str()) {
                                    let name = m.get("name").and_then(|v| v.as_str()).unwrap_or(id).to_string();
                                    let reasoning = m.get("reasoning").and_then(|v| v.as_bool()).unwrap_or(false);
                                    let context_window = m.get("contextWindow").and_then(|v| v.as_u64()).unwrap_or(128000);
                                    let max_tokens = m.get("maxTokens").and_then(|v| v.as_u64()).unwrap_or(8192);
                                    let cost_input = m.get("cost").and_then(|c| c.get("input")).and_then(|v| v.as_f64()).unwrap_or(0.0);
                                    let cost_output = m.get("cost").and_then(|c| c.get("output")).and_then(|v| v.as_f64()).unwrap_or(0.0);
                                    let compat = if let Some(c) = m.get("compat") {
                                        serde_json::from_value(c.clone()).ok()
                                    } else { None };
                                    let mut headers = headers_map.clone();
                                    if let Some(h) = m.get("headers").and_then(|v| v.as_object()) {
                                        let mut extra = headers.unwrap_or_default();
                                        for (k,v) in h { extra.insert(k.clone(), v.as_str().unwrap_or("").to_string()); }
                                        headers = Some(extra);
                                    }
                                    self.register(Model {
                                        id: id.to_string(), name,
                                        api: api.clone(), provider: provider_name.clone(),
                                        base_url: base_url.clone(),
                                        reasoning, input: vec!["text".into()],
                                        cost: Cost { input: cost_input, output: cost_output, cache_read: 0.0, cache_write: 0.0 },
                                        context_window, max_tokens,
                                        compat, headers,
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    fn models_path() -> std::path::PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        let ion_path = std::path::Path::new(&home).join(".ion").join("models.json");
        if ion_path.exists() { return ion_path; }
        std::path::Path::new(&home).join(".pi").join("agent").join("models.json")
    }
}

fn builtin_models() -> Vec<Model> {
    vec![
        Model {
            id: "deepseek-v4-flash".into(), name: "DeepSeek V4 Flash".into(),
            api: "openai-completions".into(), provider: "opencode".into(),
            base_url: "https://opencode.ai/zen/go/v1".into(),
            reasoning: true, input: vec!["text".into()],
            cost: Cost { input: 0.0, output: 0.0, cache_read: 0.0, cache_write: 0.0 },
            context_window: 128000, max_tokens: 65536,
            compat: Some(CompatConfig::OpenAICompletions(OpenAICompletionsCompat {
                max_tokens_field: Some("max_tokens".into()),
                requires_reasoning_content_on_assistant_messages: Some(true),
                thinking_format: Some("deepseek".into()),
                ..Default::default()
            })),
            headers: None,
        },
        Model {
            id: "deepseek-v4-pro".into(), name: "DeepSeek V4 Pro".into(),
            api: "openai-completions".into(), provider: "opencode".into(),
            base_url: "https://opencode.ai/zen/go/v1".into(),
            reasoning: true, input: vec!["text".into()],
            cost: Cost { input: 0.0, output: 0.0, cache_read: 0.0, cache_write: 0.0 },
            context_window: 128000, max_tokens: 65536,
            compat: Some(CompatConfig::OpenAICompletions(OpenAICompletionsCompat {
                max_tokens_field: Some("max_tokens".into()),
                requires_reasoning_content_on_assistant_messages: Some(true),
                thinking_format: Some("deepseek".into()),
                ..Default::default()
            })),
            headers: None,
        },
        Model {
            id: "gpt-4o".into(), name: "GPT-4o".into(),
            api: "openai-completions".into(), provider: "opencode".into(),
            base_url: "https://opencode.ai/zen/go/v1".into(),
            reasoning: false, input: vec!["text".into()],
            cost: Cost { input: 0.0, output: 0.0, cache_read: 0.0, cache_write: 0.0 },
            context_window: 128000, max_tokens: 4096,
            compat: Some(CompatConfig::OpenAICompletions(OpenAICompletionsCompat {
                max_tokens_field: Some("max_tokens".into()),
                ..Default::default()
            })),
            headers: None,
        },
        // ── GLM (智谱) ──
        // GLM-4.x 系列原生支持 reasoning_content（思维链）。
        // 关键：max_tokens 同时约束 reasoning + content，必须给充足预算
        // 否则复杂任务时 reasoning 会吃光预算，content 为空（"no response" 问题）。
        Model {
            id: "glm-4.7".into(), name: "GLM-4.7".into(),
            api: "openai-completions".into(), provider: "zhipuai".into(),
            base_url: "https://open.bigmodel.cn/api/coding/paas/v4".into(),
            reasoning: true, input: vec!["text".into()],
            cost: Cost { input: 0.0, output: 0.0, cache_read: 0.0, cache_write: 0.0 },
            context_window: 128000, max_tokens: 32000,
            compat: Some(CompatConfig::OpenAICompletions(OpenAICompletionsCompat {
                max_tokens_field: Some("max_tokens".into()),
                requires_reasoning_content_on_assistant_messages: Some(true),
                ..Default::default()
            })),
            headers: None,
        },
        Model {
            id: "glm-4.6".into(), name: "GLM-4.6".into(),
            api: "openai-completions".into(), provider: "zhipuai".into(),
            base_url: "https://open.bigmodel.cn/api/coding/paas/v4".into(),
            reasoning: true, input: vec!["text".into()],
            cost: Cost { input: 0.0, output: 0.0, cache_read: 0.0, cache_write: 0.0 },
            context_window: 128000, max_tokens: 32000,
            compat: Some(CompatConfig::OpenAICompletions(OpenAICompletionsCompat {
                max_tokens_field: Some("max_tokens".into()),
                requires_reasoning_content_on_assistant_messages: Some(true),
                ..Default::default()
            })),
            headers: None,
        },
    ]
}

/// Stream with StreamOptions.
pub async fn stream(
    registry: &ApiRegistry,
    model: &Model,
    context: &Context,
    options: Option<&StreamOptions>,
    cancel: Option<CancellationToken>,
) -> ProviderResult<EventStream> {
    let provider = registry.get(&model.api)
        .ok_or_else(|| crate::ProviderError::ProviderNotFound(model.api.clone()))?;
    provider.stream(model, context, options, cancel).await
}

/// Non-streaming: collect all events into a final AssistantMessage.
pub async fn complete(
    registry: &ApiRegistry,
    model: &Model,
    context: &Context,
    options: Option<&StreamOptions>,
) -> ProviderResult<AssistantMessage> {
    let stream = stream(registry, model, context, options, None).await?;
    stream.result().await
}
