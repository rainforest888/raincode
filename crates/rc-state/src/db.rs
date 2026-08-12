use crate::models::*;
use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use std::path::Path;
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

pub struct Store {
    conn: Connection,
}

fn now() -> String {
    Utc::now().to_rfc3339()
}

fn role_as_str(role: MessageRole) -> &'static str {
    match role {
        MessageRole::System => "system",
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::Tool => "tool",
    }
}

fn role_from_str(s: &str) -> MessageRole {
    match s {
        "system" => MessageRole::System,
        "user" => MessageRole::User,
        "assistant" => MessageRole::Assistant,
        _ => MessageRole::Tool,
    }
}

/// skills 表 20 列 → SkillRow(get_skill 的 SELECT 顺序)。
fn skill_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<SkillRow> {
    Ok(SkillRow {
        id: r.get(0)?,
        name: r.get(1)?,
        category: r.get(2)?,
        path: r.get(3)?,
        description: r.get(4)?,
        frontmatter: serde_json::from_str(&r.get::<_, String>(5)?).unwrap_or(Value::Null),
        version: r.get(6)?,
        confidence: r.get(7)?,
        usage_count: r.get(8)?,
        success_count: r.get(9)?,
        last_used: r.get(10)?,
        auto: r.get(11)?,
        origin: r.get(12)?,
        origin_url: r.get(13)?,
        scope: r.get(14)?,
        allow_implicit: r.get(15)?,
        relations: serde_json::from_str(&r.get::<_, String>(16)?).unwrap_or(Value::Null),
        embedding: r.get(17)?,
        created_at: r.get(18)?,
        updated_at: r.get(19)?,
    })
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, DbError> {
        if let Some(parent) = path.as_ref().parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        // 并发写(route 每个子任务各开一个 Store)时避免 SQLITE_BUSY 瞬时失败。
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    pub fn open_in_memory() -> Result<Self, DbError> {
        let conn = Connection::open_in_memory()?;
        let store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<(), DbError> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY,
                workspace TEXT NOT NULL,
                summary TEXT NOT NULL DEFAULT '',
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS messages (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL REFERENCES sessions(id),
                role TEXT NOT NULL,
                content TEXT NOT NULL,
                created_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id, created_at);
            CREATE TABLE IF NOT EXISTS experiences (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                task_signature TEXT NOT NULL,
                category_guess TEXT NOT NULL DEFAULT '',
                approach TEXT NOT NULL DEFAULT '[]',
                worked TEXT NOT NULL DEFAULT '[]',
                failed TEXT NOT NULL DEFAULT '[]',
                commands TEXT NOT NULL DEFAULT '[]',
                tools_used TEXT NOT NULL DEFAULT '[]',
                outcome TEXT NOT NULL,
                skills_used TEXT NOT NULL DEFAULT '[]',
                created_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_experiences_category ON experiences(category_guess);
            CREATE TABLE IF NOT EXISTS skills (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                category TEXT NOT NULL DEFAULT '',
                path TEXT NOT NULL,
                description TEXT NOT NULL DEFAULT '',
                frontmatter TEXT NOT NULL DEFAULT '{}',
                version INTEGER NOT NULL DEFAULT 1,
                confidence REAL NOT NULL DEFAULT 0.5,
                usage_count INTEGER NOT NULL DEFAULT 0,
                success_count INTEGER NOT NULL DEFAULT 0,
                last_used TEXT,
                auto INTEGER NOT NULL DEFAULT 0,
                origin TEXT NOT NULL DEFAULT 'manual',
                origin_url TEXT,
                scope TEXT NOT NULL DEFAULT 'user',
                allow_implicit INTEGER NOT NULL DEFAULT 1,
                relations TEXT NOT NULL DEFAULT '[]',
                embedding TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE UNIQUE INDEX IF NOT EXISTS idx_skills_name ON skills(name);
            CREATE TABLE IF NOT EXISTS embedding_cache (
                content_hash TEXT PRIMARY KEY,
                provider TEXT NOT NULL,
                model TEXT NOT NULL,
                embedding TEXT NOT NULL,
                created_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS swarm_runs (
                id TEXT PRIMARY KEY,
                task TEXT NOT NULL,
                plan_json TEXT NOT NULL DEFAULT '[]',
                result_json TEXT NOT NULL DEFAULT '{}',
                created_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS audit_log (
                id TEXT PRIMARY KEY,
                ts TEXT NOT NULL,
                action TEXT NOT NULL,
                detail TEXT NOT NULL,
                actor TEXT NOT NULL DEFAULT 'raincode'
            );
            CREATE TABLE IF NOT EXISTS model_profiles (
                model TEXT PRIMARY KEY,
                reasoning REAL NOT NULL DEFAULT 0,
                coding REAL NOT NULL DEFAULT 0,
                frontend REAL NOT NULL DEFAULT 0,
                backend REAL NOT NULL DEFAULT 0,
                math REAL NOT NULL DEFAULT 0,
                long_context REAL NOT NULL DEFAULT 0,
                input_cost_per_m REAL NOT NULL DEFAULT 0,
                output_cost_per_m REAL NOT NULL DEFAULT 0,
                context_window INTEGER NOT NULL DEFAULT 0,
                source TEXT NOT NULL DEFAULT '',
                updated_at TEXT NOT NULL DEFAULT '',
                multimodal INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS cost_stats (
                task_key TEXT NOT NULL,
                kind TEXT NOT NULL,
                samples INTEGER NOT NULL DEFAULT 0,
                sum_value REAL NOT NULL DEFAULT 0,
                updated_at TEXT NOT NULL DEFAULT '',
                UNIQUE(task_key, kind)
            );
            CREATE TABLE IF NOT EXISTS navigation_log (
                id TEXT PRIMARY KEY,
                task_signature TEXT NOT NULL,
                root TEXT NOT NULL,
                path_json TEXT NOT NULL,
                outcome TEXT NOT NULL,
                model TEXT NOT NULL DEFAULT '',
                created_at TEXT NOT NULL
            );
            "#,
        )?;
        // 已存在的旧库补列:仅忽略"重复列"错误,其他 ALTER 失败如实上报。
        match self
            .conn
            .execute("ALTER TABLE model_profiles ADD COLUMN multimodal INTEGER NOT NULL DEFAULT 0", [])
        {
            Ok(_) => {}
            Err(e) if e.to_string().contains("duplicate column") => {}
            Err(e) => return Err(DbError::Sqlite(e)),
        }
        Ok(())
    }

    pub fn create_session(&self, workspace: &str) -> Result<Session, DbError> {
        let id = Uuid::new_v4().to_string();
        let ts = now();
        self.conn.execute(
            "INSERT INTO sessions (id, workspace, summary, created_at, updated_at) VALUES (?1, ?2, '', ?3, ?3)",
            params![id, workspace, ts],
        )?;
        Ok(Session {
            id,
            workspace: workspace.to_string(),
            summary: String::new(),
            created_at: ts.clone(),
            updated_at: ts,
        })
    }

    pub fn touch_session(&self, id: &str) -> Result<(), DbError> {
        self.conn.execute(
            "UPDATE sessions SET updated_at = ?1 WHERE id = ?2",
            params![now(), id],
        )?;
        Ok(())
    }

    pub fn set_session_summary(&self, id: &str, summary: &str) -> Result<(), DbError> {
        self.conn.execute(
            "UPDATE sessions SET summary = ?1, updated_at = ?2 WHERE id = ?3",
            params![summary, now(), id],
        )?;
        Ok(())
    }

    pub fn list_sessions(&self, limit: i64) -> Result<Vec<Session>, DbError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, workspace, summary, created_at, updated_at FROM sessions ORDER BY updated_at DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], |r| {
            Ok(Session {
                id: r.get(0)?,
                workspace: r.get(1)?,
                summary: r.get(2)?,
                created_at: r.get(3)?,
                updated_at: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Fetch one session by id; None if it does not exist (/resume <id> 校验用)。
    pub fn get_session(&self, id: &str) -> Result<Option<Session>, DbError> {
        self.conn
            .query_row(
                "SELECT id, workspace, summary, created_at, updated_at FROM sessions WHERE id = ?1",
                params![id],
                |r| {
                    Ok(Session {
                        id: r.get(0)?,
                        workspace: r.get(1)?,
                        summary: r.get(2)?,
                        created_at: r.get(3)?,
                        updated_at: r.get(4)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    /// Delete a session and its messages. Foreign keys are not enabled in this
    /// schema, so messages are removed explicitly (not via ON DELETE CASCADE).
    /// Deleting a nonexistent id is a no-op and reports success.
    pub fn delete_session(&self, id: &str) -> Result<(), DbError> {
        self.conn
            .execute("DELETE FROM messages WHERE session_id = ?1", params![id])?;
        self.conn
            .execute("DELETE FROM sessions WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub fn append_message(
        &self,
        session_id: &str,
        role: MessageRole,
        content: &str,
    ) -> Result<Message, DbError> {
        let id = Uuid::new_v4().to_string();
        let ts = now();
        self.conn.execute(
            "INSERT INTO messages (id, session_id, role, content, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, session_id, role_as_str(role), content, ts],
        )?;
        self.touch_session(session_id)?;
        Ok(Message {
            id,
            session_id: session_id.to_string(),
            role,
            content: content.to_string(),
            created_at: ts,
        })
    }

    pub fn list_messages(&self, session_id: &str) -> Result<Vec<Message>, DbError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, role, content, created_at FROM messages WHERE session_id = ?1 ORDER BY created_at, rowid",
        )?;
        let rows = stmt.query_map(params![session_id], |r| {
            let role: String = r.get(2)?;
            Ok(Message {
                id: r.get(0)?,
                session_id: r.get(1)?,
                role: role_from_str(&role),
                content: r.get(3)?,
                created_at: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn save_experience(&self, mut exp: ExperienceRecord) -> Result<ExperienceRecord, DbError> {
        exp.id = Uuid::new_v4().to_string();
        exp.created_at = now();
        self.conn.execute(
            r#"INSERT INTO experiences
               (id, session_id, task_signature, category_guess, approach, worked, failed, commands, tools_used, outcome, skills_used, created_at)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)"#,
            params![
                exp.id,
                exp.session_id,
                exp.task_signature,
                exp.category_guess,
                serde_json::to_string(&exp.approach)?,
                serde_json::to_string(&exp.worked)?,
                serde_json::to_string(&exp.failed)?,
                serde_json::to_string(&exp.commands)?,
                serde_json::to_string(&exp.tools_used)?,
                exp.outcome,
                serde_json::to_string(&exp.skills_used)?,
                exp.created_at,
            ],
        )?;
        Ok(exp)
    }

    pub fn list_experiences(&self, limit: Option<i64>) -> Result<Vec<ExperienceRecord>, DbError> {
        let sql = "SELECT id, session_id, task_signature, category_guess, approach, worked, failed, commands, tools_used, outcome, skills_used, created_at FROM experiences ORDER BY created_at";
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map([], |r| {
            Ok(ExperienceRecord {
                id: r.get(0)?,
                session_id: r.get(1)?,
                task_signature: r.get(2)?,
                category_guess: r.get(3)?,
                approach: serde_json::from_str(&r.get::<_, String>(4)?).unwrap_or_default(),
                worked: serde_json::from_str(&r.get::<_, String>(5)?).unwrap_or_default(),
                failed: serde_json::from_str(&r.get::<_, String>(6)?).unwrap_or_default(),
                commands: serde_json::from_str(&r.get::<_, String>(7)?).unwrap_or_default(),
                tools_used: serde_json::from_str(&r.get::<_, String>(8)?).unwrap_or_default(),
                outcome: r.get(9)?,
                skills_used: serde_json::from_str(&r.get::<_, String>(10)?).unwrap_or_default(),
                created_at: r.get(11)?,
            })
        })?;
        let mut out: Vec<ExperienceRecord> = rows.collect::<Result<_, _>>()?;
        if let Some(limit) = limit {
            out.truncate(limit as usize);
        }
        Ok(out)
    }

    pub fn upsert_skill(&self, skill: &SkillRow) -> Result<(), DbError> {
        let ts = now();
        self.conn.execute(
            r#"INSERT INTO skills
               (id, name, category, path, description, frontmatter, version, confidence, usage_count, success_count,
                last_used, auto, origin, origin_url, scope, allow_implicit, relations, embedding, created_at, updated_at)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)
               ON CONFLICT(name) DO UPDATE SET
                 category=excluded.category, path=excluded.path, description=excluded.description,
                 frontmatter=excluded.frontmatter, version=excluded.version, confidence=excluded.confidence,
                 origin=excluded.origin, origin_url=excluded.origin_url,
                 scope=excluded.scope, allow_implicit=excluded.allow_implicit, relations=excluded.relations,
                 embedding=excluded.embedding, updated_at=excluded.updated_at"#,
            params![
                skill.id, skill.name, skill.category, skill.path, skill.description,
                serde_json::to_string(&skill.frontmatter)?, skill.version, skill.confidence,
                skill.usage_count, skill.success_count, skill.last_used,
                skill.auto as i64, skill.origin, skill.origin_url, skill.scope,
                skill.allow_implicit as i64, serde_json::to_string(&skill.relations)?,
                skill.embedding, ts.clone(), ts,
            ],
        )?;
        Ok(())
    }

    pub fn get_skill(&self, name: &str) -> Result<Option<SkillRow>, DbError> {
        self.conn
            .query_row(
                "SELECT id, name, category, path, description, frontmatter, version, confidence, usage_count, success_count,
                        last_used, auto, origin, origin_url, scope, allow_implicit, relations, embedding, created_at, updated_at
                 FROM skills WHERE name = ?1",
                params![name],
                skill_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn delete_skill(&self, name: &str) -> Result<(), DbError> {
        self.conn
            .execute("DELETE FROM skills WHERE name = ?1", params![name])?;
        Ok(())
    }

    /// 列出全部技能及学习状态(usage_count / success_count / confidence / last_used)。
    /// 供 UI 展示"skill 网络是否在学习"。
    pub fn list_skills(&self) -> Result<Vec<SkillRow>, DbError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, category, path, description, frontmatter, version, confidence, usage_count, success_count,
                    last_used, auto, origin, origin_url, scope, allow_implicit, relations, embedding, created_at, updated_at
             FROM skills ORDER BY usage_count DESC, name",
        )?;
        let rows = stmt.query_map([], skill_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn bump_skill_usage(&self, name: &str, success: bool) -> Result<(), DbError> {
        let ts = now();
        if success {
            self.conn.execute(
                "UPDATE skills SET usage_count = usage_count + 1, success_count = success_count + 1, last_used = ?2 WHERE name = ?1",
                params![name, ts],
            )?;
        } else {
            self.conn.execute(
                "UPDATE skills SET usage_count = usage_count + 1, last_used = ?2 WHERE name = ?1",
                params![name, ts],
            )?;
        }
        Ok(())
    }

    pub fn cache_embedding(
        &self,
        content_hash: &str,
        provider: &str,
        model: &str,
        embedding: &str,
    ) -> Result<(), DbError> {
        let ts = now();
        self.conn.execute(
            "INSERT OR REPLACE INTO embedding_cache (content_hash, provider, model, embedding, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![content_hash, provider, model, embedding, ts],
        )?;
        Ok(())
    }

    pub fn get_embedding(&self, content_hash: &str) -> Result<Option<String>, DbError> {
        self.conn
            .query_row(
                "SELECT embedding FROM embedding_cache WHERE content_hash = ?1",
                params![content_hash],
                |r| r.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn add_audit(
        &self,
        action: &str,
        detail: &str,
        actor: &str,
    ) -> Result<AuditEntry, DbError> {
        let id = Uuid::new_v4().to_string();
        let ts = now();
        self.conn.execute(
            "INSERT INTO audit_log (id, ts, action, detail, actor) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, ts, action, detail, actor],
        )?;
        Ok(AuditEntry {
            id,
            ts,
            action: action.to_string(),
            detail: detail.to_string(),
            actor: actor.to_string(),
        })
    }

    /// 读取最近的审计日志(演化采纳/拒绝等写审计,测试据此断言落盘)。
    pub fn list_audit(&self, limit: i64) -> Result<Vec<AuditEntry>, DbError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, ts, action, detail, actor FROM audit_log ORDER BY ts DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], |r| {
            Ok(AuditEntry {
                id: r.get(0)?,
                ts: r.get(1)?,
                action: r.get(2)?,
                detail: r.get(3)?,
                actor: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn save_swarm_run(
        &self,
        task: &str,
        plan_json: &str,
        result_json: &str,
    ) -> Result<String, DbError> {
        let id = Uuid::new_v4().to_string();
        self.conn.execute(
            "INSERT INTO swarm_runs (id, task, plan_json, result_json, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, task, plan_json, result_json, now()],
        )?;
        Ok(id)
    }

    pub fn list_swarm_runs(&self, limit: usize) -> Result<Vec<(String, String, String)>, DbError> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, task, created_at FROM swarm_runs ORDER BY created_at DESC LIMIT ?1")?;
        let rows = stmt.query_map(params![limit as i64], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// 用新消息整体替换一个会话的历史(compact 用):单事务内先删后插,再 touch_session。
    /// 事务保证中途失败不留下半截历史(autocommit 逐条执行时失败会破坏会话)。
    pub fn replace_messages(
        &self,
        session_id: &str,
        messages: &[Message],
    ) -> Result<(), DbError> {
        // Store 方法签名是 &self(并发只读共享),transaction() 需要 &mut self,
        // 用 unchecked_transaction()(&self 版)。busy_timeout 已在上层设 5s 防 SQLITE_BUSY。
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM messages WHERE session_id = ?1", params![session_id])?;
        for m in messages {
            tx.execute(
                "INSERT INTO messages (id, session_id, role, content, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![m.id, m.session_id, role_as_str(m.role), m.content, m.created_at],
            )?;
        }
        tx.execute(
            "UPDATE sessions SET updated_at = ?1 WHERE id = ?2",
            params![now(), session_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn upsert_model_profile(&self, p: &CapabilityProfileRow) -> Result<(), DbError> {
        self.conn.execute(
            "INSERT INTO model_profiles
                (model, reasoning, coding, frontend, backend, math, long_context,
                 input_cost_per_m, output_cost_per_m, context_window, source, updated_at, multimodal)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)
             ON CONFLICT(model) DO UPDATE SET
                reasoning=?2, coding=?3, frontend=?4, backend=?5, math=?6, long_context=?7,
                input_cost_per_m=?8, output_cost_per_m=?9, context_window=?10, source=?11, updated_at=?12,
                multimodal=?13",
            rusqlite::params![
                p.model, p.reasoning, p.coding, p.frontend, p.backend, p.math, p.long_context,
                p.input_cost_per_m, p.output_cost_per_m, p.context_window, p.source, p.updated_at,
                p.multimodal
            ],
        )?;
        Ok(())
    }

    /// 按主键删除一个模型能力画像行(profiles enrich 合并后删除旧版本用)。
    pub fn delete_model_profile(&self, model: &str) -> Result<(), DbError> {
        self.conn.execute("DELETE FROM model_profiles WHERE model = ?1", [model])?;
        Ok(())
    }

    pub fn get_model_profile(&self, model: &str) -> Result<Option<CapabilityProfileRow>, DbError> {
        self.conn
            .query_row(
                "SELECT model, reasoning, coding, frontend, backend, math, long_context,
                        input_cost_per_m, output_cost_per_m, context_window, source, updated_at, multimodal
                 FROM model_profiles WHERE model = ?1",
                params![model],
                capability_profile_from_row,
            )
            .optional()
            .map_err(DbError::from)
    }

    pub fn all_model_profiles(&self) -> Result<Vec<CapabilityProfileRow>, DbError> {
        let mut stmt = self.conn.prepare(
            "SELECT model, reasoning, coding, frontend, backend, math, long_context,
                    input_cost_per_m, output_cost_per_m, context_window, source, updated_at, multimodal
             FROM model_profiles ORDER BY model",
        )?;
        let rows = stmt.query_map([], capability_profile_from_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// 累计一条成本观测(task_key 按 task+model 维度,kind 如 tokens/elapsed_ms/cost)。
    pub fn record_stat(&self, task_key: &str, kind: &str, value: f64) -> Result<(), DbError> {
        self.conn.execute(
            "INSERT INTO cost_stats (task_key, kind, samples, sum_value, updated_at)
             VALUES (?1, ?2, 1, ?3, ?4)
             ON CONFLICT(task_key, kind) DO UPDATE SET
                samples = samples + 1,
                sum_value = sum_value + excluded.sum_value,
                updated_at = excluded.updated_at",
            rusqlite::params![task_key, kind, value, now()],
        )?;
        Ok(())
    }

    /// 取统计快照:(samples, avg),无记录返回 None。
    pub fn get_stat(&self, task_key: &str, kind: &str) -> Result<Option<(u64, f64)>, DbError> {
        let mut stmt = self.conn.prepare(
            "SELECT samples, sum_value FROM cost_stats WHERE task_key = ?1 AND kind = ?2",
        )?;
        let rows = stmt.query_map([task_key, kind], |r| Ok((r.get::<_, u64>(0)?, r.get::<_, f64>(1)?)))?;
        let row = rows.into_iter().next().transpose()?;
        Ok(row.map(|(s, sum)| (s, if s > 0 { sum / s as f64 } else { 0.0 })))
    }

    /// 记录一次 skill 导航决策(navigation_log),id/created_at 由 Store 生成。
    pub fn record_navigation(&self, rec: &NavigationRecord) -> Result<(), DbError> {
        let id = Uuid::new_v4().to_string();
        let ts = now();
        self.conn.execute(
            "INSERT INTO navigation_log (id, task_signature, root, path_json, outcome, model, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![id, rec.task_signature, rec.root, rec.path_json,
                    serde_json::to_string(&rec.outcome).unwrap_or_default(),
                    rec.model, ts],
        )?;
        Ok(())
    }

    /// 列出最近的导航记录,按时间倒序(演化循环的 fitness 数据源)。
    pub fn list_navigation(&self, limit: i64) -> Result<Vec<NavigationRecord>, DbError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, task_signature, root, path_json, outcome, model, created_at
             FROM navigation_log ORDER BY created_at DESC LIMIT ?1")?;
        let rows = stmt.query_map(params![limit], |r| {
            let outcome: String = r.get(4)?;
            Ok(NavigationRecord {
                id: r.get(0)?, task_signature: r.get(1)?, root: r.get(2)?,
                path_json: r.get(3)?,
                outcome: serde_json::from_str(&outcome).unwrap_or(NavOutcome::Success),
                model: r.get(5)?, created_at: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }
}

fn capability_profile_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<CapabilityProfileRow> {
    Ok(CapabilityProfileRow {
        model: r.get(0)?, reasoning: r.get(1)?, coding: r.get(2)?, frontend: r.get(3)?,
        backend: r.get(4)?, math: r.get(5)?, long_context: r.get(6)?,
        input_cost_per_m: r.get(7)?, output_cost_per_m: r.get(8)?, context_window: r.get(9)?,
        source: r.get(10)?, updated_at: r.get(11)?, multimodal: r.get::<_, bool>(12)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_and_messages_roundtrip() {
        let store = Store::open_in_memory().unwrap();
        let s = store.create_session("/tmp/proj").unwrap();
        store
            .append_message(&s.id, MessageRole::User, "hello")
            .unwrap();
        store
            .append_message(&s.id, MessageRole::Assistant, "world")
            .unwrap();
        let msgs = store.list_messages(&s.id).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].content, "hello");
        assert_eq!(store.list_sessions(10).unwrap().len(), 1);
    }

    #[test]
    fn get_session_returns_none_for_missing() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.get_session("nope").unwrap().is_none());
        let s = store.create_session("/tmp/p").unwrap();
        assert!(store.get_session(&s.id).unwrap().is_some());
    }

    #[test]
    fn replace_messages_swaps_history() {
        let store = Store::open_in_memory().unwrap();
        let s = store.create_session("/tmp/proj").unwrap();
        store.append_message(&s.id, MessageRole::User, "old1").unwrap();
        store.append_message(&s.id, MessageRole::User, "old2").unwrap();
        let replacement = vec![
            Message::new(&s.id, MessageRole::User, "前文摘要: summarized"),
            Message::new(&s.id, MessageRole::Assistant, "recent"),
        ];
        store.replace_messages(&s.id, &replacement).unwrap();
        let msgs = store.list_messages(&s.id).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].content, "前文摘要: summarized");
        assert_eq!(msgs[1].content, "recent");
        // touch_session 生效:updated_at 更新。
        let sess = store.list_sessions(10).unwrap();
        assert!(!sess[0].updated_at.is_empty());
    }

    #[test]
    fn delete_session_removes_messages_and_row() {
        let store = Store::open_in_memory().unwrap();
        let s = store.create_session("/tmp/proj").unwrap();
        store
            .append_message(&s.id, MessageRole::User, "hello")
            .unwrap();
        let other = store.create_session("/tmp/proj2").unwrap();
        store
            .append_message(&other.id, MessageRole::User, "keep me")
            .unwrap();
        store.delete_session(&s.id).unwrap();
        assert_eq!(store.list_sessions(10).unwrap().len(), 1);
        assert_eq!(store.list_sessions(10).unwrap()[0].id, other.id);
        assert!(store.list_messages(&s.id).unwrap().is_empty());
        // Deleting a nonexistent id is a no-op, not an error.
        store.delete_session("nope").unwrap();
    }

    #[test]
    fn swarm_runs_roundtrip() {
        let store = Store::open_in_memory().unwrap();
        let id = store
            .save_swarm_run("fix tests", "[]", "{\"ok\":true}")
            .unwrap();
        let runs = store.list_swarm_runs(5).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].0, id);
        assert_eq!(runs[0].1, "fix tests");
    }
    #[test]
    fn skill_upsert_and_bump() {
        let store = Store::open_in_memory().unwrap();
        let row = SkillRow {
            id: "1".into(),
            name: "pytest-flake-fix".into(),
            category: "testing.pytest".into(),
            path: "/skills/pytest/SKILL.md".into(),
            description: "fix flaky pytest".into(),
            frontmatter: serde_json::json!({}),
            version: 1,
            confidence: 0.6,
            usage_count: 0,
            success_count: 0,
            last_used: None,
            auto: true,
            origin: "evolved".into(),
            origin_url: None,
            scope: "user".into(),
            allow_implicit: true,
            relations: serde_json::json!([]),
            embedding: None,
            created_at: now(),
            updated_at: now(),
        };
        store.upsert_skill(&row).unwrap();
        store.bump_skill_usage("pytest-flake-fix", true).unwrap();
        let loaded = store.get_skill("pytest-flake-fix").unwrap().unwrap();
        assert_eq!(loaded.usage_count, 1);
        assert_eq!(loaded.success_count, 1);
        let mut refreshed = row.clone();
        // 重新 upsert(frontmatter 里的 usage 是 0)不能覆盖已累积的学习统计。
        refreshed.usage_count = 3;
        refreshed.success_count = 2;
        refreshed.confidence = 0.8;
        store.upsert_skill(&refreshed).unwrap();
        let loaded = store.get_skill("pytest-flake-fix").unwrap().unwrap();
        // 学习统计保留(来自 bump_skill_usage),不被 frontmatter 值覆盖。
        assert_eq!(loaded.usage_count, 1);
        assert_eq!(loaded.success_count, 1);
        // 元数据(confidence)照常更新。
        assert_eq!(loaded.confidence, 0.8);
    }

    #[test]
    fn upsert_skill_preserves_usage_stats_on_reupsert() {
        let store = Store::open_in_memory().unwrap();
        let mk = || SkillRow {
            id: "id-x".into(),
            name: "skill-x".into(),
            category: "cat".into(),
            path: "skills/skill-x".into(),
            description: "desc".into(),
            frontmatter: serde_json::json!({}),
            version: 1,
            confidence: 0.9,
            usage_count: 0, // frontmatter 总是 0
            success_count: 0,
            last_used: None,
            auto: false,
            origin: "seed".into(),
            origin_url: None,
            scope: "system".into(),
            allow_implicit: true,
            relations: serde_json::json!([]),
            embedding: None,
            created_at: now(),
            updated_at: now(),
        };
        store.upsert_skill(&mk()).unwrap();
        store.bump_skill_usage("skill-x", true).unwrap();
        // 重复 re-seed / re-index(usage_count=0)不得重置学习统计。
        store.upsert_skill(&mk()).unwrap();
        store.upsert_skill(&mk()).unwrap();
        let loaded = store.get_skill("skill-x").unwrap().unwrap();
        assert_eq!(loaded.usage_count, 1);
        assert_eq!(loaded.success_count, 1);
        assert!(loaded.last_used.is_some());
    }

    #[test]
    fn list_skills_returns_all_with_stats() {
        let store = Store::open_in_memory().unwrap();
        for name in ["skill-a", "skill-b"] {
            store
                .upsert_skill(&SkillRow {
                    id: format!("id-{name}"),
                    name: name.into(),
                    category: "cat".into(),
                    path: format!("skills/{name}"),
                    description: "desc".into(),
                    frontmatter: serde_json::json!({}),
                    version: 1,
                    confidence: 0.9,
                    usage_count: 0,
                    success_count: 0,
                    last_used: None,
                    auto: false,
                    origin: "manual".into(),
                    origin_url: None,
                    scope: "user".into(),
                    allow_implicit: true,
                    relations: serde_json::json!([]),
                    embedding: None,
                    created_at: now(),
                    updated_at: now(),
                })
                .unwrap();
        }
        store.bump_skill_usage("skill-a", true).unwrap();
        let all = store.list_skills().unwrap();
        assert_eq!(all.len(), 2);
        // 使用过的排前面(bump 后 skill-a usage=1)。
        assert_eq!(all[0].name, "skill-a");
        assert_eq!(all[0].usage_count, 1);
        assert_eq!(all[1].name, "skill-b");
        assert_eq!(all[1].usage_count, 0);
    }

    #[test]
    fn model_profile_roundtrip() {
        let store = Store::open_in_memory().unwrap();
        let p = CapabilityProfileRow {
            model: "deepseek-v4".into(),
            reasoning: 85.0, coding: 92.0, frontend: 80.0, backend: 95.0,
            math: 88.0, long_context: 90.0,
            input_cost_per_m: 0.1, output_cost_per_m: 0.3,
            context_window: 128_000, source: "seed".into(), updated_at: "t".into(),
            multimodal: false,
        };
        store.upsert_model_profile(&p).unwrap();
        let got = store.get_model_profile("deepseek-v4").unwrap().unwrap();
        assert_eq!(got.model, "deepseek-v4");
        assert!((got.coding - 92.0).abs() < 1e-6);
        assert_eq!(got.context_window, 128_000);
        // upsert 覆盖
        let mut p2 = p.clone();
        p2.coding = 93.0;
        store.upsert_model_profile(&p2).unwrap();
        assert!((store.get_model_profile("deepseek-v4").unwrap().unwrap().coding - 93.0).abs() < 1e-6);
        assert_eq!(store.all_model_profiles().unwrap().len(), 1);
    }

    #[test]
    fn delete_model_profile_removes_row() {
        let store = Store::open_in_memory().unwrap();
        // CapabilityProfileRow 无 Default,按 roundtrip 测试构造全字段
        let p = CapabilityProfileRow {
            model: "qwen/qwen3.8-max".into(),
            reasoning: 80.0, coding: 80.0, frontend: 80.0, backend: 80.0,
            math: 80.0, long_context: 80.0, input_cost_per_m: 1.0, output_cost_per_m: 2.0,
            context_window: 128_000, source: "s".into(), updated_at: "t".into(),
            multimodal: false,
        };
        store.upsert_model_profile(&p).unwrap();
        assert!(store.get_model_profile("qwen/qwen3.8-max").unwrap().is_some());
        store.delete_model_profile("qwen/qwen3.8-max").unwrap();
        assert!(store.get_model_profile("qwen/qwen3.8-max").unwrap().is_none());
    }

    #[test]
    fn cost_stats_and_multimodal() {
        let store = Store::open_in_memory().unwrap();
        // multimodal 列:roundtrip 含 multimodal 字段
        let p = CapabilityProfileRow {
            model: "m".into(),
            reasoning: 80.0, coding: 80.0, frontend: 80.0, backend: 80.0,
            math: 80.0, long_context: 80.0, input_cost_per_m: 1.0, output_cost_per_m: 2.0,
            context_window: 128_000, source: "s".into(), updated_at: "t".into(),
            multimodal: true,
        };
        store.upsert_model_profile(&p).unwrap();
        assert!(store.get_model_profile("m").unwrap().unwrap().multimodal);
        // cost_stats 累计
        store.record_stat("deepseek|ab12", "tokens", 1000.0).unwrap();
        store.record_stat("deepseek|ab12", "tokens", 2000.0).unwrap();
        let (samples, avg) = store.get_stat("deepseek|ab12", "tokens").unwrap().unwrap();
        assert_eq!(samples, 2);
        assert!((avg - 1500.0).abs() < 1e-6);
        assert!(store.get_stat("nope", "tokens").unwrap().is_none());
    }

    #[test]
    fn navigation_record_roundtrip() {
        let store = Store::open_in_memory().unwrap();
        let rec = NavigationRecord {
            id: String::new(), task_signature: "build react page".into(),
            root: "react".into(), path_json: r#"["react","react.performance"]"#.into(),
            outcome: NavOutcome::WrongBranch, model: "deepseek".into(), created_at: String::new(),
        };
        store.record_navigation(&rec).unwrap();
        let list = store.list_navigation(10).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].root, "react");
        assert!(matches!(list[0].outcome, NavOutcome::WrongBranch));
    }
}
