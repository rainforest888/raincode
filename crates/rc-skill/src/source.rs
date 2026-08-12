//! Skill sources: local directory and GitHub-first remote installation.
use crate::frontmatter::parse_frontmatter;
use crate::model::Skill;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillHit {
    pub name: String,
    pub description: String,
    pub origin: String,
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallReport {
    pub installed: Vec<String>,
    pub origin: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillMeta {
    pub name: String,
    pub origin: String,
    pub origin_url: String,
    pub version: String,
    pub installed_at: String,
    pub source: String,
}

#[derive(Debug, thiserror::Error)]
pub enum SkillSourceError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("git error: {0}")]
    Git(String),
    #[error("frontmatter error: {0}")]
    Frontmatter(String),
    #[error("http error: {0}")]
    Http(String),
    #[error("http status {status}: {body}")]
    HttpStatus { status: u16, body: String },
    #[error("no skills found at {0}")]
    NoSkills(String),
}

#[async_trait]
pub trait SkillSource: Send + Sync {
    fn name(&self) -> &str;
    async fn install(&self, spec: &str, dest: &Path) -> Result<InstallReport, SkillSourceError>;
    async fn search(&self, query: &str) -> Vec<SkillHit>;
}

pub struct LocalSource {
    root: PathBuf,
}

impl LocalSource {
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
        }
    }
}

#[async_trait]
impl SkillSource for LocalSource {
    fn name(&self) -> &str {
        "local"
    }

    async fn install(&self, spec: &str, dest: &Path) -> Result<InstallReport, SkillSourceError> {
        let src = PathBuf::from(spec);
        if !src.exists() {
            return Err(SkillSourceError::NoSkills(
                src.to_string_lossy().to_string(),
            ));
        }
        let skills = collect_skill_files(&src);
        if skills.is_empty() {
            return Err(SkillSourceError::NoSkills(
                src.to_string_lossy().to_string(),
            ));
        }
        std::fs::create_dir_all(dest)?;
        let mut installed = Vec::new();
        for path in skills {
            let skill = Skill::from_path(&path).map_err(SkillSourceError::Frontmatter)?;
            let target = dest.join(&skill.name);
            std::fs::create_dir_all(&target)?;
            let content = skill
                .render()
                .map_err(|e| SkillSourceError::Frontmatter(e.to_string()))?;
            std::fs::write(target.join("SKILL.md"), content)?;
            installed.push(skill.name);
        }
        Ok(InstallReport {
            installed,
            origin: spec.to_string(),
            version: "local".into(),
        })
    }

    async fn search(&self, query: &str) -> Vec<SkillHit> {
        collect_skill_files(&self.root)
            .into_iter()
            .filter_map(|p| Skill::from_path(&p).ok())
            .filter(|s| {
                let hay =
                    format!("{} {} {}", s.name, s.description, s.tags.join(" ")).to_lowercase();
                hay.contains(&query.to_lowercase())
            })
            .map(|s| SkillHit {
                name: s.name,
                description: s.description,
                origin: "local".into(),
                url: s.path.to_string_lossy().to_string(),
            })
            .collect()
    }
}

pub struct RemoteSource;

impl Default for RemoteSource {
    fn default() -> Self {
        Self::new()
    }
}

impl RemoteSource {
    pub fn new() -> Self {
        Self
    }

    async fn search_repositories(&self, query: &str) -> Vec<SkillHit> {
        let client = reqwest::Client::new();
        let url = format!(
            "https://api.github.com/search/repositories?q={query}&sort=stars&order=desc&per_page=8"
        );
        let resp = client
            .get(&url)
            .header("User-Agent", "raincode")
            .header("Accept", "application/vnd.github+json")
            .send()
            .await;
        let Ok(resp) = resp else { return vec![] };
        let Ok(body) = resp.json::<serde_json::Value>().await else {
            return vec![];
        };
        body["items"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|item| {
                let full = item["full_name"].as_str()?.to_string();
                Some(SkillHit {
                    name: full.clone(),
                    description: item["description"].as_str().unwrap_or("").to_string(),
                    origin: "github".into(),
                    url: item["html_url"].as_str()?.to_string(),
                })
            })
            .collect()
    }

    async fn search_code(&self, query: &str) -> Vec<SkillHit> {
        let q = format!("filename:SKILL.md {query}");
        let client = reqwest::Client::new();
        let mut builder = client
            .get("https://api.github.com/search/code")
            .query(&[("q", q.as_str()), ("per_page", "8")])
            .header("User-Agent", "raincode")
            .header("Accept", "application/vnd.github+json");
        if let Some(token) = github_token() {
            builder = builder.header("Authorization", format!("Bearer {token}"));
        }
        let Ok(resp) = builder.send().await else {
            return vec![];
        };
        if !resp.status().is_success() {
            return vec![];
        }
        let Ok(body) = resp.json::<serde_json::Value>().await else {
            return vec![];
        };
        body["items"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|item| {
                let repo = item["repository"]["full_name"].as_str().unwrap_or("github");
                let path = item["path"].as_str().unwrap_or("SKILL.md");
                Some(SkillHit {
                    name: item["name"].as_str()?.to_string(),
                    description: format!("{repo}: {path}"),
                    origin: "github".into(),
                    url: item["html_url"].as_str()?.to_string(),
                })
            })
            .collect()
    }

    async fn install_raw(
        &self,
        spec: &str,
        dest: &Path,
    ) -> Result<InstallReport, SkillSourceError> {
        let client = reqwest::Client::new();
        let response = client
            .get(spec)
            .header("User-Agent", "raincode")
            .send()
            .await
            .map_err(|e| SkillSourceError::Http(e.to_string()))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|e| SkillSourceError::Http(e.to_string()))?;
        if !status.is_success() {
            return Err(SkillSourceError::HttpStatus {
                status: status.as_u16(),
                body,
            });
        }
        let (fm, text) =
            parse_frontmatter(&body).map_err(|e| SkillSourceError::Frontmatter(e.to_string()))?;
        let mut skill = Skill::from_frontmatter(fm, text, PathBuf::new());
        skill.origin = "installed".into();
        skill.origin_url = Some(spec.to_string());
        skill.scope = "user".into();
        skill.auto = false;
        let origin_dir = dest.join(safe_origin_dir(spec));
        let target = origin_dir.join(&skill.name).join("SKILL.md");
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(
            &target,
            skill
                .render()
                .map_err(|e| SkillSourceError::Frontmatter(e.to_string()))?,
        )?;
        let meta = SkillMeta {
            name: skill.name.clone(),
            origin: "installed".into(),
            origin_url: spec.to_string(),
            version: "raw".into(),
            installed_at: chrono::Utc::now().to_rfc3339(),
            source: "raw".into(),
        };
        if let Some(parent) = target.parent() {
            std::fs::write(
                parent.join("meta.json"),
                serde_json::to_string_pretty(&meta).unwrap_or_else(|_| "{}".into()),
            )?;
        }
        Ok(InstallReport {
            installed: vec![skill.name],
            origin: spec.to_string(),
            version: "raw".into(),
        })
    }
}

#[async_trait]
impl SkillSource for RemoteSource {
    fn name(&self) -> &str {
        "github"
    }

    async fn install(&self, spec: &str, dest: &Path) -> Result<InstallReport, SkillSourceError> {
        if is_raw_url(spec) {
            return self.install_raw(spec, dest).await;
        }
        let slug = normalize_spec(spec);
        let tmp = std::env::temp_dir().join(format!("raincode-skill-{}", safe_origin_dir(&slug)));
        let _ = std::fs::remove_dir_all(&tmp);
        let status = Command::new("git")
            .args([
                "clone",
                "--depth",
                "1",
                &slug,
                tmp.to_str().unwrap_or_default(),
            ])
            .status()
            .map_err(|e| SkillSourceError::Git(e.to_string()))?;
        if !status.success() {
            return Err(SkillSourceError::Git(format!(
                "git clone failed for {slug}"
            )));
        }
        let skills = collect_skill_files(&tmp);
        if skills.is_empty() {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(SkillSourceError::NoSkills(slug));
        }
        let version = Command::new("git")
            .args(["-C", tmp.to_str().unwrap_or_default(), "rev-parse", "HEAD"])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_else(|| "unknown".into());
        let origin_dir = dest.join(safe_origin_dir(&slug));
        std::fs::create_dir_all(&origin_dir)?;
        let mut installed = Vec::new();
        for path in skills {
            let mut skill =
                Skill::from_path(&path).map_err(SkillSourceError::Frontmatter)?;
            skill.origin = "installed".into();
            skill.origin_url = Some(slug.clone());
            skill.scope = "user".into();
            skill.auto = false;
            let rel = path.strip_prefix(&tmp).unwrap_or(&path);
            let target = origin_dir.join(rel);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(
                &target,
                skill
                    .render()
                    .map_err(|e| SkillSourceError::Frontmatter(e.to_string()))?,
            )?;
            let meta = SkillMeta {
                name: skill.name.clone(),
                origin: "installed".into(),
                origin_url: slug.clone(),
                version: version.clone(),
                installed_at: chrono::Utc::now().to_rfc3339(),
                source: "github".into(),
            };
            if let Some(parent) = target.parent() {
                std::fs::write(
                    parent.join("meta.json"),
                    serde_json::to_string_pretty(&meta).unwrap_or_else(|_| "{}".into()),
                )?;
            }
            installed.push(skill.name);
        }
        let _ = std::fs::remove_dir_all(&tmp);
        Ok(InstallReport {
            installed,
            origin: slug,
            version,
        })
    }

    async fn search(&self, query: &str) -> Vec<SkillHit> {
        let mut hits = self.search_repositories(query).await;
        hits.extend(self.search_code(query).await);
        let mut seen = std::collections::HashSet::new();
        hits.retain(|hit| seen.insert(hit.url.clone()));
        hits
    }
}

fn github_token() -> Option<String> {
    std::env::var("GITHUB_TOKEN")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("GH_TOKEN").ok().filter(|s| !s.is_empty()))
}

fn safe_origin_dir(spec: &str) -> String {
    let trimmed = spec
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_start_matches("file://")
        .trim_start_matches("git@");
    trimmed
        .replace(['/', '\\', ':'], "__")
        .trim_matches('_')
        .to_string()
        .replace([' ', '#', '?', '%'], "_")
}

fn is_raw_url(spec: &str) -> bool {
    let lower = spec.trim().to_lowercase();
    lower.contains("raw.githubusercontent.com") || lower.ends_with(".md")
}

fn normalize_spec(spec: &str) -> String {
    let spec = spec.trim();
    if spec.starts_with("https://github.com/") || spec.starts_with("git@github.com:") {
        spec.to_string()
    } else if spec.contains('/') && !spec.contains(':') {
        format!("https://github.com/{spec}")
    } else {
        spec.to_string()
    }
}

fn collect_skill_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in walkdir::WalkDir::new(root).follow_links(false) {
        let Ok(entry) = entry else { continue };
        if entry.file_type().is_file() && entry.file_name().to_string_lossy() == "SKILL.md" {
            if let Ok(text) = std::fs::read_to_string(entry.path()) {
                if parse_frontmatter(&text).is_ok() {
                    out.push(entry.path().to_path_buf());
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn remote_source_installs_from_local_git_and_writes_meta() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("repo");
        std::fs::create_dir_all(src.join("skills").join("git.basics")).unwrap();
        std::fs::write(
            src.join("skills").join("git.basics").join("SKILL.md"),
            "---\nname: git-discipline\ndescription: commit discipline\ncategory: git\n---\nDo it.",
        )
        .unwrap();
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "test@example.com"],
            vec!["config", "user.name", "test"],
        ] {
            assert!(Command::new("git")
                .args(&args)
                .current_dir(&src)
                .status()
                .unwrap()
                .success());
        }
        assert!(Command::new("git")
            .args(["add", "."])
            .current_dir(&src)
            .status()
            .unwrap()
            .success());
        assert!(Command::new("git")
            .args(["commit", "-qm", "seed"])
            .current_dir(&src)
            .status()
            .unwrap()
            .success());
        let spec = url::Url::from_file_path(&src).unwrap().to_string();
        let dest = dir.path().join("dest");
        let report = RemoteSource::install(&RemoteSource, &spec, &dest)
            .await
            .unwrap();
        assert_eq!(report.installed, vec!["git-discipline"]);
        let files = collect_skill_files(&dest);
        assert_eq!(files.len(), 1);
        let skill = Skill::from_path(&files[0]).unwrap();
        assert_eq!(skill.origin, "installed");
        assert_eq!(skill.origin_url.as_deref(), Some(spec.as_str()));
        assert!(files[0].parent().unwrap().join("meta.json").exists());
    }

    #[tokio::test]
    async fn local_source_installs_validated_skills() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(src.join("git.basics")).unwrap();
        std::fs::write(
            src.join("git.basics").join("SKILL.md"),
            "---\nname: git-discipline\ndescription: commit discipline\ncategory: git\n---\nDo it.",
        )
        .unwrap();
        let dest = dir.path().join("dest");
        let source = LocalSource::new(src);
        let report = source
            .install(dir.path().join("src").to_str().unwrap(), &dest)
            .await
            .unwrap();
        assert_eq!(report.installed, vec!["git-discipline"]);
        assert!(dest.join("git-discipline").join("SKILL.md").exists());
    }

    #[test]
    fn detects_raw_skill_urls() {
        assert!(is_raw_url(
            "https://raw.githubusercontent.com/o/r/main/skills/x/SKILL.md"
        ));
        assert!(is_raw_url("https://example.com/path/SKILL.md"));
        assert!(!is_raw_url("owner/repo"));
        assert!(!is_raw_url("https://github.com/owner/repo"));
    }

    #[tokio::test]
    async fn remote_source_installs_raw_skill_over_http() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::thread;

        let body =
            "---\nname: raw-skill\ndescription: fetched from raw url\ncategory: raw\n---\nUse it.";
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            for stream in listener.incoming().take(1) {
                let mut stream = stream.unwrap();
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).unwrap();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/markdown\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream.write_all(response.as_bytes()).unwrap();
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("dest");
        let spec = format!("http://{addr}/SKILL.md");
        let report = RemoteSource::install(&RemoteSource, &spec, &dest)
            .await
            .unwrap();
        assert_eq!(report.installed, vec!["raw-skill"]);
        let files = collect_skill_files(&dest);
        assert_eq!(files.len(), 1);
        let skill = Skill::from_path(&files[0]).unwrap();
        assert_eq!(skill.origin, "installed");
        assert_eq!(skill.origin_url.as_deref(), Some(spec.as_str()));
        assert!(files[0].parent().unwrap().join("meta.json").exists());
        server.join().unwrap();
    }
}
