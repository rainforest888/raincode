//! 监督 agent:订阅子 agent 事件流,批量 LLM 判断敏感行为,输出 3 级动作。
use crate::SteerHub;
use futures::StreamExt;
use rc_pro::canonical::{CanonicalMessage, CanonicalRequest, ProvEvent};
use rc_pro::Provider;
use rc_proto::AgentEvent;
use rc_sandbox::guard::SuperviseConfig;
use serde_json::json;
use std::time::Instant;

const JUDGE_BATCH: usize = 5;

#[derive(Debug)]
pub enum SupervisorAction {
    Observe,
    Suggest { reason: String },
    Interrupt { agent_id: String, reason: String },
}

pub struct SupervisorBatch {
    pub events: Vec<AgentEvent>,
    pub since: Instant,
}

pub struct Supervisor {
    pub provider: Box<dyn Provider>,
    pub cfg: SuperviseConfig,
    pub boundaries: String,
}

/// 从 AgentEvent 提取子 agent id(有 id 的事件才需要监督判断)。
pub fn agent_id_of(ev: &AgentEvent) -> Option<String> {
    match ev {
        AgentEvent::AgentSpawned { id, .. }
        | AgentEvent::AgentToolCall { id, .. }
        | AgentEvent::AgentStatus { id, .. }
        | AgentEvent::AgentResult { id, .. } => Some(id.clone()),
        _ => None,
    }
}

impl Supervisor {
    /// 每累计 JUDGE_BATCH 个工具调用事件就判断一次。
    pub fn should_judge(&self, batch: &SupervisorBatch) -> bool {
        batch
            .events
            .iter()
            .filter(|e| matches!(e, AgentEvent::AgentToolCall { .. }))
            .count()
            >= JUDGE_BATCH
    }

    /// 批量判断:把策略边界 + 工具调用摘要发给监督模型,解析返回动作。
    pub async fn judge(&self, batch: &SupervisorBatch) -> SupervisorAction {
        let summary: Vec<String> = batch
            .events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::AgentToolCall {
                    id,
                    tool,
                    args_preview,
                } => Some(format!("{id} called {tool}({args_preview})")),
                AgentEvent::AgentSpawned { id, task, .. } => Some(format!("{id} spawned: {task}")),
                AgentEvent::AgentResult { id, verdict, .. } => {
                    Some(format!("{id} result: {verdict}"))
                }
                _ => None,
            })
            .collect();
        let prompt = format!(
            "You are the Raincode supervision agent. Review these sub-agent actions against the boundaries.\n\
             Boundaries:\n{}\n\
             Actions:\n{}\n\
             Reply with EXACTLY one line: OBSERVE | SUGGEST <reason> | INTERRUPT <agent_id> <reason>",
            self.boundaries,
            summary.join("\n")
        );
        let req = CanonicalRequest {
            model: self.provider.id().to_string(),
            messages: vec![
                CanonicalMessage::system("You are a strict safety supervisor. Be decisive."),
                CanonicalMessage::user(prompt),
            ],
            tools: vec![],
            temperature: Some(0.0),
            max_tokens: Some(120),
            stream: true,
            extra: json!({}),
        };
        // 20s 硬超时(与 allocator 一致):挂死的供应商不阻塞监督 —— 超时返回 Observe,
        // 并让 TUI 清掉 sup_judge_in_flight(否则监督被永久静默禁用)。
        let out = judge_stream(
            self.provider.as_ref(),
            req,
            std::time::Duration::from_secs(20),
        )
        .await;
        let line = out.trim();
        if let Some(rest) = line.strip_prefix("INTERRUPT ") {
            let mut parts = rest.splitn(2, ' ');
            let agent_id = parts.next().unwrap_or("").to_string();
            let reason = parts.next().unwrap_or("").to_string();
            if !agent_id.is_empty() {
                return SupervisorAction::Interrupt { agent_id, reason };
            }
        }
        if let Some(reason) = line.strip_prefix("SUGGEST ") {
            return SupervisorAction::Suggest {
                reason: reason.to_string(),
            };
        }
        SupervisorAction::Observe
    }

    /// 应用监督动作到子 agent:Interrupt → SteerHub 注入 STOP + 返回要取消的 id。
    pub fn apply(&self, action: &SupervisorAction, hub: &SteerHub) -> Option<String> {
        match action {
            SupervisorAction::Interrupt { agent_id, reason } => {
                hub.send(agent_id, &format!("STOP: {reason}"));
                Some(agent_id.clone())
            }
            _ => None,
        }
    }
}

/// 消费监督 provider 流聚合 Delta 文本,带硬超时。
/// 挂死的供应商(只吐 thinking 或完全卡住)在 timeout 后返回空串 → judge 回退
/// Observe,不让监督永久静默关闭(sup_judge_in_flight 一直被占用)。
async fn judge_stream(
    provider: &dyn Provider,
    req: CanonicalRequest,
    timeout: std::time::Duration,
) -> String {
    let timed = tokio::time::timeout(timeout, async {
        let mut stream = match provider.stream(req).await {
            Ok(s) => s,
            Err(_) => return String::new(),
        };
        let mut out = String::new();
        while let Some(ev) = stream.next().await {
            if let Ok(ProvEvent::Delta { text }) = ev {
                out.push_str(&text);
            }
        }
        out
    })
    .await;
    timed.unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone)]
    struct StubSupervise;
    #[async_trait::async_trait]
    impl rc_pro::Provider for StubSupervise {
        fn id(&self) -> &str {
            "mock:supervise"
        }
        async fn stream(
            &self,
            _req: rc_pro::canonical::CanonicalRequest,
        ) -> Result<
            std::pin::Pin<
                Box<
                    dyn futures::Stream<
                            Item = Result<rc_pro::canonical::ProvEvent, rc_pro::ProviderError>,
                        > + Send,
                >,
            >,
            rc_pro::ProviderError,
        > {
            let stream = futures::stream::iter(vec![
                Ok::<_, rc_pro::ProviderError>(rc_pro::canonical::ProvEvent::Delta {
                    text: "INTERRUPT s1 because secrets leak".into(),
                }),
                Ok(rc_pro::canonical::ProvEvent::Finish {
                    stop_reason: "stop".into(),
                    usage: None,
                }),
            ]);
            Ok(Box::pin(stream))
        }
        async fn embed(&self, _t: Vec<String>) -> Result<Vec<Vec<f32>>, rc_pro::ProviderError> {
            Ok(vec![])
        }
    }

    #[tokio::test]
    async fn supervisor_judge_parses_interrupt() {
        let sup = Supervisor {
            provider: Box::new(StubSupervise),
            cfg: SuperviseConfig::default(),
            boundaries: "no secrets".into(),
        };
        let batch = SupervisorBatch {
            events: vec![rc_proto::AgentEvent::AgentToolCall {
                id: "s1".into(),
                tool: "write_file".into(),
                args_preview: "sk-xxx".into(),
            }],
            since: std::time::Instant::now(),
        };
        let action = sup.judge(&batch).await;
        match action {
            SupervisorAction::Interrupt { agent_id, reason } => {
                assert_eq!(agent_id, "s1");
                assert!(reason.contains("secrets"));
            }
            other => panic!("expected Interrupt, got {other:?}"),
        }
    }

    #[derive(Clone)]
    struct StuckSupervise;
    #[async_trait::async_trait]
    impl rc_pro::Provider for StuckSupervise {
        fn id(&self) -> &str {
            "mock:stuck-supervise"
        }
        async fn stream(
            &self,
            _req: rc_pro::canonical::CanonicalRequest,
        ) -> Result<
            std::pin::Pin<
                Box<
                    dyn futures::Stream<
                            Item = Result<rc_pro::canonical::ProvEvent, rc_pro::ProviderError>,
                        > + Send,
                >,
            >,
            rc_pro::ProviderError,
        > {
            // 永不平息的流:模拟挂死的监督供应商。
            let stream = futures::stream::pending::<
                Result<rc_pro::canonical::ProvEvent, rc_pro::ProviderError>,
            >();
            Ok(Box::pin(stream))
        }
        async fn embed(&self, _t: Vec<String>) -> Result<Vec<Vec<f32>>, rc_pro::ProviderError> {
            Ok(vec![])
        }
    }

    #[tokio::test]
    async fn judge_stream_times_out_on_hung_provider() {
        let sup = Supervisor {
            provider: Box::new(StuckSupervise),
            cfg: SuperviseConfig::default(),
            boundaries: "no secrets".into(),
        };
        let req = CanonicalRequest {
            model: sup.provider.id().to_string(),
            messages: vec![CanonicalMessage::user("x")],
            tools: vec![],
            temperature: None,
            max_tokens: None,
            stream: true,
            extra: json!({}),
        };
        // 短超时验证机制(judge 内部用 20s);超时 → 空串 → judge 回退 Observe。
        let out = judge_stream(
            sup.provider.as_ref(),
            req,
            std::time::Duration::from_millis(200),
        )
        .await;
        assert!(out.is_empty(), "timeout must yield empty output");
    }

    #[test]
    fn agent_id_extracted_from_events() {
        assert_eq!(
            agent_id_of(&rc_proto::AgentEvent::AgentToolCall {
                id: "s2".into(),
                tool: "x".into(),
                args_preview: "".into()
            }),
            Some("s2".to_string())
        );
        assert_eq!(
            agent_id_of(&rc_proto::AgentEvent::Token { delta: "hi".into() }),
            None
        );
    }

    #[test]
    fn should_judge_on_batch_size() {
        let sup = Supervisor {
            provider: Box::new(StubSupervise),
            cfg: SuperviseConfig::default(),
            boundaries: String::new(),
        };
        let mut small = SupervisorBatch {
            events: vec![rc_proto::AgentEvent::Token { delta: "a".into() }],
            since: std::time::Instant::now(),
        };
        assert!(!sup.should_judge(&small));
        small.events = (0..5)
            .map(|i| rc_proto::AgentEvent::AgentToolCall {
                id: format!("s{i}"),
                tool: "x".into(),
                args_preview: "".into(),
            })
            .collect();
        assert!(sup.should_judge(&small));
    }
}
