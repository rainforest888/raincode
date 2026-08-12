use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalMode {
    #[default]
    Ask,
    Auto,
    Deny,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub tool: String,
    pub description: String,
    pub args: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum ApprovalDecision {
    Allow,
    Deny { reason: String },
    Edit { args: Value },
}

#[async_trait::async_trait]
pub trait ApprovalHook: Send + Sync {
    async fn ask(&self, req: &ApprovalRequest) -> ApprovalDecision;
}

/// Auto-approves everything. Used in non-interactive/CI modes.
pub struct AutoApproveHook;

#[async_trait::async_trait]
impl ApprovalHook for AutoApproveHook {
    async fn ask(&self, _req: &ApprovalRequest) -> ApprovalDecision {
        ApprovalDecision::Allow
    }
}

/// Denies everything. Used for read-only runs.
pub struct DenyHook;

#[async_trait::async_trait]
impl ApprovalHook for DenyHook {
    async fn ask(&self, _req: &ApprovalRequest) -> ApprovalDecision {
        ApprovalDecision::Deny {
            reason: "approval mode is deny".into(),
        }
    }
}

/// Delegates to a closure (used by the TUI, server and tests).
pub struct PromptHook<F> {
    f: F,
}

impl<F> PromptHook<F>
where
    F: Fn(&ApprovalRequest) -> ApprovalDecision + Send + Sync,
{
    pub fn new(f: F) -> Self {
        Self { f }
    }
}

#[async_trait::async_trait]
impl<F> ApprovalHook for PromptHook<F>
where
    F: Fn(&ApprovalRequest) -> ApprovalDecision + Send + Sync,
{
    async fn ask(&self, req: &ApprovalRequest) -> ApprovalDecision {
        (self.f)(req)
    }
}
