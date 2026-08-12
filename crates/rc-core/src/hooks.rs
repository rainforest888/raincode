//! Session and tool lifecycle hooks.
//!
//! Hooks are external commands that receive a JSON payload on stdin and
//! `RAINCODE_EVENT` in the environment. Pre-tool hooks may deny a call by
//! exiting non-zero or printing `deny` / `{"decision":"deny"}`.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::Path;
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HooksConfig {
    #[serde(default)]
    pub session_start: Vec<String>,
    #[serde(default)]
    pub session_end: Vec<String>,
    #[serde(default)]
    pub pre_tool: Vec<String>,
    #[serde(default)]
    pub post_tool: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct HookOutput {
    pub ok: bool,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Clone)]
pub struct HookDecision {
    pub allow: bool,
    pub reason: String,
}

impl HookDecision {
    pub fn allow() -> Self {
        Self {
            allow: true,
            reason: "allowed".into(),
        }
    }

    pub fn deny(reason: impl Into<String>) -> Self {
        Self {
            allow: false,
            reason: reason.into(),
        }
    }
}

pub async fn run_hook(
    command: &str,
    payload: &Value,
    cwd: &Path,
) -> Result<HookOutput, std::io::Error> {
    let mut process = if cfg!(windows) {
        let mut p = Command::new("cmd.exe");
        p.arg("/C").arg(command);
        p
    } else {
        let mut p = Command::new("sh");
        p.arg("-c").arg(command);
        p
    };
    process
        .current_dir(cwd)
        .env("RAINCODE_HOOK", "1")
        .env("RAINCODE_EVENT", payload.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // 挂死 hook 不应无限阻塞 agent:超时(或进程被取消/回收)时杀掉子进程。
        .kill_on_drop(true);

    let mut child = process.spawn()?;
    let write_result = async {
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(payload.to_string().as_bytes())
                .await} else {
            Ok(())
        }
    }
    .await;
    if let Err(error) = write_result {
        let _ = child.kill().await;
        return Err(error);
    }
    // 带超时的等待:pre_tool hook 在 provider 流循环内被 await,无超时会让挂死
    // hook 阻塞 agent 且 cancel() 无效(HookOutput 归为超时失败,调用方按 deny 处理)。
    let timeout = std::time::Duration::from_secs(30);
    let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(output) => output?,
        Err(_) => {
            return Ok(HookOutput {
                ok: false,
                code: None,
                stdout: String::new(),
                stderr: "hook timed out after 30s".to_string(),
            })
        }
    };
    Ok(HookOutput {
        ok: output.status.success(),
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).trim().to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    })
}

pub fn decision(output: &HookOutput) -> HookDecision {
    if !output.ok {
        return HookDecision::deny(format!("hook exited {}", output.code.unwrap_or(-1)));
    }
    match output.stdout.trim() {
        "allow" => return HookDecision::allow(),
        "deny" => return HookDecision::deny("pre-tool hook returned deny"),
        _ => {}
    }
    if let Ok(value) = serde_json::from_str::<Value>(&output.stdout) {
        if let Some(decision) = value.get("decision").and_then(Value::as_str) {
            match decision {
                "allow" => return HookDecision::allow(),
                "deny" => {
                    let reason = value
                        .get("reason")
                        .and_then(Value::as_str)
                        .unwrap_or("pre-tool hook returned deny");
                    return HookDecision::deny(reason);
                }
                _ => {}
            }
        }
    }
    HookDecision::allow()
}

pub fn session_payload(phase: &str, session_id: &str) -> Value {
    json!({"phase": phase, "session_id": session_id})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pre_tool_allow_is_parsed() {
        let output = HookOutput {
            ok: true,
            code: Some(0),
            stdout: "allow".into(),
            stderr: String::new(),
        };
        assert!(decision(&output).allow);
    }

    #[test]
    fn pre_tool_json_deny_is_parsed() {
        let output = HookOutput {
            ok: true,
            code: Some(0),
            stdout: r#"{"decision":"deny","reason":"no writes today"}"#.into(),
            stderr: String::new(),
        };
        let verdict = decision(&output);
        assert!(!verdict.allow);
        assert_eq!(verdict.reason, "no writes today");
    }

    #[test]
    fn nonzero_exit_denies_even_with_allow_output() {
        let output = HookOutput {
            ok: false,
            code: Some(1),
            stdout: "allow".into(),
            stderr: "boom".into(),
        };
        assert!(!decision(&output).allow);
    }

    #[tokio::test]
    async fn hook_receives_payload() {
        let cwd = std::env::temp_dir();
        let payload = json!({"tool": "write_file"});
        let command = if cfg!(windows) {
            "set /p line=<&0 & if not \"%line%\"==\"\" (echo ok) else (echo missing)"
        } else {
            "read line && echo ok || echo missing"
        };
        let output = run_hook(command, &payload, &cwd).await.unwrap();
        assert!(output.ok);
    }
}
