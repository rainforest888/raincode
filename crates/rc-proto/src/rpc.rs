//! JSON-RPC 2.0 request/response framing used by `raincode --serve`.
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

pub const JSONRPC_VERSION: &str = "2.0";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RequestMethod {
    Start,
    Resume,
    Respond,
    SetModel,
    SkillList,
    SkillLoad,
    SkillCreate,
    SkillEdit,
    SkillInstall,
    SkillUpdate,
    SkillUninstall,
    SkillSearch,
    Evolve,
    InsightsScan,
    ModelList,
    ModelUse,
    McpList,
    /// Inject a steering instruction into an agent's next checkpoint (desktop 接管).
    Steer,
    /// Forward a chat message to the chat model; slash-prefixed text runs the
    /// built-in command path (run_slash_command). Full chat loop is out of scope.
    Chat,
    /// List persisted sessions for the desktop 会话 template.
    Sessions,
    /// Delete a session (double-click confirm in the desktop 会话 template).
    SessionDelete,
    /// Built-in slash commands driven by the desktop command palette.
    Compact,
    Clear,
    Route,
    Risk,
    Status,
    Stop,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub jsonrpc: String,
    pub id: Value,
    pub method: RequestMethod,
    #[serde(default)]
    pub params: Value,
}

impl Request {
    pub fn new(method: RequestMethod, params: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: json!(Uuid::new_v4().to_string()),
            method,
            params,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub jsonrpc: String,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl Response {
    pub fn ok(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: Value, error: RpcError) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            result: None,
            error: Some(error),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn parse_error() -> Self {
        Self::new(-32700, "parse error")
    }
}

/// Encode a protocol message as one newline-delimited JSON line.
pub fn encode_line<T: Serialize>(msg: &T) -> Result<String, serde_json::Error> {
    let mut line = serde_json::to_string(msg)?;
    line.push('\n');
    Ok(line)
}

/// Parse a protocol message from one line, tolerating an empty line.
pub fn decode_line<T: for<'de> Deserialize<'de>>(
    line: &str,
) -> Result<Option<T>, serde_json::Error> {
    let line = line.trim();
    if line.is_empty() {
        return Ok(None);
    }
    serde_json::from_str(line).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_response_roundtrip() {
        let req = Request::new(RequestMethod::Start, json!({"prompt": "hi"}));
        let line = encode_line(&req).unwrap();
        let parsed: Request = decode_line(&line).unwrap().unwrap();
        assert_eq!(parsed.method, RequestMethod::Start);
        assert_eq!(parsed.params["prompt"], "hi");
    }

    #[test]
    fn error_roundtrip() {
        let e = RpcError::new(-32000, "boom");
        let s = serde_json::to_string(&e).unwrap();
        let back: RpcError = serde_json::from_str(&s).unwrap();
        assert_eq!(back.code, -32000);
    }

    #[test]
    fn desktop_methods_serialize_snake_case() {
        // The desktop frontend sends exactly these method names (Task 10).
        for (method, expected) in [
            (RequestMethod::Steer, "steer"),
            (RequestMethod::Chat, "chat"),
            (RequestMethod::Sessions, "sessions"),
            (RequestMethod::SessionDelete, "session_delete"),
            (RequestMethod::Compact, "compact"),
            (RequestMethod::Clear, "clear"),
            (RequestMethod::Route, "route"),
            (RequestMethod::Risk, "risk"),
            (RequestMethod::Status, "status"),
        ] {
            let req = Request::new(method, json!({}));
            let line = encode_line(&req).unwrap();
            assert!(
                line.contains(&format!("\"method\":\"{expected}\"")),
                "expected {expected} in {line}"
            );
            // And it round-trips back to the same variant.
            let parsed: Request = decode_line(&line).unwrap().unwrap();
            assert_eq!(parsed.method, req.method);
        }
    }
}
