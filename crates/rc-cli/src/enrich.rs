//! Agentic model-library enrichment for `raincode profiles enrich`.
//! Deterministic OpenRouter baseline + one-shot LLM research agent (web tools) +
//! merge that keeps latest versions / deletes old / upserts / prints a summary.

use anyhow::Result;
use futures::StreamExt;
use rc_core::{Agent, AgentConfig};
use rc_net::{tools::network_tools, SearchConfig};
use rc_profile::Registry;
use rc_proto::AgentEvent;
use rc_sandbox::{AutoApproveHook, AutoUserHook, CommandPolicy, NetworkPolicy};
use rc_skill::SkillStore;
use rc_state::{CapabilityProfileRow, Store};
use rc_tool::builtin::default_tools;
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

/// One merged output row for the final library (one per target bare-name model).
/// Capability scores, per-1M pricing and context window come from the research
/// report when it names them, else from the OpenRouter baseline row, else a
/// synthesized default.
#[derive(Debug, Clone)]
pub struct EnrichedRow {
    pub model: String,
    pub reasoning: f64,
    pub coding: f64,
    pub frontend: f64,
    pub backend: f64,
    pub math: f64,
    pub long_context: f64,
    pub input_cost_per_m: f64,
    pub output_cost_per_m: f64,
    pub context_window: u32,
    pub price_source: String,
}

/// Strip provider prefix and version suffix from a model id:
/// `deepseek/deepseek-v4-flash-0731` -> `deepseek-v4-flash`,
/// `qwen/qwen3.8-max` -> `qwen3.8-max` (trailing `-max` is not a version).
pub fn bare_name(model_id: &str) -> String {
    let last = model_id.rsplit('/').next().unwrap_or(model_id);
    match last.rfind('-') {
        Some(i)
            if i > 0 && last[i + 1..].chars().all(|c| c.is_ascii_digit() || c == '.') =>
        {
            last[..i].to_string()
        }
        _ => last.to_string(),
    }
}

/// Does this DB/model row belong to the given bare-name group?
/// e.g. `deepseek/deepseek-v4-flash-0731` matches `deepseek-v4-flash`;
/// `deepseek-v4-flashx` does not (`x` is not a dash-joined version).
fn matches_bare(row_model: &str, bare: &str) -> bool {
    let last = row_model.rsplit('/').next().unwrap_or(row_model);
    last == bare || last.strip_prefix(bare).is_some_and(|rest| rest.starts_with('-'))
}

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

/// 研究 agent 提示词构建:给定目标裸名列表与榜单 top N,产出 JSON 输出契约。
/// 纯函数(可测);真实调用在 enrich_command。
pub fn build_research_prompt(targets: &[String], top: usize) -> String {
    format!(
        "You are researching model pricing and benchmark scores for a coding-agent model pool.\n\
         Anchor sources: LMArena leaderboard (popularity), OpenRouter /api/v1/models (Artificial-Analysis \
         + Design-Arena scores), Artificial Analysis, and the opencode.ai pricing page.\n\
         Steps:\n\
         1) Identify the top {} most-used models from the external popularity leaderboard (LMArena votes; \
         fall back to OpenRouter's popular list if unreachable).\n\
         2) For each, determine the LATEST version id and its pricing: prefer the opencode.ai price you can \
         find on the web; if not found set \"price_source\": \"missing\".\n\
         3) If a model is missing frontend/backend scores, fill estimates from Artificial Analysis or the \
         official model page.\n\
         Extra models to research too: {}.\n\
         Return your final answer as a single JSON object (no prose before/after): {{\"popular\":[...], \
         \"models\":[{{\"id\":\"<latest version id>\",\"latest\":true,\"input_cost_per_m\":<num or null>,\
         \"output_cost_per_m\":<num or null>,\"price_source\":\"opencode|openrouter|missing\",\
         \"reasoning\":<0-100 or null>,\"coding\":<0-100 or null>,\"frontend\":<0-100 or null>,\
         \"backend\":<0-100 or null>,\"math\":<0-100 or null>,\"long_context\":<0-100 or null>,\
         \"context_window\":<num>}}]}}",
        top,
        if targets.is_empty() { "(none)".to_string() } else { targets.join(", ") },
    )
}

/// 一次性研究子代理:装配 default_tools(去掉 delegate_research 防递归派发)+
/// 网络工具(web_fetch/web_search),跑研究提示词并返回最终文本(应含 JSON)。
/// 交互定价兜底发生在 agent 返回之后的 enrich_command,因此这里用
/// AutoUserHook 不请求用户输入。守卫照 agent_config 工厂镜像加载 supervise.toml;
/// 无 guard_hook → 高危操作保守拦截(研究 agent 只读网络)。
async fn run_research_agent(
    config: &FileConfig,
    registry: &Registry,
    model_id: Option<&str>,
    prompt: &str,
) -> Result<String> {
    let provider = crate::provider_for_profile(registry, model_id)?;
    let skill_dir = crate::skills_dir(config);
    let store = Store::open(crate::state_path())?;
    let skill_store = SkillStore::new(&skill_dir);
    let cwd = config
        .core
        .workspace
        .as_deref()
        .map(crate::expand_tilde)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| ".".into()));

    let mut tools = default_tools(skill_store.clone());
    tools.retain(|t| t.spec().name != "delegate_research");
    tools.extend(network_tools(SearchConfig::default()));

    // 监督守卫:从 ~/.raincode/supervise.toml 加载(缺失默认守卫全开;坏 TOML 记
    // warn 并关闭守卫)。与 main.rs agent_config 工厂同源。
    let guard_cfg = match rc_sandbox::load_supervise_config(&crate::raincode_home()) {
        Ok(cfg) => Some(cfg),
        Err(e) => {
            tracing::warn!("supervise.toml 解析失败,守卫关闭: {e}");
            None
        }
    };
    let guard_memo = guard_cfg
        .as_ref()
        .map(|_| std::sync::Arc::new(rc_sandbox::guard_hook::SessionGuardMemo::default()));
    let guard_home = guard_cfg.as_ref().map(|_| crate::raincode_home());

    let session = store.create_session(&cwd.to_string_lossy())?;
    let cfg = AgentConfig {
        provider,
        plan_provider: None,
        review_provider: None,
        store,
        skill_store,
        tools,
        approval: std::sync::Arc::new(AutoApproveHook),
        command_policy: CommandPolicy::default(),
        network_policy: NetworkPolicy::default(),
        cwd,
        state_path: crate::state_path(),
        max_turns: 8,
        max_steps: 0,
        evolve_on_finish: false,
        plan_mode: false,
        hooks: config.hooks.clone(),
        agent: Some("researcher".into()),
        max_history_bytes: Some(128 * 1024),
        mcp_servers: vec![],
        entropy_mode: false,
        plan_max_rounds: 1,
        plan_max_questions: 1,
        review_max_rounds: 1,
        max_cycles: 1,
        user_input: std::sync::Arc::new(AutoUserHook::default()),
        steer_rx: None,
        context_window: 0,
        subagent: None,
        guard_cfg,
        guard_hook: None,
        guard_memo,
        guard_home,
    };
    let agent = Agent::new(cfg);
    let mut stream = agent.run(session.id, prompt.to_string());
    let mut final_text = String::new();
    while let Some(ev) = stream.next().await {
        match ev {
            AgentEvent::Token { delta } => final_text.push_str(&delta),
            AgentEvent::Done { summary, .. } => final_text = summary,
            AgentEvent::Error { message } => {
                return Err(anyhow::anyhow!("research agent error: {message}"))
            }
            _ => {}
        }
    }
    Ok(final_text.trim().to_string())
}

/// 最终汇总表:每行一个目标模型,打印能力分、每百万 token 输入价与上下文。
fn print_summary(rows: &[EnrichedRow]) {
    println!("model | reasoning | coding | frontend | backend | $/M in | ctx | price_src");
    for r in rows {
        println!(
            "{:<38} {:>7.1} {:>6.1} {:>7.1} {:>6.1} {:>9.4} {:>8} {}",
            r.model, r.reasoning, r.coding, r.frontend, r.backend,
            r.input_cost_per_m, r.context_window, r.price_source
        );
    }
}

/// For models the agent couldn't price ("missing"), ask the user interactively;
/// non-interactive (or empty/invalid answer) falls back to the baseline
/// OpenRouter price. `user_input_hook` returns `Arc<dyn rc_sandbox::UserInputHook>`
/// whose single method is `async fn ask(&self, question: &str) -> String`.
async fn resolve_missing_prices(
    rows: &mut [EnrichedRow],
    baseline: &[CapabilityProfileRow],
    interactive: bool,
) -> Result<()> {
    let hook = crate::user_input_hook(interactive);
    for row in rows.iter_mut() {
        if row.price_source != "missing" {
            continue;
        }
        let fallback = baseline
            .iter()
            .find(|b| bare_name(&b.model) == bare_name(&row.model))
            .map(|b| b.input_cost_per_m);
        let question = format!(
            "模型 {} 的价格没能在网上找到。请你自己搜一下它的每百万 token 输入价格($/M)，\
             直接输入数字(如 0.5)，或回车用 OpenRouter 基准价({:?})",
            row.model, fallback
        );
        let answer = hook.ask(&question).await.trim().to_string();
        if let Ok(p) = answer.parse::<f64>() {
            row.input_cost_per_m = p;
            row.price_source = "user".into();
        } else if let Some(f) = fallback {
            row.input_cost_per_m = f;
            row.price_source = "openrouter".into();
        }
    }
    Ok(())
}

/// `profiles enrich` 完整编排:确定性 OpenRouter 基准 → 一次性研究 agent →
/// 合并 + 交互定价兜底 → 写库(或 dry-run) → 汇总打印。
pub async fn enrich_command(config: &FileConfig, args: EnrichArgs) -> Result<()> {
    let registry = crate::load_registry()?;
    let store = Store::open(crate::state_path())?;

    // 1) deterministic baseline
    println!("[enrich] fetching OpenRouter baseline ...");
    let baseline = fetch_openrouter_baseline().await?;
    println!("[enrich] baseline: {} models", baseline.len());

    // 2) research agent
    let prompt = build_research_prompt(&args.add, args.top);
    println!(
        "[enrich] research agent researching top {} + {} extra ...",
        args.top,
        args.add.len()
    );
    let raw = run_research_agent(config, &registry, args.model.as_deref(), &prompt).await?;
    let report = parse_agent_output(&raw)?;
    println!(
        "[enrich] report: popular={} researched={}",
        report.popular.len(),
        report.models.len()
    );

    // 3) merge + interactive pricing fallback
    let mut final_rows = build_final_rows(&baseline, &report, &args.add, args.top);
    resolve_missing_prices(&mut final_rows, &baseline, !args.dry_run).await?;

    // 4) apply (or dry-run) + summary
    if args.dry_run {
        println!("[enrich] DRY-RUN: would write {} rows", final_rows.len());
    } else {
        apply_enrichment(&store, &final_rows, false).await?;
        println!("[enrich] wrote {} rows", final_rows.len());
    }
    print_summary(&final_rows);
    Ok(())
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

/// Pure merge: given the OpenRouter baseline rows, the research report, and the
/// requested `--add` models, produce the final rows to write. One `EnrichedRow`
/// per target bare name; for each target prefer the research report's chosen id
/// (and its pricing/capabilities), else the baseline row for that bare name
/// (scores kept), else a synthesized default (70s, cost 1.0/3.0, price openrouter).
pub fn build_final_rows(
    baseline: &[CapabilityProfileRow],
    report: &ResearchReport,
    add: &[String],
    top: usize,
) -> Vec<EnrichedRow> {
    use std::collections::BTreeMap;
    // Target bare names: report.popular then --add, order preserved, deduped.
    let mut targets: Vec<String> = Vec::new();
    for bare in report.popular.iter().chain(add.iter()) {
        if !targets.contains(bare) {
            targets.push(bare.clone());
        }
    }
    if targets.len() > top {
        targets.truncate(top);
    }
    // baseline row per bare name (last row wins on duplicate versions).
    let by_bare: BTreeMap<String, &CapabilityProfileRow> = baseline
        .iter()
        .map(|r| (bare_name(&r.model), r))
        .collect();

    let mut out = Vec::new();
    for bare in targets {
        // Prefer the research report's id for this bare name (its latest-flagged
        // entry, else the first match); else any baseline row; else synthesize.
        let researched = report
            .models
            .iter()
            .filter(|m| bare_name(&m.id) == bare)
            .min_by_key(|m| if m.latest == Some(true) { 0 } else { 1 });
        let base = by_bare.get(&bare).copied();
        out.push(EnrichedRow {
            model: researched
                .map(|m| m.id.clone())
                .or_else(|| base.map(|b| b.model.clone()))
                .unwrap_or_else(|| bare.clone()),
            reasoning: researched
                .and_then(|m| m.reasoning)
                .or_else(|| base.map(|b| b.reasoning))
                .unwrap_or(70.0),
            coding: researched
                .and_then(|m| m.coding)
                .or_else(|| base.map(|b| b.coding))
                .unwrap_or(70.0),
            frontend: researched
                .and_then(|m| m.frontend)
                .or_else(|| base.map(|b| b.frontend))
                .unwrap_or(70.0),
            backend: researched
                .and_then(|m| m.backend)
                .or_else(|| base.map(|b| b.backend))
                .unwrap_or(70.0),
            math: researched
                .and_then(|m| m.math)
                .or_else(|| base.map(|b| b.math))
                .unwrap_or(70.0),
            long_context: researched
                .and_then(|m| m.long_context)
                .or_else(|| base.map(|b| b.long_context))
                .unwrap_or(100.0),
            input_cost_per_m: researched
                .and_then(|m| m.input_cost_per_m)
                .or_else(|| base.map(|b| b.input_cost_per_m))
                .unwrap_or(1.0),
            output_cost_per_m: researched
                .and_then(|m| m.output_cost_per_m)
                .or_else(|| base.map(|b| b.output_cost_per_m))
                .unwrap_or(3.0),
            context_window: researched
                .and_then(|m| m.context_window)
                .or_else(|| base.map(|b| b.context_window))
                .unwrap_or(128_000),
            price_source: researched
                .map(|m| m.price_source.clone())
                .unwrap_or_else(|| "openrouter".into()),
        });
    }
    out
}

/// Apply enrichment to the store: for each final row, delete every existing DB
/// row in the same bare-name group except this one (keep-latest / delete-old),
/// then upsert. No writes happen when `dry_run`.
pub async fn apply_enrichment(
    store: &Store,
    final_rows: &[EnrichedRow],
    dry_run: bool,
) -> Result<()> {
    if dry_run {
        return Ok(());
    }
    let baseline = store.all_model_profiles()?;
    for row in final_rows {
        let bare = bare_name(&row.model);
        for e in &baseline {
            if e.model != row.model && matches_bare(&e.model, &bare) {
                store.delete_model_profile(&e.model)?;
            }
        }
        store.upsert_model_profile(&CapabilityProfileRow {
            model: row.model.clone(),
            reasoning: row.reasoning,
            coding: row.coding,
            frontend: row.frontend,
            backend: row.backend,
            math: row.math,
            long_context: row.long_context.min(100.0),
            input_cost_per_m: row.input_cost_per_m,
            output_cost_per_m: row.output_cost_per_m,
            context_window: row.context_window,
            source: "enrich".into(),
            updated_at: "now".into(),
            multimodal: false,
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_research_prompt_mentions_top_and_targets() {
        let p = build_research_prompt(&["glm-5.2".into()], 15);
        assert!(p.contains("top 15"));
        assert!(p.contains("glm-5.2"));
        assert!(p.contains("openrouter"));
    }

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

    #[test]
    fn bare_name_strips_provider_and_version() {
        assert_eq!(bare_name("deepseek/deepseek-v4-flash-0731"), "deepseek-v4-flash");
        assert_eq!(bare_name("qwen/qwen3.8-max"), "qwen3.8-max");
        assert_eq!(bare_name("deepseek-v4-flash"), "deepseek-v4-flash");
    }

    #[test]
    fn matches_bare_finds_version_variants() {
        assert!(matches_bare("deepseek/deepseek-v4-flash-0731", "deepseek-v4-flash"));
        assert!(matches_bare("deepseek/deepseek-v4-flash", "deepseek-v4-flash"));
        assert!(!matches_bare("qwen/qwen3.8-max", "deepseek-v4-flash"));
    }

    #[test]
    fn build_final_rows_uses_research_id_and_pricing() {
        let baseline = vec![rc_state::CapabilityProfileRow {
            model: "deepseek/deepseek-v4-flash-0731".into(),
            reasoning: 51.8, coding: 69.1, frontend: 65.2, backend: 65.0,
            math: 51.8, long_context: 90.0,
            input_cost_per_m: 0.08, output_cost_per_m: 0.18, context_window: 1048576,
            source: "openrouter-arena".into(), updated_at: "now".into(), multimodal: false,
        }];
        let report = ResearchReport {
            popular: vec!["deepseek-v4-flash".into()],
            models: vec![ResearchedModel {
                id: "deepseek/deepseek-v4-flash-0731".into(), latest: Some(true),
                reasoning: None, coding: None, frontend: None, backend: None, math: None,
                long_context: None, input_cost_per_m: Some(0.05), output_cost_per_m: Some(0.1),
                context_window: None, price_source: "opencode".into(),
            }],
        };
        let rows = build_final_rows(&baseline, &report, &[], 15);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].input_cost_per_m, 0.05); // agent's opencode price wins
        assert_eq!(rows[0].coding, 69.1);            // baseline score kept
        assert_eq!(rows[0].price_source, "opencode");
    }

    #[test]
    fn build_final_rows_synthesizes_default_for_unknown_model() {
        let report = ResearchReport {
            popular: vec!["brand-new-model".into()],
            models: vec![],
        };
        let rows = build_final_rows(&[], &report, &[], 15);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].model, "brand-new-model");
        assert_eq!(rows[0].reasoning, 70.0);
        assert_eq!(rows[0].input_cost_per_m, 1.0);
        assert_eq!(rows[0].price_source, "openrouter");
    }

    #[tokio::test]
    async fn apply_enrichment_deletes_old_version_keeps_latest() {
        let store = Store::open_in_memory().unwrap();
        let insert = |model: &str| {
            store
                .upsert_model_profile(&rc_state::CapabilityProfileRow {
                    model: model.into(), reasoning: 50.0, coding: 50.0, frontend: 50.0,
                    backend: 50.0, math: 50.0, long_context: 50.0,
                    input_cost_per_m: 0.1, output_cost_per_m: 0.2, context_window: 1000,
                    source: "test".into(), updated_at: "now".into(), multimodal: false,
                })
                .unwrap()
        };
        insert("deepseek/deepseek-v4-flash-0721");
        insert("deepseek/deepseek-v4-flash-0731");
        insert("qwen/qwen3.8-max");

        let report = ResearchReport {
            popular: vec!["deepseek-v4-flash".into()],
            models: vec![ResearchedModel {
                id: "deepseek/deepseek-v4-flash-0731".into(), latest: Some(true),
                reasoning: None, coding: None, frontend: None, backend: None, math: None,
                long_context: None, input_cost_per_m: None, output_cost_per_m: None,
                context_window: None, price_source: "missing".into(),
            }],
        };
        let baseline = store.all_model_profiles().unwrap();
        let final_rows = build_final_rows(&baseline, &report, &[], 15);
        assert_eq!(final_rows.len(), 1);
        assert_eq!(final_rows[0].model, "deepseek/deepseek-v4-flash-0731");
        apply_enrichment(&store, &final_rows, false).await.unwrap();

        let mut remaining: Vec<String> = store
            .all_model_profiles()
            .unwrap()
            .into_iter()
            .map(|r| r.model)
            .collect();
        remaining.sort();
        assert_eq!(
            remaining,
            vec!["deepseek/deepseek-v4-flash-0731", "qwen/qwen3.8-max"]
        );
    }

    #[tokio::test]
    async fn apply_enrichment_dry_run_writes_nothing() {
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_model_profile(&rc_state::CapabilityProfileRow {
                model: "deepseek/deepseek-v4-flash-0721".into(),
                reasoning: 50.0, coding: 50.0, frontend: 50.0, backend: 50.0,
                math: 50.0, long_context: 50.0,
                input_cost_per_m: 0.1, output_cost_per_m: 0.2, context_window: 1000,
                source: "test".into(), updated_at: "now".into(), multimodal: false,
            })
            .unwrap();

        let report = ResearchReport {
            popular: vec!["deepseek-v4-flash".into()],
            models: vec![],
        };
        let baseline = store.all_model_profiles().unwrap();
        let final_rows = build_final_rows(&baseline, &report, &[], 15);
        assert_eq!(final_rows.len(), 1);
        assert_eq!(final_rows[0].model, "deepseek/deepseek-v4-flash-0721");
        apply_enrichment(&store, &final_rows, true).await.unwrap();

        let remaining = store.all_model_profiles().unwrap();
        assert_eq!(remaining.len(), 1, "dry_run must not delete or upsert");
        assert_eq!(remaining[0].model, "deepseek/deepseek-v4-flash-0721");
    }

    #[test]
    fn print_summary_prints_a_header_and_rows() {
        let rows = vec![EnrichedRow {
            model: "deepseek/deepseek-v4-flash-0731".into(),
            reasoning: 80.0, coding: 90.0, frontend: 70.0, backend: 75.0,
            math: 80.0, long_context: 90.0,
            input_cost_per_m: 0.08, output_cost_per_m: 0.18, context_window: 1048576,
            price_source: "opencode".into(),
        }];
        // Smoke test: must not panic for one row.
        print_summary(&rows);
    }
}
