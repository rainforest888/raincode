//! rc-net tools adapted to Raincode's `Tool` trait. The network policy in
//! `ToolContext` is consulted by the same sandbox that gates shell tools.

use async_trait::async_trait;
use rc_tool::{Tool, ToolContext, ToolResult, ToolSpec};
use serde_json::{json, Value};

use crate::{fetch_url, search, SearchConfig};

pub fn network_tools(search_config: SearchConfig) -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(WebFetchTool),
        Box::new(WebSearchTool::new(search_config)),
    ]
}

pub struct WebFetchTool;

#[async_trait]
impl Tool for WebFetchTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "web_fetch".into(),
            description:
                "Fetch a URL and return its text content, respecting the sandbox network policy."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {"url": {"type": "string"}},
                "required": ["url"]
            }),
        }
    }

    async fn run(&self, args: Value, ctx: &ToolContext) -> ToolResult {
        let Some(url) = args.get("url").and_then(Value::as_str) else {
            return ToolResult::err("missing 'url'");
        };
        match fetch_url(url, &ctx.network_policy).await {
            Ok(result) => {
                let mut output = result.markdown;
                if let Some(title) = result.title {
                    if !title.is_empty() {
                        output = format!("# {title}\n\n{output}");
                    }
                }
                ToolResult::ok(output)
            }
            Err(e) => ToolResult::err(e.to_string()),
        }
    }
}

pub struct WebSearchTool {
    config: SearchConfig,
}

impl WebSearchTool {
    pub fn new(config: SearchConfig) -> Self {
        Self { config }
    }
}

#[async_trait]
impl Tool for WebSearchTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "web_search".into(),
            description: "Search the web and return title, URL and snippet hits.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {"query": {"type": "string"}},
                "required": ["query"]
            }),
        }
    }

    async fn run(&self, args: Value, ctx: &ToolContext) -> ToolResult {
        let Some(query) = args.get("query").and_then(Value::as_str) else {
            return ToolResult::err("missing 'query'");
        };
        match search(query, &ctx.network_policy, &self.config).await {
            Ok(hits) => {
                let lines: Vec<String> = hits
                    .iter()
                    .map(|hit| format!("- {}\n  {}\n  {}", hit.title, hit.url, hit.snippet))
                    .collect();
                if lines.is_empty() {
                    ToolResult::ok("no search results")
                } else {
                    ToolResult::ok(lines.join("\n"))
                }
            }
            Err(e) => ToolResult::err(e.to_string()),
        }
    }
}
