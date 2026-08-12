use rc_pro::ProviderConfig;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileKind {
    #[serde(rename = "openai")]
    OpenAI,
    #[serde(rename = "anthropic")]
    Anthropic,
    #[serde(rename = "openai-compat")]
    OpenAiCompat,
    #[serde(rename = "ollama")]
    Ollama,
    #[serde(rename = "mock")]
    Mock,
}

impl ProfileKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::OpenAI => "openai",
            Self::Anthropic => "anthropic",
            Self::OpenAiCompat => "openai-compat",
            Self::Ollama => "ollama",
            Self::Mock => "mock",
        }
    }

    // 有意的关联函数(非 FromStr trait 方法):TOML 反序列化场景调用,返回值是
    // 映射默认的 OpenAiCompat(与 serde 的 rename 语义一致),不适合实现 trait。
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        match s {
            "openai" => Self::OpenAI,
            "anthropic" => Self::Anthropic,
            "openai-compat" | "openai_compat" => Self::OpenAiCompat,
            "ollama" => Self::Ollama,
            "mock" => Self::Mock,
            _ => Self::OpenAiCompat,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub id: String,
    pub name: String,
    #[serde(default = "default_app")]
    pub app: String,
    pub kind: ProfileKind,
    #[serde(default)]
    pub base_url: String,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key_file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embedding_model: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub extra: Value,
}

fn default_app() -> String {
    "raincode".to_string()
}

impl Profile {
    pub fn to_provider_config(&self) -> ProviderConfig {
        ProviderConfig {
            kind: self.kind.as_str().to_string(),
            base_url: self.base_url.clone(),
            model: self.model.clone(),
            api_key: self.api_key.clone(),
            api_key_env: self.api_key_env.clone(),
            embedding_model: self.embedding_model.clone(),
            headers: self.headers.clone(),
            extra: self.extra.clone(),
        }
    }

    /// Resolve the effective API key without ever printing it: inline key,
    /// then the protected key file, then the environment variable.
    pub fn resolved_api_key(&self) -> Result<Option<String>, std::io::Error> {
        if let Some(key) = &self.api_key {
            if !key.is_empty() {
                return Ok(Some(key.clone()));
            }
        }
        if let Some(reference) = &self.api_key_file {
            let path = if Path::new(reference).is_absolute() {
                PathBuf::from(reference)
            } else {
                crate::secrets::home_dir().join(reference)
            };
            if let Ok(text) = std::fs::read_to_string(path) {
                let key = text.trim().to_string();
                if !key.is_empty() {
                    return Ok(Some(key));
                }
            }
        }
        if let Some(env) = &self.api_key_env {
            if let Ok(key) = std::env::var(env) {
                if !key.is_empty() {
                    return Ok(Some(key));
                }
            }
        }
        Ok(None)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Registry {
    #[serde(default)]
    pub profiles: Vec<Profile>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_id: Option<String>,
}

impl Registry {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, RegistryError> {
        let path = path.as_ref();
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path).map_err(RegistryError::Io)?;
        toml::from_str(&text).map_err(RegistryError::TomlDe)
    }

    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), RegistryError> {
        if let Some(parent) = path.as_ref().parent() {
            std::fs::create_dir_all(parent).map_err(RegistryError::Io)?;
        }
        let text = toml::to_string_pretty(self).map_err(RegistryError::TomlSer)?;
        std::fs::write(path.as_ref(), text).map_err(RegistryError::Io)
    }

    pub fn active(&self) -> Option<&Profile> {
        self.active_id
            .as_ref()
            .and_then(|id| self.profiles.iter().find(|p| &p.id == id))
    }

    pub fn get(&self, id: &str) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.id == id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut Profile> {
        self.profiles.iter_mut().find(|p| p.id == id)
    }

    pub fn set_active(&mut self, id: &str) -> Result<(), RegistryError> {
        if self.get(id).is_none() {
            return Err(RegistryError::NotFound(id.to_string()));
        }
        self.active_id = Some(id.to_string());
        Ok(())
    }

    pub fn add(&mut self, profile: Profile) {
        self.profiles.retain(|p| p.id != profile.id);
        self.profiles.push(profile);
    }

    pub fn remove(&mut self, id: &str) {
        self.profiles.retain(|p| p.id != id);
        if self.active_id.as_deref() == Some(id) {
            self.active_id = None;
        }
    }

    pub fn ensure_default(&mut self) {
        if self.profiles.is_empty() {
            self.profiles.push(Profile {
                id: "default".into(),
                name: "default".into(),
                app: "raincode".into(),
                kind: ProfileKind::Mock,
                base_url: String::new(),
                model: "mock-1".into(),
                api_key: None,
                api_key_env: None,
                api_key_file: None,
                embedding_model: None,
                headers: BTreeMap::new(),
                extra: serde_json::json!({}),
            });
            self.active_id = Some("default".into());
        } else if self.active_id.is_none() {
            self.active_id = self.profiles.first().map(|p| p.id.clone());
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("toml parse error: {0}")]
    TomlDe(#[from] toml::de::Error),
    #[error("toml serialize error: {0}")]
    TomlSer(#[from] toml::ser::Error),
    #[error("profile not found: {0}")]
    NotFound(String),
}

pub fn default_registry_path() -> PathBuf {
    crate::secrets::home_dir().join("profiles.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_kind_roundtrips_through_toml() {
        let profile = Profile {
            id: "p".into(),
            name: "p".into(),
            app: "raincode".into(),
            kind: ProfileKind::OpenAI,
            base_url: "https://api.openai.com/v1".into(),
            model: "gpt-4o-mini".into(),
            api_key: None,
            api_key_env: Some("OPENAI_API_KEY".into()),
            api_key_file: None,
            embedding_model: None,
            headers: Default::default(),
            extra: serde_json::json!({}),
        };
        let text = toml::to_string(&profile).unwrap();
        assert!(text.contains("kind = \"openai\""));
        let back: Profile = toml::from_str(&text).unwrap();
        assert_eq!(back.kind, ProfileKind::OpenAI);
        assert_eq!(back.kind.as_str(), "openai");
    }
}
