//! Slash-command parsing and the completion palette (pure, testable).
//! file-level copy from crates/rc-tui/src/repl/command.rs
#[derive(Debug, PartialEq)]
pub enum Cmd {
    /// 裸行:发任务。
    Run(String),
    /// /chat <text>:与基准模型对话(只执行用户指示)。
    Chat(String),
    Stop,
    Status,
    ListModels,
    UseModel(String),
    ListSessions,
    ListSkills,
    Setup,
    /// /configure <自然语言>:用自然语言配置模型,如 "配置 kimi 的模型"。
    Configure(String),
    Route(String),
    /// /autonomous <prompt>:自动化开发模式(自主编排:拆解→派子代理→汇总→下一步)。
    Autonomous(String),
    /// /thinking:强制下一次任务走 thinking 模式(展开模型网络)。
    Thinking,
    /// /normal:强制下一次任务走普通模式(单模型+skill)。
    Normal,
    Risk(Option<String>),
    Compact,
    /// /resume [id]:恢复一个历史会话(无参 → 交互选择器)。
    Resume(Option<String>),
    /// /refresh:更新模型能力评分(拉取 OpenRouter/arena 真实榜单)。
    Refresh,
    /// /supervise [model]:派出监督 agent,先讨论底线再监督子 agent。
    Supervise(Option<String>),
    /// /skill-nav <task>:导航 skill 网络(命中索引 → 下钻 → 叶子正文)。
    SkillNav(String),
    /// /title <name>:动态会话标题(空参清除;首条 Done 懒生成时未设则自动)。
    Title(String),
    Clear,
    Help,
    Quit,
    Unknown(String),
}

#[derive(Debug)]
pub struct CommandSpec {
    pub name: &'static str,
    pub desc: &'static str,
}

pub const COMMANDS: &[CommandSpec] = &[
    CommandSpec { name: "chat", desc: "对话(只执行你指示的内容)" },
    CommandSpec { name: "stop", desc: "中断当前任务" },
    CommandSpec { name: "status", desc: "当前会话/上下文/agent 状态" },
    CommandSpec { name: "models", desc: "列出已配置模型" },
    CommandSpec { name: "model", desc: "model <id> 切换默认模型" },
    CommandSpec { name: "sessions", desc: "列出历史会话" },
    CommandSpec { name: "skills", desc: "列出已安装技能" },
    CommandSpec { name: "setup", desc: "配置/更换模型(向导)" },
    CommandSpec { name: "configure", desc: "用自然语言配置模型,如:配置 kimi 的模型" },
    CommandSpec { name: "route", desc: "路由分解(多 agent 并行执行 + steering 干预)" },
    CommandSpec { name: "autonomous", desc: "自动化开发模式(自主编排:拆解→派子代理→汇总→下一步)" },
    CommandSpec { name: "thinking", desc: "下一次任务强制 Thinking 模式(展开模型网络)" },
    CommandSpec { name: "normal", desc: "下一次任务强制普通模式(单模型+skill)" },
    CommandSpec { name: "risk", desc: "风险模式切换:auto/assisted/ask/manual" },
    CommandSpec { name: "compact", desc: "压缩上下文:用模型总结本会话,历史收缩后继续" },
    CommandSpec { name: "resume", desc: "恢复历史会话(交互选择器或 /resume <id>)" },
    CommandSpec { name: "refresh-model-scores", desc: "更新模型评分(拉取 OpenRouter/arena 真实榜单)" },
    CommandSpec { name: "supervise", desc: "派出监督 agent(可选模型),先定义底线再监督子 agent" },
    CommandSpec { name: "skill-nav", desc: "导航 skill 网络:下钻索引到叶子" },
    CommandSpec { name: "title", desc: "设置会话标题" },
    CommandSpec { name: "clear", desc: "清空输出区" },
    CommandSpec { name: "help", desc: "命令说明" },
    CommandSpec { name: "quit", desc: "退出" },
];

pub fn parse(text: &str) -> Option<Cmd> {
    let t = text.trim();
    if t.is_empty() {
        return None;
    }
    if let Some(rest) = t.strip_prefix('/') {
        let (name, arg) = rest
            .split_once(' ')
            .map(|(n, a)| (n, a.trim().to_string()))
            .unwrap_or((rest, String::new()));
        let cmd = match name {
            "chat" => Cmd::Chat(arg),
            "stop" => Cmd::Stop,
            "status" => Cmd::Status,
            "models" => Cmd::ListModels,
            "model" => Cmd::UseModel(arg), // /model(空参)→ 交互式选择器;/model <id> → 直接切换
            "sessions" => Cmd::ListSessions,
            "skills" => Cmd::ListSkills,
            "setup" => Cmd::Setup,
            "configure" => Cmd::Configure(arg),
            "route" => Cmd::Route(arg),
            "autonomous" => Cmd::Autonomous(arg),
            "thinking" => Cmd::Thinking,
            "normal" => Cmd::Normal,
            "risk" if arg.is_empty() => Cmd::Risk(None),
            "risk" => Cmd::Risk(Some(arg)),
            "resume" if arg.is_empty() => Cmd::Resume(None),
            "resume" => Cmd::Resume(Some(arg)),
            "compact" => Cmd::Compact,
            "refresh-model-scores" | "refresh" => Cmd::Refresh,
            "supervise" if arg.is_empty() => Cmd::Supervise(None),
            "supervise" => Cmd::Supervise(Some(arg)),
            "skill-nav" => Cmd::SkillNav(arg),
            "title" => Cmd::Title(arg), // /title(空参)→ 清除;/title <name> → 设置
            "clear" => Cmd::Clear,
            "help" => Cmd::Help,
            "quit" | "exit" => Cmd::Quit,
            other => Cmd::Unknown(other.to_string()),
        };
        Some(cmd)
    } else {
        Some(Cmd::Run(t.to_string()))
    }
}

pub fn complete(prefix: &str) -> Vec<&'static CommandSpec> {
    COMMANDS
        .iter()
        .filter(|spec| spec.name.starts_with(prefix))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_is_none() {
        assert_eq!(parse(""), None);
        assert_eq!(parse("   "), None);
    }

    #[test]
    fn bare_line_is_run() {
        assert!(matches!(parse("生成一个页面"), Some(Cmd::Run(_))));
    }

    #[test]
    fn slash_commands_parse() {
        assert!(matches!(parse("/chat hi"), Some(Cmd::Chat(_))));
        assert!(matches!(parse("/stop"), Some(Cmd::Stop)));
        assert!(matches!(parse("/models"), Some(Cmd::ListModels)));
        assert!(matches!(parse("/model gpt-5"), Some(Cmd::UseModel(_))));
        assert!(matches!(parse("/model"), Some(Cmd::UseModel(id)) if id.is_empty()));
        assert!(matches!(parse("/autonomous build a cli"), Some(Cmd::Autonomous(_))));
        assert!(matches!(parse("/risk"), Some(Cmd::Risk(None))));
        assert!(matches!(parse("/risk manual"), Some(Cmd::Risk(Some(_)))));
        assert!(matches!(parse("/thinking"), Some(Cmd::Thinking)));
        assert!(matches!(parse("/normal"), Some(Cmd::Normal)));
        assert!(matches!(parse("/skills"), Some(Cmd::ListSkills)));
        assert!(matches!(parse("/setup"), Some(Cmd::Setup)));
        assert!(matches!(parse("/refresh"), Some(Cmd::Refresh)));
        assert!(matches!(parse("/quit"), Some(Cmd::Quit)));
        assert!(matches!(parse("/exit"), Some(Cmd::Quit)));
        assert!(matches!(parse("/nope"), Some(Cmd::Unknown(_))));
    }

    #[test]
    fn completion_prefix_filters() {
        let names: Vec<&str> = complete("mo").iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["models", "model"]);
        assert!(complete("z").is_empty());
    }

    #[test]
    fn resume_command_parses() {
        assert!(matches!(parse("/resume"), Some(Cmd::Resume(None))));
        assert!(matches!(parse("/resume abc123"), Some(Cmd::Resume(Some(id))) if id == "abc123"));
    }

    #[test]
    fn supervise_command_parses() {
        assert!(matches!(parse("/supervise"), Some(Cmd::Supervise(None))));
        assert!(matches!(parse("/supervise deepseek-v4-flash"), Some(Cmd::Supervise(Some(m))) if m == "deepseek-v4-flash"));
    }

    #[test]
    fn skill_nav_command_parses() {
        assert!(matches!(parse("/skill-nav build react page"), Some(Cmd::SkillNav(t)) if t.contains("react")));
        assert!(matches!(parse("/skill-nav react.performance"), Some(Cmd::SkillNav(t)) if t == "react.performance"));
    }

    #[test]
    fn title_command_parses() {
        assert!(matches!(parse("/title"), Some(Cmd::Title(t)) if t.is_empty()));
        assert!(matches!(parse("/title Fixes the login crash"), Some(Cmd::Title(t)) if t == "Fixes the login crash"));
    }
}
