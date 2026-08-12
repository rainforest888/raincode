//! Pure REPL state. Feed it real `AgentEvent`s via `apply_event`, then
//! `render` to get terminal lines. No I/O here — the shell owns the terminal.
//! file-level copy from crates/rc-tui/src/repl/model.rs

use std::collections::{BTreeMap, VecDeque};
use std::time::Instant;

use rc_proto::AgentEvent;
use rc_sandbox::{ApprovalDecision, ApprovalRequest, GuardConsent, GuardRequest};

use crate::repl::command;
use crate::repl::editor::InputEditor;
use crate::repl::fmt::{format_elapsed, format_tokens, truncate_line, truncate_output};

pub const OUTPUT_BOUND: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Running,
}

#[derive(Debug, Clone)]
pub struct AgentView {
    pub role: String,
    pub model: String,
    pub phase: String,
    pub tool: Option<String>,
    pub tokens: u64,
    pub elapsed_ms: u64,
    pub verdict: Option<String>,
    pub done: bool,
    pub failed: bool,
}

impl AgentView {
    fn new(role: String, model: String) -> Self {
        Self {
            role,
            model,
            phase: "spawned".into(),
            tool: None,
            tokens: 0,
            elapsed_ms: 0,
            verdict: None,
            done: false,
            failed: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextView {
    pub used: u64,
    pub limit: u64,
    pub pct: u8,
}

impl Default for ContextView {
    fn default() -> Self {
        Self {
            used: 0,
            limit: 128_000,
            pct: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineStyle {
    Plain,
    Dim,
    Accent,
    Warn,
    Error,
    Success,
    Agent,
    Tool,
    /// 监督 agent 输出(红色,语义上区别于 Error)。
    Supervisor,
    /// 动态颜色：直接携带 ANSI 前景色代码（如 7 色 agent 轮换结果）。
    Custom(&'static str),
}

/// 消息流层次:一行在对话里的角色。渲染用它做用户消息高亮 + 回合分隔。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    User,
    Assistant,
    Tool,
    Agent,
    System,
}

#[derive(Debug, Clone)]
pub struct Line {
    pub text: String,
    pub style: LineStyle,
    pub kind: LineKind,
}

/// Live tool-call row: the current tool being executed (claude-code inline
/// 3-state row). Lives above the HUD while running, commits to scrollback on
/// ToolResult.
#[derive(Debug, Clone)]
pub struct ToolView {
    pub id: String,
    pub name: String,
    pub args: String,
    pub state: ToolState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolState {
    Running,
    Ok,
    Failed,
    Denied,
}

/// Primitive args → `[k=v, k=v]` preview (opencode style); complex values
/// collapse to their key name; empty object → `""`.
pub fn tool_args_preview(args: &serde_json::Value) -> String {
    match args {
        serde_json::Value::Object(map) => {
            let parts: Vec<String> = map
                .iter()
                .map(|(k, v)| match v {
                    serde_json::Value::String(s) => format!("{k}={s}"),
                    serde_json::Value::Number(n) => format!("{k}={n}"),
                    serde_json::Value::Bool(b) => format!("{k}={b}"),
                    _ => k.clone(),
                })
                .collect();
            if parts.is_empty() {
                String::new()
            } else {
                format!("[{}]", parts.join(", "))
            }
        }
        _ => String::new(),
    }
}

/// Success/failure icon for a committed tool line.
pub fn icon_ok(failed: bool) -> &'static str {
    if failed {
        "✗"
    } else {
        "✓"
    }
}

/// 从 `Done.usage` 提取 (input, output, total) tokens;缺省 0。
/// 兼容 OpenAPI(prompt/completion/total_tokens)与通用(input/output_tokens)。
pub fn usage_tokens(usage: &serde_json::Value) -> (u64, u64, u64) {
    let num = |k: &str| usage.get(k).and_then(serde_json::Value::as_u64).unwrap_or(0);
    let input = num("input_tokens").max(num("prompt_tokens"));
    let output = num("output_tokens").max(num("completion_tokens"));
    let total = num("total_tokens");
    let total = if total > 0 { total } else { input + output };
    (input, output, total)
}

/// 从写文件类工具参数里取文件路径(path/file/file_path)。
fn tool_file_path(args: &serde_json::Value) -> Option<String> {
    if !matches!(args, serde_json::Value::Object(_)) {
        return None;
    }
    for key in ["path", "file", "file_path"] {
        if let Some(s) = args.get(key).and_then(serde_json::Value::as_str) {
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }
    None
}

#[derive(Debug, Clone)]
pub enum PendingPrompt {
    Approval {
        req: ApprovalRequest,
        reply: std::sync::mpsc::Sender<ApprovalDecision>,
    },
    Question {
        text: String,
        reply: std::sync::mpsc::Sender<String>,
        secret: bool,
    },
    /// 授权闸:高危操作的四选一同意(0=拒绝 Deny / 1=仅本次 Once / 2=本会话 Session / 3=永久 Forever)。
    Guard {
        req: GuardRequest,
        reply: std::sync::mpsc::Sender<GuardConsent>,
    },
}

/// 斜杠命令补全浮层状态:过滤后的候选 + 选中索引(↑↓ 循环,Enter 填入 /cmd )。
#[derive(Debug, Clone)]
pub struct SlashMenu {
    pub items: Vec<&'static command::CommandSpec>,
    pub selected: usize,
}

/// 交互式模型选择器(/model):搜索 + ↑↓ + Enter,能力标注(⬆/⬇) + 供应渠道区分。
#[derive(Debug, Clone)]
pub struct ModelPicker {
    pub all: Vec<crate::repl::env::ModelPickerEntry>,
    /// 匹配 query 的条目在 `all` 里的下标。
    pub filtered: Vec<usize>,
    pub query: String,
    /// 选中项在 `filtered` 里的下标。
    pub selected: usize,
}

impl ModelPicker {
    fn refilter(&mut self) {
        let q = self.query.trim().to_lowercase();
        self.filtered = self
            .all
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                q.is_empty()
                    || e.model.to_lowercase().contains(&q)
                    || e.provider.to_lowercase().contains(&q)
                    || format!("{}/{}", e.provider, e.model).to_lowercase().contains(&q)
            })
            .map(|(i, _)| i)
            .collect();
        if self.selected >= self.filtered.len() {
            self.selected = 0;
        }
    }

    pub fn selected_entry(&self) -> Option<&crate::repl::env::ModelPickerEntry> {
        self.filtered.get(self.selected).map(|&i| &self.all[i])
    }
}

/// 会话选择器条目(/resume):历史会话列表的一行。
#[derive(Debug, Clone)]
pub struct SessionEntry {
    pub id: String,
    pub short_id: String,
    pub summary: String,
    pub updated_at: String,
}

/// 交互式会话选择器(/resume):搜索 + ↑↓ + Enter,模式同 ModelPicker。
#[derive(Debug, Clone)]
pub struct SessionPicker {
    pub all: Vec<SessionEntry>,
    /// 匹配 query 的条目在 `all` 里的下标。
    pub filtered: Vec<usize>,
    pub query: String,
    /// 选中项在 `filtered` 里的下标。
    pub selected: usize,
}

impl SessionPicker {
    pub fn refilter(&mut self) {
        let q = self.query.trim().to_lowercase();
        self.filtered = self
            .all
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                q.is_empty()
                    || e.summary.to_lowercase().contains(&q)
                    || e.short_id.contains(&q)
            })
            .map(|(i, _)| i)
            .collect();
        if self.selected >= self.filtered.len() {
            self.selected = self.filtered.len().saturating_sub(1);
        }
    }

    pub fn next(&mut self) {
        if !self.filtered.is_empty() {
            self.selected = (self.selected + 1) % self.filtered.len();
        }
    }

    pub fn prev(&mut self) {
        if !self.filtered.is_empty() {
            self.selected = (self.selected + self.filtered.len() - 1) % self.filtered.len();
        }
    }

    pub fn selected_entry(&self) -> Option<&SessionEntry> {
        self.filtered.get(self.selected).map(|&i| &self.all[i])
    }
}

#[derive(Clone)]
pub struct ReplModel {
    pub phase: Phase,
    pub session_id: String,
    pub model: String,
    pub agents: BTreeMap<String, AgentView>,
    pub context: ContextView,
    pub output: VecDeque<Line>,
    pub streaming: Option<String>,
    /// 正在执行的工具（live 行）。
    pub live_tool: Option<ToolView>,
    /// 进行中的 thinking 标题（live 行）。
    pub live_thinking: Option<String>,
    /// 当前风险模式（Shift+Tab 循环切换；approval hook 经 risk_gate 映射审批行为）。
    pub risk_mode: rc_router::risk::RiskMode,
    pub input: InputEditor,
    pub pending: Option<PendingPrompt>,
    /// 等待中的审批(工具+描述),live 行高亮显示;回答后清空。
    pub approval_wait: Option<String>,
    pub started_at: Option<Instant>,
    pub done_at: Option<Instant>,
    pub last_elapsed: u64,
    pub task_count: u64,
    /// 当前选中的 agent id（Tab/shift+Tab 切换;Enter 进入其详情交互）。
    pub focus_agent: Option<String>,
    /// 本轮 run 是否产生过流式 Token 回答(用于 Done 时避免 summary 重复)。
    pub streamed_this_run: bool,
    /// 当前对话 agent 的待办清单(TUI 底部显示)。
    pub todo: rc_orchestrate::todo::TodoList,
    /// 自动编排的任务树(Orchestrator* 事件驱动建树;TUI 常驻看板)。
    pub tree: rc_orchestrate::tree::TaskTree,
    /// 任务树看板是否展开(Ctrl+t 切换;折叠时只显一行状态)。
    pub tree_visible: bool,
    /// 斜杠命令多行补全浮层(输入以 / 开头且无空格时开启)。
    pub slash_menu: Option<SlashMenu>,
    /// 交互式模型选择器(/model):搜索 + ↑↓ + Enter。
    pub model_picker: Option<ModelPicker>,
    /// 交互式会话选择器(/resume):搜索 + ↑↓ + Enter。
    pub session_picker: Option<SessionPicker>,
    /// 对话回合数(用户提交一次 +1;HUD 显示)。
    pub turn_count: u64,
    /// 本轮 tool 调用数(完成汇报用;start_run 时清零)。
    pub tool_calls: u64,
    /// 本轮写过的文件(完成汇报用;start_run 时清零,上限 10)。
    pub files_touched: Vec<String>,
    /// 实时阶段提示(拆解中/执行中…;来自 PhaseChanged 与 AgentSpawned,live 行显示)。
    pub phase_note: Option<String>,
    /// 完整思维链(来自 Done.reasoning),Ctrl+O 展开查看。
    pub reasoning_chain: Option<String>,
    /// reasoning 是否展开。
    pub reasoning_expanded: bool,
    /// 动态会话标题(首条 Done 懒生成首行;/title 手动覆盖)。
    pub title: Option<String>,
    /// 对话区滚动偏移(0 = 贴底)。
    pub scroll_offset: usize,
    /// 自动滚动贴底锁:true = 新内容自动滚底;用户上滚解锁。
    pub autoscroll: bool,
    /// 运行中提交但未注入的 steer 队列(codex pending_steers)。下一次 Agent
    /// 回合的 steer_rx 经 core 的 steering checkpoint 拾取。
    pub pending_steers: VecDeque<String>,
    /// Tab 排队消息(任务完成时按序重提交为新任务)。
    pub queued_input: VecDeque<String>,
    /// Esc 两级 backtrack 布防:空闲+空输入时首次 Esc 置 true,二次 Esc 恢复上条消息。
    pub backtrack_armed: bool,
    /// InterruptManager FIFO:流式期间到达的审批/提问/工具结果事件先入队,流结束按序 flush。
    pub interrupt_queue: VecDeque<AgentEvent>,
    /// 工作区根目录(绝对路径,用于汇报里把相对文件路径补全成绝对路径)。
    /// repl_command 启动时从 env.workspace() 设置;测试留空 = 相对路径原样显示。
    pub workspace: String,
}

impl ReplModel {
    pub fn new(session_id: String, model: String, context_window: u64) -> Self {
        let context = ContextView {
            used: 0,
            // 空闲时(未收到 ContextUpdate)也显示真实模型上下文,而非 128K 兜底。
            limit: if context_window > 0 { context_window } else { 128_000 },
            pct: 0,
        };
        Self {
            phase: Phase::Idle,
            session_id,
            model,
            agents: BTreeMap::new(),
            context,
            output: VecDeque::new(),
            streaming: None,
            live_tool: None,
            live_thinking: None,
            risk_mode: rc_router::risk::RiskMode::Ask,
            input: InputEditor::new(),
            pending: None,
            approval_wait: None,
            started_at: None,
            done_at: None,
            last_elapsed: 0,
            task_count: 0,
            focus_agent: None,
            streamed_this_run: false,
            todo: rc_orchestrate::todo::TodoList::default(),
            tree: rc_orchestrate::tree::TaskTree::default(),
            tree_visible: true,
            slash_menu: None,
            model_picker: None,
            session_picker: None,
            turn_count: 0,
            tool_calls: 0,
            files_touched: Vec::new(),
            phase_note: None,
            reasoning_chain: None,
            reasoning_expanded: false,
            title: None,
            scroll_offset: 0,
            autoscroll: true,
            pending_steers: VecDeque::new(),
            queued_input: VecDeque::new(),
            backtrack_armed: false,
            interrupt_queue: VecDeque::new(),
            workspace: String::new(),
        }
    }

    /// 懒生成标题:首条 Done 的摘要首行(截断 40 字符);已有标题不覆盖。
    pub fn maybe_auto_title(&mut self, summary: &str) {
        if self.title.is_some() {
            return;
        }
        let first = summary.lines().next().unwrap_or("").trim();
        if first.is_empty() {
            return;
        }
        let t: String = first.chars().take(40).collect();
        if !t.is_empty() {
            self.title = Some(t);
        }
    }

    /// 手动设置标题(/title <name>),截断 40 字符;空串清除。
    pub fn set_title(&mut self, t: &str) {
        let t: String = t.trim().chars().take(40).collect();
        self.title = if t.is_empty() { None } else { Some(t) };
    }

    /// 把工具上报的相对路径补全为绝对路径(对齐 Claude Code:写完文件用户应能
    /// 直接看到完整落盘位置)。workspace 为空(测试/未设置)时原样返回。
    pub fn abs_file_path(&self, rel: &str) -> String {
        if self.workspace.is_empty() || rel.is_empty() {
            return rel.to_string();
        }
        let ws = std::path::Path::new(&self.workspace);
        ws.join(rel).to_string_lossy().to_string()
    }

    /// 一轮 run 起点(Run/Route/Autonomous):重置本轮统计(工具调用/文件/token
    /// 标记),进入 Running。跨子代理的 route/autonomous 会累计整轮统计。
    pub fn start_run(&mut self) {
        self.phase = Phase::Running;
        self.started_at = Some(Instant::now());
        self.done_at = None;
        self.streamed_this_run = false;
        self.tool_calls = 0;
        self.files_touched.clear();
        self.phase_note = None;
        // 新 run 清掉上一条任务的完整思维链:否则新 run 期间 live 栈会渲染旧任务
        // 的 `↳ 推理: ...`,Ctrl+O 展开的也是旧链。
        self.reasoning_chain = None;
        self.reasoning_expanded = false;
    }

    /// 运行中提交 → 进 pending_steers(steer 队列)。下一轮 Agent 回合的
    /// steer_rx 经 core 的 steering checkpoint 拾取注入,不在此消费。
    pub fn enqueue_steer(&mut self, text: &str) {
        self.pending_steers.push_back(text.to_string());
    }

    /// 取走全部 pending_steers(FIFO)。
    pub fn drain_steers(&mut self) -> Vec<String> {
        self.pending_steers.drain(..).collect()
    }

    /// 是否正处 Agent 回合运行中(phase == Running)。驱动 Tab 排队/Esc 分派。
    pub fn agent_turn_running(&self) -> bool {
        self.phase == Phase::Running
    }

    /// InterruptManager 保序:流式期间(streaming 缓冲区非空)审批/提问/工具结果
    /// 事件入队,不打断流;流结束后 flush 队列再 apply 当前事件。保证事件按到达序。
    pub fn defer_or_flush(&mut self, ev: AgentEvent) {
        if self.streaming.is_some() {
            self.interrupt_queue.push_back(ev);
        } else {
            self.flush_interrupts();
            self.apply_event(ev);
        }
    }

    /// 按 FIFO 顺序应用所有延迟事件(流已结束时调用)。
    pub fn flush_interrupts(&mut self) {
        while let Some(ev) = self.interrupt_queue.pop_front() {
            self.apply_event(ev);
        }
    }

    pub fn push_line(&mut self, text: String, style: LineStyle) {
        self.push_line_kind(text, style, LineKind::System);
    }

    /// 折叠/展开完整思维链(Ctrl+O)。
    pub fn toggle_reasoning(&mut self) {
        self.reasoning_expanded = !self.reasoning_expanded;
    }

    /// 取 live_thinking 最近 max 行(折叠显示用)。
    pub fn live_thinking_lines(&self, max: usize) -> Vec<&str> {
        self.live_thinking
            .as_deref()
            .map(|t| t.lines().rev().take(max).collect())
            .unwrap_or_default()
    }

    /// 按角色标记 push 一行(消息流层次渲染用)。
    pub fn push_line_kind(&mut self, text: String, style: LineStyle, kind: LineKind) {
        if self.output.len() >= OUTPUT_BOUND {
            self.output.pop_front();
        }
        self.output.push_back(Line { text, style, kind });
        // 自动滚动:新内容到达且 autoscroll(贴底锁)时把 offset 归零;
        // 用户上滚后(autoscroll=false)不打断其查看位置。
        if self.autoscroll {
            self.scroll_offset = 0;
        }
    }

    /// 上滚 n 行,解锁自动滚动(用户手动查看历史)。
    pub fn scroll_up(&mut self, n: usize) {
        self.scroll_offset += n;
        self.autoscroll = false;
    }

    /// 下滚 n 行(不越底);回到 offset 0 时重新贴底。
    pub fn scroll_down(&mut self, n: usize) {
        self.scroll_offset = self.scroll_offset.saturating_sub(n);
        if self.scroll_offset == 0 {
            self.autoscroll = true;
        }
    }

    /// 回到底部,重新贴底。
    pub fn scroll_to_bottom(&mut self) {
        self.scroll_offset = 0;
        self.autoscroll = true;
    }

    /// 每条 user 消息在 output 里的下标(左侧导航点)。
    pub fn message_markers(&self) -> Vec<usize> {
        self.output
            .iter()
            .enumerate()
            .filter(|(_, l)| l.kind == LineKind::User)
            .map(|(i, _)| i)
            .collect()
    }

    /// 监督 agent 输出行([监督] 前缀 + Supervisor 红色)。
    pub fn push_supervisor_line(&mut self, text: String) {
        self.push_line(format!("[监督] {text}"), LineStyle::Supervisor);
    }

    /// 用户消息行(› 前缀,BOLD+PRIMARY),并记一个新回合。
    pub fn push_user_line(&mut self, text: String) {
        self.turn_count += 1;
        self.push_line_kind(text, LineStyle::Accent, LineKind::User);
    }

    /// 助手消息行(流式回答/汇报)。
    pub fn push_assistant_line(&mut self, text: String, style: LineStyle) {
        self.push_line_kind(text, style, LineKind::Assistant);
    }

    /// 是否有排队的消息:真实队列 pending_steers(运行中提交的 steer)或
    /// queued_input(运行中 Tab 排队)非空才叫排队。正常顺序会话中前一个回答
    /// 之后紧跟新回答不构成排队 —— QUEUED 徽标反映"并行 agent 仍在挂起"的
    /// 规格意图(B8),而非按 output 行序做块推断。
    pub fn has_queued(&self) -> bool {
        !self.pending_steers.is_empty() || !self.queued_input.is_empty()
    }

    /// Accumulate a token/thinking delta into the streaming line buffer,
    /// flushing completed lines on newline.
    pub fn stream_delta(&mut self, text: &str, style: LineStyle) {
        let buf = self.streaming.get_or_insert_with(String::new);
        buf.push_str(text);
        if buf.contains('\n') {
            // Flush EVERY complete line (up to and including each '\n') as its
            // own output Line so the shell's row accounting stays exact; keep
            // only the trailing partial (after the last '\n') in `streaming`.
            let flushed = std::mem::take(&mut self.streaming).unwrap_or_default();
            let mut rest = String::new();
            let mut start = 0;
            while let Some(idx) = flushed[start..].find('\n') {
                let end = start + idx;
                // 流式回答是助手消息 → Assistant 角色。
                self.push_line_kind(flushed[start..end].to_string(), style, LineKind::Assistant);
                start = end + 1;
            }
            if start < flushed.len() {
                rest = flushed[start..].to_string();
            }
            if !rest.is_empty() {
                self.streaming = Some(rest);
            }
        }
    }

    pub fn stream_chat_delta(&mut self, text: &str) {
        self.stream_delta(text, LineStyle::Plain);
    }

    pub fn flush_stream(&mut self) {
        if let Some(buf) = self.streaming.take() {
            if !buf.is_empty() {
                self.push_line(buf, LineStyle::Plain);
            }
        }
    }

    pub fn apply_event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::Token { delta } => {
                self.streamed_this_run = true;
                self.stream_delta(&delta, LineStyle::Plain)
            }
            AgentEvent::Thinking { delta } => {
                // 累积进 live_thinking(不逐字写 scrollback);UI 端只显示最近几行。
                let buf = self.live_thinking.get_or_insert_with(String::new);
                buf.push_str(&delta);
            }
            AgentEvent::ToolCall { id, name, args, .. } => {
                self.flush_stream();
                self.live_thinking = None;
                self.tool_calls += 1;
                // 写文件类工具:记录本轮改动文件(完成汇报的关键文件)。
                if matches!(name.as_str(), "write_file" | "edit" | "apply_patch" | "patch") {
                    if let Some(path) = tool_file_path(&args) {
                        if !self.files_touched.contains(&path) && self.files_touched.len() < 10 {
                            self.files_touched.push(path);
                        }
                    }
                }
                let args_str = tool_args_preview(&args);
                self.live_tool = Some(ToolView {
                    id: id.clone(),
                    name: name.clone(),
                    args: args_str,
                    state: ToolState::Running,
                });
            }
            AgentEvent::ToolResult { id, name, ok, output, .. } => {
                self.flush_stream();
                // 兜底:工具完成即清等待行(审批在工具运行前已答,pending 已消费)。
                self.approval_wait = None;
                let failed = !ok;
                // 只清/更新与本次结果同 id 的 live 行;延迟 flush 的旧结果不得冲刷
                // 后续 ToolCall 新开的 live 行(否则新 spinner 被旧结果抹掉)。
                let is_current = self.live_tool.as_ref().is_some_and(|t| t.id == id);
                if let Some(t) = &mut self.live_tool {
                    if t.id == id {
                        t.state = if failed { ToolState::Failed } else { ToolState::Ok };
                    }
                }
                // 提交完成行进 scrollback。
                let out = truncate_output(&output, 3);
                let line = if out.is_empty() {
                    format!("{} {name}", icon_ok(failed))
                } else {
                    format!("{} {name}: {out}", icon_ok(failed))
                };
                self.push_line(line, if failed { LineStyle::Error } else { LineStyle::Success });
                if is_current {
                    self.live_tool = None;
                }
            }
            AgentEvent::AgentSpawned { id, model, role, task } => {
                self.phase = Phase::Running;
                self.started_at.get_or_insert_with(Instant::now);
                self.task_count += 1;
                self.done_at = None;
                self.streamed_this_run = false; // 新 run 重置流式标记
                self.phase_note = Some(format!("执行 {id}"));
                self.agents.insert(id.clone(), AgentView::new(role.clone(), model.clone()));
                self.flush_stream();
                self.push_line_kind(
                    format!("✻ agent {id} ({role}) [{model}] {task}"),
                    LineStyle::Agent,
                    LineKind::Agent,
                );
            }
            AgentEvent::AgentToolCall { id, tool, args_preview } => {
                if let Some(agent) = self.agents.get_mut(&id) {
                    agent.tool = Some(tool.clone());
                }
                self.flush_stream();
                let preview = truncate_output(&args_preview, 1);
                self.push_line_kind(
                    format!("✻ agent {id} → {tool} {preview}"),
                    LineStyle::Tool,
                    LineKind::Tool,
                );
            }
            AgentEvent::AgentStatus {
                id,
                phase,
                tokens,
                elapsed_ms,
            } => {
                if let Some(agent) = self.agents.get_mut(&id) {
                    agent.phase = phase;
                    agent.tokens = tokens;
                    agent.elapsed_ms = elapsed_ms;
                }
            }
            AgentEvent::AgentResult { id, verdict, tests, cost } => {
                if let Some(agent) = self.agents.get_mut(&id) {
                    agent.verdict = Some(verdict.clone());
                    agent.done = true;
                    agent.failed = verdict == "failed";
                }
                self.flush_stream();
                self.push_line(
                    format!("✓ agent {id} {verdict} (tests {tests} · cost {cost:.4})"),
                    if verdict == "failed" {
                        LineStyle::Error
                    } else {
                        LineStyle::Success
                    },
                );
            }
            AgentEvent::ContextUpdate { used, limit, pct, .. } => {
                self.context = ContextView { used, limit, pct };
            }
            AgentEvent::Done { summary, usage, reasoning, .. } => {
                self.phase = Phase::Idle;
                self.approval_wait = None;
                self.live_tool = None;
                self.live_thinking = None;
                self.reasoning_chain = reasoning.filter(|r| !r.trim().is_empty());
                self.reasoning_expanded = false;
                self.phase_note = None;
                // 本轮是否有过流式回答(含已被 newline 刷进 output 的部分)。
                // Token 增量的 `\n` 会把完整行刷出,`streaming` 归 None,但回答
                // 已在 output 里 —— 用 `streamed_this_run` 标记避免 Done.summary
                // 重复 push 同一段回答。
                let had_stream = self.streamed_this_run;
                self.flush_stream();
                let elapsed = self
                    .started_at
                    .map(|t| t.elapsed().as_secs())
                    .unwrap_or(0);
                self.last_elapsed = elapsed;
                // 结构化汇报块:标题 → 摘要(无流式时)→ 统计 → 关键文件。
                self.push_line(
                    format!("✓ 任务完成 · {}", format_elapsed(elapsed)),
                    LineStyle::Success,
                );
                if !had_stream {
                    // 对齐 Claude Code:非流式时把摘要完整展示(最多 20 行),不再砍到 3 行。
                    for l in truncate_output(&summary, 20).lines() {
                        let l = l.trim();
                        if !l.is_empty() {
                            self.push_line(format!("  {l}"), LineStyle::Plain);
                        }
                    }
                }
                // 统计行:tool 调用 / 文件 / tokens(全真实事件,缺省省略)。
                let (_, _, total) = usage.as_ref().map(usage_tokens).unwrap_or((0, 0, 0));
                let mut parts: Vec<String> = vec![format!("{} tool calls", self.tool_calls)];
                if !self.files_touched.is_empty() {
                    parts.push(format!("{} files", self.files_touched.len()));
                }
                if total > 0 {
                    parts.push(format!("{} tokens", format_tokens(total)));
                }
                self.push_line(format!("  · {}", parts.join(" · ")), LineStyle::Dim);
                if !self.files_touched.is_empty() {
                    // 绝对路径汇报(对齐 Claude Code:用户能直接看到文件落盘位置)。
                    let files: Vec<String> = self
                        .files_touched
                        .iter()
                        .take(5)
                        .map(|p| self.abs_file_path(p))
                        .collect();
                    self.push_line(format!("  · {}", files.join(", ")), LineStyle::Dim);
                    if self.files_touched.len() > 5 {
                        self.push_line(
                            format!("  · … 还有 {} 个文件", self.files_touched.len() - 5),
                            LineStyle::Dim,
                        );
                    }
                }
                // 动态标题:首个任务完成时,若标题空,用摘要首行(≤40 字符)。
                self.maybe_auto_title(&summary);
                self.streamed_this_run = false;
                self.done_at = Some(Instant::now());
                self.started_at = None;
            }
            AgentEvent::Error { message } => {
                self.phase = Phase::Idle;
                self.approval_wait = None;
                self.done_at = None;
                self.live_tool = None;
                self.live_thinking = None;
                self.phase_note = None;
                self.streamed_this_run = false;
                self.flush_stream();
                // 用户中断(core/route 的 "cancelled by user")→ 友好确认"已中断"。
                if message.to_lowercase().contains("cancelled") {
                    self.push_line("✂ 已中断".into(), LineStyle::Warn);
                } else {
                    self.push_line(format!("[error] {message}"), LineStyle::Error);
                }
                self.started_at = None;
            }
            AgentEvent::AskingApproval { tool, description, .. } => {
                // 仅信息性事件 → 只进 scrollback;真实等待行由 set_pending_approval 建立。
                self.flush_stream();
                self.push_line(
                    format!("[approval] {tool}: {description}"),
                    LineStyle::Warn,
                );
            }
            AgentEvent::AskingQuestion { question, .. } => {
                self.flush_stream();
                self.push_line(format!("[question] {question}"), LineStyle::Warn);
            }
            AgentEvent::SessionStarted { session_id } => {
                self.done_at = None;
                self.push_line(format!("[session] {session_id}"), LineStyle::Dim);
            }
            AgentEvent::PlanProposed { summary, .. } => {
                self.flush_stream();
                self.push_line(truncate_output(&summary, 4), LineStyle::Dim);
            }
            AgentEvent::PhaseChanged { phase, cycle, .. } => {
                self.flush_stream();
                self.phase_note = Some(phase.clone());
                self.push_line(format!("[phase] {phase} (cycle {cycle})"), LineStyle::Dim);
            }
            AgentEvent::ReviewProposed { verdict, reason, .. } => {
                self.flush_stream();
                self.push_line(format!("[review] {verdict}: {reason}"), LineStyle::Warn);
            }
            AgentEvent::SkillLoaded { name, .. } => {
                self.push_line(format!("✻ {name} 已加载"), LineStyle::Dim);
            }
            AgentEvent::SkillSuggested { .. } => {
                // 主动提示的技能不建议默认显示(刷屏);用户实际调用时才由
                // SkillLoaded 报"已加载"。
            }
            AgentEvent::McpToolList { server, tools } => {
                self.push_line(
                    format!("[mcp] {server}: {}", tools.join(", ")),
                    LineStyle::Dim,
                );
            }
            AgentEvent::OrchestratorPlan { node_id, plan } => {
                self.flush_stream();
                // 建任务树:确保根节点存在(desc=计划首行,status Running)。
                if !self.tree.nodes.contains_key(&node_id) {
                    let first = plan.lines().next().unwrap_or("").trim().to_string();
                    let desc: String = if first.is_empty() { "任务".into() } else { first };
                    self.tree.add(rc_orchestrate::tree::TaskNode::new(
                        node_id.clone(),
                        None,
                        desc,
                    ));
                }
                self.tree
                    .update_status(&node_id, rc_orchestrate::tree::TaskStatus::Running);
                self.push_line(
                    format!("[plan] {node_id}: {}", truncate_line(&plan, 60)),
                    LineStyle::Accent,
                );
            }
            AgentEvent::OrchestratorDispatch {
                parent_id,
                child_id,
                prompt,
                model,
                ..
            } => {
                self.flush_stream();
                // 派发 → 在父节点下加子任务(携带自动选中的模型)。
                let parent_depth = self
                    .tree
                    .nodes
                    .get(&parent_id)
                    .map(|n| n.depth)
                    .unwrap_or(1);
                let depth = (parent_depth + 1).min(3);
                self.tree.add(rc_orchestrate::tree::TaskNode {
                    id: child_id.clone(),
                    parent: Some(parent_id.clone()),
                    description: prompt.clone(),
                    model: Some(model.clone()),
                    skill: None,
                    status: rc_orchestrate::tree::TaskStatus::Running,
                    summary: None,
                    depth,
                });
                // 底部 todo:子任务派发即加入待办,标进行中。
                self.todo.add_with_id(&child_id, &truncate_line(&prompt, 40));
                self.todo
                    .set_by_id(&child_id, rc_orchestrate::todo::TodoStatus::InProgress);
                self.push_line(
                    format!("[派发] {child_id} ({model}) {prompt}"),
                    LineStyle::Tool,
                );
            }
            AgentEvent::OrchestratorResult { node_id, status, summary } => {
                self.flush_stream();
                // 结果 → 更新节点状态 + 摘要。
                self.tree.update_status(
                    &node_id,
                    if status == "ok" {
                        rc_orchestrate::tree::TaskStatus::Done
                    } else {
                        rc_orchestrate::tree::TaskStatus::Failed
                    },
                );
                self.tree.set_summary(&node_id, summary.clone());
                // 底部 todo:子任务结果回标完成/失败。
                self.todo.set_by_id(
                    &node_id,
                    if status == "ok" {
                        rc_orchestrate::todo::TodoStatus::Done
                    } else {
                        rc_orchestrate::todo::TodoStatus::Failed
                    },
                );
                self.push_line(
                    format!("[结果] {node_id} {status}: {summary}"),
                    if status == "ok" { LineStyle::Success } else { LineStyle::Error },
                );
            }
        }
    }

    pub fn set_pending_approval(
        &mut self,
        req: ApprovalRequest,
        reply: std::sync::mpsc::Sender<ApprovalDecision>,
    ) {
        // 真实等待只在 pending 建立时出现(AskingApproval 事件是信息性的,每个
        // run_shell 都会发,不代表一定弹审批)。
        self.approval_wait = Some(format!("{}: {}", req.tool, req.description));
        self.pending = Some(PendingPrompt::Approval { req, reply });
    }

    pub fn set_pending_question(
        &mut self,
        text: String,
        reply: std::sync::mpsc::Sender<String>,
    ) {
        self.pending = Some(PendingPrompt::Question { text, reply, secret: false });
    }

    pub fn set_pending_secret(
        &mut self,
        text: String,
        reply: std::sync::mpsc::Sender<String>,
    ) {
        self.pending = Some(PendingPrompt::Question { text, reply, secret: true });
    }

    /// 授权闸:高危操作需用户同意(0=拒绝 1=仅本次 / 2=本会话 / 3=永久)。
    pub fn set_pending_guard(
        &mut self,
        req: GuardRequest,
        reply: std::sync::mpsc::Sender<GuardConsent>,
    ) {
        self.approval_wait = Some(format!("{}: {}", req.tool, req.reason));
        self.pending = Some(PendingPrompt::Guard { req, reply });
    }

    /// 单键回答 pending 审批/守卫。返回是否消费了该键(未知键返回 false)。
    ///
    /// 审批键:Y/N/A → Allow / Deny / Allow(本会话简化放行);
    /// 守卫键:0/1/2/3 → Deny / Once / Session / Forever。
    /// `Question` 返回 false:仍需输入栏文本(Enter 提交),不进单键路径。
    pub fn pending_answer(&mut self, key: &str) -> bool {
        let Some(pending) = &self.pending else { return false };
        let consumed = match pending {
            PendingPrompt::Approval { .. } => matches!(key, "y" | "Y" | "n" | "N" | "a" | "A"),
            PendingPrompt::Guard { .. } => matches!(key, "0" | "1" | "2" | "3"),
            PendingPrompt::Question { .. } => false, // 问题仍需输入栏文本
        };
        if !consumed {
            return false;
        }
        // 按 pending 变体产生对应枚举的决策,并把答案送回 channel。
        let (label, ok) = match (&self.pending, key.to_ascii_lowercase().as_str()) {
            (Some(PendingPrompt::Approval { .. }), "n") => ("已拒绝", false),
            (Some(PendingPrompt::Approval { .. }), _) => ("已允许", true),
            (Some(PendingPrompt::Guard { .. }), "0") => ("已拒绝", false),
            (Some(PendingPrompt::Guard { .. }), "1" | "2") => ("已允许(本次)", true),
            (Some(PendingPrompt::Guard { .. }), _) => ("已允许(永久)", true), // "3"
            _ => ("", true),
        };
        let tool_desc = match &self.pending {
            Some(PendingPrompt::Approval { req, .. }) => {
                format!("{}: {}", req.tool, req.description)
            }
            Some(PendingPrompt::Guard { req, .. }) => format!("{}: {}", req.tool, req.reason),
            _ => String::new(),
        };
        let _ = self.pending.take().map(|p| match p {
            PendingPrompt::Approval { reply, .. } => {
                let decision = match key.to_ascii_lowercase().as_str() {
                    "n" => ApprovalDecision::Deny {
                        reason: "declined by user".into(),
                    },
                    _ => ApprovalDecision::Allow, // "y" / "a" → 本会话放行
                };
                let _ = reply.send(decision);
            }
            PendingPrompt::Guard { reply, .. } => {
                let consent = match key.to_ascii_lowercase().as_str() {
                    "1" => GuardConsent::Once,
                    "2" => GuardConsent::Session,
                    "3" => GuardConsent::Forever,
                    _ => GuardConsent::Deny, // "0"
                };
                let _ = reply.send(consent);
            }
            PendingPrompt::Question { .. } => {}
        });
        self.approval_wait = None;
        self.push_line(
            format!("{} {}", if ok { "✓" } else { "✗" }, label),
            if ok { LineStyle::Success } else { LineStyle::Error },
        );
        if !tool_desc.is_empty() {
            self.push_line(format!("  {tool_desc}"), LineStyle::Dim);
        }
        true
    }

    /// 解析授权闸答案:"1"=Once,"2"=Session,"3"=Forever;"0"/空(Ctrl+C 取消)/
    /// 其它非法 → Deny(拒绝,最保守)。与 Approval 的 Ctrl+C → Deny 语义一致。
    pub fn parse_guard_consent(answer: &str) -> GuardConsent {
        match answer.trim() {
            "1" => GuardConsent::Once,
            "2" => GuardConsent::Session,
            "3" => GuardConsent::Forever,
            _ => GuardConsent::Deny,
        }
    }

    /// 若当前 pending 是授权闸,按答案解析并把 consent 送回 channel。
    pub fn resolve_guard(&mut self, answer: &str) {
        if let Some(PendingPrompt::Guard { reply, .. }) = self.pending.take() {
            let _ = reply.send(Self::parse_guard_consent(answer));
        }
    }

    pub fn pending_is_secret(&self) -> bool {
        matches!(&self.pending, Some(PendingPrompt::Question { secret: true, .. }))
    }

    /// 审批等待行的按键提示:按 pending 变体返回对应说明(渲染 live 行用)。
    /// Approval → Y/N/A;Guard → 0-3;Question 无单键提示。
    pub fn approval_hint(&self) -> Option<&'static str> {
        match &self.pending {
            Some(PendingPrompt::Approval { .. }) => Some("[Y=允许 N=拒绝 A=本会话允许]"),
            Some(PendingPrompt::Guard { .. }) => Some("[0=拒绝 1=仅本次 2=本会话 3=永久]"),
            _ => None,
        }
    }

    pub fn resolve_pending(&mut self, answer: &str) {
        self.approval_wait = None;
        if let Some(pending) = self.pending.take() {
            match pending {
                PendingPrompt::Approval { req: _, reply } => {
                    let decision = if answer.trim().eq_ignore_ascii_case("y") {
                        ApprovalDecision::Allow
                    } else {
                        ApprovalDecision::Deny {
                            reason: "declined by user".into(),
                        }
                    };
                    let _ = reply.send(decision);
                }
                PendingPrompt::Question { reply, .. } => {
                    let _ = reply.send(answer.trim().to_string());
                }
                PendingPrompt::Guard { reply, .. } => {
                    let _ = reply.send(Self::parse_guard_consent(answer));
                }
            }
        }
    }

    /// 切换到下一个存活 agent（Tab）。无 agent 时 focus 置 None。
    pub fn focus_next_agent(&mut self) {
        let ids: Vec<String> = self.agents.keys().cloned().collect();
        if ids.is_empty() {
            self.focus_agent = None;
            return;
        }
        let cur = self
            .focus_agent
            .as_deref()
            .and_then(|c| ids.iter().position(|i| i == c))
            .unwrap_or(usize::MAX);
        let next = if cur == usize::MAX { 0 } else { (cur + 1) % ids.len() };
        self.focus_agent = Some(ids[next].clone());
    }

    /// 切换到上一个存活 agent（Shift+Tab）。
    pub fn focus_prev_agent(&mut self) {
        let ids: Vec<String> = self.agents.keys().cloned().collect();
        if ids.is_empty() {
            self.focus_agent = None;
            return;
        }
        let cur = self
            .focus_agent
            .as_deref()
            .and_then(|c| ids.iter().position(|i| i == c))
            .unwrap_or(0);
        let prev = (cur + ids.len() - 1) % ids.len();
        self.focus_agent = Some(ids[prev].clone());
    }

    /// 回到父级（footer Parent 键）:agent id 树里取第一个(排序最前)前缀祖先作父。
    /// agents 是 BTreeMap 无显式父子边,按"当前 id 的某个前缀 id"推断;平铺 agent
    /// (无前缀)时清 focus 回根。未聚焦时 no-op。前缀必须落在分隔符边界
    /// (`.`/`_`)或串尾,避免 `"s1"` 被当作 `"s10"` 的父。
    pub fn focus_parent_agent(&mut self) {
        let Some(cur) = self.focus_agent.clone() else { return };
        let parent = self
            .agents
            .keys()
            .find(|k| {
                let k = k.as_str();
                let cur = cur.as_str();
                k != cur
                    && cur.starts_with(k)
                    && cur[k.len()..]
                        .chars()
                        .next()
                        .map_or(true, |c| matches!(c, '.' | '_'))
            })
            .cloned();
        self.focus_agent = parent;
    }

    /// 循环切换风险模式（Shift+Tab）：Ask → Auto → Assisted → Manual → Ask。
    pub fn cycle_risk_mode(&mut self) {
        use rc_router::risk::RiskMode::*;
        self.risk_mode = match self.risk_mode {
            Ask => Auto,
            Auto => Assisted,
            Assisted => Manual,
            Manual => Ask,
        };
    }

    /// 风险模式的短标签（状态栏/命令回显）。
    pub fn risk_label(&self) -> &'static str {
        use rc_router::risk::RiskMode::*;
        match self.risk_mode {
            Ask => "ask",
            Auto => "auto",
            Assisted => "assisted",
            Manual => "manual",
        }
    }

    /// 刷新斜杠补全浮层。开菜单规则:首行以 `/` 开头且 `/` 后无空格(有空格 =
    /// 已输参数,菜单收起)。过滤词 = `/` 后首个 token;选中项尽量保留。
    pub fn update_slash_menu(&mut self) {
        let Some(rest) = self
            .input
            .text
            .lines()
            .next()
            .and_then(|l| l.strip_prefix('/'))
        else {
            self.slash_menu = None;
            return;
        };
        if rest.contains(' ') {
            self.slash_menu = None;
            return;
        }
        let name = rest.trim();
        let items: Vec<&'static command::CommandSpec> = if name.is_empty() {
            command::COMMANDS.iter().collect()
        } else {
            command::complete(name)
        };
        let keep = self
            .slash_menu
            .as_ref()
            .and_then(|m| m.items.get(m.selected))
            .map(|s| s.name);
        let selected = items
            .iter()
            .position(|s| Some(s.name) == keep)
            .unwrap_or(0);
        self.slash_menu = Some(SlashMenu { items, selected });
    }

    /// 菜单选中项下移一个(循环回绕)。
    pub fn slash_menu_next(&mut self) {
        if let Some(menu) = &mut self.slash_menu {
            if !menu.items.is_empty() {
                menu.selected = (menu.selected + 1) % menu.items.len();
            }
        }
    }

    /// 菜单选中项上移一个(循环回绕)。
    pub fn slash_menu_prev(&mut self) {
        if let Some(menu) = &mut self.slash_menu {
            if !menu.items.is_empty() {
                menu.selected = (menu.selected + menu.items.len() - 1) % menu.items.len();
            }
        }
    }

    /// 接受选中命令:输入重写为 `/name `(光标移末尾)。含空格 → 菜单自动收起。
    /// 无候选时返回 false(调用方回落正常 submit)。
    pub fn slash_menu_accept(&mut self) -> bool {
        let Some(menu) = &self.slash_menu else { return false };
        if menu.items.is_empty() {
            return false;
        }
        let name = menu.items[menu.selected].name;
        self.input.text = format!("/{name} ");
        self.input.cursor = self.input.text.len();
        self.update_slash_menu(); // 含空格 → 收起
        true
    }

    /// 打开交互式模型选择器(/model),关闭斜杠菜单。
    pub fn open_model_picker(&mut self, entries: Vec<crate::repl::env::ModelPickerEntry>) {
        self.slash_menu = None;
        let mut picker = ModelPicker {
            all: entries,
            filtered: Vec::new(),
            query: String::new(),
            selected: 0,
        };
        picker.refilter();
        self.model_picker = Some(picker);
    }

    pub fn picker_query_push(&mut self, c: char) {
        if let Some(p) = &mut self.model_picker {
            p.query.push(c);
            p.refilter();
        }
    }

    pub fn picker_query_backspace(&mut self) {
        if let Some(p) = &mut self.model_picker {
            p.query.pop();
            p.refilter();
        }
    }

    pub fn picker_next(&mut self) {
        if let Some(p) = &mut self.model_picker {
            if !p.filtered.is_empty() {
                p.selected = (p.selected + 1) % p.filtered.len();
            }
        }
    }

    pub fn picker_prev(&mut self) {
        if let Some(p) = &mut self.model_picker {
            if !p.filtered.is_empty() {
                p.selected = (p.selected + p.filtered.len() - 1) % p.filtered.len();
            }
        }
    }

    /// 打开交互式会话选择器(/resume),关闭斜杠菜单。
    pub fn open_session_picker(&mut self, entries: Vec<SessionEntry>) {
        self.slash_menu = None;
        let mut picker = SessionPicker {
            all: entries,
            filtered: Vec::new(),
            query: String::new(),
            selected: 0,
        };
        picker.refilter();
        self.session_picker = Some(picker);
    }

    pub fn session_picker_query_push(&mut self, c: char) {
        if let Some(p) = &mut self.session_picker {
            p.query.push(c);
            p.refilter();
        }
    }

    pub fn session_picker_query_backspace(&mut self) {
        if let Some(p) = &mut self.session_picker {
            p.query.pop();
            p.refilter();
        }
    }

    /// 恢复历史会话:载入其消息到输出区(截断 200 字符/条)+ 重指向 session_id,
    /// 使下一条任务自然继续该会话。消息角色映射:User/Assistant/Tool。
    pub fn resume_session(&mut self, store: &rc_state::Store, id: &str) -> Result<(), String> {
        let messages = store.list_messages(id).map_err(|e| e.to_string())?;
        self.session_id = id.to_string();
        for m in &messages {
            let content: String = m.content.chars().take(200).collect();
            match m.role {
                rc_state::MessageRole::User => self.push_user_line(content),
                rc_state::MessageRole::Assistant => {
                    self.push_assistant_line(content, LineStyle::Accent)
                }
                _ => self.push_line(format!("[tool] {content}"), LineStyle::Tool),
            };
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rc_proto::AgentEvent;

    #[test]
    fn agent_spawn_sets_running_and_registers_agent() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.apply_event(AgentEvent::AgentSpawned {
            id: "a1".into(),
            model: "gpt-5".into(),
            role: "coder".into(),
            task: "build".into(),
        });
        assert_eq!(m.phase, Phase::Running);
        assert!(m.agents.contains_key("a1"));
        assert_eq!(m.task_count, 1);
    }

    #[test]
    fn tool_call_opens_live_row_then_result_commits() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.apply_event(AgentEvent::ToolCall {
            id: "t1".into(),
            name: "read_file".into(),
            args: serde_json::json!({"path": "a.rs"}),
        });
        assert!(m.live_tool.is_some());
        assert_eq!(m.live_tool.as_ref().unwrap().name, "read_file");
        assert_eq!(m.live_tool.as_ref().unwrap().args, "[path=a.rs]");
        assert_eq!(m.live_tool.as_ref().unwrap().state, ToolState::Running);
        m.apply_event(AgentEvent::ToolResult {
            id: "t1".into(),
            name: "read_file".into(),
            ok: true,
            output: "fn main(){}".into(),
            output_path: None,
        });
        assert!(m.live_tool.is_none());
        // 完成行已提交进 scrollback（含 ✓ 与工具名）。
        assert!(m.output.iter().any(|l| l.text.contains("✓ read_file")));
    }

    #[test]
    fn tool_result_failure_marks_red_line() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.apply_event(AgentEvent::ToolCall {
            id: "t1".into(),
            name: "run_shell".into(),
            args: serde_json::json!({"cmd": "pytest"}),
        });
        m.apply_event(AgentEvent::ToolResult {
            id: "t1".into(),
            name: "run_shell".into(),
            ok: false,
            output: "exit 1".into(),
            output_path: None,
        });
        let line = m.output.back().unwrap();
        assert!(line.text.contains("✗ run_shell"));
        assert_eq!(line.style, LineStyle::Error);
    }

    #[test]
    fn stale_tool_result_does_not_clear_newer_live_tool() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.apply_event(AgentEvent::ToolCall {
            id: "t1".into(),
            name: "read_file".into(),
            args: serde_json::json!({"path": "a.rs"}),
        });
        // 延迟结果到达前,后续 ToolCall 已开新的 live 行。
        m.apply_event(AgentEvent::ToolCall {
            id: "t2".into(),
            name: "write_file".into(),
            args: serde_json::json!({"path": "b.rs"}),
        });
        // 延迟 flush 的 t1 结果(不同 id)→ 只提交自己的完成行,不清 t2 的 spinner。
        m.apply_event(AgentEvent::ToolResult {
            id: "t1".into(),
            name: "read_file".into(),
            ok: true,
            output: "".into(),
            output_path: None,
        });
        assert!(
            m.live_tool.is_some(),
            "stale result must not wipe the newer live-tool row"
        );
        assert_eq!(m.live_tool.as_ref().unwrap().id, "t2");
        assert_eq!(m.live_tool.as_ref().unwrap().state, ToolState::Running);
        // t1 的完成行仍写入 scrollback。
        assert!(m.output.iter().any(|l| l.text.contains("✓ read_file")));
        // 同 id 结果到来 → 正常清 live 行。
        m.apply_event(AgentEvent::ToolResult {
            id: "t2".into(),
            name: "write_file".into(),
            ok: true,
            output: "ok".into(),
            output_path: None,
        });
        assert!(m.live_tool.is_none());
    }

    #[test]
    fn thinking_sets_live_thinking_cleared_on_tool_call() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.apply_event(AgentEvent::Thinking { delta: "planning".into() });
        assert_eq!(m.live_thinking.as_deref(), Some("planning"));
        m.apply_event(AgentEvent::ToolCall {
            id: "t1".into(),
            name: "read_file".into(),
            args: serde_json::json!({}),
        });
        assert!(m.live_thinking.is_none());
    }

    #[test]
    fn thinking_accumulates_and_toggles_reasoning() {
        let mut m = ReplModel::new("s".into(), "m".into(), 0);
        m.apply_event(AgentEvent::Thinking { delta: "line1\n".into() });
        m.apply_event(AgentEvent::Thinking { delta: "line2".into() });
        assert_eq!(m.live_thinking.as_deref(), Some("line1\nline2"));
        // 折叠时只露最近 1 行。
        let lines = m.live_thinking_lines(1);
        assert_eq!(lines, vec!["line2"]);
        // Done 携带完整 reasoning → 展开可看全链。
        m.apply_event(AgentEvent::Done {
            summary: "done".into(), usage: None, session_id: "s".into(),
            reasoning: Some("step1\nstep2\nstep3".into()),
        });
        assert_eq!(m.reasoning_chain.as_deref(), Some("step1\nstep2\nstep3"));
        assert!(!m.reasoning_expanded);
        m.toggle_reasoning();
        assert!(m.reasoning_expanded);
    }

    #[test]
    fn done_clears_live_rows() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.apply_event(AgentEvent::ToolCall {
            id: "t1".into(),
            name: "read_file".into(),
            args: serde_json::json!({}),
        });
        m.apply_event(AgentEvent::Done {
            summary: "done".into(),
            usage: None,
            session_id: "s1".into(),
            reasoning: None,
        });
        assert!(m.live_tool.is_none());
        assert!(m.live_thinking.is_none());
    }

    #[test]
    fn done_does_not_duplicate_streamed_answer() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        // 流式回答已被 flush 进 output。
        m.apply_event(AgentEvent::Token { delta: "你好!我是 Raincode。\n".into() });
        // Done.summary 与流式内容相同 → 不应重复 push。
        m.apply_event(AgentEvent::Done {
            summary: "你好!我是 Raincode。".into(),
            usage: None,
            session_id: "s1".into(),
            reasoning: None,
        });
        let count = m
            .output
            .iter()
            .filter(|l| l.text.contains("你好!我是 Raincode"))
            .count();
        assert_eq!(count, 1, "streamed answer must not be duplicated by Done.summary");
    }

    #[test]
    fn done_uses_summary_when_no_stream() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        // 无流式输出时,Done.summary 兜底显示。
        m.apply_event(AgentEvent::Done {
            summary: "完成任务".into(),
            usage: None,
            session_id: "s1".into(),
            reasoning: None,
        });
        assert!(m.output.iter().any(|l| l.text.contains("完成任务")));
    }

    #[test]
    fn skill_loaded_shows_loaded_banner() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.apply_event(AgentEvent::SkillLoaded {
            name: "coding-cycle".into(),
            path: "/skills/coding-cycle".into(),
        });
        assert!(m.output.iter().any(|l| l.text.contains("coding-cycle 已加载")));
    }

    #[test]
    fn skill_suggested_is_silent() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        let before = m.output.len();
        m.apply_event(AgentEvent::SkillSuggested {
            name: "git-discipline".into(),
            category: "workflow.git".into(),
            confidence: 0.0,
        });
        // 建议不产生任何输出行。
        assert_eq!(m.output.len(), before);
    }

    #[test]
    fn tool_args_preview_collapses_complex_values() {
        // serde_json 的 json! 用 BTreeMap,键按字母序 → 顺序稳定但与书写序无关。
        let preview = tool_args_preview(&serde_json::json!({"path": "a.rs", "offset": 1, "flag": true}));
        assert!(preview.contains("path=a.rs"));
        assert!(preview.contains("offset=1"));
        assert!(preview.contains("flag=true"));
        assert!(preview.starts_with("[") && preview.ends_with("]"));
        assert_eq!(tool_args_preview(&serde_json::json!({"file": {"content": "x"}})), "[file]");
        assert_eq!(tool_args_preview(&serde_json::json!({})), "");
    }

    #[test]
    fn done_returns_idle_with_completion_line() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.apply_event(AgentEvent::AgentSpawned {
            id: "a1".into(),
            model: "gpt-5".into(),
            role: "coder".into(),
            task: "build".into(),
        });
        m.apply_event(AgentEvent::Done {
            summary: "done it".into(),
            usage: None,
            session_id: "s1".into(),
            reasoning: None,
        });
        assert_eq!(m.phase, Phase::Idle);
        // 汇报块头含"任务完成"。
        assert!(m.output.iter().any(|l| l.text.contains("任务完成")));
    }

    #[test]
    fn abs_file_path_resolves_relative_when_workspace_set() {
        let m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        // 空 workspace(测试/未设置)→ 相对路径原样。
        assert_eq!(m.abs_file_path("src/a.py"), "src/a.py");
        let mut m = m;
        m.workspace = "/home/user/proj".into();
        let expected = std::path::Path::new("/home/user/proj")
            .join("src/a.py")
            .to_string_lossy()
            .to_string();
        assert_eq!(m.abs_file_path("src/a.py"), expected);
        assert!(m.abs_file_path("src/a.py").ends_with("src/a.py"));
    }

    #[test]
    fn done_reports_absolute_file_paths_when_workspace_set() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.workspace = "/home/user/proj".into();
        // 写文件工具调用 → 记录 files_touched(相对路径)。
        m.apply_event(AgentEvent::ToolCall {
            id: "t1".into(),
            name: "write_file".into(),
            args: serde_json::json!({"path": "src/a.py", "content": "x"}),
        });
        m.apply_event(AgentEvent::Done {
            summary: "done".into(),
            usage: None,
            session_id: "s1".into(),
            reasoning: None,
        });
        // 汇报里的文件是绝对路径(用户能直接看到落盘位置)。
        let abs = std::path::Path::new("/home/user/proj")
            .join("src/a.py")
            .to_string_lossy()
            .to_string();
        assert!(m.output.iter().any(|l| l.text.contains(&abs)));
    }

    #[test]
    fn error_event_returns_idle() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.apply_event(AgentEvent::Error {
            message: "boom".into(),
        });
        assert_eq!(m.phase, Phase::Idle);
        assert!(m.output.back().unwrap().text.contains("boom"));
    }

    #[test]
    fn cancelled_error_renders_interrupted() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.apply_event(AgentEvent::Error {
            message: "cancelled by user".into(),
        });
        assert_eq!(m.phase, Phase::Idle);
        let last = m.output.back().unwrap();
        assert!(last.text.contains("已中断"));
        assert!(!last.text.contains("[error]"));
        assert_eq!(last.style, LineStyle::Warn);
    }

    #[test]
    fn context_update_tracks_usage() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.apply_event(AgentEvent::ContextUpdate {
            used: 12_400,
            limit: 128_000,
            pct: 9,
            agent_id: None,
        });
        assert_eq!(m.context.used, 12_400);
        assert_eq!(m.context.pct, 9);
    }

    #[test]
    fn tokens_accumulate_and_flush_on_structural_event() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.apply_event(AgentEvent::Token { delta: "hel".into() });
        m.apply_event(AgentEvent::Token { delta: "lo".into() });
        assert_eq!(m.streaming.as_deref(), Some("hello"));
        m.apply_event(AgentEvent::ToolCall {
            id: "t1".into(),
            name: "read_file".into(),
            args: serde_json::json!({}),
        });
        assert!(m.streaming.is_none());
        assert!(m.output.iter().any(|l| l.text == "hello"));
    }

    #[test]
    fn agent_spawn_clears_done_at() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.apply_event(AgentEvent::Done {
            summary: "done it".into(),
            usage: None,
            session_id: "s1".into(),
            reasoning: None,
        });
        assert!(m.done_at.is_some());
        m.apply_event(AgentEvent::AgentSpawned {
            id: "a1".into(),
            model: "gpt-5".into(),
            role: "coder".into(),
            task: "build".into(),
        });
        assert!(m.done_at.is_none());
    }

    #[test]
    fn session_started_clears_done_at() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.apply_event(AgentEvent::Done {
            summary: "done it".into(),
            usage: None,
            session_id: "s1".into(),
            reasoning: None,
        });
        assert!(m.done_at.is_some());
        m.apply_event(AgentEvent::SessionStarted {
            session_id: "s1".into(),
        });
        assert!(m.done_at.is_none());
    }

    #[test]
    fn stream_delta_flushes_every_complete_line() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.apply_event(AgentEvent::Token { delta: "a\nb\nc".into() });
        let texts: Vec<String> = m.output.iter().map(|l| l.text.clone()).collect();
        assert!(texts.contains(&"a".to_string()));
        assert!(texts.contains(&"b".to_string()));
        assert_eq!(m.streaming.as_deref(), Some("c"));
    }

    #[test]
    fn output_is_bounded() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        for i in 0..(OUTPUT_BOUND + 50) {
            m.push_line(format!("line {i}"), LineStyle::Plain);
        }
        assert_eq!(m.output.len(), OUTPUT_BOUND);
        assert_eq!(m.output.front().unwrap().text, "line 50");
    }

    #[test]
    fn risk_mode_cycles_four_modes() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        use rc_router::risk::RiskMode::*;
        assert_eq!(m.risk_mode, Ask);
        m.cycle_risk_mode();
        assert_eq!(m.risk_mode, Auto);
        m.cycle_risk_mode();
        assert_eq!(m.risk_mode, Assisted);
        m.cycle_risk_mode();
        assert_eq!(m.risk_mode, Manual);
        m.cycle_risk_mode();
        assert_eq!(m.risk_mode, Ask); // 回绕
        assert_eq!(m.risk_label(), "ask");
    }

    #[test]
    fn agent_detail_focus_switch_and_steer() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.apply_event(AgentEvent::AgentSpawned {
            id: "s1".into(),
            model: "deepseek".into(),
            role: "backend".into(),
            task: "fix api".into(),
        });
        m.apply_event(AgentEvent::AgentSpawned {
            id: "s2".into(),
            model: "qwen".into(),
            role: "frontend".into(),
            task: "build page".into(),
        });
        assert!(m.focus_agent.is_none());
        m.focus_next_agent();
        assert_eq!(m.focus_agent.as_deref(), Some("s1"));
        m.focus_next_agent();
        assert_eq!(m.focus_agent.as_deref(), Some("s2"));
        m.focus_next_agent(); // 回绕
        assert_eq!(m.focus_agent.as_deref(), Some("s1"));
        m.focus_prev_agent();
        assert_eq!(m.focus_agent.as_deref(), Some("s2"));
    }

    #[test]
    fn focus_parent_agent_moves_to_prefix_ancestor_or_clears() {
        let mut m = ReplModel::new("s".into(), "m".into(), 0);
        m.agents
            .insert("a1".into(), AgentView::new("executor".into(), "m".into()));
        m.agents
            .insert("a2".into(), AgentView::new("executor".into(), "m".into()));
        // 平铺 agent(无前缀父):Parent 键清 focus(回根 → 普通输入)。
        m.focus_next_agent();
        assert_eq!(m.focus_agent.as_deref(), Some("a1"));
        m.focus_parent_agent();
        assert!(m.focus_agent.is_none());
        // 有前缀父(树结构):跳到第一个(排序最前)合法前缀祖先。
        // "a.b.c" 的最浅前缀父是 "a"(前缀后为分隔符 `.`;设计取排序最前祖先)。
        m.agents
            .insert("a".into(), AgentView::new("root".into(), "m".into()));
        m.agents
            .insert("a.b".into(), AgentView::new("parent".into(), "m".into()));
        m.agents
            .insert("a.b.c".into(), AgentView::new("child".into(), "m".into()));
        m.focus_agent = Some("a.b.c".into());
        m.focus_parent_agent();
        assert_eq!(m.focus_agent.as_deref(), Some("a"));
        // 未聚焦时 no-op。
        m.focus_agent = None;
        m.focus_parent_agent();
        assert!(m.focus_agent.is_none());
    }

    #[test]
    fn focus_parent_respects_prefix_boundary() {
        let mut m = ReplModel::new("s".into(), "m".into(), 0);
        m.agents
            .insert("s1".into(), AgentView::new("root".into(), "m".into()));
        m.agents
            .insert("s10".into(), AgentView::new("child".into(), "m".into()));
        // "s1" 是 "s10" 的字符串前缀,但不是父节点(前缀后必须分隔符或串尾)。
        m.focus_agent = Some("s10".into());
        m.focus_parent_agent();
        assert!(
            m.focus_agent.is_none(),
            "s10 must not treat s1 as parent (prefix boundary)"
        );
        // 分隔符父(点号)仍工作:focus "s1.sub" → 父 "s1"。
        m.agents
            .insert("s1.sub".into(), AgentView::new("grandchild".into(), "m".into()));
        m.focus_agent = Some("s1.sub".into());
        m.focus_parent_agent();
        assert_eq!(m.focus_agent.as_deref(), Some("s1"));
    }

    #[test]
    fn resolve_pending_sends_decision() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        let (tx, rx) = std::sync::mpsc::channel();
        m.set_pending_approval(
            ApprovalRequest {
                tool: "run_shell".into(),
                description: "rm -rf /".into(),
                args: serde_json::json!({}),
            },
            tx,
        );
        assert!(m.pending.is_some());
        m.resolve_pending("y");
        assert!(m.pending.is_none());
        assert!(matches!(rx.recv(), Ok(ApprovalDecision::Allow)));
    }

    #[test]
    fn orchestrator_events_build_task_tree() {
        use rc_orchestrate::tree::TaskStatus;
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        // Plan → 根节点。
        m.apply_event(AgentEvent::OrchestratorPlan {
            node_id: "root".into(),
            plan: "build an app\n- s1 backend\n- s2 frontend".into(),
        });
        assert!(m.tree.nodes.contains_key("root"));
        assert_eq!(m.tree.root().unwrap().depth, 1);
        assert_eq!(m.tree.root().unwrap().status, TaskStatus::Running);
        // Dispatch → 子任务(携带自动选中的模型)。
        m.apply_event(AgentEvent::OrchestratorDispatch {
            parent_id: "root".into(),
            child_id: "s1".into(),
            prompt: "backend api".into(),
            model: "deepseek-v4".into(),
        });
        m.apply_event(AgentEvent::OrchestratorDispatch {
            parent_id: "root".into(),
            child_id: "s2".into(),
            prompt: "react page".into(),
            model: "qwen3".into(),
        });
        assert_eq!(m.tree.children_of("root").len(), 2);
        let s1 = &m.tree.nodes["s1"];
        assert_eq!(s1.model.as_deref(), Some("deepseek-v4"));
        assert_eq!(s1.depth, 2);
        assert_eq!(s1.status, TaskStatus::Running);
        // Result → 状态流转 + 摘要。
        m.apply_event(AgentEvent::OrchestratorResult {
            node_id: "s1".into(),
            status: "ok".into(),
            summary: "wrote api, 3 tests pass".into(),
        });
        m.apply_event(AgentEvent::OrchestratorResult {
            node_id: "s2".into(),
            status: "failed".into(),
            summary: "css broken".into(),
        });
        assert_eq!(m.tree.nodes["s1"].status, TaskStatus::Done);
        assert_eq!(m.tree.nodes["s1"].summary.as_deref(), Some("wrote api, 3 tests pass"));
        assert_eq!(m.tree.nodes["s2"].status, TaskStatus::Failed);
    }

    #[test]
    fn orchestrator_dispatch_populates_todo() {
        use rc_orchestrate::todo::TodoStatus;
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.apply_event(AgentEvent::OrchestratorDispatch {
            parent_id: "root".into(),
            child_id: "s1".into(),
            prompt: "write api".into(),
            model: "deepseek".into(),
        });
        assert_eq!(m.todo.items.len(), 1);
        assert_eq!(m.todo.items[0].status, TodoStatus::InProgress);
        assert_eq!(m.todo.items[0].id, "s1");
        m.apply_event(AgentEvent::OrchestratorResult {
            node_id: "s1".into(),
            status: "ok".into(),
            summary: "wrote api".into(),
        });
        assert_eq!(m.todo.items[0].status, TodoStatus::Done);
    }

    #[test]
    fn slash_menu_opens_closes_and_filters() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        // 无 / 前缀 → 关闭。
        m.input.text = "hi".into();
        m.update_slash_menu();
        assert!(m.slash_menu.is_none());
        // / 后无空格 → 打开,全命令。
        m.input.text = "/".into();
        m.update_slash_menu();
        assert!(m.slash_menu.is_some());
        assert_eq!(m.slash_menu.as_ref().unwrap().items.len(), command::COMMANDS.len());
        // /mo → 过滤。
        m.input.text = "/mo".into();
        m.update_slash_menu();
        let names: Vec<&str> = m.slash_menu.as_ref().unwrap().items.iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["models", "model"]);
        // 有空格(已输参数)→ 关闭。
        m.input.text = "/chat hi".into();
        m.update_slash_menu();
        assert!(m.slash_menu.is_none());
    }

    #[test]
    fn slash_menu_next_prev_wraps() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.input.text = "/mo".into();
        m.update_slash_menu();
        assert_eq!(m.slash_menu.as_ref().unwrap().selected, 0);
        m.slash_menu_next();
        m.slash_menu_next();
        assert_eq!(m.slash_menu.as_ref().unwrap().selected, 0); // 2 项循环回绕
        m.slash_menu_prev();
        assert_eq!(m.slash_menu.as_ref().unwrap().selected, 1);
    }

    #[test]
    fn slash_menu_accept_fills_command_and_closes() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.input.text = "/cha".into();
        m.input.cursor = 4;
        m.update_slash_menu();
        assert!(m.slash_menu_accept());
        assert_eq!(m.input.text, "/chat ");
        assert_eq!(m.input.cursor, "/chat ".len());
        assert!(m.slash_menu.is_none()); // 含空格 → 收起
        // 无候选 → 返回 false。
        let mut m2 = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m2.input.text = "/zzz".into();
        m2.update_slash_menu();
        assert!(!m2.slash_menu_accept());
    }

    #[test]
    fn user_line_sets_kind_and_increments_turns() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        assert_eq!(m.turn_count, 0);
        m.push_user_line("hi".into());
        assert_eq!(m.turn_count, 1);
        assert_eq!(m.output.back().unwrap().kind, LineKind::User);
        assert_eq!(m.output.back().unwrap().style, LineStyle::Accent);
    }

    #[test]
    fn streamed_lines_are_assistant_kind() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.apply_event(AgentEvent::Token { delta: "hello\n".into() });
        let last = m.output.back().unwrap();
        assert_eq!(last.kind, LineKind::Assistant);
        assert_eq!(last.text, "hello");
    }

    #[test]
    fn plain_push_is_system_kind() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.push_line("note".into(), LineStyle::Dim);
        assert_eq!(m.output.back().unwrap().kind, LineKind::System);
    }

    #[test]
    fn has_queued_reflects_real_queue_state() {
        let mut m = ReplModel::new("s".into(), "m".into(), 0);
        // 无排队消息时,即使已完成块后又出现新的 assistant 块也不报排队 ——
        // 正常顺序会话第二段回答不触发 QUEUED(规格意图:并行 agent 仍挂起)。
        m.push_assistant_line("first".into(), LineStyle::Plain);
        m.push_user_line("next".into());
        m.push_assistant_line("second".into(), LineStyle::Plain);
        assert!(!m.has_queued(), "sequential replies without a queue are not queued");
        // 运行中 Tab 排队输入(queued_input)→ 排队。
        m.queued_input.push_back("steer me".into());
        assert!(m.has_queued(), "queued_input non-empty = queued");
        // 清空 queued_input → 不排队。
        m.queued_input.clear();
        assert!(!m.has_queued());
        // 运行中无 focus 提交的 steer(pending_steers)→ 排队。
        m.pending_steers.push_back("steer".into());
        assert!(m.has_queued(), "pending_steers non-empty = queued");
    }

    #[test]
    fn tool_calls_count_and_record_files() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.start_run();
        m.apply_event(AgentEvent::ToolCall {
            id: "t1".into(),
            name: "write_file".into(),
            args: serde_json::json!({"path": "src/a.rs", "content": "x"}),
        });
        m.apply_event(AgentEvent::ToolCall {
            id: "t2".into(),
            name: "read_file".into(),
            args: serde_json::json!({"path": "src/a.rs"}),
        });
        m.apply_event(AgentEvent::ToolCall {
            id: "t3".into(),
            name: "edit".into(),
            args: serde_json::json!({"file_path": "src/b.rs"}),
        });
        assert_eq!(m.tool_calls, 3);
        // 只记录写文件类;去重。
        assert_eq!(m.files_touched, vec!["src/a.rs".to_string(), "src/b.rs".to_string()]);
        // start_run 重置本轮。
        m.start_run();
        assert_eq!(m.tool_calls, 0);
        assert!(m.files_touched.is_empty());
    }

    #[test]
    fn done_produces_structured_report() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.start_run();
        m.apply_event(AgentEvent::ToolCall {
            id: "t1".into(),
            name: "write_file".into(),
            args: serde_json::json!({"path": "src/api.rs"}),
        });
        m.apply_event(AgentEvent::Done {
            summary: "wrote api\nall tests pass".into(),
            usage: Some(serde_json::json!({
                "input_tokens": 800,
                "output_tokens": 400,
                "total_tokens": 1200,
            })),
            session_id: "s1".into(),
            reasoning: None,
        });
        let texts: Vec<String> = m.output.iter().map(|l| l.text.clone()).collect();
        let joined = texts.join("\n");
        assert!(texts.iter().any(|l| l.contains("任务完成")));
        assert!(joined.contains("wrote api"));
        assert!(joined.contains("all tests pass"));
        assert!(joined.contains("1 tool calls"));
        assert!(joined.contains("1 files"));
        assert!(joined.contains("1.2K tokens"));
        assert!(joined.contains("src/api.rs"));
        // 无流式回答时摘要缩进展示。
        assert!(texts.iter().any(|l| l.starts_with("  wrote api")));
    }

    #[test]
    fn usage_tokens_parses_openai_and_generic() {
        let openai = serde_json::json!({"prompt_tokens": 100, "completion_tokens": 50, "total_tokens": 150});
        assert_eq!(usage_tokens(&openai), (100, 50, 150));
        let generic = serde_json::json!({"input_tokens": 10, "output_tokens": 20});
        assert_eq!(usage_tokens(&generic), (10, 20, 30));
        assert_eq!(usage_tokens(&serde_json::json!({})), (0, 0, 0));
    }

    #[test]
    fn start_run_resets_run_scoped_stats() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.apply_event(AgentEvent::Done {
            summary: "x".into(),
            usage: None,
            session_id: "s1".into(),
            reasoning: Some("old chain".into()),
        });
        assert_eq!(m.reasoning_chain.as_deref(), Some("old chain"));
        m.toggle_reasoning(); // 展开态也应在 start_run 复位。
        assert!(m.reasoning_expanded);
        m.start_run();
        assert_eq!(m.phase, Phase::Running);
        assert!(m.started_at.is_some());
        assert!(m.done_at.is_none());
        assert!(!m.streamed_this_run);
        assert!(
            m.reasoning_chain.is_none(),
            "start_run must clear the previous task's reasoning chain"
        );
        assert!(!m.reasoning_expanded, "start_run must reset reasoning expansion");
    }

    #[test]
    fn phase_note_tracks_phase_and_agent() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        assert!(m.phase_note.is_none());
        m.apply_event(AgentEvent::PhaseChanged {
            phase: "拆解".into(),
            cycle: 0,
            session_id: "s1".into(),
        });
        assert_eq!(m.phase_note.as_deref(), Some("拆解"));
        m.apply_event(AgentEvent::AgentSpawned {
            id: "a1".into(),
            model: "gpt-5".into(),
            role: "coder".into(),
            task: "build".into(),
        });
        assert_eq!(m.phase_note.as_deref(), Some("执行 a1"));
        // Done 清除阶段提示。
        m.apply_event(AgentEvent::Done {
            summary: "done".into(),
            usage: None,
            session_id: "s1".into(),
            reasoning: None,
        });
        assert!(m.phase_note.is_none());
    }

    fn picker_entry(id: &str, provider: &str, model: &str, coding: f64) -> crate::repl::env::ModelPickerEntry {
        crate::repl::env::ModelPickerEntry {
            id: id.into(), provider: provider.into(), model: model.into(),
            active: false, reasoning: 50.0, coding, frontend: 40.0, backend: 45.0,
        }
    }

    #[test]
    fn model_picker_filters_selects_and_cycles() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.open_model_picker(vec![
            picker_entry("deepseek-v4-flash", "deepseek", "ds-v4-flash", 69.0),
            picker_entry("opencode-ds", "opencode", "ds-v4-flash", 55.0),
            picker_entry("kimi", "kimi", "k3", 80.0),
        ]);
        assert!(m.model_picker.is_some());
        // 初始:全部,选中第一个(deepseek)。
        assert_eq!(m.model_picker.as_ref().unwrap().filtered.len(), 3);
        // 搜索 "kimi" → 只剩 kimi。
        m.picker_query_push('k');
        m.picker_query_push('i');
        let p = m.model_picker.as_ref().unwrap();
        assert_eq!(p.filtered.len(), 1);
        assert_eq!(p.selected_entry().unwrap().id, "kimi");
        // 清空搜索 → 恢复。
        m.picker_query_backspace();
        m.picker_query_backspace();
        assert_eq!(m.model_picker.as_ref().unwrap().filtered.len(), 3);
        // ↑↓ 循环选择。
        m.picker_next();
        assert_eq!(m.model_picker.as_ref().unwrap().selected_entry().unwrap().id, "opencode-ds");
        m.picker_next();
        m.picker_next();
        // 3 项循环回绕 → deepseek。
        assert_eq!(m.model_picker.as_ref().unwrap().selected_entry().unwrap().id, "deepseek-v4-flash");
        m.picker_prev();
        assert_eq!(m.model_picker.as_ref().unwrap().selected_entry().unwrap().id, "kimi");
    }

    #[test]
    fn session_picker_filters_and_selects() {
        let entries = vec![
            SessionEntry { id: "1111aaaa".into(), short_id: "1111aaaa".into(), summary: "build api".into(), updated_at: "t".into() },
            SessionEntry { id: "2222bbbb".into(), short_id: "2222bbbb".into(), summary: "fix tests".into(), updated_at: "u".into() },
        ];
        let mut p = SessionPicker { all: entries, filtered: vec![], query: String::new(), selected: 0 };
        p.refilter();
        assert_eq!(p.filtered.len(), 2);
        p.query = "fix".into();
        p.refilter();
        assert_eq!(p.filtered.len(), 1);
        assert_eq!(p.all[p.filtered[0]].summary, "fix tests");
    }

    #[test]
    fn supervisor_line_uses_supervisor_style() {
        let mut m = ReplModel::new("s".into(), "m".into(), 128_000);
        m.push_supervisor_line("s1 高风险".into());
        let last = m.output.back().unwrap();
        assert_eq!(last.style, LineStyle::Supervisor);
        assert!(last.text.starts_with("[监督]"));
    }

    #[test]
    fn resolve_guard_sends_session_consent() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        let (tx, rx) = std::sync::mpsc::channel();
        m.set_pending_guard(
            rc_sandbox::GuardRequest {
                tool: "run_shell".into(),
                reason: "command matches deny pattern 'rm -rf'".into(),
                command: Some("rm -rf /proj/build".into()),
                path: None,
            },
            tx,
        );
        assert!(m.pending.is_some());
        m.resolve_pending("2");
        assert!(m.pending.is_none());
        assert!(matches!(rx.recv(), Ok(rc_sandbox::GuardConsent::Session)));
    }

    #[test]
    fn parse_guard_consent_maps_all_keys() {
        use rc_sandbox::GuardConsent;
        assert_eq!(ReplModel::parse_guard_consent("1"), GuardConsent::Once);
        assert_eq!(ReplModel::parse_guard_consent("2"), GuardConsent::Session);
        assert_eq!(ReplModel::parse_guard_consent("3"), GuardConsent::Forever);
        // 0 / 空(Ctrl+C 取消)/ 其它非法输入 → 拒绝(最保守)。
        assert_eq!(ReplModel::parse_guard_consent("0"), GuardConsent::Deny);
        assert_eq!(ReplModel::parse_guard_consent(""), GuardConsent::Deny);
        assert_eq!(ReplModel::parse_guard_consent("   "), GuardConsent::Deny);
        assert_eq!(ReplModel::parse_guard_consent("x"), GuardConsent::Deny);
    }

    #[test]
    fn resolve_guard_denies_on_empty_or_ctrl_c() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        let (tx, rx) = std::sync::mpsc::channel();
        m.set_pending_guard(
            rc_sandbox::GuardRequest {
                tool: "run_shell".into(),
                reason: "command matches deny pattern 'rm -rf'".into(),
                command: Some("rm -rf /proj/build".into()),
                path: None,
            },
            tx,
        );
        // Ctrl+C 走 resolve_pending("") → Deny(不再是 Once 放行)。
        m.resolve_pending("");
        assert!(m.pending.is_none());
        assert!(matches!(rx.recv(), Ok(rc_sandbox::GuardConsent::Deny)));
    }

    #[test]
    fn pending_approval_resolves_via_single_key() {
        let mut m = ReplModel::new("s".into(), "m".into(), 0);
        let (tx, _rx) = std::sync::mpsc::channel();
        let req = ApprovalRequest { tool: "run_shell".into(), description: "rm -rf /etc".into(), args: serde_json::json!({"command": "rm -rf /etc"}) };
        m.set_pending_approval(req, tx);
        // Y → Allow,消费该键。
        assert!(m.pending_answer("y"));
        assert!(m.pending.is_none());
        // 历史里有一条 ✓ 已允许行。
        assert!(m.output.iter().any(|l| l.text.contains("✓ 已允许")));
    }

    #[test]
    fn pending_guard_resolves_via_number_keys() {
        let mut m = ReplModel::new("s".into(), "m".into(), 0);
        let (tx, _rx) = std::sync::mpsc::channel();
        let req = GuardRequest { tool: "run_shell".into(), reason: "high risk".into(), command: None, path: None };
        m.set_pending_guard(req, tx);
        assert!(m.pending_answer("3")); // Forever
        assert!(m.pending.is_none());
        assert!(m.output.iter().any(|l| l.text.contains("✓ 已允许(永久)")));
    }

    #[test]
    fn pending_answer_ignores_non_answer_keys() {
        let mut m = ReplModel::new("s".into(), "m".into(), 0);
        let (tx, _rx) = std::sync::mpsc::channel();
        m.set_pending_approval(ApprovalRequest { tool: "t".into(), description: "d".into(), args: serde_json::json!({}) }, tx);
        assert!(!m.pending_answer("x"), "unknown key must not consume");
        assert!(m.pending.is_some(), "approval stays pending");
    }

    #[test]
    fn asking_approval_alone_does_not_set_wait_row() {
        // AskingApproval 是信息性事件(每个 run_shell 都会发);真实等待行只由
        // set_pending_approval 建立,避免无 pending 时 live 栈常驻 [审批] 行。
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.apply_event(AgentEvent::AskingApproval {
            id: "r1".into(),
            tool: "run_shell".into(),
            description: "rm -rf /".into(),
        });
        assert!(m.approval_wait.is_none(), "informational event must not pin a wait row");
        // scrollback 仍保留 [approval] 历史行。
        assert!(m.output.iter().any(|l| l.text.contains("[approval] run_shell")));
    }

    #[test]
    fn pending_approval_sets_wait_row_cleared_by_answer() {
        let mut m = ReplModel::new("s".into(), "m".into(), 0);
        let (tx, _rx) = std::sync::mpsc::channel();
        m.set_pending_approval(
            ApprovalRequest {
                tool: "run_shell".into(),
                description: "rm -rf /etc".into(),
                args: serde_json::json!({"command": "rm -rf /etc"}),
            },
            tx,
        );
        assert_eq!(m.approval_wait.as_deref(), Some("run_shell: rm -rf /etc"));
        m.pending_answer("y");
        assert!(m.approval_wait.is_none(), "answering clears the wait row");
        assert!(m.pending.is_none());
    }

    #[test]
    fn pending_guard_sets_wait_row_cleared_by_answer() {
        let mut m = ReplModel::new("s".into(), "m".into(), 0);
        let (tx, _rx) = std::sync::mpsc::channel();
        m.set_pending_guard(
            GuardRequest {
                tool: "run_shell".into(),
                reason: "high risk".into(),
                command: None,
                path: None,
            },
            tx,
        );
        assert_eq!(m.approval_wait.as_deref(), Some("run_shell: high risk"));
        m.pending_answer("3");
        assert!(m.approval_wait.is_none());
        assert!(m.pending.is_none());
    }

    #[test]
    fn tool_result_and_done_clear_wait_row_backstop() {
        // 兜底:即使 approval_wait 被某些路径置位而无 pending,工具完成/回合
        // 结束也要清掉,避免 live 栈残留 [审批] 行。
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.approval_wait = Some("run_shell: rm -rf /".into());
        m.apply_event(AgentEvent::ToolResult {
            id: "t1".into(),
            name: "run_shell".into(),
            ok: true,
            output: "done".into(),
            output_path: None,
        });
        assert!(m.approval_wait.is_none(), "ToolResult must clear the wait row");

        m.approval_wait = Some("run_shell: rm -rf /".into());
        m.apply_event(AgentEvent::Done {
            summary: "done".into(),
            usage: None,
            session_id: "s1".into(),
            reasoning: None,
        });
        assert!(m.approval_wait.is_none(), "Done must clear the wait row");

        m.approval_wait = Some("run_shell: rm -rf /".into());
        m.apply_event(AgentEvent::Error { message: "boom".into() });
        assert!(m.approval_wait.is_none(), "Error must clear the wait row");
    }

    #[test]
    fn resume_loads_messages_into_output() {
        let store = rc_state::Store::open_in_memory().unwrap();
        let s = store.create_session("/tmp/proj").unwrap();
        store.append_message(&s.id, rc_state::MessageRole::User, "hello old").unwrap();
        let mut m = ReplModel::new("current".into(), "m".into(), 128_000);
        m.resume_session(&store, &s.id).unwrap();
        assert_eq!(m.session_id, s.id);
        assert!(m.output.iter().any(|l| l.text.contains("hello old")));
    }

    #[test]
    fn title_lazily_generates_from_first_done_summary() {
        let mut m = ReplModel::new("s".into(), "m".into(), 0);
        m.apply_event(AgentEvent::Done {
            summary: "Fixes the login crash\n\nDetails...".into(),
            usage: None, session_id: "s".into(), reasoning: None,
        });
        assert_eq!(m.title.as_deref(), Some("Fixes the login crash"));
        // 第二次 Done 不覆盖已有标题。
        m.apply_event(AgentEvent::Done {
            summary: "Other task".into(), usage: None, session_id: "s".into(), reasoning: None,
        });
        assert_eq!(m.title.as_deref(), Some("Fixes the login crash"));
    }

    #[test]
    fn set_title_overrides_and_truncates() {
        let mut m = ReplModel::new("s".into(), "m".into(), 0);
        m.set_title("a".repeat(120).as_str());
        assert!(m.title.as_ref().unwrap().chars().count() <= 40);
    }

    #[test]
    fn scroll_state_manages_offset() {
        let mut m = ReplModel::new("s".into(), "m".into(), 0);
        for i in 0..10 { m.push_line(format!("l{i}"), LineStyle::Plain); }
        m.scroll_up(3);
        assert_eq!(m.scroll_offset, 3);
        assert!(!m.autoscroll, "user scroll unlocks autoscroll");
        m.scroll_to_bottom();
        assert_eq!(m.scroll_offset, 0);
        assert!(m.autoscroll, "back to bottom re-pins");
    }

    #[test]
    fn user_scroll_not_yanked_by_new_content() {
        let mut m = ReplModel::new("s".into(), "m".into(), 0);
        for i in 0..10 { m.push_line(format!("l{i}"), LineStyle::Plain); }
        m.scroll_up(3);
        assert_eq!(m.scroll_offset, 3);
        assert!(!m.autoscroll);
        // 新内容到达且 autoscroll=false → 不重置 offset,不夺回贴底。
        m.push_line("new".into(), LineStyle::Plain);
        assert_eq!(m.scroll_offset, 3);
        assert!(!m.autoscroll);
        // 回到底部 → 重新贴底。
        m.scroll_to_bottom();
        assert_eq!(m.scroll_offset, 0);
        assert!(m.autoscroll);
    }

    #[test]
    fn message_markers_list_user_rows() {
        let mut m = ReplModel::new("s".into(), "m".into(), 0);
        m.push_line_kind("u1".into(), LineStyle::Accent, LineKind::User);
        m.push_line("a1".into(), LineStyle::Plain);
        m.push_line_kind("u2".into(), LineStyle::Accent, LineKind::User);
        let markers = m.message_markers();
        assert_eq!(markers, vec![0, 2]);
    }

    #[test]
    fn steers_queue_and_drain_in_order() {
        let mut m = ReplModel::new("s".into(), "m".into(), 0);
        m.enqueue_steer("first");
        m.enqueue_steer("second");
        assert_eq!(m.drain_steers(), vec!["first", "second"]);
        assert!(m.pending_steers.is_empty());
    }

    #[test]
    fn agent_turn_running_tracks_phase() {
        let mut m = ReplModel::new("s".into(), "m".into(), 0);
        assert!(!m.agent_turn_running());
        m.start_run();
        assert!(m.agent_turn_running());
    }

    #[test]
    fn queued_input_preserves_fifo_order() {
        let mut m = ReplModel::new("s".into(), "m".into(), 0);
        m.queued_input.push_back("one".into());
        m.queued_input.push_back("two".into());
        assert_eq!(m.queued_input.pop_front().as_deref(), Some("one"));
        assert_eq!(m.queued_input.pop_front().as_deref(), Some("two"));
        assert!(m.queued_input.is_empty());
    }

    #[test]
    fn defer_or_flush_queues_while_streaming_then_flushes_in_order() {
        let mut m = ReplModel::new("s".into(), "m".into(), 0);
        // 模拟流式进行中:streaming 缓冲区非空 → 审批/提问事件入队(不打断流)。
        m.streaming = Some("partial".into());
        m.defer_or_flush(AgentEvent::AskingApproval {
            id: "r1".into(),
            tool: "run_shell".into(),
            description: "rm -rf /".into(),
        });
        m.defer_or_flush(AgentEvent::AskingQuestion {
            id: "r2".into(),
            question: "proceed?".into(),
            session_id: "s".into(),
        });
        assert_eq!(m.interrupt_queue.len(), 2);
        // 流结束(streaming 清空)→ flush_interrupts 按序 apply(FIFO)。
        m.streaming = None;
        m.flush_interrupts();
        assert!(m.interrupt_queue.is_empty());
        let texts: Vec<&str> = m.output.iter().map(|l| l.text.as_str()).collect();
        let ap = texts.iter().position(|t| t.contains("[approval] run_shell"));
        let q = texts.iter().position(|t| t.contains("[question] proceed?"));
        assert!(ap.is_some(), "deferred approval applied: {texts:?}");
        assert!(q.is_some(), "deferred question applied: {texts:?}");
        assert!(ap.unwrap() < q.unwrap(), "FIFO order preserved");
    }

    #[test]
    fn defer_or_flush_applies_immediately_when_not_streaming() {
        let mut m = ReplModel::new("s".into(), "m".into(), 0);
        m.defer_or_flush(AgentEvent::AskingApproval {
            id: "r1".into(),
            tool: "run_shell".into(),
            description: "x".into(),
        });
        assert!(m.interrupt_queue.is_empty());
        assert!(m.output.iter().any(|l| l.text.contains("[approval] run_shell")));
    }

    #[test]
    fn backtrack_armed_defaults_false_and_is_public() {
        let mut m = ReplModel::new("s".into(), "m".into(), 0);
        assert!(!m.backtrack_armed);
        m.backtrack_armed = true;
        assert!(m.backtrack_armed);
    }
}
