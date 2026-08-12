use serde::{Deserialize, Serialize};
use url::Url;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PolicyDefault {
    #[default]
    Deny,
    Allow,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NetworkPolicy {
    #[serde(default)]
    pub allow_hosts: Vec<String>,
    #[serde(default)]
    pub deny_hosts: Vec<String>,
    #[serde(default)]
    pub default: PolicyDefault,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkDecision {
    Allowed,
    Denied { reason: String },
}

impl NetworkPolicy {
    pub fn check(&self, raw_url: &str) -> NetworkDecision {
        let url = match Url::parse(raw_url) {
            Ok(u) => u,
            Err(_) => {
                return NetworkDecision::Denied {
                    reason: format!("invalid url '{raw_url}'"),
                }
            }
        };
        let host = url.host_str().unwrap_or("").to_lowercase();
        for pattern in &self.deny_hosts {
            let p = pattern.to_lowercase();
            if host == p || host.ends_with(format!(".{p}").as_str()) {
                return NetworkDecision::Denied {
                    reason: format!("host '{host}' is denied"),
                };
            }
        }
        let allowed = self.allow_hosts.iter().any(|p| {
            host == p.to_lowercase() || host.ends_with(format!(".{}", p.to_lowercase()).as_str())
        });
        match (allowed, self.default) {
            (true, _) => NetworkDecision::Allowed,
            (false, PolicyDefault::Deny) => NetworkDecision::Denied {
                reason: format!("host '{host}' is not allowed"),
            },
            (false, PolicyDefault::Allow) => NetworkDecision::Allowed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allow_and_deny_hosts() {
        let p = NetworkPolicy {
            allow_hosts: vec!["api.github.com".into()],
            deny_hosts: vec!["bad.example.com".into()],
            default: PolicyDefault::Deny,
        };
        assert_eq!(
            p.check("https://api.github.com/repos/x"),
            NetworkDecision::Allowed
        );
        assert!(matches!(
            p.check("https://evil.com"),
            NetworkDecision::Denied { .. }
        ));
        assert!(matches!(
            p.check("https://bad.example.com/x"),
            NetworkDecision::Denied { .. }
        ));
    }

    /// F6 回归:deny 与 allow 一样需要域名边界,`evil.com` 不能误伤 `notevil.com`。
    #[test]
    fn deny_hosts_require_dot_boundary() {
        let p = NetworkPolicy {
            allow_hosts: vec![],
            deny_hosts: vec!["evil.com".into()],
            default: PolicyDefault::Allow,
        };
        assert!(matches!(
            p.check("https://evil.com"),
            NetworkDecision::Denied { .. }
        ));
        assert!(matches!(
            p.check("https://sub.evil.com/x"),
            NetworkDecision::Denied { .. }
        ));
        // 与 `evil.com` 无关的域名不应被 deny 命中(默认放行)。
        assert_eq!(
            p.check("https://notevil.com"),
            NetworkDecision::Allowed
        );
    }
}
