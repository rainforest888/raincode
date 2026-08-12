//! Skill 网络结构:目录拓扑 → SkillNetwork(索引/叶子/软引用)。
use crate::model::Skill;
use crate::store::SkillStore;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NetworkConfig {
    #[serde(default)]
    pub soft_links: Vec<SoftLink>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SoftLink {
    pub parent: String,
    pub target: String,
}

/// 读 `network.toml`;文件不存在 → 空配置。
pub fn load_network_config(root: &Path) -> Result<NetworkConfig, String> {
    let path = root.join("network.toml");
    if !path.exists() {
        return Ok(NetworkConfig::default());
    }
    let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    toml::from_str(&text).map_err(|e| e.to_string())
}

pub struct SkillNode {
    pub skill: Skill,
    pub children: Vec<String>,
    pub is_leaf: bool,
}

pub struct SkillNetwork {
    pub nodes: Vec<SkillNode>,
    config: NetworkConfig,
}

impl SkillNetwork {
    /// 从 store 目录构建网络。物理子目录 = children;network.toml 软引用 = 虚拟 children。
    pub fn from_store(store: &SkillStore) -> Self {
        let config = load_network_config(store.root()).unwrap_or_default();
        let skills = store.discover();
        let mut nodes: Vec<SkillNode> = Vec::new();
        // 目录路径 → name:父 skill 路径是子 skill 路径的前缀目录。
        for skill in &skills {
            let rel = skill.path.parent().and_then(|p| p.strip_prefix(store.root()).ok());
            let dir = rel.unwrap_or(std::path::Path::new(""));
            let children: Vec<String> = skills.iter()
                .filter_map(|other| {
                    if other.name == skill.name { return None; }
                    let orel = other.path.parent().and_then(|p| p.strip_prefix(store.root()).ok())?;
                    let odir = orel;
                    // other 的目录是 skill 目录的直接子目录。
                    if odir.parent() == Some(dir) {
                        Some(other.name.clone())
                    } else {
                        None
                    }
                })
                .collect();
            nodes.push(SkillNode {
                skill: skill.clone(),
                children,
                is_leaf: false, // 先标 false,软引用处理后再算。
            });
        }
        // 软引用:把 target 加为 parent 的虚拟 child。
        for link in &config.soft_links {
            if let Some(node) = nodes.iter_mut().find(|n| n.skill.name == link.parent) {
                node.children.push(link.target.clone());
            }
        }
        // 叶子判定:children 为空(物理+软引用都算)。
        for node in &mut nodes {
            node.is_leaf = node.children.is_empty();
        }
        Self { nodes, config }
    }

    pub fn children_of(&self, name: &str) -> Vec<&SkillNode> {
        let children_names: Vec<String> = self.nodes.iter()
            .find(|n| n.skill.name == name)
            .map(|n| n.children.clone())
            .unwrap_or_default();
        children_names.iter()
            .filter_map(|cname| self.nodes.iter().find(|n| n.skill.name == *cname))
            .collect()
    }

    pub fn leaf(&self, name: &str) -> Option<&Skill> {
        self.nodes.iter().find(|n| n.skill.name == name && n.is_leaf).map(|n| &n.skill)
    }

    /// 校验:软引用 target 必须存在;目录树天然无环(无需校验)。
    pub fn validate(&self) -> Result<(), String> {
        for link in &self.config.soft_links {
            if !self.nodes.iter().any(|n| n.skill.name == link.target) {
                return Err(format!("soft link target '{}' does not exist", link.target));
            }
        }
        Ok(())
    }
}

/// 演化把关:索引(有子)不应有正文;叶子(无子)必须有完整正文。
/// skill 在 network 中找不到节点时按叶子处理(新 skill 尚未落盘)。
pub fn enforce_skill_shape(network: &SkillNetwork, skill: &Skill) -> Result<(), String> {
    let node = network.nodes.iter().find(|n| n.skill.name == skill.name);
    let has_children = node.map(|n| !n.children.is_empty()).unwrap_or(false);
    let body_trimmed = skill.body.trim();
    if has_children && !body_trimmed.is_empty() {
        return Err(format!(
            "skill '{}' is an index (has children); body must stay empty",
            skill.name
        ));
    }
    if !has_children && body_trimmed.is_empty() {
        return Err(format!(
            "skill '{}' is a leaf; body must be complete",
            skill.name
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{enforce_skill_shape, load_network_config, SkillNetwork};
    use crate::store::SkillStore;

    fn skill_with(name: &str, desc: &str, cat: &str, body: &str, triggers: Vec<&str>) -> crate::model::Skill {
        crate::model::Skill {
            name: name.into(), description: desc.into(), short_description: None,
            category: cat.into(), path: std::path::PathBuf::new(), body: body.into(),
            relations: vec![], triggers: triggers.into_iter().map(String::from).collect(),
            tags: vec![], version: 1, confidence: 0.8, usage_count: 0, success_rate: 0.0,
            last_used: None, auto: false, origin: "manual".into(), origin_url: None,
            scope: "user".into(), allow_implicit: true, embedding: None,
        }
    }

    #[test]
    fn nested_dirs_build_tree() {
        let dir = tempfile::tempdir().unwrap();
        let store = SkillStore::new(dir.path());
        // frontend/react/SKILL.md (索引), frontend/react/react.performance/SKILL.md (叶子)。
        let react = skill_with("react", "react framework", "frontend", "body", vec![]);
        let perf = skill_with("react.performance", "react performance", "frontend.react", "full body", vec![]);
        store.save(&react).unwrap();
        store.save(&perf).unwrap();
        let net = SkillNetwork::from_store(&store);
        assert_eq!(net.nodes.len(), 2);
        let react_node = net.nodes.iter().find(|n| n.skill.name == "react").unwrap();
        assert!(!react_node.is_leaf);
        assert_eq!(react_node.children, vec!["react.performance"]);
        assert!(net.leaf("react.performance").is_some());
    }

    #[test]
    fn leaf_detection_by_no_children() {
        let dir = tempfile::tempdir().unwrap();
        let store = SkillStore::new(dir.path());
        let a = skill_with("leaf-a", "desc", "cat", "body", vec![]);
        store.save(&a).unwrap();
        let net = SkillNetwork::from_store(&store);
        assert!(net.nodes[0].is_leaf);
    }

    #[test]
    fn soft_links_add_virtual_children() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("network.toml"), r#"
[[soft_links]]
parent = "react"
target = "testing-common"
"#).unwrap();
        let store = SkillStore::new(dir.path());
        let react = skill_with("react", "d", "f", "body", vec![]);
        let common = skill_with("testing-common", "d", "f", "full", vec![]);
        store.save(&react).unwrap();
        store.save(&common).unwrap();
        let cfg = load_network_config(dir.path()).unwrap();
        assert_eq!(cfg.soft_links.len(), 1);
        let net = SkillNetwork::from_store(&store);
        // testing-common 物理在根,软引用让它成为 react 的子。
        assert!(net.children_of("react").iter().any(|c| c.skill.name == "testing-common"));
    }

    #[test]
    fn dangling_soft_link_errors() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("network.toml"), r#"
[[soft_links]]
parent = "react"
target = "does-not-exist"
"#).unwrap();
        let store = SkillStore::new(dir.path());
        let react = skill_with("react", "d", "f", "body", vec![]);
        store.save(&react).unwrap();
        let cfg = load_network_config(dir.path()).unwrap();
        assert_eq!(cfg.soft_links.len(), 1);
        let net = SkillNetwork::from_store(&store);
        assert!(net.validate().is_err(), "dangling soft link must error");
    }

    #[test]
    fn enforce_shape_rejects_index_with_body() {
        let dir = tempfile::tempdir().unwrap();
        let store = SkillStore::new(dir.path());
        let index = skill_with("react", "react framework", "frontend", "", vec![]);
        let leaf = skill_with("react.performance", "react perf", "frontend.react", "full body", vec![]);
        store.save(&index).unwrap();
        store.save(&leaf).unwrap();
        let net = SkillNetwork::from_store(&store);
        let offending = skill_with("react", "react framework", "frontend", "an index must not carry body", vec![]);
        let err = enforce_skill_shape(&net, &offending).unwrap_err();
        assert!(err.contains("index"), "err: {err}");
    }

    #[test]
    fn enforce_shape_rejects_leaf_without_body() {
        let dir = tempfile::tempdir().unwrap();
        let store = SkillStore::new(dir.path());
        let leaf = skill_with("react.performance", "react perf", "frontend.react", "full body", vec![]);
        store.save(&leaf).unwrap();
        let net = SkillNetwork::from_store(&store);
        let offending = skill_with("react.performance", "react perf", "frontend.react", "", vec![]);
        let err = enforce_skill_shape(&net, &offending).unwrap_err();
        assert!(err.contains("leaf"), "err: {err}");
    }

    #[test]
    fn enforce_shape_accepts_leaf_with_body() {
        let dir = tempfile::tempdir().unwrap();
        let store = SkillStore::new(dir.path());
        let leaf = skill_with("fix-flaky", "fix flaky tests", "testing", "full body", vec![]);
        store.save(&leaf).unwrap();
        let net = SkillNetwork::from_store(&store);
        assert!(enforce_skill_shape(&net, &leaf).is_ok());
    }

    #[test]
    fn enforce_shape_accepts_index_with_empty_body() {
        let dir = tempfile::tempdir().unwrap();
        let store = SkillStore::new(dir.path());
        let index = skill_with("react", "react framework", "frontend", "", vec![]);
        let leaf = skill_with("react.performance", "react perf", "frontend.react", "full body", vec![]);
        store.save(&index).unwrap();
        store.save(&leaf).unwrap();
        let net = SkillNetwork::from_store(&store);
        assert!(enforce_skill_shape(&net, &index).is_ok());
    }
}
