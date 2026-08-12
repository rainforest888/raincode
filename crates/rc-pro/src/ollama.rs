use crate::canonical::{CanonicalMessage, CanonicalRequest, CanonicalRole, ProvEvent};
use crate::provider::{ProvStream, Provider, ProviderConfig, ProviderError};
use crate::sse::{json_lines, response_bytes};
use async_trait::async_trait;
use futures::{Stream, StreamExt};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::pin::Pin;

pub struct OllamaProvider {
    cfg: ProviderConfig,
    id: String,
}

impl OllamaProvider {
    pub fn new(cfg: ProviderConfig, id: String) -> Self {
        Self { cfg, id }
    }
}

#[async_trait]
impl Provider for OllamaProvider {
    fn id(&self) -> &str {
        &self.id
    }

    async fn stream(&self, req: CanonicalRequest) -> Result<ProvStream, ProviderError> {
        let client = self.cfg.client()?;
        let base = self.cfg.base_url.trim_end_matches('/').to_string();
        let url = format!("{}/api/chat", base);
        let messages = messages_to_ollama(&req.messages);
        let mut body = json!({"model": self.cfg.model, "messages": messages, "stream": true});
        if !req.tools.is_empty() {
            body["tools"] = json!(req.tools.iter().map(|t| {
                json!({"type": "function", "function": {"name": t.name, "description": t.description, "parameters": t.input_schema}})
            }).collect::<Vec<_>>());
        }
        if let Some(t) = req.temperature {
            body["options"] = json!({"temperature": t});
        }
        let resp = client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()))?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(ProviderError::Http { status, body });
        }
        Ok(map_ollama_lines(json_lines(response_bytes(resp))))
    }

    async fn embed(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>, ProviderError> {
        let client = self.cfg.client()?;
        let base = self.cfg.base_url.trim_end_matches('/').to_string();
        let url = format!("{}/api/embed", base);
        let model = self
            .cfg
            .embedding_model
            .clone()
            .unwrap_or_else(|| self.cfg.model.clone());
        let resp = client
            .post(&url)
            .json(&json!({"model": model, "input": texts}))
            .send()
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()))?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(ProviderError::Http { status, body });
        }
        let data: Value = resp
            .json()
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()))?;
        let mut out = Vec::new();
        for item in data["embeddings"].as_array().unwrap_or(&vec![]) {
            out.push(
                item.as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_f64().map(|x| x as f32))
                            .collect()
                    })
                    .unwrap_or_default(),
            );
        }
        Ok(out)
    }
}

fn messages_to_ollama(messages: &[CanonicalMessage]) -> Vec<Value> {
    messages
        .iter()
        .map(|m| match m.role {
            CanonicalRole::System => json!({"role": "system", "content": m.text()}),
            CanonicalRole::User => json!({"role": "user", "content": m.text()}),
            CanonicalRole::Assistant => {
                // 多轮对话必须回显上一轮的工具调用(Ollama 要求 assistant 消息带
                // tool_calls 才能把 tool 结果对上),只发 content 会丢工具上下文。
                let mut msg = json!({"role": "assistant", "content": m.text()});
                if !m.tool_calls.is_empty() {
                    let calls: Vec<Value> = m
                        .tool_calls
                        .iter()
                        .map(|c| {
                            json!({
                                "type": "function",
                                "function": {
                                    "name": c.name,
                                    // arguments 必须是对象(与响应解析器/Ollama 原生
                                    // /api/chat 格式一致),不是 stringify 后的字符串。
                                    "arguments": c.arguments,
                                }
                            })
                        })
                        .collect();
                    msg["tool_calls"] = Value::Array(calls);
                }
                msg
            }
            CanonicalRole::Tool => json!({"role": "tool", "content": m.text()}),
        })
        .collect()
}

fn map_ollama_lines(
    lines: Pin<Box<dyn Stream<Item = Result<String, ProviderError>> + Send>>,
) -> ProvStream {
    let state = (lines, VecDeque::<Result<ProvEvent, ProviderError>>::new());
    Box::pin(futures::stream::unfold(
        state,
        |(mut s, mut pending)| async move {
            loop {
                if let Some(ev) = pending.pop_front() {
                    return Some((ev, (s, pending)));
                }
                match s.next().await {
                    None => return None,
                    Some(Err(e)) => return Some((Err(e), (s, pending))),
                    Some(Ok(line)) => {
                        let obj: Value = match serde_json::from_str(&line) {
                            Ok(v) => v,
                            Err(_) => continue,
                        };
                        if let Some(content) = obj["message"]["content"].as_str() {
                            if !content.is_empty() {
                                pending.push_back(Ok(ProvEvent::Delta {
                                    text: content.to_string(),
                                }));
                            }
                        }
                        if let Some(calls) = obj["message"]["tool_calls"].as_array() {
                            for call in calls {
                                let name = call["function"]["name"]
                                    .as_str()
                                    .unwrap_or_default()
                                    .to_string();
                                let args = call["function"]["arguments"].clone();
                                let id = format!("ollama-{}", name);
                                pending.push_back(Ok(ProvEvent::ToolCall {
                                    id: id.clone(),
                                    name,
                                    arguments: args,
                                }));
                                pending.push_back(Ok(ProvEvent::ToolCallEnd { id }));
                            }
                        }
                        if obj["done"].as_bool().unwrap_or(false) {
                            pending.push_back(Ok(ProvEvent::Finish {
                                stop_reason: "stop".into(),
                                usage: obj.get("prompt_eval_count").map(|_| json!({})),
                            }));
                        }
                        continue;
                    }
                }
            }
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CanonicalMessage, CanonicalToolCall};

    #[test]
    fn ollama_echo_keeps_object_arguments() {
        let msgs = vec![CanonicalMessage::assistant_tool_calls(vec![
            CanonicalToolCall {
                id: "ollama-run_shell".into(),
                name: "run_shell".into(),
                arguments: json!({"command": "ls"}),
            },
        ])];
        let out = messages_to_ollama(&msgs);
        assert_eq!(out[0]["role"], "assistant");
        let calls = out[0]["tool_calls"]
            .as_array()
            .expect("assistant message must echo tool_calls");
        assert_eq!(calls[0]["function"]["name"], "run_shell");
        // 回显的 arguments 必须是对象(与响应解析器读到的格式一致),不是字符串。
        assert!(
            calls[0]["function"]["arguments"].is_object(),
            "arguments must be an object, got {}",
            calls[0]["function"]["arguments"]
        );
        assert_eq!(calls[0]["function"]["arguments"], json!({"command": "ls"}));
    }
}
