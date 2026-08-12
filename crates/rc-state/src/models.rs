use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub workspace: String,
    pub summary: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NavOutcome {
    Success,
    LeafTooThin,
    WrongBranch,
    BudgetExhausted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NavigationRecord {
    pub id: String,
    pub task_signature: String,
    pub root: String,
    pub path_json: String,
    pub outcome: NavOutcome,
    pub model: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub session_id: String,
    pub role: MessageRole,
    pub content: String,
    pub created_at: String,
}

impl Message {
    /// 构造一条新消息:自动生成 id 与 created_at(compact 构造摘要消息用)。
    pub fn new(session_id: &str, role: MessageRole, content: &str) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            session_id: session_id.to_string(),
            role,
            content: content.to_string(),
            created_at: Utc::now().to_rfc3339(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExperienceRecord {
    pub id: String,
    pub session_id: String,
    pub task_signature: String,
    pub category_guess: String,
    pub approach: Vec<String>,
    pub worked: Vec<String>,
    pub failed: Vec<String>,
    pub commands: Vec<String>,
    pub tools_used: Vec<String>,
    pub outcome: String,
    pub skills_used: Vec<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillRow {
    pub id: String,
    pub name: String,
    pub category: String,
    pub path: String,
    pub description: String,
    pub frontmatter: Value,
    pub version: i64,
    pub confidence: f64,
    pub usage_count: i64,
    pub success_count: i64,
    pub last_used: Option<String>,
    pub auto: bool,
    pub origin: String,
    pub origin_url: Option<String>,
    pub scope: String,
    pub allow_implicit: bool,
    pub relations: Value,
    pub embedding: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

pub struct AuditEntry {
    pub id: String,
    pub ts: String,
    pub action: String,
    pub detail: String,
    pub actor: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CapabilityProfileRow {
    pub model: String,
    pub reasoning: f64,
    pub coding: f64,
    pub frontend: f64,
    pub backend: f64,
    pub math: f64,
    pub long_context: f64,
    pub input_cost_per_m: f64,
    pub output_cost_per_m: f64,
    pub context_window: u32,
    pub source: String,
    pub updated_at: String,
    pub multimodal: bool,
}
