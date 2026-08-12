use crate::frontmatter::{parse_frontmatter, render_frontmatter, Relation, SkillFrontmatter};
use rc_state::SkillRow;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub short_description: Option<String>,
    pub category: String,
    pub path: PathBuf,
    pub body: String,
    pub relations: Vec<Relation>,
    pub triggers: Vec<String>,
    pub tags: Vec<String>,
    pub version: u32,
    pub confidence: f32,
    pub usage_count: u64,
    pub success_rate: f32,
    pub last_used: Option<String>,
    pub auto: bool,
    pub origin: String,
    pub origin_url: Option<String>,
    pub scope: String,
    pub allow_implicit: bool,
    pub embedding: Option<Vec<f32>>,
}

impl Skill {
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, String> {
        let text = std::fs::read_to_string(path.as_ref()).map_err(|e| e.to_string())?;
        let (fm, body) = parse_frontmatter(&text).map_err(|e| e.to_string())?;
        Ok(Self::from_frontmatter(
            fm,
            body,
            path.as_ref().to_path_buf(),
        ))
    }

    pub fn from_frontmatter(fm: SkillFrontmatter, body: String, path: PathBuf) -> Self {
        Self {
            name: fm.name,
            description: fm.description,
            short_description: fm.short_description,
            category: fm.category,
            path,
            body,
            relations: fm.relations,
            triggers: fm.triggers,
            tags: fm.tags,
            version: fm.version,
            confidence: fm.confidence,
            usage_count: fm.usage_count,
            success_rate: fm.success_rate,
            last_used: fm.last_used,
            auto: fm.auto,
            origin: fm.origin,
            origin_url: fm.origin_url,
            scope: fm.scope,
            allow_implicit: fm.allow_implicit,
            embedding: decode_embedding(fm.embedding_b64.as_deref()),
        }
    }

    pub fn render(&self) -> Result<String, serde_yaml::Error> {
        let fm = SkillFrontmatter {
            name: self.name.clone(),
            description: self.description.clone(),
            short_description: self.short_description.clone(),
            category: self.category.clone(),
            relations: self.relations.clone(),
            triggers: self.triggers.clone(),
            tags: self.tags.clone(),
            version: self.version,
            confidence: self.confidence,
            usage_count: self.usage_count,
            success_rate: self.success_rate,
            last_used: self.last_used.clone(),
            auto: self.auto,
            origin: self.origin.clone(),
            origin_url: self.origin_url.clone(),
            scope: self.scope.clone(),
            allow_implicit: self.allow_implicit,
            products: vec![],
            embedding_b64: self.embedding.as_deref().map(encode_embedding),
        };
        Ok(format!(
            "---\n{}\n---\n{}",
            render_frontmatter(&fm)?,
            self.body
        ))
    }

    pub fn to_row(&self) -> SkillRow {
        SkillRow {
            id: self.name.clone(),
            name: self.name.clone(),
            category: self.category.clone(),
            path: self.path.to_string_lossy().to_string(),
            description: self.description.clone(),
            frontmatter: json!({
                "name": self.name,
                "description": self.description,
                "category": self.category,
                "relations": self.relations,
                "triggers": self.triggers,
                "tags": self.tags,
            }),
            version: self.version as i64,
            confidence: f64::from(self.confidence),
            usage_count: self.usage_count as i64,
            success_count: (self.success_rate * self.usage_count as f32) as i64,
            last_used: self.last_used.clone(),
            auto: self.auto,
            origin: self.origin.clone(),
            origin_url: self.origin_url.clone(),
            scope: self.scope.clone(),
            allow_implicit: self.allow_implicit,
            relations: serde_json::to_value(&self.relations).unwrap_or_else(|_| json!([])),
            embedding: self.embedding.as_deref().map(encode_embedding),
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillSummary {
    pub name: String,
    pub description: String,
    pub category: String,
    pub score: f32,
    pub is_leaf: bool,
}

/// Verify the skill relation graph is acyclic. Targets that do not exist in
/// `skills` are ignored; only known skills participate in the cycle check.
pub fn validate_dag(skills: &[Skill]) -> Result<(), String> {
    use std::collections::{HashMap, HashSet};

    let known: HashSet<&str> = skills.iter().map(|s| s.name.as_str()).collect();
    let mut graph: HashMap<&str, Vec<&str>> = HashMap::new();
    for skill in skills {
        for rel in &skill.relations {
            if rel.skill != skill.name && known.contains(rel.skill.as_str()) {
                graph
                    .entry(skill.name.as_str())
                    .or_default()
                    .push(rel.skill.as_str());
            }
        }
    }

    fn visit<'a>(
        node: &'a str,
        graph: &HashMap<&'a str, Vec<&'a str>>,
        state: &mut HashMap<&'a str, u8>,
        path: &mut Vec<String>,
    ) -> Result<(), String> {
        match state.get(node) {
            Some(&1) => {
                let start = path.iter().position(|n| n == node).unwrap_or(0);
                let cycle: Vec<String> = path[start..]
                    .iter()
                    .cloned()
                    .chain([node.to_string()])
                    .collect();
                return Err(format!("skill relation cycle: {}", cycle.join(" -> ")));
            }
            Some(&2) => return Ok(()),
            _ => {}
        }
        state.insert(node, 1);
        path.push(node.to_string());
        if let Some(children) = graph.get(node) {
            for child in children {
                visit(child, graph, state, path)?;
            }
        }
        path.pop();
        state.insert(node, 2);
        Ok(())
    }

    let mut state: HashMap<&str, u8> = HashMap::new();
    for node in graph.keys() {
        if state.get(node) != Some(&2) {
            visit(node, &graph, &mut state, &mut Vec::new())?;
        }
    }
    Ok(())
}
pub fn encode_embedding(v: &[f32]) -> String {
    use base64::Engine;
    let bytes: Vec<u8> = v.iter().flat_map(|f| f.to_le_bytes()).collect();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

pub fn decode_embedding(b64: Option<&str>) -> Option<Vec<f32>> {
    use base64::Engine;
    let b64 = b64?;
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    if bytes.len() % 4 != 0 {
        return None;
    }
    Some(
        bytes
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_skill(name: &str, relations: Vec<Relation>) -> Skill {
        Skill {
            name: name.into(),
            description: String::new(),
            short_description: None,
            category: "test".into(),
            path: PathBuf::new(),
            body: String::new(),
            relations,
            triggers: vec![],
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
    fn dag_validator_accepts_acyclic_graphs() {
        use crate::frontmatter::RelationKind;
        let a = test_skill(
            "a",
            vec![Relation {
                kind: RelationKind::Prerequisite,
                skill: "b".into(),
            }],
        );
        let b = test_skill("b", vec![]);
        assert!(validate_dag(&[a, b]).is_ok());
    }

    #[test]
    fn dag_validator_rejects_cycles() {
        use crate::frontmatter::RelationKind;
        let x = test_skill(
            "x",
            vec![Relation {
                kind: RelationKind::Refines,
                skill: "y".into(),
            }],
        );
        let y = test_skill(
            "y",
            vec![Relation {
                kind: RelationKind::Composes,
                skill: "x".into(),
            }],
        );
        let err = validate_dag(&[x, y]).unwrap_err();
        assert!(err.contains("cycle"));
    }
    #[test]
    fn embedding_roundtrip() {
        let v = vec![0.1f32, -0.2, 0.5, 1.0];
        assert_eq!(decode_embedding(Some(&encode_embedding(&v))), Some(v));
    }
}
