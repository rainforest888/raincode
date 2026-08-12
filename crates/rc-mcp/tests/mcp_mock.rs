//! stdio mock MCP 服务器的集成测试。
//!
//! 必须在 tests/ 目录:Cargo 只在 integration test / bench 构建时注入
//! `CARGO_BIN_EXE_mcp_mock`(lib 单元测试里 env! 编译期未定义)。
//! 行为由 mcp_mock bin 的环境变量切换(见 crates/rc-mcp/src/bin/mcp_mock.rs)。

use std::collections::BTreeMap;

use rc_mcp::{McpClient, McpError, McpManager, McpServerConfig};
use rc_tool::Tool;
use serde_json::json;

/// Rust stdio mock MCP 服务器二进制(cargo 为集成测试注入的绝对路径)。
const MOCK_BIN: &str = env!("CARGO_BIN_EXE_mcp_mock");

fn mock_stdio_config() -> McpServerConfig {
    mock_stdio_config_with_env(BTreeMap::new())
}

fn mock_stdio_config_with_env(env: BTreeMap<String, String>) -> McpServerConfig {
    McpServerConfig {
        kind: "stdio".into(),
        command: Some(MOCK_BIN.into()),
        args: vec![],
        url: None,
        headers: Default::default(),
        env,
    }
}

#[tokio::test]
async fn stdio_mock_preserves_underscores_in_server_and_tool_names() {
    // 回归:旧实现运行时从 mcp__<server>_<tool> 反解 raw name,
    // 服务器名或工具名本身含下划线时会切错;新实现注册时显式保存 server/tool_name。
    let mut configs: BTreeMap<String, McpServerConfig> = BTreeMap::new();
    configs.insert("my_server".into(), mock_stdio_config());
    let manager = McpManager::connect_all(&configs).await.unwrap();

    assert_eq!(manager.servers, vec!["my_server".to_string()]);
    let search = manager
        .tools
        .iter()
        .find(|t| t.tool_name() == "search_repos")
        .expect("mock exposes search_repos");
    assert_eq!(search.server_name(), "my_server");
    assert_eq!(search.tool_name(), "search_repos");
    assert_eq!(search.spec().name, "mcp__my_server_search_repos");

    let result = search
        .call("search_repos", json!({"query": "axum"}))
        .await
        .unwrap();
    assert_eq!(result, json!("repos:axum"));
}

#[tokio::test]
async fn connect_all_skips_slow_server_instead_of_aborting_all() {
    // 一个服务器 connect 失败(指向关闭端口的 HTTP),另一个成功(Rust mock)。
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
    let names: Vec<String> = manager.tools.iter().map(|t| t.spec().name).collect();
    assert_eq!(
        names,
        vec![
            "mcp__good_echo".to_string(),
            "mcp__good_add".to_string(),
            "mcp__good_search_repos".to_string(),
        ]
    );
}

#[tokio::test]
async fn stdio_mcp_client_lists_and_calls_tools() {
    let client = McpClient::connect(mock_stdio_config()).await.unwrap();
    let tools = client.tools().await.unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, vec!["echo", "add", "search_repos"]);
    assert_eq!(tools[0].description, "echo input");

    let echo = client.call("echo", json!({"text": "hello"})).await.unwrap();
    assert_eq!(echo, json!("echo:hello"));

    let add = client.call("add", json!({"a": 2.0, "b": 3.0})).await.unwrap();
    assert_eq!(add, json!("5"));
}

#[tokio::test]
async fn stdio_mock_tool_error_surfaces_as_tool_failure() {
    // MCP_MOCK_FAIL_CALL=1:tools/call 返回 isError: true → McpError::Tool。
    let mut env = BTreeMap::new();
    env.insert("MCP_MOCK_FAIL_CALL".into(), "1".into());
    let client = McpClient::connect(mock_stdio_config_with_env(env))
        .await
        .unwrap();
    let err = client
        .call("echo", json!({"text": "hi"}))
        .await
        .unwrap_err();
    assert!(matches!(&err, McpError::Tool(msg) if msg.contains("boom")), "{err}");
}

#[tokio::test]
async fn stdio_mock_rpc_error_surfaces_on_tools_list() {
    // MCP_MOCK_RPC_ERROR=1:tools/list 返回 JSON-RPC error → McpError::Rpc。
    let mut env = BTreeMap::new();
    env.insert("MCP_MOCK_RPC_ERROR".into(), "1".into());
    let client = McpClient::connect(mock_stdio_config_with_env(env))
        .await
        .unwrap();
    let err = client.tools().await.unwrap_err();
    assert!(matches!(err, McpError::Rpc(_)), "{err}");
}

#[tokio::test]
async fn stdio_mock_exit_after_first_request_ends_stream() {
    // MCP_MOCK_EXIT_AFTER=1:收到 initialize 后不回复直接退出 → StreamEnded。
    let mut env = BTreeMap::new();
    env.insert("MCP_MOCK_EXIT_AFTER".into(), "1".into());
    let err = McpClient::connect(mock_stdio_config_with_env(env))
        .await
        .unwrap_err();
    assert!(matches!(err, McpError::StreamEnded), "{err}");
}
