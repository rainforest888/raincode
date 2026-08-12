//! 单元测试专用 stdio mock MCP 服务器(纯 Rust,无 node 依赖)。
//!
//! 通过 env!("CARGO_BIN_EXE_mcp_mock") 在 rc-mcp 的单元测试中作为子进程
//! 启动,按 newline-delimited JSON-RPC 与客户端对话。行为由环境变量切换,
//! 便于覆盖错误路径:
//!
//! - `MCP_MOCK_FAIL_CALL=1`  tools/call 返回 `isError: true`(工具级错误)。
//! - `MCP_MOCK_RPC_ERROR=1` tools/list 返回 JSON-RPC error(协议级错误)。
//! - `MCP_MOCK_EXIT_AFTER=1` 收到首个请求后不回复直接退出(模拟流中断)。

use std::io::{BufRead, Write};

fn main() {
    let fail_call = std::env::var("MCP_MOCK_FAIL_CALL").is_ok();
    let rpc_error = std::env::var("MCP_MOCK_RPC_ERROR").is_ok();
    let exit_after = std::env::var("MCP_MOCK_EXIT_AFTER").is_ok();

    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(line) => line,
            Err(_) => break,
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let request: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(value) => value,
            Err(_) => continue,
        };
        // 通知(无 id)只接收不回复。
        let Some(id) = request.get("id") else {
            continue;
        };
        if exit_after {
            // 不回复直接退出 → 客户端 read_line 读到 0 → StreamEnded。
            std::process::exit(0);
        }
        let method = request
            .get("method")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let response = match method {
            "initialize" => {
                let result = serde_json::json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "mock-mcp", "version": "1.0"}
                });
                serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result})
            }
            "tools/list" if rpc_error => serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32601, "message": "method not found"}
            }),
            "tools/list" => {
                let result = serde_json::json!({
                    "tools": [
                        {
                            "name": "echo",
                            "description": "echo input",
                            "inputSchema": {
                                "type": "object",
                                "properties": {"text": {"type": "string"}},
                                "required": ["text"]
                            }
                        },
                        {
                            "name": "add",
                            "description": "add two numbers",
                            "inputSchema": {
                                "type": "object",
                                "properties": {"a": {"type": "number"}, "b": {"type": "number"}},
                                "required": ["a", "b"]
                            }
                        },
                        {
                            // 工具名本身含下划线,验证 McpTool 注册时保存原始名、
                            // 不再从 mcp__<server>_<tool> 反解。
                            "name": "search_repos",
                            "description": "search repositories",
                            "inputSchema": {
                                "type": "object",
                                "properties": {"query": {"type": "string"}},
                                "required": ["query"]
                            }
                        }
                    ]
                });
                serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result})
            }
            "tools/call" if fail_call => serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "content": [{"type": "text", "text": "boom"}],
                    "isError": true
                }
            }),
            "tools/call" => {
                let name = request
                    .get("params")
                    .and_then(|p| p.get("name"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                let args = request
                    .get("params")
                    .and_then(|p| p.get("arguments"))
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                let result = match name {
                    "echo" => serde_json::json!({
                        "content": [{"type": "text", "text": format!(
                            "echo:{}",
                            args.get("text").and_then(serde_json::Value::as_str).unwrap_or("")
                        )}]
                    }),
                    "add" => {
                        let a = args.get("a").and_then(serde_json::Value::as_f64).unwrap_or(0.0);
                        let b = args.get("b").and_then(serde_json::Value::as_f64).unwrap_or(0.0);
                        serde_json::json!({"content": [{"type": "text", "text": format!("{}", a + b)}]})
                    }
                    "search_repos" => serde_json::json!({
                        "content": [{"type": "text", "text": format!(
                            "repos:{}",
                            args.get("query").and_then(serde_json::Value::as_str).unwrap_or("")
                        )}]
                    }),
                    _ => serde_json::json!({"content": [], "isError": true}),
                };
                serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result})
            }
            _ => serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {}}),
        };
        let mut line = response.to_string();
        line.push('\n');
        if out.write_all(line.as_bytes()).is_err() {
            break;
        }
        if out.flush().is_err() {
            break;
        }
    }
}
