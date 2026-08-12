//! 成本模型:启发式估算 + rc-state cost_stats 实测校准。

use crate::capability::{CapabilityProfile, Requirements, Risk, Subtask};
use rc_state::Store;

#[derive(Debug, Clone)]
pub struct CostModel {
    pub min_evidence: u64,
}

impl CostModel {
    pub fn new(min_evidence: u64) -> Self { Self { min_evidence } }

    /// 启发式:描述长度 × 需求相关性 × 模型价格因子
    pub fn estimate_tokens(&self, subtask: &Subtask, profile: &CapabilityProfile) -> f64 {
        // chars().count() 而非 .len():多字节描述(中文等)按字符计,避免字节数虚高。
        let len = subtask.description.chars().count() as f64;
        let relevance = capability_score_heuristic(&subtask.requirements, profile);
        (len / 40.0).max(1.0) * 500.0 * relevance * profile.input_cost_per_m.max(0.001)
    }

    /// 撕破概率种子:risk 权重 × (1 - capability/100)
    pub fn estimate_tear_p(&self, risk: Risk, capability: f64) -> f64 {
        let w = match risk { Risk::Low => 0.1, Risk::Med => 0.3, Risk::High => 0.7 };
        (w * (1.0 - capability / 100.0)).clamp(0.0, 1.0)
    }

    /// C_tear 影响面
    pub fn impact(&self, risk: Risk) -> f64 {
        match risk { Risk::Low => 1.0, Risk::Med => 5.0, Risk::High => 20.0 }
    }

    pub fn token_estimate(&self, store: &Store, key: &str, heuristic: f64) -> f64 {
        match store.get_stat(key, "tokens").ok().flatten() {
            Some((samples, avg)) if samples >= self.min_evidence => avg,
            _ => heuristic,
        }
    }
    pub fn tear_estimate(&self, store: &Store, key: &str, heuristic: f64) -> f64 {
        match store.get_stat(key, "tear_p").ok().flatten() {
            Some((samples, avg)) if samples >= self.min_evidence => avg,
            _ => heuristic,
        }
    }
}

fn capability_score_heuristic(req: &Requirements, p: &CapabilityProfile) -> f64 {
    // 无需求权重时回退等权
    let sum: f64 = req.reasoning + req.coding + req.frontend + req.backend + req.math + req.long_context;
    if sum <= 0.0 { return 0.9; }
    (req.reasoning * p.reasoning + req.coding * p.coding + req.frontend * p.frontend
        + req.backend * p.backend + req.math * p.math + req.long_context * p.long_context) / sum / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::{CapabilityProfile, Requirements, Risk, Subtask};

    fn prof(model: &str, multimodal: bool) -> CapabilityProfile {
        CapabilityProfile {
            model: model.into(), reasoning: 80.0, coding: 80.0, frontend: 80.0,
            backend: 80.0, math: 80.0, long_context: 80.0,
            input_cost_per_m: 1.0, output_cost_per_m: 2.0, context_window: 128_000,
            provenance: "s".into(), multimodal,
        }
    }

    #[test]
    fn estimates_are_deterministic() {
        let m = CostModel::new(5);
        let s = Subtask { id: "s".into(), description: "fix backend api".into(),
            requirements: Requirements { backend: 0.8, coding: 0.2, ..Default::default() },
            cost_pressure: crate::capability::CostPressure::Med, depends_on: vec![], risk: Risk::High };
        let t = m.estimate_tokens(&s, &prof("d", false));
        assert!(t > 0.0);
        let tp = m.estimate_tear_p(Risk::High, 60.0);
        assert!(tp > m.estimate_tear_p(Risk::Low, 90.0));
        assert_eq!(m.impact(Risk::High), 20.0);
    }

    #[test]
    fn calibration_switches_at_evidence_threshold() {
        let store = rc_state::Store::open_in_memory().unwrap();
        let m = CostModel::new(2); // MIN_EVIDENCE=2
        let _s = Subtask { id: "s".into(), description: "x".into(),
            requirements: Requirements::default(), cost_pressure: Default::default(),
            depends_on: vec![], risk: Risk::Low };
        // 1 条:仍用启发式
        store.record_stat("k", "tokens", 10_000.0).unwrap();
        assert_eq!(m.token_estimate(&store, "k", 123.0), 123.0);
        // 第 2 条:切实测
        store.record_stat("k", "tokens", 20_000.0).unwrap();
        let calibrated = m.token_estimate(&store, "k", 123.0);
        assert!((calibrated - 15_000.0).abs() < 1e-6);
    }
}
