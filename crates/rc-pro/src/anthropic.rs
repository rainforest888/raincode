use crate::canonical::{CanonicalMessage, CanonicalRequest, CanonicalRole, ProvEvent};
use crate::provider::{
    parse_tool_arguments, retry_provider_request, ProvStream, Provider, ProviderConfig,
    ProviderError,
};
use crate::sse::{response_bytes, sse_events};
use async_trait::async_trait;
use futures::{Stream, StreamExt};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::pin::Pin;

/// 单个工具调用累积参数的上限:超出即丢弃后续分片,防止畸形/恶意 provider
/// 无限流 input_json_delta 把堆撑爆(与 openai.rs 的 MAX_TOOL_ARGS_BYTES 一致)。
const MAX_TOOL_ARGS_BYTES: usize = 128 * 1024;

pub struct AnthropicProvider {
    cfg: ProviderConfig,
    id: String,
}

impl AnthropicProvider {
    pub fn new(cfg: ProviderConfig, id: String) -> Self {
        Self { cfg, id }
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn id(&self) -> &str {
        &self.id
    }

    async fn stream(&self, req: CanonicalRequest) -> Result<ProvStream, ProviderError> {
        let client = self.cfg.client()?;
        let base = self.cfg.base_url.trim_end_matches('/').to_string();
        let url = format!("{}/v1/messages", base);
        let body = build_anthropic_body(&req, &self.cfg.model);
        let key = self.cfg.resolve_api_key();
        // 503/429/5xx/传输抖动自动重试(最多 1+3 次);stream 返回后不再重试。
        let resp = retry_provider_request(|| {
            let mut builder = client.post(&url).json(&body);
            if let Some(k) = key.as_deref() {
                builder = builder.header("x-api-key", k);
            }
            builder = builder.header("anthropic-version", "2023-06-01");
            async move {
                let resp = builder
                    .send()
                    .await
                    .map_err(|e| ProviderError::Transport(e.to_string()))?;
                if !resp.status().is_success() {
                    let status = resp.status().as_u16();
                    let body = resp.text().await.unwrap_or_default();
                    return Err(ProviderError::Http { status, body });
                }
                Ok(resp)
            }
        })
        .await?;
        Ok(map_anthropic_sse(sse_events(response_bytes(resp))))
    }

    async fn embed(&self, _texts: Vec<String>) -> Result<Vec<Vec<f32>>, ProviderError> {
        Err(ProviderError::Unsupported(
            "anthropic provider has no embed method".into(),
        ))
    }
}

/// 构建 Anthropic Messages 请求体。cache_control 默认开(3 个 ephemeral 断点:
/// 最后一个工具定义、最后一个 system part、最新 user 消息),`extra.cache_control
/// == false` 时关闭。抽取为纯函数以便单测。
/// `wire_model` = `cfg.model`(不是 `req.model`,后者是 `kind:model` 复合 id)。
pub(crate) fn build_anthropic_body(req: &CanonicalRequest, wire_model: &str) -> Value {
    let cache_on = req
        .extra
        .get("cache_control")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let system: Vec<String> = req
        .messages
        .iter()
        .filter(|m| m.role == CanonicalRole::System)
        .map(|m| m.text())
        .collect();
    let messages: Vec<Value> = req
        .messages
        .iter()
        .filter(|m| m.role != CanonicalRole::System)
        .map(to_anthropic_message)
        .collect();

    let mut body = json!({
        "model": wire_model,
        "max_tokens": req.max_tokens.unwrap_or(4096),
        "messages": messages,
        "stream": true,
    });
    if !system.is_empty() {
        let mut blocks: Vec<Value> = system
            .iter()
            .map(|s| json!({"type": "text", "text": s}))
            .collect();
        if cache_on {
            if let Some(b) = blocks.last_mut() {
                b["cache_control"] = json!({"type": "ephemeral"});
            }
        }
        body["system"] = json!(blocks);
    }
    if !req.tools.is_empty() {
        let mut tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.input_schema,
                })
            })
            .collect();
        if cache_on {
            if let Some(t) = tools.last_mut() {
                t["cache_control"] = json!({"type": "ephemeral"});
            }
        }
        body["tools"] = json!(tools);
    }
    if cache_on {
        // 直接在 body 的 messages 数组上打断点(json! 把局部 Vec 克隆进 body,
        // 事后改局部变量不会反映到 body)。
        if let Some(arr) = body["messages"].as_array_mut() {
            mark_latest_user_message(arr);
        }
    }
    if let Some(t) = req.temperature {
        body["temperature"] = t.into();
    }
    body
}

/// 在最新一条"纯文本 user 消息"(非 tool_result)的最后一个文本块上打断点。
fn mark_latest_user_message(messages: &mut [Value]) {
    let idx = messages
        .iter()
        .rposition(|m| {
            m["role"] == "user"
                && m["content"]
                    .as_array()
                    .and_then(|c| c.first())
                    .and_then(|c| c.get("type"))
                    .and_then(Value::as_str)
                    .map(|t| t != "tool_result")
                    .unwrap_or(false)
        });
    if let Some(i) = idx {
        if let Some(content) = messages[i]["content"].as_array_mut() {
            if let Some(last) = content.last_mut() {
                last["cache_control"] = json!({"type": "ephemeral"});
            }
        }
    }
}

fn to_anthropic_message(m: &CanonicalMessage) -> Value {
    match m.role {
        CanonicalRole::System => unreachable!(),
        CanonicalRole::User => {
            let mut content = vec![json!({"type": "text", "text": m.text()})];
            if let Some(tool_id) = &m.tool_call_id {
                content = vec![json!({
                    "type": "tool_result",
                    "tool_use_id": tool_id,
                    "content": m.text(),
                })];
            }
            json!({"role": "user", "content": content})
        }
        CanonicalRole::Assistant => {
            let mut content = Vec::new();
            if !m.text().is_empty() {
                content.push(json!({"type": "text", "text": m.text()}));
            }
            for call in &m.tool_calls {
                content.push(json!({
                    "type": "tool_use",
                    "id": call.id,
                    "name": call.name,
                    "input": call.arguments,
                }));
            }
            json!({"role": "assistant", "content": content})
        }
        CanonicalRole::Tool => {
            json!({
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": m.tool_call_id.clone().unwrap_or_default(),
                    "content": m.text(),
                }]
            })
        }
    }
}

struct BlockState {
    kind: String,
    id: String,
    name: String,
    args: String,
}

fn map_anthropic_sse(
    sse: Pin<Box<dyn Stream<Item = Result<String, ProviderError>> + Send>>,
) -> ProvStream {
    let state = (
        sse,
        VecDeque::<Result<ProvEvent, ProviderError>>::new(),
        Option::<BlockState>::None,
    );
    Box::pin(futures::stream::unfold(
        state,
        |(mut s, mut pending, mut block)| async move {
            loop {
                if let Some(ev) = pending.pop_front() {
                    return Some((ev, (s, pending, block)));
                }
                match s.next().await {
                    None => {
                        // 流提前结束(content_block_stop 未到达):把挂起的工具调用发出,
                        // 避免已分片到达的参数因缺收尾事件而静默丢弃。
                        if let Some(state) = block.take() {
                            if state.kind == "tool" {
                                let args =
                                    parse_tool_arguments(&state.args).unwrap_or_default();
                                pending.push_back(Ok(ProvEvent::ToolCall {
                                    id: state.id.clone(),
                                    name: state.name.clone(),
                                    arguments: args,
                                }));
                                pending
                                    .push_back(Ok(ProvEvent::ToolCallEnd { id: state.id }));
                            }
                        }
                        if pending.is_empty() {
                            return None;
                        }
                        if let Some(ev) = pending.pop_front() {
                            return Some((ev, (s, pending, block)));
                        }
                        return None;
                    }
                    Some(Err(e)) => return Some((Err(e), (s, pending, block))),
                    Some(Ok(line)) => {
                        let ev: Value = match serde_json::from_str(&line) {
                            Ok(v) => v,
                            Err(_) => continue,
                        };
                        match ev["type"].as_str().unwrap_or("") {
                            "content_block_start" => {
                                let cb = &ev["content_block"];
                                let idx = ev["index"].as_u64().unwrap_or(0);
                                match cb["type"].as_str().unwrap_or("") {
                                    "tool_use" => {
                                        // 参数经 input_json_delta 分片到达,start 时通常为空:
                                        // 这里只记录状态,等 content_block_stop(或流结束)收尾
                                        // 时再发完整参数的 ToolCall——start 即发会向前端丢一个
                                        // 空参数幽灵调用,并在 start 后截断时误执行空参调用。
                                        let id = cb["id"].as_str().unwrap_or_default().to_string();
                                        let name =
                                            cb["name"].as_str().unwrap_or_default().to_string();
                                        block = Some(BlockState {
                                            kind: "tool".into(),
                                            id: id.clone(),
                                            name: name.clone(),
                                            args: String::new(),
                                        });
                                    }
                                    _ => {
                                        block = Some(BlockState {
                                            kind: "text".into(),
                                            id: idx.to_string(),
                                            name: String::new(),
                                            args: String::new(),
                                        });
                                    }
                                }
                            }
                            "content_block_delta" => {
                                let delta = &ev["delta"];
                                match delta["type"].as_str().unwrap_or("") {
                                    "text_delta" => {
                                        let text =
                                            delta["text"].as_str().unwrap_or_default().to_string();
                                        if !text.is_empty() {
                                            pending.push_back(Ok(ProvEvent::Delta { text }));
                                        }
                                    }
                                    "thinking_delta" => {
                                        let text = delta["thinking"]
                                            .as_str()
                                            .unwrap_or_default()
                                            .to_string();
                                        if !text.is_empty() {
                                            pending.push_back(Ok(ProvEvent::Thinking { text }));
                                        }
                                    }
                                    "input_json_delta" => {
                                        if let Some(state) = &mut block {
                                            if state.kind == "tool" {
                                                let partial = delta["partial_json"]
                                                    .as_str()
                                                    .unwrap_or_default();
                                                if !partial.is_empty()
                                                    && state.args.len() < MAX_TOOL_ARGS_BYTES
                                                {
                                                    state.args.push_str(partial);
                                                }
                                            }
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            "content_block_stop" => {
                                if let Some(state) = block.take() {
                                    if state.kind == "tool" {
                                        let args =
                                            parse_tool_arguments(&state.args).unwrap_or_default();
                                        pending.push_back(Ok(ProvEvent::ToolCall {
                                            id: state.id.clone(),
                                            name: state.name.clone(),
                                            arguments: args,
                                        }));
                                        pending
                                            .push_back(Ok(ProvEvent::ToolCallEnd { id: state.id }));
                                    }
                                }
                            }
                            "message_delta" => {
                                let reason = ev["delta"]["stop_reason"]
                                    .as_str()
                                    .unwrap_or("end_turn")
                                    .to_string();
                                pending.push_back(Ok(ProvEvent::Finish {
                                    stop_reason: reason,
                                    usage: None,
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
    use crate::canonical::{CanonicalToolCall, ToolDef};

    fn sample_request() -> CanonicalRequest {
        CanonicalRequest {
            model: "claude-x".into(),
            messages: vec![
                CanonicalMessage::system("You are Raincode."),
                CanonicalMessage::system("Workspace: /tmp/p"),
                CanonicalMessage::user("first user task"),
                CanonicalMessage::assistant_tool_calls(vec![CanonicalToolCall {
                    id: "t1".into(),
                    name: "read_file".into(),
                    arguments: json!({"path": "a"}),
                }]),
                CanonicalMessage::tool("t1", "read_file", "file contents"),
                CanonicalMessage::user("follow up"),
            ],
            tools: vec![
                ToolDef {
                    name: "read_file".into(),
                    description: "read".into(),
                    input_schema: json!({"type": "object"}),
                },
                ToolDef {
                    name: "run_shell".into(),
                    description: "run".into(),
                    input_schema: json!({"type": "object"}),
                },
            ],
            temperature: None,
            max_tokens: None,
            stream: true,
            extra: json!({}),
        }
    }

    /// 统计所有 cache_control 断点:system 块、工具定义、以及每条 message 的
    /// content 块(user 消息断点在 content 块上,不在 message 对象顶层)。
    fn count_breakpoints(body: &Value) -> usize {
        let mut count = 0;
        for v in [&body["system"], &body["tools"]] {
            if let Some(arr) = v.as_array() {
                for part in arr {
                    if part.get("cache_control").is_some() {
                        count += 1;
                    }
                }
            }
        }
        if let Some(arr) = body["messages"].as_array() {
            for part in arr {
                if let Some(content) = part["content"].as_array() {
                    for block in content {
                        if block.get("cache_control").is_some() {
                            count += 1;
                        }
                    }
                }
            }
        }
        count
    }

    #[test]
    fn anthropic_body_has_exactly_three_ephemeral_breakpoints() {
        let body = build_anthropic_body(&sample_request(), "claude-x");
        // (a) 系统数组最后一块
        let system = body["system"].as_array().unwrap();
        assert!(system[0].get("cache_control").is_none());
        assert_eq!(system.last().unwrap()["cache_control"]["type"], "ephemeral");
        // (b) 最后一个工具定义
        let tools = body["tools"].as_array().unwrap();
        assert!(tools[0].get("cache_control").is_none());
        assert_eq!(tools.last().unwrap()["cache_control"]["type"], "ephemeral");
        // (c) 最新 user 消息(文本,非 tool_result)——注意用 rev().find() 取最新
        let messages = body["messages"].as_array().unwrap();
        let last_user = messages
            .iter()
            .rev()
            .find(|m| {
                m["role"] == "user"
                    && !m["content"][0].get("type").map(|t| t == "tool_result").unwrap_or(false)
            })
            .unwrap_or_else(|| panic!("no text user message"));
        let content = last_user["content"].as_array().unwrap();
        assert_eq!(content.last().unwrap()["cache_control"]["type"], "ephemeral");
        // 没有多余断点
        assert_eq!(count_breakpoints(&body), 3, "exactly 3 breakpoints");
        // wire model 用 cfg.model,不是 req.model。
        assert_eq!(body["model"], "claude-x");
    }

    #[test]
    fn anthropic_body_system_is_array_of_blocks() {
        let body = build_anthropic_body(&sample_request(), "claude-x");
        let system = body["system"].as_array().unwrap();
        assert_eq!(system.len(), 2);
        assert_eq!(system[0]["type"], "text");
        assert_eq!(system[0]["text"], "You are Raincode.");
    }

    #[test]
    fn anthropic_cache_control_can_be_disabled_via_extra() {
        let mut req = sample_request();
        req.extra = json!({"cache_control": false});
        let body = build_anthropic_body(&req, "claude-x");
        assert_eq!(count_breakpoints(&body), 0, "disabled: no breakpoints");
    }

    #[tokio::test]
    async fn map_anthropic_sse_caps_tool_args_bytes() {
        use futures::StreamExt;
        use std::pin::Pin;
        // 畸形/恶意端点无限流 partial_json:累积必须被 cap 截断。一旦 len 越过
        // MAX_TOOL_ARGS_BYTES,后续分片被丢弃 → 截断破坏 JSON → 兜底空对象。
        // 修复前无 cap,20 片 10KB 全部追加,堆无上限增长。
        let mut lines: Vec<Result<String, ProviderError>> = vec![
            Ok(json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "tool_use", "id": "tu_1", "name": "run_shell", "input": {}}
            })
            .to_string()),
            Ok(r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"command\":\""}}"#.to_string()),
        ];
        let pad = "a".repeat(10 * 1024);
        for _ in 0..20 {
            lines.push(Ok(json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "input_json_delta", "partial_json": pad.clone()}
            })
            .to_string()));
        }
        lines.push(Ok(r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"\"}"}}"#.to_string()));
        lines.push(Ok(r#"{"type":"content_block_stop","index":0}"#.to_string()));
        lines.push(Ok(r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#.to_string()));
        let sse: Pin<Box<dyn futures::Stream<Item = Result<String, ProviderError>> + Send>> =
            Box::pin(futures::stream::iter(lines));
        let mut stream = map_anthropic_sse(sse);
        let mut calls = Vec::new();
        while let Some(ev) = stream.next().await {
            if let Ok(ProvEvent::ToolCall { name, arguments, .. }) = ev {
                calls.push((name, arguments));
            }
        }
        assert_eq!(calls.len(), 1, "must emit one ToolCall, got {calls:?}");
        assert_eq!(calls[0].0, "run_shell");
        // 无 cap 时 20 片 10KB 全部追加、尾部 `"}` 拼上 → 解析为完整对象;
        // 有 cap 时尾部被丢 → JSON 不完整 → parse 失败。这证明 cap 生效。
        let full = "a".repeat(20 * 10 * 1024);
        assert_ne!(
            calls[0].1,
            json!({"command": full}),
            "cap must drop trailing deltas so the full command cannot be parsed"
        );
    }
}
