//! 风险治理:棘轮升级(只升不降)+ 降级仅用户 + 风险模式 + 始终自动抽查。

use crate::capability::Risk;
use crate::cost::CostModel;
use rc_state::Store;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RiskMode { Auto, Assisted, Ask, Manual }

/// RiskMode → 审批门控三态:Auto 放行,Manual 拒绝,Ask/Assisted 交由交互层弹审批。
/// 独立枚举而非复用 ApprovalDecision,避免给 rc-sandbox 加变体波及全仓穷尽匹配。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RiskGate { Allow, Deny, Prompt }

pub fn risk_gate(mode: RiskMode) -> RiskGate {
    match mode {
        RiskMode::Auto => RiskGate::Allow,
        RiskMode::Manual => RiskGate::Deny,
        RiskMode::Ask | RiskMode::Assisted => RiskGate::Prompt,
    }
}

/// 解析 /risk 参数为 RiskMode(大小写不敏感)。无效时返回含合法值的错误。
pub fn parse_risk_mode(s: &str) -> Result<RiskMode, String> {
    match s.trim().to_lowercase().as_str() {
        "auto" => Ok(RiskMode::Auto),
        "assisted" => Ok(RiskMode::Assisted),
        "ask" => Ok(RiskMode::Ask),
        "manual" => Ok(RiskMode::Manual),
        other => Err(format!(
            "unknown risk mode '{other}' (valid: auto, assisted, ask, manual)"
        )),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum EscalationTrigger { System(&'static str), Agent(String), User(String) }

#[derive(Debug, Clone, PartialEq)]
pub struct EscalationEvent {
    pub subtask_id: String,
    pub from: Risk,
    pub to: Risk,
    pub trigger: String,
    pub reason: String,
}

pub struct RiskState {
    pub mode: RiskMode,
    levels: HashMap<String, Risk>,
    pending: HashMap<String, EscalationEvent>,
    pub log: Vec<EscalationEvent>,
}

impl RiskState {
    pub fn new(mode: RiskMode) -> Self {
        Self { mode, levels: HashMap::new(), pending: HashMap::new(), log: Vec::new() }
    }
    pub fn level(&self, id: &str) -> Risk { self.levels.get(id).copied().unwrap_or(Risk::Low) }

    /// 棘轮升级:只升不降;过成本门;按模式决定自动/待确认
    #[allow(clippy::too_many_arguments)]
    pub fn maybe_escalate(&mut self, id: &str, current: Risk, to: Risk, trigger: EscalationTrigger, cost: &CostModel, store: &Store, capability: f64) -> Option<EscalationEvent> {
        if to as u8 <= current as u8 { return None; } // 棘轮:不降
        let stored = self.level(id);
        // Risk 无 Ord 派生,按 as u8(Low=0/Med=1/High=2)取"当前有效等级"= 两者取高
        let current_eff = if current as u8 > stored as u8 { current } else { stored };
        if to as u8 <= current_eff as u8 { return None; }
        // 成本门:预期撕破代价 > 升级成本才升
        let key = format!("{}|risk{}", id, to as u8);
        let p = cost.tear_estimate(store, &key, cost.estimate_tear_p(to, capability));
        let expected_tear = p * cost.impact(to);
        let upgrade_cost = 2.0; // 简化:升级模型价差因子
        if expected_tear < upgrade_cost { return None; }
        let (reason, tstr) = match &trigger {
            EscalationTrigger::System(r) => (r.to_string(), format!("system:{r}")),
            EscalationTrigger::Agent(r) => (r.clone(), "agent".into()),
            EscalationTrigger::User(r) => (r.clone(), "user".into()),
        };
        let ev = EscalationEvent { subtask_id: id.into(), from: current_eff, to, trigger: tstr, reason };
        match self.mode {
            RiskMode::Auto => {
                self.levels.insert(id.to_string(), to);
                self.log.push(ev.clone());
                Some(ev)
            }
            RiskMode::Assisted if current_eff == Risk::Low => {
                self.levels.insert(id.to_string(), to);
                self.log.push(ev.clone());
                Some(ev)
            }
            RiskMode::Assisted | RiskMode::Ask => {
                // 待确认:存下完整事件,确认应用时入 log。
                self.pending.insert(id.to_string(), ev.clone());
                None
            }
            RiskMode::Manual => None,
        }
    }

    /// 暴露待确认的升级事件(Ask / Assisted(>Low) 模式经 [`maybe_escalate`] 存入,
    /// 尚未应用)。调用方(CLI/TUI 交互层)据此弹审批,`confirm_pending` 应用之。
    pub fn pending_escalations(&self) -> Vec<EscalationEvent> {
        self.pending.values().cloned().collect()
    }

    /// 应用一条待确认升级:从 pending 移入 levels(棘轮仍只升不降)+ log。
    /// 返回被应用的事件;id 无待确认项时返回 None。
    pub fn confirm_pending(&mut self, id: &str) -> Option<EscalationEvent> {
        let ev = self.pending.remove(id)?;
        self.levels.insert(id.to_string(), ev.to);
        self.log.push(ev.clone());
        Some(ev)
    }
}

pub mod spot_check {
    use crate::execute::SubtaskResult;

    #[derive(Debug, Clone, PartialEq)]
    pub struct SpotCheckVerdict {
        pub subtask_id: String,
        pub ok: bool,
        pub issue: String,
        pub severity: u8, // 0-3
    }

    /// 始终自动:对完成的子任务抽样,产出判定并(由调用方)回灌校准
    pub fn inspect(completed: &[SubtaskResult]) -> Vec<SpotCheckVerdict> {
        // 最小实现:非 ok 的子任务 → 撕裂信号
        completed.iter().map(|r| SpotCheckVerdict {
            subtask_id: r.subtask_id.clone(),
            ok: r.ok,
            issue: if r.ok { String::new() } else { r.summary.clone() },
            severity: if r.ok { 0 } else { 3 },
        }).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::Risk;
    use crate::cost::CostModel;

    #[test]
    fn ratchet_escalates_up_but_never_auto_downgrades() {
        let mut st = RiskState::new(RiskMode::Auto);
        let cost = CostModel::new(2);
        let store = rc_state::Store::open_in_memory().unwrap();
        // 升级(Med→High)自动生效;棘轮只升不降。
        let ev = st.maybe_escalate("s1", Risk::Med, Risk::High, EscalationTrigger::System("approval-denied"), &cost, &store, 50.0);
        assert!(ev.is_some());
        assert_eq!(st.level("s1"), Risk::High);
        // 试图"降到"Low:棘轮拒绝(to <= current 不生效)。
        let ev2 = st.maybe_escalate("s1", Risk::High, Risk::Low, EscalationTrigger::System("x"), &cost, &store, 50.0);
        assert!(ev2.is_none());
        assert_eq!(st.level("s1"), Risk::High);
    }

    #[test]
    fn ask_mode_requires_confirmation_for_escalation() {
        let mut st = RiskState::new(RiskMode::Ask);
        let cost = CostModel::new(2);
        let store = rc_state::Store::open_in_memory().unwrap();
        // ask 模式:升级不立即生效,进入待确认(pending)。
        let ev = st.maybe_escalate("s2", Risk::Low, Risk::High, EscalationTrigger::System("spot-check"), &cost, &store, 30.0);
        assert!(ev.is_none());
        assert_eq!(st.level("s2"), Risk::Low);
        assert!(st.pending.contains_key("s2"), "ask-mode escalation must be held pending");
        // 待确认事件可被外部读取(CLI/TUI 据此弹审批)。
        let pend = st.pending_escalations();
        assert_eq!(pend.len(), 1);
        assert_eq!(pend[0].subtask_id, "s2");
        assert_eq!(pend[0].from, Risk::Low);
        assert_eq!(pend[0].to, Risk::High);
        // confirm 应用:移入 levels + log,待确认清空。
        let confirmed = st.confirm_pending("s2").expect("pending escalation must confirm");
        assert_eq!(confirmed.to, Risk::High);
        assert_eq!(st.level("s2"), Risk::High);
        assert_eq!(st.log.len(), 1);
        assert!(st.pending_escalations().is_empty());
        // 已应用/不存在 id → None。
        assert!(st.confirm_pending("s2").is_none());
        assert!(st.confirm_pending("nope").is_none());
    }

    #[test]
    fn risk_mode_maps_to_gate() {
        assert_eq!(risk_gate(RiskMode::Auto), RiskGate::Allow);
        assert_eq!(risk_gate(RiskMode::Manual), RiskGate::Deny);
        assert_eq!(risk_gate(RiskMode::Ask), RiskGate::Prompt);
        assert_eq!(risk_gate(RiskMode::Assisted), RiskGate::Prompt);
    }

    #[test]
    fn risk_mode_parses_case_insensitive() {
        assert_eq!(parse_risk_mode("auto").unwrap(), RiskMode::Auto);
        assert_eq!(parse_risk_mode("Manual").unwrap(), RiskMode::Manual);
        assert_eq!(parse_risk_mode("ASK").unwrap(), RiskMode::Ask);
        assert_eq!(parse_risk_mode("assisted").unwrap(), RiskMode::Assisted);
        assert!(parse_risk_mode("nope").is_err());
    }
}
