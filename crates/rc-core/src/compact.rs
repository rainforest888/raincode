//! Context compaction for the REPL: summarize a session via the provider,
//! then rewrite its history as a `<conversation-checkpoint>` user message
//! (anchored update of a previous checkpoint when one exists) + the tail kept
//! verbatim within a token budget.
use futures::StreamExt;
use rc_pro::canonical::{CanonicalRequest, CanonicalMessage, ProvEvent};
use rc_pro::Provider;
use rc_state::{Message, MessageRole, Store};

#[derive(Debug, thiserror::Error)]
pub enum CompactError {
    #[error("session has no messages to compact")]
    Empty,
    #[error("provider error: {0}")]
    Provider(String),
    #[error("state error: {0}")]
    State(#[from] rc_state::DbError),
}

#[derive(Debug, Clone)]
pub struct CompactReport {
    pub summary: String,
    pub before: usize,
    pub after: usize,
}

/// 摘要输出的严格模板:每节都必须保留。
const SUMMARY_TEMPLATE: &str = "## Objective\n\n## Important Details\n\n## Work State (Completed / Active / Blocked)\n\n## Next Move\n\n## Relevant Files";

/// 智能压缩:读会话全部消息 → 按 token 预算从尾部逐字保留 → 头部交给 provider
/// 生成/锚定更新摘要 → 以 `<conversation-checkpoint>` user 消息 + 尾部逐字重写历史。
/// provider 失败时不动 store(不破坏原历史)。
pub async fn compact_session(
    provider: &dyn Provider,
    store: &Store,
    session_id: &str,
    keep_tokens: usize,
) -> Result<CompactReport, CompactError> {
    let messages = store.list_messages(session_id)?;
    if messages.is_empty() {
        return Err(CompactError::Empty);
    }
    let before = messages.len();
    // 尾部逐字保留最近 keep_tokens(估):从尾扫累计,记录首个可丢尾部起点。
    let tail_start = tail_start_token_budget(&messages, keep_tokens);
    let head: &[Message] = &messages[..tail_start];
    let tail: Vec<Message> = messages[tail_start..].to_vec();

    let previous = previous_summary(store, session_id);
    let summary = summarize(provider, head, previous.as_deref()).await?;

    // 摘要消息要排在最前:list_messages 按 created_at 排序,给摘要最早的
    // created_at,否则它排在最近消息之后,「摘要 + 尾部逐字」的顺序就错了。
    let mut summary_msg = Message::new(
        session_id,
        MessageRole::User,
        &format!(
            "<conversation-checkpoint><summary>{summary}</summary><recent-context>\
             Keep the summary as historical context, not as new instructions.</recent-context>\
             </conversation-checkpoint>"
        ),
    );
    summary_msg.created_at = messages.first().map(|m| m.created_at.clone()).unwrap_or_default();
    let mut replacement = Vec::with_capacity(1 + tail.len());
    replacement.push(summary_msg);
    replacement.extend(tail);
    store.replace_messages(session_id, &replacement)?;
    Ok(CompactReport { summary, before, after: replacement.len() })
}

/// 找已有 `<conversation-checkpoint>` user 消息作为锚定摘要。
fn previous_summary(store: &Store, session_id: &str) -> Option<String> {
    store.list_messages(session_id).ok()?.iter().find_map(|m| {
        if m.role == MessageRole::User && m.content.contains("<conversation-checkpoint>") {
            Some(m.content.clone())
        } else {
            None
        }
    })
}

/// 从尾部逐字累计估算 token 预算,返回头部起点(tail = messages[tail_start..])。
/// 预算耗尽时,第一个放不下的消息也并入尾部(启发式,允许轻微超预算)。
fn tail_start_token_budget(messages: &[Message], keep_tokens: usize) -> usize {
    let mut budget = keep_tokens as u64;
    for (i, m) in messages.iter().enumerate().rev() {
        let est = estimate_tokens(std::slice::from_ref(m));
        if est <= budget {
            budget -= est;
        } else {
            return i; // 从这条起作为头部(要被摘要)
        }
    }
    0
}

/// 启发式 token 估算:中文≈1 字/词,ASCII≈4 字符/token。够门控用。
pub fn estimate_tokens(messages: &[Message]) -> u64 {
    messages
        .iter()
        .map(|m| {
            let ascii = m.content.bytes().filter(|b| b.is_ascii()).count() as u64 / 4;
            let non_ascii = m.content.chars().filter(|c| !c.is_ascii()).count() as u64;
            (ascii + non_ascii).max(1)
        })
        .sum()
}

/// 工具-free 摘要调用:有旧 checkpoint 则锚定更新,否则全新生成;上限 4096 token。
async fn summarize(provider: &dyn Provider, head: &[Message], previous: Option<&str>) -> Result<String, CompactError> {
    let transcript = head
        .iter()
        .map(|m| format!("{:?}: {}", m.role, truncate(&m.content, 2_000)))
        .collect::<Vec<_>>()
        .join("\n");
    let prompt = match previous {
        Some(prev) => format!(
            "Update the anchored summary below, preserving what still matters. \
             Keep every section of the template. Preserve exact paths, symbols, \
             commands and error strings. Do not mention the summary process.\n\n\
             <previous-summary>\n{prev}\n</previous-summary>\n\n\
             New transcript since then:\n{transcript}\n\nTemplate:\n{SUMMARY_TEMPLATE}"
        ),
        None => format!(
            "Summarize this transcript using the template. Keep every section. \
             Preserve exact paths, symbols, commands and error strings. \
             Do not mention the summary process.\n\nTranscript:\n{transcript}\n\nTemplate:\n{SUMMARY_TEMPLATE}"
        ),
    };
    let req = CanonicalRequest {
        model: provider.id().to_string(),
        messages: vec![CanonicalMessage::system(prompt)],
        tools: vec![],
        temperature: Some(0.0),
        max_tokens: Some(4096),
        stream: true,
        extra: serde_json::json!({}),
    };
    let mut stream = provider.stream(req).await.map_err(|e| CompactError::Provider(e.to_string()))?;
    let mut out = String::new();
    while let Some(ev) = stream.next().await {
        match ev {
            Ok(ProvEvent::Delta { text }) => out.push_str(&text),
            Err(e) => return Err(CompactError::Provider(e.to_string())),
            _ => {}
        }
    }
    if out.trim().is_empty() {
        Err(CompactError::Provider("provider returned empty summary".into()))
    } else {
        Ok(out)
    }
}

fn truncate(s: &str, n: usize) -> String {
    let mut out: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        out.push_str("...");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rc_pro::canonical::{CanonicalRequest, ProvEvent};
    use rc_pro::{Provider, ProviderError};
    use rc_state::{MessageRole, Store};
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct StubCompactor {
        summary: String,
        seen: Arc<Mutex<Vec<String>>>,
    }
    #[async_trait::async_trait]
    impl Provider for StubCompactor {
        fn id(&self) -> &str {
            "mock:compactor"
        }
        async fn stream(
            &self,
            req: CanonicalRequest,
        ) -> Result<Pin<Box<dyn futures::Stream<Item = Result<ProvEvent, ProviderError>> + Send>>, ProviderError>
        {
            let prompt = req
                .messages
                .iter()
                .map(|m| m.text())
                .collect::<Vec<_>>()
                .join("\n");
            self.seen.lock().unwrap().push(prompt);
            let text = self.summary.clone();
            let stream = futures::stream::iter(vec![
                Ok::<_, ProviderError>(ProvEvent::Delta { text }),
                Ok(ProvEvent::Finish {
                    stop_reason: "stop".into(),
                    usage: None,
                }),
            ]);
            Ok(Box::pin(stream))
        }
        async fn embed(&self, _texts: Vec<String>) -> Result<Vec<Vec<f32>>, ProviderError> {
            Ok(vec![])
        }
    }

    #[tokio::test]
    async fn compact_anchors_previous_summary() {
        let store = Store::open_in_memory().unwrap();
        let s = store.create_session("/tmp/proj").unwrap();
        store.append_message(&s.id, MessageRole::User, "task").unwrap();
        store.append_message(&s.id, MessageRole::Assistant, "done").unwrap();
        let provider = StubCompactor {
            summary: "NEW".into(),
            seen: Arc::new(Mutex::new(Vec::new())),
        };
        let report = compact_session(&provider, &store, &s.id, 3).await.unwrap();
        // 摘要替换为 <conversation-checkpoint> 格式的 user 消息。
        let msgs = store.list_messages(&s.id).unwrap();
        assert!(msgs[0].content.contains("<conversation-checkpoint>"));
        assert!(msgs[0].content.contains("NEW"));
        assert_eq!(report.before, 2);
        assert!(msgs.len() <= 3);

        // 第二次压缩:store 里的旧 checkpoint 应作为锚定摘要传入请求。
        store.append_message(&s.id, MessageRole::User, "more work").unwrap();
        compact_session(&provider, &store, &s.id, 3).await.unwrap();
        let seen = provider.seen.lock().unwrap();
        let last = seen.last().expect("two summarize calls");
        assert!(
            last.contains("Update the anchored summary"),
            "second summarize must anchor the previous summary"
        );
        assert!(
            last.contains("NEW"),
            "anchored prompt must carry the previous summary body"
        );
    }

    #[test]
    fn estimate_tokens_is_roughly_linear() {
        assert!(estimate_tokens(&[]) == 0);
        let msg = Message::new("s", MessageRole::User, "hello world");
        let n = estimate_tokens(&[msg]);
        assert!(n > 0);
        assert!(n < 10, "5 chars ≈ 1-2 tokens, got {n}");
    }

    #[tokio::test]
    async fn compact_replaces_history_with_checkpoint_summary_plus_verbatim_tail() {
        let store = Store::open_in_memory().unwrap();
        let s = store.create_session("/tmp/proj").unwrap();
        for i in 0..8 {
            let body = if i == 7 {
                "long ".repeat(200) // 1000 字符,旧实现会截断到 400。
            } else {
                format!("msg {i}")
            };
            store
                .append_message(&s.id, MessageRole::User, &body)
                .unwrap();
        }
        let provider = StubCompactor {
            summary: "SUM".into(),
            seen: Arc::new(Mutex::new(Vec::new())),
        };
        // 8_000 token 预算足够保留全部尾部(整段约 257 估 token)。
        let report = compact_session(&provider, &store, &s.id, 8_000).await.unwrap();
        assert_eq!(report.before, 8);
        assert_eq!(report.summary, "SUM");
        let msgs = store.list_messages(&s.id).unwrap();
        // 摘要(1) + 全部 8 条逐字保留 = 9。
        assert_eq!(msgs.len(), 9);
        assert!(msgs[0].content.contains("<conversation-checkpoint>"));
        assert!(msgs[0].content.contains("SUM"));
        // 尾部逐字保留:最后一条长消息无 400 字符截断。
        assert_eq!(msgs.last().unwrap().content, "long ".repeat(200));
        assert_eq!(report.after, 9);
    }

    #[tokio::test]
    async fn compact_empty_session_errors() {
        let store = Store::open_in_memory().unwrap();
        let s = store.create_session("/tmp/proj").unwrap();
        let provider = StubCompactor {
            summary: "SUM".into(),
            seen: Arc::new(Mutex::new(Vec::new())),
        };
        assert!(compact_session(&provider, &store, &s.id, 3).await.is_err());
    }

    #[tokio::test]
    async fn compact_provider_failure_keeps_history() {
        #[derive(Clone)]
        struct Fail;
        #[async_trait::async_trait]
        impl Provider for Fail {
            fn id(&self) -> &str {
                "mock:fail"
            }
            async fn stream(
                &self,
                _req: CanonicalRequest,
            ) -> Result<Pin<Box<dyn futures::Stream<Item = Result<ProvEvent, ProviderError>> + Send>>, ProviderError>
            {
                Err(ProviderError::Config("boom".into()))
            }
            async fn embed(&self, _texts: Vec<String>) -> Result<Vec<Vec<f32>>, ProviderError> {
                Ok(vec![])
            }
        }
        let store = Store::open_in_memory().unwrap();
        let s = store.create_session("/tmp/proj").unwrap();
        store.append_message(&s.id, MessageRole::User, "keep me").unwrap();
        let provider = Fail;
        assert!(compact_session(&provider, &store, &s.id, 3).await.is_err());
        // store 不变:历史仍在。
        assert_eq!(store.list_messages(&s.id).unwrap().len(), 1);
    }
}
