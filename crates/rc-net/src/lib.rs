//! Web fetch/search utilities for Raincode. Every request is checked against
//! the sandbox `NetworkPolicy` before any bytes cross the wire.

pub mod tools;

use rc_sandbox::{NetworkDecision, NetworkPolicy};
use regex::Regex;
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, thiserror::Error)]
pub enum NetError {
    #[error("network policy denied request: {0}")]
    Policy(String),
    #[error("http error: {0}")]
    Http(String),
    #[error("transport error: {0}")]
    Transport(String),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid url: {0}")]
    Url(String),
    #[error("search failed: {0}")]
    Search(String),
}

/// Cap on how many characters of fetched page text are handed back to the
/// caller, so large pages don't flood the agent context with tokens.
const MAX_FETCH_CHARS: usize = 20_000;

#[derive(Debug, Clone)]
pub struct FetchResult {
    pub url: String,
    pub title: Option<String>,
    pub markdown: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SearchHit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct SearchConfig {
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default = "default_max_results")]
    pub max_results: usize,
}

fn default_max_results() -> usize {
    8
}

/// Fetch a URL and return a plain-text/markdown-ish representation. HTML is
/// stripped to text so provider contexts stay compact. Output longer than
/// [`MAX_FETCH_CHARS`] characters is truncated with a `...[truncated: N chars]`
/// marker.
pub async fn fetch_url(url: &str, policy: &NetworkPolicy) -> Result<FetchResult, NetError> {
    check_policy(policy, url)?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| NetError::Transport(e.to_string()))?;
    let response = client
        .get(url)
        .header("User-Agent", "raincode/0.1")
        .send()
        .await
        .map_err(|e| NetError::Transport(e.to_string()))?;
    if !response.status().is_success() {
        return Err(NetError::Http(format!(
            "{} returned {}",
            url,
            response.status()
        )));
    }
    let body = response
        .text()
        .await
        .map_err(|e| NetError::Transport(e.to_string()))?;
    let title = extract_title(&body);
    let markdown = truncate_text(html_to_text(&body), MAX_FETCH_CHARS);
    Ok(FetchResult {
        url: url.to_string(),
        title,
        markdown,
    })
}

/// Keep the first `max` bytes of `text`; if anything was cut off, append
/// a marker reporting how many characters were dropped. The cut never splits
/// a UTF-8 character in half.
fn truncate_text(text: String, max: usize) -> String {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let removed = text[end..].chars().count();
    format!("{}...[truncated: {removed} chars]", &text[..end])
}

/// Search the web. When `config.endpoint` is set it is called as an
/// OpenAI-style JSON search endpoint; otherwise a DuckDuckGo HTML search is
/// used as a keyless fallback.
pub async fn search(
    query: &str,
    policy: &NetworkPolicy,
    config: &SearchConfig,
) -> Result<Vec<SearchHit>, NetError> {
    if query.trim().is_empty() {
        return Err(NetError::Search("empty query".into()));
    }
    match &config.endpoint {
        Some(endpoint) => search_json(query, policy, endpoint, config.api_key.as_deref()).await,
        None => search_duckduckgo(query, policy).await,
    }
    .map(|hits| {
        let mut hits = hits;
        if hits.len() > config.max_results {
            hits.truncate(config.max_results);
        }
        hits
    })
}

fn check_policy(policy: &NetworkPolicy, url: &str) -> Result<(), NetError> {
    match policy.check(url) {
        NetworkDecision::Allowed => Ok(()),
        NetworkDecision::Denied { reason } => Err(NetError::Policy(reason)),
    }
}

async fn search_duckduckgo(
    query: &str,
    policy: &NetworkPolicy,
) -> Result<Vec<SearchHit>, NetError> {
    let url = format!(
        "https://html.duckduckgo.com/html/?q={}",
        url::form_urlencoded::byte_serialize(query.as_bytes()).collect::<String>()
    );
    check_policy(policy, &url)?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|e| NetError::Transport(e.to_string()))?;
    let body = client
        .get(&url)
        .header("User-Agent", "raincode/0.1")
        .send()
        .await
        .map_err(|e| NetError::Transport(e.to_string()))?
        .text()
        .await
        .map_err(|e| NetError::Transport(e.to_string()))?;
    Ok(parse_duckduckgo(&body))
}

async fn search_json(
    query: &str,
    policy: &NetworkPolicy,
    endpoint: &str,
    api_key: Option<&str>,
) -> Result<Vec<SearchHit>, NetError> {
    let separator = if endpoint.contains('?') { '&' } else { '?' };
    let url = format!(
        "{endpoint}{separator}q={}",
        url::form_urlencoded::byte_serialize(query.as_bytes()).collect::<String>()
    );
    check_policy(policy, &url)?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| NetError::Transport(e.to_string()))?;
    let mut request = client.get(&url);
    if let Some(key) = api_key {
        request = request.bearer_auth(key);
    }
    let response = request
        .send()
        .await
        .map_err(|e| NetError::Transport(e.to_string()))?;
    if !response.status().is_success() {
        return Err(NetError::Http(format!(
            "{url} returned {}",
            response.status()
        )));
    }
    let value: Value = response
        .json()
        .await
        .map_err(|e| NetError::Transport(e.to_string()))?;
    Ok(parse_json_results(&value))
}

fn parse_duckduckgo(html: &str) -> Vec<SearchHit> {
    let link_re =
        Regex::new(r#"(?is)<a[^>]+class="[^"]*result__a[^"]*"[^>]*href="([^"]+)"[^>]*>(.*?)</a>"#)
            .unwrap();
    let snippet_re =
        Regex::new(r#"(?is)<a[^>]+class="[^"]*result__snippet[^"]*"[^>]*>(.*?)</a>"#).unwrap();
    let titles: Vec<String> = link_re
        .captures_iter(html)
        .map(|cap| html_to_text(&cap[2]))
        .filter(|s| !s.is_empty())
        .collect();
    let links: Vec<String> = link_re
        .captures_iter(html)
        .map(|cap| normalize_link(&cap[1]))
        .collect();
    let snippets: Vec<String> = snippet_re
        .captures_iter(html)
        .map(|cap| html_to_text(&cap[1]))
        .collect();
    links
        .into_iter()
        .zip(titles)
        .enumerate()
        .map(|(i, (url, title))| SearchHit {
            title,
            url,
            snippet: snippets.get(i).cloned().unwrap_or_default(),
        })
        .collect()
}

fn normalize_link(href: &str) -> String {
    let absolute = if href.starts_with("//") {
        format!("https:{href}")
    } else {
        href.to_string()
    };
    if let Ok(parsed) = url::Url::parse(&absolute) {
        if let Some((_, target)) = parsed.query_pairs().find(|(key, _)| key == "uddg") {
            return target.into_owned();
        }
        if parsed.scheme() == "http" || parsed.scheme() == "https" {
            return parsed.to_string();
        }
        if href.starts_with("//") {
            return format!("https:{href}");
        }
    }
    if href.starts_with("//") {
        return format!("https:{href}");
    }
    href.to_string()
}

fn parse_json_results(value: &Value) -> Vec<SearchHit> {
    let candidates = [
        value.get("items"),
        value.get("results"),
        value.get("organic"),
        value.get("web").and_then(|web| web.get("results")),
        value.get("data"),
        Some(value),
    ];
    let mut out = Vec::new();
    for candidate in candidates.into_iter().flatten() {
        if let Value::Array(items) = candidate {
            for item in items {
                let obj = item.as_object();
                let title = obj
                    .and_then(|o| o.get("title"))
                    .or_else(|| obj.and_then(|o| o.get("name")))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let url = obj
                    .and_then(|o| o.get("url"))
                    .or_else(|| obj.and_then(|o| o.get("link")))
                    .or_else(|| obj.and_then(|o| o.get("href")))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let snippet = obj
                    .and_then(|o| o.get("snippet"))
                    .or_else(|| obj.and_then(|o| o.get("description")))
                    .or_else(|| obj.and_then(|o| o.get("content")))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if !url.is_empty() {
                    out.push(SearchHit {
                        title,
                        url,
                        snippet,
                    });
                }
            }
            if !out.is_empty() {
                break;
            }
        }
    }
    out
}

fn extract_title(html: &str) -> Option<String> {
    let re = Regex::new(r"(?is)<title[^>]*>(.*?)</title>").ok()?;
    re.captures(html).map(|cap| html_to_text(&cap[1]))
}

pub fn html_to_text(html: &str) -> String {
    // Rust 的 regex 不支持 backreference(\1),无法写 `<(script|...)>.*?</\1>`。
    // 拆成三个独立 regex 逐个剥离 script/style/noscript 块,避免非法 regex。
    let script_re = Regex::new(r"(?is)<script[^>]*>.*?</script>");
    let style_re = Regex::new(r"(?is)<style[^>]*>.*?</style>");
    let noscript_re = Regex::new(r"(?is)<noscript[^>]*>.*?</noscript>");
    let mut stripped = html.to_string();
    if let Ok(re) = &script_re {
        stripped = re.replace_all(&stripped, " ").into_owned();
    }
    if let Ok(re) = &style_re {
        stripped = re.replace_all(&stripped, " ").into_owned();
    }
    if let Ok(re) = &noscript_re {
        stripped = re.replace_all(&stripped, " ").into_owned();
    }
    let stripped = Regex::new(r"(?is)<[^>]+>")
        .map(|re| re.replace_all(&stripped, " ").into_owned())
        .unwrap_or(stripped);
    let mut text = stripped
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ");
    text = Regex::new(r"\s+")
        .map(|re| re.replace_all(&text, " ").into_owned())
        .unwrap_or(text);
    text.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_html_tags() {
        let html = "<html><head><title>T</title></head><body><p>Hello &amp; bye</p></body></html>";
        assert_eq!(html_to_text(html), "T Hello & bye");
    }

    #[test]
    fn truncates_long_text_with_marker() {
        let out = truncate_text("a".repeat(25_000), MAX_FETCH_CHARS);
        assert_eq!(
            out,
            format!("{}...[truncated: 5000 chars]", "a".repeat(20_000))
        );
    }

    #[test]
    fn leaves_short_text_untouched() {
        let short = "short page".to_string();
        assert_eq!(truncate_text(short, MAX_FETCH_CHARS), "short page");
    }

    #[test]
    fn text_exactly_at_cap_is_not_truncated() {
        let exact = "a".repeat(MAX_FETCH_CHARS);
        assert_eq!(truncate_text(exact.clone(), MAX_FETCH_CHARS), exact);
    }

    #[test]
    fn multibyte_text_exactly_at_cap_is_kept_whole() {
        // 6666 个"界"恰好 19998 字节,补 2 个 ASCII 字节正好等于 20000 字节上限,
        // 且上限落在 ASCII 字节上,无需回退、不应截断。
        let mut exact = "界".repeat(6666);
        exact.push_str("ab");
        assert_eq!(exact.len(), MAX_FETCH_CHARS);
        assert_eq!(truncate_text(exact.clone(), MAX_FETCH_CHARS), exact);
    }

    #[test]
    fn empty_text_stays_empty() {
        assert_eq!(truncate_text(String::new(), MAX_FETCH_CHARS), "");
        assert_eq!(truncate_text(String::new(), 0), "");
    }

    #[test]
    fn truncates_even_at_zero_cap() {
        // 上限为 0:保留空串,剩余字符全部计入 marker。
        let out = truncate_text("abc".to_string(), 0);
        assert_eq!(out, "...[truncated: 3 chars]");
    }

    #[test]
    fn truncation_respects_byte_cap_and_reports_chars() {
        // 混合字节宽度的文本:上限按字节截断,marker 报告的是被丢弃的字符数。
        // 19998 字节处开始"界"(3 字节),20000 上限落在"界"中间 → 回退到 19998。
        let mixed = format!("{}界{}", "a".repeat(19_998), "b".repeat(20));
        let out = truncate_text(mixed.clone(), MAX_FETCH_CHARS);
        assert!(out.starts_with(&"a".repeat(19_998)));
        assert!(out.ends_with("...[truncated: 21 chars]"));
        assert!(!out.contains('界'));
        assert!(!out.contains('b'));
    }

    #[test]
    fn truncates_on_utf8_char_boundary() {
        // Each 界 is 3 bytes; a 20000-byte boundary would split one in half,
        // so the cut must land at 19998 bytes (6666 chars), dropping 14334.
        let long = "界".repeat(21_000);
        let out = truncate_text(long, MAX_FETCH_CHARS);
        assert_eq!(
            out,
            format!("{}...[truncated: 14334 chars]", "界".repeat(6666))
        );
    }

    #[test]
    fn parses_duckduckgo_links() {
        let html = r##"<a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fdoc">Example</a><a class="result__snippet" href="#">Some text</a>"##;
        let hits = parse_duckduckgo(html);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].url, "https://example.com/doc");
        assert_eq!(hits[0].title, "Example");
        assert_eq!(hits[0].snippet, "Some text");
    }

    #[tokio::test]
    async fn fetch_url_is_blocked_by_policy_before_network() {
        use rc_sandbox::PolicyDefault;
        let policy = NetworkPolicy {
            allow_hosts: vec![],
            deny_hosts: vec![],
            default: PolicyDefault::Deny,
        };
        let err = fetch_url("https://example.com/", &policy)
            .await
            .unwrap_err();
        assert!(matches!(err, NetError::Policy(_)));
    }

    #[tokio::test]
    async fn search_rejects_empty_query() {
        let policy = NetworkPolicy {
            allow_hosts: vec![],
            deny_hosts: vec![],
            default: rc_sandbox::PolicyDefault::Deny,
        };
        let err = search("", &policy, &SearchConfig::default())
            .await
            .unwrap_err();
        assert!(matches!(err, NetError::Search(_)));
    }
}
