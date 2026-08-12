//! 导航反馈消化:聚合 navigation_log → 菜单改写 / 叶子补充候选。
use crate::darwinian::mutate;
use rc_pro::Provider;
use rc_state::{NavOutcome, Store};
use rc_skill::{SkillNavigator, SkillNetwork, SkillStore};

#[derive(Debug, Clone, Default)]
pub struct NavDigest {
    pub menu_rewrites: Vec<(String, String)>,  // (skill_name, description-only 变体:空正文)
    pub leaf_backfills: Vec<(String, String)>, // (skill_name, backfill_text)
}

/// 聚合最近 navigation_log,按 root 分组,生成菜单/叶子改写候选。
pub async fn digest_navigation(
    provider: &dyn Provider,
    store: &Store,
    skill_store: &SkillStore,
) -> Result<NavDigest, String> {
    let recs = store.list_navigation(500).unwrap_or_default();
    let mut digest = NavDigest::default();
    // WrongBranch 高频 root → 菜单改写(≥3 次)。
    let mut wb: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for r in recs.iter().filter(|r| r.outcome == NavOutcome::WrongBranch) {
        *wb.entry(r.root.clone()).or_default() += 1;
    }
    // 菜单改写的目标恒为索引:叶子根的导航只会记 Success,WrongBranch root 的
    // 正文为空。拿空正文当 mutate 上下文只会让模型产出带正文的变体,被
    // enforce_skill_shape(索引无正文)拒绝 → 生产环境菜单改写永远无法落盘。
    // 改为喂索引的真实菜单文本,并要求只改索引的方向描述,正文保持为空。
    let network = SkillNetwork::from_store(skill_store);
    let navigator = SkillNavigator { network: &network, limits: rc_skill::NavigatorLimits::default() };
    for (root, count) in wb {
        if count >= 3
            && skill_store.load(&root).is_some() {
                let menu = navigator.menu(&root);
                let boundary = format!(
                    "This index skill's menu caused {count} wrong-branch navigations. \
                     Rewrite the direction description for this index skill. \
                     Return ONLY frontmatter with a better 'description' field; no body."
                );
                if let Ok(variant) = mutate(provider, &root, &menu, &boundary).await {
                    digest.menu_rewrites.push((root, variant));
                }
            }
    }
    // LeafTooThin → 叶子补充(≥2 次)。叶子取 path_json 末段(serde_json 解析,逗号路径也安全)。
    let mut thin: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for r in recs.iter().filter(|r| r.outcome == NavOutcome::LeafTooThin) {
        let leaf = serde_json::from_str::<Vec<String>>(&r.path_json)
            .ok()
            .and_then(|v| v.last().cloned())
            .unwrap_or_else(|| r.root.clone());
        *thin.entry(leaf).or_default() += 1;
    }
    for (leaf, count) in thin {
        if count >= 2 {
            if let Some(skill) = skill_store.load(&leaf) {
                let boundary = format!(
                    "This leaf skill was too thin ({count} times). Expand with actionable detail."
                );
                if let Ok(variant) = mutate(provider, &leaf, &skill.body, &boundary).await {
                    digest.leaf_backfills.push((leaf, variant));
                }
            }
        }
    }
    Ok(digest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rc_pro::mock::MockProvider;
    use rc_pro::ProviderConfig;
    use rc_skill::Skill;
    use rc_state::NavigationRecord;
    use std::path::PathBuf;

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

    fn skill_for(name: &str, desc: &str, cat: &str, body: &str) -> Skill {
        Skill {
            name: name.into(),
            description: desc.into(),
            short_description: None,
            category: cat.into(),
            path: PathBuf::new(),
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
    async fn digest_groups_wrongbranch_by_parent() {
        // 3 次 WrongBranch root=react(索引,有子)→ 生成 react 菜单改写候选;未达阈值不触发。
        // mutate 上下文是索引的真实菜单文本(见 digest 实现),产出 description-only 变体。
        let dir = tempfile::tempdir().unwrap();
        let skill_store = SkillStore::new(dir.path());
        skill_store
            .save(&skill_for("react", "react index", "frontend", ""))
            .unwrap();
        skill_store
            .save(&skill_for("react.performance", "react perf", "frontend.react", "full body\n"))
            .unwrap();
        skill_store
            .save(&skill_for("python", "python index", "backend", ""))
            .unwrap();

        let store = Store::open_in_memory().unwrap();
        for _ in 0..3 {
            store
                .record_navigation(&nav_rec("react", "[]", NavOutcome::WrongBranch))
                .unwrap();
        }
        // python 只有 2 次(< 3),不应触发菜单改写。
        for _ in 0..2 {
            store
                .record_navigation(&nav_rec("python", "[]", NavOutcome::WrongBranch))
                .unwrap();
        }

        let provider = mock_provider("---\nname: react\ndescription: clearer menu directions\n---\n");
        let digest = digest_navigation(&provider, &store, &skill_store).await.unwrap();

        assert_eq!(digest.menu_rewrites.len(), 1, "only react crosses the >=3 threshold");
        let (name, variant) = &digest.menu_rewrites[0];
        assert_eq!(name, "react");
        assert!(variant.contains("clearer menu"), "variant: {variant}");
        assert!(digest.leaf_backfills.is_empty());
        assert_eq!(provider.calls(), 1, "mutate called once for the single candidate");
    }

    #[tokio::test]
    async fn digest_menu_rewrite_variant_passes_index_shape() {
        // Fix 3:索引菜单改写候选是 description-only(空正文)→ parse 后正文为空,
        // enforce_skill_shape(索引无正文)可通过 → 真正能在生产落盘。
        let dir = tempfile::tempdir().unwrap();
        let skill_store = SkillStore::new(dir.path());
        skill_store
            .save(&skill_for("react", "react index", "frontend", ""))
            .unwrap();
        skill_store
            .save(&skill_for("react.performance", "react perf", "frontend.react", "full body\n"))
            .unwrap();
        let store = Store::open_in_memory().unwrap();
        for _ in 0..3 {
            store
                .record_navigation(&nav_rec("react", "[]", NavOutcome::WrongBranch))
                .unwrap();
        }
        let provider = mock_provider("---\nname: react\ncategory: frontend\ndescription: clearer react direction\n---\n");
        let digest = digest_navigation(&provider, &store, &skill_store).await.unwrap();
        assert_eq!(digest.menu_rewrites.len(), 1);
        let (name, variant) = &digest.menu_rewrites[0];
        assert_eq!(name, "react");
        let (fm, body) = rc_skill::parse_frontmatter(variant).unwrap();
        assert!(fm.description.contains("clearer react direction"));
        assert_eq!(body.trim(), "", "index menu rewrite must keep empty body");
        // 形状通过:react 有子 react.performance(索引),空正文合法。
        let network = SkillNetwork::from_store(&skill_store);
        let candidate = Skill::from_frontmatter(fm, body, PathBuf::new());
        rc_skill::enforce_skill_shape(&network, &candidate).unwrap();
        assert_eq!(provider.calls(), 1);
    }

    #[tokio::test]
    async fn digest_marks_leaf_toothin() {
        // 2 次 LeafTooThin 落在 react.performance 叶子 → 生成叶子补充候选。
        let dir = tempfile::tempdir().unwrap();
        let skill_store = SkillStore::new(dir.path());
        skill_store
            .save(&skill_for("react", "react index", "frontend", "menu body\n"))
            .unwrap();
        skill_store
            .save(&skill_for("react.performance", "react perf", "frontend.react", "thin body\n"))
            .unwrap();
        skill_store
            .save(&skill_for("python", "python index", "backend", "menu body\n"))
            .unwrap();

        let store = Store::open_in_memory().unwrap();
        for _ in 0..2 {
            store
                .record_navigation(
                    &nav_rec("react", r#"["react","react.performance"]"#, NavOutcome::LeafTooThin),
                )
                .unwrap();
        }
        // python 只有 1 次(< 2),不应触发叶子补充。
        store
            .record_navigation(&nav_rec("python", "[]", NavOutcome::LeafTooThin))
            .unwrap();

        let provider = mock_provider("---\nname: react.performance\n---\n# expanded leaf\n");
        let digest = digest_navigation(&provider, &store, &skill_store).await.unwrap();

        assert_eq!(digest.leaf_backfills.len(), 1, "only react.performance crosses the >=2 threshold");
        let (name, variant) = &digest.leaf_backfills[0];
        assert_eq!(name, "react.performance");
        assert!(variant.contains("expanded leaf"), "variant: {variant}");
        assert!(digest.menu_rewrites.is_empty());
        assert_eq!(provider.calls(), 1, "mutate called once for the single candidate");
    }

    #[tokio::test]
    async fn digest_leaf_parse_handles_comma_in_path() {
        // path_json 中路径元素本身含逗号("web.helpers,extra"),仍应解析出完整叶子。
        // 若用字符串按逗号切分,会错误得到 "extra",这里断言 serde_json 解析取到完整叶子。
        let dir = tempfile::tempdir().unwrap();
        let skill_store = SkillStore::new(dir.path());
        skill_store
            .save(&skill_for("web.helpers,extra", "web helpers extra", "frontend.react", "thin body\n"))
            .unwrap();

        let store = Store::open_in_memory().unwrap();
        for _ in 0..2 {
            store
                .record_navigation(
                    &nav_rec(
                        "react",
                        r#"["react","web.helpers,extra"]"#,
                        NavOutcome::LeafTooThin,
                    ),
                )
                .unwrap();
        }

        let provider = mock_provider("---\nname: web.helpers,extra\n---\n# expanded\n");
        let digest = digest_navigation(&provider, &store, &skill_store).await.unwrap();

        assert_eq!(digest.leaf_backfills.len(), 1);
        assert_eq!(digest.leaf_backfills[0].0, "web.helpers,extra");
        assert_eq!(provider.calls(), 1);
    }

    #[tokio::test]
    async fn digest_leaf_empty_path_falls_back_to_root() {
        // 空 path_json → 叶子回落为 root;2 次 LeafTooThin 落在 root 本身 → root 作为叶子补充候选。
        let dir = tempfile::tempdir().unwrap();
        let skill_store = SkillStore::new(dir.path());
        skill_store
            .save(&skill_for("react", "react index", "frontend", "menu body\n"))
            .unwrap();

        let store = Store::open_in_memory().unwrap();
        for _ in 0..2 {
            store
                .record_navigation(&nav_rec("react", "[]", NavOutcome::LeafTooThin))
                .unwrap();
        }

        let provider = mock_provider("---\nname: react\n---\n# expanded root\n");
        let digest = digest_navigation(&provider, &store, &skill_store).await.unwrap();

        assert_eq!(digest.leaf_backfills.len(), 1);
        assert_eq!(digest.leaf_backfills[0].0, "react");
        assert_eq!(provider.calls(), 1);
    }
}
