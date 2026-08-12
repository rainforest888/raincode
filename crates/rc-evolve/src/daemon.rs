//! Background pattern daemon: periodically clusters all experience records,
//! abstracts uncovered clusters into higher-order skills (`composes`
//! relations), compacts near-duplicate skills, and decays stale skills.
use crate::cluster::greedy_clusters;
use crate::darwinian::{apply_organism, evolve_skill, Organism};
use crate::engine::{parse_skill_md, EvolveError};
use crate::nav_feedback::digest_navigation;
use chrono::{DateTime, Utc};
use futures::StreamExt;
use rc_pro::Provider;
use rc_skill::{enforce_skill_shape, SkillNetwork, SkillRouter, SkillStore};
use rc_state::Store;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonConfig {
    #[serde(default = "default_interval")]
    pub interval_minutes: u64,
    #[serde(default = "default_cluster_min")]
    pub min_cluster: usize,
    #[serde(default = "default_threshold")]
    pub similarity_threshold: f32,
    #[serde(default = "default_coverage")]
    pub coverage_factor: f32,
    #[serde(default = "default_stale_days")]
    pub stale_days: i64,
    #[serde(default = "default_lock")]
    pub lock_path: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 新生成的组 skill 是否直接进 user 作用域(false 时进 .review 隔离区等用户批准)。
    #[serde(default = "default_auto_approve")]
    pub auto_approve: bool,
}

fn default_auto_approve() -> bool {
    true
}

fn default_interval() -> u64 {
    15
}
fn default_cluster_min() -> usize {
    3
}
fn default_threshold() -> f32 {
    0.78
}
fn default_coverage() -> f32 {
    0.95
}
fn default_stale_days() -> i64 {
    30
}
fn default_lock() -> String {
    "~/.raincode/.daemon.lock".into()
}
fn default_true() -> bool {
    true
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            interval_minutes: default_interval(),
            min_cluster: default_cluster_min(),
            similarity_threshold: default_threshold(),
            coverage_factor: default_coverage(),
            stale_days: default_stale_days(),
            lock_path: default_lock(),
            enabled: default_true(),
            auto_approve: default_auto_approve(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DaemonReport {
    pub clusters_found: usize,
    pub skills_proposed: Vec<String>,
    pub skills_merged: Vec<String>,
    pub skills_decayed: Vec<String>,
    pub skills_promoted: Vec<String>,
    /// 导航反馈消化:成功应用的菜单改写候选 skill 名(auto_approve 直接写,否则进 .review)。
    pub menu_rewrites: Vec<String>,
    /// 导航反馈消化:成功应用的叶子补充候选 skill 名。
    pub leaf_backfills: Vec<String>,
    /// darwinian 演化:成功采纳/或 stage 进 .review 的变体 skill 名(回溯率 > 0.5 → 变异)。
    pub skills_evolved: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error("state error: {0}")]
    State(#[from] rc_state::DbError),
    #[error("evolve error: {0}")]
    Evolve(#[from] EvolveError),
    #[error("lock error: {0}")]
    Lock(String),
    #[error("io error: {0}")]
    Io(String),
}

pub struct PatternDaemon {
    provider: Arc<dyn Provider>,
    store: Store,
    skill_store: SkillStore,
    config: DaemonConfig,
    lock_path: PathBuf,
}

impl PatternDaemon {
    pub fn new(
        provider: Arc<dyn Provider>,
        store: Store,
        skill_store: SkillStore,
        config: DaemonConfig,
    ) -> Self {
        let lock_path = expand_tilde(Path::new(&config.lock_path));
        Self {
            provider,
            store,
            skill_store,
            config,
            lock_path,
        }
    }

    /// Acquire the single-instance lock; fails when another daemon runs.
    pub fn acquire_lock(&self) -> Result<LockGuard, DaemonError> {
        if let Some(parent) = self.lock_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| DaemonError::Io(e.to_string()))?;
        }
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&self.lock_path)
        {
            Ok(file) => {
                let _ = std::fs::write(&self.lock_path, format!("pid={}", std::process::id()));
                Ok(LockGuard {
                    path: self.lock_path.clone(),
                    _file: file,
                })
            }
            Err(e) => Err(DaemonError::Lock(format!("daemon already running: {e}"))),
        }
    }

    pub fn offline_window_active(&self) -> Result<bool, DaemonError> {
        let recent = self.store.list_sessions(5)?;
        let now = Utc::now();
        for s in recent {
            if let Ok(ts) = DateTime::parse_from_rfc3339(&s.updated_at) {
                let age = now.signed_duration_since(ts.with_timezone(&Utc));
                if age.num_minutes() < 10 {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    /// Run forever: sleep -> offline check -> scan.
    pub async fn run(&self) -> Result<(), DaemonError> {
        let _guard = self.acquire_lock()?;
        tracing::info!(
            "raincode daemon started (interval {}m)",
            self.config.interval_minutes
        );
        loop {
            tokio::time::sleep(Duration::from_secs(self.config.interval_minutes * 60)).await;
            match self.offline_window_active() {
                Ok(true) => {
                    let report = self.scan().await?;
                    if !report.skills_proposed.is_empty() || !report.skills_merged.is_empty() {
                        tracing::info!("daemon scan: {:?}", report);
                    }
                }
                Ok(false) => tracing::debug!("active session; skipping scan"),
                Err(e) => tracing::warn!("daemon offline check failed: {e}"),
            }
        }
    }

    /// One full cross-session scan: cluster -> propose higher-order skills ->
    /// compact duplicates -> decay stale skills.
    pub async fn scan(&self) -> Result<DaemonReport, DaemonError> {
        let mut report = DaemonReport::default();
        let experiences = self.store.list_experiences(Some(5000))?;
        if experiences.len() < self.config.min_cluster {
            // 经验不足:跳过聚类/提案(与历史行为一致),但导航反馈 + darwinian 演化
            // 仍要跑 —— 该 pass 的数据源是 navigation_log,不依赖 experience。
            self.navigation_pass(&mut report).await?;
            return Ok(report);
        }
        let texts: Vec<String> = experiences
            .iter()
            .map(|e| {
                format!(
                    "{} {} {}",
                    e.task_signature,
                    e.category_guess,
                    e.approach.join(" ")
                )
            })
            .collect();
        let Ok(mut embeddings) = self.provider.embed(texts).await else {
            tracing::warn!("daemon: embedding failed, skipping cluster scan");
            self.navigation_pass(&mut report).await?;
            return Ok(report);
        };
        // Align embedding count with records (mock/local providers may batch).
        embeddings.truncate(experiences.len());
        // 用与真实向量相同的维度补零:1 维 [0.0] 会让 cosine 因长度不匹配恒为 0,
        // 这些记录的聚类永远被静默丢弃(部分批次丢尾 = 跨会话挖掘缺记录)。
        let dim = embeddings
            .iter()
            .find(|e| !e.is_empty())
            .map(Vec::len)
            .unwrap_or(0);
        while embeddings.len() < experiences.len() {
            embeddings.push(vec![0.0; dim]);
        }
        let ids: Vec<String> = experiences.iter().map(|e| e.id.clone()).collect();
        let sessions: Vec<String> = experiences.iter().map(|e| e.session_id.clone()).collect();
        let tasks: Vec<String> = experiences
            .iter()
            .map(|e| e.task_signature.clone())
            .collect();
        let cats: Vec<String> = experiences
            .iter()
            .map(|e| e.category_guess.clone())
            .collect();
        let clusters = greedy_clusters(
            &ids,
            &sessions,
            &tasks,
            &cats,
            &embeddings,
            self.config.similarity_threshold,
        );
        report.clusters_found = clusters.len();

        let skills = self.skill_store.discover();
        let router = SkillRouter::new(skills.clone());
        for cluster in &clusters {
            if cluster.record_ids.len() < self.config.min_cluster {
                continue;
            }
            let distinct_sessions: std::collections::HashSet<&String> =
                cluster.session_ids.iter().collect();
            if distinct_sessions.len() < 2 {
                continue;
            }
            let Some(centroid) = &cluster.centroid else {
                continue;
            };
            // Covered by an existing skill?
            let covered =
                router.select_with_embedding(&cluster.task_signatures.join(" "), centroid, 1);
            let already_covered = covered
                .first()
                .map(|s| s.score >= self.config.similarity_threshold * self.config.coverage_factor)
                .unwrap_or(false);
            if already_covered {
                continue;
            }
            let member_skills: Vec<String> = skills
                .iter()
                .filter(|s| cluster.task_signatures.iter().any(|t| t.contains(&s.name)))
                .map(|s| s.name.clone())
                .collect();
            if let Ok(name) = self.propose_group_skill(cluster, &member_skills).await {
                report.skills_proposed.push(name);
            }
        }
        report.skills_merged = self.compact_skills()?;
        report.skills_decayed = self.decay_stale()?;
        report.skills_promoted = self.promote_project_skills()?;
        // 导航反馈消化 + darwinian 演化(见 navigation_evolution)。
        self.navigation_pass(&mut report).await?;
        Ok(report)
    }

    /// 把导航反馈 + darwinian 演化 pass 的结果写进报告。
    async fn navigation_pass(&self, report: &mut DaemonReport) -> Result<(), DaemonError> {
        let (menu_rewrites, leaf_backfills, skills_evolved) = self.navigation_evolution().await?;
        report.menu_rewrites = menu_rewrites;
        report.leaf_backfills = leaf_backfills;
        report.skills_evolved = skills_evolved;
        Ok(())
    }

    /// 导航反馈消化 + darwinian 演化:把 navigation_log 变成 skill 网络改进。
    /// 1) digest_navigation → 菜单改写/叶子补充候选:auto_approve 直接应用,否则
    ///    stage 进 .review 隔离区等用户批准(与 propose_group_skill 的保守策略一致)。
    /// 2) 有导航记录的 skill(root ∈ navigation_log)→ evolve_skill(回溯率 > 0.5 才
    ///    调模型)→ Some(organism) 时走与菜单改写同一条 stage_nav_rewrite:auto_approve
    ///    直接 apply_organism 落盘,否则 stage 进 .review(后台进程绝不静默覆盖用户 skill)。
    ///    返回 (menu_rewrites, leaf_backfills, skills_evolved)——只含成功应用/采纳或已 stage 的。
    async fn navigation_evolution(&self) -> Result<(Vec<String>, Vec<String>, Vec<String>), DaemonError> {
        let mut menu_rewrites = Vec::new();
        let mut leaf_backfills = Vec::new();
        let mut evolved = Vec::new();
        // 1) 导航反馈消化:WrongBranch → 菜单改写;LeafTooThin → 叶子补充。
        let digest = digest_navigation(self.provider.as_ref(), &self.store, &self.skill_store)
            .await
            .map_err(DaemonError::Io)?;
        for (name, variant) in &digest.menu_rewrites {
            match self.stage_nav_rewrite(name, variant).await {
                Ok(true) => menu_rewrites.push(name.clone()),
                Ok(false) => {}
                Err(e) => tracing::warn!("daemon: menu rewrite {name} skipped: {e}"),
            }
        }
        for (name, variant) in &digest.leaf_backfills {
            match self.stage_nav_rewrite(name, variant).await {
                Ok(true) => leaf_backfills.push(name.clone()),
                Ok(false) => {}
                Err(e) => tracing::warn!("daemon: leaf backfill {name} skipped: {e}"),
            }
        }
        // 2) darwinian:只对有导航记录的 skill 演化(高流量信号),避免对冷门 skill 白跑模型。
        let nav_roots: std::collections::HashSet<String> = self
            .store
            .list_navigation(5000)
            .unwrap_or_default()
            .into_iter()
            .map(|r| r.root)
            .collect();
        for skill in self.skill_store.discover() {
            if !nav_roots.contains(&skill.name) {
                continue;
            }
            let current = skill.render().unwrap_or_default();
            match evolve_skill(self.provider.as_ref(), &self.store, &skill.name, &current).await {
                Ok(Some(organism)) => {
                    // 与菜单改写/叶子补充同一条保守路径:auto_approve 直接 apply_organism
                    // 落盘,否则 stage 进 .review 隔离区等用户批准——后台进程绝不静默覆盖用户 skill。
                    match self.stage_nav_rewrite(&skill.name, &organism.variant).await {
                        Ok(true) => evolved.push(skill.name.clone()),
                        Ok(false) => {}
                        Err(e) => tracing::warn!("daemon: adopt {} skipped: {e}", skill.name),
                    }
                }
                Ok(None) => {}
                Err(e) => tracing::warn!("daemon: evolve {} failed: {e}", skill.name),
            }
        }
        Ok((menu_rewrites, leaf_backfills, evolved))
    }

    /// 应用一个演化/导航改写候选(菜单改写/叶子补充/darwinian 变体):auto_approve
    /// 直接写 user skill,否则 scope=review 进 .review 隔离区。解析/形状/名称任一不过 → Err(调用方记日志)。
    async fn stage_nav_rewrite(&self, skill_name: &str, variant: &str) -> Result<bool, DaemonError> {
        if self.config.auto_approve {
            // 直接采纳:apply_organism 负责名称校验/形状把关/遥测保留/审计。
            let organism = Organism {
                skill_name: skill_name.to_string(),
                variant: variant.to_string(),
            };
            let ok = apply_organism(&self.store, &self.skill_store, &organism)
                .await
                .map_err(DaemonError::Io)?;
            return Ok(ok);
        }
        // 保守路径:stage 进 .review,等 `skills review <name> --approve`。
        let existing = self
            .skill_store
            .load(skill_name)
            .ok_or_else(|| DaemonError::Io(format!("candidate '{skill_name}' not found")))?;
        let (fm, body) = rc_skill::parse_frontmatter(variant)
            .map_err(|e| DaemonError::Io(format!("candidate '{skill_name}' invalid SKILL.md: {e}")))?;
        if fm.name != existing.name {
            return Err(DaemonError::Io(format!(
                "candidate name '{}' does not match skill '{}'",
                fm.name, existing.name
            )));
        }
        let mut new_skill = rc_skill::Skill::from_frontmatter(fm, body, existing.path.clone());
        new_skill.category = existing.category.clone();
        new_skill.path = existing.path.clone();
        new_skill.version = existing.version.saturating_add(1);
        new_skill.scope = "review".into();
        // 遥测保留(与 apply_organism 一致):staged 变体不重置使用数据。
        new_skill.usage_count = existing.usage_count;
        new_skill.success_rate = existing.success_rate;
        new_skill.last_used = existing.last_used.clone();
        new_skill.confidence = existing.confidence;
        new_skill.embedding = existing.embedding.clone();
        new_skill.allow_implicit = existing.allow_implicit;
        new_skill.origin_url = existing.origin_url.clone();
        let network = rc_skill::SkillNetwork::from_store(&self.skill_store);
        rc_skill::enforce_skill_shape(&network, &new_skill)
            .map_err(|e| DaemonError::Io(format!("candidate '{skill_name}' violates shape: {e}")))?;
        let path = self.skill_store.save(&new_skill).map_err(DaemonError::Io)?;
        self.store.add_audit(
            "daemon.nav_rewrite.staged",
            &format!("staged {} v{} at {}", skill_name, new_skill.version, path.display()),
            "daemon",
        )?;
        Ok(true)
    }

    async fn propose_group_skill(
        &self,
        cluster: &crate::cluster::Cluster,
        members: &[String],
    ) -> Result<String, DaemonError> {
        let member_list = if members.is_empty() {
            "(none; refer to categories)".to_string()
        } else {
            members.join(", ")
        };
        let prompt = format!(
            "You are the Raincode pattern abstractor. These cross-session tasks share a common method. \
             Write a higher-order SKILL.md that captures the shared abstraction. \
             In the frontmatter include relations as a list of {{kind: composes, skill: <member>}} for each member below. \
             Return ONLY the SKILL.md.\n\nTasks:\n{}\n\nMembers:\n{}",
            cluster.task_signatures.join("\n"),
            member_list,
        );
        let req = rc_pro::CanonicalRequest {
            model: self.provider.id().to_string(),
            messages: vec![rc_pro::CanonicalMessage::user(&prompt)],
            tools: vec![],
            temperature: Some(0.3),
            max_tokens: Some(3000),
            stream: true,
            extra: serde_json::json!({}),
        };
        let Ok(mut stream) = self.provider.stream(req).await else {
            return Err(DaemonError::Io("provider stream failed".into()));
        };
        let mut text = String::new();
        while let Some(Ok(ev)) = stream.next().await {
            if let rc_pro::ProvEvent::Delta { text: t } = ev {
                text.push_str(&t);
            }
        }
        let mut skill = parse_skill_md(&text).map_err(DaemonError::Io)?;
        skill.auto = true;
        skill.origin = "evolved".into();
        // 与 engine::propose_skill 一致:auto_approve=false 时进 .review 隔离区等用户批准,
        // 不能由后台进程直接把 LLM 生成的 skill 静默写入活跃集合。
        skill.scope = if self.config.auto_approve {
            "user".into()
        } else {
            "review".into()
        };
        skill.confidence = 0.65;
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
            "> Auto-generated by Raincode PatternDaemon at {}.\n\n{}",
            Utc::now().to_rfc3339(),
            skill.body.trim()
        );
        // 演化把关(与 engine::propose_skill 一致):形状违规或重名 → 拒绝并记审计,
        // 后台进程绝不静默覆盖用户 skill(索引无正文/叶子有正文 + 硬性重名校验)。
        let network = SkillNetwork::from_store(&self.skill_store);
        if let Err(e) = enforce_skill_shape(&network, &skill) {
            let _ = self.store.add_audit(
                "daemon.propose.rejected",
                &format!("group skill '{}' shape violation: {e}", skill.name),
                "daemon",
            );
            return Err(DaemonError::Io(format!(
                "proposed group skill '{}' violates shape: {e}",
                skill.name
            )));
        }
        if self.skill_store.load(&skill.name).is_some() {
            let _ = self.store.add_audit(
                "daemon.propose.rejected",
                &format!(
                    "group skill '{}' already exists; refusing to overwrite",
                    skill.name
                ),
                "daemon",
            );
            return Err(DaemonError::Io(format!(
                "proposed group skill '{}' already exists; refusing to overwrite",
                skill.name
            )));
        }
        let parents: Vec<String> = skill.relations.iter().map(|r| r.skill.clone()).collect();
        let path = self.skill_store.save(&skill).map_err(DaemonError::Io)?;
        self.store.upsert_skill(&skill.to_row())?;
        self.store.add_audit(
            "daemon.propose",
            &format!(
                "{} v{} model={} parents=[{}] at {}",
                skill.name,
                skill.version,
                self.provider.id(),
                parents.join(", "),
                path.display()
            ),
            "daemon",
        )?;
        Ok(skill.name)
    }

    /// Merge near-duplicate skills (cosine >= 0.98), keeping the higher-confidence one.
    fn compact_skills(&self) -> Result<Vec<String>, DaemonError> {
        let skills = self.skill_store.discover();
        let mut merged = Vec::new();
        // 合并会改动 skill_store,不能边迭代边依赖初始快照:已删除的名字必须跳过,
        // 否则 (i,j) 对里一个早已被删的 skill 会被再次当作 winner 保存(撤销删除),
        // 或被重复合并(丢 usage_count)。
        let mut removed: std::collections::HashSet<String> = std::collections::HashSet::new();
        for i in 0..skills.len() {
            if removed.contains(&skills[i].name) {
                continue;
            }
            for j in (i + 1)..skills.len() {
                let (a, b) = (&skills[i], &skills[j]);
                if removed.contains(&a.name) || removed.contains(&b.name) {
                    continue;
                }
                let sim = match (&a.embedding, &b.embedding) {
                    (Some(x), Some(y)) => rc_skill::cosine(x, y),
                    _ => 0.0,
                };
                if sim >= 0.98 && a.name != b.name {
                    let keep = if a.confidence >= b.confidence { a } else { b };
                    let drop = if a.confidence >= b.confidence { b } else { a };
                    let mut winner = keep.clone();
                    winner.usage_count += drop.usage_count;
                    winner.version = winner.version.max(drop.version);
                    self.skill_store.save(&winner).map_err(DaemonError::Io)?;
                    self.skill_store
                        .remove(&drop.name)
                        .map_err(DaemonError::Io)?;
                    self.store.delete_skill(&drop.name)?;
                    self.store.add_audit(
                        "daemon.merge",
                        &format!("{} <- {}", winner.name, drop.name),
                        "daemon",
                    )?;
                    removed.insert(drop.name.clone());
                    merged.push(format!("{} <- {}", winner.name, drop.name));
                }
            }
        }
        Ok(merged)
    }

    /// Promote high-usage project skills to user scope so they outlive one repo.
    fn promote_project_skills(&self) -> Result<Vec<String>, DaemonError> {
        let mut promoted = Vec::new();
        for mut skill in self.skill_store.discover() {
            if skill.scope != "project" || skill.usage_count < 5 {
                continue;
            }
            skill.scope = "user".into();
            let path = self.skill_store.save(&skill).map_err(DaemonError::Io)?;
            self.store.upsert_skill(&skill.to_row())?;
            self.store.add_audit(
                "daemon.promote",
                &format!("{} -> user at {}", skill.name, path.display()),
                "daemon",
            )?;
            promoted.push(skill.name);
        }
        Ok(promoted)
    }

    /// Decay skills never used in `stale_days` days.
    fn decay_stale(&self) -> Result<Vec<String>, DaemonError> {
        let skills = self.skill_store.discover();
        let now = Utc::now();
        let mut decayed = Vec::new();
        for mut skill in skills {
            if skill.origin == "seed" || skill.scope == "system" {
                continue;
            }
            let stale = match &skill.last_used {
                Some(ts) => DateTime::parse_from_rfc3339(ts)
                    .map(|t| {
                        now.signed_duration_since(t.with_timezone(&Utc)).num_days()
                            > self.config.stale_days
                    })
                    .unwrap_or(false),
                None => skill.usage_count == 0,
            };
            if stale {
                if skill.confidence <= 0.35 {
                    self.skill_store
                        .remove(&skill.name)
                        .map_err(DaemonError::Io)?;
                    self.store.delete_skill(&skill.name)?;
                    self.store.add_audit(
                        "daemon.decay",
                        &format!("removed {}", skill.name),
                        "daemon",
                    )?;
                } else {
                    skill.confidence = (skill.confidence * 0.9).max(0.2);
                    self.skill_store.save(&skill).map_err(DaemonError::Io)?;
                    self.store.upsert_skill(&skill.to_row())?;
                    self.store.add_audit(
                        "daemon.decay",
                        &format!("lowered confidence for {}", skill.name),
                        "daemon",
                    )?;
                }
                decayed.push(skill.name);
            }
        }
        Ok(decayed)
    }
}

pub struct LockGuard {
    path: PathBuf,
    _file: std::fs::File,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn expand_tilde(path: &Path) -> PathBuf {
    let s = path.to_string_lossy().to_string();
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rc_pro::mock::MockProvider;
    use rc_pro::provider::ProviderConfig;
    use rc_skill::Skill;
    use rc_state::{ExperienceRecord, NavOutcome, NavigationRecord};

    fn mock_provider(script: serde_json::Value) -> Arc<dyn Provider> {
        let cfg = ProviderConfig {
            kind: "mock".into(),
            base_url: String::new(),
            model: "mock-daemon".into(),
            api_key: None,
            api_key_env: None,
            embedding_model: None,
            headers: Default::default(),
            extra: serde_json::json!({"script": script, "auto_advance": false}),
        };
        Arc::new(MockProvider::new(cfg, "mock-daemon".into()))
    }

    fn experience(session: &str, task: &str) -> ExperienceRecord {
        ExperienceRecord {
            id: String::new(),
            session_id: session.into(),
            task_signature: task.into(),
            category_guess: "engineering.refactor".into(),
            approach: vec!["read first".into(), "small commits".into()],
            worked: vec!["refactor worked".into()],
            failed: vec![],
            commands: vec![],
            tools_used: vec!["read_file".into(), "apply_patch".into()],
            outcome: "success".into(),
            skills_used: vec![],
            created_at: String::new(),
        }
    }

    #[tokio::test]
    async fn scan_abstracts_cross_session_cluster() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        let tasks = [
            "refactor rust service",
            "refactor rust service",
            "refactor rust service",
        ];
        for (i, task) in tasks.iter().enumerate() {
            store
                .save_experience(experience(&format!("s{i}"), task))
                .unwrap();
        }
        let skill_md = "---\nname: rust-refactor\ndescription: shared rust refactor method\ncategory: engineering.refactor\nrelations:\n  - kind: composes\n    skill: refactor-base\n---\nRead, plan, small commits.";
        let provider = mock_provider(serde_json::json!([
            {"type": "text", "text": skill_md},
            {"type": "done", "stop_reason": "end_turn"}
        ]));
        let daemon = PatternDaemon::new(
            provider,
            store,
            SkillStore::new(dir.path()),
            DaemonConfig {
                interval_minutes: 15,
                min_cluster: 3,
                similarity_threshold: 0.2,
                coverage_factor: 1.0,
                lock_path: dir
                    .path()
                    .join("daemon.lock")
                    .to_string_lossy()
                    .into_owned(),
                ..Default::default()
            },
        );
        let report = daemon.scan().await.unwrap();
        assert_eq!(report.clusters_found, 1);
        assert!(report
            .skills_proposed
            .contains(&"rust-refactor".to_string()));
        let saved = daemon.skill_store.discover();
        assert!(saved.iter().any(|s| s.name == "rust-refactor"));
        let high = daemon.skill_store.load("rust-refactor").unwrap();
        assert!(high.auto);
        assert!(high
            .relations
            .iter()
            .any(|r| { r.kind == rc_skill::RelationKind::Composes && r.skill == "refactor-base" }));
        let router = SkillRouter::new(daemon.skill_store.discover());
        let top = router.select_keyword("refactor rust service again", 3);
        assert!(top.iter().any(|s| s.name == "rust-refactor"));
    }

    #[tokio::test]
    async fn scan_rejects_group_skill_colliding_with_existing_skill() {
        // D1:propose_group_skill 必须有名称碰撞守卫 —— 模型生成的组 skill 名字撞上
        // 现有 skill → 拒绝(记审计),现有 skill 不被覆盖。auto_approve=true 默认直写,
        // 若无守卫会静默覆盖用户文件。
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        let tasks = [
            "refactor rust service",
            "refactor rust service",
            "refactor rust service",
        ];
        for (i, task) in tasks.iter().enumerate() {
            store
                .save_experience(experience(&format!("s{i}"), task))
                .unwrap();
        }
        let skill_store = SkillStore::new(dir.path());
        // 现有用户 skill,名字与模型将要生成的名字一致。
        skill_store
            .save(&skill_for_daemon("collision-target", "collision target", "unrelated", "existing body"))
            .unwrap();
        // 模型返回的名字撞上现有 skill。
        let provider = mock_provider(serde_json::json!([
            {"type": "text", "text": "---\nname: collision-target\ndescription: group abstraction\ncategory: unrelated\n---\nGroup body from model."},
            {"type": "done", "stop_reason": "end_turn"}
        ]));
        let daemon = PatternDaemon::new(
            provider,
            store,
            skill_store.clone(),
            DaemonConfig {
                interval_minutes: 15,
                min_cluster: 3,
                similarity_threshold: 0.2,
                coverage_factor: 1.0,
                lock_path: dir
                    .path()
                    .join("daemon.lock")
                    .to_string_lossy()
                    .into_owned(),
                ..Default::default()
            },
        );
        let report = daemon.scan().await.unwrap();
        // 碰撞 → 组 skill 被拒绝,不进报告。
        assert!(
            report.skills_proposed.is_empty(),
            "colliding group skill must be rejected"
        );
        // 现有 skill 未被覆盖。
        let existing = skill_store.load("collision-target").unwrap();
        assert_eq!(existing.body, "existing body", "existing skill must stay untouched");
        assert_eq!(existing.version, 1);
        // 拒绝事件已审计。
        let audit = daemon.store.list_audit(20).unwrap();
        assert!(
            audit.iter().any(|a| a.action == "daemon.propose.rejected"),
            "rejection must be audited"
        );
    }

    #[tokio::test]
    async fn scan_promotes_high_usage_project_skill() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        for (i, task) in [
            "refactor rust service",
            "refactor rust service",
            "refactor rust service",
        ]
        .iter()
        .enumerate()
        {
            store
                .save_experience(experience(&format!("s{i}"), task))
                .unwrap();
        }
        let skill_store = SkillStore::new(dir.path());
        let mut project = Skill {
            name: "project-favorite".into(),
            description: "favorite project method".into(),
            short_description: None,
            category: "engineering".into(),
            path: std::path::PathBuf::new(),
            body: "body".into(),
            relations: vec![],
            triggers: vec!["project".into()],
            tags: vec![],
            version: 1,
            confidence: 0.9,
            usage_count: 5,
            success_rate: 0.9,
            last_used: None,
            auto: false,
            origin: "manual".into(),
            origin_url: None,
            scope: "project".into(),
            allow_implicit: true,
            embedding: None,
        };
        project.scope = "project".into();
        project.usage_count = 5;
        project.confidence = 0.9;
        skill_store.save(&project).unwrap();
        store.upsert_skill(&project.to_row()).unwrap();
        let provider = mock_provider(serde_json::json!([
            {"type": "done", "stop_reason": "end_turn"}
        ]));
        let daemon = PatternDaemon::new(
            provider,
            store,
            skill_store,
            DaemonConfig {
                interval_minutes: 15,
                min_cluster: 3,
                similarity_threshold: 0.99,
                coverage_factor: 1.0,
                lock_path: dir
                    .path()
                    .join("daemon.lock")
                    .to_string_lossy()
                    .into_owned(),
                ..Default::default()
            },
        );
        let report = daemon.scan().await.unwrap();
        assert!(report
            .skills_promoted
            .contains(&"project-favorite".to_string()));
        assert_eq!(
            daemon.skill_store.load("project-favorite").unwrap().scope,
            "user"
        );
    }

    #[tokio::test]
    async fn scan_rejects_too_few_experiences() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        store
            .save_experience(experience("s1", "refactor rust module"))
            .unwrap();
        let provider = mock_provider(serde_json::json!([
            {"type": "done", "stop_reason": "end_turn"}
        ]));
        let daemon = PatternDaemon::new(
            provider,
            store,
            SkillStore::new(dir.path()),
            DaemonConfig {
                interval_minutes: 15,
                min_cluster: 3,
                similarity_threshold: 0.2,
                coverage_factor: 1.0,
                lock_path: dir
                    .path()
                    .join("daemon.lock")
                    .to_string_lossy()
                    .into_owned(),
                ..Default::default()
            },
        );
        let report = daemon.scan().await.unwrap();
        assert_eq!(report.clusters_found, 0);
        assert!(report.skills_proposed.is_empty());
    }

    fn skill_for_daemon(name: &str, desc: &str, category: &str, body: &str) -> Skill {
        Skill {
            name: name.into(),
            description: desc.into(),
            short_description: None,
            category: category.into(),
            path: std::path::PathBuf::new(),
            body: body.into(),
            relations: vec![],
            triggers: vec![name.into()],
            tags: vec![],
            version: 1,
            confidence: 0.8,
            usage_count: 0,
            success_rate: 0.0,
            last_used: None,
            auto: true,
            origin: "evolved".into(),
            origin_url: None,
            scope: "user".into(),
            allow_implicit: true,
            embedding: None,
        }
    }

    fn nav_rec(root: &str, path_json: &str, outcome: NavOutcome) -> NavigationRecord {
        NavigationRecord {
            id: String::new(),
            task_signature: "t".into(),
            root: root.into(),
            path_json: path_json.into(),
            outcome,
            model: "mock".into(),
            created_at: String::new(),
        }
    }

    /// 每次 stream 调用按 turn 返回脚本序列的下一项(auto_advance=false 时),
    /// 让同一 provider 为 digest / evolve 的不同调用返回不同变体。
    fn sequence_provider(scripts: serde_json::Value) -> Arc<dyn Provider> {
        let cfg = ProviderConfig {
            kind: "mock".into(),
            base_url: String::new(),
            model: "mock-daemon".into(),
            api_key: None,
            api_key_env: None,
            embedding_model: None,
            headers: Default::default(),
            extra: serde_json::json!({"script_sequence": scripts, "auto_advance": false}),
        };
        Arc::new(MockProvider::new(cfg, "mock-daemon".into()))
    }

    fn daemon_with(
        dir: &tempfile::TempDir,
        store: Store,
        skill_store: SkillStore,
        provider: Arc<dyn Provider>,
    ) -> PatternDaemon {
        PatternDaemon::new(
            provider,
            store,
            skill_store,
            DaemonConfig {
                interval_minutes: 15,
                min_cluster: 3,
                similarity_threshold: 0.2,
                coverage_factor: 1.0,
                lock_path: dir
                    .path()
                    .join("daemon.lock")
                    .to_string_lossy()
                    .into_owned(),
                ..Default::default()
            },
        )
    }

    #[tokio::test]
    async fn scan_stages_navigation_feedback_candidates() {
        // digest_navigation 被 daemon pass 调用:react(索引)3 次 WrongBranch → 菜单改写
        // 候选(此 mock 返回带正文的变体 → 违反"索引无正文"形状 → 拒绝,pass 正常继续;
        // description-only 变体可落盘,见 scan_applies_index_menu_rewrite_description);
        // react.performance(叶子)2 次 LeafTooThin → 叶子补充 → auto_approve 直接应用。
        // react 回溯率 3/11 ≤ 0.5 → 不演化。
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        for _ in 0..8 {
            store
                .record_navigation(&nav_rec("react", "[]", NavOutcome::Success))
                .unwrap();
        }
        for _ in 0..3 {
            store
                .record_navigation(&nav_rec("react", "[]", NavOutcome::WrongBranch))
                .unwrap();
        }
        for _ in 0..2 {
            store
                .record_navigation(
                    &nav_rec("react", r#"["react","react.performance"]"#, NavOutcome::LeafTooThin),
                )
                .unwrap();
        }
        let skill_store = SkillStore::new(dir.path());
        skill_store
            .save(&skill_for_daemon("react", "react index", "frontend", ""))
            .unwrap();
        skill_store
            .save(&skill_for_daemon("react.performance", "react perf", "frontend.react", "thin body"))
            .unwrap();

        // 调用顺序(仅 digest,无演化):react 菜单改写 → react.performance 叶子补充。
        let provider = sequence_provider(serde_json::json!([
            [{"type":"text","text":"---\nname: react\ncategory: frontend\n---\n# clearer menu\n"}, {"type":"done","stop_reason":"end_turn"}],
            [{"type":"text","text":"---\nname: react.performance\ncategory: frontend.react\n---\n# expanded leaf\n"}, {"type":"done","stop_reason":"end_turn"}],
        ]));
        let daemon = daemon_with(&dir, store, skill_store, provider);
        let report = daemon.scan().await.unwrap();

        // digest_navigation 确实被调用:叶子补充被成功应用。
        assert_eq!(report.leaf_backfills, vec!["react.performance".to_string()]);
        // 索引菜单改写因形状被拒(索引不允许正文)→ 不进报告,pass 不 panic、继续。
        assert!(report.menu_rewrites.is_empty(), "index menu rewrite must be shape-rejected");
        assert!(report.skills_evolved.is_empty());
        // 叶子补充已落盘。
        assert!(daemon
            .skill_store
            .load("react.performance")
            .unwrap()
            .body
            .contains("expanded"));
    }

    #[tokio::test]
    async fn scan_applies_darwinian_organism() {
        // 有导航记录的 leaf skill python(回溯率 2/3 ≈ 0.67 > 0.5)→ evolve_skill 生成
        // 变体 → stage_nav_rewrite(auto_approve)→ apply_organism 落盘。digest 侧 python
        // WrongBranch 仅 2 次(<3)不触发改写,单次 mutate 只服务演化。
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        for _ in 0..2 {
            store
                .record_navigation(&nav_rec("python", "[]", NavOutcome::WrongBranch))
                .unwrap();
        }
        store
            .record_navigation(&nav_rec("python", "[]", NavOutcome::Success))
            .unwrap();
        let skill_store = SkillStore::new(dir.path());
        skill_store
            .save(&skill_for_daemon("python", "python method", "backend", "python body"))
            .unwrap();

        let provider = sequence_provider(serde_json::json!([
            [{"type":"text","text":"---\nname: python\ncategory: backend\n---\n# evolved python body\n"}, {"type":"done","stop_reason":"end_turn"}],
        ]));
        let daemon = daemon_with(&dir, store, skill_store, provider);
        let report = daemon.scan().await.unwrap();

        assert_eq!(report.skills_evolved, vec!["python".to_string()]);
        assert!(report.menu_rewrites.is_empty());
        assert!(report.leaf_backfills.is_empty());
        // 变体已落盘 + 审计(darwinian.adopt)。
        let on_disk = daemon.skill_store.load("python").unwrap();
        assert!(on_disk.body.contains("evolved python body"), "body: {}", on_disk.body);
        let audit = daemon.store.list_audit(10).unwrap();
        assert!(audit.iter().any(|a| a.action == "darwinian.adopt"), "adopt audit expected");
    }

    #[tokio::test]
    async fn scan_stages_darwinian_organism_when_conservative() {
        // auto_approve=false:darwinian 变体不能直接覆盖 user skill → stage 进 .review
        // 隔离区等用户批准(与菜单改写的保守策略一致),活跃副本保持不动。
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        for _ in 0..2 {
            store
                .record_navigation(&nav_rec("python", "[]", NavOutcome::WrongBranch))
                .unwrap();
        }
        store
            .record_navigation(&nav_rec("python", "[]", NavOutcome::Success))
            .unwrap();
        let skill_store = SkillStore::new(dir.path());
        skill_store
            .save(&skill_for_daemon("python", "python method", "backend", "python body"))
            .unwrap();

        let provider = sequence_provider(serde_json::json!([
            [{"type":"text","text":"---\nname: python\ncategory: backend\n---\n# evolved python body\n"}, {"type":"done","stop_reason":"end_turn"}],
        ]));
        let daemon = PatternDaemon::new(
            provider,
            store,
            skill_store,
            DaemonConfig {
                interval_minutes: 15,
                min_cluster: 3,
                similarity_threshold: 0.2,
                coverage_factor: 1.0,
                auto_approve: false,
                lock_path: dir
                    .path()
                    .join("daemon.lock")
                    .to_string_lossy()
                    .into_owned(),
                ..Default::default()
            },
        );
        let report = daemon.scan().await.unwrap();

        assert_eq!(report.skills_evolved, vec!["python".to_string()]);
        // 活跃 user 副本未被改写(还在正常 discover)。
        let active = daemon.skill_store.load("python").unwrap();
        assert!(active.body.contains("python body"), "active copy must stay untouched: {}", active.body);
        // 变体已 stage 进 .review 隔离区。
        let review = daemon.skill_store.discover_review();
        assert_eq!(review.len(), 1, "one staged variant expected");
        assert!(review[0].body.contains("evolved python body"), "body: {}", review[0].body);
        assert_eq!(review[0].scope, "review");
        assert_eq!(review[0].version, 2, "staged variant bumps version");
        // 审计是 staged 事件,而非直接 adopt。
        let audit = daemon.store.list_audit(10).unwrap();
        assert!(audit.iter().any(|a| a.action == "daemon.nav_rewrite.staged"), "staged audit expected");
    }

    #[tokio::test]
    async fn scan_applies_index_menu_rewrite_description() {
        // Fix 3:WrongBranch 落在索引 react(有子 react.performance)。digest 用菜单文本作
        // mutate 上下文、要求 description-only(空正文)变体 → 形状通过(索引无正文)→
        // 菜单改写真正落盘。回溯率 3/6 = 0.5 ≤ 0.5 → darwinian 不演化,单次 mutate 只服务 digest。
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        for _ in 0..3 {
            store
                .record_navigation(&nav_rec("react", "[]", NavOutcome::WrongBranch))
                .unwrap();
        }
        for _ in 0..3 {
            store
                .record_navigation(&nav_rec("react", "[]", NavOutcome::Success))
                .unwrap();
        }
        let skill_store = SkillStore::new(dir.path());
        skill_store
            .save(&skill_for_daemon("react", "react index", "frontend", ""))
            .unwrap();
        skill_store
            .save(&skill_for_daemon("react.performance", "react perf", "frontend.react", "thin body"))
            .unwrap();

        // description-only 变体(frontmatter 带更好 description,正文空)。
        let provider = sequence_provider(serde_json::json!([
            [{"type":"text","text":"---\nname: react\ncategory: frontend\ndescription: clearer react direction\n---\n"}, {"type":"done","stop_reason":"end_turn"}],
        ]));
        let daemon = daemon_with(&dir, store, skill_store, provider);
        let report = daemon.scan().await.unwrap();

        assert_eq!(report.menu_rewrites, vec!["react".to_string()]);
        // 索引描述已更新,正文保持为空(形状不变)。
        let on_disk = daemon.skill_store.load("react").unwrap();
        assert!(on_disk.description.contains("clearer react direction"), "desc: {}", on_disk.description);
        assert_eq!(on_disk.body.trim(), "", "index body must stay empty");
        assert!(report.skills_evolved.is_empty(), "backtrack 0.5 不得触发 darwinian 演化");
    }
}
