use crate::canonical::{CanonicalMessage, CanonicalRequest, CanonicalRole, ProvEvent, ToolDef};
use crate::provider::{parse_tool_arguments, ProvStream, Provider, ProviderConfig, ProviderError};
use crate::sse::{response_bytes, sse_events};
use async_trait::async_trait;
use futures::{Stream, StreamExt};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::pin::Pin;

fn base(cfg: &ProviderConfig) -> String {
    cfg.base_url.trim_end_matches('/').to_string()
}

pub(crate) fn messages_to_openai(messages: &[CanonicalMessage]) -> Vec<Value> {
    messages.iter().map(|m| match m.role {
        CanonicalRole::System => json!({"role": "system", "content": m.text()}),
        CanonicalRole::User => json!({"role": "user", "content": m.text()}),
        CanonicalRole::Assistant => {
            if m.tool_calls.is_empty() {
                json!({"role": "assistant", "content": m.text()})
            } else {
                let calls: Vec<Value> = m
                    .tool_calls
                    .iter()
                    .map(|c| {
                        json!({
                            "id": c.id,
                            "type": "function",
                            "function": {
                                "name": c.name,
                                "arguments": serde_json::to_string(&c.arguments).unwrap_or_else(|_| "{}".into()),
                            }
                        })
                    })
                    .collect();
                json!({"role": "assistant", "content": m.text(), "tool_calls": calls})
            }
        }
        CanonicalRole::Tool => json!({
            "role": "tool",
            "tool_call_id": m.tool_call_id.clone().unwrap_or_default(),
            "content": m.text(),
        }),
    }).collect()
}

pub(crate) fn tools_to_openai(tools: &[ToolDef]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.input_schema,
                }
            })
        })
        .collect()
}

/// 从 `extra.session_id` 取缓存键(字符串);缺省返回 None → 不发射
/// prompt_cache_key,旧请求(无 session 语义)请求体保持不变。
fn session_id(req: &CanonicalRequest) -> Option<String> {
    req.extra
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// 构建 OpenAI Chat Completions(OpenRouter)请求体。OpenRouter 支持顶层
/// `prompt_cache_key` 复用跨请求 prompt 缓存。抽取为纯函数以便单测。
/// `wire_model` = `cfg.model`(不是 `req.model`,后者是 `kind:model` 复合 id)。
pub(crate) fn build_chat_body(req: &CanonicalRequest, wire_model: &str) -> Value {
    let mut body = json!({
        "model": wire_model,
        "messages": messages_to_openai(&req.messages),
        "stream": true,
    });
    if !req.tools.is_empty() {
        body["tools"] = json!(tools_to_openai(&req.tools));
    }
    if let Some(t) = req.temperature {
        body["temperature"] = t.into();
    }
    if let Some(m) = req.max_tokens {
        body["max_tokens"] = m.into();
    }
    if let Some(key) = session_id(req) {
        body["prompt_cache_key"] = key.into();
    }
    body
}

/// 构建 OpenAI Responses 请求体。`prompt_cache_key` 位于 `options` 下。
/// 抽取为纯函数以便单测。`wire_model` = `cfg.model`。
pub(crate) fn build_responses_body(req: &CanonicalRequest, wire_model: &str) -> Value {
    let input: Vec<Value> = req
        .messages
        .iter()
        .filter(|m| m.role != CanonicalRole::System)
        .map(|m| match m.role {
            CanonicalRole::User => json!({"role": "user", "content": [{"type": "input_text", "text": m.text()}]}),
            CanonicalRole::Assistant => json!({"role": "assistant", "content": [{"type": "output_text", "text": m.text()}]}),
            CanonicalRole::Tool => json!({
                "type": "function_call_output",
                "call_id": m.tool_call_id.clone().unwrap_or_default(),
                "output": m.text(),
            }),
            CanonicalRole::System => unreachable!(),
        })
        .collect();
    let mut body = json!({"model": wire_model, "input": input, "stream": true});
    if !req.tools.is_empty() {
        body["tools"] = json!(tools_to_openai(&req.tools));
    }
    if let Some(key) = session_id(req) {
        body["options"] = json!({"prompt_cache_key": key});
    }
    body
}

/// 单个工具调用累积参数的上限:超出即丢弃后续分片,防止畸形/恶意 provider
/// 无限流参数把堆撑爆(只影响该次解析,不影响其他工具调用)。
const MAX_TOOL_ARGS_BYTES: usize = 128 * 1024;

struct ToolAccum {
    id: String,
    name: String,
    args: String,
}

pub struct OpenAiChatProvider {
    cfg: ProviderConfig,
    id: String,
}

impl OpenAiChatProvider {
    pub fn new(cfg: ProviderConfig, id: String) -> Self {
        Self { cfg, id }
    }
}

#[async_trait]
impl Provider for OpenAiChatProvider {
    fn id(&self) -> &str {
        &self.id
    }

    async fn stream(&self, req: CanonicalRequest) -> Result<ProvStream, ProviderError> {
        let client = self.cfg.client()?;
        let url = format!("{}/chat/completions", base(&self.cfg));
        let body = build_chat_body(&req, &self.cfg.model);
        let mut builder = client.post(&url).json(&body);
        if let Some(key) = self.cfg.resolve_api_key() {
            builder = builder.bearer_auth(key);
        }
        let resp = builder
            .send()
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()))?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(ProviderError::Http { status, body });
        }
        Ok(map_chat_sse(sse_events(response_bytes(resp))))
    }

    async fn embed(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>, ProviderError> {
        let client = self.cfg.client()?;
        let model = self
            .cfg
            .embedding_model
            .clone()
            .unwrap_or_else(|| self.cfg.model.clone());
        let url = format!("{}/embeddings", base(&self.cfg));
        let mut builder = client
            .post(&url)
            .json(&json!({"model": model, "input": texts}));
        if let Some(key) = self.cfg.resolve_api_key() {
            builder = builder.bearer_auth(key);
        }
        let resp = builder
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
        for item in data["data"].as_array().unwrap_or(&vec![]) {
            let vec: Vec<f32> = item["embedding"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_f64().map(|x| x as f32))
                        .collect()
                })
                .unwrap_or_default();
            out.push(vec);
        }
        out.sort_by_key(|_| 0usize); // preserve provider order; data is already ordered
        Ok(out)
    }
}

fn map_chat_sse(
    sse: Pin<Box<dyn Stream<Item = Result<String, ProviderError>> + Send>>,
) -> ProvStream {
    // 第 4 个元素 = 单调递增的下标:部分 OpenAI 兼容端点不发 index。带 id/name 的
    // 分片是新调用起点,用自增下标避免多个调用合并;纯参数延续分片追加到最近的
    // 累积器(见下),否则每个分片各占一个累积器导致参数被丢。
    let state = (
        sse,
        BTreeMap::<usize, ToolAccum>::new(),
        std::collections::VecDeque::<Result<ProvEvent, ProviderError>>::new(),
        0usize,
    );
    Box::pin(futures::stream::unfold(
        state,
        |(mut s, mut accs, mut pending, mut next_idx)| async move {
            loop {
                if let Some(ev) = pending.pop_front() {
                    return Some((ev, (s, accs, pending, next_idx)));
                }
                match s.next().await {
                    None => {
                        // 流提前结束(无 finish_reason 收尾):把已累积的工具调用发出,
                        // 否则参数分片已到却缺收尾事件,调用被静默丢弃(rc-core 的
                        // ToolCallEnd 兜底也无济于事,因为没有 ToolCall 被发出)。
                        for acc in accs.values() {
                            if !acc.name.is_empty() {
                                let (id, name) = (acc.id.clone(), acc.name.clone());
                                let arguments = parse_tool_arguments(&acc.args)
                                    .unwrap_or(Value::Object(Default::default()));
                                pending.push_back(Ok(ProvEvent::ToolCall { id, name, arguments }));
                                pending.push_back(Ok(ProvEvent::ToolCallEnd { id: acc.id.clone() }));
                            }
                        }
                        accs.clear();
                        if pending.is_empty() {
                            return None;
                        }
                        if let Some(ev) = pending.pop_front() {
                            return Some((ev, (s, accs, pending, next_idx)));
                        }
                        return None;
                    }
                    Some(Err(e)) => return Some((Err(e), (s, accs, pending, next_idx))),
                    Some(Ok(line)) => {
                        let chunk: Value = match serde_json::from_str(&line) {
                            Ok(v) => v,
                            Err(_) => continue,
                        };
                        let choices = chunk["choices"].as_array().cloned().unwrap_or_default();
                        for choice in &choices {
                            let delta = &choice["delta"];
                            if let Some(text) = delta["content"].as_str() {
                                if !text.is_empty() {
                                    pending.push_back(Ok(ProvEvent::Delta {
                                        text: text.to_string(),
                                    }));
                                }
                            }
                            if let Some(text) = delta["reasoning_content"].as_str() {
                                if !text.is_empty() {
                                    pending.push_back(Ok(ProvEvent::Thinking {
                                        text: text.to_string(),
                                    }));
                                }
                            }
                            if let Some(tool_calls) = delta["tool_calls"].as_array() {
                                for tc in tool_calls {
                                    let fnv = &tc["function"];
                                    // 带 id/name 的分片是某次调用的起点(含 index 时以
                                    // provider 的显式 index 为准)。
                                    let has_start = tc["id"]
                                        .as_str()
                                        .map(|s| !s.is_empty())
                                        .unwrap_or(false)
                                        || fnv["name"]
                                            .as_str()
                                            .map(|s| !s.is_empty())
                                            .unwrap_or(false);
                                    let idx =
                                        tc["index"].as_u64().map(|i| i as usize).unwrap_or_else(|| {
                                            if has_start {
                                                // 新调用起点且无 index:用自增下标,避免
                                                // 多个调用合并进同一个累积器。
                                                let i = next_idx;
                                                next_idx += 1;
                                                i
                                            } else {
                                                // 纯参数延续分片(无 index 且无 id/name):
                                                // 追加到最近的活动累积器。修复前每个分片
                                                // 各拿一个新下标、各占一个累积器,参数被丢
                                                // (run_shell: missing 'command' 的根因)。
                                                accs.keys()
                                                    .next_back()
                                                    .copied()
                                                    .unwrap_or_else(|| {
                                                        let i = next_idx;
                                                        next_idx += 1;
                                                        i
                                                    })
                                            }
                                        });
                                    let acc = accs.entry(idx).or_insert_with(|| ToolAccum {
                                        id: tc["id"].as_str().unwrap_or_default().to_string(),
                                        name: fnv["name"].as_str().unwrap_or_default().to_string(),
                                        args: String::new(),
                                    });
                                    if let Some(id) = tc["id"].as_str() {
                                        if !id.is_empty() {
                                            acc.id = id.to_string();
                                        }
                                    }
                                    if let Some(name) = fnv["name"].as_str() {
                                        if !name.is_empty() {
                                            acc.name = name.to_string();
                                        }
                                    }
                                    if let Some(args) = fnv["arguments"].as_str() {
                                        if !args.is_empty()
                                            && acc.args.len() < MAX_TOOL_ARGS_BYTES
                                        {
                                            acc.args.push_str(args);
                                        }
                                    }
                                }
                            }
                            if let Some(reason) = choice["finish_reason"].as_str() {
                                if !reason.is_empty() {
                                    // 工具调用在 finish_reason 才一次性发出:参数是流式分片
                                    // 到达的,必须累积完整后再 parse,否则每个分片都会发一个
                                    // 空参数的 ToolCall(run_shell: missing 'command' 的根因)。
                                    for acc in accs.values() {
                                        if !acc.name.is_empty() {
                                            let (id, name) = (acc.id.clone(), acc.name.clone());
                                            let arguments = parse_tool_arguments(&acc.args)
                                                .unwrap_or(Value::Object(Default::default()));
                                            pending.push_back(Ok(ProvEvent::ToolCall {
                                                id,
                                                name,
                                                arguments,
                                            }));
                                            pending.push_back(Ok(ProvEvent::ToolCallEnd {
                                                id: acc.id.clone(),
                                            }));
                                        }
                                    }
                                    accs.clear();
                                    let usage = chunk.get("usage").cloned();
                                    pending.push_back(Ok(ProvEvent::Finish {
                                        stop_reason: reason.to_string(),
                                        usage,
                                    }));
                                }
                            }
                        }
                        continue;
                    }
                }
            }
        },
    ))
}
pub struct OpenAiResponsesProvider {
    cfg: ProviderConfig,
    id: String,
}

impl OpenAiResponsesProvider {
    pub fn new(cfg: ProviderConfig, id: String) -> Self {
        Self { cfg, id }
    }
}

#[async_trait]
impl Provider for OpenAiResponsesProvider {
    fn id(&self) -> &str {
        &self.id
    }

    async fn stream(&self, req: CanonicalRequest) -> Result<ProvStream, ProviderError> {
        let client = self.cfg.client()?;
        let url = format!("{}/responses", base(&self.cfg));
        let body = build_responses_body(&req, &self.cfg.model);
        let mut builder = client.post(&url).json(&body);
        if let Some(key) = self.cfg.resolve_api_key() {
            builder = builder.bearer_auth(key);
        }
        let resp = builder
            .send()
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()))?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(ProviderError::Http { status, body });
        }
        Ok(map_responses_sse(sse_events(response_bytes(resp))))
    }

    async fn embed(&self, _texts: Vec<String>) -> Result<Vec<Vec<f32>>, ProviderError> {
        Err(ProviderError::Unsupported(
            "responses provider has no embed method".into(),
        ))
    }
}

fn map_responses_sse(
    sse: Pin<Box<dyn Stream<Item = Result<String, ProviderError>> + Send>>,
) -> ProvStream {
    let state = (
        sse,
        BTreeMap::<String, (String, String)>::new(),
        std::collections::VecDeque::<Result<ProvEvent, ProviderError>>::new(),
    );
    Box::pin(futures::stream::unfold(
        state,
        |(mut s, mut calls, mut pending)| async move {
            loop {
                if let Some(ev) = pending.pop_front() {
                    return Some((ev, (s, calls, pending)));
                }
                match s.next().await {
                    None => {
                        // 流提前结束(output_item.done 未到达):把仍挂着的函数调用
                        // 一次性发出,避免参数已分片到达却因缺收尾事件而静默丢弃。
                        let leftover: Vec<(String, String, String)> = calls
                            .iter()
                            .map(|(id, (name, args))| {
                                (id.clone(), name.clone(), args.clone())
                            })
                            .collect();
                        calls.clear();
                        for (id, name, args) in leftover {
                            if !name.is_empty() {
                                let arguments = parse_tool_arguments(&args).unwrap_or_default();
                                pending.push_back(Ok(ProvEvent::ToolCall {
                                    id: id.clone(),
                                    name,
                                    arguments,
                                }));
                                pending.push_back(Ok(ProvEvent::ToolCallEnd { id }));
                            }
                        }
                        if pending.is_empty() {
                            return None;
                        }
                        if let Some(ev) = pending.pop_front() {
                            return Some((ev, (s, calls, pending)));
                        }
                        return None;
                    }
                    Some(Err(e)) => return Some((Err(e), (s, calls, pending))),
                    Some(Ok(line)) => {
                        let ev: Value = match serde_json::from_str(&line) {
                            Ok(v) => v,
                            Err(_) => continue,
                        };
                        match ev["type"].as_str().unwrap_or("") {
                            "response.output_text.delta" => {
                                let text = ev["delta"].as_str().unwrap_or_default();
                                if !text.is_empty() {
                                    pending.push_back(Ok(ProvEvent::Delta {
                                        text: text.to_string(),
                                    }));
                                }
                            }
                            "response.output_item.added" => {
                                if ev["item"]["type"].as_str() == Some("function_call") {
                                    let item = &ev["item"];
                                    let id = item["id"].as_str().unwrap_or_default().to_string();
                                    let name =
                                        item["name"].as_str().unwrap_or_default().to_string();
                                    // output_item.added 的 arguments 通常为空,真实参数经
                                    // function_call_arguments.delta 分片到达(见下)。
                                    let args =
                                        item["arguments"].as_str().unwrap_or_default().to_string();
                                    calls.insert(id.clone(), (name.clone(), args.clone()));
                                    // 先不发 ToolCall:等 output_item.done 收尾(带完整参数)。
                                    // 空参数提前发会让 rc-core 的 pending 覆盖机制丢失最终参数。
                                }
                            }
                            "response.function_call_arguments.delta" => {
                                // 参数分片:item_id 与 output_item.added 的 item.id 一致。
                                let item_id = ev["item_id"].as_str().unwrap_or_default();
                                let delta = ev["delta"].as_str().unwrap_or_default();
                                if !item_id.is_empty() && !delta.is_empty() {
                                    if let Some((_, args)) = calls.get_mut(item_id) {
                                        if args.len() < MAX_TOOL_ARGS_BYTES {
                                            args.push_str(delta);
                                        }
                                    }
                                }
                            }
                            "response.output_item.done" => {
                                if ev["item"]["type"].as_str() == Some("function_call") {
                                    let item = &ev["item"];
                                    let id = item["id"].as_str().unwrap_or_default().to_string();
                                    if let Some((name, mut args)) = calls.remove(&id) {
                                        // 部分实现把完整参数直接放在 done 的 item.arguments
                                        // (不流 delta);此时累积分片可能为空,取 item 的为准。
                                        let final_args =
                                            item["arguments"].as_str().unwrap_or_default();
                                        if !final_args.is_empty() && args.is_empty() {
                                            args = final_args.to_string();
                                        }
                                        let arguments =
                                            parse_tool_arguments(&args).unwrap_or_default();
                                        pending.push_back(Ok(ProvEvent::ToolCall {
                                            id: id.clone(),
                                            name,
                                            arguments,
                                        }));
                                        pending.push_back(Ok(ProvEvent::ToolCallEnd { id }));
                                    } else {
                                        // output_item.added 未见过(代理丢事件/流从中途
                                        // 开始):done 自带完整 name+arguments,直接从 done
                                        // 项发出,不再静默丢弃整个调用。
                                        let name = item["name"]
                                            .as_str()
                                            .unwrap_or_default()
                                            .to_string();
                                        if !name.is_empty() {
                                            let args = item["arguments"]
                                                .as_str()
                                                .unwrap_or_default()
                                                .to_string();
                                            let arguments =
                                                parse_tool_arguments(&args).unwrap_or_default();
                                            pending.push_back(Ok(ProvEvent::ToolCall {
                                                id: id.clone(),
                                                name,
                                                arguments,
                                            }));
                                            pending.push_back(Ok(ProvEvent::ToolCallEnd { id }));
                                        }
                                    }
                                }
                            }
                            "response.completed" => {
                                let usage = ev["response"]["usage"].clone();
                                pending.push_back(Ok(ProvEvent::Finish {
                                    stop_reason: "end_turn".into(),
                                    usage: Some(usage),
                                }));
                            }
                            _ => {}
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
    use crate::CanonicalToolCall;

    #[test]
    fn canonical_messages_map_to_openai_chat() {
        let msgs = vec![
            CanonicalMessage::system("be nice"),
            CanonicalMessage::user("hello"),
            CanonicalMessage::assistant_tool_calls(vec![CanonicalToolCall {
                id: "call_1".into(),
                name: "read_file".into(),
                arguments: json!({"path": "a.txt"}),
            }]),
            CanonicalMessage::tool("call_1", "read_file", "contents"),
        ];
        let out = messages_to_openai(&msgs);
        assert_eq!(out[0]["role"], "system");
        assert_eq!(out[2]["tool_calls"][0]["function"]["name"], "read_file");
        assert_eq!(out[3]["role"], "tool");
    }

    fn chat_request(session: &str) -> CanonicalRequest {
        CanonicalRequest {
            model: "deepseek".into(),
            messages: vec![
                CanonicalMessage::system("sys"),
                CanonicalMessage::user("hello"),
            ],
            tools: vec![],
            temperature: None,
            max_tokens: None,
            stream: true,
            extra: json!({"session_id": session}),
        }
    }

    #[test]
    fn chat_body_carries_prompt_cache_key_from_session_id() {
        let body = build_chat_body(&chat_request("sess-abc"), "wire-deepseek");
        assert_eq!(body["prompt_cache_key"], "sess-abc");
        // 会话变化 → key 变;同会话 → 稳定。
        assert_ne!(
            body["prompt_cache_key"],
            build_chat_body(&chat_request("sess-xyz"), "wire-deepseek")["prompt_cache_key"]
        );
        // 无 session_id → 不发射 key(不破坏旧请求)。
        let mut req = chat_request("s");
        req.extra = json!({});
        assert!(build_chat_body(&req, "wire-deepseek")
            .get("prompt_cache_key")
            .is_none());
        // wire model 用 cfg.model,不是 req.model。
        assert_eq!(body["model"], "wire-deepseek");
    }

    #[test]
    fn responses_body_carries_prompt_cache_key_in_options() {
        let body = build_responses_body(&chat_request("sess-abc"), "wire-deepseek");
        assert_eq!(body["options"]["prompt_cache_key"], "sess-abc");
        // 无 session_id → 不发射 options.prompt_cache_key(不破坏旧请求)。
        let mut req = chat_request("s");
        req.extra = json!({});
        let body2 = build_responses_body(&req, "wire-deepseek");
        assert!(body2.get("options").is_none());
        // wire model 用 cfg.model,不是 req.model。
        assert_eq!(body["model"], "wire-deepseek");
    }

    #[tokio::test]
    async fn map_chat_sse_accumulates_fragmented_tool_args() {
        use futures::StreamExt;
        use std::pin::Pin;
        // 模拟 DeepSeek 流式 tool_call:arguments 分两片到达,finish_reason 收尾。
        let lines = vec![
            Ok::<_, ProviderError>(r#"{"choices":[{"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_1","function":{"name":"run_shell","arguments":"{\"com"}}]},"index":0}]}"#.to_string()),
            Ok(r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"mand\":\"ls\"}"}}]},"index":0}]}"#.to_string()),
            Ok(r#"{"choices":[{"delta":{},"index":0,"finish_reason":"tool_calls"}]}"#.to_string()),
        ];
        let sse: Pin<Box<dyn futures::Stream<Item = Result<String, ProviderError>> + Send>> =
            Box::pin(futures::stream::iter(lines));
        let mut stream = map_chat_sse(sse);
        let mut calls = Vec::new();
        while let Some(ev) = stream.next().await {
            if let Ok(ProvEvent::ToolCall { name, arguments, .. }) = ev {
                calls.push((name, arguments));
            }
        }
        // 只发一个 ToolCall,参数是完整 JSON(修复前:每个分片各发一个空参数调用)。
        assert_eq!(calls.len(), 1, "must emit exactly one ToolCall, got {calls:?}");
        assert_eq!(calls[0].0, "run_shell");
        assert_eq!(calls[0].1, json!({"command": "ls"}));
    }

    #[tokio::test]
    async fn map_responses_sse_accumulates_delta_args_and_emits_once() {
        use futures::StreamExt;
        use std::pin::Pin;
        // Responses API:output_item.added(空参)→ function_call_arguments.delta 分片
        // → output_item.done 收尾。修复前:added 即发空参数 ToolCall,delta 被丢弃。
        let lines = vec![
            Ok::<_, ProviderError>(
                r#"{"type":"response.output_item.added","item":{"type":"function_call","id":"fc_1","name":"run_shell","arguments":""}}"#.to_string(),
            ),
            Ok(r#"{"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"{\"command\":"}"#.to_string()),
            Ok(r#"{"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"\"ls\"}"}"#.to_string()),
            Ok(r#"{"type":"response.output_item.done","item":{"type":"function_call","id":"fc_1","name":"run_shell","arguments":""}}"#.to_string()),
            Ok(r#"{"type":"response.completed","response":{"usage":{"total_tokens":5}}}"#.to_string()),
        ];
        let sse: Pin<Box<dyn futures::Stream<Item = Result<String, ProviderError>> + Send>> =
            Box::pin(futures::stream::iter(lines));
        let mut stream = map_responses_sse(sse);
        let mut calls = Vec::new();
        let mut finished = false;
        while let Some(ev) = stream.next().await {
            match ev {
                Ok(ProvEvent::ToolCall { name, arguments, .. }) => {
                    calls.push((name, arguments));
                }
                Ok(ProvEvent::Finish { .. }) => finished = true,
                _ => {}
            }
        }
        // 只发一个 ToolCall(done 收尾),参数由 delta 分片拼成。
        assert_eq!(calls.len(), 1, "must emit exactly one ToolCall, got {calls:?}");
        assert_eq!(calls[0].0, "run_shell");
        assert_eq!(calls[0].1, json!({"command": "ls"}));
        assert!(finished, "stream must terminate with Finish");
    }

    #[tokio::test]
    async fn map_chat_sse_no_index_fragments_append_to_last_accumulator() {
        use futures::StreamExt;
        use std::pin::Pin;
        // 兼容端点完全不带 index:首片带 id/name 开新调用,后续纯参数分片
        // (无 index 且无 id/name)必须追加到同一累积器。修复前每个分片各拿
        // 一个新下标、各占一个累积器,只发出首片的空参调用 → run_shell 回归。
        let lines = vec![
            Ok::<_, ProviderError>(
                r#"{"choices":[{"delta":{"role":"assistant","tool_calls":[{"id":"call_1","function":{"name":"run_shell","arguments":"{\"com"}}]},"index":0}]}"#.to_string(),
            ),
            Ok(r#"{"choices":[{"delta":{"tool_calls":[{"function":{"arguments":"mand\":\"ls\"}"}}]},"index":0}]}"#.to_string()),
            Ok(r#"{"choices":[{"delta":{},"index":0,"finish_reason":"tool_calls"}]}"#.to_string()),
        ];
        let sse: Pin<Box<dyn futures::Stream<Item = Result<String, ProviderError>> + Send>> =
            Box::pin(futures::stream::iter(lines));
        let mut stream = map_chat_sse(sse);
        let mut calls = Vec::new();
        while let Some(ev) = stream.next().await {
            if let Ok(ProvEvent::ToolCall { name, arguments, .. }) = ev {
                calls.push((name, arguments));
            }
        }
        // 只发一个 ToolCall,参数由两个无 index 分片拼成。
        assert_eq!(calls.len(), 1, "must emit exactly one ToolCall, got {calls:?}");
        assert_eq!(calls[0].0, "run_shell");
        assert_eq!(calls[0].1, json!({"command": "ls"}));
    }

    #[tokio::test]
    async fn map_responses_sse_done_without_added_still_emits_call() {
        use futures::StreamExt;
        use std::pin::Pin;
        // output_item.added 未到达(代理丢事件/流从中途开始):done 自带完整
        // name+arguments,必须直接发出。修复前 remove 返回 None → 整个调用被丢。
        let lines = vec![
            Ok::<_, ProviderError>(
                r#"{"type":"response.output_item.done","item":{"type":"function_call","id":"fc_9","name":"run_shell","arguments":"{\"command\":\"ls\"}"}}"#.to_string(),
            ),
            Ok(r#"{"type":"response.completed","response":{"usage":{"total_tokens":3}}}"#.to_string()),
        ];
        let sse: Pin<Box<dyn futures::Stream<Item = Result<String, ProviderError>> + Send>> =
            Box::pin(futures::stream::iter(lines));
        let mut stream = map_responses_sse(sse);
        let mut calls = Vec::new();
        while let Some(ev) = stream.next().await {
            if let Ok(ProvEvent::ToolCall { name, arguments, .. }) = ev {
                calls.push((name, arguments));
            }
        }
        assert_eq!(calls.len(), 1, "must emit ToolCall from done, got {calls:?}");
        assert_eq!(calls[0].0, "run_shell");
        assert_eq!(calls[0].1, json!({"command": "ls"}));
    }
}
