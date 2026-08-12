//! Agent event stream shared by the core and every frontend.
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    Token,
    Thinking,
    ToolCall,
    ToolResult,
    AskingApproval,
    AskingQuestion,
    SkillLoaded,
    SkillSuggested,
    McpToolList,
    SessionStarted,
    PlanProposed,
    PhaseChanged,
    ReviewProposed,
    AgentSpawned,
    AgentToolCall,
    AgentStatus,
    AgentResult,
    OrchestratorPlan,
    OrchestratorDispatch,
    OrchestratorResult,
    ContextUpdate,
    Done,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentEvent {
    Token {
        delta: String,
    },
    Thinking {
        delta: String,
    },
    ToolCall {
        id: String,
        name: String,
        args: Value,
    },
    ToolResult {
        id: String,
        name: String,
        ok: bool,
        output: String,
        /// 超限输出落盘路径(None = 内联)。
        #[serde(default)]
        output_path: Option<String>,
    },
    AskingApproval {
        id: String,
        tool: String,
        description: String,
    },
    AskingQuestion {
        id: String,
        question: String,
        session_id: String,
    },
    SkillLoaded {
        name: String,
        path: String,
    },
    SkillSuggested {
        name: String,
        category: String,
        confidence: f32,
    },
    McpToolList {
        server: String,
        tools: Vec<String>,
    },
    SessionStarted {
        session_id: String,
    },
    PlanProposed {
        summary: String,
        session_id: String,
    },
    PhaseChanged {
        phase: String,
        cycle: usize,
        session_id: String,
    },
    ReviewProposed {
        verdict: String,
        reason: String,
        next_intent: String,
        summary: String,
        cycle: usize,
        session_id: String,
    },
    AgentSpawned {
        id: String,
        model: String,
        role: String,
        task: String,
    },
    AgentToolCall {
        id: String,
        tool: String,
        args_preview: String,
    },
    AgentStatus {
        id: String,
        phase: String,
        tokens: u64,
        elapsed_ms: u64,
    },
    AgentResult {
        id: String,
        verdict: String,
        tests: String,
        cost: f64,
    },
    /// 主控产出/更新计划(供 TUI 树状呈现)。
    OrchestratorPlan {
        node_id: String,
        plan: String,
    },
    /// 主控派发子代理。
    OrchestratorDispatch {
        parent_id: String,
        child_id: String,
        prompt: String,
        model: String,
    },
    /// 子代理结果汇总。
    OrchestratorResult {
        node_id: String,
        status: String,
        summary: String,
    },
    ContextUpdate {
        used: u64,
        limit: u64,
        pct: u8,
        /// Which agent's context window this update describes. `None` for a
        /// single-agent session (rc-core), `Some(subtask_id)` for a routed run
        /// (rc-router). Frontends aggregate per-agent `used` into a session-wide
        /// window instead of last-writer-wins.
        #[serde(default)]
        agent_id: Option<String>,
    },
    Done {
        summary: String,
        usage: Option<Value>,
        session_id: String,
        /// 完整思维链(可选)。UI 可展开;不回传模型。
        #[serde(default)]
        reasoning: Option<String>,
    },
    Error {
        message: String,
    },
}

impl AgentEvent {
    pub fn kind(&self) -> EventKind {
        match self {
            Self::Token { .. } => EventKind::Token,
            Self::Thinking { .. } => EventKind::Thinking,
            Self::ToolCall { .. } => EventKind::ToolCall,
            Self::ToolResult { .. } => EventKind::ToolResult,
            Self::AskingApproval { .. } => EventKind::AskingApproval,
            Self::AskingQuestion { .. } => EventKind::AskingQuestion,
            Self::SkillLoaded { .. } => EventKind::SkillLoaded,
            Self::SkillSuggested { .. } => EventKind::SkillSuggested,
            Self::McpToolList { .. } => EventKind::McpToolList,
            Self::SessionStarted { .. } => EventKind::SessionStarted,
            Self::PlanProposed { .. } => EventKind::PlanProposed,
            Self::PhaseChanged { .. } => EventKind::PhaseChanged,
            Self::ReviewProposed { .. } => EventKind::ReviewProposed,
            Self::AgentSpawned { .. } => EventKind::AgentSpawned,
            Self::AgentToolCall { .. } => EventKind::AgentToolCall,
            Self::AgentStatus { .. } => EventKind::AgentStatus,
            Self::AgentResult { .. } => EventKind::AgentResult,
            Self::OrchestratorPlan { .. } => EventKind::OrchestratorPlan,
            Self::OrchestratorDispatch { .. } => EventKind::OrchestratorDispatch,
            Self::OrchestratorResult { .. } => EventKind::OrchestratorResult,
            Self::ContextUpdate { .. } => EventKind::ContextUpdate,
            Self::Done { .. } => EventKind::Done,
            Self::Error { .. } => EventKind::Error,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_update_roundtrips_with_kind() {
        let e = AgentEvent::ContextUpdate {
            used: 1280,
            limit: 128_000,
            pct: 1,
            agent_id: None,
        };
        let s = serde_json::to_string(&e).unwrap();
        assert!(s.contains("\"kind\":\"context_update\""));
        assert!(s.contains("\"used\":1280"));
        let back: AgentEvent = serde_json::from_str(&s).unwrap();
        match back {
            AgentEvent::ContextUpdate { used, limit, pct, ref agent_id } => {
                assert_eq!(used, 1280);
                assert_eq!(limit, 128_000);
                assert_eq!(pct, 1);
                assert!(agent_id.is_none());
            }
            _ => panic!("wrong variant"),
        }
        assert_eq!(back.kind(), EventKind::ContextUpdate);
    }

    #[test]
    fn event_serializes_with_tag() {
        let e = AgentEvent::Token { delta: "hi".into() };
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["kind"], "token");
        let back: AgentEvent = serde_json::from_value(v).unwrap();
        assert_eq!(back.kind(), EventKind::Token);
    }

    #[test]
    fn agent_events_serialize_with_kind_tag() {
        let ev = AgentEvent::AgentSpawned {
            id: "s1".into(),
            model: "deepseek".into(),
            role: "executor".into(),
            task: "fix api".into(),
        };
        let s = serde_json::to_string(&ev).unwrap();
        assert!(s.contains("\"kind\":\"agent_spawned\""));
        let ev2: AgentEvent = serde_json::from_str(&s).unwrap();
        match ev2 {
            AgentEvent::AgentSpawned { id, model, .. } => {
                assert_eq!(id, "s1");
                assert_eq!(model, "deepseek");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn done_roundtrips_with_reasoning() {
        let e = AgentEvent::Done {
            summary: "done".into(),
            usage: None,
            session_id: "s".into(),
            reasoning: Some("step 1... step 2...".into()),
        };
        let s = serde_json::to_string(&e).unwrap();
        assert!(s.contains("\"reasoning\":\"step 1... step 2...\""));
        let back: AgentEvent = serde_json::from_str(&s).unwrap();
        match back {
            AgentEvent::Done { reasoning, .. } => {
                assert_eq!(reasoning.as_deref(), Some("step 1... step 2..."));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn orchestrator_events_roundtrip() {
        let e = AgentEvent::OrchestratorDispatch {
            parent_id: "root".into(),
            child_id: "a1".into(),
            prompt: "sort list".into(),
            model: "deepseek-v4-flash".into(),
        };
        let s = serde_json::to_string(&e).unwrap();
        assert!(s.contains("\"kind\":\"orchestrator_dispatch\""));
        let back: AgentEvent = serde_json::from_str(&s).unwrap();
        match back {
            AgentEvent::OrchestratorDispatch { ref child_id, .. } => assert_eq!(child_id, "a1"),
            _ => panic!("wrong variant"),
        }
        assert_eq!(back.kind(), EventKind::OrchestratorDispatch);
    }
}
