//! Sandbox policies for Raincode: shell command allow/deny rules,
//! network host rules, and the approval hook used by tools.
pub mod approval;
pub mod command;
pub mod guard;
pub mod guard_hook;
pub mod network;
pub mod user_input;

pub use approval::{
    ApprovalDecision, ApprovalHook, ApprovalMode, ApprovalRequest, AutoApproveHook, DenyHook,
    PromptHook,
};
pub use command::{CommandDecision, CommandPolicy};
pub use guard_hook::{
    memo_allows, memo_record, GuardConsent, GuardHook, GuardRequest, PromptGuardHook,
    SessionGuardMemo,
};
pub use guard::{
    append_allow_high_risk, guard_check, load_supervise_config, AllowRules, DenyRules,
    GuardDecision, GuardError, GuardFlags, SuperviseConfig,
};
pub use network::{NetworkDecision, NetworkPolicy, PolicyDefault};
pub use user_input::{AutoUserHook, PromptUserHook, UserInputHook};
