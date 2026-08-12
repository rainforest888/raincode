//! CC-Switch-style HTTP gateway. Exposes the active Raincode profile as
//! OpenAI-compatible (`/v1/chat/completions`, `/v1/embeddings`) and
//! Anthropic-compatible (`/v1/messages`) endpoints, plus a live `/switch`
//! endpoint for changing the active profile without restarting clients.

use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::StreamExt;
use rc_pro::canonical::{CanonicalMessage, CanonicalRequest, CanonicalToolCall, ToolDef};
use rc_pro::{create_provider, Provider, ProviderError};
use rc_profile::model::{Profile, Registry};
use rc_profile::writers::all_writers;
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("registry error: {0}")]
    Registry(#[from] rc_profile::model::RegistryError),
    #[error("provider error: {0}")]
    Provider(#[from] ProviderError),
    #[error("lock poisoned")]
    Poisoned,
    #[error("{0}")]
    Switch(String),
    #[error("{0}")]
    BadRequest(String),
    #[error("server error: {0}")]
    Server(String),
}

#[derive(Debug, Clone)]
pub struct GatewayConfig {
    pub addr: std::net::SocketAddr,
    pub registry_path: PathBuf,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            addr: "127.0.0.1:4040".parse().expect("valid local addr"),
            registry_path: rc_profile::model::default_registry_path(),
        }
    }
}

pub struct GatewayState {
    registry_path: PathBuf,
    registry: RwLock<Registry>,
}

impl GatewayState {
    fn active_profile(&self) -> Result<Profile, GatewayError> {
        let registry = self.registry.read().map_err(|_| GatewayError::Poisoned)?;
        registry
            .active()
            .cloned()
            .ok_or_else(|| GatewayError::Switch("no active profile".into()))
    }

    fn provider(&self) -> Result<Box<dyn Provider>, GatewayError> {
        let profile = self.active_profile()?;
        Ok(create_provider(profile.to_provider_config())?)
    }

    fn switch(&self, id_or_name: &str) -> Result<Profile, GatewayError> {
        let mut registry = self.registry.write().map_err(|_| GatewayError::Poisoned)?;
        let profile = registry
            .profiles
            .iter()
            .find(|p| p.id == id_or_name || p.name == id_or_name)
            .cloned()
            .ok_or_else(|| GatewayError::Switch(format!("profile not found: {id_or_name}")))?;
        registry.set_active(&profile.id)?;
        registry.save(&self.registry_path)?;
        Ok(profile)
    }
}

#[derive(Debug, Deserialize)]
struct ChatBody {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    messages: Vec<Value>,
    #[serde(default)]
    tools: Vec<Value>,
    #[serde(default)]
    stream: bool,
    #[serde(default)]
    temperature: Option<f32>,
    #[serde(default)]
    max_tokens: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct MessagesBody {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    max_tokens: Option<u32>,
    #[serde(default)]
    system: Option<Value>,
    #[serde(default)]
    messages: Vec<Value>,
    #[serde(default)]
    tools: Vec<Value>,
    #[serde(default)]
    stream: bool,
}

#[derive(Debug, Deserialize)]
struct EmbeddingBody {
    #[serde(default)]
    model: Option<String>,
    input: Value,
}

#[derive(Debug, Deserialize)]
struct SwitchBody {
    profile: String,
    #[serde(default)]
    app: Option<String>,
    #[serde(default)]
    write_target: bool,
}

pub async fn serve(config: GatewayConfig) -> Result<(), GatewayError> {
    let mut registry = Registry::load(&config.registry_path)?;
    registry.ensure_default();
    let _ = registry.save(&config.registry_path);
    let state = Arc::new(GatewayState {
        registry_path: config.registry_path,
        registry: RwLock::new(registry),
    });
    let app = Router::new()
        .route("/health", get(health))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/messages", post(messages))
        .route("/v1/embeddings", post(embeddings))
        .route("/switch", post(switch_profile))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(config.addr)
        .await
        .map_err(|e| GatewayError::Server(e.to_string()))?;
    tracing::info!("raincode gateway listening on {}", config.addr);
    axum::serve(listener, app)
        .await
        .map_err(|e| GatewayError::Server(e.to_string()))
}

async fn health() -> Json<Value> {
    Json(json!({"ok": true, "service": "raincode-gateway"}))
}

async fn chat_completions(
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<ChatBody>,
) -> Response {
    let profile = match state.active_profile() {
        Ok(p) => p,
        Err(e) => return gateway_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let model = body.model.clone().unwrap_or_else(|| profile.model.clone());
    let messages = match openai_messages(&body.messages) {
        Ok(m) => m,
        Err(e) => return gateway_error(StatusCode::BAD_REQUEST, e),
    };
    let tools = openai_tools(&body.tools);
    let provider = match state.provider() {
        Ok(p) => p,
        Err(e) => return gateway_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let request = CanonicalRequest {
        model: model.clone(),
        messages,
        tools,
        temperature: body.temperature,
        max_tokens: body.max_tokens,
        stream: body.stream,
        extra: json!({}),
    };
    let stream = match provider.stream(request).await {
        Ok(s) => s,
        Err(e) => return gateway_error(StatusCode::BAD_GATEWAY, e.to_string()),
    };

    if body.stream {
        let id = format!("chatcmpl-{}", now_ts());
        let stream = stream.map(move |event| {
            let data = match event {
                Ok(rc_pro::ProvEvent::Delta { text }) => {
                    openai_chunk(&id, &model, json!({"content": text}), None)
                }
                Ok(rc_pro::ProvEvent::Thinking { text }) => {
                    openai_chunk(&id, &model, json!({"reasoning_content": text}), None)
                }
                Ok(rc_pro::ProvEvent::ToolCall {
                    id: call_id,
                    name,
                    arguments,
                }) => openai_chunk(
                    &id,
                    &model,
                    json!({"tool_calls": [{
                        "index": 0,
                        "id": call_id,
                        "type": "function",
                        "function": {"name": name, "arguments": arguments.to_string()}
                    }]}),
                    None,
                ),
                Ok(rc_pro::ProvEvent::ToolCallEnd { .. }) => {
                    openai_chunk(&id, &model, json!({}), None)
                }
                Ok(rc_pro::ProvEvent::Finish { stop_reason, .. }) => {
                    openai_chunk(&id, &model, json!({}), Some(&stop_reason))
                }
                Ok(rc_pro::ProvEvent::Error { message }) => {
                    openai_error_chunk(&id, &model, &message)
                }
                Err(e) => openai_error_chunk(&id, &model, &e.to_string()),
            };
            Ok::<Event, Infallible>(data)
        });
        return Sse::new(stream)
            .keep_alive(KeepAlive::default())
            .into_response();
    }

    let mut content = String::new();
    let mut tool_calls = Vec::new();
    let mut finish_reason = "stop".to_string();
    let mut usage = Value::Null;
    for event in stream.collect::<Vec<_>>().await {
        match event {
            Ok(rc_pro::ProvEvent::Delta { text }) => content.push_str(&text),
            Ok(rc_pro::ProvEvent::ToolCall {
                id,
                name,
                arguments,
            }) => tool_calls.push(json!({
                "id": id,
                "type": "function",
                "function": {"name": name, "arguments": arguments.to_string()}
            })),
            Ok(rc_pro::ProvEvent::Finish {
                stop_reason,
                usage: u,
            }) => {
                finish_reason = stop_reason;
                usage = u.unwrap_or(Value::Null);
            }
            Ok(rc_pro::ProvEvent::Error { message }) => {
                return gateway_error(StatusCode::BAD_GATEWAY, message)
            }
            Ok(_) => {}
            Err(e) => return gateway_error(StatusCode::BAD_GATEWAY, e.to_string()),
        }
    }
    let mut message = json!({
        "role": "assistant",
        "content": content,
        "tool_calls": tool_calls
    });
    if let Value::Object(ref mut obj) = message {
        if obj
            .get("tool_calls")
            .and_then(Value::as_array)
            .map(Vec::is_empty)
            .unwrap_or(true)
        {
            obj.remove("tool_calls");
        }
    }
    Json(json!({
        "id": format!("chatcmpl-{}", now_ts()),
        "object": "chat.completion",
        "created": now_ts(),
        "model": model,
        "choices": [{"index": 0, "message": message, "finish_reason": finish_reason}],
        "usage": usage
    }))
    .into_response()
}

async fn messages(
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<MessagesBody>,
) -> Response {
    let profile = match state.active_profile() {
        Ok(p) => p,
        Err(e) => return gateway_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let model = body.model.clone().unwrap_or_else(|| profile.model.clone());
    let mut canonical = match anthropic_messages(&body.system, &body.messages) {
        Ok(m) => m,
        Err(e) => return gateway_error(StatusCode::BAD_REQUEST, e),
    };
    if let Some(system) = &body.system {
        let text = system_text(system);
        if !text.is_empty() {
            canonical.insert(0, CanonicalMessage::system(text));
        }
    }
    let tools = anthropic_tools(&body.tools);
    let provider = match state.provider() {
        Ok(p) => p,
        Err(e) => return gateway_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let request = CanonicalRequest {
        model: model.clone(),
        messages: canonical,
        tools,
        temperature: None,
        max_tokens: body.max_tokens,
        stream: body.stream,
        extra: json!({}),
    };
    let stream = match provider.stream(request).await {
        Ok(s) => s,
        Err(e) => return gateway_error(StatusCode::BAD_GATEWAY, e.to_string()),
    };
    if body.stream {
        let id = format!("msg_{}", now_ts());
        let stream = stream.map(move |event| {
            let data = match event {
                Ok(rc_pro::ProvEvent::Delta { text }) => {
                    let mut data = String::new();
                    data.push_str(&anthropic_event(
                        "content_block_start",
                        &id,
                        json!({
                            "index": 0,
                            "content_block": {"type": "text", "text": ""}
                        }),
                    ));
                    data.push_str(&anthropic_event(
                        "content_block_delta",
                        &id,
                        json!({
                            "index": 0,
                            "delta": {"type": "text_delta", "text": text}
                        }),
                    ));
                    data.push_str(&anthropic_event(
                        "content_block_stop",
                        &id,
                        json!({"index": 0}),
                    ));
                    data.push_str(&anthropic_event(
                        "message_delta",
                        &id,
                        json!({
                            "delta": {"stop_reason": "end_turn"}
                        }),
                    ));
                    data
                }
                Ok(rc_pro::ProvEvent::ToolCall {
                    id: call_id,
                    name,
                    arguments,
                }) => anthropic_event(
                    "content_block_start",
                    &id,
                    json!({
                        "index": 0,
                        "content_block": {
                            "type": "tool_use",
                            "id": call_id,
                            "name": name,
                            "input": arguments
                        }
                    }),
                ),
                Ok(rc_pro::ProvEvent::Finish { stop_reason, .. }) => anthropic_event(
                    "message_delta",
                    &id,
                    json!({"delta": {"stop_reason": stop_reason}}),
                ),
                Ok(rc_pro::ProvEvent::Error { .. }) => {
                    format!(
                        "event: error\ndata: {}\n\n",
                        json!({"error": {"message": "provider error"}})
                    )
                }
                Err(e) => {
                    format!(
                        "event: error\ndata: {}\n\n",
                        json!({"error": {"message": e.to_string()}})
                    )
                }
                Ok(_) => String::new(),
            };
            Ok::<Event, Infallible>(Event::default().data(data))
        });
        return Sse::new(stream)
            .keep_alive(KeepAlive::default())
            .into_response();
    }

    let mut text = String::new();
    let mut tool_uses = Vec::new();
    let mut stop_reason = "end_turn".to_string();
    for event in stream.collect::<Vec<_>>().await {
        match event {
            Ok(rc_pro::ProvEvent::Delta { text: t }) => text.push_str(&t),
            Ok(rc_pro::ProvEvent::ToolCall {
                id,
                name,
                arguments,
            }) => tool_uses.push(json!({
                "type": "tool_use",
                "id": id,
                "name": name,
                "input": arguments
            })),
            Ok(rc_pro::ProvEvent::Finish { stop_reason: s, .. }) => stop_reason = s,
            Ok(rc_pro::ProvEvent::Error { message }) => {
                return gateway_error(StatusCode::BAD_GATEWAY, message)
            }
            Ok(_) => {}
            Err(e) => return gateway_error(StatusCode::BAD_GATEWAY, e.to_string()),
        }
    }
    let mut content = Vec::new();
    if !text.is_empty() {
        content.push(json!({"type": "text", "text": text}));
    }
    content.extend(tool_uses);
    Json(json!({
        "id": format!("msg_{}", now_ts()),
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content,
        "stop_reason": stop_reason,
        "usage": {"input_tokens": 0, "output_tokens": 0}
    }))
    .into_response()
}

async fn embeddings(
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<EmbeddingBody>,
) -> Response {
    let profile = match state.active_profile() {
        Ok(p) => p,
        Err(e) => return gateway_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let model = body.model.clone().unwrap_or_else(|| profile.model.clone());
    let texts = match embedding_texts(&body.input) {
        Ok(t) => t,
        Err(e) => return gateway_error(StatusCode::BAD_REQUEST, e),
    };
    let provider = match state.provider() {
        Ok(p) => p,
        Err(e) => return gateway_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    match provider.embed(texts).await {
        Ok(vectors) => {
            let data: Vec<Value> = vectors
                .into_iter()
                .enumerate()
                .map(|(index, embedding)| {
                    json!({
                        "object": "embedding",
                        "embedding": embedding,
                        "index": index
                    })
                })
                .collect();
            Json(json!({
                "object": "list",
                "data": data,
                "model": model,
                "usage": {"prompt_tokens": 0, "total_tokens": 0}
            }))
            .into_response()
        }
        Err(e) => gateway_error(StatusCode::BAD_GATEWAY, e.to_string()),
    }
}

async fn switch_profile(
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<SwitchBody>,
) -> Response {
    let profile = match state.switch(&body.profile) {
        Ok(p) => p,
        Err(e) => return gateway_error(StatusCode::NOT_FOUND, e.to_string()),
    };
    let mut written = Vec::new();
    if body.write_target {
        if let Some(app) = &body.app {
            for writer in all_writers() {
                if writer.app() == app {
                    if let Err(e) = writer.apply(&profile) {
                        return gateway_error(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            format!("failed to write {app} config: {e}"),
                        );
                    }
                    written.push(app.clone());
                }
            }
        }
    }
    Json(json!({"ok": true, "profile": profile, "written": written})).into_response()
}

fn openai_messages(messages: &[Value]) -> Result<Vec<CanonicalMessage>, String> {
    let mut out = Vec::new();
    for value in messages {
        let obj = value
            .as_object()
            .ok_or_else(|| "message must be an object".to_string())?;
        let role = obj
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user")
            .to_string();
        let mut content = Vec::new();
        match obj.get("content") {
            Some(Value::String(s)) if !s.is_empty() => {
                content.push(rc_pro::CanonicalContent::text(s.clone()))
            }
            Some(Value::Array(parts)) => {
                for part in parts {
                    let part_obj = part.as_object().ok_or("content part must be object")?;
                    let text = part_obj.get("text").and_then(Value::as_str);
                    if let Some(text) = text {
                        content.push(rc_pro::CanonicalContent::text(text));
                    } else if let Some(url) = part_obj
                        .get("image_url")
                        .and_then(|u| u.get("url"))
                        .and_then(Value::as_str)
                    {
                        content.push(rc_pro::CanonicalContent::Image {
                            data_url: url.to_string(),
                        });
                    }
                }
            }
            _ => {}
        }
        let mut tool_calls = Vec::new();
        if let Some(calls) = obj.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                let empty_map = serde_json::Map::new();
                let call_obj = call.as_object().unwrap_or(&empty_map);
                let id = call_obj
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("call_1")
                    .to_string();
                let function = call_obj.get("function").unwrap_or(&Value::Null);
                let name = function
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let args = function
                    .get("arguments")
                    .and_then(Value::as_str)
                    .unwrap_or("{}");
                let parsed =
                    serde_json::from_str(args).unwrap_or_else(|_| Value::String(args.to_string()));
                tool_calls.push(CanonicalToolCall {
                    id,
                    name,
                    arguments: parsed,
                });
            }
        }
        match role.as_str() {
            "system" => out.push(CanonicalMessage::system(
                content
                    .iter()
                    .filter_map(|c| match c {
                        rc_pro::CanonicalContent::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(""),
            )),
            "tool" => {
                let text = content
                    .iter()
                    .filter_map(|c| match c {
                        rc_pro::CanonicalContent::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                out.push(CanonicalMessage::tool(
                    obj.get("tool_call_id")
                        .and_then(Value::as_str)
                        .unwrap_or(""),
                    obj.get("name").and_then(Value::as_str).unwrap_or("tool"),
                    text,
                ));
            }
            _ => {
                let mut msg = CanonicalMessage {
                    role: rc_pro::CanonicalRole::User,
                    content,
                    tool_calls: vec![],
                    tool_call_id: None,
                    name: None,
                };
                msg.tool_calls = tool_calls;
                out.push(msg);
            }
        }
    }
    Ok(out)
}

fn openai_tools(tools: &[Value]) -> Vec<ToolDef> {
    tools
        .iter()
        .filter_map(|tool| {
            let function = tool.get("function")?;
            Some(ToolDef {
                name: function.get("name").and_then(Value::as_str)?.to_string(),
                description: function
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                input_schema: function
                    .get("parameters")
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object"})),
            })
        })
        .collect()
}

fn anthropic_messages(
    _system: &Option<Value>,
    messages: &[Value],
) -> Result<Vec<CanonicalMessage>, String> {
    let mut out = Vec::new();
    for value in messages {
        let obj = value.as_object().ok_or("message must be object")?;
        let role = obj.get("role").and_then(Value::as_str).unwrap_or("user");
        let content = obj.get("content").cloned().unwrap_or(Value::Null);
        if role == "user" {
            let mut has_text = false;
            let mut text = String::new();
            match &content {
                Value::String(s) => {
                    text.push_str(s);
                    has_text = true;
                }
                Value::Array(parts) => {
                    for part in parts {
                        let empty_map = serde_json::Map::new();
                        let p = part.as_object().unwrap_or(&empty_map);
                        if let Some(t) = p.get("text").and_then(Value::as_str) {
                            text.push_str(t);
                            has_text = true;
                        }
                    }
                }
                _ => {}
            }
            if has_text {
                out.push(CanonicalMessage::user(text));
            }
            if let Value::Array(parts) = &content {
                for part in parts {
                    let empty_map = serde_json::Map::new();
                    let p = part.as_object().unwrap_or(&empty_map);
                    if p.get("type").and_then(Value::as_str) == Some("tool_result") {
                        let id = p.get("tool_use_id").and_then(Value::as_str).unwrap_or("");
                        let result_text = match p.get("content") {
                            Some(Value::String(s)) => s.clone(),
                            Some(Value::Array(items)) => items
                                .iter()
                                .filter_map(|i| i.get("text").and_then(Value::as_str))
                                .collect::<Vec<_>>()
                                .join(""),
                            _ => String::new(),
                        };
                        out.push(CanonicalMessage::tool(id, "tool", result_text));
                    }
                }
            }
        } else {
            let mut text = String::new();
            let mut tool_calls = Vec::new();
            match &content {
                Value::String(s) => text.push_str(s),
                Value::Array(parts) => {
                    for part in parts {
                        let empty_map = serde_json::Map::new();
                        let p = part.as_object().unwrap_or(&empty_map);
                        match p.get("type").and_then(Value::as_str) {
                            Some("text") => {
                                if let Some(t) = p.get("text").and_then(Value::as_str) {
                                    text.push_str(t);
                                }
                            }
                            Some("tool_use") => {
                                tool_calls.push(CanonicalToolCall {
                                    id: p
                                        .get("id")
                                        .and_then(Value::as_str)
                                        .unwrap_or("call")
                                        .to_string(),
                                    name: p
                                        .get("name")
                                        .and_then(Value::as_str)
                                        .unwrap_or("tool")
                                        .to_string(),
                                    arguments: p.get("input").cloned().unwrap_or_else(|| json!({})),
                                });
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
            let mut msg = CanonicalMessage {
                role: rc_pro::CanonicalRole::Assistant,
                content: vec![],
                tool_calls,
                tool_call_id: None,
                name: None,
            };
            if !text.is_empty() {
                msg.content.push(rc_pro::CanonicalContent::text(text));
            }
            out.push(msg);
        }
    }
    Ok(out)
}

fn anthropic_tools(tools: &[Value]) -> Vec<ToolDef> {
    tools
        .iter()
        .filter_map(|tool| {
            Some(ToolDef {
                name: tool.get("name").and_then(Value::as_str)?.to_string(),
                description: tool
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                input_schema: tool
                    .get("input_schema")
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object"})),
            })
        })
        .collect()
}

fn system_text(system: &Value) -> String {
    match system {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

fn embedding_texts(input: &Value) -> Result<Vec<String>, String> {
    match input {
        Value::String(s) => Ok(vec![s.clone()]),
        Value::Array(items) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| "embedding input must be strings".to_string())
            })
            .collect(),
        _ => Err("embedding input must be a string or array".to_string()),
    }
}

fn gateway_error(status: StatusCode, message: String) -> Response {
    (status, Json(json!({"error": {"message": message}}))).into_response()
}

fn now_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn openai_chunk(id: &str, model: &str, delta: Value, finish: Option<&str>) -> Event {
    let data = json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": now_ts(),
        "model": model,
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]
    });
    Event::default().event("message").data(data.to_string())
}

fn openai_error_chunk(id: &str, model: &str, message: &str) -> Event {
    let data = json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": now_ts(),
        "model": model,
        "choices": [{"index": 0, "delta": {}, "finish_reason": "error"}],
        "error": {"message": message}
    });
    Event::default().event("message").data(data.to_string())
}

fn anthropic_event(name: &str, id: &str, payload: Value) -> String {
    let mut payload = payload;
    if let Value::Object(ref mut obj) = payload {
        obj.insert("type".into(), Value::String(name.to_string()));
        obj.insert("message".into(), json!({"id": id, "type": "message"}));
    }
    format!("event: {name}\ndata: {}\n\n", payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use tower::util::ServiceExt;

    fn test_app() -> Router {
        let dir = tempfile::tempdir().unwrap();
        let registry_path = dir.path().join("profiles.toml");
        let mut registry = Registry::default();
        registry.ensure_default();
        registry.save(&registry_path).unwrap();
        drop(dir);
        let state = Arc::new(GatewayState {
            registry_path,
            registry: RwLock::new(registry),
        });
        Router::new()
            .route("/health", get(health))
            .route("/switch", post(switch_profile))
            .with_state(state)
    }

    #[tokio::test]
    async fn health_returns_ok() {
        let app = test_app();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), 4096).await.unwrap();
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["ok"], true);
    }

    #[tokio::test]
    async fn switch_activates_default_profile() {
        let app = test_app();
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/switch")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"profile":"default"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), 4096).await.unwrap();
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["profile"]["id"], "default");
    }
}
