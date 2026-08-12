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

/// provider HTTP 错误是否值得重试:传输层抖动(连接失败/超时)或 429/5xx
/// (上游暂时不可用/限流)。其余 4xx 是确定性错误,重试无意义。
pub fn is_retryable_provider_error(err: &ProviderError) -> bool {
    match err {
        ProviderError::Transport(_) => true,
        ProviderError::Http { status, .. } => *status == 429 || (500..=599).contains(status),
        _ => false,
    }
}

/// 指数退避重试"建立 HTTP 请求"阶段。只包 send + 状态检查——stream 返回后
/// 由调用方消费,不再重试(避免重复消费/重放流)。最多 1+3 次尝试,间隔
/// 500ms / 1.5s / 3s。非可重试错误立即返回。
pub async fn retry_provider_request<T, Fut, F>(mut f: F) -> Result<T, ProviderError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, ProviderError>>,
{
    const DELAYS_MS: [u64; 3] = [500, 1500, 3000];
    let mut attempt = 0usize;
    loop {
        match f().await {
            Ok(v) => return Ok(v),
            Err(e) if is_retryable_provider_error(&e) && attempt < DELAYS_MS.len() => {
                tokio::time::sleep(std::time::Duration::from_millis(DELAYS_MS[attempt])).await;
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retryable_error_classification() {
        assert!(is_retryable_provider_error(&ProviderError::Transport("conn reset".into())));
        assert!(is_retryable_provider_error(&ProviderError::Http { status: 429, body: "rate".into() }));
        assert!(is_retryable_provider_error(&ProviderError::Http { status: 503, body: "unavail".into() }));
        assert!(is_retryable_provider_error(&ProviderError::Http { status: 500, body: "boom".into() }));
        assert!(!is_retryable_provider_error(&ProviderError::Http { status: 400, body: "bad req".into() }));
        assert!(!is_retryable_provider_error(&ProviderError::Http { status: 401, body: "auth".into() }));
        assert!(!is_retryable_provider_error(&ProviderError::Json(
            serde_json::from_str::<serde_json::Value>("x").unwrap_err()
        )));
        assert!(!is_retryable_provider_error(&ProviderError::Unsupported("x".into())));
        assert!(!is_retryable_provider_error(&ProviderError::Config("x".into())));
    }

    #[tokio::test]
    async fn retry_succeeds_on_first_attempt() {
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let out = retry_provider_request(|| {
            let n = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async move { Ok::<_, ProviderError>(n) }
        })
        .await
        .unwrap();
        assert_eq!(out, 0);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn retry_transient_503_then_succeeds() {
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let out = retry_provider_request(|| {
            let n = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async move {
                if n == 0 {
                    Err(ProviderError::Http { status: 503, body: "upstream down".into() })
                } else {
                    Ok::<_, ProviderError>("recovered".to_string())
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(out, "recovered");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn retry_non_retryable_returns_immediately() {
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let err = retry_provider_request(|| {
            let n = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async move {
                let _ = n;
                Err::<(), _>(ProviderError::Http { status: 401, body: "auth".into() })
            }
        })
        .await
        .unwrap_err();
        assert!(matches!(err, ProviderError::Http { status: 401, .. }));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn retry_exhausts_after_3_retries() {
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let err = retry_provider_request(|| {
            let n = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async move {
                let _ = n;
                Err::<(), _>(ProviderError::Http { status: 503, body: "still down".into() })
            }
        })
        .await
        .unwrap_err();
        assert!(matches!(err, ProviderError::Http { status: 503, .. }));
        // 1 初始 + 3 次重试 = 4 次调用;总延迟 500+1500+3000 = 5s。
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 4);
    }
}
