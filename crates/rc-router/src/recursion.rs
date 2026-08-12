//! 递归执行器:同质短路 + 深度守卫 + 分解预算,决定 Execute / Decompose。
//!
//! `process` 是异步递归:对每个节点先做同质/深度短路,决策 Execute 则直接派活;
//! 异构且深度未满则调用 `allocator::decompose` 拆成子图并递归(受 `DECOMPOSE_BUDGET`
//! 约束)。历史成本门 `gate_decides` 保留为公共 API,但已从 `process` 移除调用——其
//! 成本数学代数消元后等价于 `max_weight < 0.625`,而同质短路(> 0.6)已拦截所有
//! max_weight ≥ 0.6 的节点,门在集成流程中恒返回 Decompose(惰性)。
//! 派活见 `crate::capability::dispatch`,成本见 `crate::cost::CostModel`。

use crate::allocator::{self, AllocatorError};
use crate::capability::{self, CapabilityProfile, DispatchEntry, Requirements, Subtask};
use crate::cost::CostModel;
use rc_pro::Provider;
use rc_proto::AgentEvent;
use rc_state::Store;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

pub const MAX_DEPTH: usize = 3;
pub const GATE_THRESHOLD: f64 = 0.2;
pub const ALPHA: f64 = 0.6;
/// 单次 run 的 decompose 调用预算:推理分配者每次 10-20s,预算耗尽后剩余节点直接
/// 派模型执行,避免"递归多层 × 慢分配者"把整个 run 拖到几分钟。
pub const DECOMPOSE_BUDGET: usize = 2;

#[derive(Debug, Clone, PartialEq)]
pub enum ExecAction {
    Execute { entry: DispatchEntry },
    Decompose { children: Vec<ExecPlan> },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExecPlan {
    pub subtask: Subtask,
    pub depth: usize,
    pub action: ExecAction,
    pub basis: String, // 成本依据文本,供 --plan-only 审计
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GateDecision { Execute, Decompose }

/// 同质短路:最大权重 > α → 路由收益低
pub fn is_homogeneous(req: &Requirements, alpha: f64) -> bool {
    let weights = [req.reasoning, req.coding, req.frontend, req.backend, req.math, req.long_context];
    weights.iter().cloned().fold(0.0, f64::max) > alpha
}

/// 成本门:E_decomposed < E_direct × (1 - GATE_THRESHOLD)
///
/// 拆解收益依赖异构度:需求越分散(heterogeneity 越高),拆解带来的分而治之收益越大。
/// 注意:该成本数学在代数上完全消元——e_decomposed = 0.1·e_direct + e_direct·(1-0.8·(1-max_weight)),
/// 代入门条件后等价于 `max_weight < 0.625`,与模型价格/Token 估计/校准无关。
/// `process` 已不再调用本门(见 [`process`] 内注释:同质短路 max_weight>0.6 已拦截所有
/// max_weight≥0.6 的节点,到达门处的节点必然 max_weight ≤ 0.6 < 0.625 → 恒 Decompose,
/// 门在集成流程中失效)。本函数保留供 API/测试独立使用。
pub fn gate_decides(
    node: &Subtask,
    profiles: &[CapabilityProfile],
    cost: &CostModel,
    store: &Store,
) -> GateDecision {
    if profiles.is_empty() {
        return GateDecision::Execute;
    }
    let table = capability::dispatch(std::slice::from_ref(node), profiles, 60.0, None);
    let best = &table[0];
    let best_profile = profiles.iter().find(|p| p.model == best.model).unwrap();
    // 校准键与 CLI 写侧对齐(rc-cli record_stat `{model}|usage` → kind "tokens"),
    // 使 token 实测在独立调用本门时能命中校准;旧 `{model}|{req_hash}` 永远不命中。
    let key = format!("{}|usage", best.model);
    // estimate_tokens 已含价格因子(cost.rs),此处不再乘 input_cost_per_m,避免价格平方。
    let e_direct = cost.token_estimate(store, &key, cost.estimate_tokens(node, best_profile));
    // 分配者调用成本 ≈ 一次直接执行的一小部分(相对而非绝对,避免绝对启发式淹没 e_direct)。
    let allocator_cost = e_direct * 0.1;
    // 异构度:1 - 最大需求权重。权重越分散,拆解收益越高。
    let max_weight = node.requirements.reasoning
        .max(node.requirements.coding)
        .max(node.requirements.frontend)
        .max(node.requirements.backend)
        .max(node.requirements.math)
        .max(node.requirements.long_context);
    let heterogeneity = 1.0 - max_weight;
    let benefit = 0.8 * heterogeneity;
    // e_decomposed = 分配者成本 + 直接成本 × (1 - 拆解收益)。
    // 异构(max_weight 0.4 → benefit 0.48)→ 0.62 < 0.8 → Decompose;
    // 近同质但未过 α(max_weight 0.8 → benefit 0.16)→ 0.94 > 0.8 → Execute。
    let e_decomposed = allocator_cost + e_direct * (1.0 - benefit);
    if e_decomposed < e_direct * (1.0 - GATE_THRESHOLD) { GateDecision::Decompose } else { GateDecision::Execute }
}

/// 递归执行:depth>=MAX_DEPTH 或同质 → Execute;否则(异构且深度未满)→ 拆解。
///
/// `emit` 可选:流式发编排事件(TUI 实时跟进)。拆解前发 `PhaseChanged{拆解}`,每个
/// 子任务派活时发 `OrchestratorDispatch`(含自动选中的模型)——任务树据此实时生长。
/// `cancel` 可选:置位后立即返回 `Cancelled`,让 /stop 能中断长拆解。
/// `budget` 可选:剩余 decompose 调用次数预算(见 [`DECOMPOSE_BUDGET`]),耗尽后直接 Execute。
// 参数多但各自语义清晰、被多个调用点使用;引入 context struct 会大改调用点,得不偿失。
#[allow(clippy::too_many_arguments)]
pub async fn process(
    node: Subtask,
    depth: usize,
    profiles: &[CapabilityProfile],
    cost: &CostModel,
    store: &Store,
    allocator: &dyn Provider,
    emit: Option<&(dyn Fn(AgentEvent) + Send + Sync)>,
    cancel: Option<&Arc<AtomicBool>>,
    budget: Option<&AtomicUsize>,
) -> Result<ExecPlan, AllocatorError> {
    if profiles.is_empty() {
        return Err(AllocatorError::Empty);
    }
    if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
        return Err(AllocatorError::Cancelled);
    }
    if depth >= MAX_DEPTH || is_homogeneous(&node.requirements, ALPHA) {
        let table = capability::dispatch(std::slice::from_ref(&node), profiles, 60.0, None);
        let entry = table.into_iter().next().unwrap();
        return Ok(ExecPlan { subtask: node.clone(), depth, action: ExecAction::Execute { entry }, basis: "homogeneous-or-depth".into() });
    }
    // 异构且深度未满 → 拆解(预算允许时)。成本门(`gate_decides`)在代数上等价于
    // max_weight < 0.625,而同质短路(> 0.6)已拦截所有 max_weight ≥ 0.6 的节点——
    // 到达这里的节点必然 max_weight ≤ 0.6 < 0.625,门恒返回 Decompose,故不再调用
    // (保留 `gate_decides` 供 API/测试);实际决策即"异构且深度未满 → 拆解"。
    if let Some(emit) = emit {
        emit(AgentEvent::PhaseChanged {
            phase: "拆解".into(),
            cycle: depth,
            session_id: String::new(),
        });
    }
    // 分解预算:耗尽(或不存在)则直接 Execute,避免慢分配者把 run 拖长。
    let can_decompose = budget.is_none_or(|b| {
        b.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
            if v > 0 { Some(v - 1) } else { None }
        })
        .is_ok()
    });
    if !can_decompose {
        let table =
            capability::dispatch(std::slice::from_ref(&node), profiles, 60.0, None);
        let entry = table.into_iter().next().unwrap();
        return Ok(ExecPlan {
            subtask: node,
            depth,
            action: ExecAction::Execute { entry },
            basis: "budget-exhausted".into(),
        });
    }
    // 尝试分解;失败(超时/解析/空)降级为单 agent Execute,而不是让整个
    // route 失败——分配者模型偶发问题时任务仍能跑通。
    let graph = match allocator::decompose(allocator, &node.description).await {
        Ok(g) => g,
        Err(_) => {
            let table =
                capability::dispatch(std::slice::from_ref(&node), profiles, 60.0, None);
            let entry = table.into_iter().next().unwrap();
            return Ok(ExecPlan {
                subtask: node,
                depth,
                action: ExecAction::Execute { entry },
                basis: "decompose-fallback".into(),
            });
        }
    };
    let mut children = Vec::new();
    for child in graph.subtasks {
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            return Err(AllocatorError::Cancelled);
        }
        // 先派活预览(能力×成本 argmax),拿自动选中的模型 → 发 Dispatch,
        // 让任务树在子任务解析出来时就长出来(顺序正确:先父后子)。
        let preview = capability::dispatch(std::slice::from_ref(&child), profiles, 60.0, None)
            .into_iter()
            .next()
            .map(|e| e.model)
            .unwrap_or_default();
        if let Some(emit) = emit {
            emit(AgentEvent::OrchestratorDispatch {
                parent_id: node.id.clone(),
                child_id: child.id.clone(),
                prompt: child.description.clone(),
                model: preview,
            });
        }
        let plan = Box::pin(process(
            child,
            depth + 1,
            profiles,
            cost,
            store,
            allocator,
            emit,
            cancel,
            budget,
        ))
        .await?;
        children.push(plan);
    }
    Ok(ExecPlan { subtask: node, depth, action: ExecAction::Decompose { children }, basis: "decompose".into() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::{CapabilityProfile, CostPressure, Requirements, Risk, Subtask};
    use crate::cost::CostModel;

    fn prof(model: &str, reasoning: f64, coding: f64, frontend: f64, cost: f64) -> CapabilityProfile {
        CapabilityProfile { model: model.into(), reasoning, coding, frontend, backend: 80.0,
            math: 80.0, long_context: 80.0, input_cost_per_m: cost, output_cost_per_m: cost * 3.0,
            context_window: 128_000, provenance: "s".into(), multimodal: false }
    }

    fn sub(id: &str, req: Requirements, risk: Risk) -> Subtask {
        Subtask { id: id.into(), description: "some heterogeneous work that warrants decomposition across capabilities".into(),
            requirements: req, cost_pressure: CostPressure::Med, depends_on: vec![], risk }
    }

    #[test]
    fn heterogeneous_decomposes_deeper_than_homogeneous() {
        let profiles = vec![prof("a", 60.0, 60.0, 60.0, 1.0), prof("b", 95.0, 95.0, 95.0, 5.0)];
        let store = rc_state::Store::open_in_memory().unwrap();
        let cost = CostModel::new(5);
        // 同质:需求被单一维度主导
        let homo = sub("h", Requirements { coding: 1.0, ..Default::default() }, Risk::Low);
        assert!(is_homogeneous(&homo.requirements, 0.6));
        // 异构:需求分散
        let hetero = sub("v", Requirements { reasoning: 0.3, coding: 0.4, frontend: 0.3, ..Default::default() }, Risk::Med);
        assert!(!is_homogeneous(&hetero.requirements, 0.6));
        // 近同质(max_weight 0.8):`gate_decides` 独立决策,由成本数学(代数消元后等价于
        // max_weight < 0.625)算得 Execute。该门已从 `process` 移除调用(集成流程中
        // 同质短路 >0.6 先拦截 max_weight ≥ 0.6 的节点,门恒 Decompose),此处直接
        // 测门本身,保留其公共 API 语义。
        let near_homo = sub("n", Requirements { coding: 0.8, reasoning: 0.1, frontend: 0.1, ..Default::default() }, Risk::Low);
        // 门:异构(0.4<0.625)拆,同质/近同质(≥0.625)直接执行
        let d_homo = gate_decides(&homo, &profiles, &cost, &store);
        let d_hetero = gate_decides(&hetero, &profiles, &cost, &store);
        let d_near = gate_decides(&near_homo, &profiles, &cost, &store);
        assert_eq!(d_homo, GateDecision::Execute);
        assert_eq!(d_hetero, GateDecision::Decompose);
        assert_eq!(d_near, GateDecision::Execute);
    }

    #[test]
    fn gate_decision_is_max_weight_only_not_cost() {
        // 诚实记录成本门的真实行为:其成本数学在代数上完全消元,
        // e_decomposed < e_direct×(1-0.2) ⟺ max_weight < 0.625,
        // 模型价格/Token 估计/校准对决策零影响。用极端价格差证明。
        let store = rc_state::Store::open_in_memory().unwrap();
        let cost = CostModel::new(5);
        let node = sub("n", Requirements { coding: 0.5, reasoning: 0.3, frontend: 0.2, ..Default::default() }, Risk::Med);
        let cheap = vec![prof("a", 60.0, 60.0, 60.0, 0.001), prof("b", 95.0, 95.0, 95.0, 5.0)];
        let pricey = vec![prof("a", 60.0, 60.0, 60.0, 0.001), prof("b", 95.0, 95.0, 95.0, 500.0)];
        // 0.5 < 0.625 → 恒 Decompose,不管模型贵 100 倍。
        assert_eq!(gate_decides(&node, &cheap, &cost, &store), GateDecision::Decompose);
        assert_eq!(gate_decides(&node, &pricey, &cost, &store), GateDecision::Decompose);
        // 0.8 ≥ 0.625 → 恒 Execute,即使模型极便宜。
        let focused = sub("f", Requirements { coding: 0.8, reasoning: 0.1, frontend: 0.1, ..Default::default() }, Risk::Low);
        assert_eq!(gate_decides(&focused, &cheap, &cost, &store), GateDecision::Execute);
    }

    // 伪分配者:固定返回一个含 2 个子任务的 JSON 图(子任务同质 → 深度 1 Execute)。
    struct StubAllocator;
    #[async_trait::async_trait]
    impl Provider for StubAllocator {
        fn id(&self) -> &str { "mock:allocator" }
        async fn stream(&self, _req: rc_pro::canonical::CanonicalRequest) -> Result<rc_pro::provider::ProvStream, rc_pro::ProviderError> {
            let text = "```json\n{\"intent\":\"build\",\"subtasks\":[{\"id\":\"s1\",\"description\":\"write api\",\"requirements\":{\"coding\":1.0},\"cost_pressure\":\"low\",\"depends_on\":[],\"risk\":\"low\"},{\"id\":\"s2\",\"description\":\"write tests\",\"requirements\":{\"coding\":1.0},\"cost_pressure\":\"low\",\"depends_on\":[],\"risk\":\"low\"}]}\n```";
            let stream = futures::stream::iter(vec![
                Ok::<_, rc_pro::ProviderError>(rc_pro::canonical::ProvEvent::Delta { text: text.to_string() }),
                Ok(rc_pro::canonical::ProvEvent::Finish { stop_reason: "stop".into(), usage: None }),
            ]);
            Ok(Box::pin(stream))
        }
        async fn embed(&self, _texts: Vec<String>) -> Result<Vec<Vec<f32>>, rc_pro::ProviderError> { Ok(vec![]) }
    }

    #[tokio::test]
    async fn process_streams_phase_and_dispatch_events() {
        let profiles = vec![prof("a", 90.0, 90.0, 90.0, 1.0)];
        let store = rc_state::Store::open_in_memory().unwrap();
        let cost = CostModel::new(5);
        let root = Subtask {
            id: "root".into(),
            description: "build app".into(),
            requirements: Requirements::default(),
            cost_pressure: CostPressure::Med,
            depends_on: vec![],
            risk: Risk::Low,
        };
        let events: Arc<std::sync::Mutex<Vec<AgentEvent>>> = Arc::default();
        let sink = {
            let events = events.clone();
            move |ev: AgentEvent| events.lock().unwrap().push(ev)
        };
        let plan = process(root, 0, &profiles, &cost, &store, &StubAllocator, Some(&sink), None, None)
            .await
            .unwrap();
        // root 分解成 s1,s2(同质 → Execute)。
        let children = match &plan.action {
            ExecAction::Decompose { children } => children,
            other => panic!("expected decompose, got {other:?}"),
        };
        assert_eq!(children.len(), 2);
        let evs = events.lock().unwrap().clone();
        // 拆解阶段事件先于派发。
        assert!(evs.iter().any(|e| matches!(e, AgentEvent::PhaseChanged { .. })));
        let dispatches: Vec<&AgentEvent> = evs
            .iter()
            .filter(|e| matches!(e, AgentEvent::OrchestratorDispatch { .. }))
            .collect();
        assert_eq!(dispatches.len(), 2);
        if let AgentEvent::OrchestratorDispatch { child_id, model, parent_id, .. } = dispatches[0] {
            assert_eq!(child_id, "s1");
            assert_eq!(parent_id, "root");
            assert!(!model.is_empty(), "dispatch must carry the auto-selected model");
        }
    }

    #[tokio::test]
    async fn process_cancelled_returns_cancelled() {
        let profiles = vec![prof("a", 90.0, 90.0, 90.0, 1.0)];
        let store = rc_state::Store::open_in_memory().unwrap();
        let cost = CostModel::new(5);
        let root = Subtask {
            id: "root".into(),
            description: "x".into(),
            requirements: Requirements::default(),
            cost_pressure: CostPressure::Med,
            depends_on: vec![],
            risk: Risk::Low,
        };
        let cancel = Arc::new(AtomicBool::new(true));
        let err = process(root, 0, &profiles, &cost, &store, &StubAllocator, None, Some(&cancel), None)
            .await
            .unwrap_err();
        assert!(matches!(err, AllocatorError::Cancelled));
    }

    #[tokio::test]
    async fn process_budget_zero_forces_execute() {
        let profiles = vec![prof("a", 90.0, 90.0, 90.0, 1.0)];
        let store = rc_state::Store::open_in_memory().unwrap();
        let cost = CostModel::new(5);
        let root = Subtask {
            id: "root".into(),
            description: "x".into(),
            requirements: Requirements::default(),
            cost_pressure: CostPressure::Med,
            depends_on: vec![],
            risk: Risk::Low,
        };
        // 预算 0 → 根节点不分解,直接 Execute(budget-exhausted)。
        let budget = AtomicUsize::new(0);
        let plan = process(root, 0, &profiles, &cost, &store, &StubAllocator, None, None, Some(&budget))
            .await
            .unwrap();
        match plan.action {
            ExecAction::Execute { .. } => assert_eq!(plan.basis, "budget-exhausted"),
            other => panic!("expected budget-exhausted Execute, got {other:?}"),
        }
    }
}
