//! 用户授权闸:高危操作的三选一同意 + 会话级记忆。
use async_trait::async_trait;
use std::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardConsent { Once, Session, Forever, Deny }

impl GuardConsent {
    pub fn as_str(&self) -> &'static str {
        match self { GuardConsent::Once => "once", GuardConsent::Session => "session", GuardConsent::Forever => "forever", GuardConsent::Deny => "deny" }
    }
}

#[derive(Debug, Clone)]
pub struct GuardRequest {
    pub tool: String,
    pub reason: String,
    pub command: Option<String>,
    pub path: Option<String>,
}

#[async_trait]
pub trait GuardHook: Send + Sync {
    async fn ask(&self, req: &GuardRequest) -> GuardConsent;
}

/// Delegates to a closure (used by the TUI: sends the request to the main loop
/// and blocks on the user's 0/1/2/3 answer).
pub struct PromptGuardHook<F> {
    f: F,
}

impl<F> PromptGuardHook<F>
where
    F: Fn(&GuardRequest) -> GuardConsent + Send + Sync,
{
    pub fn new(f: F) -> Self {
        Self { f }
    }
}

#[async_trait]
impl<F> GuardHook for PromptGuardHook<F>
where
    F: Fn(&GuardRequest) -> GuardConsent + Send + Sync,
{
    async fn ask(&self, req: &GuardRequest) -> GuardConsent {
        (self.f)(req)
    }
}

/// 会话级放行记忆:一次 Session 同意后,后续同类请求不再弹。
#[derive(Default)]
pub struct SessionGuardMemo {
    inner: Mutex<Vec<String>>,
}

fn request_key(req: &GuardRequest) -> String {
    // 实例级键:同一 reason 命中的不同命令/路径不得共享一次 Session 同意。
    format!(
        "{}|{}|{}|{}",
        req.tool,
        req.reason,
        req.command.as_deref().unwrap_or(""),
        req.path.as_deref().unwrap_or("")
    )
}

pub fn memo_allows(memo: &SessionGuardMemo, req: &GuardRequest) -> bool {
    memo.inner.lock().map(|v| v.contains(&request_key(req))).unwrap_or(false)
}

pub fn memo_record(memo: &SessionGuardMemo, req: &GuardRequest) {
    if let Ok(mut v) = memo.inner.lock() {
        v.push(request_key(req));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_consent_strings() {
        assert_eq!(GuardConsent::Once.as_str(), "once");
        assert_eq!(GuardConsent::Session.as_str(), "session");
        assert_eq!(GuardConsent::Forever.as_str(), "forever");
        assert_eq!(GuardConsent::Deny.as_str(), "deny");
    }

    #[test]
    fn session_memo_records_and_allows() {
        let memo = SessionGuardMemo::default();
        let req = GuardRequest {
            tool: "run_shell".into(),
            reason: "command matches deny pattern 'rm -rf'".into(),
            command: Some("rm -rf /proj/build".into()),
            path: None,
        };
        assert!(!memo_allows(&memo, &req));
        memo_record(&memo, &req);
        assert!(memo_allows(&memo, &req));
    }

    #[test]
    fn session_memo_distinct_requests() {
        let memo = SessionGuardMemo::default();
        let req1 = GuardRequest {
            tool: "run_shell".into(),
            reason: "a".into(),
            command: Some("rm -rf /x".into()),
            path: None,
        };
        let req2 = GuardRequest {
            tool: "run_shell".into(),
            reason: "b".into(),
            command: Some("rm -rf /y".into()),
            path: None,
        };
        memo_record(&memo, &req1);
        assert!(memo_allows(&memo, &req1));
        assert!(!memo_allows(&memo, &req2));
    }

    /// F1 回归:Session 记忆键必须是实例级(含具体命令),不能是 pattern 级。
    /// 同一个 deny pattern 命中两条不同命令时,一次 Session 同意不能放行另一条。
    #[test]
    fn session_memo_key_is_instance_specific() {
        let memo = SessionGuardMemo::default();
        let mk = |cmd: &str| GuardRequest {
            tool: "run_shell".into(),
            reason: "command matches deny pattern 'rm -rf'".into(),
            command: Some(cmd.into()),
            path: None,
        };
        let req_a = mk("rm -rf /proj/build");
        let req_b = mk("rm -rf /etc");
        memo_record(&memo, &req_a);
        assert!(memo_allows(&memo, &req_a));
        assert!(
            !memo_allows(&memo, &req_b),
            "Session consent for one command must not allow a different command with the same reason"
        );
    }
}
