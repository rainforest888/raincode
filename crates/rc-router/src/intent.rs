use crate::capability::{CapabilityProfile, DispatchEntry};
use std::collections::HashMap;

/// 模型池约束:过滤到用户指定允许集内;池空 = 全部
pub fn filter_pool(profiles: Vec<CapabilityProfile>, pool: &[String]) -> Vec<CapabilityProfile> {
    if pool.is_empty() { return profiles; }
    profiles.into_iter().filter(|p| pool.iter().any(|m| m == &p.model)).collect()
}

/// 子任务 pin:硬指定,覆盖路由结果
pub fn apply_pins(table: Vec<DispatchEntry>, pins: &HashMap<String, String>) -> Vec<DispatchEntry> {
    table.into_iter().map(|mut e| {
        if let Some(model) = pins.get(&e.subtask_id) {
            e.model = model.clone();
        }
        e
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::{CapabilityProfile, DispatchEntry};

    fn prof(model: &str) -> CapabilityProfile {
        CapabilityProfile { model: model.into(), reasoning: 80.0, coding: 80.0, frontend: 80.0,
            backend: 80.0, math: 80.0, long_context: 80.0, input_cost_per_m: 1.0,
            output_cost_per_m: 2.0, context_window: 128_000, provenance: "s".into(), multimodal: false }
    }

    fn entry(id: &str, model: &str) -> DispatchEntry {
        DispatchEntry { subtask_id: id.into(), model: model.into(), capability: 80.0, efficiency: 1.0, score: 80.0, escalated: false }
    }

    #[test]
    fn pool_filters_and_pin_overrides() {
        let profiles = vec![prof("deepseek"), prof("qwen"), prof("gpt")];
        // 池约束:只在池内选
        let filtered = filter_pool(profiles, &["deepseek".to_string(), "qwen".to_string()]);
        assert_eq!(filtered.len(), 2);
        assert!(filtered.iter().all(|p| p.model != "gpt"));
        // pin 覆盖:子任务 s2 指定 qwen
        let table = vec![entry("s1", "deepseek"), entry("s2", "deepseek")];
        let mut pins = std::collections::HashMap::new();
        pins.insert("s2".to_string(), "qwen".to_string());
        let applied = apply_pins(table, &pins);
        assert_eq!(applied[0].model, "deepseek");
        assert_eq!(applied[1].model, "qwen");
    }
}
