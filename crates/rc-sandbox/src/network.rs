use serde::{Deserialize, Serialize};
use url::Url;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PolicyDefault {
    /// 默认放行:web_fetch/web_search 开箱即用。上传/密钥外发仍由 guard 的
    /// 上传意图 + 密钥扫描拦截(否则 agent 完全无法联网查资料)。
    #[default]
    Allow,
    Deny,
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
        // DNS 根点:`example.com.` 与 `example.com` 是同一域名。host_str 保留尾点,
        // 不先去掉会让 deny/allow 的域名边界匹配被尾点绕过。
        let host = url.host_str().unwrap_or("").trim_end_matches('.').to_lowercase();
        for pattern in &self.deny_hosts {
            let p = pattern.trim_end_matches('.').to_lowercase();
            if host == p || host.ends_with(format!(".{p}").as_str()) {
                return NetworkDecision::Denied {
                    reason: format!("host '{host}' is denied"),
                };
            }
        }
        let allowed = self.allow_hosts.iter().any(|p| {
            let p = p.trim_end_matches('.').to_lowercase();
            host == p || host.ends_with(format!(".{p}").as_str())
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

    /// host_str 提取:端口、userinfo、子域、IPv4 等正常场景不影响 host。
    #[test]
    fn host_str_normal_cases() {
        let cases = [
            ("https://example.com/x", "example.com"),
            ("https://example.com:8080/x", "example.com"),
            ("https://user:pass@example.com/x", "example.com"),
            ("https://sub.example.com/x", "sub.example.com"),
            ("https://127.0.0.1/x", "127.0.0.1"),
        ];
        for (raw, want) in cases {
            assert_eq!(Url::parse(raw).unwrap().host_str(), Some(want), "raw={raw}");
        }
    }

    /// host_str 边界场景:尾点保留、IDN 归一化为 punycode、IPv6 带方括号。
    #[test]
    fn host_str_edge_cases() {
        let cases = [
            ("https://example.com./x", "example.com."),
            ("https://例子.测试/x", "xn--fsqu00a.xn--0zwm56d"),
            ("https://xn--fsqu00a.xn--0zwm56d/x", "xn--fsqu00a.xn--0zwm56d"),
            ("https://[2001:db8::1]/x", "[2001:db8::1]"),
            ("https://[::1]/x", "[::1]"),
        ];
        for (raw, want) in cases {
            assert_eq!(Url::parse(raw).unwrap().host_str(), Some(want), "raw={raw}");
        }
    }

    /// DNS 根点尾点不应绕过边界匹配:`evil.com.` 与 `evil.com` 是同一域名,
    /// 应同样被 deny;allow 也应对 `example.com.` 放行。
    #[test]
    fn trailing_root_dot_does_not_bypass_boundary() {
        let p = NetworkPolicy {
            allow_hosts: vec![],
            deny_hosts: vec!["evil.com".into()],
            default: PolicyDefault::Allow,
        };
        assert!(matches!(
            p.check("https://evil.com./x"),
            NetworkDecision::Denied { .. }
        ));
        assert!(matches!(
            p.check("https://sub.evil.com./x"),
            NetworkDecision::Denied { .. }
        ));

        let p = NetworkPolicy {
            allow_hosts: vec!["example.com".into()],
            deny_hosts: vec![],
            default: PolicyDefault::Deny,
        };
        assert_eq!(
            p.check("https://example.com./x"),
            NetworkDecision::Allowed
        );
    }

    /// 匹配大小写不敏感(host 与策略模式都归一化为小写)。
    #[test]
    fn host_matching_is_case_insensitive() {
        let p = NetworkPolicy {
            allow_hosts: vec!["API.GITHUB.COM".into()],
            deny_hosts: vec!["EVIL.COM".into()],
            default: PolicyDefault::Deny,
        };
        assert_eq!(
            p.check("https://api.github.com/repos/x"),
            NetworkDecision::Allowed
        );
        assert!(matches!(
            p.check("https://evil.com/x"),
            NetworkDecision::Denied { .. }
        ));
    }

    /// IDN 域名:host_str 为 punycode,策略模式需按 punycode 书写才能命中。
    #[test]
    fn idn_hosts_match_by_punycode_pattern() {
        let p = NetworkPolicy {
            allow_hosts: vec!["xn--fsqu00a.xn--0zwm56d".into()],
            deny_hosts: vec![],
            default: PolicyDefault::Deny,
        };
        assert_eq!(
            p.check("https://例子.测试/x"),
            NetworkDecision::Allowed
        );
        // 其他域名不在 allow 列表,默认 Deny 应拒绝。
        assert!(matches!(
            p.check("https://other.example/x"),
            NetworkDecision::Denied { .. }
        ));
    }

    /// IPv6 host_str 带方括号,策略模式需同样书写。
    #[test]
    fn ipv6_hosts_match_with_brackets() {
        let p = NetworkPolicy {
            allow_hosts: vec!["[::1]".into()],
            deny_hosts: vec![],
            default: PolicyDefault::Deny,
        };
        assert_eq!(p.check("https://[::1]/x"), NetworkDecision::Allowed);
        assert!(matches!(
            p.check("https://[2001:db8::1]/x"),
            NetworkDecision::Denied { .. }
        ));
    }

    /// 非法 URL 无法解析 host,直接拒绝。
    #[test]
    fn invalid_url_is_denied() {
        let p = NetworkPolicy {
            allow_hosts: vec![],
            deny_hosts: vec![],
            default: PolicyDefault::Allow,
        };
        assert!(matches!(
            p.check("not a url"),
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
