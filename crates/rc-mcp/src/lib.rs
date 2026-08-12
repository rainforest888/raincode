//! Minimal MCP client for Raincode. Supports stdio servers (newline-delimited
//! JSON-RPC) and HTTP/SSE servers (single JSON-RPC POST), exposes tools as
//! ordinary Raincode `Tool`s under the `mcp__<server>_<tool>` namespace.

use std::collections::BTreeMap;
use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
use rc_tool::{Tool, ToolContext, ToolResult, ToolSpec};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::Mutex;

#[derive(Debug, Clone, Deserialize)]
pub struct McpServerConfig {
    #[serde(default, alias = "transport")]
    pub kind: String,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolSpec {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_schema")]
    pub input_schema: Value,
}

fn default_schema() -> Value {
    json!({"type": "object", "properties": {}})
}

#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("rpc error: {0}")]
    Rpc(String),
    #[error("mcp server missing {0}")]
    Missing(String),
    #[error("http status {status}: {body}")]
    HttpStatus { status: u16, body: String },
    #[error("mcp stream ended unexpectedly")]
    StreamEnded,
    #[error("tool failed: {0}")]
    Tool(String),
}

#[derive(Debug)]
pub struct McpClient {
    inner: McpInner,
}

#[derive(Debug)]
enum McpInner {
    // StdioClient 含 Child/BufReader(480B),box 以缩小 McpInner 枚举尺寸。
    Stdio(Box<Mutex<StdioClient>>),
    Http(HttpClient),
}

#[allow(dead_code)]
#[derive(Debug)]
struct StdioClient {
    child: Child,
    stdin: ChildStdin,
    reader: BufReader<ChildStdout>,
    next_id: u64,
}

#[derive(Debug)]
struct HttpClient {
    url: String,
    headers: BTreeMap<String, String>,
    client: reqwest::Client,
}

impl McpClient {
    pub async fn connect(config: McpServerConfig) -> Result<Self, McpError> {
        let kind = config.kind.to_lowercase();
        if kind == "http" || kind == "sse" || config.url.is_some() {
            let url = config.url.ok_or_else(|| McpError::Missing("url".into()))?;
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(60))
                .build()?;
            let http = HttpClient {
                url,
                headers: config.headers,
                client,
            };
            http.request(
                "initialize",
                json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {"name": "raincode", "version": "0.1"}
                }),
            )
            .await?;
            // 2024-11-05 规范:initialize 完成后客户端应发送 notifications/initialized。
            // 部分严格服务器在收到它之前会拒绝后续请求;通知尽力而为,失败不阻断连接。
            if let Err(e) = http.notify("notifications/initialized", json!({})).await {
                tracing::warn!("MCP HTTP server rejected notifications/initialized: {e}");
            }
            Ok(Self {
                inner: McpInner::Http(http),
            })
        } else {
            let command = config
                .command
                .ok_or_else(|| McpError::Missing("command".into()))?;
            let mut cmd = tokio::process::Command::new(&command);
            cmd.args(&config.args);
            cmd.stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null());
            for (key, value) in &config.env {
                cmd.env(key, value);
            }
            let mut child = cmd.spawn()?;
            let stdin = child
                .stdin
                .take()
                .ok_or_else(|| McpError::Missing("stdin".into()))?;
            let stdout = child
                .stdout
                .take()
                .ok_or_else(|| McpError::Missing("stdout".into()))?;
            let mut stdio = StdioClient {
                child,
                stdin,
                reader: BufReader::new(stdout),
                next_id: 1,
            };
            stdio
                .request(
                    "initialize",
                    json!({
                        "protocolVersion": "2024-11-05",
                        "capabilities": {},
                        "clientInfo": {"name": "raincode", "version": "0.1"}
                    }),
                )
                .await?;
            stdio.notify("notifications/initialized", json!({})).await?;
            Ok(Self {
                inner: McpInner::Stdio(Box::new(Mutex::new(stdio))),
            })
        }
    }


    pub async fn tools(&self) -> Result<Vec<McpToolSpec>, McpError> {
        let result = self.request("tools/list", json!({})).await?;
        let tools = result
            .get("tools")
            .and_then(Value::as_array)
            .ok_or_else(|| McpError::Rpc("tools/list returned no tools array".into()))?;
        let mut specs = Vec::new();
        for tool in tools {
            let name = tool
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| McpError::Rpc("tool missing name".into()))?;
            specs.push(McpToolSpec {
                name: name.to_string(),
                description: tool
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                input_schema: tool
                    .get("inputSchema")
                    .or_else(|| tool.get("input_schema"))
                    .cloned()
                    .unwrap_or_else(default_schema),
            });
        }
        Ok(specs)
    }

    pub async fn call(&self, tool_name: &str, args: Value) -> Result<Value, McpError> {
        let result = self
            .request("tools/call", json!({"name": tool_name, "arguments": args}))
            .await?;
        if result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            let message = text_from_content(result.get("content"));
            return Err(McpError::Tool(message));
        }
        Ok(Value::String(text_from_content(result.get("content"))))
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, McpError> {
        match &self.inner {
            McpInner::Stdio(mutex) => mutex.lock().await.request(method, params).await,
            McpInner::Http(http) => http.request(method, params).await,
        }
    }
}

impl StdioClient {
    async fn request(&mut self, method: &str, params: Value) -> Result<Value, McpError> {
        let id = self.next_id;
        self.next_id += 1;
        let request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.write(&request).await?;
        loop {
            let mut line = String::new();
            let read = self.reader.read_line(&mut line).await?;
            if read == 0 {
                return Err(McpError::StreamEnded);
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let value: Value = serde_json::from_str(trimmed)?;
            if value.get("id") == Some(&json!(id)) {
                if let Some(error) = value.get("error") {
                    return Err(McpError::Rpc(error.to_string()));
                }
                return value
                    .get("result")
                    .cloned()
                    .ok_or_else(|| McpError::Rpc(format!("response for {method} has no result")));
            }
        }
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<(), McpError> {
        let request = json!({"jsonrpc": "2.0", "method": method, "params": params});
        self.write(&request).await
    }

    async fn write(&mut self, value: &Value) -> Result<(), McpError> {
        let mut line = value.to_string();
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await?;
        self.stdin.flush().await?;
        Ok(())
    }
}

impl HttpClient {
    async fn request(&self, method: &str, params: Value) -> Result<Value, McpError> {
        let request = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        let mut builder = self.client.post(&self.url).json(&request);
        for (key, value) in &self.headers {
            builder = builder.header(key, value);
        }
        let response = builder.send().await?;
        let status = response.status();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let body = response.text().await?;
        if !status.is_success() {
            return Err(McpError::HttpStatus {
                status: status.as_u16(),
                body,
            });
        }
        if content_type.contains("text/event-stream")
            || body.trim_start().starts_with("event:")
            || body.trim_start().starts_with("data:")
        {
            return parse_sse_response(&body, method);
        }
        let value: Value = serde_json::from_str(&body)?;
        if let Some(error) = value.get("error") {
            return Err(McpError::Rpc(error.to_string()));
        }
        value
            .get("result")
            .cloned()
            .ok_or_else(|| McpError::Rpc(format!("response for {method} has no result")))
    }

    /// 发送 JSON-RPC 通知(无 id,服务器不回复 result)。返回 HTTP 层错误,
    /// 非 2xx 状态视为失败(调用方决定是否致命)。
    async fn notify(&self, method: &str, params: Value) -> Result<(), McpError> {
        let request = json!({"jsonrpc": "2.0", "method": method, "params": params});
        let mut builder = self.client.post(&self.url).json(&request);
        for (key, value) in &self.headers {
            builder = builder.header(key, value);
        }
        let response = builder.send().await?;
        let status = response.status();
        if !status.is_success() {
            return Err(McpError::HttpStatus {
                status: status.as_u16(),
                body: String::new(),
            });
        }
        Ok(())
    }
}

fn parse_sse_response(body: &str, method: &str) -> Result<Value, McpError> {
    // SSE 规范:一个事件的多个 data: 行用换行连接(JSON 通常单行)。
    let mut data = Vec::new();
    let mut events = Vec::new();
    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            data.push(rest.trim().to_string());
        } else if line.trim().is_empty() && !data.is_empty() {
            let text = data.join("\n");
            data.clear();
            if let Ok(value) = serde_json::from_str::<Value>(&text) {
                events.push(value);
            }
        }
    }
    if !data.is_empty() {
        if let Ok(value) = serde_json::from_str::<Value>(&data.join("\n")) {
            events.push(value);
        }
    }
    for event in events {
        if let Some(error) = event.get("error") {
            return Err(McpError::Rpc(error.to_string()));
        }
        if let Some(result) = event.get("result") {
            return Ok(result.clone());
        }
    }
    Err(McpError::Rpc(format!(
        "response for {method} has no result"
    )))
}

pub struct McpManager {
    pub tools: Vec<McpTool>,
    pub servers: Vec<String>,
    /// 连接失败被跳过的服务器(慢连接/启动失败不阻塞 agent 启动)。
    pub failed: Vec<String>,
}

impl McpManager {
    pub async fn connect_all(
        configs: &BTreeMap<String, McpServerConfig>,
    ) -> Result<Self, McpError> {
        const STARTUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
        let mut tools = Vec::new();
        let mut servers = Vec::new();
        let mut failed = Vec::new();
        for (name, config) in configs {
            let client = match tokio::time::timeout(STARTUP_TIMEOUT, McpClient::connect(config.clone())).await {
                Ok(Ok(client)) => Arc::new(client),
                _ => {
                    failed.push(name.clone());
                    tracing::warn!("MCP server `{name}` skipped: connect failed/timed out");
                    continue;
                }
            };
            let specs = match tokio::time::timeout(STARTUP_TIMEOUT, client.tools()).await {
                Ok(Ok(specs)) => specs,
                Ok(Err(e)) => {
                    failed.push(name.clone());
                    tracing::warn!("MCP server `{name}` skipped: tools/list failed: {e}");
                    continue;
                }
                Err(_) => {
                    failed.push(name.clone());
                    tracing::warn!("MCP server `{name}` skipped: tools/list timed out");
                    continue;
                }
            };
            servers.push(name.clone());
            for spec in specs {
                let namespaced = McpToolSpec {
                    name: format!("mcp__{}_{}", name, spec.name),
                    description: spec.description,
                    input_schema: spec.input_schema,
                };
                tools.push(McpTool {
                    client: client.clone(),
                    server: name.clone(),
                    tool_name: spec.name.clone(),
                    spec: namespaced,
                });
            }
        }
        Ok(Self {
            tools,
            servers,
            failed,
        })
    }
}

pub struct McpTool {
    client: Arc<McpClient>,
    server: String,
    tool_name: String,
    spec: McpToolSpec,
}

impl McpTool {
    /// MCP 服务器名(配置键,如 `github`)。
    pub fn server_name(&self) -> &str {
        &self.server
    }

    /// 服务器暴露的原始工具名(如 `search_repos`),未加 `mcp__` 前缀。
    pub fn tool_name(&self) -> &str {
        &self.tool_name
    }

    /// 直接调用本工具(绕过 Tool trait 的命名空间映射),集成测试用。
    pub async fn call(&self, name: &str, args: Value) -> Result<Value, McpError> {
        self.client.call(name, args).await
    }
}

#[async_trait]
impl Tool for McpTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.spec.name.clone(),
            description: self.spec.description.clone(),
            input_schema: self.spec.input_schema.clone(),
        }
    }

    async fn run(&self, args: Value, _ctx: &ToolContext) -> ToolResult {
        match self.client.call(&self.tool_name, args).await {
            Ok(Value::String(text)) => ToolResult::ok(text),
            Ok(value) => ToolResult::ok(value.to_string()),
            Err(e) => ToolResult::err(e.to_string()),
        }
    }
}

fn text_from_content(content: Option<&Value>) -> String {
    let Some(content) = content else {
        return String::new();
    };
    let Some(items) = content.as_array() else {
        return content.to_string();
    };
    let mut text = String::new();
    for item in items {
        match item.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(t) = item.get("text").and_then(Value::as_str) {
                    text.push_str(t);
                    text.push('\n');
                }
            }
            Some("image") => {
                let data = item
                    .get("data")
                    .and_then(Value::as_str)
                    .unwrap_or("<binary>");
                let mime = item
                    .get("mimeType")
                    .and_then(Value::as_str)
                    .unwrap_or("application/octet-stream");
                text.push_str(&format!("[image {mime}: {data}]\n"));
            }
            _ => text.push_str(&item.to_string()),
        }
    }
    text.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_namespace_is_double_underscore() {
        // 命名规则:name = mcp__<server>_<tool>,与 rc-core 暴露给模型的名字一致。
        let namespaced = format!("mcp__{}_{}", "github", "search_repos");
        assert_eq!(namespaced, "mcp__github_search_repos");
    }

    #[test]
    fn schema_default_is_object() {
        assert_eq!(
            default_schema(),
            json!({"type": "object", "properties": {}})
        );
    }

    #[test]
    fn parses_sse_response_result() {
        let body =
            "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n\n";
        let value = parse_sse_response(body, "tools/list").unwrap();
        assert_eq!(value["ok"], json!(true));
    }

    #[test]
    fn parses_sse_response_error() {
        let body = "event: error\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":-32601,\"message\":\"nope\"}}\n\n";
        assert!(parse_sse_response(body, "tools/list").is_err());
    }

    #[test]
    fn parses_sse_response_joins_multiline_data_with_newline() {
        // SSE 规范:同一事件的多行 data: 用换行连接;JSON 在 token 之间换行仍合法。
        let body = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":\ndata: {\"ok\":true}}\n\n";
        let value = parse_sse_response(body, "tools/list").unwrap();
        assert_eq!(value["ok"], json!(true));
    }
}
