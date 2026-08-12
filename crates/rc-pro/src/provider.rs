use crate::anthropic::AnthropicProvider;
use crate::canonical::{CanonicalRequest, ProvEvent};
use crate::mock::MockProvider;
use crate::ollama::OllamaProvider;
use crate::openai::{OpenAiChatProvider, OpenAiResponsesProvider};
use async_trait::async_trait;
use futures::Stream;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::env;
use std::pin::Pin;

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("http transport error: {0}")]
    Transport(String),
    #[error("http status {status}: {body}")]
    Http { status: u16, body: String },
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("provider does not support this operation: {0}")]
    Unsupported(String),
    #[error("config error: {0}")]
    Config(String),
}

pub type ProvStream = Pin<Box<dyn Stream<Item = Result<ProvEvent, ProviderError>> + Send>>;

#[async_trait]
pub trait Provider: Send + Sync {
    fn id(&self) -> &str;
    async fn stream(&self, req: CanonicalRequest) -> Result<ProvStream, ProviderError>;
    async fn embed(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>, ProviderError>;
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProviderConfig {
    #[serde(default = "default_kind")]
    pub kind: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embedding_model: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub extra: Value,
}

fn default_kind() -> String {
    "openai".to_string()
}

impl ProviderConfig {
    pub fn resolve_api_key(&self) -> Option<String> {
        if let Some(key) = &self.api_key {
            if !key.is_empty() {
                return Some(key.clone());
            }
        }
        self.api_key_env.as_ref().and_then(|k| env::var(k).ok())
    }

    pub fn client(&self) -> Result<reqwest::Client, ProviderError> {
        // 用 read_timeout(块间空闲)而非 timeout(整段总超时):长流式生成(agent 跑
        // 复杂任务可 >120s)不应被总超时切断;空闲 >120s 仍算挂起,安全兜底。
        // connect_timeout 单独给:连不上(TCP 黑洞/错误端口)时 OS 默认会挂几分钟,
        // 显式 15s 让它快速失败。
        let mut builder = reqwest::Client::builder()
            .read_timeout(std::time::Duration::from_secs(120))
            .connect_timeout(std::time::Duration::from_secs(15));
        let mut headers = reqwest::header::HeaderMap::new();
        for (k, v) in &self.headers {
            if let (Ok(name), Ok(value)) = (
                reqwest::header::HeaderName::from_bytes(k.as_bytes()),
                reqwest::header::HeaderValue::from_str(v),
            ) {
                headers.insert(name, value);
            }
        }
        builder = builder.default_headers(headers);
        builder
            .build()
            .map_err(|e| ProviderError::Transport(e.to_string()))
    }
}

/// Create a provider from a profile config. The `extra.api` field selects
/// `chat` (default) vs `responses` for OpenAI-compatible endpoints.
pub fn create_provider(cfg: ProviderConfig) -> Result<Box<dyn Provider>, ProviderError> {
    let id = format!("{}:{}", cfg.kind, cfg.model);
    match cfg.kind.as_str() {
        "openai" => {
            let api = cfg
                .extra
                .get("api")
                .and_then(|v| v.as_str())
                .unwrap_or("chat");
            if api == "responses" {
                Ok(Box::new(OpenAiResponsesProvider::new(cfg, id)))
            } else {
                Ok(Box::new(OpenAiChatProvider::new(cfg, id)))
            }
        }
        "openai-compat" | "openai_compat" => Ok(Box::new(OpenAiChatProvider::new(cfg, id))),
        "anthropic" => Ok(Box::new(AnthropicProvider::new(cfg, id))),
        "ollama" => Ok(Box::new(OllamaProvider::new(cfg, id))),
        "mock" => Ok(Box::new(MockProvider::new(cfg, id))),
        other => Err(ProviderError::Config(format!(
            "unknown provider kind '{other}'"
        ))),
    }
}

pub(crate) fn parse_tool_arguments(raw: &str) -> Result<Value, ProviderError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(Value::Object(Default::default()));
    }
    // Some providers return code-fenced JSON. Strip prefix and suffix
    // independently: if only one fence is present (mismatched), keep the
    // prefix-stripped string instead of reverting to the original fenced one.
    let stripped = raw
        .strip_prefix("```json")
        .or_else(|| raw.strip_prefix("```"))
        .unwrap_or(raw);
    let stripped = stripped
        .strip_suffix("```")
        .unwrap_or(stripped)
        .trim();
    serde_json::from_str(stripped).map_err(ProviderError::Json)
}
