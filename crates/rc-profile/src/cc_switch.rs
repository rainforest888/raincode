//! CC-Switch compatibility layer.
//!
//! Raincode can import provider profiles from a local cc-switch sqlite
//! database (`~/.cc-switch/cc-switch.db`) or from `ccswitch://` deep links,
//! then hand the same profiles to Raincode's registry and target writers.

use std::path::Path;

use rusqlite::{types::Value as SqlValue, Connection};
use serde_json::{Map, Value};

use crate::model::{Profile, ProfileKind};

/// A provider profile imported from cc-switch. It is intentionally not a
/// `Profile` yet: import may need a disambiguation step before it is added
/// to Raincode's own registry.
#[derive(Debug, Clone)]
pub struct ProfileImport {
    pub name: String,
    pub app: String,
    pub kind: ProfileKind,
    pub base_url: String,
    pub model: String,
    pub api_key: Option<String>,
    pub api_key_env: Option<String>,
    pub extra: Value,
    pub source: String,
}

impl ProfileImport {
    pub fn to_profile(&self, id: String) -> Profile {
        Profile {
            id,
            name: self.name.clone(),
            app: self.app.clone(),
            kind: self.kind,
            base_url: self.base_url.clone(),
            model: self.model.clone(),
            api_key: self.api_key.clone(),
            api_key_env: self.api_key_env.clone(),
            api_key_file: None,
            embedding_model: None,
            headers: Default::default(),
            extra: self.extra.clone(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CcSwitchError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("cc-switch database not found: {0}")]
    Missing(String),
    #[error("database has no usable provider rows")]
    NoRows,
    #[error("invalid deep link: {0}")]
    BadLink(String),
}

/// Parse a `ccswitch://provider?...` deep link into an importable profile.
///
/// Supported query keys: `app`, `api_key`, `base_url`, `model`, `provider`
/// and `type`. Unknown keys are preserved in `extra`.
pub fn parse_deeplink(input: &str) -> Option<ProfileImport> {
    let input = input.trim();
    let url = url::Url::parse(input).ok()?;
    if url.scheme() != "ccswitch" {
        return None;
    }
    let mut params = Map::new();
    for (key, value) in url.query_pairs() {
        params.insert(key.into_owned(), Value::String(value.into_owned()));
    }
    if params.get("api_key").is_none() && params.get("base_url").is_none() {
        return None;
    }
    let app = string_param(&params, &["app", "target"]).unwrap_or_else(|| "raincode".into());
    let provider = string_param(&params, &["provider", "type", "kind"]);
    let base_url = string_param(&params, &["base_url", "baseUrl", "endpoint"]).unwrap_or_default();
    let model = string_param(&params, &["model", "model_name"]).unwrap_or_default();
    let kind = provider
        .as_deref()
        .map(|raw| kind_from_str(raw, &base_url))
        .unwrap_or_else(|| kind_from_base_url(&base_url));
    let name = string_param(&params, &["name", "profile"]).unwrap_or_else(|| {
        provider
            .clone()
            .unwrap_or_else(|| kind.as_str().to_string())
    });
    let mut extra = Map::new();
    for (key, value) in &params {
        if !matches!(
            key.as_str(),
            "app"
                | "target"
                | "provider"
                | "type"
                | "kind"
                | "base_url"
                | "baseUrl"
                | "endpoint"
                | "model"
                | "model_name"
                | "api_key"
                | "apiKey"
                | "name"
                | "profile"
        ) {
            extra.insert(key.clone(), value.clone());
        }
    }
    Some(ProfileImport {
        name,
        app,
        kind,
        base_url,
        model,
        api_key: string_param(&params, &["api_key", "apiKey"]),
        api_key_env: None,
        // deeplink 参数里也可能带 key/token/secret,一并剥离防落盘。
        extra: strip_secret_fields(Value::Object(extra)),
        source: format!("deeplink:{input}"),
    })
}

/// Import all provider rows from a cc-switch sqlite database.
pub fn import_from_db(path: impl AsRef<Path>) -> Result<Vec<ProfileImport>, CcSwitchError> {
    let path = path.as_ref();
    if !path.exists() {
        return Err(CcSwitchError::Missing(path.display().to_string()));
    }
    let conn = Connection::open(path)?;
    let mut tables = Vec::new();
    {
        let mut stmt = conn.prepare("SELECT name FROM sqlite_master WHERE type = 'table'")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let name: String = row.get(0)?;
            tables.push(name);
        }
    }
    let lower: Vec<String> = tables.iter().map(|t| t.to_lowercase()).collect();
    let mut imports = Vec::new();
    if let Some(index) = lower.iter().position(|t| t == "providers") {
        imports.extend(import_rows(&conn, &tables[index])?);
    }
    if let Some(index) = lower.iter().position(|t| t == "profiles") {
        imports.extend(import_rows(&conn, &tables[index])?);
    }
    if imports.is_empty() {
        return Err(CcSwitchError::NoRows);
    }
    Ok(imports)
}

fn import_rows(conn: &Connection, table: &str) -> Result<Vec<ProfileImport>, CcSwitchError> {
    let quoted = table.replace('"', "\"\"");
    let sql = format!("SELECT * FROM \"{quoted}\"");
    let mut stmt = conn.prepare(&sql)?;
    let columns: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let mut rows = stmt.query([])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let mut obj = Map::new();
        for (index, name) in columns.iter().enumerate() {
            let value: SqlValue = row.get(index)?;
            obj.insert(name.clone(), sql_to_json(value));
        }
        if let Some(import) = row_to_import(&Value::Object(obj), table) {
            out.push(import);
        }
    }
    Ok(out)
}

fn sql_to_json(value: SqlValue) -> Value {
    match value {
        SqlValue::Null => Value::Null,
        SqlValue::Integer(n) => Value::from(n),
        SqlValue::Real(n) => Value::from(n),
        SqlValue::Text(s) => Value::String(s),
        SqlValue::Blob(bytes) => Value::Array(bytes.into_iter().map(Value::from).collect()),
    }
}

fn row_to_import(row: &Value, table: &str) -> Option<ProfileImport> {
    let obj = row.as_object()?;
    let mut merged = obj.clone();
    for key in ["settings_config", "settings", "config", "config_json"] {
        let value = match obj.get(key) {
            Some(v) => v,
            None => continue,
        };
        let nested = match value {
            Value::String(s) => serde_json::from_str::<Value>(s).ok(),
            other => Some(other.clone()),
        };
        if let Some(Value::Object(map)) = nested {
            for (k, v) in map {
                merged.entry(k).or_insert(v);
            }
        }
    }
    let name = string_field(&merged, &["name", "title", "profile_name", "provider_name"])
        .unwrap_or_else(|| "imported".to_string());
    let app = string_field(&merged, &["app", "application", "target", "target_app"])
        .unwrap_or_else(|| "raincode".to_string());
    let base_url = string_field(
        &merged,
        &["base_url", "baseUrl", "baseURL", "url", "endpoint"],
    )
    .unwrap_or_default();
    let provider =
        string_field(&merged, &["provider", "type", "kind", "vendor"]).unwrap_or_default();
    let model = string_field(&merged, &["model", "model_name", "modelId"]).unwrap_or_default();
    let api_key = string_field(&merged, &["api_key", "apiKey", "key", "token", "secret"]);
    let kind = kind_from_str(&provider, &base_url);
    let source = format!("cc-switch:{table}");
    Some(ProfileImport {
        name,
        app,
        kind,
        base_url,
        model,
        api_key,
        api_key_env: None,
        // 不要把整行塞进 extra:cc-switch 的 providers 表含 api_key/key/token/secret 列,
        // 整行克隆会让明文 key 跟着 profiles.toml 落盘(违反「key 只进 ~/.raincode/keys/」)。
        extra: strip_secret_fields(row.clone()),
        source,
    })
}

/// 移除可能含密钥的顶层字段(键名或值形状像 key 的),防止 key 随 extra 序列化落盘。
fn strip_secret_fields(mut row: Value) -> Value {
    if let Some(obj) = row.as_object_mut() {
        let secret_like: Vec<String> = obj
            .iter()
            .filter(|(k, v)| {
                let key = k.to_lowercase();
                key.contains("key")
                    || key.contains("token")
                    || key.contains("secret")
                    || key.contains("apikey")
                    || key.contains("password")
                    || key.contains("credential")
                    || key.contains("authorization")
                    || (matches!(v, Value::String(s) if s.starts_with("sk-")
                        || s.starts_with("Bearer ")
                        || s.len() >= 24))
            })
            .map(|(k, _)| k.clone())
            .collect();
        for k in secret_like {
            obj.remove(&k);
        }
    }
    row
}

fn string_field(map: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| map.get(*key))
        .and_then(|value| match value {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        })
}

fn string_param(map: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    string_field(map, keys)
}

fn kind_from_str(raw: &str, base_url: &str) -> ProfileKind {
    let lower = raw.to_lowercase();
    if lower.contains("anthropic") || lower.contains("claude") {
        ProfileKind::Anthropic
    } else if lower.contains("ollama") {
        ProfileKind::Ollama
    } else if lower.contains("compat") || lower.contains("deepseek") {
        // openai-compatible / deepseek 等非官方 OpenAI 端点走 compat 线
        // (wire format 一致但语义区分),必须先于 "openai" 判断。
        ProfileKind::OpenAiCompat
    } else if lower.contains("openai") || base_url.contains("api.openai.com") {
        ProfileKind::OpenAI
    } else if lower.contains("mock") {
        ProfileKind::Mock
    } else if base_url.contains("api.anthropic.com") {
        ProfileKind::Anthropic
    } else {
        kind_from_base_url(base_url)
    }
}

fn kind_from_base_url(base_url: &str) -> ProfileKind {
    if base_url.contains("api.anthropic.com") {
        ProfileKind::Anthropic
    } else if base_url.contains("localhost:11434") || base_url.contains("127.0.0.1:11434") {
        ProfileKind::Ollama
    } else if base_url.contains("api.openai.com") {
        ProfileKind::OpenAI
    } else {
        ProfileKind::OpenAiCompat
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ccswitch_deeplink() {
        let link = "ccswitch://provider?app=claude&provider=anthropic&api_key=sk-test&base_url=https%3A%2F%2Fapi.anthropic.com&model=claude-sonnet-4";
        let imported = parse_deeplink(link).expect("link should parse");
        assert_eq!(imported.app, "claude");
        assert_eq!(imported.kind, ProfileKind::Anthropic);
        assert_eq!(imported.api_key.as_deref(), Some("sk-test"));
        assert_eq!(imported.model, "claude-sonnet-4");
        assert_eq!(imported.source, format!("deeplink:{link}"));
    }

    #[test]
    fn rejects_non_ccswitch_links() {
        assert!(parse_deeplink("https://example.com/provider?api_key=x").is_none());
    }

    #[test]
    fn infers_openai_compat_kind() {
        let imported = parse_deeplink(
            "ccswitch://provider?base_url=https%3A%2F%2Fapi.deepseek.com&model=deepseek-chat&api_key=k",
        )
        .expect("link should parse");
        assert_eq!(imported.kind, ProfileKind::OpenAiCompat);
    }

    #[test]
    fn extra_never_contains_secret_keys() {
        // deeplink 里带 secret 形状的额外参数:key 不落 extra。
        let imported = parse_deeplink(
            "ccswitch://provider?base_url=https%3A%2F%2Fapi.deepseek.com&model=x&api_key=sk-real&extra_token=sk-leak&secret=x123456789012345678901234",
        )
        .expect("link should parse");
        let extra = imported.extra.as_object().unwrap();
        assert!(!extra.contains_key("extra_token"));
        assert!(!extra.contains_key("secret"));
        assert!(!extra.contains_key("api_key"));
        assert!(!extra.contains_key("key"));
    }

    #[test]
    fn row_extra_strips_secret_columns() {
        // cc-switch providers 表整行:key/token 列不随 extra 落盘。
        let row = serde_json::json!({
            "name": "deepseek",
            "base_url": "https://api.deepseek.com",
            "model": "deepseek-chat",
            "api_key": "sk-real-key",
            "key": "sk-alt-key",
            "token": "tok-123",
            "secret": "s3cret",
            "settings": "{\"temperature\":0.5}"
        });
        let stripped = strip_secret_fields(row);
        let obj = stripped.as_object().unwrap();
        assert!(!obj.contains_key("api_key"));
        assert!(!obj.contains_key("key"));
        assert!(!obj.contains_key("token"));
        assert!(!obj.contains_key("secret"));
        assert_eq!(obj.get("name").unwrap(), "deepseek");
        assert_eq!(obj.get("model").unwrap(), "deepseek-chat");
    }
}
