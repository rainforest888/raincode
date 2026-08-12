# `raincode profiles enrich` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `raincode profiles enrich` — a hybrid command that refreshes the model score library (`~/.raincode/state.db` `model_profiles`) with real web-sourced pricing and benchmark scores for the top-N most-used models plus user-added ones.

**Architecture:** Deterministic OpenRouter fetch (baseline) → one-shot LLM research agent (active model + web tools) returns a JSON report (popularity list, per-model latest version / opencode pricing / score gap-fills) → command merges baseline+report, keeps only latest versions, deletes old ones, upserts, prints a before/after table. Interactive pricing fallback: models the agent couldn't price are prompted to the user (`user_input_hook`); non-interactive falls back to the OpenRouter baseline price.

**Tech Stack:** Rust, reqwest (HTTP), serde/serde_json (parsing), rusqlite (SQLite via `rc-state`), existing `rc-core::Agent` + `rc-net::network_tools` (web tools).

## Global Constraints

- Workspace: `G:\claude codex_workspace\raincode-iter` (this is the iteration copy; the two copies are iterated in tandem).
- `crates/rc-cli` owns CLI commands; `ProfilesCmd` already has `Show` / `Refresh` — `Enrich` joins them.
- `model` is the PRIMARY KEY of `model_profiles`; upserts use `ON CONFLICT(model) DO UPDATE`.
- OpenRouter `/api/v1/models` pricing fields are **$ per token**; the DB stores **$ per 1M tokens** → multiply by `1e6`.
- Score dims (0-100): reasoning/coding/math from `artificial_analysis` indices; frontend/backend from `design_arena` Elo (`normalize_arena_elo`: elo 1000→0, 1400→100, clamp 0-100); `long_context` = `context_length/128_000` clamped 0-100.
- All new code in `crates/rc-cli/src/enrich.rs`; keep it self-contained (no changes to `rc-router`, `rc-pro`).
- No emojis in comments/output (project rule).

---
---

## Task 1: Fix pricing unit bug in `parse_openrouter_models`

**Files:**
- Modify: `crates/rc-cli/src/main.rs` (the `parse_openrouter_models` fn, ~lines 2248-2285)
- Test: inline `#[cfg(test)]` mod in `main.rs` (existing pattern), or new `crates/rc-cli/tests/` — use inline mod next to the fn.

**Interfaces:**
- Produces: `parse_openrouter_models(raw: &str) -> Result<Vec<rc_state::CapabilityProfileRow>, serde_json::Error>` with `input_cost_per_m` / `output_cost_per_m` now in real $/1M (×1e6).

- [ ] **Step 1: Write the failing test**

Add to the existing `#[cfg(test)] mod tests` in `main.rs` (near the existing `parse_openrouter_models_extracts_pricing_and_context` test):

```rust
#[test]
fn parse_openrouter_models_scales_per_token_to_per_million() {
    // OpenRouter pricing is $/token; the DB stores $/1M tokens. deepseek 0.00000008/token
    // = $0.08/M, output 0.00000018/token = $0.18/M.
    let raw = r#"{"data":[{"id":"deepseek/deepseek-v4-flash-0731","context_length":1048576,
        "pricing":{"prompt":"0.00000008","completion":"0.00000018"},
        "benchmarks":{},"description":"x"}]}"#;
    let rows = parse_openrouter_models(raw).unwrap();
    assert_eq!(rows[0].input_cost_per_m, 0.08);
    assert_eq!(rows[0].output_cost_per_m, 0.18);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p rc-cli parse_openrouter_models_scales_per_token_to_per_million`
Expected: FAIL — currently `input_cost_per_m` is `0.00000008` clamped to `0.0001`.

- [ ] **Step 3: Fix the multiplication**

In `parse_openrouter_models`, change the pricing lines:

```rust
let inp = m.pricing.prompt.parse::<f64>().unwrap_or(0.0) * 1_000_000.0;
let outp = m.pricing.completion.parse::<f64>().unwrap_or(0.0) * 1_000_000.0;
```

(`input_cost_per_m: inp.max(0.0001)` and `output_cost_per_m: outp.max(0.0001)` already clamp — leave them.)

- [ ] **Step 4: Run the test + full rc-cli tests**

Run: `cargo test -p rc-cli`
Expected: PASS (all rc-cli tests including the new one; the existing pricing test may need its expected value updated to the ×1e6 scale — check `parse_openrouter_models_extracts_pricing_and_context` and update its assertions to real $/M).

- [ ] **Step 5: Commit**

```bash
git add crates/rc-cli/src/main.rs
git commit -m "fix(rc-cli): OpenRouter pricing per-token -> per-1M (x1e6), so cost_efficiency is real"
```

---
---

## Task 2: Add `delete_model_profile` to rc-state

**Files:**
- Modify: `crates/rc-state/src/db.rs` (near `upsert_model_profile` ~line 576)
- Test: inline `#[cfg(test)]` in `db.rs` (pattern already there: `model_profile_roundtrip`).

**Interfaces:**
- Produces: `Store::delete_model_profile(&self, model: &str) -> Result<(), DbError>` — deletes a row by primary key.

- [ ] **Step 1: Write the failing test**

In `db.rs` `#[cfg(test)] mod tests`, add:

```rust
#[test]
fn delete_model_profile_removes_row() {
    let store = Store::open_in_memory().unwrap();
    let mut p = CapabilityProfileRow::default();
    p.model = "qwen/qwen3.8-max".into();
    store.upsert_model_profile(&p).unwrap();
    assert!(store.get_model_profile("qwen/qwen3.8-max").unwrap().is_some());
    store.delete_model_profile("qwen/qwen3.8-max").unwrap();
    assert!(store.get_model_profile("qwen/qwen3.8-max").unwrap().is_none());
}
```

(If `CapabilityProfileRow` has no `Default` impl, construct it with all fields set — check the `model_profile_roundtrip` test for the exact construction.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p rc-state delete_model_profile_removes_row`
Expected: FAIL — no such method.

- [ ] **Step 3: Implement**

```rust
pub fn delete_model_profile(&self, model: &str) -> Result<(), DbError> {
    self.conn.execute("DELETE FROM model_profiles WHERE model = ?1", [model])?;
    Ok(())
}
```

- [ ] **Step 4: Run test**

Run: `cargo test -p rc-state`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/rc-state/src/db.rs
git commit -m "feat(rc-state): delete_model_profile for enrich keep-latest/delete-old"
```

---
---

## Task 3: CLI skeleton — `ProfilesCmd::Enrich` + `enrich.rs` module

**Files:**
- Modify: `crates/rc-cli/src/main.rs` (module decl, `ProfilesCmd` enum, `profiles_command` dispatch)
- Create: `crates/rc-cli/src/enrich.rs` (stub with the pub entry fn returning an error for now)

**Interfaces:**
- Consumes: existing `FileConfig`, `Store`, `Registry` types from rc-cli.
- Produces: `pub async fn enrich_command(config: &FileConfig, args: EnrichArgs) -> anyhow::Result<()>` and `pub struct EnrichArgs { pub add: Vec<String>, pub top: usize, pub model: Option<String>, pub dry_run: bool }`.

- [ ] **Step 1: Add module + enum variant**

In `main.rs`:
- Add `mod enrich;` near the other `mod` declarations.
- Extend `ProfilesCmd` (line ~467):

```rust
/// Agentic refresh: research top-N most-used models' pricing/scores from the web and
/// enrich the model score library (keep latest versions, delete old).
Enrich {
    /// Comma-separated extra models to research beyond the popularity top-N (e.g. glm-5.2,kimi-k3).
    #[arg(long)]
    add: Option<String>,
    /// How many models to take from the external popularity leaderboard (default 15).
    #[arg(long)]
    top: Option<usize>,
    /// Provider profile id for the research agent (default: active profile).
    #[arg(long)]
    model: Option<String>,
    /// Print what would change without writing to the DB.
    #[arg(long)]
    dry_run: bool,
},
```

- [ ] **Step 2: Dispatch in `profiles_command`**

In `profiles_command` (line ~2293), add the arm:

```rust
ProfilesCmd::Enrich { add, top, model, dry_run } => {
    let add: Vec<String> = add
        .as_deref()
        .map(|s| s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect())
        .unwrap_or_default();
    enrich::enrich_command(config, enrich::EnrichArgs {
        add,
        top: top.unwrap_or(15),
        model,
        dry_run,
    }).await?;
}
```

- [ ] **Step 3: Create `enrich.rs` stub**

```rust
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
```

- [ ] **Step 4: Verify it compiles and CLI shows the flag**

Run: `cargo build -p rc-cli`
Run: `cargo run -p rc-cli -- profiles enrich --help`
Expected: compiles; help shows `--add`, `--top`, `--model`, `--dry-run`.

- [ ] **Step 5: Commit**

```bash
git add crates/rc-cli/src/main.rs crates/rc-cli/src/enrich.rs
git commit -m "feat(rc-cli): profiles enrich CLI skeleton (--add/--top/--model/--dry-run)"
```

---
---

## Task 4: `ResearchReport` schema + JSON parser (agent output)

**Files:**
- Modify: `crates/rc-cli/src/enrich.rs`

**Interfaces:**
- Produces: `#[derive(Debug, Deserialize)] pub struct ResearchReport { pub popular: Vec<String>, pub models: Vec<ResearchedModel> }`, `ResearchedModel` (fields below), `pub fn parse_agent_output(raw: &str) -> Result<ResearchReport>`, and a private `extract_json_object(text: &str) -> Option<String>`.

- [ ] **Step 1: Write the failing test**

In `enrich.rs` `#[cfg(test)] mod tests`:

```rust
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
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p rc-cli parse_agent_output`
Expected: FAIL — `parse_agent_output` doesn't exist yet.

- [ ] **Step 3: Implement the schema + parser**

```rust
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
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p rc-cli parse_agent_output`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/rc-cli/src/enrich.rs
git commit -m "feat(rc-cli): ResearchReport schema + agent JSON parser"
```

---
---

## Task 5: OpenRouter baseline fetch

**Files:**
- Modify: `crates/rc-cli/src/enrich.rs`

**Interfaces:**
- Consumes: `parse_openrouter_models` (from Task 1, now ×1e6 correct) — expose it as `pub(crate) fn parse_openrouter_models` or call it directly (same crate, it's already `fn` in `main.rs`; enrich.rs can call `crate::parse_openrouter_models` if it's `pub(crate)`).
- Produces: `async fn fetch_openrouter_baseline() -> Result<Vec<rc_state::CapabilityProfileRow>>` (raw OpenRouter JSON → rows). Also `fn row_to_capability(row: &CapabilityProfileRow) -> CapabilityProfile` if needed by dispatch (skip if not needed by later tasks).

- [ ] **Step 1: Make `parse_openrouter_models` reachable**

In `main.rs`, change `fn parse_openrouter_models` → `pub(crate) fn parse_openrouter_models`. (It's currently private to `main.rs`.)

- [ ] **Step 2: Write the failing test**

In `enrich.rs` tests:

```rust
#[tokio::test]
async fn baseline_scales_pricing_and_keeps_scores() {
    // Serve a canned OpenRouter payload via a local HTTP listener (tiny HTTP server on 127.0.0.1:0).
    // Simpler alternative: refactor fetch to accept a URL, then point at a `test_server` that
    // returns fixed JSON. Implement the URL-injection first, then this test.
    let json = r#"{"data":[{"id":"qwen/qwen3.8-max","context_length":1000000,
        "pricing":{"prompt":"0.000002","completion":"0.000006"},
        "benchmarks":{"artificial_analysis":{"intelligence_index":58.1,"coding_index":71.8},
                      "design_arena":[{"category":"website","elo":1295}]}}]}"#;
    let rows = fetch_openrouter_baseline_from(json).await.unwrap();
    assert_eq!(rows[0].input_cost_per_m, 2.0);      // 0.000002 * 1e6
    assert_eq!(rows[0].coding, 71.8);
    assert!((rows[0].frontend - 73.75).abs() < 0.01); // (1295-1000)/400*100
}
```

To make this testable, **inject the raw JSON**: split `fetch_openrouter_baseline()` into `fetch_openrouter_raw() -> Result<String>` (real network) and `fn baseline_from_json(raw: &str) -> Result<Vec<CapabilityProfileRow>>` (pure, calls `parse_openrouter_models`). Test `baseline_from_json` directly; `fetch_openrouter_raw` is a thin reqwest wrapper.

- [ ] **Step 3: Implement**

```rust
const OPENROUTER_MODELS_URL: &str = "https://openrouter.ai/api/v1/models";

async fn fetch_openrouter_raw() -> Result<String> {
    let resp = reqwest::Client::new()
        .get(OPENROUTER_MODELS_URL)
        .send()
        .await?
        .error_for_status()?;
    Ok(resp.text().await?)
}

fn baseline_from_json(raw: &str) -> Result<Vec<rc_state::CapabilityProfileRow>> {
    Ok(crate::parse_openrouter_models(raw)?)
}

async fn fetch_openrouter_baseline() -> Result<Vec<rc_state::CapabilityProfileRow>> {
    baseline_from_json(&fetch_openrouter_raw().await?)
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p rc-cli baseline_`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/rc-cli/src/main.rs crates/rc-cli/src/enrich.rs
git commit -m "feat(rc-cli): OpenRouter baseline fetch with real pricing + scores"
```

---
---

## Task 6: Bare-name grouping + keep-latest/delete-old merge

**Files:**
- Modify: `crates/rc-cli/src/enrich.rs`

**Interfaces:**
- Consumes: `CapabilityProfileRow` (rc-state), `ResearchReport` (Task 4), `Store`.
- Produces:
  - `pub fn bare_name(model_id: &str) -> String` — last `/`-segment, version suffix stripped (e.g. `deepseek/deepseek-v4-flash-0731` → `deepseek-v4-flash`).
  - `fn matches_bare(row_model: &str, bare: &str) -> bool` — row id's bare name equals `bare` (used for delete-old).
  - `async fn apply_enrichment(store: &Store, report: &ResearchReport, add: &[String], top: usize, dry_run: bool) -> Result<Vec<EnrichedRow>>` — returns the rows it would write (for printing), performs upserts/delete-old unless `dry_run`.
  - `pub struct EnrichedRow { pub model: String, pub reasoning: f64, pub coding: f64, pub frontend: f64, pub backend: f64, pub input_cost_per_m: f64, pub output_cost_per_m: f64, pub context_window: u32, pub price_source: String }`

- [ ] **Step 1: Write the failing tests**

```rust
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
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p rc-cli bare_name`
Expected: FAIL.

- [ ] **Step 3: Implement grouping helpers**

```rust
pub fn bare_name(model_id: &str) -> String {
    let last = model_id.rsplit('/').next().unwrap_or(model_id);
    // strip a "-<version>" suffix: `deepseek-v4-flash-0731` -> `deepseek-v4-flash`.
    // Find the last "-" that follows a digit-less tail pattern (heuristic: keep the
    // longest prefix that still appears as a model on its own, or simply cut at the
    // last dash if the remainder is all digits/dots and the prefix is non-trivial).
    // Conservative: if last contains a '-' and the suffix after the final '-' is
    // digits-and-dots, strip it.
    match last.rfind('-') {
        Some(i) if last[i + 1..].chars().all(|c| c.is_ascii_digit() || c == '.') && i > 0 => {
            last[..i].to_string()
        }
        _ => last.to_string(),
    }
}

fn matches_bare(row_model: &str, bare: &str) -> bool {
    let last = row_model.rsplit('/').next().unwrap_or(row_model);
    last == bare || last.strip_prefix(bare).is_some_and(|rest| rest.starts_with('-'))
}
```

- [ ] **Step 4: Implement `apply_enrichment` (pure merge logic, DB-agnostic core first)**

Write the merge as a pure function first for testability:

```rust
/// Pure merge: given baseline rows (all OpenRouter), the research report, and the
/// requested add-models, produce the final rows to write (one per target model).
pub fn build_final_rows(
    baseline: &[rc_state::CapabilityProfileRow],
    report: &ResearchReport,
    add: &[String],
    top: usize,
) -> Vec<EnrichedRow> {
    use std::collections::BTreeMap;
    // target bare names, order preserved
    let mut targets: Vec<String> = Vec::new();
    for bare in report.popular.iter().chain(add.iter()) {
        if !targets.contains(bare) {
            targets.push(bare.clone());
        }
    }
    if targets.len() > top {
        targets.truncate(top);
    }
    let by_bare: BTreeMap<String, &rc_state::CapabilityProfileRow> = baseline
        .iter()
        .filter(|r| matches_bare(&r.model, r.model.rsplit('/').next().unwrap_or(&r.model)))
        .map(|r| (bare_name(&r.model), r))
        .collect();

    let mut out = Vec::new();
    for bare in targets {
        // prefer the research report's chosen id; else any baseline row for the bare name
        let researched = report.models.iter().find(|m| bare_name(&m.id) == bare);
        let base = by_bare.get(&bare).copied();
        let row = EnrichedRow {
            model: researched
                .map(|m| m.id.clone())
                .or_else(|| base.map(|b| b.model.clone()))
                .unwrap_or_else(|| bare.clone()),
            reasoning: researched.and_then(|m| m.reasoning).or_else(|| base.map(|b| b.reasoning)).unwrap_or(70.0),
            coding: researched.and_then(|m| m.coding).or_else(|| base.map(|b| b.coding)).unwrap_or(70.0),
            frontend: researched.and_then(|m| m.frontend).or_else(|| base.map(|b| b.frontend)).unwrap_or(70.0),
            backend: researched.and_then(|m| m.backend).or_else(|| base.map(|b| b.backend)).unwrap_or(70.0),
            input_cost_per_m: researched.and_then(|m| m.input_cost_per_m).or_else(|| base.map(|b| b.input_cost_per_m)).unwrap_or(1.0),
            output_cost_per_m: researched.and_then(|m| m.output_cost_per_m).or_else(|| base.map(|b| b.output_cost_per_m)).unwrap_or(3.0),
            context_window: researched.and_then(|m| m.context_window).or_else(|| base.map(|b| b.context_window)).unwrap_or(128_000),
            price_source: researched.map(|m| m.price_source.clone()).unwrap_or_else(|| "openrouter".into()),
        };
        out.push(row);
    }
    out
}
```

Note: this keeps ONE row per target bare name (the researched latest id). The delete-old step then removes any OTHER DB row in the same bare-name group.

- [ ] **Step 5: Test the pure merge**

```rust
#[test]
fn build_final_rows_uses_research_id_and_pricing() {
    let baseline = vec![rc_state::CapabilityProfileRow {
        model: "deepseek/deepseek-v4-flash-0731".into(), reasoning: 51.8, coding: 69.1,
        frontend: 65.2, backend: 65.0, math: 51.8, long_context: 90.0,
        input_cost_per_m: 0.08, output_cost_per_m: 0.18, context_window: 1048576,
        source: "openrouter-arena".into(), updated_at: "now".into(), multimodal: 0,
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
```

(Construct the `CapabilityProfileRow` literal to match rc-state's actual fields — verify against the `parse_openrouter_models` construction in Task 1.)

- [ ] **Step 6: Implement the DB write + delete-old wrapper**

```rust
async fn apply_enrichment(
    store: &Store,
    final_rows: &[EnrichedRow],
    dry_run: bool,
) -> Result<()> {
    let existing = store.all_model_profiles()?;
    for row in final_rows {
        if !dry_run {
            // delete every existing row in the same bare-name group except this one
            for e in &existing {
                if e.model != row.model && matches_bare(&e.model, &bare_name(&row.model)) {
                    store.delete_model_profile(&e.model)?;
                }
            }
            store.upsert_model_profile(&rc_state::CapabilityProfileRow {
                model: row.model.clone(), reasoning: row.reasoning, coding: row.coding,
                frontend: row.frontend, backend: row.backend, math: row.reasoning,
                long_context: row.long_context.min(100.0), input_cost_per_m: row.input_cost_per_m,
                output_cost_per_m: row.output_cost_per_m, context_window: row.context_window,
                source: "enrich".into(), updated_at: "now".into(), multimodal: 0,
            })?;
        }
    }
    Ok(())
}
```

Add a test with an in-memory `Store::open_in_memory()` (if rc-state exposes one; else open a temp file DB via `tempfile` + `Store::open(path)`) that inserts two versions of a bare name, runs `apply_enrichment`, and asserts only the kept row remains.

- [ ] **Step 7: Run all enrich tests**

Run: `cargo test -p rc-cli enrich::`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add crates/rc-cli/src/enrich.rs
git commit -m "feat(rc-cli): bare-name grouping + keep-latest/delete-old enrichment merge"
```

---
---

## Task 7: Research agent runner (one-shot, web tools, final text)

**Files:**
- Modify: `crates/rc-cli/src/enrich.rs`

**Interfaces:**
- Consumes: `provider_for_profile` (main.rs), `default_tools`, `network_tools`, `SearchConfig`, `AgentConfig`, `Agent`, `Store`, `rc_core::AgentEvent`.
- Produces: `async fn run_research_agent(config: &FileConfig, registry: &Registry, model_id: Option<&str>, prompt: &str) -> Result<String>` — returns the agent's final text (JSON).

- [ ] **Step 1: Write the prompt builder + its test**

```rust
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
```

Test:

```rust
#[test]
fn build_research_prompt_mentions_top_and_targets() {
    let p = build_research_prompt(&["glm-5.2".into()], 15);
    assert!(p.contains("top 15"));
    assert!(p.contains("glm-5.2"));
    assert!(p.contains("openrouter"));
}
```

- [ ] **Step 2: Implement the agent runner (mirror the existing researcher subagent factory)**

```rust
async fn run_research_agent(
    config: &FileConfig,
    registry: &Registry,
    model_id: Option<&str>,
    prompt: &str,
) -> Result<String> {
    use rc_core::{Agent, AgentConfig, AgentEvent};
    use rc_net::{SearchConfig, tools::network_tools};
    use rc_sandbox::{CommandPolicy, NetworkPolicy};

    let provider = crate::provider_for_profile(registry, model_id)?;
    let skill_dir = crate::skills_dir(config);
    let store = Store::open(crate::state_path())?;
    let skill_store = crate::rc_skill_store(&skill_dir); // adjust to the existing helper name used in main.rs
    let cwd = config.core.workspace.as_deref().map(crate::expand_tilde)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| ".".into()));

    let mut tools = crate::default_tools(skill_store.clone());
    tools.retain(|t| t.spec().name != "delegate_research");
    tools.extend(network_tools(SearchConfig::default()));

    let session = store.create_session(&cwd.to_string_lossy())?;
    let cfg = AgentConfig {
        provider,
        plan_provider: None,
        review_provider: None,
        store,
        skill_store,
        tools,
        approval: std::sync::Arc::new(rc_sandbox::AutoApproveHook),
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
        user_input: std::sync::Arc::new(rc_sandbox::AutoUserHook::default()),
        steer_rx: None,
        context_window: 0,
        subagent: None,
        guard_cfg: None,
        guard_hook: None,
        guard_memo: None,
        guard_home: None,
    };
    let agent = Agent::new(cfg);
    let mut stream = agent.run(session.id, prompt.to_string());
    let mut final_text = String::new();
    while let Some(ev) = stream.next().await {
        match ev {
            AgentEvent::Token { delta } => final_text.push_str(&delta),
            AgentEvent::Done { summary, .. } => final_text = summary,
            AgentEvent::Error { message } => return Err(anyhow::anyhow!("research agent error: {message}")),
            _ => {}
        }
    }
    Ok(final_text.trim().to_string())
}
```

Verify the exact helper names in main.rs (`skills_dir`, `default_tools`, `state_path`, `expand_tilde`) before finalizing — adjust imports to whatever exists. If `guard_cfg`/`guard_hook` fields are required (they may be `Option`), set them like the existing factory does (load `supervise.toml`). The existing subagent factory at main.rs:1360-1445 is the authoritative reference — copy its field set.

- [ ] **Step 3: Compile check**

Run: `cargo build -p rc-cli`
Expected: compiles. If `AgentConfig` has more fields, copy them from the main.rs factory.

- [ ] **Step 4: Commit**

```bash
git add crates/rc-cli/src/enrich.rs
git commit -m "feat(rc-cli): one-shot research agent runner with web tools for enrich"
```

---
---

## Task 8: `enrich_command` orchestration + interactive pricing fallback + summary

**Files:**
- Modify: `crates/rc-cli/src/enrich.rs`

**Interfaces:**
- Consumes: everything from Tasks 4-7, plus `crate::user_input_hook(interactive)` and `crate::load_registry`.
- Produces: complete `enrich_command`.

- [ ] **Step 1: Write the summary printer + test**

```rust
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
```

- [ ] **Step 2: Interactive pricing fallback**

```rust
/// For models the agent couldn't price ("missing"), ask the user interactively;
/// non-interactive (or empty answer) falls back to the baseline OpenRouter price.
async fn resolve_missing_prices(
    rows: &mut [EnrichedRow],
    baseline: &[rc_state::CapabilityProfileRow],
    interactive: bool,
) -> Result<()> {
    let hook = crate::user_input_hook(interactive);
    for row in rows.iter_mut() {
        if row.price_source != "missing" {
            continue;
        }
        let fallback = baseline.iter().find(|b| bare_name(&b.model) == bare_name(&row.model))
            .map(|b| b.input_cost_per_m);
        let question = format!(
            "模型 {} 的价格没能在网上找到。请你自己搜一下它的每百万 token 输入价格($/M)，\
             直接输入数字(如 0.5)，或回车用 OpenRouter 基准价({:?})",
            row.model, fallback
        );
        let answer = hook.ask(&question).await.unwrap_or_default().trim().to_string();
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
```

(The `user_input_hook` returns `Arc<dyn UserInputHook>`; check its exact trait method name (`ask` vs `ask_user`) in main.rs / rc-sandbox before finalizing. The interactive prompt writes to stderr and reads a line from stdin.)

- [ ] **Step 3: Implement `enrich_command`**

```rust
pub async fn enrich_command(config: &FileConfig, args: EnrichArgs) -> Result<()> {
    let registry = crate::load_registry()?;
    let store = Store::open(crate::state_path())?;

    // 1) deterministic baseline
    println!("[enrich] fetching OpenRouter baseline ...");
    let baseline = fetch_openrouter_baseline().await?;
    println!("[enrich] baseline: {} models", baseline.len());

    // 2) research agent
    let prompt = build_research_prompt(&args.add, args.top);
    println!("[enrich] research agent researching top {} + {} extra ...", args.top, args.add.len());
    let raw = run_research_agent(config, &registry, args.model.as_deref(), &prompt).await?;
    let report = parse_agent_output(&raw)?;
    println!("[enrich] report: popular={} researched={}", report.popular.len(), report.models.len());

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
```

- [ ] **Step 4: Build + fix compile issues**

Run: `cargo build -p rc-cli`
Expected: compiles. Resolve any trait/field mismatches against the real types.

- [ ] **Step 5: Run unit tests**

Run: `cargo test -p rc-cli`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/rc-cli/src/enrich.rs
git commit -m "feat(rc-cli): profiles enrich orchestration + interactive pricing fallback + summary"
```

---
---

## Task 9: Manual verification (real network + real models)

**Files:** none (verification only).

- [ ] **Step 1: Dry-run against the real pool**

From `G:\claude codex_workspace\raincode-iter`:
Run: `cargo run -p rc-cli -- profiles enrich --top 5 --dry-run`
Expected: prints baseline count, research report summary, and a 5-row table; NO writes (verify `model_profiles` unchanged).

- [ ] **Step 2: Real run**

Run: `cargo run -p rc-cli -- profiles enrich --top 5 --add glm-5.2,kimi-k3`
Expected: baseline fetched, research agent runs (uses the active model via `~/.raincode/bin` config), interactive prompts appear for any "missing" pricing, rows upserted, old versions deleted, summary printed.

- [ ] **Step 3: Verify library + routing**

Run: `cargo run -p rc-cli -- profiles show | head`
Expected: researched models present with `source=enrich`; old version rows gone.
Then run `raincode route "<decomposable task>" --plan-only` to confirm dispatch still works with the enriched rows.

- [ ] **Step 4: Report results to the user** (models enriched, prices, scores, any fallbacks hit).

---
---

## Self-Review Notes (checked)

- **Spec coverage:** §2 command/args → Task 3; §4 stage1 baseline → Tasks 1+5; stage2 LLM research → Tasks 4+7; stage3 merge/delete/upsert/summary → Tasks 6+8; §5 pricing fallback (opencode → ask_user → openrouter) → Task 8; §6 error handling (no partial write on bad JSON → Task 4 error path; unreachable sources degrade → Task 5 `.error_for_status()` + fallback in prompt) ; §8 pricing bug fix → Task 1.
- **Placeholder scan:** no TBD/TODO; all code blocks complete.
- **Type consistency:** `EnrichArgs`, `ResearchReport`, `ResearchedModel`, `EnrichedRow`, `build_final_rows`, `apply_enrichment`, `run_research_agent`, `parse_agent_output`, `fetch_openrouter_baseline`, `bare_name`, `matches_bare`, `resolve_missing_prices` are used consistently across tasks.
- **Deferred verification:** exact `AgentConfig` field set, `user_input_hook` trait method name, `Store::open_in_memory` availability, and `rc_state::CapabilityProfileRow` literal shape must be checked against main.rs/db.rs during Task 7/8 (the existing researcher factory at main.rs:1360-1445 is the authoritative reference).
