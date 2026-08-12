use crate::capability::{CapabilityProfile, Subtask};

/// 子任务是否需要视觉(分配者已标)
pub fn needs_vision(subtask: &Subtask) -> bool { subtask.requirements.vision }

/// 派给的模型非多模态 → 需要视觉桥
pub fn should_bridge(profile: &CapabilityProfile) -> bool { !profile.multimodal }

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::{CapabilityProfile, Requirements, Risk, Subtask};

    fn prof(model: &str, multimodal: bool) -> CapabilityProfile {
        CapabilityProfile { model: model.into(), reasoning: 80.0, coding: 80.0, frontend: 80.0,
            backend: 80.0, math: 80.0, long_context: 80.0, input_cost_per_m: 1.0,
            output_cost_per_m: 2.0, context_window: 128_000, provenance: "s".into(), multimodal }
    }

    #[test]
    fn multimodal_native_skips_bridge() {
        let sub = Subtask { id: "s".into(), description: "check screenshot".into(),
            requirements: Requirements { vision: true, ..Default::default() },
            cost_pressure: Default::default(), depends_on: vec![], risk: Risk::Low };
        assert!(needs_vision(&sub));
        assert!(!should_bridge(&prof("gpt-4o", true)));   // 多模态原生,不绕桥
        assert!(should_bridge(&prof("deepseek", false))); // 纯文本,走桥
    }
}
