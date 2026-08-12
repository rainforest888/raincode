use crate::capability::{SubtaskGraph, parse_subtask_graph};
use futures::StreamExt;
use rc_pro::Provider;
use rc_pro::canonical::{CanonicalMessage, CanonicalRequest};

#[derive(Debug, thiserror::Error)]
pub enum AllocatorError {
    #[error("provider stream failed: {0}")]
    Provider(#[from] rc_pro::ProviderError),
    #[error("parse failed: {0}")]
    Parse(#[from] crate::capability::ParseError),
    #[error("allocator produced no text")]
    Empty,
    #[error("allocator timed out")]
    TimedOut,
    #[error("cancelled by user")]
    Cancelled,
}

const DECOMPOSE_PROMPT: &str = r#"You are the allocator. Decompose the user's task into a
structured subtask graph. Return ONLY a JSON object, no prose, no markdown outside a ```json fence:

{"intent": "<one line>",
 "subtasks": [{"id": "s1", "description": "<one line>",
   "requirements": {"reasoning": 0.0-1.0, "coding": 0.0-1.0, "frontend": 0.0-1.0,
                    "backend": 0.0-1.0, "math": 0.0-1.0, "long_context": 0.0-1.0},
   "cost_pressure": "low|med|high", "depends_on": ["<id>", ...], "risk": "low|med|high"}, ...]}

Rules: weights for a subtask sum to 1.0. Use depends_on for ordering (later subtasks depend on
earlier ones). Mark risk high for destructive/irreversible/shared-workspace operations.
Be FAST: do not think at length, emit the JSON immediately.
Task: "#;

/// 消费 provider 流聚合 Delta 文本,带硬超时。
/// 推理模型(DeepSeek 等)可能长时间只吐 thinking、content 迟迟不出;没有
/// 超时会让 route 无限卡住。超时返回 `TimedOut`。
async fn decompose_stream(
    provider: &dyn Provider,
    req: CanonicalRequest,
    timeout: std::time::Duration,
) -> Result<String, AllocatorError> {
    let timed = tokio::time::timeout(timeout, async {
        let mut stream = provider.stream(req).await?;
        let mut text = String::new();
        while let Some(ev) = stream.next().await {
            let ev = ev.map_err(AllocatorError::Provider)?;
            match ev {
                rc_pro::canonical::ProvEvent::Delta { text: t } => text.push_str(&t),
                // 推理模型(deepseek-v4 等)把大部分 token 花在 thinking 上,content 可能
                // 迟迟不出或不够;把 reasoning 也累加,JSON 常出现在思考里。
                rc_pro::canonical::ProvEvent::Thinking { text: t } => text.push_str(&t),
                rc_pro::canonical::ProvEvent::Error { message } => {
                    return Err(AllocatorError::Provider(rc_pro::ProviderError::Transport(message)));
                }
                _ => {}
            }
        }
        Ok(text)
    })
    .await;
    match timed {
        Ok(Ok(t)) => Ok(t),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(AllocatorError::TimedOut),
    }
}

/// 调用分配者 provider 把任务拆解成子任务图。20s 硬超时(快速失败,降级单 agent)。
pub async fn decompose(provider: &dyn Provider, task: &str) -> Result<SubtaskGraph, AllocatorError> {
    let prompt = format!("{DECOMPOSE_PROMPT}\n{task}");
    let req = CanonicalRequest {
        model: provider.id().to_string(),
        messages: vec![CanonicalMessage::user(prompt)],
        tools: vec![],
        temperature: Some(0.2),
        // 推理模型会把 token 花在思考上;3000 给 reasoning + content 都留足空间,
        // 配合 decompose_stream 累加 Thinking,避免"只思考没内容"→ 拆解失败。
        max_tokens: Some(3000),
        stream: true,
        extra: serde_json::json!({}),
    };
    let text = decompose_stream(provider, req, std::time::Duration::from_secs(20)).await?;
    if text.trim().is_empty() { return Err(AllocatorError::Empty); }
    let graph = parse_subtask_graph(&text)?;
    if graph.subtasks.is_empty() {
        return Err(AllocatorError::Parse(crate::capability::ParseError::NoJson(
            "no subtasks parsed".into(),
        )));
    }
    Ok(graph)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rc_pro::Provider;
    use rc_pro::canonical::{CanonicalRequest, ProvEvent};
    use rc_pro::provider::ProvStream;
    use std::pin::Pin;

    // 伪 provider:固定返回一段 JSON 子任务图
    struct StubAllocator;
    #[async_trait::async_trait]
    impl Provider for StubAllocator {
        fn id(&self) -> &str { "mock:allocator" }
        async fn stream(&self, _req: CanonicalRequest) -> Result<ProvStream, rc_pro::ProviderError> {
            let text = "```json\n{\"intent\":\"build app\",\"subtasks\":[{\"id\":\"s1\",\"description\":\"backend api\",\"requirements\":{\"backend\":0.8,\"coding\":0.2},\"cost_pressure\":\"med\",\"depends_on\":[],\"risk\":\"med\"},{\"id\":\"s2\",\"description\":\"react page\",\"requirements\":{\"frontend\":0.8},\"cost_pressure\":\"high\",\"depends_on\":[\"s1\"],\"risk\":\"low\"}]}\n```";
            let stream = futures::stream::iter(vec![
                Ok::<_, rc_pro::ProviderError>(ProvEvent::Delta { text: text.to_string() }),
                Ok(ProvEvent::Finish { stop_reason: "stop".into(), usage: None }),
            ]);
            Ok(Pin::from(Box::new(stream)))
        }
        async fn embed(&self, _texts: Vec<String>) -> Result<Vec<Vec<f32>>, rc_pro::ProviderError> {
            Ok(vec![])
        }
    }

    #[tokio::test]
    async fn decompose_builds_subtask_graph() {
        let provider = StubAllocator;
        let g = decompose(&provider, "build an app").await.unwrap();
        assert_eq!(g.subtasks.len(), 2);
        assert_eq!(g.subtasks[1].depends_on, vec!["s1".to_string()]);
    }

    // 伪 provider:永不结束(只吐 thinking),验证 decompose 超时而非无限卡住。
    struct StuckAllocator;
    #[async_trait::async_trait]
    impl Provider for StuckAllocator {
        fn id(&self) -> &str { "mock:stuck" }
        async fn stream(&self, _req: CanonicalRequest) -> Result<ProvStream, rc_pro::ProviderError> {
            // 永不平息的流(不产 content),模拟推理模型卡在 thinking。
            let stream = futures::stream::pending::<Result<ProvEvent, rc_pro::ProviderError>>();
            Ok(Pin::from(Box::new(stream)))
        }
        async fn embed(&self, _texts: Vec<String>) -> Result<Vec<Vec<f32>>, rc_pro::ProviderError> {
            Ok(vec![])
        }
    }

    #[tokio::test]
    async fn decompose_times_out_on_thinking_only_stream() {
        let provider = StuckAllocator;
        let req = CanonicalRequest {
            model: provider.id().to_string(),
            messages: vec![CanonicalMessage::user("task")],
            tools: vec![],
            temperature: None,
            max_tokens: None,
            stream: true,
            extra: serde_json::json!({}),
        };
        // 用短超时验证机制,而非真实 60s。
        let err = decompose_stream(&provider, req, std::time::Duration::from_millis(200))
            .await
            .unwrap_err();
        assert!(matches!(err, AllocatorError::TimedOut));
    }
}
