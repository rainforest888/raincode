//! Agentic model-library enrichment for `raincode profiles enrich`.
//! Deterministic OpenRouter baseline + one-shot LLM research agent (web tools) +
//! merge that keeps latest versions / deletes old / upserts / prints a summary.

use anyhow::Result;
use crate::FileConfig;

pub struct EnrichArgs {
    pub add: Vec<String>,
    pub top: usize,
    pub model: Option<String>,
    pub dry_run: bool,
}

pub async fn enrich_command(_config: &FileConfig, _args: EnrichArgs) -> Result<()> {
    Err(anyhow::anyhow!("enrich: not implemented yet"))
}
