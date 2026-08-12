//! Agentic model-library enrichment for `raincode profiles enrich`.
//! Deterministic OpenRouter baseline + one-shot LLM research agent (web tools) +
//! merge that keeps latest versions / deletes old / upserts / prints a summary.

use anyhow::Result;
use serde::Deserialize;
use crate::FileConfig;

/// OpenRouter 公开模型列表端点(无需 key)。返回 data 数组,每项含 id、
/// context_length、pricing(prompt/completion 每 token 单价)与 benchmarks。
const OPENROUTER_MODELS_URL: &str = "https://openrouter.ai/api/v1/models";

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

/// 拉取 OpenRouter /api/v1/models 原始 JSON(薄 reqwest 包装,可测部分在
/// [`baseline_from_json`])。
async fn fetch_openrouter_raw() -> Result<String> {
    let resp = reqwest::Client::new()
        .get(OPENROUTER_MODELS_URL)
        .send()
        .await?
        .error_for_status()?;
    Ok(resp.text().await?)
}

/// 纯函数:OpenRouter 原始 JSON → 能力档案行。定价 ×1e6(每 token → 每百万),
/// 榜单分按人工分析/design-arena 归一化,详情见 `parse_openrouter_models`。
fn baseline_from_json(raw: &str) -> Result<Vec<rc_state::CapabilityProfileRow>> {
    Ok(crate::parse_openrouter_models(raw)?)
}

/// 确定性基准:拉取 OpenRouter 模型列表并转成能力档案行(不命中网络的可测
/// 核心是 `baseline_from_json`,此处仅补一次 HTTP)。
async fn fetch_openrouter_baseline() -> Result<Vec<rc_state::CapabilityProfileRow>> {
    baseline_from_json(&fetch_openrouter_raw().await?)
}

#[cfg(test)]
mod tests {
    use super::{baseline_from_json, parse_agent_output};

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

    #[test]
    fn baseline_scales_pricing_and_keeps_scores() {
        // Canned OpenRouter payload: pricing is per-token (0.000002/0.000006),
        // so baseline must scale ×1e6 to per-1M; scores pass through normalized.
        let json = r#"{"data":[{"id":"qwen/qwen3.8-max","context_length":1000000,
            "pricing":{"prompt":"0.000002","completion":"0.000006"},
            "benchmarks":{"artificial_analysis":{"intelligence_index":58.1,"coding_index":71.8},
                          "design_arena":[{"category":"website","elo":1295}]}}]}"#;
        let rows = baseline_from_json(json).unwrap();
        assert_eq!(rows[0].input_cost_per_m, 2.0);       // 0.000002 * 1e6
        assert_eq!(rows[0].coding, 71.8);
        assert!((rows[0].frontend - 73.75).abs() < 0.01); // (1295-1000)/400*100
    }
}
