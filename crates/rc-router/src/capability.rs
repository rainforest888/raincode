use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CostPressure { #[default] Low, Med, High }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Risk { #[default] Low, Med, High }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Requirements {
    #[serde(default)] pub reasoning: f64,
    #[serde(default)] pub coding: f64,
    #[serde(default)] pub frontend: f64,
    #[serde(default)] pub backend: f64,
    #[serde(default)] pub math: f64,
    #[serde(default)] pub long_context: f64,
    #[serde(default)] pub vision: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityProfile {
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
    pub provenance: String,
    #[serde(default)]
    pub multimodal: bool,
}

impl CapabilityProfile {
    pub fn from_row(r: rc_state::CapabilityProfileRow) -> Self {
        Self {
            model: r.model, reasoning: r.reasoning, coding: r.coding, frontend: r.frontend,
            backend: r.backend, math: r.math, long_context: r.long_context,
            input_cost_per_m: r.input_cost_per_m, output_cost_per_m: r.output_cost_per_m,
            context_window: r.context_window, provenance: r.source, multimodal: r.multimodal,
        }
    }
    /// 成本效率:efficiency = (cheapest_input / input_cost)^γ,越便宜越高
    pub fn cost_efficiency(&self, cheapest_input: f64, gamma: f64) -> f64 {
        if cheapest_input <= 0.0 { return 1.0; }
        (cheapest_input / self.input_cost_per_m.max(0.0001)).powf(gamma)
    }
    /// 能力分:加权和 Σ w_i × P_i
    pub fn capability_score(&self, req: &Requirements) -> f64 {
        req.reasoning * self.reasoning + req.coding * self.coding + req.frontend * self.frontend
            + req.backend * self.backend + req.math * self.math + req.long_context * self.long_context
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Subtask {
    pub id: String,
    pub description: String,
    #[serde(default)] pub requirements: Requirements,
    #[serde(default)] pub cost_pressure: CostPressure,
    #[serde(default)] pub depends_on: Vec<String>,
    #[serde(default)] pub risk: Risk,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubtaskGraph {
    pub intent: String,
    #[serde(default)] pub subtasks: Vec<Subtask>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DispatchEntry {
    pub subtask_id: String,
    pub model: String,
    pub capability: f64,
    pub efficiency: f64,
    pub score: f64,
    pub escalated: bool,
}

/// γ 由 cost_pressure 调制:high→1.0 med→0.7 low→0.2
pub fn gamma_for(pressure: CostPressure) -> f64 {
    match pressure { CostPressure::High => 1.0, CostPressure::Med => 0.7, CostPressure::Low => 0.2 }
}

/// 能力容差带(分数,0-100 标尺):两个候选能力差 ≤ 带 → 视为同级,同级内比性价比。
/// 成本压力越高带越宽(更愿用便宜模型),压力低则带窄(能力优先)。
pub fn capability_band(pressure: CostPressure) -> f64 {
    match pressure {
        CostPressure::High => 12.0,
        CostPressure::Med => 7.0,
        CostPressure::Low => 3.0,
    }
}

/// 确定性派活:每个子任务 → **能力优先**排序(能力差 > 带 → 高能力胜;带内 → 便宜胜),
/// 能力低于 FIT_THRESHOLD → 升级给最强模型。
pub fn dispatch(subtasks: &[Subtask], profiles: &[CapabilityProfile], fit_threshold: f64, gamma_override: Option<f64>) -> Vec<DispatchEntry> {
    if profiles.is_empty() { return Vec::new(); }
    let cheapest = profiles.iter()
        .map(|p| p.input_cost_per_m).fold(f64::INFINITY, f64::min);
    // "最强模型"用等权中性需求(Σ=1/6)评定,避免全 0 权重下 everyone 得 0
    let neutral = Requirements {
        reasoning: 1.0 / 6.0, coding: 1.0 / 6.0, frontend: 1.0 / 6.0,
        backend: 1.0 / 6.0, math: 1.0 / 6.0, long_context: 1.0 / 6.0,
        ..Default::default()
    };
    let strongest = profiles.iter()
        .max_by(|a, b| a.capability_score(&neutral).total_cmp(&b.capability_score(&neutral)))
        .map(|p| p.model.clone());

    subtasks.iter().map(|s| {
        let gamma = gamma_override.unwrap_or_else(|| gamma_for(s.cost_pressure));
        // 收集全部候选。
        let mut candidates: Vec<DispatchEntry> = profiles.iter().map(|p| {
            let capability = p.capability_score(&s.requirements);
            let efficiency = p.cost_efficiency(cheapest, gamma);
            let score = capability * efficiency;
            DispatchEntry {
                subtask_id: s.id.clone(), model: p.model.clone(),
                capability, efficiency, score, escalated: false,
            }
        }).collect();
        // 能力优先:以最高能力为"带锚点",锚点带内(capability ≥ max_cap - band)
        // 比性价比(效率高/便宜者胜),带外候选不参与竞争。逐对比较(差>带→强胜;
        // 差≤带→廉胜)在 ≥3 候选跨带时构成非传递环(排序结果实现相关),弃用;
        // 锚点算法是总序、确定性的,且等价于文档意图"cap gap > band → 强胜;带内 → 廉胜"。
        let band = capability_band(s.cost_pressure);
        let max_cap = candidates
            .iter()
            .map(|c| c.capability)
            .fold(f64::NEG_INFINITY, f64::max);
        candidates.sort_by(|a, b| {
            let a_in = a.capability >= max_cap - band;
            let b_in = b.capability >= max_cap - band;
            match (a_in, b_in) {
                // 带内候选排前,带内比效率(便宜胜),效率相同再比能力,仍同则比模型名。
                (true, true) => b
                    .efficiency
                    .total_cmp(&a.efficiency)
                    .then_with(|| b.capability.total_cmp(&a.capability))
                    .then_with(|| a.model.cmp(&b.model)),
                // 带外候选排后,带外比能力(接近锚点者靠前)。
                (false, false) => b
                    .capability
                    .total_cmp(&a.capability)
                    .then_with(|| b.efficiency.total_cmp(&a.efficiency))
                    .then_with(|| a.model.cmp(&b.model)),
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
            }
        });
        let mut entry = candidates.swap_remove(0);
        // 能力优先下 best 通常即最强;仅当连它都低于 fit_threshold 时,仍升级给最强兜底。
        if entry.capability < fit_threshold {
            if let Some(model) = &strongest {
                if model != &entry.model {
                    let strong = profiles
                        .iter()
                        .find(|p| &p.model == model)
                        .expect("strongest is derived from profiles");
                    let capability = strong.capability_score(&s.requirements);
                    let efficiency = strong.cost_efficiency(cheapest, gamma);
                    entry.model = model.clone();
                    entry.capability = capability;
                    entry.efficiency = efficiency;
                    entry.score = capability * efficiency;
                    entry.escalated = true;
                }
            }
        }
        entry
    }).collect()
}

/// 解析分配者输出:剥代码围栏 → 提取 JSON 对象 → SubtaskGraph
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("no JSON block in allocator output: {0}")]
    NoJson(String),
    #[error("json parse failed: {0}")]
    Json(#[from] serde_json::Error),
}

/// 从混合文本里提取第一个可解析的 JSON 对象:逐 `{` 做括号配平(跳过字符串内的
/// 括号),能解析的才返回。推理模型的 reasoning + content 混在一起时,rfind('}')
/// 会把尾部杂文带进来导致 trailing characters;这个版本稳健。
fn extract_json_block(text: &str) -> Option<String> {
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
                if escape {
                    escape = false;
                } else if c == '\\' {
                    escape = true;
                } else if c == '"' {
                    in_str = false;
                }
                continue;
            }
            match c {
                '"' => in_str = true,
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(i);
                        break;
                    }
                }
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

/// 解析分配者输出:剥代码围栏 → 提取 JSON 对象 → SubtaskGraph
pub fn parse_subtask_graph(raw: &str) -> Result<SubtaskGraph, ParseError> {
    let text = raw.trim();
    let text = text.strip_prefix("```json").or_else(|| text.strip_prefix("```")).unwrap_or(text);
    let text = text.strip_suffix("```").unwrap_or(text).trim();
    let json = extract_json_block(text).ok_or_else(|| ParseError::NoJson(raw.to_string()))?;
    Ok(serde_json::from_str(&json)?)
}

/// 内置种子画像:空库时兜底,让 `raincode route` 无需 profiles refresh 也能跑通闭环
pub fn seed_profiles() -> Vec<CapabilityProfile> {
    let m = |model: &str, reasoning: f64, coding: f64, frontend: f64, backend: f64,
             math: f64, long_context: f64, inp: f64, outp: f64, window: u32| CapabilityProfile {
        model: model.into(), reasoning, coding, frontend, backend, math, long_context,
        input_cost_per_m: inp, output_cost_per_m: outp, context_window: window,
        provenance: "seed".into(), multimodal: false,
    };
    vec![
        m("deepseek-v4", 85.0, 92.0, 80.0, 95.0, 88.0, 90.0, 0.1, 0.3, 128_000),
        m("kimi-k3", 82.0, 84.0, 88.0, 80.0, 80.0, 92.0, 0.05, 0.2, 128_000),
        m("qwen3.8-max", 80.0, 86.0, 90.0, 82.0, 85.0, 90.0, 0.04, 0.16, 128_000),
        m("gpt-5.6-luna", 97.0, 95.0, 90.0, 96.0, 97.0, 95.0, 10.0, 30.0, 400_000),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(model: &str, reasoning: f64, coding: f64, frontend: f64, backend: f64, cost: f64) -> CapabilityProfile {
        CapabilityProfile {
            model: model.into(),
            reasoning, coding, frontend, backend, math: 70.0, long_context: 70.0,
            input_cost_per_m: cost, output_cost_per_m: cost * 3.0,
            context_window: 128_000, provenance: "seed".into(), multimodal: false,
        }
    }

    #[test]
    fn frontend_subtask_goes_to_frontend_strong_cheap_model() {
        // 验收场景 1:前端子任务 → 前端强且便宜的模型
        let profiles = vec![
            profile("expensive", 95.0, 95.0, 97.0, 95.0, 10.0),
            profile("cheap", 70.0, 85.0, 92.0, 80.0, 0.3),
        ];
        let subtasks = vec![Subtask {
            id: "s1".into(),
            description: "build react page".into(),
            requirements: Requirements { reasoning: 0.1, coding: 0.3, frontend: 0.5, backend: 0.1, math: 0.0, long_context: 0.0, ..Default::default() },
            cost_pressure: CostPressure::High,
            depends_on: vec![],
            risk: Risk::Low,
        }];
        let table = dispatch(&subtasks, &profiles, 60.0, None);
        assert_eq!(table[0].model, "cheap");
    }

    #[test]
    fn reasoning_task_consistently_picks_reasoning_strong() {
        // 验收场景 2:同类推理任务稳定选推理强模型
        let profiles = vec![
            profile("reasoner", 96.0, 60.0, 50.0, 50.0, 2.0),
            profile("coder", 60.0, 95.0, 80.0, 90.0, 1.0),
        ];
        let req = Requirements { reasoning: 0.8, coding: 0.1, frontend: 0.0, backend: 0.1, math: 0.0, long_context: 0.0, ..Default::default() };
        for _ in 0..10 {
            let subtasks = vec![Subtask {
                id: "r".into(), description: "evaluate architecture".into(),
                requirements: req.clone(), cost_pressure: CostPressure::Low,
                depends_on: vec![], risk: Risk::Low,
            }];
            assert_eq!(dispatch(&subtasks, &profiles, 60.0, None)[0].model, "reasoner");
        }
    }

    #[test]
    fn similar_capability_cheaper_wins_under_cost_pressure() {
        let profiles = vec![
            profile("a", 90.0, 90.0, 90.0, 90.0, 1.0),
            profile("b", 92.0, 92.0, 92.0, 92.0, 5.0),
        ];
        let subtasks = vec![Subtask {
            id: "s".into(), description: "generic".into(),
            requirements: Requirements { reasoning: 0.3, coding: 0.3, frontend: 0.2, backend: 0.2, math: 0.0, long_context: 0.0, ..Default::default() },
            cost_pressure: CostPressure::High, depends_on: vec![], risk: Risk::Low,
        }];
        assert_eq!(dispatch(&subtasks, &profiles, 60.0, None)[0].model, "a"); // 高成本压力下便宜者胜
    }

    #[test]
    fn below_fit_threshold_escalates_to_strongest() {
        let profiles = vec![
            profile("strong", 95.0, 95.0, 95.0, 95.0, 10.0),
            profile("weak", 40.0, 40.0, 40.0, 40.0, 0.1),
        ];
        let subtasks = vec![Subtask {
            id: "s".into(), description: "hard".into(),
            requirements: Requirements { reasoning: 0.5, coding: 0.5, frontend: 0.0, backend: 0.0, math: 0.0, long_context: 0.0, ..Default::default() },
            cost_pressure: CostPressure::Low, depends_on: vec![], risk: Risk::High,
        }];
        let table = dispatch(&subtasks, &profiles, 60.0, None);
        // 能力优先:strong(95) vs weak(40) 能力差远超带宽 → strong 直接胜,无需升级。
        assert_eq!(table[0].model, "strong");
        assert!(!table[0].escalated);
        // audit 报真实能力分(0.5*95 + 0.5*95 = 95)。
        assert!((table[0].capability - 95.0).abs() < 1e-6);
        assert!((table[0].score - table[0].capability * table[0].efficiency).abs() < 1e-9);
    }

    #[test]
    fn capability_priority_beats_cheap_when_gap_exceeds_band() {
        // 能力优先:strong 贵 100 倍,但能力差(25)远超 Low 带宽(3)→ strong 胜。
        let profiles = vec![
            profile("strong", 95.0, 95.0, 95.0, 95.0, 10.0),
            profile("weak", 70.0, 70.0, 70.0, 70.0, 0.1),
        ];
        let subtasks = vec![Subtask {
            id: "s".into(), description: "high-value task".into(),
            requirements: Requirements { reasoning: 0.5, coding: 0.5, frontend: 0.0, backend: 0.0, math: 0.0, long_context: 0.0, ..Default::default() },
            cost_pressure: CostPressure::Low, depends_on: vec![], risk: Risk::Med,
        }];
        let table = dispatch(&subtasks, &profiles, 60.0, None);
        assert_eq!(table[0].model, "strong", "capability gap > band → stronger model wins regardless of cost");
        // 同级(带内)时便宜者胜:strong vs strong-2(97 vs 95, 差 2 ≤ Low 带 3)→ 便宜的 strong 胜。
        let profiles2 = vec![
            profile("cheap", 97.0, 97.0, 97.0, 97.0, 0.1),
            profile("pricy", 99.0, 99.0, 99.0, 99.0, 10.0),
        ];
        let table2 = dispatch(&subtasks, &profiles2, 60.0, None);
        assert_eq!(table2[0].model, "cheap", "within band → cheaper model wins");
    }

    #[test]
    fn three_candidate_band_sort_is_total_and_deterministic() {
        // 非传递陷阱:cap 90/85/81,bot(81) 最便宜且与 mid(85) 差 4 ≤ 带 7,
        // 逐对比较会成环(90 胜 81,81 胜 85,85 胜 90);锚点算法应确定选出带内最便宜的 mid。
        let profiles = vec![
            profile("top", 90.0, 90.0, 90.0, 90.0, 5.0),
            profile("mid", 85.0, 85.0, 85.0, 85.0, 1.0),
            profile("bot", 81.0, 81.0, 81.0, 81.0, 0.2),
        ];
        let subtasks = vec![Subtask {
            id: "s".into(), description: "generic".into(),
            requirements: Requirements { reasoning: 1.0, coding: 0.0, frontend: 0.0, backend: 0.0, math: 0.0, long_context: 0.0, ..Default::default() },
            cost_pressure: CostPressure::Med, // 带 7:锚点 max_cap=90,带内 cap≥83 → {top, mid}
            depends_on: vec![], risk: Risk::Low,
        }];
        for _ in 0..10 {
            let table = dispatch(&subtasks, &profiles, 60.0, None);
            // bot(81) 带外不参与;带内 top(90)/mid(85) 比效率 → mid 更便宜胜。
            assert_eq!(table[0].model, "mid", "deterministic winner must be stable across runs");
        }
    }

    #[test]
    fn seed_coding_heavy_subtask_routes_to_deepseek_not_qwen() {
        // 回归:旧逐对比较在种子画像上成环,编码重子任务被路由到 qwen3.8-max;
        // 锚点算法应确定性地选中带内最便宜的 deepseek-v4。
        let profiles = seed_profiles();
        let subtasks = vec![Subtask {
            id: "s".into(), description: "implement the backend module".into(),
            requirements: Requirements { coding: 1.0, backend: 0.0, reasoning: 0.0, frontend: 0.0, math: 0.0, long_context: 0.0, ..Default::default() },
            cost_pressure: CostPressure::Med, // 带 7
            depends_on: vec![], risk: Risk::Low,
        }];
        let table = dispatch(&subtasks, &profiles, 60.0, None);
        // 锚点 gpt-5.6-luna(95),带内 ≥ 88 → {deepseek-v4(92), gpt-5.6-luna(95)},
        // 带内最便宜 = deepseek-v4(0.1 < 10.0)。qwen(86) 在带外,不参与。
        assert_eq!(table[0].model, "deepseek-v4");
        assert!(!table[0].escalated);
    }

    #[test]
    fn empty_profiles_returns_empty_dispatch() {
        let subtasks = vec![Subtask {
            id: "s".into(), description: "x".into(),
            requirements: Requirements { reasoning: 0.5, coding: 0.5, frontend: 0.0, backend: 0.0, math: 0.0, long_context: 0.0, ..Default::default() },
            cost_pressure: CostPressure::Low, depends_on: vec![], risk: Risk::Low,
        }];
        assert!(dispatch(&subtasks, &[], 60.0, None).is_empty());
    }

    #[test]
    fn parse_subtask_graph_strips_fences() {
        let raw = "```json\n{\"intent\":\"fix\",\"subtasks\":[{\"id\":\"s1\",\"description\":\"x\",\"requirements\":{\"coding\":1.0},\"cost_pressure\":\"high\",\"depends_on\":[],\"risk\":\"low\"}]}\n```";
        let g = parse_subtask_graph(raw).unwrap();
        assert_eq!(g.intent, "fix");
        assert_eq!(g.subtasks[0].id, "s1");
        assert!((g.subtasks[0].requirements.coding - 1.0).abs() < 1e-9);
    }

    #[test]
    fn parse_subtask_graph_extracts_json_amid_reasoning_and_trailing_junk() {
        // 推理模型:reasoning 杂文 + JSON + 尾部碎句(旧的 rfind('}') 会带进尾部)。
        let raw = "我先想想…… 应该拆成后端和前端\n```json\n{\"intent\":\"build app\",\"subtasks\":[{\"id\":\"s1\",\"description\":\"backend\",\"requirements\":{\"backend\":0.8},\"cost_pressure\":\"low\",\"depends_on\":[],\"risk\":\"low\"}]}\n```\n以上就是我的方案,括号 } 之类的别管";
        let g = parse_subtask_graph(raw).unwrap();
        assert_eq!(g.intent, "build app");
        assert_eq!(g.subtasks[0].id, "s1");
    }

    #[test]
    fn parse_subtask_graph_no_json_errors() {
        assert!(parse_subtask_graph("完全没有 JSON").is_err());
    }
}
