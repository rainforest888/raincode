//! Append-only context machinery for byte-stable prompt prefixes.
//!
//! StablePrefix freezes the system prompt + tool definitions for a session and
//! fingerprints them; providers that do prefix caching (Anthropic cache_control,
//! OpenAI/OpenRouter prompt_cache_key) then hit the identical bytes every turn.
//! Derived from pi's append-only-context.ts and opencode's context-epoch.
//!
//! 【VESTIGIAL】本模块的核心工具(StablePrefix、truncate_to_longest_stable_prefix)
//! 当前不被 run loop 调用:字节稳定性现在由结构保证 —— 每次请求前 system prompt 与
//! tool_defs 统一重建,压缩走「front-trim 保留前缀 + 整日志重建」,不逐条截断。
//! 这些是声音的独立工具,保留给未来 in-place-rewrite 场景(Plan B / 显式前缀缓存命中)。
use rc_pro::canonical::{CanonicalMessage, ToolDef};

#[derive(Debug, Clone)]
pub struct StablePrefix {
    pub system_prompt: String,
    pub tools: Vec<ToolDef>,
    fingerprint: u64,
}

impl StablePrefix {
    pub fn build(system_prompt: String, tools: Vec<ToolDef>) -> Self {
        let fingerprint = prefix_fingerprint(&system_prompt, &tools);
        Self {
            system_prompt,
            tools,
            fingerprint,
        }
    }

    pub fn fingerprint(&self) -> u64 {
        self.fingerprint
    }

    /// 指纹相同 ⇒ 底层输入字节未变 ⇒ 可复用完全相同的 system+tools 前缀。
    pub fn matches(&self, system_prompt: &str, tools: &[ToolDef]) -> bool {
        self.fingerprint == prefix_fingerprint(system_prompt, tools)
    }

    /// 强制重建前缀(MCP 重连 / 工具集变化 / 模型切换)。会丢缓存命中,保持稳定优先。
    pub fn invalidate(&mut self) {
        self.fingerprint = u64::MAX;
    }
}

fn prefix_fingerprint(system_prompt: &str, tools: &[ToolDef]) -> u64 {
    let mut acc = crate::content_hash(system_prompt);
    for t in tools {
        acc.push('|');
        acc.push_str(&t.name);
        acc.push('|');
        acc.push_str(&t.description);
        acc.push('|');
        acc.push_str(&serde_json::to_string(&t.input_schema).unwrap_or_default());
    }
    // 复用 lib.rs 的 DefaultHasher(进程内确定性,会话级缓存足够)。
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    acc.hash(&mut h);
    h.finish()
}

/// 消息级摘要:序列化角色 + 文本 + 工具调用 + tool_call_id + name。
/// 覆盖 provider 可能序列化的每个字段,用于 AppendOnlyLog 找最长字节稳定前缀。
pub fn message_digest(m: &CanonicalMessage) -> u64 {
    let mut acc = format!("{:?}|", m.role);
    acc.push_str(&m.text());
    for c in &m.tool_calls {
        acc.push_str(&format!(
            "|{}|{}|{}",
            c.id,
            c.name,
            serde_json::to_string(&c.arguments).unwrap_or_default()
        ));
    }
    if let Some(id) = &m.tool_call_id {
        acc.push_str(&format!("|tid:{id}"));
    }
    if let Some(name) = &m.name {
        acc.push_str(&format!("|n:{name}"));
    }
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    acc.hash(&mut h);
    h.finish()
}

/// 消息层只追加日志。原地重写(压缩/steering 重包)时按 per-message digest
/// 截断回最长字节稳定前缀,再追加分歧尾巴——provider KV 缓存从分歧点才失效。
#[derive(Debug, Clone)]
pub struct AppendOnlyLog {
    messages: Vec<CanonicalMessage>,
}

impl AppendOnlyLog {
    pub fn new(messages: Vec<CanonicalMessage>) -> Self {
        Self { messages }
    }

    pub fn push(&mut self, m: CanonicalMessage) {
        self.messages.push(m);
    }

    pub fn extend(&mut self, ms: Vec<CanonicalMessage>) {
        self.messages.extend(ms);
    }

    pub fn as_slice(&self) -> &[CanonicalMessage] {
        &self.messages
    }

    pub fn into_messages(self) -> Vec<CanonicalMessage> {
        self.messages
    }

    /// 比较 other,返回「本日志中与 other 逐条 digest 相同」的最长前缀条数,
    /// 并把日志截断到该条数。返回被保留的条数(0 = 前缀完全不共享)。
    /// 【VESTIGIAL】当前压缩不走此路径(见模块文档);保留给未来 in-place-rewrite。
    pub fn truncate_to_longest_stable_prefix(&mut self, other: &[CanonicalMessage]) -> usize {
        let max = self.messages.len().min(other.len());
        let mut kept = 0;
        for (i, theirs) in other.iter().take(max).enumerate() {
            if message_digest(&self.messages[i]) == message_digest(theirs) {
                kept = i + 1;
            } else {
                break;
            }
        }
        self.messages.truncate(kept);
        kept
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn stable_prefix_reuses_bytes_when_unchanged() {
        let tools = vec![ToolDef {
            name: "read_file".into(),
            description: "read a file".into(),
            input_schema: json!({"type": "object"}),
        }];
        let p1 = StablePrefix::build("sys".into(), tools.clone());
        let p2 = StablePrefix::build("sys".into(), tools.clone());
        assert_eq!(p1.fingerprint(), p2.fingerprint());
        assert!(p2.matches("sys", &tools), "same bytes must match");
    }

    #[test]
    fn stable_prefix_invalidates_and_rebuilds_on_tool_change() {
        let tools = vec![ToolDef {
            name: "read_file".into(),
            description: "read a file".into(),
            input_schema: json!({"type": "object"}),
        }];
        let mut p = StablePrefix::build("sys".into(), tools.clone());
        p.invalidate();
        assert!(!p.matches("sys", &tools), "invalidated prefix must not match");
        let changed = vec![ToolDef {
            name: "run_shell".into(),
            description: "run a shell".into(),
            input_schema: json!({"type": "object"}),
        }];
        assert!(!p.matches("sys", &changed), "tool change must break the match");
    }

    #[test]
    fn message_digest_differs_on_content_and_role() {
        let a = CanonicalMessage::user("hello");
        let b = CanonicalMessage::user("world");
        let c = CanonicalMessage::assistant_text("hello");
        assert_ne!(message_digest(&a), message_digest(&b), "content differs");
        assert_ne!(message_digest(&a), message_digest(&c), "role differs");
        assert_eq!(message_digest(&a), message_digest(&CanonicalMessage::user("hello")));
    }

    #[test]
    fn message_digest_differs_on_tool_name() {
        let bash = CanonicalMessage::tool("id1", "bash", "out");
        let read_file = CanonicalMessage::tool("id1", "read_file", "out");
        assert_ne!(
            message_digest(&bash),
            message_digest(&read_file),
            "same tool_call_id + output but different name must not collide"
        );
    }

    #[test]
    fn append_only_log_grows_only() {
        let mut log = AppendOnlyLog::new(vec![CanonicalMessage::system("s")]);
        log.push(CanonicalMessage::user("u1"));
        log.extend(vec![CanonicalMessage::assistant_text("a"), CanonicalMessage::user("u2")]);
        assert_eq!(log.as_slice().len(), 4);
    }

    #[test]
    fn truncate_finds_longest_shared_prefix_by_digest() {
        // 前两条与 other 相同 → 稳定前缀长度 2;第三条起分歧。
        let base = vec![
            CanonicalMessage::system("s"),
            CanonicalMessage::user("u1"),
            CanonicalMessage::assistant_text("old-tail"),
        ];
        let divergent = vec![
            CanonicalMessage::system("s"),
            CanonicalMessage::user("u1"),
            CanonicalMessage::assistant_text("new-tail"),
        ];
        let mut log = AppendOnlyLog::new(base);
        let kept = log.truncate_to_longest_stable_prefix(&divergent);
        assert_eq!(kept, 2, "must keep exactly the first 2 messages");
        assert_eq!(log.as_slice().len(), 2);
        // 保留的消息逐字节与 other 对应消息一致。
        assert_eq!(log.as_slice()[0].text(), divergent[0].text());
        assert_eq!(log.as_slice()[1].text(), divergent[1].text());
    }
}
