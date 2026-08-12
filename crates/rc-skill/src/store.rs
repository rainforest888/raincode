use crate::model::Skill;
use std::path::{Path, PathBuf};

#[derive(Clone)]
pub struct SkillStore {
    root: PathBuf,
}

impl SkillStore {
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Discover every `SKILL.md` under the store root. Review-staged skills
    /// (under `.review/`) are excluded — they await user approval, not selection.
    pub fn discover(&self) -> Vec<Skill> {
        if !self.root.exists() {
            return Vec::new();
        }
        let mut skills = Vec::new();
        for entry in walkdir::WalkDir::new(&self.root).follow_links(false) {
            let Ok(entry) = entry else { continue };
            if !entry.file_type().is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if name != "SKILL.md" {
                continue;
            }
            // 跳过 review 隔离区(未确认的 auto skill)。
            if entry.path().to_string_lossy().contains(".review") {
                continue;
            }
            if let Ok(skill) = Skill::from_path(entry.path()) {
                skills.push(skill);
            }
        }
        skills
    }

    /// 列出 review 隔离区(pending 确认)的 skill。
    pub fn discover_review(&self) -> Vec<Skill> {
        let dir = self.root.join(".review");
        if !dir.exists() {
            return Vec::new();
        }
        let mut skills = Vec::new();
        for entry in walkdir::WalkDir::new(&dir).follow_links(false) {
            let Ok(entry) = entry else { continue };
            if !entry.file_type().is_file() {
                continue;
            }
            if entry.file_name().to_string_lossy() != "SKILL.md" {
                continue;
            }
            if let Ok(skill) = Skill::from_path(entry.path()) {
                skills.push(skill);
            }
        }
        skills
    }

    pub fn load(&self, name: &str) -> Option<Skill> {
        self.discover().into_iter().find(|s| s.name == name)
    }

    pub fn save(&self, skill: &Skill) -> Result<PathBuf, String> {
        // review scope 的新 skill 进 .review 隔离区,等用户 approve 才转正。
        // 非 review:category 点分变成嵌套子目录(frontend.react → frontend/react/)。
        let dir = if skill.scope == "review" {
            self.root.join(".review").join(&skill.name)
        } else {
            self.root.join(skill.category.replace('.', "/")).join(&skill.name)
        };
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let path = dir.join("SKILL.md");
        std::fs::write(&path, skill.render().map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        Ok(path)
    }

    /// 把 review 区 skill 转正(scope → user),移出 .review 到正式 category 路径。
    pub fn approve(&self, name: &str) -> Result<PathBuf, String> {
        let review_dir = self.root.join(".review").join(name);
        let md = review_dir.join("SKILL.md");
        let mut skill = Skill::from_path(&md).map_err(|e| e.to_string())?;
        skill.scope = "user".into();
        skill.confidence = skill.confidence.max(0.7);
        // 先删 review 副本,再按 user 路径保存。
        std::fs::remove_dir_all(&review_dir).map_err(|e| e.to_string())?;
        self.save(&skill)
    }

    /// 拒绝 review 区 skill(删除)。
    pub fn reject(&self, name: &str) -> Result<(), String> {
        let review_dir = self.root.join(".review").join(name);
        if review_dir.exists() {
            std::fs::remove_dir_all(&review_dir).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// 删除一个 skill。只删该 skill 自身的目录,不波及子树:
    /// 叶子(目录无子目录)→ 删整个目录;索引(目录有子目录,含子 skill)
    /// → 只删 SKILL.md 文件,保留子 skill 子树。根级 skill(无父目录)只删文件。
    pub fn remove(&self, name: &str) -> Result<(), String> {
        if let Some(skill) = self.load(name) {
            let Some(dir) = skill.path.parent() else {
                return Ok(());
            };
            if dir == self.root {
                std::fs::remove_file(&skill.path).map_err(|e| e.to_string())?;
                return Ok(());
            }
            let is_index = std::fs::read_dir(dir)
                .map(|rd| {
                    rd.filter_map(std::result::Result::ok)
                        .any(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                })
                .unwrap_or(false);
            if is_index {
                // 索引:保留子 skill 目录,只删自身的 SKILL.md。
                std::fs::remove_file(&skill.path).map_err(|e| e.to_string())?;
            } else {
                // 叶子:目录里只有自身 SKILL.md → 整个删除。
                std::fs::remove_dir_all(dir).map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frontmatter::RelationKind;

    #[test]
    fn save_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = SkillStore::new(dir.path());
        let mut skill = Skill {
            name: "demo".into(),
            description: "a demo skill".into(),
            short_description: None,
            category: "scratch".into(),
            path: PathBuf::new(),
            body: "Do the thing.\n".into(),
            relations: vec![],
            triggers: vec!["demo".into()],
            tags: vec![],
            version: 1,
            confidence: 0.9,
            usage_count: 0,
            success_rate: 0.0,
            last_used: None,
            auto: true,
            origin: "evolved".into(),
            origin_url: None,
            scope: "user".into(),
            allow_implicit: true,
            embedding: None,
        };
        store.save(&skill).unwrap();
        let loaded = store.load("demo").unwrap();
        assert_eq!(loaded.body.trim(), "Do the thing.");
        assert_eq!(loaded.confidence, 0.9);
        skill.relations = vec![crate::frontmatter::Relation {
            kind: RelationKind::Composes,
            skill: "scratch.base".into(),
        }];
        skill.body = "Updated.\n".into();
        store.save(&skill).unwrap();
        let loaded = store.load("demo").unwrap();
        assert_eq!(loaded.body.trim(), "Updated.");
        assert_eq!(loaded.relations[0].kind, RelationKind::Composes);
        store.remove("demo").unwrap();
        assert!(store.load("demo").is_none());
    }

    #[test]
    fn review_scope_stages_then_approves() {
        let dir = tempfile::tempdir().unwrap();
        let store = SkillStore::new(dir.path());
        let skill = Skill {
            name: "pending-skill".into(),
            description: "a pending skill".into(),
            short_description: None,
            category: "scratch".into(),
            path: PathBuf::new(),
            body: "Do the thing.\n".into(),
            relations: vec![],
            triggers: vec![],
            tags: vec![],
            version: 1,
            confidence: 0.6,
            usage_count: 0,
            success_rate: 0.0,
            last_used: None,
            auto: true,
            origin: "evolved".into(),
            origin_url: None,
            scope: "review".into(),
            allow_implicit: true,
            embedding: None,
        };
        // 写入 review 隔离区:正常 discover 看不到,discover_review 看得到。
        store.save(&skill).unwrap();
        assert!(store.load("pending-skill").is_none(), "review skill not in normal discover");
        assert_eq!(store.discover_review().len(), 1);
        // approve 转正 → 进正常 discover。
        store.approve("pending-skill").unwrap();
        let approved = store.load("pending-skill").unwrap();
        assert_eq!(approved.scope, "user");
        assert!(approved.confidence >= 0.7);
        assert!(store.discover_review().is_empty());
    }

    #[test]
    fn reject_removes_review_skill() {
        let dir = tempfile::tempdir().unwrap();
        let store = SkillStore::new(dir.path());
        let skill = Skill {
            name: "junk".into(),
            description: "junk".into(),
            short_description: None,
            category: "scratch".into(),
            path: PathBuf::new(),
            body: "Do.\n".into(),
            relations: vec![],
            triggers: vec![],
            tags: vec![],
            version: 1,
            confidence: 0.5,
            usage_count: 0,
            success_rate: 0.0,
            last_used: None,
            auto: true,
            origin: "evolved".into(),
            origin_url: None,
            scope: "review".into(),
            allow_implicit: true,
            embedding: None,
        };
        store.save(&skill).unwrap();
        assert_eq!(store.discover_review().len(), 1);
        store.reject("junk").unwrap();
        assert!(store.discover_review().is_empty());
    }

    fn leaf_skill(name: &str, category: &str, body: &str) -> Skill {
        Skill {
            name: name.into(),
            description: format!("{name} desc"),
            short_description: None,
            category: category.into(),
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
            auto: false,
            origin: "manual".into(),
            origin_url: None,
            scope: "user".into(),
            allow_implicit: true,
            embedding: None,
        }
    }

    #[test]
    fn remove_index_keeps_child_subtrees() {
        let dir = tempfile::tempdir().unwrap();
        let store = SkillStore::new(dir.path());
        // 索引 react(空正文)+ 子叶子 react.performance。
        store.save(&leaf_skill("react", "frontend", "")).unwrap();
        store
            .save(&leaf_skill("react.performance", "frontend.react", "full body"))
            .unwrap();
        // 删除索引 → 只删自身 SKILL.md,子 skill 子树必须保留。
        store.remove("react").unwrap();
        assert!(store.load("react").is_none(), "index must be removed");
        assert!(
            store.load("react.performance").is_some(),
            "child subtree must survive index removal"
        );
        // 索引目录仍在(容纳子目录),但自身 SKILL.md 已删。
        let index_dir = dir.path().join("frontend/react");
        assert!(index_dir.is_dir(), "index dir must survive (children live there)");
        assert!(
            !index_dir.join("SKILL.md").exists(),
            "index SKILL.md must be deleted"
        );
        assert!(
            index_dir.join("react.performance/SKILL.md").exists(),
            "child SKILL.md must survive"
        );
    }

    #[test]
    fn remove_leaf_deletes_only_its_dir() {
        let dir = tempfile::tempdir().unwrap();
        let store = SkillStore::new(dir.path());
        store.save(&leaf_skill("demo", "scratch", "body")).unwrap();
        store.save(&leaf_skill("other", "scratch.other", "other body")).unwrap();
        let demo_dir = dir.path().join("scratch/demo");
        assert!(demo_dir.is_dir());
        store.remove("demo").unwrap();
        assert!(store.load("demo").is_none());
        assert!(!demo_dir.exists(), "leaf dir must be deleted");
        assert!(
            store.load("other").is_some(),
            "unrelated skill must survive"
        );
    }
}
