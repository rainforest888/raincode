//! Session digest engine: extract -> classify -> decide -> bump/refine/propose.
use chrono::Utc;
use futures::StreamExt;
use rc_pro::{CanonicalMessage, CanonicalRequest, Provider};
use rc_skill::{
    enforce_skill_shape, parse_frontmatter, Relation, RelationKind, Skill, SkillNetwork,
    SkillRouter, SkillStore,
};
use rc_state::{ExperienceRecord, MessageRole, Store};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvolveConfig {
    #[serde(default = "default_min")]
    pub min_experiences: u32,
    #[serde(default = "default_threshold")]
    pub similarity_threshold: f32,
    #[serde(default = "default_auto_approve")]
    pub auto_approve: bool,
}

impl Default for EvolveConfig {
    fn default() -> Self {
        Self {
            min_experiences: default_min(),
            similarity_threshold: default_threshold(),
            auto_approve: default_auto_approve(),
        }
    }
}

fn default_min() -> u32 {
    3
}

fn default_threshold() -> f32 {
    0.78
}

fn default_auto_approve() -> bool {
    // 新 auto skill 默认需用户确认(review 区),不直接进 user 作用域。
    false
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum EvolveAction {
    Bump { skill: String },
    Refine { skill: String, version: u32 },
    Propose { skill: String },
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvolveReport {
    pub session_id: String,
    pub extracted: bool,
    pub matched_skill: Option<String>,
    pub action: EvolveAction,
    pub detail: String,
}

pub struct EvolveEngine {
    provider: Arc<dyn Provider>,
    store: Store,
    skill_store: SkillStore,
    router: SkillRouter,
    config: EvolveConfig,
}

impl EvolveEngine {
    pub fn new(
        provider: Arc<dyn Provider>,
        store: Store,
        skill_store: SkillStore,
        config: EvolveConfig,
    ) -> Self {
        let router = SkillRouter::new(skill_store.discover());
        Self {
            provider,
            store,
            skill_store,
            router,
            config,
        }
    }

    /// Run one digest pass over a finished session.
    pub async fn digest(&mut self, session_id: &str) -> Result<EvolveReport, EvolveError> {
        let messages = self.store.list_messages(session_id)?;
        if messages.is_empty() {
            return Ok(EvolveReport {
                session_id: session_id.to_string(),
                extracted: false,
                matched_skill: None,
                action: EvolveAction::None,
                detail: "no messages in session".into(),
            });
        }
        let transcript = build_transcript(&messages);
        let extracted = self.ask_json(&format!(
            "You are the Raincode experience extractor. Read the transcript and return ONE JSON object with exactly these fields: \
             {{task_signature: string, category_guess: string, approach: [string], worked: [string], failed: [string], \
             commands: [string], tools_used: [string], outcome: \"success\"|\"partial\"|\"fail\", skills_used: [string]}}.\n\nTranscript:\n{transcript}"
        )).await;

        let record = match extracted.clone() {
            Some(v) => record_from_json(v, session_id),
            None => heuristic_record(&messages, session_id),
        };
        self.store.save_experience(record.clone())?;
        let outcome_success = record.outcome == "success";

        // Classify against existing skills.
        let mut matched = None;
        let mut matched_score = 0.0f32;
        if let Ok(mut embs) = self
            .provider
            .embed(vec![record.task_signature.clone()])
            .await
        {
            if let Some(task_emb) = embs.pop() {
                let top = self
                    .router
                    .select_with_embedding(&record.task_signature, &task_emb, 1);
                if let Some(first) = top.into_iter().next() {
                    matched_score = first.score;
                    matched = Some(first.name);
                }
            }
        }
        let top = self.router.select_keyword(&record.task_signature, 1);
        if let Some(first) = top.into_iter().next() {
            if first.score > matched_score {
                matched_score = first.score;
                matched = Some(first.name);
            }
        }

        if let Some(skill_name) = &matched {
            if matched_score >= self.config.similarity_threshold {
                if outcome_success {
                    self.bump_skill(skill_name, &record).await?;
                    return Ok(EvolveReport {
                        session_id: session_id.to_string(),
                        extracted: extracted.is_some(),
                        matched_skill: Some(skill_name.clone()),
                        action: EvolveAction::Bump {
                            skill: skill_name.clone(),
                        },
                        detail: format!("skill '{skill_name}' corroborated, confidence raised"),
                    });
                } else {
                    let version = self.refine_skill(skill_name, &record).await?;
                    return Ok(EvolveReport {
                        session_id: session_id.to_string(),
                        extracted: extracted.is_some(),
                        matched_skill: Some(skill_name.clone()),
                        action: EvolveAction::Refine {
                            skill: skill_name.clone(),
                            version,
                        },
                        detail: format!("skill '{skill_name}' refined from conflicting experience"),
                    });
                }
            }
        }

        // No strong match: propose a new skill once enough corroborating
        // experiences have accumulated in the same category.
        let same_category = self
            .store
            .list_experiences(None)?
            .iter()
            .filter(|e| !e.category_guess.is_empty() && e.category_guess == record.category_guess)
            .filter(|e| e.outcome == "success")
            .count();
        if same_category >= self.config.min_experiences as usize {
            let name = self.propose_skill(&record).await?;
            return Ok(EvolveReport {
                session_id: session_id.to_string(),
                extracted: extracted.is_some(),
                matched_skill: None,
                action: EvolveAction::Propose {
                    skill: name.clone(),
                },
                detail: format!(
                    "proposed new skill '{name}' from {same_category} corroborating experiences"
                ),
            });
        }

        Ok(EvolveReport {
            session_id: session_id.to_string(),
            extracted: extracted.is_some(),
            matched_skill: matched.clone(),
            action: EvolveAction::None,
            detail: "not enough evidence to propose or refine".into(),
        })
    }

    async fn bump_skill(
        &mut self,
        name: &str,
        record: &ExperienceRecord,
    ) -> Result<(), EvolveError> {
        let mut skill = self.skill_store.load(name);
        if let Some(ref mut skill) = skill {
            skill.usage_count = skill.usage_count.saturating_add(1);
            skill.success_rate = if skill.usage_count == 0 {
                1.0
            } else {
                ((skill.success_rate * (skill.usage_count as f32 - 1.0)) + 1.0)
                    / skill.usage_count as f32
            };
            skill.confidence = (skill.confidence + 0.05).min(1.0);
            skill.last_used = Some(Utc::now().to_rfc3339());
            let path = self.skill_store.save(skill).map_err(EvolveError::Io)?;
            self.store.upsert_skill(&skill.to_row())?;
            // 学习统计统一走 bump_skill_usage(+1,保留既有值);upsert 只写元数据。
            self.store.bump_skill_usage(name, true)?;
            self.store.add_audit(
                "evolve.bump",
                &format!(
                    "{name} corroborated by {} at {}",
                    record.session_id,
                    path.display()
                ),
                "evolve",
            )?;
        } else {
            self.store.bump_skill_usage(name, true)?;
            self.store.add_audit(
                "evolve.bump",
                &format!("{name} corroborated by {}", record.session_id),
                "evolve",
            )?;
        }
        Ok(())
    }

    async fn refine_skill(
        &mut self,
        name: &str,
        record: &ExperienceRecord,
    ) -> Result<u32, EvolveError> {
        let skill = self
            .skill_store
            .load(name)
            .ok_or_else(|| EvolveError::MissingSkill(name.to_string()))?;
        let prompt = format!(
            "You are the Raincode skill refiner. Rewrite the SKILL.md below to incorporate the new experience. \
             Keep the same name and category, fix contradictions, and return the full SKILL.md (frontmatter + body).\n\n\
             Existing SKILL.md:\n{}\n\nNew experience:\n{}\n\nReturn ONLY the SKILL.md.",
            skill.render().map_err(|e| EvolveError::Io(e.to_string()))?,
            serde_json::to_string_pretty(record).unwrap_or_default()
        );
        let text = self.ask_text(&prompt).await.unwrap_or_default();
        let mut updated = parse_skill_md(&text).map_err(EvolveError::Frontmatter)?;
        updated.name = skill.name.clone();
        updated.category = skill.category.clone();
        updated.version = skill.version + 1;
        updated.auto = true;
        updated.origin = "evolved".into();
        updated.confidence = (skill.confidence + 0.1).min(1.0);
        // 遥测保留(与 apply_organism 一致):refine 绝不重置使用数据/embedding/作用域。
        // 否则每次 refine 都会清零 usage_count/success_rate、丢弃 embedding、
        // 清空 last_used(skill 立即被判 stale),并把 system/project 作用域降级回 user。
        updated.usage_count = skill.usage_count;
        updated.success_rate = skill.success_rate;
        updated.last_used = skill.last_used.clone();
        updated.embedding = skill.embedding.clone();
        updated.scope = skill.scope.clone();
        updated.allow_implicit = skill.allow_implicit;
        updated.origin_url = skill.origin_url.clone();
        updated.body = format!(
            "> Auto-generated by Raincode EvolveEngine at {}.\n\n{}",
            Utc::now().to_rfc3339(),
            updated.body.trim()
        );
        // 演化把关:索引(有子)写正文 → 拒绝,不破坏现有 skill。
        let network = SkillNetwork::from_store(&self.skill_store);
        enforce_skill_shape(&network, &updated).map_err(EvolveError::Shape)?;
        let parents: Vec<String> = updated.relations.iter().map(|r| r.skill.clone()).collect();
        let path = self.skill_store.save(&updated).map_err(EvolveError::Io)?;
        let row = updated.to_row();
        self.store.upsert_skill(&row)?;
        self.store.add_audit(
            "evolve.refine",
            &format!(
                "{} v{} model={} parents=[{}] at {}",
                name,
                updated.version,
                self.provider.id(),
                parents.join(", "),
                path.display()
            ),
            "evolve",
        )?;
        Ok(updated.version)
    }

    async fn propose_skill(&mut self, record: &ExperienceRecord) -> Result<String, EvolveError> {
        let prompt = format!(
            "You are the Raincode skill author. Write a reusable SKILL.md capturing the common method behind these tasks. \
             Use YAML frontmatter with name, description, category, triggers, tags. Keep the body concise and actionable. \
             Return ONLY the SKILL.md.\n\nTask: {}\nCategory: {}\nApproach that worked: {}\nPitfalls: {}",
            record.task_signature,
            record.category_guess,
            record.worked.join("; "),
            record.failed.join("; ")
        );
        let text = self.ask_text(&prompt).await.unwrap_or_default();
        let mut skill = parse_skill_md(&text).map_err(EvolveError::Frontmatter)?;
        skill.auto = true;
        skill.origin = "evolved".into();
        // 确认机制:auto_approve=false 时,新 skill 写进 .review 隔离区(scope=review),
        // 等用户 `skills review --approve` 才转正;true 则直接进 user 作用域。
        let auto_approved = self.config.auto_approve;
        skill.scope = if auto_approved { "user".into() } else { "review".into() };
        skill.confidence = 0.6;
        skill.version = 1;
        if skill.category.is_empty() {
            skill.category = record.category_guess.clone();
        }
        // Prefer an existing skill in the nearest ancestor category, then
        // fall back to the strongest keyword parent. This attaches a new
        // skill into the existing network instead of leaving it floating.
        let mut parent = self
            .router
            .all()
            .iter()
            .filter(|existing| {
                existing.name != skill.name
                    && !existing.category.is_empty()
                    && !skill.category.is_empty()
                    && skill
                        .category
                        .starts_with(&format!("{}.", existing.category))
            })
            .max_by_key(|existing| existing.category.len())
            .map(|existing| rc_skill::SkillSummary {
                name: existing.name.clone(),
                description: existing.description.clone(),
                category: existing.category.clone(),
                score: 0.5,
                is_leaf: false,
            });
        if parent.is_none() {
            parent = self
                .router
                .select_keyword(&record.task_signature, 1)
                .into_iter()
                .next();
        }
        if let Some(parent) = parent {
            if parent.name != skill.name {
                skill.relations.push(Relation {
                    kind: RelationKind::Refines,
                    skill: parent.name,
                });
            }
        }
        if let Ok(mut embs) = self
            .provider
            .embed(vec![format!("{} {}", skill.name, skill.description)])
            .await
        {
            if let Some(emb) = embs.pop() {
                skill.embedding = Some(emb);
            }
        }
        skill.body = format!(
            "> Auto-generated by Raincode EvolveEngine at {}.\n\n{}",
            Utc::now().to_rfc3339(),
            skill.body.trim()
        );
        // 演化把关:新 skill 落在现有索引名下(重名)→ 拒绝,不破坏索引。
        // 祖先目录存在性由 router(来自 discover)保证,save() 的 create_dir_all
        // 会用 category 补建缺失目录。
        let network = SkillNetwork::from_store(&self.skill_store);
        enforce_skill_shape(&network, &skill).map_err(EvolveError::Shape)?;
        // 名称冲突:撞上任何现有 skill(索引或叶子)→ 拒绝,不覆盖用户文件。
        // 形状把关只挡"索引带正文",叶子-带正文(合法)的重名会从形状把关漏过,
        // 直接 save() 覆盖用户 skill —— 这里补上硬性重名校验。
        if self.skill_store.load(&skill.name).is_some() {
            return Err(EvolveError::Shape(format!(
                "skill '{}' already exists; refusing to overwrite",
                skill.name
            )));
        }
        let parents: Vec<String> = skill.relations.iter().map(|r| r.skill.clone()).collect();
        let path = self.skill_store.save(&skill).map_err(EvolveError::Io)?;
        self.store.upsert_skill(&skill.to_row())?;
        self.store.add_audit(
            "evolve.propose",
            &format!(
                "{} v{} model={} parents=[{}] at {}",
                skill.name,
                skill.version,
                self.provider.id(),
                parents.join(", "),
                path.display()
            ),
            "evolve",
        )?;
        self.router = SkillRouter::new(self.skill_store.discover());
        Ok(skill.name)
    }

    async fn ask_text(&mut self, prompt: &str) -> Option<String> {
        let req = CanonicalRequest {
            model: self.provider.id().to_string(),
            messages: vec![CanonicalMessage::user(prompt)],
            tools: vec![],
            temperature: Some(0.2),
            max_tokens: Some(3000),
            stream: true,
            extra: json!({}),
        };
        let Ok(mut stream) = self.provider.stream(req).await else {
            return None;
        };
        let mut text = String::new();
        while let Some(Ok(ev)) = stream.next().await {
            if let rc_pro::ProvEvent::Delta { text: t } = ev {
                text.push_str(&t);
            }
        }
        Some(text)
    }

    async fn ask_json(&mut self, prompt: &str) -> Option<Value> {
        let text = self.ask_text(prompt).await?;
        extract_json(&text)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EvolveError {
    #[error("state error: {0}")]
    State(#[from] rc_state::DbError),
    #[error("skill missing: {0}")]
    MissingSkill(String),
    #[error("frontmatter error: {0}")]
    Frontmatter(String),
    #[error("skill shape violation: {0}")]
    Shape(String),
    #[error("io error: {0}")]
    Io(String),
}

fn build_transcript(messages: &[rc_state::Message]) -> String {
    // 控制总体积:用户/助手消息完整保留会随会话增长无限放大,提供方上下文会溢出。
    const PER_MSG_MAX: usize = 800;
    const TOTAL_MAX: usize = 12_000;
    let mut total = 0usize;
    let mut parts: Vec<String> = Vec::new();
    for m in messages {
        let line = match m.role {
            MessageRole::User => format!("USER: {}", truncate(&m.content, PER_MSG_MAX)),
            MessageRole::Assistant => {
                format!("ASSISTANT: {}", truncate(&m.content, PER_MSG_MAX))
            }
            MessageRole::Tool => format!(
                "TOOL[{}]: {}",
                m.content.lines().next().unwrap_or(""),
                truncate(&m.content, 300)
            ),
            MessageRole::System => format!("SYSTEM: {}", truncate(&m.content, 300)),
        };
        total += line.len();
        if total > TOTAL_MAX && !parts.is_empty() {
            parts.push("... [transcript truncated]".into());
            break;
        }
        parts.push(line);
    }
    parts.join("\n")
}

fn truncate(s: &str, n: usize) -> String {
    let mut out: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        out.push_str("...");
    }
    out
}

fn record_from_json(v: Value, session_id: &str) -> ExperienceRecord {
    ExperienceRecord {
        id: String::new(),
        session_id: session_id.to_string(),
        task_signature: v["task_signature"]
            .as_str()
            .unwrap_or("unknown task")
            .to_string(),
        category_guess: v["category_guess"].as_str().unwrap_or("").to_string(),
        approach: str_vec(&v["approach"]),
        worked: str_vec(&v["worked"]),
        failed: str_vec(&v["failed"]),
        commands: str_vec(&v["commands"]),
        tools_used: str_vec(&v["tools_used"]),
        outcome: normalize_outcome(v["outcome"].as_str()),
        skills_used: str_vec(&v["skills_used"]),
        created_at: String::new(),
    }
}

/// 归一化 LLM 自由文本的 outcome 到受控枚举(success/partial/fail)。
/// 提示词虽指定了枚举,但模型可能输出 "succeeded"/"failed"/"ok",游离字符串
/// 会让下游 `outcome == "success"` 的 bump-vs-refine 判定失效。
fn normalize_outcome(raw: Option<&str>) -> String {
    match raw.unwrap_or("partial").to_lowercase().as_str() {
        "success" | "succeeded" | "successful" | "ok" | "complete" | "completed" | "pass" => {
            "success".into()
        }
        "fail" | "failed" | "failure" | "error" | "err" => "fail".into(),
        _ => "partial".into(),
    }
}

fn str_vec(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

fn heuristic_record(messages: &[rc_state::Message], session_id: &str) -> ExperienceRecord {
    let first_user = messages
        .iter()
        .find(|m| m.role == MessageRole::User)
        .map(|m| truncate(&m.content, 200))
        .unwrap_or_else(|| "unknown".into());
    let tools: Vec<String> = messages
        .iter()
        .filter(|m| m.role == MessageRole::Tool)
        .filter_map(|m| m.content.lines().next().map(|l| l.to_string()))
        .collect();
    ExperienceRecord {
        id: String::new(),
        session_id: session_id.to_string(),
        task_signature: first_user,
        category_guess: String::new(),
        approach: Vec::new(),
        worked: Vec::new(),
        failed: Vec::new(),
        commands: Vec::new(),
        tools_used: tools,
        outcome: "partial".into(),
        skills_used: Vec::new(),
        created_at: String::new(),
    }
}

/// Parse a model-generated SKILL.md. Tolerates code fences and leading prose.
pub fn parse_skill_md(text: &str) -> Result<Skill, String> {
    let start = text.find("---").ok_or("no frontmatter delimiters")?;
    let rest = &text[start..];
    let (fm, body) = parse_frontmatter(rest).map_err(|e| e.to_string())?;
    Ok(Skill::from_frontmatter(fm, body, std::path::PathBuf::new()))
}

/// Extract the first balanced JSON object from free text.
pub fn extract_json(text: &str) -> Option<Value> {
    let start = text.find('{')?;
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    for (i, ch) in text[start..].char_indices() {
        match ch {
            '"' if !escaped => in_string = !in_string,
            '\\' if in_string => escaped = !escaped,
            _ => escaped = false,
        }
        if in_string {
            continue;
        }
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    let end = start + i + 1;
                    return serde_json::from_str(&text[start..end]).ok();
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_balanced_json() {
        let text = "Here you go: {\"a\": [1, {\"b\": \"x}\"}]} done";
        let v = extract_json(text).unwrap();
        assert_eq!(v["a"][1]["b"], "x}");
    }

    #[test]
    fn parses_model_skill_md() {
        let md = "Sure!\n---\nname: fix-flaky\ndescription: fix flaky tests\ncategory: testing\n---\nBody text.";
        let skill = parse_skill_md(md).unwrap();
        assert_eq!(skill.name, "fix-flaky");
        assert_eq!(skill.body, "Body text.");
    }

    fn mock_provider(script: Value) -> Arc<dyn Provider> {
        let cfg = rc_pro::provider::ProviderConfig {
            kind: "mock".into(),
            base_url: String::new(),
            model: "mock-evolve".into(),
            api_key: None,
            api_key_env: None,
            embedding_model: None,
            headers: Default::default(),
            extra: json!({"script": script, "auto_advance": false}),
        };
        Arc::new(rc_pro::mock::MockProvider::new(cfg, "mock-evolve".into()))
    }

    fn session_with_messages(store: &Store, prompt: &str) -> String {
        let session = store.create_session("test").unwrap();
        store
            .append_message(&session.id, MessageRole::User, prompt)
            .unwrap();
        store
            .append_message(&session.id, MessageRole::Assistant, "did the work")
            .unwrap();
        session.id
    }

    fn script_with(extract: &str, skill_md: &str) -> Value {
        json!([
            {"type": "text", "text": extract},
            {"type": "text", "text": skill_md},
            {"type": "done", "stop_reason": "end_turn"}
        ])
    }

    fn basic_skill(name: &str, category: &str, trigger: &str) -> Skill {
        Skill {
            name: name.into(),
            description: format!("{trigger} helper"),
            short_description: None,
            category: category.into(),
            path: std::path::PathBuf::new(),
            body: "body".into(),
            relations: vec![],
            triggers: vec![trigger.into()],
            tags: vec![],
            version: 1,
            confidence: 0.8,
            usage_count: 0,
            success_rate: 0.0,
            last_used: None,
            auto: false,
            origin: "manual".into(),
            origin_url: None,
            scope: "user".into(),
            allow_implicit: true,
            embedding: None,
        }
    }

    #[tokio::test]
    async fn digest_proposes_skill_after_enough_experiences() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        let session = session_with_messages(&store, "fix pytest flakiness");
        let extract = r#"{"task_signature":"fix pytest flakiness","category_guess":"testing.pytest","approach":["run pytest -x"],"worked":["found flaky test"],"failed":[],"commands":["pytest -x"],"tools_used":["shell"],"outcome":"success","skills_used":[]}"#;
        let skill_md = "---\nname: pytest-recipe\ndescription: fix flaky pytest tests\ncategory: testing.pytest\ntriggers: [pytest, flaky]\ntags: [testing]\n---\nRun pytest -x and quarantine the flaky test.";
        let provider = mock_provider(script_with(extract, skill_md));
        let mut engine = EvolveEngine::new(
            provider,
            store,
            SkillStore::new(dir.path()),
            EvolveConfig {
                min_experiences: 1,
                similarity_threshold: 0.9,
                auto_approve: true,
            },
        );
        let report = engine.digest(&session).await.unwrap();
        assert!(matches!(report.action, EvolveAction::Propose { .. }));
        let skill = engine
            .skill_store
            .load("pytest-recipe")
            .expect("proposed skill saved");
        assert!(skill.auto);
        assert_eq!(skill.origin, "evolved");
        assert_eq!(engine.store.list_experiences(None).unwrap().len(), 1);
    }
    #[tokio::test]
    async fn digest_twice_grows_network_and_second_pass_hits_new_skill() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        let skill_store = SkillStore::new(dir.path());
        let base = basic_skill("testing-base", "testing", "testing");
        skill_store.save(&base).unwrap();
        store.upsert_skill(&base.to_row()).unwrap();

        let session1 = session_with_messages(&store, "fix pytest flakiness");
        let session2 = session_with_messages(&store, "fix pytest flakiness again");
        let extract1 = r#"{"task_signature":"fix pytest flakiness","category_guess":"testing.pytest","approach":["run pytest -x"],"worked":["found flaky test"],"failed":[],"commands":["pytest -x"],"tools_used":["shell"],"outcome":"success","skills_used":[]}"#;
        let skill_md = "---\nname: pytest-recipe\ndescription: fix flaky pytest tests\ncategory: testing.pytest\ntriggers: [pytest, flaky]\ntags: [testing]\n---\nRun pytest -x and quarantine the flaky test.";
        let extract2 = r#"{"task_signature":"fix pytest flakiness again","category_guess":"testing.pytest","approach":["run pytest -x"],"worked":["found flaky test"],"failed":[],"commands":["pytest -x"],"tools_used":["shell"],"outcome":"success","skills_used":["pytest-recipe"]}"#;
        let cfg = rc_pro::provider::ProviderConfig {
            kind: "mock".into(),
            base_url: String::new(),
            model: "mock-evolve".into(),
            api_key: None,
            api_key_env: None,
            embedding_model: None,
            headers: Default::default(),
            extra: json!({
                "script_sequence": [
                    [
                        {"type": "text", "text": extract1},
                        {"type": "done", "stop_reason": "end_turn"}
                    ],
                    [
                        {"type": "text", "text": skill_md},
                        {"type": "done", "stop_reason": "end_turn"}
                    ],
                    [
                        {"type": "text", "text": extract2},
                        {"type": "done", "stop_reason": "end_turn"}
                    ]
                ],
                "auto_advance": false
            }),
        };
        let provider: Arc<dyn Provider> =
            Arc::new(rc_pro::mock::MockProvider::new(cfg, "mock-evolve".into()));
        let mut engine = EvolveEngine::new(
            provider,
            store,
            skill_store,
            EvolveConfig {
                min_experiences: 1,
                similarity_threshold: 0.5,
                auto_approve: true,
            },
        );
        let first = engine.digest(&session1).await.unwrap();
        assert!(matches!(first.action, EvolveAction::Propose { .. }));
        let proposed = engine.skill_store.load("pytest-recipe").unwrap();
        assert!(proposed.auto);
        assert_eq!(proposed.origin, "evolved");
        assert!(proposed
            .relations
            .iter()
            .any(|r| { r.kind == rc_skill::RelationKind::Refines && r.skill == "testing-base" }));

        let second = engine.digest(&session2).await.unwrap();
        assert!(matches!(
            second.action,
            EvolveAction::Bump { ref skill } if skill == "pytest-recipe"
        ));
        let bumped = engine.skill_store.load("pytest-recipe").unwrap();
        assert_eq!(bumped.usage_count, 1);
        assert!(bumped.confidence > proposed.confidence);
        assert!(bumped
            .relations
            .iter()
            .any(|r| { r.kind == rc_skill::RelationKind::Refines && r.skill == "testing-base" }));
        assert_eq!(engine.store.list_experiences(None).unwrap().len(), 2);
    }
    #[tokio::test]
    async fn digest_bumps_matching_skill_on_success() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        let skill_store = SkillStore::new(dir.path());
        let skill = basic_skill("pytest-fix", "testing", "pytest");
        skill_store.save(&skill).unwrap();
        store.upsert_skill(&skill.to_row()).unwrap();
        let session = session_with_messages(&store, "pytest flaky failure");
        let extract = r#"{"task_signature":"pytest flaky failure","category_guess":"testing.pytest","approach":["run pytest"],"worked":["found"],"failed":[],"commands":["pytest"],"tools_used":["shell"],"outcome":"success","skills_used":["pytest-fix"]}"#;
        let skill_md =
            "---\nname: pytest-fix\ndescription: fix pytest\ncategory: testing\n---\nBody.";
        let provider = mock_provider(script_with(extract, skill_md));
        let mut engine = EvolveEngine::new(
            provider,
            store,
            skill_store,
            EvolveConfig {
                min_experiences: 1,
                similarity_threshold: 0.2,
                auto_approve: true,
            },
        );
        let report = engine.digest(&session).await.unwrap();
        assert!(matches!(report.action, EvolveAction::Bump { ref skill } if skill == "pytest-fix"));
        let row = engine.store.get_skill("pytest-fix").unwrap().unwrap();
        assert_eq!(row.usage_count, 1);
        assert_eq!(row.success_count, 1);
        let loaded = engine.skill_store.load("pytest-fix").unwrap();
        assert!(loaded.confidence > 0.8);
        assert_eq!(loaded.usage_count, 1);
        assert!(loaded.last_used.is_some());
    }

    #[tokio::test]
    async fn digest_refines_conflicting_skill() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        let skill_store = SkillStore::new(dir.path());
        let skill = basic_skill("pytest-fix", "testing", "pytest");
        skill_store.save(&skill).unwrap();
        store.upsert_skill(&skill.to_row()).unwrap();
        let session = session_with_messages(&store, "pytest flaky failure");
        let extract = r#"{"task_signature":"pytest flaky failure","category_guess":"testing.pytest","approach":["run pytest"],"worked":[],"failed":["still flaky"],"commands":["pytest"],"tools_used":["shell"],"outcome":"fail","skills_used":["pytest-fix"]}"#;
        let skill_md =
            "---\nname: pytest-fix\ndescription: fix pytest\ncategory: testing\n---\nRun pytest.";
        let provider = mock_provider(script_with(extract, skill_md));
        let mut engine = EvolveEngine::new(
            provider,
            store,
            skill_store,
            EvolveConfig {
                min_experiences: 1,
                similarity_threshold: 0.2,
                auto_approve: true,
            },
        );
        let report = engine.digest(&session).await.unwrap();
        assert!(matches!(
            report.action,
            EvolveAction::Refine { version: 2, .. }
        ));
        let row = engine.store.get_skill("pytest-fix").unwrap().unwrap();
        assert_eq!(row.version, 2);
        assert_eq!(row.origin, "evolved");
    }

    #[tokio::test]
    async fn digest_refine_preserves_telemetry() {
        // refine 不得重置 usage_count/success_rate/last_used/embedding,不得降级
        // scope、不得翻转 allow_implicit / origin_url —— 与 apply_organism 的遥测保留一致。
        // 直接调用 refine_skill(私有的异步方法),绕开 digest 的 allow_implicit 匹配过滤。
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        let skill_store = SkillStore::new(dir.path());
        let mut skill = basic_skill("pytest-fix", "testing", "pytest");
        skill.usage_count = 5;
        skill.success_rate = 0.8;
        skill.last_used = Some("2026-01-01T00:00:00Z".into());
        skill.embedding = Some(vec![0.1, 0.2]);
        skill.scope = "project".into();
        skill.allow_implicit = false;
        skill.origin_url = Some("https://example.com/skill".into());
        skill_store.save(&skill).unwrap();
        store.upsert_skill(&skill.to_row()).unwrap();

        let skill_md =
            "---\nname: pytest-fix\ndescription: fix pytest\ncategory: testing\n---\nRun pytest.";
        let provider = mock_provider(json!([
            {"type": "text", "text": skill_md},
            {"type": "done", "stop_reason": "end_turn"}
        ]));
        let mut engine = EvolveEngine::new(
            provider,
            store,
            skill_store,
            EvolveConfig {
                min_experiences: 1,
                similarity_threshold: 0.2,
                auto_approve: true,
            },
        );
        let record = ExperienceRecord {
            id: String::new(),
            session_id: "s1".into(),
            task_signature: "pytest flaky failure".into(),
            category_guess: "testing.pytest".into(),
            approach: vec![],
            worked: vec![],
            failed: vec!["still flaky".into()],
            commands: vec![],
            tools_used: vec![],
            outcome: "fail".into(),
            skills_used: vec![],
            created_at: String::new(),
        };
        let version = engine.refine_skill("pytest-fix", &record).await.unwrap();
        assert_eq!(version, 2, "refine bumps version");
        let loaded = engine.skill_store.load("pytest-fix").unwrap();
        assert_eq!(loaded.usage_count, 5, "usage_count must survive refine");
        assert!(
            (loaded.success_rate - 0.8).abs() < 1e-4,
            "success_rate must survive refine: {}",
            loaded.success_rate
        );
        assert_eq!(loaded.last_used.as_deref(), Some("2026-01-01T00:00:00Z"));
        assert_eq!(
            loaded.embedding.as_deref(),
            Some(&[0.1f32, 0.2][..]),
            "embedding must survive refine"
        );
        assert_eq!(loaded.scope, "project", "scope must survive refine");
        assert!(!loaded.allow_implicit, "allow_implicit must survive refine");
        assert_eq!(
            loaded.origin_url.as_deref(),
            Some("https://example.com/skill"),
            "origin_url must survive refine"
        );
        // DB 行同样保留遥测(daemon 读 DB 行做 usage 合并/retention gate)。
        let row = engine.store.get_skill("pytest-fix").unwrap().unwrap();
        assert_eq!(row.usage_count, 5);
        assert_eq!(row.scope, "project");
    }

    #[test]
    fn enforce_shape_guard_runs_over_constructed_network() {
        let dir = tempfile::tempdir().unwrap();
        let skill_store = SkillStore::new(dir.path());
        let mut index = basic_skill("react", "frontend", "react");
        index.body = String::new();
        skill_store.save(&index).unwrap();
        let child = basic_skill("react.performance", "frontend.react", "perf");
        skill_store.save(&child).unwrap();
        let net = SkillNetwork::from_store(&skill_store);
        // 索引(有子)+ 正文 → 拒绝。
        let with_body = basic_skill("react", "frontend", "react");
        assert!(enforce_skill_shape(&net, &with_body).is_err());
        // 叶子无正文 → 拒绝。
        let mut bare_leaf = basic_skill("react.performance", "frontend.react", "perf");
        bare_leaf.body = String::new();
        assert!(enforce_skill_shape(&net, &bare_leaf).is_err());
        // 良构(index 空正文 / leaf 有正文)→ 通过。
        assert!(enforce_skill_shape(&net, &child).is_ok());
        assert!(enforce_skill_shape(&net, &index).is_ok());
    }

    #[tokio::test]
    async fn digest_propose_rejects_when_new_skill_would_stuff_index() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        let skill_store = SkillStore::new(dir.path());
        let mut index = basic_skill("react", "frontend", "react");
        index.body = String::new();
        skill_store.save(&index).unwrap();
        store.upsert_skill(&index.to_row()).unwrap();
        let child = basic_skill("react.performance", "frontend.react", "perf");
        skill_store.save(&child).unwrap();
        store.upsert_skill(&child.to_row()).unwrap();

        let session = session_with_messages(&store, "optimize data pipeline");
        let extract = r#"{"task_signature":"optimize data pipeline","category_guess":"data.etl","approach":["profile"],"worked":["found hot path"],"failed":[],"commands":["dbt"],"tools_used":["shell"],"outcome":"success","skills_used":[]}"#;
        // 模型返回的名字撞上现有索引 react,且携带正文 → 必须被 guard 拒绝。
        let skill_md = "---\nname: react\ndescription: react optimization\ncategory: frontend\ntriggers: [react]\n---\nMemoize components and profile renders.";
        let provider = mock_provider(script_with(extract, skill_md));
        let mut engine = EvolveEngine::new(
            provider,
            store,
            skill_store,
            EvolveConfig {
                min_experiences: 1,
                similarity_threshold: 0.9,
                auto_approve: true,
            },
        );
        let res = engine.digest(&session).await;
        assert!(res.is_err(), "guard must reject stuffing an index with body");
        match res.unwrap_err() {
            EvolveError::Shape(msg) => assert!(msg.contains("index"), "err: {msg}"),
            other => panic!("expected Shape error, got {other:?}"),
        }
        let loaded = engine.skill_store.load("react").unwrap();
        assert!(loaded.body.trim().is_empty(), "index body must stay empty");
    }

    #[tokio::test]
    async fn digest_propose_rejects_collision_with_existing_leaf() {
        // 形状把关挡不住"与现有叶子重名"(叶子带正文合法)→ 必须有重名硬校验:
        // 模型返回的名字撞上现有叶子 → 拒绝且不覆盖用户文件。
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        let skill_store = SkillStore::new(dir.path());
        let leaf = basic_skill("fix-flaky", "testing", "flaky");
        skill_store.save(&leaf).unwrap();
        store.upsert_skill(&leaf.to_row()).unwrap();

        let session = session_with_messages(&store, "optimize data pipeline");
        let extract = r#"{"task_signature":"optimize data pipeline","category_guess":"data.etl","approach":["profile"],"worked":["found hot path"],"failed":[],"commands":["dbt"],"tools_used":["shell"],"outcome":"success","skills_used":[]}"#;
        // 模型返回的名字撞上现有叶子 fix-flaky(带正文)→ 必须被拒绝。
        let skill_md = "---\nname: fix-flaky\ndescription: fix flaky tests\ncategory: testing\ntriggers: [flaky]\n---\nNew body from model.";
        let provider = mock_provider(script_with(extract, skill_md));
        let mut engine = EvolveEngine::new(
            provider,
            store,
            skill_store,
            EvolveConfig {
                min_experiences: 1,
                similarity_threshold: 0.9,
                auto_approve: true,
            },
        );
        let res = engine.digest(&session).await;
        assert!(res.is_err(), "collision with existing leaf must be rejected");
        match res.unwrap_err() {
            EvolveError::Shape(msg) => assert!(msg.contains("already exists"), "err: {msg}"),
            other => panic!("expected Shape error, got {other:?}"),
        }
        let loaded = engine.skill_store.load("fix-flaky").unwrap();
        assert_eq!(loaded.body, "body", "existing leaf body must stay untouched");
        assert_eq!(loaded.version, 1, "existing leaf version must stay untouched");
    }

    #[tokio::test]
    async fn digest_refine_rejects_stuffing_body_into_index() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        let skill_store = SkillStore::new(dir.path());
        let mut index = basic_skill("react", "frontend", "react");
        index.body = String::new();
        skill_store.save(&index).unwrap();
        store.upsert_skill(&index.to_row()).unwrap();
        let child = basic_skill("react.performance", "frontend.react", "perf");
        skill_store.save(&child).unwrap();
        store.upsert_skill(&child.to_row()).unwrap();

        let session = session_with_messages(&store, "react flaky failure");
        let extract = r#"{"task_signature":"react flaky failure","category_guess":"frontend.react","approach":["profile"],"worked":[],"failed":["render too slow"],"commands":["react"],"tools_used":["shell"],"outcome":"fail","skills_used":["react"]}"#;
        let skill_md = "---\nname: react\ndescription: react\ncategory: frontend\n---\nMemoize components.";
        let provider = mock_provider(script_with(extract, skill_md));
        let mut engine = EvolveEngine::new(
            provider,
            store,
            skill_store,
            EvolveConfig {
                min_experiences: 1,
                similarity_threshold: 0.2,
                auto_approve: true,
            },
        );
        let res = engine.digest(&session).await;
        assert!(res.is_err(), "guard must refuse stuffing body into an index");
        match res.unwrap_err() {
            EvolveError::Shape(msg) => assert!(msg.contains("index"), "err: {msg}"),
            other => panic!("expected Shape error, got {other:?}"),
        }
        let loaded = engine.skill_store.load("react").unwrap();
        assert!(loaded.body.trim().is_empty(), "index body must stay empty");
    }
}
