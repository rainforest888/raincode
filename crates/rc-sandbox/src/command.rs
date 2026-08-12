use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CommandPolicy {
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandDecision {
    Allowed,
    Denied { reason: String },
    NeedsApproval,
}

impl CommandPolicy {
    pub fn check(&self, command: &str) -> CommandDecision {
        let c = command.trim();
        if c.is_empty() {
            return CommandDecision::Denied {
                reason: "empty command".into(),
            };
        }
        for pattern in &self.deny {
            if c.contains(pattern) {
                return CommandDecision::Denied {
                    reason: format!("command matches deny pattern '{pattern}'"),
                };
            }
        }
        if self.allow.is_empty() {
            return CommandDecision::NeedsApproval;
        }
        for pattern in &self.allow {
            if c.starts_with(pattern) || c.contains(pattern) {
                return CommandDecision::Allowed;
            }
        }
        CommandDecision::NeedsApproval
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deny_wins_over_allow() {
        let p = CommandPolicy {
            allow: vec!["cargo".into()],
            deny: vec!["--release".into()],
        };
        assert_eq!(p.check("cargo build"), CommandDecision::Allowed);
        assert!(matches!(
            p.check("cargo build --release"),
            CommandDecision::Denied { .. }
        ));
    }

    #[test]
    fn empty_allow_requires_approval() {
        let p = CommandPolicy::default();
        assert_eq!(p.check("anything"), CommandDecision::NeedsApproval);
    }
}
