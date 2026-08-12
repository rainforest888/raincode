//! Turn ReplModel state into terminal lines. Pure; the shell draws them.
//! claude-code style: committed output → scrollback delta lines; live state
//! (streaming / tool / thinking) → a bottom live-line stack above the HUD.
use std::time::Instant;

use unicode_width::UnicodeWidthStr;

use crate::repl::agent_palette::{agent_color, queued_badge};
use crate::repl::fmt::{format_elapsed, format_tokens, truncate_line};
use crate::repl::model::{LineKind, LineStyle, PendingPrompt, Phase, ReplModel, ToolState};
use crate::repl::palette::{
    fg, BOLD, BRIGHT_YELLOW, DIM, ERROR, INFO, PRIMARY, RED, RESET, SECONDARY, STRIKE, SUCCESS,
    WARNING,
};

pub fn style_line(style: LineStyle) -> &'static str {
    match style {
        LineStyle::Plain => "",
        LineStyle::Dim => DIM,
        LineStyle::Accent => PRIMARY,
        LineStyle::Warn => WARNING,
        LineStyle::Error => ERROR,
        LineStyle::Success => SUCCESS,
        LineStyle::Agent => SECONDARY,
        LineStyle::Tool => INFO,
        LineStyle::Supervisor => RED,
        LineStyle::Custom(c) => c,
    }
}

pub fn paint(text: &str, style: LineStyle) -> String {
    let code = style_line(style);
    if code.is_empty() {
        text.to_string()
    } else {
        format!("{code}{text}{RESET}")
    }
}

/// One draw frame: the FULL screen laid out into rows (claude-code/opencode
/// model — app-side buffer, diff-rendered to the terminal). `lines[r]` is the
/// complete content of screen row `r` (0-based). Bottom rows are the pinned
/// input area; above them the scrollable conversation area.
pub struct RenderFrame {
    /// 整屏每一行的内容(已着色)。索引 = 屏幕行号。
    pub lines: Vec<String>,
    /// 输入框所在屏幕行号(光标停这里)。
    pub input_row: u16,
    /// 输入框内光标列。
    pub cursor_col: usize,
}

pub fn render(
    model: &ReplModel,
    width: usize,
    height: usize,
    now: Instant,
) -> RenderFrame {
    let width = width.max(1);
    let height = height.max(3);
    // 底部钉行:live 栈 + 看板(任务树 或 agent 看板)+ HUD + 分隔线 + input + status。
    let live_lines = render_live(model, width, now);
    // 有任务树(自动编排)时用树看板(超集),否则回退平铺 agent 看板。
    let tree_lines = render_tree(model, width, now);
    let board_lines: Vec<String> = if !tree_lines.is_empty() {
        tree_lines
    } else {
        render_agents(model, width)
    };
    let hud = render_hud(model, width, now);
    let todo_lines = render_todo(model, width);
    let separator = render_separator(width);
    // 输入区(含斜杠菜单多行):先取行数,菜单行计入 pinned_h,避免挤掉滚动区。
    let (input_rows, cursor_col) = render_input(model, width);
    let status = render_status(model, width);
    // 钉行:live 栈 + 看板 + 对话区下横线 + hud + todo + 输入区上横线 + input(含菜单)+ 状态行上横线 + status。
    let pinned_h = live_lines.len() + board_lines.len() + todo_lines.len() + 5 + input_rows.len(); // 3 seps + hud + status + input rows
    // 可滚动区高度(对话区)。
    let scroll_h = height.saturating_sub(pinned_h).max(1);
    // 可见对话:取最近 scroll_h 行(贴底)。消息流层次:User 行前插细分隔线,
    // 用户消息 BOLD + 当前 agent 色(`›` 前缀在文本里),其余按原样式。
    // 当前 agent 色:focus_agent 存在 → 该 agent 的稳定色;否则 PRIMARY(单 agent)。
    let user_color = model
        .focus_agent
        .as_deref()
        .map(|id| agent_color(id, None))
        .unwrap_or(PRIMARY);
    // 可见正文宽:左侧 gutter(消息导航点列,2 字符)占掉 2 格,正文截断到剩余宽度。
    let avail = width.saturating_sub(2);
    let mut output_lines: Vec<String> = Vec::with_capacity(model.output.len());
    // 并行 gutter:每条 user 消息起点一个 `· ` 导航点,其余行 `  ` 占位保持对齐。
    let mut gutter: Vec<String> = Vec::with_capacity(model.output.len());
    for l in model.output.iter() {
        if l.kind == LineKind::User {
            // 用户消息前插细分隔线(gutter 无点,点落在消息文本行,对齐用户行)。
            gutter.push("  ".to_string());
            output_lines.push(paint(
                &truncate_line(&"─".repeat(avail), avail),
                LineStyle::Custom(user_color),
            ));
            gutter.push("· ".to_string());
            output_lines.push(format!(
                "{BOLD}{user_color}{}{RESET}",
                truncate_line(&l.text, avail)
            ));
        } else {
            gutter.push("  ".to_string());
            output_lines.push(paint(&truncate_line(&l.text, avail), l.style));
        }
    }
    // 滚动窗口:贴底(0)显示最近 scroll_h 行;上滚则从底部往上移 scroll_offset 行。
    let start = output_lines.len().saturating_sub(scroll_h + model.scroll_offset);
    let mut lines: Vec<String> = Vec::with_capacity(height);
    // 对话区:可见窗口内的历史行;不足时补空行(空白只在滚动区内部,不挤输入框)。
    for r in 0..scroll_h {
        let idx = start + r;
        if idx < output_lines.len() {
            lines.push(format!("{}{}", gutter[idx], output_lines[idx]));
        } else {
            lines.push(String::new());
        }
    }
    // 钉行。
    for l in &live_lines {
        lines.push(l.clone());
    }
    // 看板:任务树(自动编排)或平铺 agent 看板;每个存活 agent 一行(名称/状态/模型/工具)。
    for l in &board_lines {
        lines.push(l.clone());
    }
    // 对话区下横线:把对话区与底部状态区(HUD/输入)界定开。
    lines.push(separator.clone());
    lines.push(hud);
    // 当前对话 agent 的 todo 状态(claude-code 风格)。
    for l in render_todo(model, width) {
        lines.push(l);
    }
    lines.push(separator.clone());
    // 输入行(含下方斜杠菜单行);光标停在输入行。
    let input_row = lines.len() as u16;
    for l in &input_rows {
        lines.push(l.clone());
    }
    // 状态行上横线:把输入区与权限状态行界定开。
    lines.push(separator);
    lines.push(status);
    // 高度补齐(极窄屏)。
    while lines.len() < height {
        lines.push(String::new());
    }
    RenderFrame {
        lines: lines.into_iter().take(height).collect(),
        input_row,
        cursor_col,
    }
}

/// 输入区上方的横线分隔(claude-code 式),把对话区与输入区界定开。
fn render_separator(width: usize) -> String {
    paint(&"─".repeat(width), LineStyle::Dim)
}

/// 工具 pending 动词表（opencode pending 动词风格）。
pub fn tool_pending_verb(name: &str) -> &'static str {
    match name {
        "run_shell" | "execute" | "run" => "Running command...",
        "write_file" | "edit" | "apply_patch" | "patch" => "Writing file...",
        "read_file" | "read" => "Reading file...",
        "glob" | "grep" | "search" | "find" => "Finding files...",
        "task" | "delegate" | "send_agent" => "Delegating...",
        "web_search" | "websearch" | "fetch" => "Searching web...",
        "skill" | "load_skill" => "Loading skill...",
        _ => "Using tool...",
    }
}

/// live 行栈（自底向上）：streaming / thinking / tool。底部 HUD 之下。
fn render_live(model: &ReplModel, width: usize, now: Instant) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    // pending_steers 预览:运行中提交但未注入的 steer 队列(codex 式,最多 3 条)。
    for s in model.pending_steers.iter().take(3) {
        out.push(paint(
            &truncate_line(&format!("↳ 待注入: {s}"), width),
            LineStyle::Tool,
        ));
    }
    // 审批等待行(对话流审批):pending 时高亮显示等待的工具。
    if let Some(wait) = &model.approval_wait {
        let mut text = format!("[审批] {wait}");
        // 按键提示按 pending 变体变化:Approval → Y/N/A;Guard → 0-3。
        if let Some(hint) = model.approval_hint() {
            text.push(' ');
            text.push_str(hint);
        }
        out.push(paint(&truncate_line(&text, width), LineStyle::Warn));
    }
    // agent 详情指示:当前选中某子代理时,顶部显示其状态 + steer 提示。
    if let Some(focus) = &model.focus_agent {
        if let Some(a) = model.agents.get(focus) {
            let mark = if a.done { "✓" } else if a.failed { "✗" } else { "✻" };
            let tool = a.tool.as_deref().unwrap_or("");
            let line = format!(
                "{mark} ▣ agent {focus} ({}) [{}] {}{} · steer ❯",
                a.role, a.model, a.phase, tool
            );
            out.push(paint(&truncate_line(&line, width), LineStyle::Agent));
        }
    }
    if let Some(t) = &model.live_tool {
        // 动画 spinner 帧(运行中):由 now 时间驱动,8 帧 80ms。
        let frames = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
        let content = match t.state {
            ToolState::Running => {
                let idx = (now
                    .duration_since(model.started_at.unwrap_or(now))
                    .as_millis()
                    / 80) as usize
                    % frames.len();
                format!("{} ~ {}{}", frames[idx], tool_pending_verb(&t.name), t.args)
            }
            ToolState::Ok => format!("✓ {}{}", t.name, t.args),
            ToolState::Failed => format!("✗ {}{}", t.name, t.args),
            ToolState::Denied => format!("✗ {}{}", t.name, t.args),
        };
        let color = match t.state {
            ToolState::Running => fg(DIM, &content),
            ToolState::Ok => fg(SUCCESS, &content),
            ToolState::Failed => fg(ERROR, &content),
            ToolState::Denied => {
                // 删除线:denied 工具不执行,画删除线表达"被拒"。
                format!("{STRIKE}{content}{RESET}")
            }
        };
        out.push(truncate_line(&color, width));
    }
    // 思考中:显示最近 3 行(暗色);Ctrl+O 展开全链(可滚动区随 output)。
    if let Some(thinking) = &model.live_thinking {
        let lines: Vec<&str> = thinking.lines().rev().take(3).collect();
        for line in lines.iter().rev() {
            out.push(paint(
                &truncate_line(&format!("↳ Thinking: {line}"), width),
                LineStyle::Dim,
            ));
        }
    }
    // 思考完成:可折叠行(Enter 展开)。完整链存在 reasoning_chain 时显示。
    else if let Some(chain) = &model.reasoning_chain {
        if model.reasoning_expanded {
            // 展开:完整思维链逐行显示(不截断成一行),否则长链只露第一屏。
            for line in chain.lines() {
                out.push(paint(&truncate_line(line, width), LineStyle::Dim));
            }
            out.push(paint(
                &truncate_line("(Ctrl+O 收起)", width),
                LineStyle::Dim,
            ));
        } else {
            let first = chain.lines().next().unwrap_or("").trim();
            out.push(paint(
                &truncate_line(&format!("↳ 推理: {first}"), width),
                LineStyle::Dim,
            ));
        }
    }
    if let Some(text) = &model.streaming {
        out.push(paint(&truncate_line(text, width), LineStyle::Plain));
    } else if model.phase == Phase::Running
        && model.live_tool.is_none()
        && model.live_thinking.is_none()
    {
        // 运行中但还没有输出:Knight Rider 扫描灯(忙碌输入区;8 格 trail 指数衰减,
        // agent 色)。lead 每 100ms 右移一格,距离 lead 越远格越暗。保留原 ✻ 呼吸
        // 行的 phase_note 标签 + 耗时;✻ 呼吸仍在 HUD 与任务树 running 节点。
        let ms = model
            .started_at
            .map(|s| now.duration_since(s).as_millis())
            .unwrap_or(0);
        let secs = (ms / 1000) as u64;
        let label = model.phase_note.as_deref().unwrap_or("工作中");
        let lead = (ms / 100) as usize % 8;
        let cells: Vec<&str> = (0..8usize)
            .map(|i| {
                let dist = i.abs_diff(lead);
                if dist == 0 {
                    "◈"
                } else if dist <= 2 {
                    "◆"
                } else {
                    "·"
                }
            })
            .collect();
        // 标签区先按无色截断(留 8 格 + 空格给扫描灯),再上色:ANSI 码会被
        // unicode-width 误计宽,含码字符串直接 truncate 会切断色码。
        let label_plain = truncate_line(
            &format!("{label} · {}", format_elapsed(secs)),
            width.saturating_sub(9),
        );
        let color = agent_color("busy", None);
        let mut rider = String::new();
        for &ch in &cells {
            rider.push_str(&format!("{color}{ch}{RESET}"));
        }
        out.push(paint(&format!("{rider} {label_plain}"), LineStyle::Dim));
    }
    // 完成闪烁(2.4s):在 live 栈顶部给一行"✓ 完成"。
    if let Some(done_at) = model.done_at {
        let ms = now.duration_since(done_at).as_millis();
        if ms < 2_400 {
            let flash = if (ms / 200) % 2 == 0 { BOLD } else { "" };
            let plain = format!("✓ 完成 · {}", format_elapsed(model.last_elapsed));
            out.push(format!(
                "{flash}{}{RESET}",
                truncate_line(&plain, width)
            ));
        }
    }
    out
}

/// 子代理看板:每个存活 agent 一行,显示名称/状态/模型/工具(主界面常驻)。
/// 只显示运行中或已完成的 agent;空时返回空 Vec(不占行)。
fn render_agents(model: &ReplModel, width: usize) -> Vec<String> {
    if model.agents.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (id, a) in &model.agents {
        let mark = if a.done {
            "✓"
        } else if a.failed {
            "✗"
        } else {
            "✻"
        };
        let tool = a.tool.as_deref().unwrap_or("");
        let status = if a.done { "done" } else if a.failed { "failed" } else { &a.phase };
        // 聚焦的 agent 高亮。
        let focused = model.focus_agent.as_deref() == Some(id.as_str());
        let line = format!(
            "{mark} agent {id} · {status} · {} · {}{}",
            a.model,
            tool,
            if focused { " · [focused]" } else { "" }
        );
        // 7 色轮换:每个 agent 按 id 哈希取稳定色(opencode 式)。
        out.push(paint(
            &truncate_line(&line, width),
            LineStyle::Custom(agent_color(id, None)),
        ));
    }
    // 子代理导航 footer:多 agent 时追加 {label} ({idx} of {total}) · {ctx}%。
    // cost 无 usage 追踪(AgentView 无 cost 字段)→ 缺失即省略。
    if out.len() > 1 {
        let total = model.agents.len();
        let idx = model
            .focus_agent
            .as_ref()
            .and_then(|f| model.agents.keys().position(|k| k == f))
            .map(|i| i + 1)
            .unwrap_or(0);
        let label = model.focus_agent.as_deref().unwrap_or("agents");
        let ctx = model.context.pct;
        let footer = format!("{label} ({idx} of {total}) · {ctx}%");
        out.push(paint(&truncate_line(&footer, width), LineStyle::Dim));
    }
    out
}

/// 任务树看板(自动编排常驻):根 + 缩进子任务(按 depth,最深 3 层),状态标记。
/// 有任务树时替代平铺 agent 看板;无树返回空(回退 render_agents)。
/// `tree_visible=false` 折叠成一行状态(Ctrl+t 切换)。running 节点呼吸脉冲。
fn render_tree(model: &ReplModel, width: usize, now: Instant) -> Vec<String> {
    use rc_orchestrate::tree::TaskStatus;
    let tree = &model.tree;
    let Some(root) = tree.root() else { return Vec::new() };
    let total = tree.nodes.len().saturating_sub(1); // 除根外的子任务数
    let done = tree
        .nodes
        .values()
        .filter(|n| n.status == TaskStatus::Done)
        .count();
    if !model.tree_visible {
        return vec![paint(
            &truncate_line(
                &format!("✻ 任务树 · {total} 子任务 · {done} done (Ctrl+t 展开)"),
                width,
            ),
            LineStyle::Dim,
        )];
    }
    let mut out = vec![paint(
        &truncate_line(&format!("✻ 任务树 · {total} 子任务 (Ctrl+t 折叠)"), width),
        LineStyle::Dim,
    )];
    // DFS 渲染:根 + 缩进子任务(状态标记 + 自动选中的模型 + focus 高亮)。
    fn walk(
        node_id: &str,
        tree: &rc_orchestrate::tree::TaskTree,
        model: &ReplModel,
        width: usize,
        now: Instant,
        out: &mut Vec<String>,
    ) {
        if let Some(node) = tree.nodes.get(node_id) {
            let mark = match node.status {
                TaskStatus::Done => "✓",
                TaskStatus::Failed => "✗",
                TaskStatus::Running => "✻",
                TaskStatus::Pending => "◻",
            };
            let model_suffix = node
                .model
                .as_ref()
                .map(|m| format!(" ({m})"))
                .unwrap_or_default();
            let focused = model.focus_agent.as_deref() == Some(node.id.as_str());
            let line = format!(
                "{mark} {}{}{}",
                node.description,
                model_suffix,
                if focused { " · [focused]" } else { "" }
            );
            let indent = "  ".repeat(node.depth.saturating_sub(1) as usize);
            // running 节点呼吸:蓝/亮黄交替(相对任务开始时间)。
            let style = match node.status {
                TaskStatus::Done => LineStyle::Success,
                TaskStatus::Failed => LineStyle::Error,
                TaskStatus::Running => {
                    let ms = model
                        .started_at
                        .map(|s| now.duration_since(s).as_millis())
                        .unwrap_or(0);
                    if (ms / 400) % 2 == 0 {
                        LineStyle::Custom(SECONDARY)
                    } else {
                        LineStyle::Custom(BRIGHT_YELLOW)
                    }
                }
                TaskStatus::Pending => LineStyle::Dim,
            };
            out.push(paint(&truncate_line(&format!("{indent}{line}"), width), style));
            for child in tree.children_of(node_id) {
                walk(&child.id, tree, model, width, now, out);
            }
        }
    }
    walk(&root.id, tree, model, width, now, &mut out);
    out
}

/// 斜杠命令补全浮层:菜单行在输入行下方,每条一行,选中项 ▸ + BOLD+PRIMARY。
/// 由 `render_input` 组多行;无匹配时单行 `? 无匹配命令`。
fn render_menu_rows(model: &ReplModel, width: usize) -> Vec<String> {
    let Some(menu) = &model.slash_menu else { return Vec::new() };
    if menu.items.is_empty() {
        let name = model
            .input
            .text
            .trim_start()
            .strip_prefix('/')
            .and_then(|r| r.split(' ').next())
            .unwrap_or("");
        return vec![paint(
            &truncate_line(&format!("? 无匹配命令: {name}"), width),
            LineStyle::Warn,
        )];
    }
    menu.items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let sel = i == menu.selected;
            let prefix = if sel { "▸ " } else { "  " };
            let line = format!("{prefix}/{:<14}{}", item.name, item.desc);
            if sel {
                format!("{BOLD}{PRIMARY}{}{RESET}", truncate_line(&line, width))
            } else {
                paint(&truncate_line(&line, width), LineStyle::Dim)
            }
        })
        .collect()
}

pub fn colored_bar(pct: u8, width: usize) -> String {
    let width = width.max(2);
    let filled = ((pct as usize) * width / 100).min(width);
    let color = if pct >= 90 {
        ERROR
    } else if pct >= 70 {
        WARNING
    } else {
        INFO
    };
    format!("{color}{}{}{RESET}", "█".repeat(filled), "░".repeat(width - filled))
}

fn bar_width(width: usize) -> usize {
    (width / 4).clamp(4, 20)
}

pub fn render_hud(model: &ReplModel, width: usize, now: Instant) -> String {
    let ctx = &model.context;
    let bar = colored_bar(ctx.pct, bar_width(width));
    let running = model.agents.values().filter(|a| !a.done).count();
    let agents = if running > 0 {
        format!(" · ✻ {running} agents")
    } else if !model.agents.is_empty() {
        format!(" · ○ {} agents", model.agents.len())
    } else {
        String::new()
    };
    let elapsed = model
        .started_at
        .map(|t| format_elapsed(now.duration_since(t).as_secs()))
        .unwrap_or_default();
    let turns = if model.turn_count > 0 {
        format!(" · {t} turns", t = model.turn_count)
    } else {
        String::new()
    };
    let mut plain = format!(
        "{} · ctx {} {}/{} ({}%){}{}",
        model.model,
        bar,
        format_tokens(ctx.used),
        format_tokens(ctx.limit),
        ctx.pct,
        agents,
        turns,
    );
    if model.phase == Phase::Running && !elapsed.is_empty() {
        plain.push_str(&format!(" · {}", elapsed));
    }
    if model.phase == Phase::Running {
        if let Some(pn) = &model.phase_note {
            plain.push_str(&format!(" · {pn}"));
        }
    }
    // 行宽预算:标题段(✻ <title> ·)与 QUEUED 徽标(≈10 格)共享 width,plain 只在
    // 剩余宽度内截断 —— 否则标题段 + plain + 徽标可拼到 ~2×width,终端换行会
    // 顶动下方状态/输入行(shell 按帧逐行原样写,无行级 clamp)。先给徽标留宽,
    // 余下(base)按"标题优先"分配:标题段自身也截断到 base,plain 占剩余。
    let queued = model.has_queued();
    let badge_w = if queued { 10 } else { 0 };
    let base = width.saturating_sub(badge_w);
    let title_seg = model
        .title
        .as_ref()
        .map(|t| truncate_line(&format!("✻ {t} · "), base));
    let title_w = title_seg
        .as_ref()
        .map_or(0, |s| UnicodeWidthStr::width(s.as_str()));
    let plain = truncate_line(&plain, base.saturating_sub(title_w));
    let mut painted = paint(&plain, LineStyle::Dim);
    // 动态会话标题:HUD 首行 ✻ <title> ·(Accent);缺失即省略。
    if let Some(seg) = &title_seg {
        painted = format!("{}{painted}", paint(seg, LineStyle::Accent));
    }
    // QUEUED 徽标:真实排队(pending_steers / queued_input 非空)→ agent 色背景
    // + 亮度对比前景(黑/白)。
    if queued {
        let color = agent_color(model.focus_agent.as_deref().unwrap_or("queued"), None);
        painted = format!("{painted} {}", queued_badge(color));
    }
    if running > 0 {
        // 让工作中的 ✻ 呼吸:黄/亮黄交替(相对任务开始时间)。
        let t = model
            .started_at
            .map(|s| now.duration_since(s).as_millis())
            .unwrap_or(0);
        let breath = if (t / 250) % 2 == 0 { WARNING } else { BRIGHT_YELLOW };
        // 呼吸只作用于行尾 agents 的 ✻;标题若存在其 ✻ 在行首,取最后一个匹配。
        replace_last_char(&painted, '✻', &format!("{breath}✻{RESET}"))
    } else {
        painted
    }
}

/// 替换字符串中最后一个 marker 字符(余下内容不变)。用于 HUD 呼吸动画只
/// 作用于 agents 的 ✻,而不误伤行首标题的 ✻。
fn replace_last_char(s: &str, marker: char, rep: &str) -> String {
    match s.rfind(marker) {
        Some(i) => {
            let after = i + marker.len_utf8();
            format!("{}{}{}", &s[..i], rep, &s[after..])
        }
        None => s.to_string(),
    }
}

/// 底部状态栏（HUD 之下、输入框之上）：`tokens (pct) · cost · 快捷键`。
/// 空信息省略（行不跳动）。真实 token 来自 ContextUpdate；成本暂以 0 占位
/// （rc-core Done.usage 里有 cost 时由模型写入）。
/// 模型能力标注:最强维度 ≥70 → ⬆(强),≥40 → ➖(中),否则 ⬇(弱)。真实榜单分。
fn model_marker(coding: f64, reasoning: f64, frontend: f64, backend: f64) -> &'static str {
    let best = coding.max(reasoning).max(frontend).max(backend);
    if best >= 70.0 {
        "⬆"
    } else if best >= 40.0 {
        "➖"
    } else {
        "⬇"
    }
}

/// 交互式模型选择器(/model):搜索行 + 过滤后的模型列表(渠道/模型 + 能力标注 + 选中高亮)。
fn render_model_picker(model: &ReplModel, width: usize) -> Vec<String> {
    let Some(picker) = &model.model_picker else { return Vec::new() };
    let search = format!("模型选择器 · 搜索: {}", picker.query);
    let mut out = vec![paint(&truncate_line(&search, width), LineStyle::Warn)];
    if picker.filtered.is_empty() {
        out.push(paint(&truncate_line("? 无匹配模型", width), LineStyle::Warn));
        return out;
    }
    for (row, &idx) in picker.filtered.iter().enumerate() {
        let e = &picker.all[idx];
        let mark = model_marker(e.coding, e.reasoning, e.frontend, e.backend);
        let active = if e.active { " · [active]" } else { "" };
        // 渠道/模型 复合标识:同一模型名不同渠道是不同模型(deepseek/ds-* vs opencode/ds-*)。
        let line = format!(
            "{mark} {}/{} · 编{:.0} 推{:.0}{}",
            e.provider, e.model, e.coding, e.reasoning, active
        );
        if row == picker.selected {
            out.push(format!(
                "{BOLD}{PRIMARY}▸ {}{RESET}",
                truncate_line(&line, width)
            ));
        } else {
            out.push(paint(&truncate_line(&line, width), LineStyle::Dim));
        }
    }
    out
}

/// 交互式会话选择器(/resume):搜索行 + 过滤后的会话列表(short_id · summary · updated_at)。
fn render_session_picker(model: &ReplModel, width: usize) -> Vec<String> {
    let Some(picker) = &model.session_picker else { return Vec::new() };
    let search = format!("会话选择器 · 搜索: {}", picker.query);
    let mut out = vec![paint(&truncate_line(&search, width), LineStyle::Warn)];
    if picker.filtered.is_empty() {
        out.push(paint(&truncate_line("? 无匹配会话", width), LineStyle::Warn));
        return out;
    }
    for (row, &idx) in picker.filtered.iter().enumerate() {
        let e = &picker.all[idx];
        let line = format!("{} · {} · {}", e.short_id, e.summary, e.updated_at);
        if row == picker.selected {
            out.push(format!(
                "{BOLD}{PRIMARY}▸ {}{RESET}",
                truncate_line(&line, width)
            ));
        } else {
            out.push(paint(&truncate_line(&line, width), LineStyle::Dim));
        }
    }
    out
}

fn render_input(model: &ReplModel, width: usize) -> (Vec<String>, usize) {
    if let Some(pending) = &model.pending {
        return match pending {
            PendingPrompt::Approval { req, .. } => {
                let plain =
                    truncate_line(&format!("[approval] {}: {} [y/N] ", req.tool, req.description), width);
                (vec![paint(&plain, LineStyle::Warn)], plain.chars().count() + 1)
            }
            PendingPrompt::Question { text, secret, .. } => {
                let stars = if *secret { "*".repeat(model.input.text.chars().count()) } else { String::new() };
                let prefix = if *secret { "[key] " } else { "[question] " };
                let plain = truncate_line(&format!("{prefix}{text} {stars}"), width);
                (vec![paint(&plain, LineStyle::Warn)], plain.chars().count() + 1)
            }
            PendingPrompt::Guard { req, .. } => {
                let plain = truncate_line(
                    &format!("[guard] {}: {} [0=拒绝 1=仅本次 2=本会话 3=永久] ", req.tool, req.reason),
                    width,
                );
                (vec![paint(&plain, LineStyle::Warn)], plain.chars().count() + 1)
            }
        };
    }
    // 模型选择器模式:搜索行(光标在此)+ 模型列表。
    if let Some(picker) = &model.model_picker {
        let search = format!("模型选择器 · 搜索: {}", picker.query);
        let search_w = UnicodeWidthStr::width(search.as_str()) + 1;
        return (render_model_picker(model, width), search_w);
    }
    // 会话选择器模式:搜索行(光标在此)+ 会话列表。
    if let Some(picker) = &model.session_picker {
        let search = format!("会话选择器 · 搜索: {}", picker.query);
        let search_w = UnicodeWidthStr::width(search.as_str()) + 1;
        return (render_session_picker(model, width), search_w);
    }
    let prompt = "❯ ";
    let editable = truncate_line(&model.input.text, width.saturating_sub(3));
    let before = truncate_line(&model.input.text[..model.input.cursor], width.saturating_sub(3));
    let cursor_col = 2 + UnicodeWidthStr::width(before.as_str());
    let mut rows = vec![format!("{prompt}{editable}")];
    // 斜杠菜单:菜单行在输入行下方(每条一行,↑↓ 选)。
    if model.slash_menu.is_some() {
        rows.extend(render_menu_rows(model, width));
    }
    (rows, cursor_col)
}

/// 当前对话 agent 的 todo 状态(claude-code 风格):统计行 + 最近进行中/未开始项。
/// 无 todo 时返回空(不占行)。
fn render_todo(model: &ReplModel, width: usize) -> Vec<String> {
    use rc_orchestrate::todo::TodoStatus;
    let todo = &model.todo;
    if todo.items.is_empty() {
        return Vec::new();
    }
    let mut out = vec![paint(
        &truncate_line(&format!("✻ {}", todo.stats_line()), width),
        LineStyle::Dim,
    )];
    for item in todo.recent(4) {
        let mark = match item.status {
            TodoStatus::InProgress => "◼",
            TodoStatus::Pending => "◻",
            _ => "✓",
        };
        out.push(paint(
            &truncate_line(&format!("{mark} {}", item.text), width),
            LineStyle::Dim,
        ));
    }
    out
}

/// 底部状态栏(输入框下一行):⏵⏵ 风险模式 + (shift+tab to cycle) 提示。
fn render_status(model: &ReplModel, width: usize) -> String {
    use rc_router::risk::RiskMode::*;
    let color = match model.risk_mode {
        Ask => INFO,
        Assisted => WARNING,
        Auto => SUCCESS,
        Manual => ERROR,
    };
    let plain = format!(
        "⏵⏵ {} on (shift+tab to cycle)",
        model.risk_label()
    );
    paint(&truncate_line(&plain, width), LineStyle::Custom(color))
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;
    use crate::repl::command;
    use crate::repl::model::{ReplModel, ToolView};

    fn now() -> Instant {
        Instant::now()
    }

    fn strip_ansi(s: &str) -> String {
        let re = regex::Regex::new(r"\x1b\[[0-9;]*m").unwrap();
        re.replace_all(s, "").to_string()
    }

    #[test]
    fn colored_bar_scales_with_pct() {
        assert!(colored_bar(0, 10).contains("░".repeat(10).as_str()));
        assert!(colored_bar(100, 10).contains("█".repeat(10).as_str()));
        assert!(colored_bar(50, 10).contains("█".repeat(5).as_str()));
    }

    #[test]
    fn hud_shows_zero_context_when_nothing_happened() {
        let m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        let hud = render_hud(&m, 80, now());
        let plain = strip_ansi(&hud);
        assert!(plain.contains("gpt-5"));
        assert!(plain.contains("0"));
        assert!(!plain.contains("agents"));
    }

    #[test]
    fn hud_shows_agent_count_after_spawn() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.apply_event(rc_proto::AgentEvent::AgentSpawned {
            id: "a1".into(),
            model: "gpt-5".into(),
            role: "coder".into(),
            task: "build".into(),
        });
        let hud = render_hud(&m, 80, now());
        assert!(strip_ansi(&hud).contains("1 agents"));
    }

    #[test]
    fn slash_menu_shows_all_commands_for_bare_slash() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.input.text = "/".into();
        m.input.cursor = 1;
        m.update_slash_menu();
        let rows = render_input(&m, 160);
        assert!(rows.0.len() > 1, "input row + menu rows");
        let plain = strip_ansi(&rows.0.join("\n"));
        assert!(plain.contains("/chat"));
        assert!(plain.contains("/quit"));
        // 每条候选一行(菜单不挤成单行)。
        assert!(plain.lines().count() >= command::COMMANDS.len());
    }

    #[test]
    fn slash_menu_filters_by_prefix() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.input.text = "/mo".into();
        m.input.cursor = 3;
        m.update_slash_menu();
        let rows = render_input(&m, 120);
        let plain = strip_ansi(&rows.0.join("\n"));
        assert!(plain.contains("/models"));
        assert!(plain.contains("/model"));
        assert!(!plain.contains("/chat"));
        // 选中项带 ▸(候选序:models 在前,selected=0)。
        assert!(plain.contains("▸ /models"));
    }

    #[test]
    fn slash_menu_reports_no_match() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.input.text = "/zzz".into();
        m.input.cursor = 4;
        m.update_slash_menu();
        let rows = render_input(&m, 120);
        assert!(strip_ansi(&rows.0.join("\n")).contains("无匹配"));
    }

    #[test]
    fn approval_wait_renders_warn_row() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        let (tx, _rx) = std::sync::mpsc::channel();
        m.set_pending_approval(
            rc_sandbox::ApprovalRequest {
                tool: "run_shell".into(),
                description: "rm -rf /".into(),
                args: serde_json::json!({}),
            },
            tx,
        );
        let live = render_live(&m, 80, now());
        let joined = strip_ansi(&live.join("\n"));
        assert!(joined.contains("[审批] run_shell: rm -rf /"));
        assert!(joined.contains("Y=允许 N=拒绝 A=本会话允许"));
        // 无等待行时不出现该提示。
        m.approval_wait = None;
        let live = render_live(&m, 80, now());
        assert!(!strip_ansi(&live.join("\n")).contains("[审批]"));
    }

    #[test]
    fn guard_wait_renders_0_3_hint() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        let (tx, _rx) = std::sync::mpsc::channel();
        m.set_pending_guard(
            rc_sandbox::GuardRequest {
                tool: "run_shell".into(),
                reason: "high risk".into(),
                command: None,
                path: None,
            },
            tx,
        );
        let live = render_live(&m, 80, now());
        let joined = strip_ansi(&live.join("\n"));
        assert!(joined.contains("[审批] run_shell: high risk"));
        assert!(joined.contains("0=拒绝 1=仅本次 2=本会话 3=永久"));
        assert!(!joined.contains("Y=允许"), "guard hint must not show approval keys");
    }

    #[test]
    fn working_indicator_shows_while_running_without_stream() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.phase = Phase::Running;
        m.started_at = Some(Instant::now());
        let out = render_live(&m, 80, Instant::now() + Duration::from_millis(1_500));
        assert!(strip_ansi(&out[0]).contains("工作中"));
    }

    #[test]
    fn pending_steers_show_preview_up_to_three() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.enqueue_steer("one");
        m.enqueue_steer("two");
        m.enqueue_steer("three");
        m.enqueue_steer("four"); // 第 4 条不显示(最多 3 条)。
        let out = render_live(&m, 80, Instant::now());
        let joined = strip_ansi(&out.join("\n"));
        assert!(joined.contains("↳ 待注入: one"));
        assert!(joined.contains("↳ 待注入: two"));
        assert!(joined.contains("↳ 待注入: three"));
        assert!(!joined.contains("↳ 待注入: four"));
    }

    #[test]
    fn knight_rider_shows_during_busy_and_keeps_label() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.phase = Phase::Running;
        m.started_at = Some(Instant::now());
        let out = render_live(&m, 80, Instant::now() + Duration::from_millis(500));
        let joined = strip_ansi(&out.join("\n"));
        assert!(joined.contains("◈"), "knight rider lead char present: {joined}");
        assert!(joined.contains("工作中"), "busy label preserved");
        // 扫描灯随时间移动(lead 位置不同 → 帧内容不同)。
        let a = render_live(&m, 80, Instant::now() + Duration::from_millis(50));
        let b = render_live(&m, 80, Instant::now() + Duration::from_millis(250));
        assert_ne!(a, b, "scan light must move over time");
    }

    #[test]
    fn hud_shows_queued_badge_when_queue_has_messages() {
        // 正常顺序会话:已完成块后出现新 assistant 块 → 真实队列为空 → 不排队。
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.push_assistant_line("first".into(), LineStyle::Plain);
        m.push_assistant_line("second".into(), LineStyle::Plain);
        m.push_user_line("next".into());
        m.push_assistant_line("third".into(), LineStyle::Plain);
        assert!(
            !strip_ansi(&render_hud(&m, 80, now())).contains("QUEUED"),
            "sequential replies with an empty queue must not show QUEUED"
        );
        // 运行中 Tab 排队输入 → 徽标出现。
        m.queued_input.push_back("steer me".into());
        assert!(strip_ansi(&render_hud(&m, 80, now())).contains("QUEUED"));
        // pending_steers(运行中无 focus 提交的 steer)→ 同样触发徽标。
        let mut m2 = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m2.pending_steers.push_back("steer".into());
        assert!(strip_ansi(&render_hud(&m2, 80, now())).contains("QUEUED"));
        // 队列清空 → 徽标消失。
        m.queued_input.clear();
        assert!(!strip_ansi(&render_hud(&m, 80, now())).contains("QUEUED"));
        // 单条 assistant(无队列)始终无徽标。
        let mut m3 = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m3.push_assistant_line("first".into(), LineStyle::Plain);
        assert!(!strip_ansi(&render_hud(&m3, 80, now())).contains("QUEUED"));
    }

    #[test]
    fn hud_width_never_exceeds_terminal_with_title_and_queued() {
        // 回归:render_hud 先截断 plain 再拼标题段 + 徽标,总显示宽度不得超 width。
        let mut m = ReplModel::new("deepseek-v4-flash".into(), "gpt-5".into(), 128_000);
        m.set_title("Fixes the login crash on the billing page");
        m.phase = Phase::Running;
        m.started_at = Some(Instant::now());
        m.phase_note = Some("一个超长的阶段提示用于窄屏截断回归验证".into());
        m.pending_steers.push_back("steer".into());
        let width = 40;
        let hud = render_hud(&m, width, now());
        let stripped = strip_ansi(&hud);
        assert!(stripped.contains("QUEUED"), "badge must be present");
        assert!(stripped.contains("✻"), "title must be present");
        let w = UnicodeWidthStr::width(stripped.as_str());
        assert!(
            w <= width,
            "HUD display width {w} exceeds terminal {width}: {stripped:?}"
        );
    }

    #[test]
    fn live_tool_running_shows_pending_verb() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.apply_event(rc_proto::AgentEvent::ToolCall {
            id: "t1".into(),
            name: "read_file".into(),
            args: serde_json::json!({"path": "a.rs"}),
        });
        let live = render_live(&m, 80, now());
        assert!(strip_ansi(&live[0]).contains("Reading file..."));
    }

    #[test]
    fn live_tool_denied_renders_strikethrough() {
        let mut m = ReplModel::new("s".into(), "m".into(), 0);
        m.live_tool = Some(ToolView {
            id: "t1".into(),
            name: "run_shell".into(),
            args: "[command=rm -rf /etc]".into(),
            state: ToolState::Denied,
        });
        let lines = render_live(&m, 100, Instant::now());
        let joined = lines.join("\n");
        assert!(joined.contains("\x1b[9m"), "denied must render strikethrough");
    }

    #[test]
    fn live_tool_running_has_pending_prefix() {
        let mut m = ReplModel::new("s".into(), "m".into(), 0);
        m.live_tool = Some(ToolView {
            id: "t1".into(),
            name: "read_file".into(),
            args: "[path=a.txt]".into(),
            state: ToolState::Running,
        });
        let lines = render_live(&m, 100, Instant::now());
        assert!(lines.iter().any(|l| l.contains("~ Reading file...")));
    }

    #[test]
    fn focused_agent_shows_detail_line() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.apply_event(rc_proto::AgentEvent::AgentSpawned {
            id: "s1".into(),
            model: "deepseek".into(),
            role: "backend".into(),
            task: "fix api".into(),
        });
        m.focus_agent = Some("s1".into());
        let live = render_live(&m, 80, now());
        let joined = strip_ansi(&live.join("\n"));
        assert!(joined.contains("▣ agent s1"));
        assert!(joined.contains("steer ❯"));
    }

    #[test]
    fn agents_footer_shows_position_and_context() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.apply_event(rc_proto::AgentEvent::AgentSpawned {
            id: "a1".into(),
            model: "m".into(),
            role: "r".into(),
            task: "t".into(),
        });
        m.apply_event(rc_proto::AgentEvent::AgentSpawned {
            id: "a2".into(),
            model: "m".into(),
            role: "r".into(),
            task: "t".into(),
        });
        m.focus_agent = Some("a2".into());
        let rows = render_agents(&m, 120);
        let plain = strip_ansi(&rows.join("\n"));
        assert!(plain.contains("a2 (2 of 2)"), "footer shows focused idx: {plain}");
        // 单 agent 无 footer(不占行)。
        m.agents.remove("a1");
        let rows = render_agents(&m, 120);
        let plain = strip_ansi(&rows.join("\n"));
        assert!(!plain.contains("of 1"), "no footer with 1 agent: {plain}");
    }

    #[test]
    fn status_shows_risk_mode() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.risk_mode = rc_router::risk::RiskMode::Manual;
        let s = strip_ansi(&render_status(&m, 80));
        assert!(s.contains("manual"));
        assert!(s.contains("shift+tab to cycle"));
        assert!(s.contains("⏵⏵"));
    }

    #[test]
    fn separator_line_renders_full_width() {
        let sep = strip_ansi(&render_separator(20));
        assert_eq!(sep, "─".repeat(20));
    }

    #[test]
    fn agent_board_lists_agents_with_status() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.apply_event(rc_proto::AgentEvent::AgentSpawned {
            id: "s1".into(),
            model: "deepseek".into(),
            role: "backend".into(),
            task: "fix api".into(),
        });
        m.apply_event(rc_proto::AgentEvent::AgentSpawned {
            id: "s2".into(),
            model: "qwen".into(),
            role: "frontend".into(),
            task: "build page".into(),
        });
        let board = render_agents(&m, 80);
        // 2 个 agent 行 + 1 行子代理导航 footer(多 agent 时追加)。
        assert_eq!(board.len(), 3);
        let joined = strip_ansi(&board.join("\n"));
        assert!(joined.contains("agent s1"));
        assert!(joined.contains("agent s2"));
        assert!(joined.contains("deepseek"));
        // 聚焦 s1 时标记 focused。
        m.focus_agent = Some("s1".into());
        let board = render_agents(&m, 80);
        assert!(strip_ansi(&board[0]).contains("focused"));
    }

    #[test]
    fn agent_board_empty_when_no_agents() {
        let m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        assert!(render_agents(&m, 80).is_empty());
    }

    #[test]
    fn todo_status_shows_stats_and_recent() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.todo.add("全面功能验证");
        m.todo.add("补全 skill 网络");
        m.todo.add("主界面子代理看板");
        m.todo.set("全面功能验证", rc_orchestrate::todo::TodoStatus::Done);
        m.todo.set("补全 skill 网络", rc_orchestrate::todo::TodoStatus::InProgress);
        let out = render_todo(&m, 80);
        assert_eq!(out.len(), 3); // 统计行 + 2 recent
        let joined = strip_ansi(&out.join("\n"));
        assert!(joined.contains("3 tasks (1 done, 1 in progress, 1 open)"));
        assert!(joined.contains("◼ 补全 skill 网络"));
        assert!(joined.contains("◻ 主界面子代理看板"));
    }

    #[test]
    fn todo_empty_renders_nothing() {
        let m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        assert!(render_todo(&m, 80).is_empty());
    }

    #[test]
    fn input_row_has_prompt_and_cursor() {
        let m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        let (input, col) = render_input(&m, 80);
        assert_eq!(input, vec!["❯ ".to_string()]);
        assert_eq!(col, 2);
    }

    #[test]
    fn render_shows_recent_output_in_scroll_area() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.push_line("one".into(), LineStyle::Plain);
        m.push_line("two".into(), LineStyle::Plain);
        let frame = render(&m, 80, 12, now());
        // 整屏行里能看到最近的输出 + 底部钉行。
        let all = strip_ansi(&frame.lines.join("\n"));
        assert!(all.contains("two"), "recent output visible: {all:?}");
        assert!(all.contains("❯"), "input prompt present");
    }

    #[test]
    fn scrolled_window_shows_older_lines_with_gutter_dot() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        for i in 0..30 {
            m.push_line(format!("line {i:02}"), LineStyle::Plain);
        }
        m.push_user_line("USER MSG".into());
        // 贴底:最新内容可见,最早内容不可见。
        let frame = render(&m, 80, 20, now());
        let bottom = strip_ansi(&frame.lines.join("\n"));
        assert!(bottom.contains("USER MSG"), "bottom shows newest");
        assert!(!bottom.contains("line 00"), "oldest hidden at bottom");
        // 导航点:用户消息文本行前有 `· `,其上的分隔行无点。
        let user_row = frame.lines.iter().find(|l| l.contains("USER MSG")).unwrap();
        assert!(user_row.starts_with("· "), "gutter dot precedes user msg: {user_row:?}");
        // 上滚到顶:最早行可见。
        m.scroll_up(100);
        let frame = render(&m, 80, 20, now());
        let scrolled = strip_ansi(&frame.lines.join("\n"));
        assert!(scrolled.contains("line 00"), "scrolled view shows oldest");
    }

    #[test]
    fn completion_flashes_in_live_for_2_4s() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.last_elapsed = 26;
        let done_at = now();
        m.done_at = Some(done_at);
        let frame = render(&m, 80, 12, done_at + Duration::from_millis(100));
        assert!(strip_ansi(&frame.lines.join("\n")).contains("完成"));
        // 超过 2.4s 后不再闪烁。
        let later = render(&m, 80, 12, done_at + Duration::from_millis(2_500));
        assert!(!strip_ansi(&later.lines.join("\n")).contains("完成"));
    }

    fn build_tree(m: &mut ReplModel) {
        m.apply_event(rc_proto::AgentEvent::OrchestratorPlan {
            node_id: "root".into(),
            plan: "build an app\n- s1 backend".into(),
        });
        m.apply_event(rc_proto::AgentEvent::OrchestratorDispatch {
            parent_id: "root".into(),
            child_id: "s1".into(),
            prompt: "backend api".into(),
            model: "deepseek-v4".into(),
        });
        m.apply_event(rc_proto::AgentEvent::OrchestratorDispatch {
            parent_id: "s1".into(),
            child_id: "s1-1".into(),
            prompt: "api tests".into(),
            model: "qwen3".into(),
        });
        m.apply_event(rc_proto::AgentEvent::OrchestratorResult {
            node_id: "s1".into(),
            status: "ok".into(),
            summary: "wrote api".into(),
        });
    }

    #[test]
    fn tree_board_renders_indented_with_status_and_model() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        build_tree(&mut m);
        let out = render_tree(&m, 120, now());
        assert_eq!(out.len(), 4); // 头 + root + s1 + s1-1
        let joined = strip_ansi(&out.join("\n"));
        assert!(joined.contains("任务树 · 2 子任务"));
        assert!(joined.contains("✓ backend api (deepseek-v4)"));
        assert!(joined.contains("    ✻ api tests (qwen3)")); // 第 3 层缩进 4 格,派发后 Running
    }

    #[test]
    fn tree_board_collapses_to_one_line() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        build_tree(&mut m);
        m.tree_visible = false;
        let out = render_tree(&m, 120, now());
        assert_eq!(out.len(), 1);
        let joined = strip_ansi(&out[0]);
        assert!(joined.contains("任务树 · 2 子任务 · 1 done"));
        assert!(joined.contains("Ctrl+t 展开"));
    }

    #[test]
    fn tree_board_empty_falls_back_to_agent_board() {
        // 无任务树时 render_tree 返回空 → render() 用 agent 看板。
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        assert!(render_tree(&m, 120, now()).is_empty());
        m.apply_event(rc_proto::AgentEvent::AgentSpawned {
            id: "a1".into(),
            model: "deepseek".into(),
            role: "coder".into(),
            task: "build".into(),
        });
        assert!(render_agents(&m, 120).len() == 1);
    }

    #[test]
    fn live_shows_phase_note_when_running() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.phase = Phase::Running;
        m.started_at = Some(Instant::now());
        m.phase_note = Some("拆解".into());
        let out = render_live(&m, 80, Instant::now() + Duration::from_millis(1_500));
        assert!(strip_ansi(&out[0]).contains("拆解"));
        // 无 phase_note 时回退"工作中"。
        m.phase_note = None;
        let out = render_live(&m, 80, Instant::now() + Duration::from_millis(1_500));
        assert!(strip_ansi(&out[0]).contains("工作中"));
    }

    #[test]
    fn tree_running_node_pulses_over_time() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        build_tree(&mut m); // s1-1 是 Running
        let t0 = Instant::now();
        m.started_at = Some(t0);
        let a = render_tree(&m, 120, t0).join("\n");
        let b = render_tree(&m, 120, t0 + Duration::from_millis(400)).join("\n");
        // running 节点在 400ms 间隔下颜色不同(呼吸脉冲)。
        assert_ne!(a, b, "running tree node should pulse");
    }

    #[test]
    fn model_picker_renders_provider_model_and_marker() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        use crate::repl::env::ModelPickerEntry;
        m.open_model_picker(vec![
            ModelPickerEntry { id: "d".into(), provider: "deepseek".into(), model: "ds-v4-flash".into(), active: true, reasoning: 52.0, coding: 75.0, frontend: 64.0, backend: 64.0 },
            ModelPickerEntry { id: "o".into(), provider: "opencode".into(), model: "ds-v4-flash".into(), active: false, reasoning: 40.0, coding: 55.0, frontend: 50.0, backend: 50.0 },
        ]);
        let rows = render_model_picker(&m, 120);
        let joined = strip_ansi(&rows.join("\n"));
        // 供应渠道复合标识:deepseek/ds-v4-flash ≠ opencode/ds-v4-flash。
        assert!(joined.contains("deepseek/ds-v4-flash"));
        assert!(joined.contains("opencode/ds-v4-flash"));
        // 能力标注:编75 → ⬆(强),编55 → ➖(中)。
        assert!(joined.contains("⬆"));
        assert!(joined.contains("➖"));
        assert!(joined.contains("编75"));
        // active 标记。
        assert!(joined.contains("[active]"));
        // 选中项带 ▸。
        assert!(rows[1].contains("▸"));
    }

    #[test]
    fn session_picker_renders_entries_with_selected_marker() {
        use crate::repl::model::SessionEntry;
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.open_session_picker(vec![
            SessionEntry { id: "1111aaaa".into(), short_id: "1111aaaa".into(), summary: "build api".into(), updated_at: "t".into() },
            SessionEntry { id: "2222bbbb".into(), short_id: "2222bbbb".into(), summary: "fix tests".into(), updated_at: "u".into() },
        ]);
        let rows = render_session_picker(&m, 120);
        let joined = strip_ansi(&rows.join("\n"));
        assert!(joined.contains("会话选择器"));
        assert!(joined.contains("1111aaaa · build api · t"));
        assert!(joined.contains("2222bbbb · fix tests · u"));
        // 选中项带 ▸。
        assert!(rows[1].contains("▸"));
    }

    #[test]
    fn ctrl_t_toggles_tree_visibility_via_action() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        build_tree(&mut m);
        assert!(m.tree_visible);
        // handle_key 只返回动作;翻转在 main loop。这里验证动作产生 + 翻转效果。
        let action = crate::repl::r#loop::handle_key(
            &mut m,
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('t'),
                crossterm::event::KeyModifiers::CONTROL,
            ),
        );
        assert!(matches!(action, crate::repl::r#loop::Action::ToggleTree));
    }

    #[test]
    fn user_line_renders_bold_peach_with_separator() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.push_user_line("my task".into());
        let frame = render(&m, 80, 12, now());
        let raw = frame.lines.join("\n");
        let pos = raw.find("my task").unwrap();
        // 单 agent(无 focus)用户行 BOLD + PRIMARY(peach)回退色。
        assert!(raw[..pos].contains("\x1b[1m"));
        assert!(raw[..pos].contains("\x1b[38;2;250;178;131m"));
        // 用户行前插了 ─ 分隔线(消息流层次)。
        assert!(raw[..pos].contains("─"));
    }

    #[test]
    fn user_line_border_uses_focused_agent_color() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.apply_event(rc_proto::AgentEvent::AgentSpawned {
            id: "a1".into(),
            model: "m".into(),
            role: "r".into(),
            task: "t".into(),
        });
        m.focus_agent = Some("a1".into());
        m.push_user_line("my task".into());
        let frame = render(&m, 80, 12, now());
        let color = agent_color("a1", None);
        // 用户消息文本:紧邻 "my task" 之前是 BOLD + agent 色(不再硬编码 PRIMARY)。
        let user_line = frame
            .lines
            .iter()
            .find(|l| l.contains("my task"))
            .expect("user message rendered");
        assert!(
            user_line.contains(&format!("\x1b[1m{color}my task{RESET}")),
            "user text must use focused agent color: {user_line:?}"
        );
        // 消息流分隔线也用 agent 色。
        let idx = frame
            .lines
            .iter()
            .position(|l| l.contains("my task"))
            .expect("user message present");
        let sep = &frame.lines[idx - 1];
        assert!(sep.contains(color), "user separator uses agent color: {sep:?}");
        // focus 的 agent 色通常非 PRIMARY(哈希取模 7);若是 PRIMARY 则断言等价通过。
        if color != PRIMARY {
            assert!(!user_line.contains(PRIMARY), "must not hardcode PRIMARY");
        }
    }

    #[test]
    fn knight_rider_small_width_truncates_without_broken_ansi() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.phase = Phase::Running;
        m.started_at = Some(Instant::now());
        m.phase_note = Some("一个超长的阶段提示用于窄屏截断回归验证".into());
        let out = render_live(&m, 20, Instant::now() + Duration::from_millis(500));
        assert_eq!(out.len(), 1, "busy rider is the only live row");
        let line = &out[0];
        let re = regex::Regex::new(r"\x1b\[[0-9;]*m").unwrap();
        let stripped = re.replace_all(line, "");
        // 截断(无色先行再上色)后:剥离合法 CSI 不应残留半截转义。
        assert!(!stripped.contains("\x1b["), "dangling escape in {line:?}");
        // 打开过的色码必须有 RESET 闭合。
        if line.contains("\x1b[") {
            assert!(line.contains(RESET), "opened color must be closed: {line:?}");
        }
        // 行尾不得停在半截转义上(裸 `\x1b[` 会在上一步剥离后残留 → 已覆盖)。
        assert!(
            !line.ends_with("\x1b["),
            "line ends mid-escape: {line:?}"
        );
        // 实际显示宽度不超 20。
        let w = UnicodeWidthStr::width(stripped.as_ref());
        assert!(w <= 20, "rendered width {w} exceeds 20: {line:?}");
        // 长标签确实走了截断路径(带省略号)。
        assert!(
            stripped.contains('…'),
            "long label should be truncated with ellipsis: {line:?}"
        );
    }

    #[test]
    fn agent_line_small_width_truncates_without_broken_ansi() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.apply_event(rc_proto::AgentEvent::AgentSpawned {
            id: "a1".into(),
            model: "deepseek-v4-flash".into(),
            role: "backend".into(),
            task: "一个超长的任务描述用于窄屏截断回归验证".into(),
        });
        let rows = render_agents(&m, 20);
        let re = regex::Regex::new(r"\x1b\[[0-9;]*m").unwrap();
        for row in rows {
            let stripped = re.replace_all(&row, "");
            assert!(!stripped.contains("\x1b["), "dangling escape in {row:?}");
            assert!(
                UnicodeWidthStr::width(stripped.as_ref()) <= 20,
                "agent row exceeds 20 cells: {row:?}"
            );
        }
    }

    #[test]
    fn supervisor_line_paints_red() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.push_supervisor_line("s1 高风险".into());
        let frame = render(&m, 80, 12, now());
        let raw = frame.lines.join("\n");
        let pos = raw.find("[监督]").unwrap();
        assert!(raw[..pos].contains(RED));
    }

    #[test]
    fn hud_shows_turn_count() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.push_user_line("a".into());
        m.push_user_line("b".into());
        let hud = render_hud(&m, 80, now());
        assert!(strip_ansi(&hud).contains("2 turns"));
    }

    #[test]
    fn hud_shows_title_when_set() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.set_title("Fixes the login crash");
        let hud = render_hud(&m, 80, now());
        assert!(strip_ansi(&hud).contains("✻ Fixes the login crash"));
    }

    #[test]
    fn hud_omits_title_when_none() {
        let m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        let hud = render_hud(&m, 80, now());
        assert!(!strip_ansi(&hud).contains("✻"));
    }
}
