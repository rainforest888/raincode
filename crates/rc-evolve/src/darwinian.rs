//! darwinian 演化循环(参考 hermes darwinian-evolver 精神):organism/mutator/fitness。
//! fitness 用真实 navigation_log 数据,不靠模型自夸。
use futures::StreamExt;
use rc_pro::canonical::{CanonicalMessage, CanonicalRequest, ProvEvent};
use rc_pro::Provider;
use rc_skill::{Skill, SkillNetwork};
use rc_state::{NavOutcome, Store};

#[derive(Debug, Clone)]
pub struct FitnessScore {
    pub hit_rate: f32,
    pub backtrack_rate: f32,
}

/// 被采纳的变体:skill_name + 完整 SKILL.md 变体文本。由 apply_organism 落盘。
#[derive(Debug, Clone, PartialEq)]
pub struct Organism {
    pub skill_name: String,
    pub variant: String,
}

/// 从 navigation_log 算 skill 的命中率/回溯率(root 匹配该 skill 的记录)。
pub async fn fitness(store: &Store, skill_name: &str) -> FitnessScore {
    let recs = store.list_navigation(5000).unwrap_or_default();
    let relevant: Vec<_> = recs.iter().filter(|r| r.root == skill_name).collect();
    let total = relevant.len() as f32;
    if total == 0.0 {
        return FitnessScore { hit_rate: 0.0, backtrack_rate: 0.0 };
    }
    let success = relevant.iter().filter(|r| r.outcome == NavOutcome::Success).count() as f32;
    let backtrack = relevant.iter().filter(|r| r.outcome == NavOutcome::WrongBranch).count() as f32;
    FitnessScore { hit_rate: success / total, backtrack_rate: backtrack / total }
}

/// mutator:调模型生成 SKILL.md 变体(补充/精简/纠错)。boundary 是约束提示。
pub async fn mutate(
    provider: &dyn Provider,
    skill_name: &str,
    current: &str,
    boundary: &str,
) -> Result<String, String> {
    let prompt = format!(
        "Improve this SKILL.md for '{skill_name}'. Keep frontmatter valid.\n\
         Constraint: {boundary}\n\
         Current:\n{current}\n\
         Return ONLY the improved SKILL.md (frontmatter + body)."
    );
    let req = CanonicalRequest {
        model: provider.id().to_string(),
        messages: vec![
            CanonicalMessage::system("You are a skill editor. Improve the skill while keeping it focused."),
            CanonicalMessage::user(prompt),
        ],
        tools: vec![],
        temperature: Some(0.3),
        max_tokens: Some(2000),
        stream: true,
        extra: serde_json::json!({}),
    };
    let mut stream = provider.stream(req).await.map_err(|e| e.to_string())?;
    let mut out = String::new();
    while let Some(ev) = stream.next().await {
        if let Ok(ProvEvent::Delta { text }) = ev {
            out.push_str(&text);
        }
    }
    if out.trim().is_empty() {
        Err("mutate returned empty".to_string())
    } else {
        Ok(out)
    }
}

/// 采纳门槛:回溯率必须严格高于该比例(过半数导航走错)才调用模型变异。
/// 0.5 = 清晰多数。低于此门槛的波动(如 0.31)不能作为真实退化的证据,避免
/// 单次幻觉 LLM 改写直接覆盖可用 skill(变异是全额付费调用 + 无差别覆盖)。
pub const ADOPT_THRESHOLD: f32 = 0.5;

/// 演化一个 skill:回溯率 > ADOPT_THRESHOLD(0.5) 才调用模型生成变体;否则保留当前(不调用模型)。
/// 返回 `Ok(None)` = 保留当前;`Ok(Some(Organism))` = 生成待采纳的变体,
/// 调用方用 `apply_organism` 落盘。变体本身会校验是可解析的 SKILL.md。
pub async fn evolve_skill(
    provider: &dyn Provider,
    store: &Store,
    skill_name: &str,
    current: &str,
) -> Result<Option<Organism>, String> {
    let current_f = fitness(store, skill_name).await;
    // 无数据时不盲目演化:没有使用记录就保留当前(防退化)。
    if current_f.hit_rate == 0.0 && current_f.backtrack_rate == 0.0 {
        return Ok(None);
    }
    // 回溯率 ≤ ADOPT_THRESHOLD(0.5):走错未过半数,当前 skill 表现够好,保留现状,
    // 不调用模型(避免为丢弃的变体付全额调用费)。
    if current_f.backtrack_rate <= ADOPT_THRESHOLD {
        return Ok(None);
    }
    let boundary = format!(
        "当前命中率 {:.2},回溯率 {:.2}。重点降低回溯率:菜单/正文应更明确地指向正确分支。",
        current_f.hit_rate, current_f.backtrack_rate
    );
    let variant = mutate(provider, skill_name, current, &boundary).await?;
    // 变体必须是合法 SKILL.md(frontmatter + body),防模型输出垃圾。
    rc_skill::parse_frontmatter(&variant)
        .map_err(|e| format!("mutate returned invalid SKILL.md: {e}"))?;
    Ok(Some(Organism { skill_name: skill_name.to_string(), variant }))
}

/// 采纳一个 organism:把变体 SKILL.md 写盘 + 更新 DB skills 行 + 写审计日志(§4c 采纳→写审计)。
/// 变体必须与现有 skill 同名,并通过 enforce_skill_shape(索引无正文/叶子有正文)。
pub async fn apply_organism(
    store: &Store,
    skill_store: &rc_skill::SkillStore,
    organism: &Organism,
) -> Result<bool, String> {
    let existing = skill_store
        .load(&organism.skill_name)
        .ok_or_else(|| format!("skill '{}' not found in skill store", organism.skill_name))?;
    let (fm, body) = rc_skill::parse_frontmatter(&organism.variant)
        .map_err(|e| format!("variant is not a valid SKILL.md: {e}"))?;
    if fm.name != existing.name {
        return Err(format!(
            "variant name '{}' does not match skill '{}'",
            fm.name, existing.name
        ));
    }
    let mut new_skill = Skill::from_frontmatter(fm, body, existing.path.clone());
    // 保持网络拓扑稳定:category/scope 沿用现有,只采纳正文/关系/触发/描述等演化字段。
    new_skill.category = existing.category.clone();
    new_skill.path = existing.path.clone();
    new_skill.scope = existing.scope.clone();
    new_skill.auto = true;
    new_skill.origin = "darwinian".into();
    new_skill.version = existing.version.saturating_add(1);
    // 遥测保留:采纳变体绝不重置使用数据/置信度/embedding。否则每次采纳都清零
    // usage_count,daemon 的 retention gate(usage_count < 5)与 usage 合并(compact_skills
    // 累加 usage_count)都会因清零而失效,排名数据在演化中被反复擦除。
    new_skill.usage_count = existing.usage_count;
    new_skill.success_rate = existing.success_rate;
    new_skill.last_used = existing.last_used.clone();
    new_skill.confidence = existing.confidence;
    new_skill.embedding = existing.embedding.clone();
    // 同类静默重置:allow_implicit(false → variant 省略字段时回 true)与 origin_url 一并沿用。
    new_skill.allow_implicit = existing.allow_implicit;
    new_skill.origin_url = existing.origin_url.clone();

    // 形状把关:索引(有子)正文必须为空,叶子必须完整 —— 变体违规直接拒绝、不落盘。
    let network = SkillNetwork::from_store(skill_store);
    rc_skill::enforce_skill_shape(&network, &new_skill)
        .map_err(|e| format!("variant violates skill shape: {e}"))?;

    let path = skill_store.save(&new_skill).map_err(|e| e.to_string())?;
    new_skill.path = path;
    store.upsert_skill(&new_skill.to_row()).map_err(|e| e.to_string())?;
    store
        .add_audit(
            "darwinian.adopt",
            &format!("adopted {} v{}", organism.skill_name, new_skill.version),
            "darwinian",
        )
        .map_err(|e| e.to_string())?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rc_pro::mock::MockProvider;
    use rc_pro::ProviderConfig;
    use rc_state::{NavOutcome, NavigationRecord, Store};

    fn nav_rec(root: &str, outcome: NavOutcome) -> NavigationRecord {
        NavigationRecord {
            id: String::new(),
            task_signature: "t".into(),
            root: root.into(),
            path_json: "[]".into(),
            outcome,
            model: "mock".into(),
            created_at: String::new(),
        }
    }

    fn mock_provider(variant: &str) -> MockProvider {
        let cfg = ProviderConfig {
            kind: "mock".into(),
            base_url: String::new(),
            model: "mock-1".into(),
            api_key: None,
            api_key_env: None,
            embedding_model: None,
            headers: Default::default(),
            extra: serde_json::json!({
                "script": [
                    {"type": "text", "text": variant},
                    {"type": "done", "stop_reason": "end_turn"}
                ]
            }),
        };
        MockProvider::new(cfg, "mock-1".into())
    }

    #[tokio::test]
    async fn fitness_from_navigation_log() {
        // 记录:react 命中 4 次,2 Success 2 WrongBranch → hit_rate 0.5 backtrack_rate 0.5
        let store = Store::open_in_memory().unwrap();
        for outcome in [
            NavOutcome::Success,
            NavOutcome::Success,
            NavOutcome::WrongBranch,
            NavOutcome::WrongBranch,
        ] {
            store.record_navigation(&nav_rec("react", outcome)).unwrap();
        }
        // 其它 skill 的记录不应计入 react 的 fitness。
        store.record_navigation(&nav_rec("python", NavOutcome::Success)).unwrap();

        let f = fitness(&store, "react").await;
        assert_eq!(f.hit_rate, 0.5);
        assert_eq!(f.backtrack_rate, 0.5);

        // 无关 skill 无数据 → 全 0(演化时不盲目进化)。
        let none = fitness(&store, "rust").await;
        assert_eq!(none.hit_rate, 0.0);
        assert_eq!(none.backtrack_rate, 0.0);
    }

    fn skill_for(name: &str, desc: &str, cat: &str, body: &str) -> Skill {
        Skill {
            name: name.into(),
            description: desc.into(),
            short_description: None,
            category: cat.into(),
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
            origin: "manual".into(),
            origin_url: None,
            scope: "user".into(),
            allow_implicit: true,
            embedding: None,
        }
    }

    #[tokio::test]
    async fn evolve_adopts_when_variant_better() {
        // 当前 skill 回溯率高(3/4 = 0.75 > ADOPT_THRESHOLD 0.5)→ 触发变异并返回待采纳变体。
        let store = Store::open_in_memory().unwrap();
        for outcome in [
            NavOutcome::Success,
            NavOutcome::WrongBranch,
            NavOutcome::WrongBranch,
            NavOutcome::WrongBranch,
        ] {
            store.record_navigation(&nav_rec("react", outcome)).unwrap();
        }
        let provider = mock_provider("---\nname: react\n---\n# Improved variant\n");
        let current = "---\nname: react\n---\n# Current body\n";
        let organism = evolve_skill(&provider, &store, "react", current).await.unwrap();
        let org = organism.expect("高回溯率应生成待采纳变体");
        assert_eq!(org.skill_name, "react");
        assert!(org.variant.contains("# Improved variant"), "variant: {}", org.variant);
        assert_eq!(provider.calls(), 1, "采纳分支应恰好调用一次模型");
    }

    #[tokio::test]
    async fn evolve_keeps_current_when_not_better() {
        // 回溯率低(0.25 ≤ ADOPT_THRESHOLD 0.5)→ 保留当前,不调用模型(mutate 在采纳门槛之后)。
        let store = Store::open_in_memory().unwrap();
        for outcome in [
            NavOutcome::Success,
            NavOutcome::Success,
            NavOutcome::Success,
            NavOutcome::WrongBranch,
        ] {
            store.record_navigation(&nav_rec("react", outcome)).unwrap();
        }
        let provider = mock_provider("---\nname: react\n---\n# Variant body\n");
        let current = "---\nname: react\n---\n# Current body\n";
        let org = evolve_skill(&provider, &store, "react", current).await.unwrap();
        assert!(org.is_none(), "低回溯率应保留当前");
        assert_eq!(provider.calls(), 0, "保留分支不应调用模型(避免浪费付费调用)");
    }

    #[tokio::test]
    async fn evolve_keeps_current_at_adopt_boundary() {
        // 回溯率恰好 = ADOPT_THRESHOLD(0.5) 时严格不采纳(需 > 0.5):无清晰多数 →
        // 保留当前,不调用模型。0.31 之类的小波动被这道门挡住。
        let store = Store::open_in_memory().unwrap();
        for outcome in [
            NavOutcome::Success,
            NavOutcome::Success,
            NavOutcome::WrongBranch,
            NavOutcome::WrongBranch,
        ] {
            store.record_navigation(&nav_rec("react", outcome)).unwrap();
        }
        let provider = mock_provider("---\nname: react\n---\n# Variant body\n");
        let current = "---\nname: react\n---\n# Current body\n";
        let org = evolve_skill(&provider, &store, "react", current).await.unwrap();
        assert!(org.is_none(), "回溯率恰好 0.5 不应触发变异(需严格 > 0.5)");
        assert_eq!(provider.calls(), 0, "边界不应调用模型");
    }

    #[tokio::test]
    async fn evolve_keeps_current_when_no_data() {
        // 无 navigation 数据 → 不盲目演化,保留当前(防退化),也不调用模型。
        let store = Store::open_in_memory().unwrap();
        let provider = mock_provider("---\nname: react\n---\n# Variant body\n");
        let current = "---\nname: react\n---\n# Current body\n";
        let org = evolve_skill(&provider, &store, "react", current).await.unwrap();
        assert!(org.is_none(), "无数据时应保留当前");
        assert_eq!(provider.calls(), 0);
    }

    #[tokio::test]
    async fn apply_organism_persists_adoption() {
        // 叶子 skill:apply 合法变体 → 盘上正文更新 + DB 行更新 + 审计日志写入。
        let dir = tempfile::tempdir().unwrap();
        let skill_store = rc_skill::SkillStore::new(dir.path());
        let existing = skill_for("react", "react guide", "frontend", "old body\n");
        skill_store.save(&existing).unwrap();

        let store = Store::open_in_memory().unwrap();
        store.upsert_skill(&existing.to_row()).unwrap();

        let variant =
            "---\nname: react\ndescription: improved react guide\ncategory: frontend\ntriggers:\n  - react\n---\n# Improved body\nDo it the right way.\n";
        let organism = Organism {
            skill_name: "react".into(),
            variant: variant.into(),
        };
        let ok = apply_organism(&store, &skill_store, &organism).await.unwrap();
        assert!(ok, "valid variant should be adopted");

        // 盘上正文已更新。
        let on_disk = skill_store.load("react").unwrap();
        assert!(on_disk.body.contains("Improved body"), "body: {}", on_disk.body);

        // DB 行更新:version 递增、description 反映采纳后的 frontmatter。
        let row = store.get_skill("react").unwrap().unwrap();
        assert_eq!(row.version, 2);
        assert!(row.description.contains("improved react guide"));

        // 审计日志:darwinian.adopt(§4c 采纳 → 写审计)。
        let audit = store.list_audit(10).unwrap();
        let adopt = audit
            .iter()
            .find(|a| a.action == "darwinian.adopt")
            .expect("darwinian.adopt audit entry");
        assert!(adopt.detail.contains("react"), "detail: {}", adopt.detail);
        assert_eq!(adopt.actor, "darwinian");
    }

    #[tokio::test]
    async fn apply_organism_preserves_telemetry() {
        // 采纳变体不得重置 usage_count/success_rate/last_used/confidence/embedding:
        // daemon 的 retention gate(usage_count < 5)与 usage 合并都依赖这些字段存活。
        let dir = tempfile::tempdir().unwrap();
        let skill_store = rc_skill::SkillStore::new(dir.path());
        let mut existing = skill_for("react", "react guide", "frontend", "old body\n");
        existing.usage_count = 7;
        existing.success_rate = 0.85;
        existing.last_used = Some("2026-01-01T00:00:00Z".into());
        existing.confidence = 0.9;
        existing.embedding = Some(vec![0.1, 0.2, 0.3]);
        existing.allow_implicit = false;
        skill_store.save(&existing).unwrap();

        let store = Store::open_in_memory().unwrap();
        store.upsert_skill(&existing.to_row()).unwrap();

        let variant =
            "---\nname: react\ndescription: improved react guide\ncategory: frontend\n---\n# Improved body\n";
        let organism = Organism {
            skill_name: "react".into(),
            variant: variant.into(),
        };
        let ok = apply_organism(&store, &skill_store, &organism).await.unwrap();
        assert!(ok, "valid leaf variant should be adopted");

        let on_disk = skill_store.load("react").unwrap();
        assert_eq!(on_disk.usage_count, 7, "usage_count must survive adoption");
        assert!(
            (on_disk.success_rate - 0.85).abs() < 1e-4,
            "success_rate must survive adoption: {}",
            on_disk.success_rate
        );
        assert_eq!(on_disk.last_used.as_deref(), Some("2026-01-01T00:00:00Z"));
        assert!(
            (on_disk.confidence - 0.9).abs() < 1e-4,
            "confidence must survive adoption: {}",
            on_disk.confidence
        );
        assert_eq!(
            on_disk.embedding.as_deref(),
            Some(&[0.1f32, 0.2, 0.3][..]),
            "embedding must survive adoption"
        );
        assert!(!on_disk.allow_implicit, "allow_implicit must survive adoption");

        // DB 行同样保留遥测(daemon 读 DB 行做 usage 合并)。
        let row = store.get_skill("react").unwrap().unwrap();
        assert_eq!(row.usage_count, 7);
    }

    #[tokio::test]
    async fn apply_organism_rejects_shape_violation() {
        // react 是索引(有子 react.performance):带正文的变体违反"索引无正文" → 拒绝且不落盘。
        let dir = tempfile::tempdir().unwrap();
        let skill_store = rc_skill::SkillStore::new(dir.path());
        skill_store.save(&skill_for("react", "react index", "frontend", "")).unwrap();
        skill_store
            .save(&skill_for("react.performance", "react perf", "frontend.react", "full body\n"))
            .unwrap();

        let store = Store::open_in_memory().unwrap();
        let bad_variant =
            "---\nname: react\ncategory: frontend\n---\n# an index must not carry a body\n";
        let organism = Organism {
            skill_name: "react".into(),
            variant: bad_variant.into(),
        };
        let err = apply_organism(&store, &skill_store, &organism).await.unwrap_err();
        assert!(err.contains("shape"), "err: {err}");

        // 盘上未被改写,DB 无该 skill 行,审计无 adopt。
        let on_disk = skill_store.load("react").unwrap();
        assert!(!on_disk.body.contains("must not carry"), "body: {}", on_disk.body);
        assert!(store.get_skill("react").unwrap().is_none());
        assert!(store.list_audit(10).unwrap().is_empty());
    }
}
