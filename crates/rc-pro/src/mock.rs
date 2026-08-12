//! Scripted provider used by tests, demos and evals. The script lives in
//! `ProviderConfig.extra.script` as an array of step objects.
use crate::canonical::{CanonicalRequest, ProvEvent};
use crate::provider::{ProvStream, Provider, ProviderConfig, ProviderError};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};

pub struct MockProvider {
    cfg: ProviderConfig,
    id: String,
    turn: AtomicUsize,
}

impl MockProvider {
    pub fn new(cfg: ProviderConfig, id: String) -> Self {
        Self {
            cfg,
            id,
            turn: AtomicUsize::new(0),
        }
    }

    /// 已调用 `stream` 的次数(测试用:断言演化循环的 keep 分支不调用模型)。
    pub fn calls(&self) -> usize {
        self.turn.load(Ordering::Relaxed)
    }

    fn auto_advance(&self) -> bool {
        self.cfg
            .extra
            .get("auto_advance")
            .and_then(|v| v.as_bool())
            .unwrap_or(true)
    }

    fn script_sequence(&self) -> Vec<Vec<Value>> {
        self.cfg
            .extra
            .get("script_sequence")
            .and_then(|v| v.as_array())
            .map(|steps| {
                steps
                    .iter()
                    .filter_map(|step| step.as_array().cloned())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn script(&self) -> Vec<Value> {
        self.cfg
            .extra
            .get("script")
            .and_then(|s| s.as_array().cloned())
            .unwrap_or_else(|| {
                vec![
                    json!({"type": "text", "text": "I will inspect the repository first."}),
                    json!({"type": "tool", "name": "list_dir", "arguments": {"path": "."}}),
                    json!({"type": "text", "text": "The repository is understood. Summary complete."}),
                    json!({"type": "done", "stop_reason": "end_turn"}),
                ]
            })
    }
}

#[async_trait]
impl Provider for MockProvider {
    fn id(&self) -> &str {
        &self.id
    }

    async fn stream(&self, _req: CanonicalRequest) -> Result<ProvStream, ProviderError> {
        let turn = self.turn.fetch_add(1, Ordering::Relaxed);
        let mut events = Vec::new();
        let mut n = 0u64;
        let sequence = self.script_sequence();
        let steps = if self.auto_advance() && turn > 0 {
            vec![
                json!({"type": "text", "text": "Tool results processed; wrapping up."}),
                json!({"type": "done", "stop_reason": "end_turn"}),
            ]
        } else if !sequence.is_empty() {
            sequence
                .get(turn.min(sequence.len().saturating_sub(1)))
                .cloned()
                .unwrap_or_else(|| self.script())
        } else {
            self.script()
        };
        for step in steps {
            match step["type"].as_str().unwrap_or("") {
                "text" => {
                    let text = step["text"].as_str().unwrap_or_default();
                    events.push(Ok(ProvEvent::Delta {
                        text: text.to_string(),
                    }));
                }
                "think" => {
                    let text = step["text"].as_str().unwrap_or_default();
                    events.push(Ok(ProvEvent::Thinking {
                        text: text.to_string(),
                    }));
                }
                "tool" => {
                    n += 1;
                    let id = format!("mock_call_{n}");
                    let name = step["name"].as_str().unwrap_or("noop").to_string();
                    let arguments = step
                        .get("arguments")
                        .cloned()
                        .unwrap_or(Value::Object(Default::default()));
                    events.push(Ok(ProvEvent::ToolCall {
                        id: id.clone(),
                        name,
                        arguments,
                    }));
                    events.push(Ok(ProvEvent::ToolCallEnd { id }));
                }
                "sleep" => {
                    let ms = step["ms"].as_u64().unwrap_or(100);
                    tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                }
                "done" => {
                    let reason = step["stop_reason"]
                        .as_str()
                        .unwrap_or("end_turn")
                        .to_string();
                    // Optional `usage` object (e.g. {"total_tokens": N} or
                    // {"input_tokens": N, "output_tokens": M}) lets scripted
                    // runs exercise rc-core's context accumulation. Defaults
                    // to None, matching real providers that omit usage.
                    let usage = step.get("usage").cloned();
                    events.push(Ok(ProvEvent::Finish {
                        stop_reason: reason,
                        usage,
                    }));
                }
                // 让脚本化 mock 也能模拟 provider 失败(供失败路径 smoke/演示)。
                "error" => {
                    let message = step["message"]
                        .as_str()
                        .unwrap_or("mock provider error")
                        .to_string();
                    events.push(Ok(ProvEvent::Error { message }));
                }
                _ => {}
            }
        }
        let iter = futures::stream::iter(events);
        Ok(Box::pin(iter))
    }

    async fn embed(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>, ProviderError> {
        // Deterministic pseudo embeddings so router tests are stable offline.
        Ok(texts.iter().map(|t| hash_embedding(t)).collect())
    }
}

pub fn hash_embedding(text: &str) -> Vec<f32> {
    let mut seed: u64 = 0x9E3779B97F4A7C15;
    for b in text.bytes() {
        seed = seed.rotate_left(5) ^ (seed ^ u64::from(b)).wrapping_mul(0x2545F4914F6CDD1D);
    }
    let mut out = Vec::with_capacity(32);
    let mut x = seed;
    for _ in 0..32 {
        x = x
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        out.push(((x >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canonical::CanonicalMessage;
    use futures::StreamExt;

    #[tokio::test]
    async fn mock_streams_script_in_order() {
        let cfg = ProviderConfig {
            kind: "mock".into(),
            base_url: String::new(),
            model: "mock-1".into(),
            api_key: None,
            api_key_env: None,
            embedding_model: None,
            headers: Default::default(),
            extra: json!({
                "script": [
                    {"type": "text", "text": "hi"},
                    {"type": "tool", "name": "list_dir", "arguments": {"path": "."}},
                    {"type": "done", "stop_reason": "end_turn"}
                ]
            }),
        };
        let p = MockProvider::new(cfg, "mock-1".into());
        let mut stream = p
            .stream(CanonicalRequest {
                model: "mock-1".into(),
                messages: vec![CanonicalMessage::user("hello")],
                tools: vec![],
                temperature: None,
                max_tokens: None,
                stream: true,
                extra: json!({}),
            })
            .await
            .unwrap();
        let first = stream.next().await.unwrap().unwrap();
        assert!(matches!(first, ProvEvent::Delta { .. }));
        let tool = stream.next().await.unwrap().unwrap();
        assert!(matches!(tool, ProvEvent::ToolCall { name, .. } if name == "list_dir"));
        let tool_end = stream.next().await.unwrap().unwrap();
        assert!(matches!(tool_end, ProvEvent::ToolCallEnd { .. }));
        let done = stream.next().await.unwrap().unwrap();
        assert!(matches!(done, ProvEvent::Finish { .. }));
    }

    #[test]
    fn embeddings_are_deterministic() {
        assert_eq!(hash_embedding("same"), hash_embedding("same"));
        assert_ne!(hash_embedding("a"), hash_embedding("b"));
    }
}
