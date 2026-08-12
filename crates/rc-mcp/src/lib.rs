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

pub struct McpClient {
    inner: McpInner,
}

enum McpInner {
    // StdioClient 含 Child/BufReader(480B),box 以缩小 McpInner 枚举尺寸。
    Stdio(Box<Mutex<StdioClient>>),
    Http(HttpClient),
}

#[allow(dead_code)]
struct StdioClient {
    child: Child,
    stdin: ChildStdin,
    reader: BufReader<ChildStdout>,
    next_id: u64,
}

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
}

fn parse_sse_response(body: &str, method: &str) -> Result<Value, McpError> {
    let mut data = Vec::new();
    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            data.push(rest.trim().to_string());
        } else if line.trim().is_empty() && !data.is_empty() {
            let text = data.join("");
            data.clear();
            if let Ok(value) = serde_json::from_str::<Value>(&text) {
                if let Some(error) = value.get("error") {
                    return Err(McpError::Rpc(error.to_string()));
                }
                if let Some(result) = value.get("result") {
                    return Ok(result.clone());
                }
            }
        }
    }
    if !data.is_empty() {
        let text = data.join("");
        if let Ok(value) = serde_json::from_str::<Value>(&text) {
            if let Some(error) = value.get("error") {
                return Err(McpError::Rpc(error.to_string()));
            }
            if let Some(result) = value.get("result") {
                return Ok(result.clone());
            }
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
    spec: McpToolSpec,
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
        let raw_name = self
            .spec
            .name
            .strip_prefix("mcp__")
            .and_then(|rest| rest.split_once('_'))
            .map(|(_, name)| name.to_string())
            .unwrap_or_else(|| self.spec.name.clone());
        match self.client.call(&raw_name, args).await {
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

    // Shared node-based mock MCP server for stdio integration tests.
    const MOCK_MCP_SCRIPT: &str = r#"
const readline = require("node:readline");
const rl = readline.createInterface({ input: process.stdin });
rl.on("line", (line) => {
  const req = JSON.parse(line);
  if (!req.id) return;
  let result;
  if (req.method === "initialize") {
    result = { protocolVersion: "2024-11-05", capabilities: { tools: {} }, serverInfo: { name: "mock-mcp", version: "1.0" } };
  } else if (req.method === "tools/list") {
    result = { tools: [{ name: "echo", description: "echo input", inputSchema: { type: "object", properties: { text: { type: "string" } }, required: ["text"] } }] };
  } else if (req.method === "tools/call") {
    result = { content: [{ type: "text", text: "echo:" + req.params.arguments.text }] };
  } else {
    result = {};
  }
  process.stdout.write(JSON.stringify({ jsonrpc: "2.0", id: req.id, result }) + "\n");
});
"#;

    fn mock_stdio_config() -> McpServerConfig {
        McpServerConfig {
            kind: "stdio".into(),
            command: Some("node".into()),
            args: vec!["-e".into(), MOCK_MCP_SCRIPT.into()],
            url: None,
            headers: Default::default(),
            env: Default::default(),
        }
    }

    #[test]
    fn mcp_namespace_is_double_underscore() {
        // 命名规则:name = mcp__<server>_<tool>。
        let name = "mcp__github_search_repos";
        assert_eq!(name, "mcp__github_search_repos");
        // run 提取:strip_prefix("mcp__") + split_once('_') → 第一个下划线之后是 raw tool name。
        let (_, raw) = name
            .strip_prefix("mcp__")
            .and_then(|rest| rest.split_once('_'))
            .unwrap();
        assert_eq!(raw, "search_repos");
    }

    #[test]
    fn raw_name_preserves_underscores_in_tool_name() {
        // 回归:工具名本身含下划线时,rsplit_once('_') 会误取最后一个下划线之后的部分。
        let name = "mcp__github_search_repositories";
        let raw = name
            .strip_prefix("mcp__")
            .and_then(|rest| rest.split_once('_'))
            .map(|(_, name)| name.to_string())
            .unwrap_or_else(|| name.to_string());
        assert_eq!(raw, "search_repositories");
    }

    #[tokio::test]
    async fn connect_all_skips_slow_server_instead_of_aborting_all() {
        // 一个服务器 connect 失败(指向关闭端口的 HTTP),另一个成功(node mock)。
        // 断言:坏服务器被跳过并记入 failed,好服务器的工具正常暴露。
        let mut configs: BTreeMap<String, McpServerConfig> = BTreeMap::new();
        configs.insert("good".into(), mock_stdio_config());
        configs.insert(
            "bad".into(),
            McpServerConfig {
                kind: "http".into(),
                command: None,
                args: vec![],
                url: Some("http://127.0.0.1:1".into()),
                headers: Default::default(),
                env: Default::default(),
            },
        );

        let manager = McpManager::connect_all(&configs).await.unwrap();
        assert_eq!(manager.failed, vec!["bad".to_string()]);
        assert_eq!(manager.servers, vec!["good".to_string()]);
        assert_eq!(manager.tools.len(), 1);
        assert_eq!(manager.tools[0].spec().name, "mcp__good_echo");
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

    #[tokio::test]
    async fn stdio_mcp_client_lists_and_calls_tools() {
        let config = mock_stdio_config();
        let client = McpClient::connect(config).await.unwrap();
        let tools = client.tools().await.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");
        assert_eq!(tools[0].description, "echo input");
        let result = client.call("echo", json!({"text": "hello"})).await.unwrap();
        assert_eq!(result, json!("echo:hello"));
    }
}
