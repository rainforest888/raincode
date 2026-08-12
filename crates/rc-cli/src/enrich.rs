//! Agentic model-library enrichment for `raincode profiles enrich`.
//! Deterministic OpenRouter baseline + one-shot LLM research agent (web tools) +
//! merge that keeps latest versions / deletes old / upserts / prints a summary.

use anyhow::Result;
use serde::Deserialize;
use crate::FileConfig;

pub struct EnrichArgs {
    pub add: Vec<String>,
    pub top: usize,
    pub model: Option<String>,
    pub dry_run: bool,
}

/// Structured output from the research agent: popular bare-name models plus a
/// per-model detail table (id, capabilities, pricing, context).
#[derive(Debug, Deserialize)]
pub struct ResearchReport {
    pub popular: Vec<String>,
    #[serde(default)]
    pub models: Vec<ResearchedModel>,
}

#[derive(Debug, Deserialize)]
pub struct ResearchedModel {
    pub id: String,
    #[serde(default)]
    pub latest: Option<bool>,
    #[serde(default)]
    pub reasoning: Option<f64>,
    #[serde(default)]
    pub coding: Option<f64>,
    #[serde(default)]
    pub frontend: Option<f64>,
    #[serde(default)]
    pub backend: Option<f64>,
    #[serde(default)]
    pub math: Option<f64>,
    #[serde(default)]
    pub long_context: Option<f64>,
    #[serde(default)]
    pub input_cost_per_m: Option<f64>,
    #[serde(default)]
    pub output_cost_per_m: Option<f64>,
    #[serde(default)]
    pub context_window: Option<u32>,
    #[serde(default = "default_price_source")]
    pub price_source: String, // "opencode" | "openrouter" | "missing"
}

fn default_price_source() -> String { "missing".to_string() }

/// Extract the first balanced JSON object from mixed text (handles reasoning noise
/// around a code-fenced JSON block). Bracket-matching skips strings; returns the
/// pretty-printed object only if it parses.
fn extract_json_object(text: &str) -> Option<String> {
    let mut search_from = 0;
    while let Some(rel_start) = text[search_from..].find('{') {
        let start = search_from + rel_start;
        let rest = &text[start..];
        let mut depth = 0i32;
        let mut in_str = false;
        let mut escape = false;
        let mut end = None;
        for (i, c) in rest.char_indices() {
            if in_str {
                if escape { escape = false; }
                else if c == '\\' { escape = true; }
                else if c == '"' { in_str = false; }
                continue;
            }
            match c {
                '"' => in_str = true,
                '{' => depth += 1,
                '}' => { depth -= 1; if depth == 0 { end = Some(i); break; } }
                _ => {}
            }
        }
        if let Some(e) = end {
            let candidate = &text[start..=start + e];
            if serde_json::from_str::<serde_json::Value>(candidate).is_ok() {
                return Some(candidate.to_string());
            }
        }
        search_from = start + 1;
    }
    None
}

pub fn parse_agent_output(raw: &str) -> Result<ResearchReport> {
    let json = extract_json_object(raw).ok_or_else(|| anyhow::anyhow!("no JSON object in research agent output"))?;
    Ok(serde_json::from_str(&json)?)
}

pub async fn enrich_command(_config: &FileConfig, _args: EnrichArgs) -> Result<()> {
    Err(anyhow::anyhow!("enrich: not implemented yet"))
}

#[cfg(test)]
mod tests {
    use super::parse_agent_output;

    #[test]
    fn parse_agent_output_accepts_fenced_json() {
        let raw = "研究完成。\n```json\n{\"popular\":[\"deepseek-v4-flash\"],\
            \"models\":[{\"id\":\"deepseek-v4-flash-0731\",\"latest\":true,\
            \"input_cost_per_m\":0.08,\"output_cost_per_m\":0.18,\"price_source\":\"opencode\"}]}\n```\n结束";
        let r = parse_agent_output(raw).unwrap();
        assert_eq!(r.popular, vec!["deepseek-v4-flash"]);
        assert_eq!(r.models[0].id, "deepseek-v4-flash-0731");
        assert_eq!(r.models[0].price_source, "opencode");
    }

    #[test]
    fn parse_agent_output_rejects_no_json() {
        assert!(parse_agent_output("完全没有 JSON").is_err());
    }
}
