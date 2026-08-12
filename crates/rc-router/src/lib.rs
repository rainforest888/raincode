pub mod allocator;
pub mod capability;
pub mod cost;
pub mod execute;
pub mod intent;
pub mod recursion;
pub mod risk;
pub mod vision;
pub use allocator::decompose;
pub use capability::{
    CostPressure, DispatchEntry, Requirements, Risk, Subtask, SubtaskGraph, dispatch,
    parse_subtask_graph,
};
pub use cost::CostModel;
pub use execute::{SubtaskResult, execute_subtasks, execute_subtasks_batched};
pub use intent::{apply_pins, filter_pool};
pub use risk::{EscalationEvent, EscalationTrigger, RiskMode, RiskState, parse_risk_mode};
pub use risk::{RiskGate, risk_gate};
pub use risk::spot_check::SpotCheckVerdict;
pub use recursion::{
    ALPHA, GATE_THRESHOLD, MAX_DEPTH, ExecAction, ExecPlan, GateDecision, gate_decides,
    is_homogeneous, process,
};
