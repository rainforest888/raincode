use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationKind {
    Refines,
    Prerequisite,
    VariantOf,
    Composes,
    Contradicts,
}

impl RelationKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Refines => "refines",
            Self::Prerequisite => "prerequisite",
            Self::VariantOf => "variant_of",
            Self::Composes => "composes",
            Self::Contradicts => "contradicts",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Relation {
    pub kind: RelationKind,
    pub skill: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillFrontmatter {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short_description: Option<String>,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub relations: Vec<Relation>,
    #[serde(default)]
    pub triggers: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default = "default_confidence")]
    pub confidence: f32,
    #[serde(default)]
    pub usage_count: u64,
    #[serde(default)]
    pub success_rate: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used: Option<String>,
    #[serde(default)]
    pub auto: bool,
    #[serde(default = "default_origin")]
    pub origin: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_url: Option<String>,
    #[serde(default = "default_scope")]
    pub scope: String,
    #[serde(default = "default_true")]
    pub allow_implicit: bool,
    #[serde(default)]
    pub products: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding_b64: Option<String>,
}

impl Default for SkillFrontmatter {
    fn default() -> Self {
        Self {
            name: String::new(),
            description: String::new(),
            short_description: None,
            category: String::new(),
            relations: Vec::new(),
            triggers: Vec::new(),
            tags: Vec::new(),
            version: default_version(),
            confidence: default_confidence(),
            usage_count: 0,
            success_rate: 0.0,
            last_used: None,
            auto: false,
            origin: default_origin(),
            origin_url: None,
            scope: default_scope(),
            allow_implicit: default_true(),
            products: Vec::new(),
            embedding_b64: None,
        }
    }
}

fn default_version() -> u32 {
    1
}

fn default_confidence() -> f32 {
    0.5
}

fn default_origin() -> String {
    "manual".to_string()
}

fn default_scope() -> String {
    "user".to_string()
}

fn default_true() -> bool {
    true
}

#[derive(Debug, thiserror::Error)]
pub enum FrontmatterError {
    #[error("missing frontmatter delimiters in skill file")]
    MissingDelimiters,
    #[error("yaml parse error: {0}")]
    Yaml(#[from] serde_yaml::Error),
    #[error("missing required field: {0}")]
    MissingField(String),
}

/// Split a SKILL.md into frontmatter and body. Expects:
/// ```text
/// ---
/// yaml...
/// ---
/// body...
/// ```
pub fn parse_frontmatter(text: &str) -> Result<(SkillFrontmatter, String), FrontmatterError> {
    let trimmed = text.trim_start_matches('\u{feff}');
    let rest = trimmed
        .strip_prefix("---")
        .ok_or(FrontmatterError::MissingDelimiters)?;
    let end = rest
        .find("\n---")
        .ok_or(FrontmatterError::MissingDelimiters)?;
    let yaml = &rest[..end];
    let body_start = end + 4;
    let mut body = rest[body_start..].to_string();
    if let Some(stripped) = body.strip_prefix('\n') {
        body = stripped.to_string();
    }
    let fm: SkillFrontmatter = serde_yaml::from_str(yaml)?;
    if fm.name.trim().is_empty() {
        return Err(FrontmatterError::MissingField("name".into()));
    }
    Ok((fm, body))
}

/// Render frontmatter back to the YAML block of a SKILL.md.
pub fn render_frontmatter(fm: &SkillFrontmatter) -> Result<String, serde_yaml::Error> {
    let s = serde_yaml::to_string(fm)?;
    Ok(s.trim_end_matches('\n').to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "---\nname: pytest-flake-fix\ndescription: fix flaky pytest collection errors\ncategory: testing.pytest\nrelations:\n  - kind: refines\n    skill: testing.debugging\ntriggers:\n  - pytest\n  - flaky\ntags: [python, pytest]\nauto: true\norigin: evolved\n---\n# Body\nRun pytest once, note collection errors, fix conftest imports.\n";

    #[test]
    fn parses_frontmatter_and_body() {
        let (fm, body) = parse_frontmatter(SAMPLE).unwrap();
        assert_eq!(fm.name, "pytest-flake-fix");
        assert_eq!(fm.category, "testing.pytest");
        assert_eq!(fm.relations.len(), 1);
        assert_eq!(fm.relations[0].kind, RelationKind::Refines);
        assert_eq!(fm.triggers, vec!["pytest", "flaky"]);
        assert!(fm.auto);
        assert!(body.contains("Run pytest once"));
    }

    #[test]
    fn rejects_missing_name() {
        let s = "---\ndescription: no name\n---\nbody";
        assert!(parse_frontmatter(s).is_err());
    }
}
