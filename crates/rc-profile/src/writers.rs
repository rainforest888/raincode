//! Writers that push a Raincode profile into each target CLI's own config.
//!
//! This is the layer that lets Raincode act as a drop-in replacement for
//! cc-switch: one profile can be written into Claude Code, Codex, Gemini
//! CLI, Grok, Hermes and opencode with a single command.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use toml_edit::{DocumentMut, Item, Table};

use crate::model::Profile;

#[derive(Debug, thiserror::Error)]
pub enum WriterError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("toml parse error: {0}")]
    Toml(String),
    #[error("config shape error: {0}")]
    Shape(String),
}

pub trait TargetConfigWriter {
    fn app(&self) -> &str;
    fn apply(&self, profile: &Profile) -> Result<(), WriterError>;
}

pub fn all_writers() -> Vec<Box<dyn TargetConfigWriter>> {
    vec![
        Box::new(ClaudeCodeWriter),
        Box::new(CodexWriter),
        Box::new(GeminiWriter),
        Box::new(SimpleJsonWriter {
            app: "grok",
            path: home_dir().join(".grok").join("config.json"),
        }),
        Box::new(SimpleJsonWriter {
            app: "hermes",
            path: home_dir().join(".hermes").join("config.json"),
        }),
        Box::new(OpencodeWriter),
    ]
}

fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

struct ClaudeCodeWriter;

impl TargetConfigWriter for ClaudeCodeWriter {
    fn app(&self) -> &str {
        "claude"
    }

    fn apply(&self, profile: &Profile) -> Result<(), WriterError> {
        let path = home_dir().join(".claude").join("settings.json");
        let mut root = read_json_map(&path)?;
        let env = root
            .entry("env".to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        let env_map = env
            .as_object_mut()
            .ok_or_else(|| WriterError::Shape("settings.env must be an object".into()))?;
        if !profile.base_url.is_empty() {
            env_map.insert(
                "ANTHROPIC_BASE_URL".into(),
                Value::String(profile.base_url.clone()),
            );
        }
        if let Some(key) = profile.resolved_api_key()? {
            env_map.insert("ANTHROPIC_AUTH_TOKEN".into(), Value::String(key.clone()));
            env_map.insert("ANTHROPIC_API_KEY".into(), Value::String(key.clone()));
        }
        if !profile.model.is_empty() {
            env_map.insert(
                "ANTHROPIC_MODEL".into(),
                Value::String(profile.model.clone()),
            );
            env_map.insert(
                "ANTHROPIC_SMALL_FAST_MODEL".into(),
                Value::String(profile.model.clone()),
            );
        }
        write_json(&path, &root)
    }
}

struct CodexWriter;

impl TargetConfigWriter for CodexWriter {
    fn app(&self) -> &str {
        "codex"
    }

    fn apply(&self, profile: &Profile) -> Result<(), WriterError> {
        let config_path = home_dir().join(".codex").join("config.toml");
        let text = if config_path.exists() {
            fs::read_to_string(&config_path)?
        } else {
            String::new()
        };
        let mut doc: DocumentMut = text
            .parse()
            .map_err(|e| WriterError::Toml(format!("{e}")))?;
        let provider_id = format!("raincode-{}", profile.id);
        let model = if profile.model.is_empty() {
            "gpt-4o".to_string()
        } else {
            profile.model.clone()
        };

        doc["model"] = toml_edit::value(model.clone());
        doc["model_provider"] = toml_edit::value(provider_id.clone());
        if doc.get("model_providers").is_none() {
            doc["model_providers"] = Item::Table(Table::new());
        }
        let providers = doc
            .get_mut("model_providers")
            .and_then(Item::as_table_mut)
            .ok_or_else(|| WriterError::Shape("model_providers must be a table".into()))?;

        let mut provider_table = Table::new();
        provider_table.insert("name", toml_edit::value(profile.name.clone()));
        if !profile.base_url.is_empty() {
            provider_table.insert("base_url", toml_edit::value(profile.base_url.clone()));
        }
        provider_table.insert("wire_api", toml_edit::value("chat"));
        let env_key = "RAINCODE_API_KEY".to_string();
        if let Some(api_key) = profile.resolved_api_key()? {
            provider_table.insert("env_key", toml_edit::value(env_key.clone()));
            let auth_path = home_dir().join(".codex").join("auth.json");
            let mut auth = read_json_map(&auth_path)?;
            auth.insert(env_key, Value::String(api_key.clone()));
            write_json(&auth_path, &auth)?;
        } else if let Some(env) = &profile.api_key_env {
            provider_table.insert("env_key", toml_edit::value(env.clone()));
        }
        providers.insert(&provider_id, Item::Table(provider_table));

        if let Some(parent) = config_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&config_path, doc.to_string())?;
        Ok(())
    }
}

struct GeminiWriter;

impl TargetConfigWriter for GeminiWriter {
    fn app(&self) -> &str {
        "gemini"
    }

    fn apply(&self, profile: &Profile) -> Result<(), WriterError> {
        let path = home_dir().join(".gemini").join(".env");
        let mut entries = Vec::new();
        if !profile.base_url.is_empty() {
            entries.push(("GEMINI_BASE_URL".to_string(), profile.base_url.clone()));
        }
        // 解析顺序:字面 key(api_key/api_key_file)优先;只有没字面 key 时才写 env 引用。
        // 若两者都写会得到两行 GEMINI_API_KEY,env loader 取最后一行 → 破坏配置。
        if let Some(key) = profile.resolved_api_key()? {
            entries.push(("GEMINI_API_KEY".to_string(), key.clone()));
        } else if let Some(env) = &profile.api_key_env {
            entries.push(("GEMINI_API_KEY".to_string(), format!("${{{env}}}")));
        }
        if !profile.model.is_empty() {
            entries.push(("GEMINI_MODEL".to_string(), profile.model.clone()));
        }
        write_env_file(&path, &entries)
    }
}

struct SimpleJsonWriter {
    app: &'static str,
    path: PathBuf,
}

impl TargetConfigWriter for SimpleJsonWriter {
    fn app(&self) -> &str {
        self.app
    }

    fn apply(&self, profile: &Profile) -> Result<(), WriterError> {
        let mut root = read_json_map(&self.path)?;
        if let Some(key) = profile.resolved_api_key()? {
            root.insert("api_key".into(), Value::String(key.clone()));
        }
        if !profile.base_url.is_empty() {
            root.insert("base_url".into(), Value::String(profile.base_url.clone()));
        }
        if !profile.model.is_empty() {
            root.insert("model".into(), Value::String(profile.model.clone()));
        }
        write_json(&self.path, &root)
    }
}

struct OpencodeWriter;

impl TargetConfigWriter for OpencodeWriter {
    fn app(&self) -> &str {
        "opencode"
    }

    fn apply(&self, profile: &Profile) -> Result<(), WriterError> {
        let path = home_dir()
            .join(".config")
            .join("opencode")
            .join("opencode.json");
        let mut root = read_json_map(&path)?;
        let provider_name = format!("raincode-{}", profile.id);
        let provider = root
            .entry("provider".to_string())
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut()
            .cloned()
            .unwrap_or_default();
        let mut provider_map = provider;
        let mut options = Map::new();
        if !profile.base_url.is_empty() {
            options.insert("baseURL".into(), Value::String(profile.base_url.clone()));
        }
        if let Some(key) = profile.resolved_api_key()? {
            options.insert("apiKey".into(), Value::String(key.clone()));
        }
        let mut model_map = Map::new();
        model_map.insert(profile.model.clone(), Value::Object(Map::new()));
        let mut provider_entry = Map::new();
        provider_entry.insert(
            "npm".into(),
            Value::String("@ai-sdk/openai-compatible".into()),
        );
        provider_entry.insert("options".into(), Value::Object(options));
        provider_entry.insert("models".into(), Value::Object(model_map));
        provider_map.insert(provider_name.clone(), Value::Object(provider_entry));
        root.insert("provider".into(), Value::Object(provider_map));
        root.insert(
            "model".into(),
            Value::String(format!("{provider_name}/{}", profile.model)),
        );
        write_json(&path, &root)
    }
}

/// 读 JSON 配置;文件不存在 → 空 map。**解析失败必须报错**,不能静默返回空 map:
/// 否则调用方用空 map 重写会把用户已有配置整份抹掉(静默数据丢失)。
fn read_json_map(path: &Path) -> Result<Map<String, Value>, WriterError> {
    if !path.exists() {
        return Ok(Map::new());
    }
    let text = fs::read_to_string(path)?;
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Err(WriterError::Shape(format!(
            "{} is not a JSON object",
            path.display()
        ))),
        Err(e) => Err(WriterError::Shape(format!(
            "failed to parse {}: {e}",
            path.display()
        ))),
    }
}

fn write_json(path: &Path, value: &Map<String, Value>) -> Result<(), WriterError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(&Value::Object(value.clone()))?;
    fs::write(path, text)?;
    Ok(())
}

fn write_env_file(path: &Path, entries: &[(String, String)]) -> Result<(), WriterError> {
    let existing = if path.exists() {
        fs::read_to_string(path)?
    } else {
        String::new()
    };
    let keys: Vec<String> = entries.iter().map(|(key, _)| key.clone()).collect();
    let mut lines: Vec<String> = existing
        .lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            !keys
                .iter()
                .any(|key| trimmed.starts_with(&format!("{key}=")))
        })
        .map(str::to_string)
        .collect();
    if !lines.is_empty() && !lines.last().map(String::is_empty).unwrap_or(false) {
        lines.push(String::new());
    }
    for (key, value) in entries {
        lines.push(format!("{key}={value}"));
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, lines.join("\n"))?;
    Ok(())
}
