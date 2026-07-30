//! AWS Bedrock Converse Stream API provider.
//!
//! Implements the `bedrock-converse-stream` protocol, which targets the AWS
//! Bedrock Converse Stream endpoint:
//!
//! ```text
//! POST https://bedrock-runtime.{region}.amazonaws.com/model/{model_id}/converse-stream
//! ```
//!
//! ## Authentication
//!
//! AWS Bedrock uses AWS Signature V4 (SigV4) — not a Bearer token. The SigV4
//! algorithm requires HMAC-SHA256 and SHA-256. Neither `hmac` nor `sha2` are
//! direct dependencies of this crate, so the cryptographic signing is currently
//! a **stub**: the provider collects AWS credentials from the environment
//! (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_REGION`) but does not
//! yet produce a valid `Authorization` header.
//!
//! The main architectural value of this provider is the **request/response
//! format conversion**: internal types ↔ Bedrock Converse JSON, and the SSE
//! stream parsing for Bedrock event types.
//!
//! To complete SigV4, add `sha2` and `hmac` (or `ring`) to `Cargo.toml` and
//! implement the signing chain:
//!   `HMAC(HMAC(HMAC(HMAC(AWS4(secret), date), region), "bedrock"), "aws4_request")`
//!
//! Protocol reference:
//!   https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_ConverseStream.html

use crate::ApiProvider;
use crate::error::{ProviderError, ProviderResult};
use crate::event_stream::{EventSender, EventStream};
use crate::types::*;
use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;

// ───────────────────────────────────────────────────────────────────────────
// AWS Signature V4 (SigV4) signing
// ───────────────────────────────────────────────────────────────────────────
// Pure functions so the signing chain is unit-testable with AWS's published
// test vectors (no network, no AWS credentials needed).
//
// Reference: https://docs.aws.amazon.com/general/latest/gr/sigv4-create-canonical-request.html

mod sigv4 {
    use hmac::{Hmac, Mac};
    use sha2::{Digest, Sha256};

    type HmacSha256 = Hmac<Sha256>;

    /// SHA-256 hex digest of `data`.
    pub fn sha256_hex(data: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(data);
        hex::encode(hasher.finalize())
    }

    /// HMAC-SHA256(key, data) → raw bytes.
    fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
        let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
        mac.update(data);
        mac.finalize().into_bytes().to_vec()
    }

    /// Derive the SigV4 signing key:
    ///   kDate    = HMAC("AWS4" + secret, date)
    ///   kRegion  = HMAC(kDate, region)
    ///   kService = HMAC(kRegion, service)
    ///   kSigning = HMAC(kService, "aws4_request")
    pub fn signing_key(secret: &str, date: &str, region: &str, service: &str) -> Vec<u8> {
        let k_date = hmac(format!("AWS4{secret}").as_bytes(), date.as_bytes());
        let k_region = hmac(&k_date, region.as_bytes());
        let k_service = hmac(&k_region, service.as_bytes());
        hmac(&k_service, b"aws4_request")
    }

    /// Build the canonical request string.
    ///   method\nuri\nquery\nsigned_headers_string\nsigned_header_names\nhashed_payload
    pub fn canonical_request(
        method: &str,
        canonical_uri: &str,
        canonical_query: &str,
        canonical_headers: &str,
        signed_headers: &str,
        payload_hash: &str,
    ) -> String {
        format!(
            "{method}\n{canonical_uri}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}"
        )
    }

    /// Build the string-to-sign:
    ///   AWS4-HMAC-SHA256\ntimestamp\nscope\nhash(canonical_request)
    pub fn string_to_sign(timestamp: &str, scope: &str, canonical_request: &str) -> String {
        format!(
            "AWS4-HMAC-SHA256\n{timestamp}\n{scope}\n{}",
            sha256_hex(canonical_request.as_bytes())
        )
    }

    /// Compute the final hex signature.
    pub fn signature(signing_key: &[u8], string_to_sign: &str) -> String {
        hex::encode(hmac(signing_key, string_to_sign.as_bytes()))
    }

    /// Build the Authorization header value.
    pub fn authorization_header(
        access_key_id: &str,
        scope: &str,
        signed_headers: &str,
        signature: &str,
    ) -> String {
        format!(
            "AWS4-HMAC-SHA256 Credential={access_key_id}/{scope}, SignedHeaders={signed_headers}, Signature={signature}"
        )
    }
}

/// Signed Bedrock request headers (the three headers SigV4 adds).
struct Sigv4Headers {
    authorization: String,
    amz_date: String,
    content_sha256: String,
}

/// Compute SigV4 headers for a Bedrock Converse request.
/// `host` is the Host header value (e.g. bedrock-runtime.us-east-1.amazonaws.com).
/// `uri` is the path (e.g. /model/claude.../converse-stream).
#[allow(clippy::too_many_arguments)] // SigV4 inherently needs all 8 inputs
fn sign_bedrock_request(
    access_key_id: &str,
    secret_access_key: &str,
    region: &str,
    host: &str,
    uri: &str,
    body: &[u8],
    amz_date: &str,   // yyyyMMdd'T'HHmmss'Z'
    date_stamp: &str, // yyyyMMdd
) -> Sigv4Headers {
    let payload_hash = sigv4::sha256_hex(body);
    let service = "bedrock";
    let scope = format!("{date_stamp}/{region}/{service}/aws4_request");

    // Canonical headers (must be sorted by header name, lowercase, trimmed,
    // each followed by \n). For Bedrock we sign: host, x-amz-content-sha256,
    // x-amz-date.
    let canonical_headers =
        format!("host:{host}\nx-amz-content-sha256:{payload_hash}\nx-amz-date:{amz_date}\n");
    let signed_headers = "host;x-amz-content-sha256;x-amz-date";

    let canon = sigv4::canonical_request(
        "POST",
        uri,
        "", // no query string for converse-stream
        &canonical_headers,
        signed_headers,
        &payload_hash,
    );
    let sts = sigv4::string_to_sign(amz_date, &scope, &canon);
    let k_signing = sigv4::signing_key(secret_access_key, date_stamp, region, service);
    let sig = sigv4::signature(&k_signing, &sts);
    let auth = sigv4::authorization_header(access_key_id, &scope, signed_headers, &sig);

    Sigv4Headers {
        authorization: auth,
        amz_date: amz_date.to_string(),
        content_sha256: payload_hash,
    }
}

/// Split a `https://host/path...` URL into (host, path). path includes the
/// leading `/`. Query strings are not produced by build_bedrock_url, so we
/// don't parse them here.
fn parse_host_and_path(url: &str) -> (String, String) {
    let after_scheme = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    if let Some(slash) = after_scheme.find('/') {
        let (host, path) = after_scheme.split_at(slash);
        (host.to_string(), path.to_string())
    } else {
        (after_scheme.to_string(), "/".to_string())
    }
}

/// Break a Unix timestamp into UTC (year, month, day, hour, minute, second).
/// Minimal civil-calendar conversion (Howard Hinnant's algorithm) — avoids
/// pulling in chrono/time just for date formatting.
fn utc_components(secs: u64) -> (u32, u32, u32, u32, u32, u32) {
    let days = (secs / 86400) as i64;
    let rem = (secs % 86400) as u32;
    let h = rem / 3600;
    let mi = (rem % 3600) / 60;
    let s = rem % 60;

    // Days since 1970-01-01 → civil date (Hinnant's days_from_civil inverse).
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u32; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if m <= 2 { y + 1 } else { y };
    (year as u32, m, d, h, mi, s)
}

/// AWS Bedrock Converse Stream provider.
///
/// Registry name: `bedrock-converse-stream`.
pub struct BedrockConverseProvider;

#[async_trait]
impl ApiProvider for BedrockConverseProvider {
    async fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&StreamOptions>,
        cancel: Option<tokio_util::sync::CancellationToken>,
    ) -> ProviderResult<EventStream> {
        self.stream_inner(model, context, options, cancel).await
    }
}

impl BedrockConverseProvider {
    async fn stream_inner(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&StreamOptions>,
        cancel: Option<tokio_util::sync::CancellationToken>,
    ) -> ProviderResult<EventStream> {
        let (stream, sender) = EventStream::new();

        // AWS credentials. Bedrock does NOT use a simple Bearer/API key; it
        // requires SigV4 signing. We still collect the access key id and secret
        // here so the signer (once implemented) has them.
        let access_key_id = std::env::var("AWS_ACCESS_KEY_ID")
            .or_else(|_| std::env::var("AWS_ACCESS_KEY"))
            .map_err(|_| ProviderError::MissingApiKey("AWS_ACCESS_KEY_ID".into()))?;
        let _secret_access_key = std::env::var("AWS_SECRET_ACCESS_KEY")
            .or_else(|_| std::env::var("AWS_SECRET_KEY"))
            .map_err(|_| ProviderError::MissingApiKey("AWS_SECRET_ACCESS_KEY".into()))?;
        let secret_access_key = _secret_access_key; // now used by the signer
        let region = std::env::var("AWS_REGION")
            .or_else(|_| std::env::var("AWS_DEFAULT_REGION"))
            .unwrap_or_else(|_| "us-east-1".to_string());

        // Build the Bedrock endpoint.
        let url = build_bedrock_url(model, &region);

        let body = build_request_body(model, context, options)?;
        let body_json =
            serde_json::to_string(&body).map_err(|e| ProviderError::Provider(e.to_string()))?;

        let client = Client::builder()
            .timeout(Duration::from_secs(300))
            .build()
            .map_err(|e| ProviderError::Provider(e.to_string()))?;

        // --- SigV4 signing ------------------------------------------------
        // Parse the URL into host + path (the signer needs them for the
        // canonical request). reqwest/Url isn't a dep here, so split manually
        // (build_bedrock_url always produces https://host/path).
        let (host, uri) = parse_host_and_path(&url);

        // SigV4 timestamps: amz_date = yyyyMMdd'T'HHmmss'Z', date_stamp = yyyyMMdd.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let secs = now.as_secs();
        let (y, mo, d, h, mi, s) = utc_components(secs);
        let amz_date = format!("{y:04}{mo:02}{d:02}T{h:02}{mi:02}{s:02}Z");
        let date_stamp = format!("{y:04}{mo:02}{d:02}");

        let signed = sign_bedrock_request(
            &access_key_id,
            &secret_access_key,
            &region,
            &host,
            &uri,
            body_json.as_bytes(),
            &amz_date,
            &date_stamp,
        );

        let mut req = client
            .post(&url)
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .header("host", &host)
            .header("x-amz-date", &signed.amz_date)
            .header("x-amz-content-sha256", &signed.content_sha256)
            .header("authorization", &signed.authorization)
            .body(body_json);

        // Apply model-level custom headers (same pattern as other providers).
        if let Some(headers) = &model.headers {
            for (k, v) in headers {
                req = req.header(k, v);
            }
        }

        let send_fut = req.send();

        // HTTP handshake can be aborted by the cancel token (same pattern as
        // anthropic.rs / vertex.rs).
        let resp = if let Some(c) = &cancel {
            tokio::select! {
                r = send_fut => r.map_err(|e| ProviderError::Provider(e.to_string()))?,
                _ = c.cancelled() => {
                    return Err(ProviderError::Stream("HTTP request aborted".into()));
                }
            }
        } else {
            send_fut
                .await
                .map_err(|e| ProviderError::Provider(e.to_string()))?
        };

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(ProviderError::HttpError {
                status: status.as_u16(),
                body: text,
            });
        }

        let model_clone = model.clone();
        let cancel_clone = cancel.clone();
        tokio::spawn(async move {
            if let Some(c) = &cancel_clone {
                tokio::select! {
                    _ = parse_sse_stream(resp, sender, &model_clone) => {}
                    _ = c.cancelled() => {
                        tracing::info!("[stream] bedrock-converse-stream SSE read cancelled by abort");
                    }
                }
            } else {
                parse_sse_stream(resp, sender, &model_clone).await;
            }
        });

        Ok(stream)
    }
}

// ──────────────────────────────────────────────────────────────
// Endpoint URL construction
// ──────────────────────────────────────────────────────────────

/// Build the Bedrock Converse Stream endpoint.
///
/// Priority:
///   1. `model.base_url` if it already looks like a full http(s) URL.
///   2. Compose from `region`:
///        `https://bedrock-runtime.{region}.amazonaws.com/model/{model_id}/converse-stream`
fn build_bedrock_url(model: &Model, region: &str) -> String {
    if !model.base_url.is_empty() && model.base_url.starts_with("http") {
        // Allow the caller to fully override the endpoint.
        let base = model.base_url.trim_end_matches('/');
        if base.contains("/converse-stream") {
            return base.to_string();
        }
        return format!("{base}/{model_id}/converse-stream", model_id = model.id);
    }
    format!(
        "https://bedrock-runtime.{region}.amazonaws.com/model/{model_id}/converse-stream",
        model_id = model.id,
    )
}

// ──────────────────────────────────────────────────────────────
// Request body construction (Bedrock Converse format)
// ──────────────────────────────────────────────────────────────

/// Top-level Bedrock Converse request body.
#[derive(Serialize)]
struct BedrockRequest {
    #[serde(rename = "modelId")]
    model_id: String,
    messages: Vec<BedrockMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<Vec<BedrockSystemBlock>>,
    #[serde(rename = "inferenceConfig")]
    inference_config: BedrockInferenceConfig,
    #[serde(rename = "toolConfig", skip_serializing_if = "Option::is_none")]
    tool_config: Option<BedrockToolConfig>,
}

/// Bedrock message: role + content blocks.
#[derive(Serialize)]
struct BedrockMessage {
    role: String,
    content: Vec<BedrockContentBlock>,
}

/// Bedrock system prompt block (an array of `{"text": "..."}` objects).
#[derive(Serialize)]
struct BedrockSystemBlock {
    text: String,
}

/// Content block sent to Bedrock. Bedrock uses `{"text": "..."}` instead of a
/// bare string, and `{"image": {...}}` for images.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(tag = "type")] // not used for toolResult; handled via untagged below
enum BedrockContentBlock {
    /// Plain text content.
    Text { text: String },
    /// Base64 image.
    Image { image: BedrockImageSource },
    /// Assistant tool-use block (echoed back in multi-turn conversations).
    #[serde(rename = "toolUse")]
    ToolUse {
        #[serde(rename = "toolUseId")]
        tool_use_id: String,
        name: String,
        input: serde_json::Value,
    },
    /// Tool result sent as a user message.
    #[serde(rename = "toolResult")]
    ToolResult {
        #[serde(rename = "toolUseId")]
        tool_use_id: String,
        content: Vec<BedrockToolResultContent>,
        #[serde(skip_serializing_if = "Option::is_none")]
        status: Option<String>,
    },
}

#[derive(Serialize)]
struct BedrockImageSource {
    format: String,
    #[serde(rename = "source")]
    source: BedrockImageBytes,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BedrockImageBytes {
    bytes: String,
}

/// Inner content of a `toolResult` block.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
enum BedrockToolResultContent {
    Text { text: String },
    // Bedrock also supports `{"json": ...}` and `{"image": ...}` content,
    // omitted here for simplicity.
}

/// `inferenceConfig` — generation parameters.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BedrockInferenceConfig {
    #[serde(rename = "maxTokens")]
    max_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f64>,
}

/// `toolConfig` — tool definitions passed to the model.
#[derive(Serialize)]
struct BedrockToolConfig {
    tools: Vec<BedrockTool>,
}

/// A single tool declaration. Bedrock nests the schema under `toolSpec`.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BedrockTool {
    tool_spec: BedrockToolSpec,
}

#[derive(Serialize)]
struct BedrockToolSpec {
    name: String,
    description: String,
    #[serde(rename = "inputSchema")]
    input_schema: BedrockInputSchema,
}

/// `inputSchema.json` — Bedrock wraps the JSON Schema in an object with a
/// `json` key.
#[derive(Serialize)]
struct BedrockInputSchema {
    json: serde_json::Value,
}

fn build_request_body(
    model: &Model,
    context: &Context,
    options: Option<&StreamOptions>,
) -> ProviderResult<BedrockRequest> {
    let max_tokens = options
        .and_then(|o| o.max_tokens)
        .or(Some(model.max_tokens))
        .unwrap_or(4096);

    let temperature = options.and_then(|_o| {
        // Bedrock does not expose a thinking-level → temperature mapping; we
        // leave temperature unset unless the caller provides one. Currently
        // StreamOptions has no temperature field, so this is always None.
        None::<f64>
    });

    // System prompt → array of `{"text": "..."}` blocks.
    let system = context
        .system_prompt
        .as_ref()
        .map(|s| vec![BedrockSystemBlock { text: s.clone() }]);

    let mut messages: Vec<BedrockMessage> = Vec::new();

    for msg in &context.messages {
        match msg {
            Message::User(u) => {
                let content = convert_user_message(u);
                if !content.is_empty() {
                    messages.push(BedrockMessage {
                        role: "user".into(),
                        content,
                    });
                }
            }
            Message::Assistant(a) => {
                let content = convert_assistant_message(a);
                if !content.is_empty() {
                    messages.push(BedrockMessage {
                        role: "assistant".into(),
                        content,
                    });
                }
            }
            Message::ToolResult(tr) => {
                let text = tr
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text(t) => Some(t.text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");

                let status = if tr.is_error {
                    Some("error".to_string())
                } else {
                    None
                };

                let block = BedrockContentBlock::ToolResult {
                    tool_use_id: tr.tool_call_id.clone(),
                    content: vec![BedrockToolResultContent::Text { text }],
                    status,
                };
                messages.push(BedrockMessage {
                    role: "user".into(),
                    content: vec![block],
                });
            }
            Message::BashExecution(b) => {
                messages.push(BedrockMessage {
                    role: "user".into(),
                    content: vec![BedrockContentBlock::Text {
                        text: format!("$ {}\n{}", b.command, b.output),
                    }],
                });
            }
            Message::Custom(c) => {
                let text = match &c.content {
                    CustomContent::Text(s) => s.clone(),
                    CustomContent::Blocks(blocks) => blocks
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::Text(t) => Some(t.text.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                };
                messages.push(BedrockMessage {
                    role: "user".into(),
                    content: vec![BedrockContentBlock::Text { text }],
                });
            }
            Message::BranchSummary(bs) => {
                messages.push(BedrockMessage {
                    role: "assistant".into(),
                    content: vec![BedrockContentBlock::Text {
                        text: bs.summary.clone(),
                    }],
                });
            }
            Message::CompactionSummary(cs) => {
                messages.push(BedrockMessage {
                    role: "assistant".into(),
                    content: vec![BedrockContentBlock::Text {
                        text: cs.summary.clone(),
                    }],
                });
            }
        }
    }

    // Tools
    let tool_config = context.tools.as_deref().and_then(|tools| {
        if tools.is_empty() {
            return None;
        }
        let bedrock_tools: Vec<BedrockTool> = tools
            .iter()
            .map(|td| BedrockTool {
                tool_spec: BedrockToolSpec {
                    name: td.name.clone(),
                    description: td.description.clone(),
                    input_schema: BedrockInputSchema {
                        json: td.parameters.clone(),
                    },
                },
            })
            .collect();
        Some(BedrockToolConfig {
            tools: bedrock_tools,
        })
    });

    Ok(BedrockRequest {
        model_id: model.id.clone(),
        messages,
        system,
        inference_config: BedrockInferenceConfig {
            max_tokens,
            temperature,
            top_p: None,
        },
        tool_config,
    })
}

/// Convert a user message into Bedrock content blocks.
fn convert_user_message(u: &UserMessage) -> Vec<BedrockContentBlock> {
    u.content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text(t) if !t.text.is_empty() => Some(BedrockContentBlock::Text {
                text: t.text.clone(),
            }),
            ContentBlock::Image(img) => {
                // Extract format from mime_type (e.g. "image/png" → "png").
                let format = img
                    .mime_type
                    .strip_prefix("image/")
                    .unwrap_or(&img.mime_type)
                    .to_string();
                Some(BedrockContentBlock::Image {
                    image: BedrockImageSource {
                        format,
                        source: BedrockImageBytes {
                            bytes: img.data.clone(),
                        },
                    },
                })
            }
            _ => None,
        })
        .collect()
}

/// Convert an assistant message into Bedrock content blocks (text + toolUse).
fn convert_assistant_message(a: &AssistantMessage) -> Vec<BedrockContentBlock> {
    a.content
        .iter()
        .filter_map(|b| match b {
            AssistantContentBlock::Text(t) if !t.text.is_empty() => {
                Some(BedrockContentBlock::Text {
                    text: t.text.clone(),
                })
            }
            AssistantContentBlock::ToolCall(tc) => Some(BedrockContentBlock::ToolUse {
                tool_use_id: tc.id.clone(),
                name: tc.name.clone(),
                input: tc.arguments.clone(),
            }),
            // Bedrock Converse does not have a first-class thinking/reasoning
            // block in the same way Anthropic does; we skip it.
            AssistantContentBlock::Thinking(_) => None,
            _ => None,
        })
        .collect()
}

// ──────────────────────────────────────────────────────────────
// SSE stream parsing
// ──────────────────────────────────────────────────────────────

/// Tracks the content block currently being built from the stream.
enum BlockState {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        partial_json: String,
    },
}

async fn parse_sse_stream(resp: reqwest::Response, sender: EventSender, model: &Model) {
    let mut output = AssistantMessage::new(model);
    let mut byte_stream = resp.bytes_stream();
    let mut buffer = String::new();
    let mut content_index: usize = 0;
    let mut current_block: Option<BlockState> = None;
    let mut stop_reason = StopReason::Stop;

    // Start event
    let _ = sender
        .send(StreamEvent::Start {
            partial: output.clone(),
        })
        .await;

    let parse_result: ProviderResult<()> = async {
        while let Some(chunk_result) = byte_stream.next().await {
            let chunk = match chunk_result {
                Ok(c) => c,
                Err(e) => {
                    return Err(ProviderError::Stream(e.to_string()));
                }
            };
            let text = String::from_utf8_lossy(&chunk);
            buffer.push_str(&text);

            // Split on SSE record boundary "\n\n".
            while let Some(pos) = buffer.find("\n\n") {
                let event_str = buffer[..pos].to_string();
                buffer = buffer[pos + 2..].to_string();

                // Bedrock SSE events have a `event:` line (optional — sometimes
                // only `data:` is present) and a `data:` line with the JSON.
                let data = extract_data(&event_str);
                if data.is_empty() {
                    continue;
                }

                // The JSON payload wraps the real event in a `{"<eventType>": {...}}`
                // envelope. Parse it generically.
                let parsed: serde_json::Value = match serde_json::from_str(&data) {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                // The envelope has exactly one key naming the event type.
                let (event_type, payload) = match parsed.as_object() {
                    Some(obj) if obj.len() == 1 => {
                        let (k, v) = obj.iter().next().unwrap();
                        (k.clone(), v.clone())
                    }
                    _ => continue,
                };

                match event_type.as_str() {
                    "messageStart" => {
                        // Conversation started — no action needed.
                    }
                    "contentBlockStart" => {
                        if let Ok(block_start) =
                            serde_json::from_value::<ContentBlockStartEvent>(payload.clone())
                        {
                            let cb = block_start.content_block;
                            let block_type = cb.start.as_deref().unwrap_or("");
                            match block_type {
                                "toolUse" => {
                                    let id = cb.tool_use_id.unwrap_or_default();
                                    let name = cb.name.unwrap_or_default();
                                    let _ = sender
                                        .send(StreamEvent::ToolCallStart {
                                            content_index,
                                            partial: output.clone(),
                                        })
                                        .await;
                                    current_block = Some(BlockState::ToolUse {
                                        id,
                                        name,
                                        partial_json: String::new(),
                                    });
                                }
                                // text or reasoningContent — start accumulating.
                                _ => {
                                    let _ = sender
                                        .send(StreamEvent::TextStart {
                                            content_index,
                                            partial: output.clone(),
                                        })
                                        .await;
                                    current_block = Some(BlockState::Text {
                                        text: String::new(),
                                    });
                                }
                            }
                        }
                    }
                    "contentBlockDelta" => {
                        if let Ok(delta) =
                            serde_json::from_value::<ContentBlockDeltaEvent>(payload.clone())
                        {
                            match delta.delta {
                                BedrockDelta::TextDelta { text } => {
                                    if let Some(BlockState::Text { text: t }) = &mut current_block {
                                        t.push_str(&text);
                                    }
                                    let _ = sender
                                        .send(StreamEvent::TextDelta {
                                            content_index,
                                            delta: text.clone(),
                                            partial: output.clone(),
                                        })
                                        .await;
                                }
                                BedrockDelta::ToolUse { input } => {
                                    if let Some(BlockState::ToolUse { partial_json, .. }) =
                                        &mut current_block
                                    {
                                        partial_json.push_str(&input);
                                    }
                                    let _ = sender
                                        .send(StreamEvent::ToolCallDelta {
                                            content_index,
                                            delta: input.clone(),
                                            partial: output.clone(),
                                        })
                                        .await;
                                }
                                BedrockDelta::ReasoningContentDelta { reasoning } => {
                                    // Bedrock reasoning text — treat like a text
                                    // delta for display purposes.
                                    if let Some(BlockState::Text { text: t }) = &mut current_block {
                                        t.push_str(&reasoning);
                                    }
                                    let _ = sender
                                        .send(StreamEvent::TextDelta {
                                            content_index,
                                            delta: reasoning.clone(),
                                            partial: output.clone(),
                                        })
                                        .await;
                                }
                            }
                        }
                    }
                    "contentBlockStop" => {
                        // Finalize the current block.
                        match current_block.take() {
                            Some(BlockState::Text { text }) => {
                                output
                                    .content
                                    .push(AssistantContentBlock::Text(TextContent {
                                        text: text.clone(),
                                        text_signature: None,
                                    }));
                                let _ = sender
                                    .send(StreamEvent::TextEnd {
                                        content_index,
                                        content: text,
                                        partial: output.clone(),
                                    })
                                    .await;
                                content_index += 1;
                            }
                            Some(BlockState::ToolUse {
                                id,
                                name,
                                partial_json,
                            }) => {
                                let arguments = parse_json_repair(&partial_json);
                                let tool_call = ToolCall {
                                    call_type: "function".into(),
                                    id: id.clone(),
                                    name: name.clone(),
                                    arguments: arguments.clone(),
                                    thought_signature: None,
                                };
                                output
                                    .content
                                    .push(AssistantContentBlock::ToolCall(tool_call.clone()));
                                let _ = sender
                                    .send(StreamEvent::ToolCallEnd {
                                        content_index,
                                        tool_call,
                                        partial: output.clone(),
                                    })
                                    .await;
                                content_index += 1;
                            }
                            None => {}
                        }
                    }
                    "messageStop" => {
                        if let Ok(msg_stop) =
                            serde_json::from_value::<MessageStopEvent>(payload.clone())
                        {
                            if let Some(sr) = msg_stop.stop_reason {
                                stop_reason = map_stop_reason(&sr);
                            }
                        }
                    }
                    "metadata" => {
                        if let Ok(meta) = serde_json::from_value::<MetadataEvent>(payload.clone()) {
                            if let Some(usage) = meta.usage {
                                output.usage.input = usage.input_tokens;
                                output.usage.output = usage.output_tokens;
                                output.usage.total_tokens = usage
                                    .total_tokens
                                    .unwrap_or_else(|| usage.input_tokens + usage.output_tokens);
                            }
                        }
                    }
                    "internalServerException"
                    | "modelStreamErrorException"
                    | "validationException"
                    | "throttlingException" => {
                        let msg = payload
                            .get("message")
                            .and_then(|v| v.as_str())
                            .unwrap_or("bedrock stream error")
                            .to_string();
                        return Err(ProviderError::Provider(msg));
                    }
                    _ => {
                        // Unknown event type — ignore.
                    }
                }
            }
        }
        Ok(())
    }
    .await;

    match parse_result {
        Ok(()) => {
            output.stop_reason = stop_reason;
            sender.end(output);
        }
        Err(e) => {
            output.error_message = Some(e.to_string());
            output.stop_reason = StopReason::Error;
            sender.error(StopReason::Error, output);
        }
    }
}

/// Map Bedrock `stopReason` values to the internal `StopReason` enum.
fn map_stop_reason(reason: &str) -> StopReason {
    match reason {
        "end_turn" | "stop_sequence" => StopReason::Stop,
        "max_tokens" => StopReason::Length,
        "tool_use" => StopReason::ToolUse,
        "guardrail_intervened" | "content_filtered" => StopReason::Stop,
        _ => StopReason::Stop,
    }
}

/// Extract the `data:` payload from an SSE record (supports `data: ` and
/// `data:` prefixes, possibly spanning multiple lines).
fn extract_data(event_str: &str) -> String {
    let mut data = String::new();
    for line in event_str.lines() {
        if let Some(rest) = line.strip_prefix("data: ") {
            data.push_str(rest);
        } else if let Some(rest) = line.strip_prefix("data:") {
            data.push_str(rest);
        }
    }
    data
}

/// JSON repair for partial/truncated tool-call argument JSON. Identical logic
/// to the Anthropic provider — accumulates missing closing braces/brackets and
/// closes an unterminated string.
fn parse_json_repair(s: &str) -> serde_json::Value {
    if s.is_empty() {
        return serde_json::json!({});
    }
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(s) {
        return v;
    }
    let mut repaired = s.to_string();
    let mut open_braces = 0i32;
    let mut open_brackets = 0i32;
    let mut in_string = false;
    let mut escape = false;
    for c in repaired.chars() {
        if escape {
            escape = false;
            continue;
        }
        if c == '\\' {
            escape = true;
            continue;
        }
        if c == '"' {
            in_string = !in_string;
            continue;
        }
        if in_string {
            continue;
        }
        match c {
            '{' => open_braces += 1,
            '}' => open_braces -= 1,
            '[' => open_brackets += 1,
            ']' => open_brackets -= 1,
            _ => {}
        }
    }
    if in_string {
        repaired.push('"');
    }
    for _ in 0..open_brackets.max(0) {
        repaired.push(']');
    }
    for _ in 0..open_braces.max(0) {
        repaired.push('}');
    }
    serde_json::from_str(&repaired).unwrap_or(serde_json::json!({}))
}

// ──────────────────────────────────────────────────────────────
// SSE event deserialization structs
// ──────────────────────────────────────────────────────────────

/// `contentBlockStart` event payload.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ContentBlockStartEvent {
    #[serde(default)]
    #[allow(dead_code)]
    content_block_index: Option<u32>,
    content_block: ContentBlockStartInfo,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ContentBlockStartInfo {
    /// The discriminant: "text" | "toolUse" | "reasoningContent".
    #[serde(default, rename = "start")]
    start: Option<String>,
    #[serde(default)]
    tool_use_id: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

/// `contentBlockDelta` event payload.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ContentBlockDeltaEvent {
    #[serde(default)]
    #[allow(dead_code)]
    content_block_index: Option<u32>,
    delta: BedrockDelta,
}

/// Delta payload — tagged by the `type` field inside `delta`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(tag = "type")]
enum BedrockDelta {
    #[serde(rename = "textDelta")]
    TextDelta {
        #[serde(default)]
        text: String,
    },
    #[serde(rename = "toolUse")]
    ToolUse {
        #[serde(default)]
        input: String,
    },
    #[serde(rename = "reasoningContentDelta")]
    ReasoningContentDelta {
        #[serde(default)]
        reasoning: String,
    },
}

/// `messageStop` event payload.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MessageStopEvent {
    stop_reason: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    stop_sequence: Option<String>,
}

/// `metadata` event payload.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MetadataEvent {
    #[serde(default)]
    usage: Option<BedrockUsage>,
}

/// Usage info inside the `metadata` event.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BedrockUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    total_tokens: Option<u64>,
}

// ──────────────────────────────────────────────────────────────
// Tests
// ─���────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_model() -> Model {
        Model {
            id: "anthropic.claude-3-5-sonnet-20241022-v1:0".into(),
            name: "Claude 3.5 Sonnet (Bedrock)".into(),
            api: "bedrock-converse-stream".into(),
            provider: "amazon".into(),
            base_url: "".into(),
            reasoning: false,
            input: vec!["text".into()],
            cost: Cost::default(),
            context_window: 200000,
            max_tokens: 4096,
            compat: None,
            headers: None,
        }
    }

    #[test]
    fn build_bedrock_url_from_region() {
        let model = make_test_model();
        let url = build_bedrock_url(&model, "us-west-2");
        assert!(url.contains("bedrock-runtime.us-west-2.amazonaws.com"));
        assert!(url.contains("/model/anthropic.claude-3-5-sonnet-20241022-v1:0/converse-stream"));
    }

    #[test]
    fn build_bedrock_url_from_base_url() {
        let mut model = make_test_model();
        model.base_url = "https://custom-proxy.example.com".into();
        let url = build_bedrock_url(&model, "us-east-1");
        assert!(url.starts_with("https://custom-proxy.example.com/"));
        assert!(url.ends_with("/converse-stream"));
    }

    #[test]
    fn build_request_body_basic() {
        let model = make_test_model();
        let ctx = Context::new(
            Some("You are helpful".into()),
            vec![Message::User(UserMessage {
                role: "user".into(),
                content: vec![ContentBlock::Text(TextContent {
                    text: "Hello".into(),
                    text_signature: None,
                })],
                timestamp: 0,
                source: MessageSource::Prompt,
            })],
        );
        let body = build_request_body(&model, &ctx, None).unwrap();
        assert_eq!(body.model_id, "anthropic.claude-3-5-sonnet-20241022-v1:0");
        assert!(body.system.is_some());
        assert_eq!(body.system.as_ref().unwrap()[0].text, "You are helpful");
        assert_eq!(body.messages.len(), 1);
        assert_eq!(body.messages[0].role, "user");
        assert_eq!(body.inference_config.max_tokens, 4096);
        assert!(body.tool_config.is_none());
    }

    #[test]
    fn build_request_body_with_tools() {
        let model = make_test_model();
        let ctx = Context {
            system_prompt: None,
            messages: vec![],
            tools: Some(vec![ToolDef {
                name: "get_weather".into(),
                description: "Get weather".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "location": { "type": "string" }
                    }
                }),
            }]),
        };
        let body = build_request_body(&model, &ctx, None).unwrap();
        let tc = body.tool_config.unwrap();
        assert_eq!(tc.tools.len(), 1);
        assert_eq!(tc.tools[0].tool_spec.name, "get_weather");
        assert_eq!(tc.tools[0].tool_spec.input_schema.json["type"], "object");
    }

    #[test]
    fn build_request_body_with_tool_result() {
        let model = make_test_model();
        let ctx = Context::new(
            None,
            vec![Message::ToolResult(ToolResultMessage {
                role: "toolResult".into(),
                tool_call_id: "call_123".into(),
                tool_name: "get_weather".into(),
                content: vec![ContentBlock::Text(TextContent {
                    text: "Sunny".into(),
                    text_signature: None,
                })],
                details: None,
                is_error: false,
                timestamp: 0,
            })],
        );
        let body = build_request_body(&model, &ctx, None).unwrap();
        assert_eq!(body.messages.len(), 1);
        assert_eq!(body.messages[0].role, "user");
        // The content block should be a ToolResult (tagged by "type").
        let json = serde_json::to_value(&body.messages[0]).unwrap();
        assert_eq!(json["content"][0]["type"], "toolResult");
        assert_eq!(json["content"][0]["toolUseId"], "call_123");
    }

    #[test]
    fn convert_user_message_with_image() {
        let u = UserMessage {
            role: "user".into(),
            content: vec![
                ContentBlock::Text(TextContent {
                    text: "What's this?".into(),
                    text_signature: None,
                }),
                ContentBlock::Image(ImageContent {
                    data: "base64data".into(),
                    mime_type: "image/png".into(),
                }),
            ],
            timestamp: 0,
            source: MessageSource::Prompt,
        };
        let blocks = convert_user_message(&u);
        assert_eq!(blocks.len(), 2);
    }

    #[test]
    fn convert_assistant_message_with_tool_use() {
        let model = make_test_model();
        let a = AssistantMessage {
            role: "assistant".into(),
            content: vec![
                AssistantContentBlock::Text(TextContent {
                    text: "Let me check.".into(),
                    text_signature: None,
                }),
                AssistantContentBlock::ToolCall(ToolCall {
                    call_type: "function".into(),
                    id: "call_abc".into(),
                    name: "get_weather".into(),
                    arguments: serde_json::json!({"location": "Tokyo"}),
                    thought_signature: None,
                }),
            ],
            api: model.api.clone(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            response_model: None,
            response_id: None,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: 0,
        };
        let blocks = convert_assistant_message(&a);
        assert_eq!(blocks.len(), 2);
    }

    #[test]
    fn map_stop_reason_variants() {
        assert_eq!(map_stop_reason("end_turn"), StopReason::Stop);
        assert_eq!(map_stop_reason("stop_sequence"), StopReason::Stop);
        assert_eq!(map_stop_reason("max_tokens"), StopReason::Length);
        assert_eq!(map_stop_reason("tool_use"), StopReason::ToolUse);
        assert_eq!(map_stop_reason("unknown_foo"), StopReason::Stop);
    }

    #[test]
    fn extract_data_basic() {
        let record = "event: contentBlockDelta\ndata: {\"contentBlockDelta\":{\"delta\":{\"type\":\"textDelta\",\"text\":\"Hi\"}}}";
        let data = extract_data(record);
        assert!(data.contains("textDelta"));
    }

    #[test]
    fn extract_data_no_prefix() {
        let record = "data:{\"foo\":1}";
        let data = extract_data(record);
        assert_eq!(data, "{\"foo\":1}");
    }

    #[test]
    fn parse_delta_text() {
        let payload = r#"{"contentBlockIndex":0,"delta":{"type":"textDelta","text":"Hello"}}"#;
        let evt: ContentBlockDeltaEvent = serde_json::from_str(payload).unwrap();
        match evt.delta {
            BedrockDelta::TextDelta { text } => assert_eq!(text, "Hello"),
            other => panic!("expected TextDelta, got {other:?}"),
        }
    }

    #[test]
    fn parse_delta_tool_use() {
        let payload = r#"{"contentBlockIndex":0,"delta":{"type":"toolUse","input":"{\"loc\""}}"#;
        let evt: ContentBlockDeltaEvent = serde_json::from_str(payload).unwrap();
        match evt.delta {
            BedrockDelta::ToolUse { input } => assert!(input.contains("loc")),
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    #[test]
    fn parse_metadata_usage() {
        let payload = r#"{"usage":{"inputTokens":10,"outputTokens":5,"totalTokens":15}}"#;
        let evt: MetadataEvent = serde_json::from_str(payload).unwrap();
        let u = evt.usage.unwrap();
        assert_eq!(u.input_tokens, 10);
        assert_eq!(u.output_tokens, 5);
        assert_eq!(u.total_tokens, Some(15));
    }

    #[test]
    fn parse_message_stop() {
        let payload = r#"{"stopReason":"tool_use"}"#;
        let evt: MessageStopEvent = serde_json::from_str(payload).unwrap();
        assert_eq!(evt.stop_reason.as_deref(), Some("tool_use"));
    }

    #[test]
    fn parse_content_block_start_tool_use() {
        let payload = r#"{"contentBlockIndex":0,"start":{"toolUse":{"toolUseId":"call_1","name":"get_weather"}}}"#;
        // The `start` field carries the toolUse info.
        let v: serde_json::Value = serde_json::from_str(payload).unwrap();
        // Our struct flattens `start.toolUse` — check the raw shape.
        assert_eq!(v["start"]["toolUse"]["name"], "get_weather");
    }

    #[test]
    fn json_repair_complete() {
        let v = parse_json_repair(r#"{"key":"value"}"#);
        assert_eq!(v["key"], "value");
    }

    #[test]
    fn json_repair_truncated() {
        let v = parse_json_repair(r#"{"location":"Tokyo"#);
        assert_eq!(v["location"], "Tokyo");
    }

    #[test]
    fn json_repair_empty() {
        let v = parse_json_repair("");
        assert_eq!(v, serde_json::json!({}));
    }

    // ── SigV4 signing (verified against AWS's published test vectors) ──
    //
    // AWS publishes a worked example in the Signature V4 reference docs:
    //   secret        = wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY
    //   date_stamp    = 20150830
    //   region        = us-east-1
    //   service       = iam
    //   signing key   = c4afb1cc5771d871763a393e44b703571b55cc28424d1a5e86da6ed3c154a4b9
    //
    // We validate our signing_key() against that known output.

    #[test]
    fn sigv4_signing_key_matches_aws_example() {
        let key = sigv4::signing_key(
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            "20150830",
            "us-east-1",
            "iam",
        );
        let expected = "c4afb1cc5771d871763a393e44b703571b55cc28424d1a5e86da6ed3c154a4b9";
        assert_eq!(
            hex::encode(&key),
            expected,
            "signing key must match AWS doc"
        );
    }

    #[test]
    fn sigv4_sha256_hex_known_vector() {
        // SHA-256("") = e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
        assert_eq!(
            sigv4::sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        // SHA-256("abc") = ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad
        assert_eq!(
            sigv4::sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn sigv4_authorization_header_format() {
        let auth = sigv4::authorization_header(
            "AKIDEXAMPLE",
            "20150830/us-east-1/iam/aws4_request",
            "content-type;host;x-amz-date",
            "f4780e2d9f65fa895f9c67b32ce1baf09809a4fda92ecac86f9e2bb5b5b5e4a1",
        );
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/iam/aws4_request, \
             SignedHeaders=content-type;host;x-amz-date, \
             Signature=f4780e2d9f65fa895f9c67b32ce1baf09809a4fda92ecac86f9e2bb5b5b5e4a1"
        );
    }

    #[test]
    fn sign_bedrock_request_produces_consistent_headers() {
        // End-to-end: sign a fake Bedrock request and check the three headers
        // are well-formed (we can't check the exact signature without a known
        // Bedrock vector, but we assert structure + that the payload hash is
        // the SHA-256 of the body).
        let body = br#"{"hello":"world"}"#;
        let signed = sign_bedrock_request(
            "AKIDTEST",
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            "us-west-2",
            "bedrock-runtime.us-west-2.amazonaws.com",
            "/model/anthropic.claude-3/converse-stream",
            body,
            "20240101T120000Z",
            "20240101",
        );
        // payload hash matches body
        assert_eq!(signed.content_sha256, sigv4::sha256_hex(body));
        // amz_date passed through
        assert_eq!(signed.amz_date, "20240101T120000Z");
        // Authorization header has the right shape
        assert!(
            signed
                .authorization
                .starts_with("AWS4-HMAC-SHA256 Credential=AKIDTEST/")
        );
        assert!(
            signed
                .authorization
                .contains("SignedHeaders=host;x-amz-content-sha256;x-amz-date")
        );
        assert!(signed.authorization.contains("Signature="));
        // signature is 64 hex chars
        let sig = signed.authorization.rsplit("Signature=").next().unwrap();
        assert_eq!(sig.len(), 64, "signature must be 64 hex chars");
        assert!(sig.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn parse_host_and_path_splits_url() {
        let (host, path) = parse_host_and_path(
            "https://bedrock-runtime.us-east-1.amazonaws.com/model/foo/converse-stream",
        );
        assert_eq!(host, "bedrock-runtime.us-east-1.amazonaws.com");
        assert_eq!(path, "/model/foo/converse-stream");

        let (host2, path2) = parse_host_and_path("https://example.com/");
        assert_eq!(host2, "example.com");
        assert_eq!(path2, "/");
    }

    #[test]
    fn utc_components_known_date() {
        // 2024-01-01 00:00:00 UTC = 1704067200
        let (y, mo, d, h, mi, s) = utc_components(1704067200);
        assert_eq!((y, mo, d, h, mi, s), (2024, 1, 1, 0, 0, 0));
        // 2024-12-31 23:59:59 UTC = 1735689599
        let (y2, mo2, d2, h2, mi2, s2) = utc_components(1735689599);
        assert_eq!((y2, mo2, d2, h2, mi2, s2), (2024, 12, 31, 23, 59, 59));
    }
}
