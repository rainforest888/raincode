use crate::model::{Skill, SkillSummary};
use rc_pro::Provider;
use std::collections::HashSet;

pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.is_empty() || b.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    let mut dot = 0.0f64;
    let mut na = 0.0f64;
    let mut nb = 0.0f64;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += f64::from(*x) * f64::from(*y);
        na += f64::from(*x) * f64::from(*x);
        nb += f64::from(*y) * f64::from(*y);
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    (dot / (na.sqrt() * nb.sqrt())) as f32
}

fn tokens(text: &str) -> HashSet<String> {
    text.split(|c: char| !c.is_alphanumeric() && c != '.' && c != '_')
        .map(str::to_lowercase)
        .filter(|t| t.len() > 1)
        .collect()
}

fn keyword_score(task: &str, skill: &Skill) -> f32 {
    let task_tokens = tokens(task);
    if task_tokens.is_empty() {
        return 0.0;
    }
    let mut hits = 0.0;
    for trigger in &skill.triggers {
        if task.to_lowercase().contains(&trigger.to_lowercase()) {
            hits += 2.0;
        }
    }
    let text = format!(
        "{} {} {}",
        skill.name,
        skill.description,
        skill.tags.join(" ")
    );
    let skill_tokens = tokens(&text);
    let overlap = task_tokens.intersection(&skill_tokens).count() as f32;
    hits + overlap / (task_tokens.len() as f32).max(1.0)
}

pub struct SkillRouter {
    skills: Vec<Skill>,
}

/// 联网选择结果:summary 携带 leaf 状态,调用方据此决定直接加载(叶子)或进导航(索引)。
pub struct SkillSelection {
    pub summary: SkillSummary,
    leaf: bool,
}

impl SkillSelection {
    pub fn is_leaf(&self) -> bool {
        self.leaf
    }
}

impl SkillRouter {
    pub fn new(skills: Vec<Skill>) -> Self {
        Self { skills }
    }


    pub fn all(&self) -> &Vec<Skill> {
        &self.skills
    }

    /// Pure keyword + stored-embedding selection (offline, deterministic).
    pub fn select_keyword(&self, task: &str, k: usize) -> Vec<SkillSummary> {
        let mut scored: Vec<(f32, &Skill)> = self
            .skills
            .iter()
            .filter(|s| s.allow_implicit)
            .map(|s| (keyword_score(task, s), s))
            .collect();
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored
            .into_iter()
            .take(k)
            .map(|(score, s)| SkillSummary {
                name: s.name.clone(),
                description: s.description.clone(),
                category: s.category.clone(),
                score,
                is_leaf: false,
            })
            .collect()
    }

    /// 联网选择:命中后标注 is_leaf,调用方据此决定直接加载(叶子)或进导航(索引)。
    pub fn select_networked(
        &self,
        network: &crate::network::SkillNetwork,
        task: &str,
        k: usize,
        provider: Option<&dyn Provider>,
    ) -> Vec<SkillSelection> {
        let base = if provider.is_some() {
            // 复用 embedding 选择(需 async);这里用纯函数路径,embedding 由调用方注入。
            self.select_keyword(task, k)
        } else {
            self.select_keyword(task, k)
        };
        base.into_iter()
            .map(|s| {
                let leaf = network
                    .nodes
                    .iter()
                    .find(|n| n.skill.name == s.name)
                    .map(|n| n.is_leaf)
                    .unwrap_or(true);
                SkillSelection { summary: s, leaf }
            })
            .collect()
    }

    /// Embedding-augmented selection. Falls back to keywords when the
    /// provider or skill lacks an embedding.
    pub async fn select(
        &self,
        task: &str,
        k: usize,
        provider: Option<&dyn Provider>,
        _provider_id: &str,
        _model: &str,
    ) -> Vec<SkillSummary> {
        let mut embedding = None;
        if let Some(p) = provider {
            if let Ok(mut vecs) = p.embed(vec![task.to_string()]).await {
                embedding = vecs.pop();
            }
        }
        match embedding {
            Some(task_emb) => self.select_with_embedding(task, &task_emb, k),
            None => self.select_keyword(task, k),
        }
    }

    pub fn select_with_embedding(
        &self,
        task: &str,
        task_emb: &[f32],
        k: usize,
    ) -> Vec<SkillSummary> {
        let mut scored: Vec<(f32, &Skill)> = self
            .skills
            .iter()
            .filter(|s| s.allow_implicit)
            .map(|s| {
                let emb_score = s
                    .embedding
                    .as_deref()
                    .map(|e| cosine(task_emb, e))
                    .unwrap_or(0.0);
                let kw_score = keyword_score(task, s);
                let combined = emb_score * 0.7 + (kw_score / 6.0).min(0.3);
                (combined, s)
            })
            .collect();
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored
            .into_iter()
            .take(k)
            .map(|(score, s)| SkillSummary {
                name: s.name.clone(),
                description: s.description.clone(),
                category: s.category.clone(),
                score,
                is_leaf: false,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::SkillNetwork;
    use crate::store::SkillStore;

    fn skill(name: &str, desc: &str, triggers: Vec<&str>, emb: Option<Vec<f32>>) -> Skill {
        Skill {
            name: name.into(),
            description: desc.into(),
            short_description: None,
            category: "test".into(),
            path: std::path::PathBuf::new(),
            body: "body".into(),
            relations: vec![],
            triggers: triggers.into_iter().map(String::from).collect(),
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
            embedding: emb,
        }
    }

    #[test]
    fn keyword_router_ranks_by_triggers() {
        let router = SkillRouter::new(vec![
            skill("pytest-fix", "fix pytest failures", vec!["pytest"], None),
            skill("docker-debug", "debug docker compose", vec!["docker"], None),
        ]);
        let top = router.select_keyword("my pytest tests are flaky", 1);
        assert_eq!(top[0].name, "pytest-fix");
        assert!(top[0].score > 0.0);
    }

    #[test]
    fn embedding_router_uses_cosine() {
        let pytest_emb = vec![1.0f32, 0.0, 0.0];
        let docker_emb = vec![0.0f32, 1.0, 0.0];
        let task_emb = vec![0.9f32, 0.1, 0.0];
        let router = SkillRouter::new(vec![
            skill(
                "pytest-fix",
                "fix pytest failures",
                vec![],
                Some(pytest_emb),
            ),
            skill(
                "docker-debug",
                "debug docker compose",
                vec![],
                Some(docker_emb),
            ),
        ]);
        let top = router.select_with_embedding("run pytest", &task_emb, 1);
        assert_eq!(top[0].name, "pytest-fix");
    }

    #[test]
    fn networked_select_marks_leaf_vs_index() {
        // 索引 react + 叶子 react.performance;select_networked("react ...") 应返回 react 且 is_leaf=false。
        let dir = tempfile::tempdir().unwrap();
        let store = SkillStore::new(dir.path());
        let mut index = skill("react", "react framework", vec![], None);
        index.category = "frontend".into();
        let mut perf = skill("react.performance", "react performance", vec![], None);
        perf.category = "frontend.react".into();
        store.save(&index).unwrap();
        store.save(&perf).unwrap();
        let network = SkillNetwork::from_store(&store);
        let router = SkillRouter::new(store.discover());
        let selections = router.select_networked(&network, "react performance tuning", 2, None);
        assert_eq!(selections.len(), 2);
        let react = selections
            .iter()
            .find(|s| s.summary.name == "react")
            .expect("index react selected");
        assert!(!react.is_leaf(), "index react must be marked as index (not leaf)");
        let perf_sel = selections
            .iter()
            .find(|s| s.summary.name == "react.performance")
            .expect("leaf react.performance selected");
        assert!(perf_sel.is_leaf(), "leaf react.performance must be marked as leaf");
    }
}
