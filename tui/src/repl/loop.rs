//! Interactive REPL (claude-code-style line UI) main loop.
//!
//! Migrated from `crates/rc-cli/src/repl.rs`. Owns the orchestration loop:
//! runs tasks via `Agent::run`, executes slash commands, and renders through the
//! raincode-tui model/shell. All displayed state comes from real `AgentEvent`s.
//! Hosting-CLI specifics (registry / store / providers) go through [`ReplEnv`].

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use futures::StreamExt;
use rc_core::Agent;
use rc_proto::AgentEvent;
use rc_sandbox::{
    ApprovalDecision, ApprovalRequest, GuardConsent, GuardRequest, PromptHook, PromptUserHook,
    UserInputHook,
};
use rc_skill::SkillStore;
use serde_json::json;

use crate::repl::command::{self, Cmd};
use crate::repl::env::{AgentFeed, ReplEnv};
use crate::repl::fmt::truncate_line;
use crate::repl::model::{LineKind, LineStyle, Phase, ReplModel, SessionEntry};
use crate::repl::render;
use crate::repl::shell::{read_keys, Shell};

/// 主循环事件:Agent 任务事件 + /chat 对话流事件(ChatDelta 增量 / ChatDone 收尾)。
enum LoopEvent {
    Agent(AgentEvent),
    ChatDelta(String),
    ChatDone { reply: String },
    /// 监督判断结果(judge 在后台 tokio 任务跑,结果经事件通道回到主循环,
    /// 避免 LLM 判断阻塞键盘/事件处理)。携带 spawn 时的 run_epoch:运行结束后
    /// 返回的陈旧判断按 epoch 丢弃,避免打到下一次运行的同 id agent。
    SupervisorAction {
        action: rc_core::SupervisorAction,
        epoch: u64,
    },
}

enum HookMsg {
    Approval {
        req: ApprovalRequest,
        reply: std::sync::mpsc::Sender<ApprovalDecision>,
    },
    Question {
        text: String,
        reply: std::sync::mpsc::Sender<String>,
    },
    /// 监督授权闸:高危操作的四选一(0=拒绝 1=仅本次 2=本会话 3=永久)。
    Guard {
        req: GuardRequest,
        reply: std::sync::mpsc::Sender<GuardConsent>,
    },
}

pub(crate) enum Action {
    None,
    Cmd(Cmd),
    Complete,
    FocusNext,
    CycleRisk,
    Interrupt,
    Quit,
    Clear,
    /// 折叠/展开任务树看板(Ctrl+t)。
    ToggleTree,
    /// 模型选择器选中一个条目(Enter)→ 主循环切活跃模型。
    ModelPick,
    /// 会话选择器选中一个条目(Enter)→ 主循环恢复该会话。
    SessionPick,
}

/// 任务难度(意图路由):Simple→普通模式(单模型+skill),Complex→Thinking(展开网络)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Difficulty {
    Simple,
    Complex,
}

/// 每次任务判一次难度:用活跃 provider 发一次极小请求(Reply 'simple' or 'complex'),
/// 返回 Simple/Complex;任何失败回退 Simple(不让路由卡住用户)。
async fn classify_difficulty(env: &dyn ReplEnv, prompt: &str) -> Difficulty {
    use futures::StreamExt;
    let registry = match env.load_registry() {
        Ok(r) => r,
        Err(_) => return Difficulty::Simple,
    };
    let provider = match env.make_provider(&registry) {
        Ok(p) => p,
        Err(_) => return Difficulty::Simple,
    };
    let req = rc_pro::canonical::CanonicalRequest {
        model: provider.id().to_string(),
        messages: vec![rc_pro::canonical::CanonicalMessage::user(format!(
            "Classify the difficulty of this task. Reply with exactly one word: simple or complex.\nTask: {prompt}"
        ))],
        tools: vec![],
        temperature: Some(0.0),
        max_tokens: Some(5),
        stream: true,
        extra: json!({}),
    };
    let mut stream = match provider.stream(req).await {
        Ok(s) => s,
        Err(_) => return Difficulty::Simple,
    };
    let mut text = String::new();
    while let Some(ev) = stream.next().await {
        if let Ok(rc_pro::canonical::ProvEvent::Delta { text: t }) = ev {
            text.push_str(&t);
        }
    }
    if text.to_lowercase().contains("complex") {
        Difficulty::Complex
    } else {
        Difficulty::Simple
    }
}

/// Thinking 模式状态机:先 plan_only 拆解 → 用户批准 → 再 execute。
#[derive(Default)]
struct ThinkingFlow {
    /// 一个 plan_only run 正在飞行(等 plan_only 的 Done 触发自动执行)。
    plan_running: bool,
    /// plan_only run 的 prompt(等 Done 时取走)。
    prompt: Option<String>,
}

/// 在独立线程启动一次 route_run(plan_only 或 execute),事件经 emit 转发给 TUI。
/// 从共享 `risk_mode` Arc 读当前值透传给 CLI route(驱动 RiskState 棘轮策略)。
/// `supervisor` 为 Some(监督会话已启动)时,route_run 会把子代理事件转发到 `feed`,
/// TUI 主循环周期排空 + judge(本函数只传递,不在此判断)。
// 参数多但都是 route_run 的显式依赖;包 context struct 会大改 3 个调用点,保持现状。
#[allow(clippy::too_many_arguments)]
fn start_route_run(
    model: &mut ReplModel,
    event_tx: &tokio::sync::mpsc::UnboundedSender<LoopEvent>,
    prompt: String,
    plan_only: bool,
    run_cancel: &mut Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    steer_hub: &std::sync::Arc<rc_core::SteerHub>,
    risk_mode: &std::sync::Arc<std::sync::Mutex<rc_router::risk::RiskMode>>,
    subagent_approval: &std::sync::Arc<dyn rc_sandbox::ApprovalHook>,
    supervisor: &Option<std::sync::Arc<rc_core::Supervisor>>,
    feed: &AgentFeed,
    env: &dyn ReplEnv,
) {
    model.start_run();
    model.focus_agent = None;
    let tx = event_tx.clone();
    let hub = steer_hub.clone();
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    *run_cancel = Some(cancel.clone());
    let risk_mode = risk_mode
        .lock()
        .map(|m| *m)
        .unwrap_or(rc_router::risk::RiskMode::Ask);
    env.route_run(
        prompt,
        plan_only,
        {
            let tx = tx.clone();
            std::sync::Arc::new(move |ev: AgentEvent| {
                let _ = tx.send(LoopEvent::Agent(ev));
            })
        },
        hub,
        cancel,
        risk_mode,
        subagent_approval.clone(),
        supervisor.clone(),
        feed.clone(),
    );
}

/// 把 route_run 线程写入 feed 的子代理事件取走,并入本地累积批次(主循环判断用)。
fn drain_supervisor_feed(feed: &AgentFeed, acc: &mut Vec<AgentEvent>) {
    let mut drained = feed.lock().unwrap_or_else(|p| p.into_inner());
    acc.append(&mut drained);
}

fn risk_approval_hook(
    tx: &tokio::sync::mpsc::UnboundedSender<HookMsg>,
    mode: std::sync::Arc<std::sync::Mutex<rc_router::risk::RiskMode>>,
) -> std::sync::Arc<dyn rc_sandbox::ApprovalHook> {
    let tx = tx.clone();
    std::sync::Arc::new(PromptHook::new(move |req: &ApprovalRequest| {
        use rc_router::risk::RiskGate;
        // 读共享的风险模式:经 risk_gate 映射 — Auto 放行,Manual 只拦高危,Ask/Assisted 弹审批。
        let mode = mode.lock().map(|m| *m).unwrap_or(rc_router::risk::RiskMode::Ask);
        match rc_router::risk::risk_gate(mode) {
            RiskGate::Allow => return ApprovalDecision::Allow,
            RiskGate::Deny => {
                // Manual 不能把 agent 完全锁死:只拦高危命令(rm -rf/系统破坏/
                // 上传等),安全命令(git status/cp 工作区内等)放行,否则无法工作。
                let high_risk = req.tool == "run_shell"
                    && rc_sandbox::guard::command_is_high_risk(&req.description);
                if high_risk {
                    return ApprovalDecision::Deny {
                        reason: "risk mode: manual (high-risk command)".into(),
                    };
                }
                return ApprovalDecision::Allow;
            }
            // Prompt(Ask/Assisted)→ 弹审批(原 Ask 分支逻辑)。
            RiskGate::Prompt => {}
        }
        let (reply, rx) = std::sync::mpsc::channel();
        if tx.send(HookMsg::Approval { req: req.clone(), reply }).is_err() {
            return ApprovalDecision::Deny { reason: "repl closed".into() };
        }
        rx.recv().unwrap_or(ApprovalDecision::Deny { reason: "repl closed".into() })
    }))
}

fn repl_user_hook(tx: &tokio::sync::mpsc::UnboundedSender<HookMsg>) -> std::sync::Arc<dyn UserInputHook> {
    let tx = tx.clone();
    std::sync::Arc::new(PromptUserHook::new(move |question: &str| {
        let (reply, rx) = std::sync::mpsc::channel();
        if tx.send(HookMsg::Question { text: question.to_string(), reply }).is_err() {
            return "No response provided; use best judgment.".into();
        }
        rx.recv().unwrap_or_else(|_| "No response provided; use best judgment.".into())
    }))
}

/// 监督授权闸 hook:高危操作把 GuardRequest 发给主循环 → `set_pending_guard` 弹
/// 0/1/2/3 选择,阻塞等用户答案(GuardConsent)。主循环关闭/无应答 → Deny(最保守)。
fn repl_guard_hook(
    tx: &tokio::sync::mpsc::UnboundedSender<HookMsg>,
) -> std::sync::Arc<dyn rc_sandbox::GuardHook> {
    let tx = tx.clone();
    std::sync::Arc::new(rc_sandbox::guard_hook::PromptGuardHook::new(
        move |req: &GuardRequest| {
            let (reply, rx) = std::sync::mpsc::channel();
            if tx.send(HookMsg::Guard { req: req.clone(), reply }).is_err() {
                return GuardConsent::Deny;
            }
            rx.recv().unwrap_or(GuardConsent::Deny)
        },
    ))
}

/// 自然语言风险档位切换的短意图提取:"risk manual" / "风险改成 auto"。
/// 只有命中合法档位词才返回(否则 None,回落普通任务),避免误吞如
/// "risk analysis of the deploy" 这类真实任务。
fn try_nl_risk(text: &str) -> Option<String> {
    let t = text.trim().to_lowercase();
    let word = t
        .strip_prefix("risk ")
        .or_else(|| t.strip_prefix("风险改成 "))
        .or_else(|| t.strip_prefix("风险改为 "))
        .or_else(|| t.strip_prefix("风险调到 "))
        .or_else(|| t.strip_prefix("风险模式 "))
        .map(str::trim)
        .filter(|w| !w.is_empty())?;
    if rc_router::risk::parse_risk_mode(word).is_ok() {
        Some(word.to_string())
    } else {
        None
    }
}

/// 问候/闲聊检测(对齐 Claude Code:纯寒暄不应触发一次完整任务跑)。
/// 只匹配「整条输入就是一句问候/确认」的短输入;任何带实际任务内容的输入
/// 都不命中,避免误吞真实请求。
fn is_smalltalk(text: &str) -> bool {
    let t = text.trim().to_lowercase();
    let t = t.trim_matches(|c: char| c.is_whitespace() || c == '?' || c == '？' || c == '!');
    matches!(
        t,
        "你好" | "hello" | "hi" | "hey" | "嗨" | "哈喽" | "在吗" | "在不在"
            | "早上好" | "下午好" | "晚上好" | "谢谢" | "感谢" | "好的" | "可以"
            | "ok" | "okay" | "yes" | "嗯" | "好"
    )
}

pub async fn repl_command(env: &dyn ReplEnv) -> Result<()> {
    let mut registry = env.load_registry()?;
    let skill_dir = env.skills_dir();
    let workspace = env.workspace();
    let session = env.create_session()?;
    // 输入历史按会话分文件(对齐 Claude Code 的会话隔离):每段对话的输入记录
    // 互不混杂,重启/恢复会话后仍能看到本会话的历史。
    let history_dir = env.home_dir().join("history");
    let _ = std::fs::create_dir_all(&history_dir);
    let history_path = history_dir.join(format!("{session}.jsonl"));
    let _ = std::fs::create_dir_all(env.home_dir());
    // 多 agent steering 注册表:route_run 为每个子代理 register,详情视图发 steer。
    let steer_hub = std::sync::Arc::new(rc_core::SteerHub::new());

    let mut agent_cfg = env
        .agent_config(&registry, true) // run_slash_command:chat 只在用户明确指令时执行
        .await?;
    // 共享的风险模式:approval hook 读它(经 risk_gate 决定放行/拒绝/弹审批),Shift+Tab 切换。
    let risk_mode = std::sync::Arc::new(std::sync::Mutex::new(rc_router::risk::RiskMode::Ask));
    let (hook_tx, mut hook_rx) = tokio::sync::mpsc::unbounded_channel::<HookMsg>();
    agent_cfg.approval = risk_approval_hook(&hook_tx, risk_mode.clone());
    // 子代理授权钩子 = 主 agent 的 risk 钩子:route 子代理跟随共享风险档。
    let subagent_approval = agent_cfg.approval.clone();
    agent_cfg.user_input = repl_user_hook(&hook_tx);
    // 授权闸 hook:高危操作(工作区外/上传/deny 命中)弹 0/1/2/3 四选一。
    // guard_cfg/guard_memo/guard_home 已由 FileEnv::agent_config 从 supervise.toml 加载。
    agent_cfg.guard_hook = Some(repl_guard_hook(&hook_tx));
    let agent = Agent::new(agent_cfg);

    let model_name = registry
        .active()
        .map(|p| p.model.clone())
        .unwrap_or_else(|| "未设置".into());
    let mut model = ReplModel::new(session.clone(), model_name.clone(), env.context_window(&registry));
    // 工作区根目录:汇报文件时补全为绝对路径(用户能直接看到落盘位置)。
    model.workspace = workspace.to_string_lossy().to_string();
    // 加载本会话历史(重启/恢复后 Up 箭头仍能看到本会话输入)。
    model.input.load_history(&history_path);
    if let Ok(mode) = risk_mode.lock() {
        model.risk_mode = *mode;
    }
    // 首屏欢迎:真实信息(版本/工作目录/会话/模型)+ 操作提示,避免界面光秃秃。
    let short_session: String = session.chars().take(8).collect();
    model.push_line(
        format!(
            "✻ raincode v{} · {}",
            env!("CARGO_PKG_VERSION"),
            workspace.to_string_lossy()
        ),
        LineStyle::Accent,
    );
    model.push_line(
        format!("session {short_session} · model {model_name}"),
        LineStyle::Dim,
    );
    // 首次使用:未配置真实模型(缺 API key 或兜底 Mock)时必须提示怎么导入,否则
    // 发任务只会跑 Mock 脚本,不会真正对话。
    let active_is_mock = registry
        .active()
        .map(|p| p.kind == rc_profile::model::ProfileKind::Mock)
        .unwrap_or(false);
    // 模型配置向导状态(Setup)。setup_active 时,每次 select 后轮询答案推进。
    let mut setup = Setup::default();
    let mut setup_rx: Option<std::sync::mpsc::Receiver<String>> = None;
    let mut setup_active = false;
    if env.make_provider(&registry).is_err() || active_is_mock {
        model.push_line("✻ 当前用的是 Mock 假模型(未配置真实模型),不会真正对话".into(), LineStyle::Warn);
        model.push_line("  开始配置模型…(向导),或随时输入 /setup 重开".into(), LineStyle::Dim);
        model.push_line("  (key 只存 ~/.raincode/keys/,绝不进项目文件夹)".into(), LineStyle::Dim);
        // 首次使用(活跃 profile 是 Mock):直接进入配置向导,免去记忆 CLI 命令。
        if active_is_mock {
            start_setup(&mut model, &mut setup, &mut setup_rx);
            setup_active = true;
        }
    }
    model.push_line(
        "裸行 = 发任务 · /chat = 对话 · / = 命令补全 · /help = 全部命令 · Ctrl+C 中断/退出".into(),
        LineStyle::Dim,
    );

    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<LoopEvent>();
    let (key_tx, mut key_rx) = tokio::sync::mpsc::unbounded_channel::<crossterm::event::KeyEvent>();
    std::thread::spawn(move || {
        let _ = read_keys(key_tx);
    });

    // 先进入 alternate screen,再播 splash(RAINCODE 在 alternate screen 里,
    // 退出时 leave 自动清除,不会残留);然后立即首帧进入 REPL。
    let mut shell = Shell::enter()?;
    crate::repl::splash::play(&mut std::io::stdout())?;
    let mut last_draw = Instant::now() - Duration::from_millis(100);
    let mut task_handle: Option<tokio::task::JoinHandle<()>> = None;
    let mut chat_handle: Option<tokio::task::JoinHandle<()>> = None;
    let mut chat_history: Vec<rc_pro::canonical::CanonicalMessage> = Vec::new();
    // 当前 route/autonomous run 的取消令牌:/stop 置位 → 引擎在下一检查点中断。
    let mut run_cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>> = None;
    // 运行纪元:每次 run 结束(Done/Error)自增。监督 judge 在后台任务里跑,可能跨过
    // 运行结束才返回;用它判定"陈旧判断"并丢弃,避免打到下一次运行的同 id agent。
    let mut run_epoch: u64 = 0;
    // 意图路由覆盖(/thinking → Complex, /normal → Simple,作用于下一次任务)。
    let mut mode_override: Option<Difficulty> = None;
    // Thinking 模式状态机(plan_only → 确认 → execute)。
    let mut thinking = ThinkingFlow::default();
    // 监督接线(Task 7):feed 由 route_run 线程写入子代理事件,主循环周期排空 + 判断。
    // supervisor 由 /supervise 启动后持有;judge 放后台 tokio 任务,结果经事件通道回。
    let agent_feed: AgentFeed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut supervisor: Option<std::sync::Arc<rc_core::Supervisor>> = None;
    let mut sup_batch: Vec<AgentEvent> = Vec::new();
    let mut sup_since: Option<Instant> = None;
    let mut sup_judge_in_flight = false;
    // splash 后立即同步首帧:覆盖 RAINCODE,避免残留到循环首个事件才重绘。
    draw(&mut shell, &model, Instant::now())?;
    let mut drawn_once = true;
    let mut ticker = tokio::time::interval(Duration::from_millis(250));
    let mut quit = false;

    while !quit {
        let mut force_draw = false;
        let mut ticked = false;
        tokio::select! {
            Some(ev) = event_rx.recv() => {
                // run 收尾(Done/Error)→ 清取消令牌 + 清监督待判批次(避免对已结束
                // 的 run 做陈旧判断);Error 还清掉 Thinking 状态机(plan_running/prompt),
                // 否则下一次 Done 会用旧的 prompt 弹"批准执行?"确认、误执行过期任务。
                teardown_run(
                    &mut thinking,
                    &mut task_handle,
                    &mut run_cancel,
                    &mut sup_batch,
                    &mut sup_since,
                    &mut run_epoch,
                    &ev,
                );
                // Thinking:plan_only 完成(Done)→ 弹批准确认,批准后再 execute。
                let is_done = matches!(&ev, LoopEvent::Agent(AgentEvent::Done { .. }));
                match ev {
                    LoopEvent::Agent(agent_event) => {
                        let is_terminal = matches!(
                            &agent_event,
                            AgentEvent::Done { .. } | AgentEvent::Error { .. }
                        );
                        // InterruptManager FIFO:审批/提问/工具结果事件在流式期间入队
                        // (不打断流),流结束按序 flush;其余事件(Token/ToolCall/…)直接应用。
                        match &agent_event {
                            AgentEvent::AskingApproval { .. }
                            | AgentEvent::AskingQuestion { .. }
                            | AgentEvent::ToolResult { .. } => {
                                model.defer_or_flush(agent_event)
                            }
                            _ => model.apply_event(agent_event),
                        }
                        // 流已结束(streaming 清空)→ 按序 apply 延迟的审批/提问/结果。
                        if model.streaming.is_none() {
                            model.flush_interrupts();
                        }
                        // 任务完成(Done/Error,且非 plan_only)→ 冲刷排队消息:
                        // queued_input(运行中 Tab 排队)+ pending_steers(运行中
                        // 无 focus 提交的 steer)按序重提交为新任务。pending_steers
                        // 无 core 消费方,若不在此时冲刷会永久滞留。
                        if is_terminal && !thinking.plan_running {
                            let mut queue: VecDeque<String> = VecDeque::new();
                            queue.append(&mut model.queued_input);
                            queue.extend(model.drain_steers());
                            while let Some(text) = queue.pop_front() {
                                if !execute_cmd(
                                    &mut model,
                                    &Cmd::Run(text),
                                    &mut shell,
                                    &agent,
                                    &event_tx,
                                    &mut task_handle,
                                    &mut chat_handle,
                                    &mut chat_history,
                                    &mut registry,
                                    &skill_dir,
                                    &steer_hub,
                                    &mut run_cancel,
                                    &mut mode_override,
                                    &mut thinking,
                                    &risk_mode,
                                    &subagent_approval,
                                    &supervisor,
                                    &agent_feed,
                                    env,
                                )
                                .await?
                                {
                                    quit = true;
                                    break;
                                }
                            }
                        }
                    }
                    LoopEvent::ChatDelta(text) => model.stream_chat_delta(&text),
                    LoopEvent::ChatDone { reply } => {
                        model.flush_stream();
                        // 流已结束:与 agent 事件 flush 一致,把流式期间延迟的
                        // 审批/提问/工具结果按 FIFO apply,避免留到下次 agent 事件。
                        model.flush_interrupts();
                        // 持久化 chat 回复(与用户消息对应),resume 后可回看。
                        if let Ok(store) = env.open_store() {
                            let _ = store.append_message(
                                &model.session_id,
                                rc_state::MessageRole::Assistant,
                                &reply,
                            );
                        }
                        chat_history.push(rc_pro::canonical::CanonicalMessage::assistant_text(reply));
                        chat_handle = None;
                    }
                    // 监督判断结果:Interrupt → SteerHub 注入 STOP + 红色 [监督] 行。
                    // epoch 不匹配 = 上个运行的陈旧判断(运行已结束或新运行已开始):
                    // 丢弃,不注入 STOP(否则会打到下一次运行的同 id agent)。
                    LoopEvent::SupervisorAction { action, epoch } => {
                        sup_judge_in_flight = false;
                        if epoch != run_epoch {
                            model.push_line(
                                "监督:上个运行的判断已过期,已丢弃".into(),
                                LineStyle::Dim,
                            );
                        } else if let Some(sup) = &supervisor {
                            match &action {
                                rc_core::SupervisorAction::Interrupt { agent_id, reason } => {
                                    sup.apply(&action, &steer_hub); // STOP 注入 steer 通道
                                    model.push_supervisor_line(format!(
                                        "{agent_id} 越界 → 已发送 STOP: {reason}"
                                    ));
                                }
                                rc_core::SupervisorAction::Suggest { reason } => {
                                    model.push_supervisor_line(format!("建议: {reason}"));
                                }
                                rc_core::SupervisorAction::Observe => {}
                            }
                        }
                    }
                }
                // Thinking:plan_only 的 Done 到了 → 自动执行(模型判复杂即自动
                // 展开子代理网络,不再弹"批准执行?"——用户无需手动 /route)。
                if is_done && thinking.plan_running {
                    thinking.plan_running = false;
                    let prompt = thinking.prompt.take().unwrap_or_default();
                    start_route_run(
                        &mut model, &event_tx, prompt, false,
                        &mut run_cancel, &steer_hub, &risk_mode,
                        &subagent_approval, &supervisor, &agent_feed, env,
                    );
                }
            }
            Some(key) = key_rx.recv() => {
                force_draw = true;
                let action = handle_key(&mut model, key);
                match action {
                    Action::None => {}
                    Action::Complete => complete_slash(&mut model),
                    Action::FocusNext => model.focus_next_agent(),
                    // Shift+Tab:循环风险模式,同步共享 Arc 供 approval hook 读取。
                    Action::CycleRisk => {
                        model.cycle_risk_mode();
                        if let Ok(mut m) = risk_mode.lock() {
                            *m = model.risk_mode;
                        }
                        model.push_line(
                            format!("⏵⏵ {} (shift+tab to cycle)", model.risk_label()),
                            LineStyle::Dim,
                        );
                    }
                    Action::Interrupt => {
                        if let Some(cancel) = &run_cancel {
                            // route/autonomous 在独立线程:置位取消令牌,引擎在下一
                            // 检查点中断,随后发 Error{cancelled} → 显示"已中断"。
                            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                            model.push_line("[stop] 正在中断…".into(), LineStyle::Dim);
                        } else if model.phase == Phase::Running {
                            agent.cancel();
                            model.push_line("[stop] 正在中断…".into(), LineStyle::Dim);
                        } else if let Some(h) = chat_handle.take() {
                            h.abort();
                            model.flush_stream();
                            model.push_line("[stop] 对话已中断".into(), LineStyle::Dim);
                            chat_history.clear();
                        } else {
                            quit = true;
                        }
                    }
                    Action::Quit => quit = true,
                    Action::Clear => {
                        model.output.clear();
                        model.streaming = None;
                        model.scroll_to_bottom();
                        shell.clear_all()?;
                    }
                    Action::ToggleTree => model.tree_visible = !model.tree_visible,
                    // 模型选择器 Enter:切活跃模型。
                    Action::ModelPick => {
                        let picked = model
                            .model_picker
                            .as_ref()
                            .and_then(|p| p.selected_entry().map(|e| e.id.clone()));
                        model.model_picker = None;
                        if let Some(id) = picked {
                            match use_model(env, &mut registry, &id) {
                                Ok(lines) => {
                                    for l in lines {
                                        model.push_line(l, LineStyle::Dim);
                                    }
                                }
                                Err(e) => {
                                    model.push_line(format!("[error] {e}"), LineStyle::Error);
                                }
                            }
                        }
                    }
                    // 会话选择器 Enter:恢复选中的历史会话(载入消息 + 重指向 session_id)。
                    Action::SessionPick => {
                        let picked = model
                            .session_picker
                            .as_ref()
                            .and_then(|p| p.selected_entry().cloned());
                        model.session_picker = None;
                        if let Some(entry) = picked {
                            match env.open_store() {
                                Ok(store) => match model.resume_session(&store, &entry.id) {
                                    Ok(()) => {
                                        // 恢复会话后清空在途 chat_history,避免 /chat 把上一
                                        // 会话的对话和新会话混成一次 provider 请求。
                                        chat_history.clear();
                                        model.push_line(
                                            format!("[resume] 会话 {}", entry.short_id),
                                            LineStyle::Dim,
                                        );
                                    }
                                    Err(e) => {
                                        model.push_line(format!("[error] {e}"), LineStyle::Error);
                                    }
                                },
                                Err(e) => {
                                    model.push_line(format!("[error] {e}"), LineStyle::Error);
                                }
                            }
                        }
                    }
                    Action::Cmd(cmd) => {
                        // /setup 与 /configure 拦截在 execute_cmd 之外:向导状态是 REPL 私有的。
                        if matches!(&cmd, Cmd::Setup) {
                            start_setup(&mut model, &mut setup, &mut setup_rx);
                            setup_active = true;
                        } else if let Cmd::Configure(text) = &cmd {
                            // 自然语言配置模型:识别供应商 → 预设进向导;识别不出列出目录。
                            let text = text.trim();
                            if text.is_empty() {
                                model.push_line(
                                    "/configure <自然语言> 例如:配置 kimi 的模型".into(),
                                    LineStyle::Dim,
                                );
                            } else if let Some(entry) = find_provider_in_text(text) {
                                model.push_line(
                                    format!("✻ 识别到供应商 {} ,开始配置…", entry.display_name),
                                    LineStyle::Tool,
                                );
                                start_setup_with_entry(&mut model, &mut setup, &mut setup_rx, entry);
                                setup_active = true;
                            } else {
                                let names: Vec<String> = rc_profile::catalog::catalog()
                                    .iter()
                                    .map(|e| format!("{} ({})", e.display_name, e.id))
                                    .collect();
                                model.push_line("? 没识别到供应商,可选:".into(), LineStyle::Warn);
                                for n in names {
                                    model.push_line(format!("  {n}"), LineStyle::Dim);
                                }
                            }
                        } else if let Cmd::Supervise(model_opt) = &cmd {
                            // 启动监督:持有 Supervisor 锚点(主循环状态),route 时子代理
                            // 事件经 feed 被周期 should_judge/judge。监督状态是 REPL 私有,
                            // 故在 execute_cmd 之外拦截。
                            match env.supervise_start(&registry, model_opt.as_deref()) {
                                Ok(sup) => {
                                    supervisor = Some(sup.clone());
                                    model.push_supervisor_line(format!(
                                        "✻ 监督会话已启动(model={})",
                                        sup.provider.id()
                                    ));
                                    model.push_line(
                                        "  先定义底线:如 '不要动项目目录以外的东西' / '别覆盖未标注文件' / '别把密钥写进代码'".into(),
                                        LineStyle::Dim,
                                    );
                                }
                                Err(e) => model.push_line(format!("[error] {e}"), LineStyle::Error),
                            }
                        } else if !execute_cmd(
                            &mut model, &cmd, &mut shell,
                            &agent, &event_tx, &mut task_handle,
                            &mut chat_handle, &mut chat_history, &mut registry,
                            &skill_dir, &steer_hub, &mut run_cancel,
                            &mut mode_override, &mut thinking, &risk_mode,
                            &subagent_approval, &supervisor, &agent_feed, env,
                        ).await? {
                            quit = true;
                        }
                        // 持久化历史(仅非 pending、非 secret 的 submit 进入)。
                        if !matches!(cmd, Cmd::Setup) {
                            let _ = model.input.append_history(&history_path);
                        }
                    }
                }
            }
            Some(msg) = hook_rx.recv() => {
                match msg {
                    HookMsg::Approval { req, reply } => model.set_pending_approval(req, reply),
                    HookMsg::Question { text, reply } => model.set_pending_question(text, reply),
                    HookMsg::Guard { req, reply } => model.set_pending_guard(req, reply),
                }
            }
            _ = ticker.tick() => { ticked = true; }
        }
        // 监督(Task 7):排空 feed 累积子代理事件;达阈值(should_judge)或已过 ~2s
        // 冷却 → 后台 judge(不阻塞主循环),结果经 LoopEvent::SupervisorAction 回到
        // 主循环处理(Interrupt → STOP / [监督] 行)。judge 在途时不再并发触发。
        if let Some(sup) = &supervisor {
            drain_supervisor_feed(&agent_feed, &mut sup_batch);
            if !sup_batch.is_empty() && sup_since.is_none() {
                sup_since = Some(Instant::now());
            }
            if !sup_judge_in_flight && !sup_batch.is_empty() {
                let cooled =
                    sup_since.map_or(true, |t| t.elapsed() >= Duration::from_secs(2));
                let batch_ready = sup.should_judge(&rc_core::SupervisorBatch {
                    events: sup_batch.clone(),
                    since: sup_since.unwrap_or_else(Instant::now),
                }) || cooled;
                if batch_ready {
                    let batch = rc_core::SupervisorBatch {
                        events: std::mem::take(&mut sup_batch),
                        since: sup_since.take().unwrap_or_else(Instant::now),
                    };
                    sup_judge_in_flight = true;
                    // 记录 spawn 时的运行纪元:运行在 judge 返回前结束 → 判断过期。
                    let epoch = run_epoch;
                    let sup = sup.clone();
                    let tx = event_tx.clone();
                    tokio::spawn(async move {
                        let action = sup.judge(&batch).await;
                        let _ = tx.send(LoopEvent::SupervisorAction { action, epoch });
                    });
                }
            }
        }
        // 向导推进:每次 select 后都轮询 setup_rx(try_recv 无答案时跳过)。
        if setup_active {
            if let Some(rx) = setup_rx.as_ref() {
                if let Ok(answer) = rx.try_recv() {
                    let done = wizard_advance(&mut setup, &mut model, &mut registry, &mut setup_rx, answer, env).await?;
                    if done {
                        setup_active = false;
                    }
                }
            }
        }
        // 节流重绘:按键/事件立即(≥30ms),纯 tick 只在需要动画时。
        let now = Instant::now();
        let animating = model.phase == Phase::Running
            || model
                .done_at
                .is_some_and(|t| now.duration_since(t) < Duration::from_millis(2_400));
        let fresh = now.duration_since(last_draw) >= Duration::from_millis(30);
        if !drawn_once || force_draw || (ticked && animating) || (!ticked && fresh) {
            draw(&mut shell, &model, now)?;
            drawn_once = true;
            last_draw = now;
        }
    }

    shell.leave()?;
    Ok(())
}

/// /stop:中断主 chat agent + 置位 route/autonomous 线程的取消令牌。
/// route/autonomous 在独立线程,靠 AtomicBool 取消(rc-router 检查点消费);
/// chat 主任务靠 agent.cancel()。运行结束(Done/Error)后主循环把 run_cancel 清回 None。
fn stop_run(
    model: &mut ReplModel,
    agent: &Agent,
    run_cancel: &Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
) {
    if model.phase == Phase::Running {
        agent.cancel();
        if let Some(cancel) = run_cancel {
            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        model.push_line("[stop] 正在中断…".into(), LineStyle::Dim);
    } else {
        model.push_line("没有运行中的任务".into(), LineStyle::Dim);
    }
}

/// run 收尾(Done/Error):清取消令牌、监督待判批次、推进运行纪元。
/// Error 还清掉 Thinking 状态机(plan_running/prompt),避免下一次 Done 弹旧确认、
/// 批准后误执行过期任务。
fn teardown_run(
    thinking: &mut ThinkingFlow,
    task_handle: &mut Option<tokio::task::JoinHandle<()>>,
    run_cancel: &mut Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    sup_batch: &mut Vec<AgentEvent>,
    sup_since: &mut Option<Instant>,
    run_epoch: &mut u64,
    ev: &LoopEvent,
) {
    match ev {
        LoopEvent::Agent(AgentEvent::Done { .. }) => {
            *task_handle = None;
            *run_cancel = None;
            sup_batch.clear();
            *sup_since = None;
            // 推进运行纪元:在途监督判断至此过期,回来后按 epoch 丢弃。
            *run_epoch += 1;
        }
        LoopEvent::Agent(AgentEvent::Error { .. }) => {
            *task_handle = None;
            *run_cancel = None;
            sup_batch.clear();
            *sup_since = None;
            *run_epoch += 1;
            thinking.plan_running = false;
            thinking.prompt = None;
        }
        _ => {}
    }
}

fn draw(shell: &mut Shell, model: &ReplModel, now: Instant) -> Result<()> {
    let (width, height) = shell.size()?;
    let frame = render::render(model, width as usize, height as usize, now);
    shell.draw(&frame)?;
    Ok(())
}

/// 编辑键(不含 Enter/Tab/Ctrl 组合)。
fn edit_input(model: &mut ReplModel, key: KeyEvent) {
    use KeyCode::*;
    match key.code {
        Backspace => model.input.delete_before(),
        Delete => model.input.delete_after(),
        Left => model.input.move_cursor(-1),
        Right => model.input.move_cursor(1),
        Home => model.input.move_to_start(),
        End => model.input.move_to_end(),
        Up => model.input.history_prev(),
        Down => model.input.history_next(),
        Char(ch) => model.input.insert_char(ch),
        _ => {}
    }
}

pub(crate) fn handle_key(model: &mut ReplModel, key: KeyEvent) -> Action {
    use KeyCode::*;
    // 鼠标滚轮 / PageUp/PageDown:任何状态下(含 pending 审批、选择器)都能滚动
    // 对话历史——滚轮被映射成 PageUp/PageDown。否则 pending 分支会把它们当
    // 普通编辑键吞掉,setup 向导/审批时历史就滚不动了。
    if key.modifiers.is_empty() {
        match key.code {
            PageUp => {
                model.scroll_up(5);
                return Action::None;
            }
            PageDown => {
                model.scroll_down(5);
                return Action::None;
            }
            _ => {}
        }
    }
    // setup 向导选择器:↑↓ 移动高亮,Enter 确认(按高亮项编号喂给向导),Esc 关闭。
    // 编号仍可直接输入 + Enter(走下方 pending 分支)。
    if model.setup_picker.is_some() {
        let (confirm, close) = match key.code {
            Up if key.modifiers.is_empty() => {
                if let Some(p) = &mut model.setup_picker {
                    p.selected = p.selected.saturating_sub(1);
                }
                (false, false)
            }
            Down if key.modifiers.is_empty() => {
                if let Some(p) = &mut model.setup_picker {
                    p.selected = (p.selected + 1) % p.items.len();
                }
                (false, false)
            }
            Enter if key.modifiers.is_empty() => (true, true),
            Esc => (false, true),
            _ => (false, false),
        };
        if confirm {
            let pick = model.setup_picker.as_ref().map(|p| p.selected + 1).unwrap_or(1);
            model.setup_picker = None;
            if model.pending.is_some() {
                model.resolve_pending(&pick.to_string());
            }
            return Action::None;
        }
        if close {
            model.setup_picker = None;
            return Action::None;
        }
    }
    if model.pending.is_some() {
        // 审批/守卫:单键回答(Y/N/A / 0-3),不进输入栏;问题仍需 Enter+文本。
        // 排除 Ctrl 组合:Ctrl+C 仍走下方 resolve_pending("") 的拒绝路径,不当成键入 'c'。
        if let Some(k) = match key.code {
            Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => Some(c.to_string()),
            _ => None,
        } {
            if model.pending_answer(&k) {
                return Action::None;
            }
        }
        match key.code {
            Enter => {
                if model.pending_is_secret() {
                    let answer = std::mem::take(&mut model.input.text);
                    model.input.cursor = 0;
                    model.resolve_pending(&answer);
                } else {
                    let answer = model.input.submit().unwrap_or_default();
                    model.resolve_pending(&answer);
                }
            }
            Char('c') | Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                model.resolve_pending("");
            }
            _ => edit_input(model, key),
        }
        return Action::None;
    }
    // 模型选择器模式(/model):键入过滤、↑↓ 选、Enter 确定、Esc 关闭。
    if model.model_picker.is_some() {
        return match key.code {
            Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                model.picker_query_push(c);
                Action::None
            }
            Backspace => {
                model.picker_query_backspace();
                Action::None
            }
            Up => {
                model.picker_prev();
                Action::None
            }
            Down => {
                model.picker_next();
                Action::None
            }
            Enter => Action::ModelPick,
            Esc | Char('c') | Char('d') => {
                model.model_picker = None;
                Action::None
            }
            _ => Action::None,
        };
    }
    // 会话选择器模式(/resume):键入过滤、↑↓ 选、Enter 确定、Esc 关闭(同 ModelPicker)。
    if model.session_picker.is_some() {
        return match key.code {
            Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                model.session_picker_query_push(c);
                Action::None
            }
            Backspace => {
                model.session_picker_query_backspace();
                Action::None
            }
            Up => {
                if let Some(p) = &mut model.session_picker {
                    p.prev();
                }
                Action::None
            }
            Down => {
                if let Some(p) = &mut model.session_picker {
                    p.next();
                }
                Action::None
            }
            Enter => Action::SessionPick,
            Esc | Char('c') | Char('d') => {
                model.session_picker = None;
                Action::None
            }
            _ => Action::None,
        };
    }
    // 先按当前输入刷新斜杠菜单(开/关/过滤),再决定键位。
    model.update_slash_menu();
    // 菜单开启时:↑↓ 选、Enter 填 /cmd 、Esc 只关菜单、Tab 循环候选。
    if model.slash_menu.is_some() {
        match key.code {
            Up => {
                model.slash_menu_prev();
                return Action::None;
            }
            Down => {
                model.slash_menu_next();
                return Action::None;
            }
            Enter => {
                if model.slash_menu_accept() {
                    return Action::None;
                }
                // 无候选(如 /zzz)→ 回落正常 submit(parse → Unknown)。
            }
            Esc => {
                model.slash_menu = None;
                return Action::None;
            }
            Tab => {
                model.slash_menu_next();
                return Action::None;
            }
            _ => {}
        }
    }
    match key.code {
        Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
            model.input.insert_newline();
            model.update_slash_menu();
            Action::None
        }
        Enter => match model.input.submit() {
            Some(text) => Action::Cmd(command::parse(&text).unwrap_or(Cmd::Unknown(text))),
            None => Action::None,
        },
        // 有 agent 时 Tab 循环切换选中;输入是 / 命令时保留补全。
        Tab if model.agents.is_empty() || model.input.text.trim_start().starts_with('/') => {
            Action::Complete
        }
        // 运行中 Tab + 有输入 → 排队到 queued_input(任务完成自动冲刷重提交)。
        // 置于 FocusNext 之前:运行态优先排队,不切 agent 焦点。
        Tab if model.agent_turn_running() && !model.input.text.trim().is_empty() => {
            let text = model.input.submit().unwrap_or_default();
            model.queued_input.push_back(text.clone());
            model.push_line(format!("[queued] {text}"), LineStyle::Tool);
            Action::None
        }
        Tab => Action::FocusNext,
        // agent 详情内导航(有 focus 且输入框为空时):,=prev  .=next  p=parent。
        // 输入框非空时这些键让位给 steer 输入(文档化流程:聚焦时输入内容 + Enter)。
        Char(',') if model.focus_agent.is_some()
            && model.input.text.is_empty()
            && key.modifiers.is_empty() =>
        {
            model.focus_prev_agent();
            Action::None
        }
        Char('.') if model.focus_agent.is_some()
            && model.input.text.is_empty()
            && key.modifiers.is_empty() =>
        {
            model.focus_next_agent();
            Action::None
        }
        Char('p') if model.focus_agent.is_some()
            && model.input.text.is_empty()
            && key.modifiers.is_empty() =>
        {
            model.focus_parent_agent();
            Action::None
        }
        // Shift+Tab:循环风险模式(Ask → Auto → Assisted → Manual)。
        BackTab => Action::CycleRisk,
        Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Action::Interrupt,
        Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => Action::Quit,
        Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => Action::Clear,
        // Ctrl+t:折叠/展开任务树看板。
        Char('t') if key.modifiers.contains(KeyModifiers::CONTROL) => Action::ToggleTree,
        // Esc 按状态分派:运行中 → 中断;空闲+空框 → 两级 backtrack(首次布防、
        // 二次恢复上条用户消息到输入框);否则退出 agent 详情(回到普通输入)。
        Esc => {
            if model.agent_turn_running() {
                return Action::Interrupt;
            }
            if model.input.text.is_empty() && model.focus_agent.is_none() {
                if model.backtrack_armed {
                    model.backtrack_armed = false;
                    // 二次 Esc:恢复上一条用户消息到输入框(简版 backtrack)。
                    if let Some(last_user) = model
                        .output
                        .iter()
                        .rev()
                        .find(|l| l.kind == LineKind::User)
                    {
                        // 实时路径存储的是带 `› ` 前缀的用户行,resume 路径则不带;
                        // 恢复到输入框时剥离显示前缀,避免提交时产生 `› › task`。
                        let text = last_user.text.strip_prefix("› ").unwrap_or(&last_user.text);
                        model.input.text = text.to_owned();
                        model.input.cursor = model.input.text.len();
                    }
                } else {
                    model.backtrack_armed = true;
                    model.push_line("Esc again to edit previous message".into(), LineStyle::Dim);
                }
                return Action::None;
            }
            model.focus_agent = None;
            model.backtrack_armed = false;
            Action::None
        }
        // Ctrl+R: reverse-i-search 历史(用当前输入作 query,匹配最近项)。
        Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            let query = model.input.text.trim();
            if !query.is_empty() {
                if let Some(m) = model.input.history_search(query) {
                    model.input.text = m;
                    model.input.cursor = model.input.text.len();
                }
            }
            Action::None
        }
        // Ctrl+O:展开/折叠完整思维链(思考完成后的 reasoning_chain)。
        Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            model.toggle_reasoning();
            Action::None
        }
        // (PageUp/PageDown 已在 handle_key 顶部处理,任何模式可滚)
        _ => {
            edit_input(model, key);
            model.update_slash_menu();
            // 任何非 Esc 键取消 backtrack 布防(避免二次 Esc 误恢复旧消息)。
            model.backtrack_armed = false;
            Action::None
        }
    }
}

/// 输入以 `/` 开头时补全为唯一匹配,多个匹配则列出候选。
pub(crate) fn complete_slash(model: &mut ReplModel) {
    let text = model.input.text.trim_start();
    let Some(rest) = text.strip_prefix('/') else { return };
    let (name, _) = rest.split_once(' ').unwrap_or((rest, ""));
    if name.is_empty() {
        return;
    }
    let matches = command::complete(name);
    if matches.len() == 1 {
        model.input.text = format!("/{} ", matches[0].name);
        model.input.cursor = model.input.text.len();
    } else if matches.len() > 1 {
        model.push_line(
            matches.iter().map(|m| format!("/{}  {}", m.name, m.desc)).collect::<Vec<_>>().join("\n"),
            LineStyle::Dim,
        );
    }
}

/// 真执行 /compact:make_provider → open_store → compact_session(锚定摘要 + 尾部逐字保留 ≈8k token)。
/// 任何失败 push [error] 并返回 Ok(true),不 panic(与 /chat、/resume 的错误处理一致)。
/// /compact 命令与 NL("压缩"/"compact")触发共用此执行体,避免重复。
async fn do_compact(
    model: &mut ReplModel,
    env: &dyn ReplEnv,
    registry: &mut rc_profile::model::Registry,
) -> Result<bool> {
    let provider = match env.make_provider(registry) {
        Ok(p) => p,
        Err(e) => {
            model.push_line(format!("[error] {e}"), LineStyle::Error);
            return Ok(true);
        }
    };
    let store = match env.open_store() {
        Ok(s) => s,
        Err(e) => {
            model.push_line(format!("[error] {e}"), LineStyle::Error);
            return Ok(true);
        }
    };
    model.push_line("✻ 正在压缩上下文…".into(), LineStyle::Dim);
    match rc_core::compact::compact_session(&*provider, &store, &model.session_id, 8_000).await {
        Ok(report) => {
            model.push_line(
                format!("[compact] 摘要已生成 · 历史 {} → {} 条", report.before, report.after),
                LineStyle::Success,
            );
            model.push_line(report.summary, LineStyle::Dim);
        }
        Err(e) => model.push_line(format!("[error] 压缩失败:{e}"), LineStyle::Error),
    }
    Ok(true)
}

/// 执行一条命令。返回 false 表示退出。
#[allow(clippy::too_many_arguments)]
async fn execute_cmd(
    model: &mut ReplModel,
    cmd: &Cmd,
    shell: &mut Shell,
    agent: &Agent,
    event_tx: &tokio::sync::mpsc::UnboundedSender<LoopEvent>,
    task_handle: &mut Option<tokio::task::JoinHandle<()>>,
    chat_handle: &mut Option<tokio::task::JoinHandle<()>>,
    chat_history: &mut Vec<rc_pro::canonical::CanonicalMessage>,
    registry: &mut rc_profile::model::Registry,
    skill_dir: &std::path::Path,
    steer_hub: &std::sync::Arc<rc_core::SteerHub>,
    run_cancel: &mut Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    mode_override: &mut Option<Difficulty>,
    thinking: &mut ThinkingFlow,
    risk_mode: &std::sync::Arc<std::sync::Mutex<rc_router::risk::RiskMode>>,
    subagent_approval: &std::sync::Arc<dyn rc_sandbox::ApprovalHook>,
    supervisor: &Option<std::sync::Arc<rc_core::Supervisor>>,
    feed: &AgentFeed,
    env: &dyn ReplEnv,
) -> Result<bool> {
    match cmd {
        Cmd::Quit => return Ok(false),
        Cmd::Help => {
            let lines = help_lines();
            push_lines(model, lines);
        }
        Cmd::Run(text) => {
            // 处于 agent 详情(选中某子代理)时,裸行 = 向该 agent 发 steer/命令,
            // 而不是启动新任务。焦点/运行态守卫先于 NL 短意图:运行中不压缩、不切
            // 会话(压缩会撞上在途 agent 正在写的 store;恢复会绕过 "用 /stop 中断" 警告)。
            if let Some(focus) = model.focus_agent.clone() {
                let sent = steer_hub.send(&focus, text);
                model.push_line(
                    if sent {
                        format!("[steer] → {focus}: {text}")
                    } else {
                        format!("[error] agent {focus} 不存在或已结束")
                    },
                    if sent { LineStyle::Tool } else { LineStyle::Error },
                );
                return Ok(true);
            }
            if model.agent_turn_running() {
                // 运行中提交 → 进 pending_steers(steer 队列),不渲染;footer 提示。
                // 下轮 Agent 回合的 steer_rx 经 core 的 steering checkpoint 注入。
                model.enqueue_steer(text);
                model.push_line(format!("[排队] {text}"), LineStyle::Tool);
                return Ok(true);
            }
            // 以下 NL 短意图只在空闲(非 Running、未聚焦 agent)时生效。
            // 自然语言恢复会话短意图:"继续上次"/"恢复会话"/"resume" → 打开会话选择器。
            if try_nl_resume(text) {
                start_session_picker(model, env);
                return Ok(true);
            }
            // 自然语言风险档位切换:命中 "risk manual" / "风险改成 auto" 短意图。
            if let Some(word) = try_nl_risk(text) {
                match rc_router::risk::parse_risk_mode(&word) {
                    Ok(m) => {
                        *risk_mode.lock().unwrap() = m;
                        model.risk_mode = m;
                        model.push_line(
                            format!("✻ 风险模式 → {}", model.risk_label()),
                            LineStyle::Tool,
                        );
                    }
                    Err(e) => model.push_line(format!("[error] {e}"), LineStyle::Error),
                }
                return Ok(true);
            }
            // 自然语言命中 /compact("压缩"/"compact")→ 压缩上下文,不进任务分类。
            // 与 /compact 命令共用 do_compact 执行体。
            if nl_compact_trigger(text) {
                return do_compact(model, env, registry).await;
            }
            model.push_user_line(format!("› {text}"));
            // 问候/闲聊(对齐 Claude Code):整条输入只是寒暄 → 不回显任务分类、不
            // 触发一次完整任务跑,直接给一句对话式回复。
            if is_smalltalk(text) {
                model.push_line(
                    "✻ 我在。直接发任务给我,或 /chat 对话、/help 看命令、/thinking 或 /normal 切换模式。".into(),
                    LineStyle::Accent,
                );
                return Ok(true);
            }
            // 意图路由:有 /thinking /normal 覆盖用覆盖;否则每次任务判一次难度。
            let mode = match mode_override.take() {
                Some(m) => m,
                None => classify_difficulty(env, text).await,
            };
            if mode == Difficulty::Complex {
                // Thinking 模式:先拆解出任务划分 → 确认 → 展开模型网络。
                model.push_line(
                    "✻ thinking:拆解任务划分 → 确认后展开模型网络".into(),
                    LineStyle::Tool,
                );
                thinking.plan_running = true;
                thinking.prompt = Some(text.clone());
                start_route_run(
                    model, event_tx, text.clone(), true,
                    run_cancel, steer_hub, risk_mode, subagent_approval,
                    supervisor, feed, env,
                );
            } else {
                // 普通模式:单模型 + skill(chat 模型主控,不展开网络)。
                model.push_line("✻ 普通模式:单模型执行".into(), LineStyle::Dim);
                model.start_run();
                // 克隆为 owned,避免 async move 捕获 `text: &String`。
                let prompt = text.clone();
                let tx = event_tx.clone();
                let agent = agent.clone();
                let session_id = model.session_id.clone();
                *task_handle = Some(tokio::spawn(async move {
                    let mut stream = agent.run(session_id, prompt);
                    while let Some(ev) = stream.next().await {
                        if tx.send(LoopEvent::Agent(ev)).is_err() {
                            break;
                        }
                    }
                }));
            }
        }
        Cmd::Stop => stop_run(model, agent, run_cancel),
        Cmd::Clear => {
            model.output.clear();
            model.streaming = None;
            shell.clear_all()?;
        }
        Cmd::Unknown(name) => {
            model.push_line(format!("? 未知命令 /{name} (见 /help)"), LineStyle::Error);
        }
        Cmd::Status => push_lines(model, status_lines(model)),
        Cmd::Title(name) => {
            // /title <name> 设置;裸 /title 清空(set_title 映射空→None)。
            let cleared = name.trim().is_empty();
            model.set_title(name);
            model.push_line(
                if cleared {
                    "✻ 标题已清除".into()
                } else {
                    format!("✻ 标题 → {name}")
                },
                LineStyle::Accent,
            );
        }
        Cmd::ListModels => push_lines(model, format_models(registry)),
        Cmd::UseModel(id) => {
            if id.is_empty() {
                // /model(无参数)→ 交互式选择器:搜索 + ↑↓ + Enter。
                match env.model_picker_entries() {
                    Ok(entries) => {
                        model.push_line("✻ 模型选择器:输入过滤 · ↑↓ 选择 · Enter 确定 · Esc 退出".into(), LineStyle::Dim);
                        model.open_model_picker(entries);
                    }
                    Err(e) => model.push_line(format!("[error] {e}"), LineStyle::Error),
                }
            } else {
                match use_model(env, registry, id) {
                    Ok(lines) => push_lines(model, lines),
                    Err(e) => model.push_line(format!("[error] {e}"), LineStyle::Error),
                }
            }
        }
        Cmd::ListSessions | Cmd::Resume(None) => start_session_picker(model, env),
        // /resume <id>:校验会话存在 → 载入历史并重指向 session_id。
        Cmd::Resume(Some(id)) => {
            let store = match env.open_store() {
                Ok(s) => s,
                Err(e) => {
                    model.push_line(format!("[error] {e}"), LineStyle::Error);
                    return Ok(true);
                }
            };
            match store.get_session(id) {
                Ok(Some(_)) => match model.resume_session(&store, id) {
                    Ok(()) => {
                        // 与 SessionPick 恢复一致:清空在途 chat_history,避免 /chat 混入
                        // 上一会话的对话。
                        chat_history.clear();
                        let short: String = id.chars().take(8).collect();
                        model.push_line(format!("[resume] 会话 {short}"), LineStyle::Dim);
                    }
                    Err(e) => model.push_line(format!("[error] {e}"), LineStyle::Error),
                },
                Ok(None) => model.push_line(
                    format!("[error] 会话 {id} 不存在(见 /resume 选择器)"),
                    LineStyle::Error,
                ),
                Err(e) => model.push_line(format!("[error] {e}"), LineStyle::Error),
            }
        }
        Cmd::ListSkills => {
            push_lines(model, format_skills(&SkillStore::new(skill_dir)));
            // 学习状态(对齐用户关切:skill 网络是否真的在学)。
            match env.open_store() {
                Ok(store) => match store.list_skills() {
                    Ok(skills) => push_lines(model, format_skill_learning(&skills)),
                    Err(e) => model.push_line(format!("[error] 读学习状态失败: {e}"), LineStyle::Error),
                },
                Err(e) => model.push_line(format!("[error] 打不开状态库: {e}"), LineStyle::Error),
            }
        }
        // /skill-nav <task>:导航 skill 网络。命中索引 → 菜单(用户 /skill-nav <子名> 下钻);
        // 命中叶子 → 完整正文。驱动方(FileEnv::skill_nav)执行带预算约束的下钻/回溯。
        Cmd::SkillNav(task) => {
            let task = task.trim().to_string();
            if task.is_empty() {
                model.push_line("/skill-nav <task> 导航 skill 网络".into(), LineStyle::Dim);
                return Ok(true);
            }
            match env.skill_nav(&task) {
                Ok(lines) => push_lines(model, lines),
                Err(e) => model.push_line(format!("[error] {e}"), LineStyle::Error),
            }
        }
        Cmd::Setup => {}
        // /configure 在主循环拦截(向导状态私有);这里兜底(不应到达)。
        Cmd::Configure(_) => {
            model.push_line("/configure 在向导层处理".into(), LineStyle::Dim);
        }
        // /route <prompt>:启动多 agent 路由执行(子代理并行 + steering 注册)。
        Cmd::Route(arg) => {
            let prompt = arg.trim();
            if prompt.is_empty() {
                model.push_line("/route <prompt> 启动多 agent 执行".into(), LineStyle::Dim);
                return Ok(true);
            }
            if model.phase == Phase::Running {
                model.push_line("已有任务运行中,用 /stop 中断".into(), LineStyle::Warn);
                return Ok(true);
            }
            model.push_user_line(format!("› /route {prompt}"));
            // route_run 自起独立线程 + runtime,主循环保持响应(期间可 Tab 选 agent + 发 steer)。
            start_route_run(
                model, event_tx, prompt.to_string(), false,
                run_cancel, steer_hub, risk_mode, subagent_approval,
                supervisor, feed, env,
            );
            *task_handle = None; // route 在独立线程,主循环 task_handle 不跟踪它
        }
        // /autonomous <prompt>:自动化开发模式。用主控(allocator)拆解成任务树,
        // 把 OrchestratorPlan/OrchestratorDispatch 事件渲染到界面。
        Cmd::Autonomous(arg) => {
            // `--plan` 前缀:只拆解计划不执行(拆解预览);默认真执行。
            let (plan_only, prompt) = match arg.trim().strip_prefix("--plan ") {
                Some(rest) => (true, rest.trim().to_string()),
                None if arg.trim() == "--plan" => (true, String::new()),
                None => (false, arg.trim().to_string()),
            };
            if prompt.is_empty() {
                model.push_line("/autonomous [--plan] <prompt> 自动化开发模式".into(), LineStyle::Dim);
                return Ok(true);
            }
            if model.phase == Phase::Running {
                model.push_line("已有任务运行中,用 /stop 中断".into(), LineStyle::Warn);
                return Ok(true);
            }
            model.push_user_line(format!("› /autonomous {prompt}"));
            // 旗舰能力 banner:自动选模型 + 真执行(rc-router 引擎)。
            if plan_only {
                model.push_line("✻ 自动编排(plan):拆解任务树 → 预览自动选模型".into(), LineStyle::Tool);
            } else {
                model.push_line("✻ 自动编排:拆解 → 按能力/成本自动选模型 → 子代理执行".into(), LineStyle::Tool);
            }
            // rc-router 引擎在独立线程跑(拆解→自动选模型→子代理→结果回灌→Done),
            // 事件经 emit 转发;主循环保持响应(期间可 Tab 选 agent + 发 steer)。
            start_route_run(
                model, event_tx, prompt, plan_only,
                run_cancel, steer_hub, risk_mode, subagent_approval,
                supervisor, feed, env,
            );
            *task_handle = None; // autonomous 在独立线程,主循环 task_handle 不跟踪它
        }
        // /supervise 在主循环拦截(需持有 Supervisor 锚点,状态是 REPL 私有的);这里兜底(不应到达)。
        Cmd::Supervise(_) => {
            model.push_line("/supervise 在监督层处理".into(), LineStyle::Dim);
        }
        Cmd::Thinking => {
            *mode_override = Some(Difficulty::Complex);
            model.push_line(
                "⏵ 下一次任务强制 Thinking 模式(展开模型网络)".into(),
                LineStyle::Tool,
            );
        }
        Cmd::Normal => {
            *mode_override = Some(Difficulty::Simple);
            model.push_line(
                "⏵ 下一次任务强制普通模式(单模型+skill)".into(),
                LineStyle::Dim,
            );
        }
        Cmd::Risk(Some(arg)) => {
            match rc_router::risk::parse_risk_mode(arg) {
                Ok(m) => {
                    *risk_mode.lock().unwrap() = m;
                    model.risk_mode = m;
                    model.push_line(
                        format!("✻ 风险模式 → {}", model.risk_label()),
                        LineStyle::Tool,
                    );
                }
                Err(e) => model.push_line(format!("[error] {e}"), LineStyle::Error),
            }
        }
        Cmd::Risk(None) => {
            let m = *risk_mode.lock().unwrap();
            model.risk_mode = m;
            model.push_line(format!("风险模式:{}", model.risk_label()), LineStyle::Dim);
            model.push_line(
                "  四档:auto(自动) assisted(低风险自动) ask(弹确认) manual(全手动)".into(),
                LineStyle::Dim,
            );
        }
        // /compact:真执行(摘要 + 保留最近 8 条),不再走 slash 占位。
        Cmd::Compact => return do_compact(model, env, registry).await,
        // /refresh:更新模型评分(拉取 OpenRouter/arena 真实榜单)。网络失败时报错不崩。
        Cmd::Refresh => {
            model.push_line("✻ 正在拉取模型评分(OpenRouter/arena)…".into(), LineStyle::Dim);
            match env.refresh_profiles().await {
                Ok(summary) => model.push_line(summary, LineStyle::Success),
                Err(e) => model.push_line(format!("[error] 刷新失败:{e:#}"), LineStyle::Error),
            }
        }
        Cmd::Chat(text) => {
            if text.trim().is_empty() {
                model.push_line("/chat <text> 说点什么".into(), LineStyle::Dim);
                return Ok(true);
            }
            if chat_handle.is_some() || model.phase == Phase::Running {
                model.push_line("对话进行中或任务运行中,先 /stop".into(), LineStyle::Warn);
                return Ok(true);
            }
            let provider = match env.make_provider(registry) {
                Ok(p) => p,
                Err(e) => {
                    model.push_line(format!("[error] {e}"), LineStyle::Error);
                    return Ok(true);
                }
            };
            model.push_line(format!("❯ {text}"), LineStyle::Accent);
            // 持久化 chat 用户消息:否则 /resume 或重开会话后对话记录就丢了(被"隐藏")。
            if let Ok(store) = env.open_store() {
                let _ = store.append_message(
                    &model.session_id,
                    rc_state::MessageRole::User,
                    text,
                );
            }
            model.done_at = None;
            chat_history.push(rc_pro::canonical::CanonicalMessage::user(text.clone()));
            let history = chat_history.clone();
            let tx = event_tx.clone();
            *chat_handle = Some(spawn_chat(provider, history, tx));
        }
    }
    Ok(true)
}

/// 构造一次无工具对话请求(纯函数,便于测试)。
fn chat_request(
    provider_id: &str,
    history: Vec<rc_pro::canonical::CanonicalMessage>,
) -> rc_pro::canonical::CanonicalRequest {
    rc_pro::canonical::CanonicalRequest {
        model: provider_id.to_string(),
        messages: history,
        tools: vec![],
        temperature: None,
        max_tokens: None,
        stream: true,
        extra: json!({}),
    }
}

/// 启动一次无工具对话:入参 history 已含最新用户消息,流式消费 provider 增量,
/// 收尾发 ChatDone。
fn spawn_chat(
    provider: std::sync::Arc<dyn rc_pro::Provider>,
    history: Vec<rc_pro::canonical::CanonicalMessage>,
    tx: tokio::sync::mpsc::UnboundedSender<LoopEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let request = chat_request(provider.id(), history);
        let mut assistant = String::new();
        match provider.stream(request).await {
            Ok(mut stream) => {
                while let Some(ev) = stream.next().await {
                    match ev {
                        Ok(rc_pro::ProvEvent::Delta { text }) => {
                            assistant.push_str(&text);
                            if tx.send(LoopEvent::ChatDelta(text)).is_err() {
                                return;
                            }
                        }
                        Ok(rc_pro::ProvEvent::Finish { .. }) => break,
                        Ok(rc_pro::ProvEvent::Error { message }) => {
                            let _ = tx.send(LoopEvent::ChatDelta(format!("\n[error] {message}")));
                            break;
                        }
                        _ => {}
                    }
                }
            }
            Err(e) => {
                let _ = tx.send(LoopEvent::ChatDelta(format!("\n[error] {e}")));
            }
        }
        let _ = tx.send(LoopEvent::ChatDone { reply: assistant });
    })
}

fn push_lines(model: &mut ReplModel, lines: Vec<String>) {
    for line in lines {
        model.push_line(line, LineStyle::Plain);
    }
}

fn status_lines(model: &ReplModel) -> Vec<String> {
    let ctx = &model.context;
    let mut out = vec![
        format!("session {}", model.session_id),
        format!("model {}", model.model),
        format!("context {}/{} ({}%)", ctx.used, ctx.limit, ctx.pct),
        format!("turns {}", model.turn_count),
        format!("phase {:?}", model.phase),
        format!("risk {}", model.risk_label()),
    ];
    if model.agents.is_empty() {
        out.push("agents: (none)".into());
    } else {
        for (id, a) in &model.agents {
            let mark = if a.done { "✓" } else if a.failed { "✗" } else { "✻" };
            out.push(format!(
                "{mark} agent {id} ({}) [{}] {} · {} tok",
                a.role, a.model, a.phase, a.tokens
            ));
        }
    }
    out
}

fn format_models(registry: &rc_profile::model::Registry) -> Vec<String> {
    let active = registry.active().map(|p| p.id.clone()).unwrap_or_default();
    if registry.profiles.is_empty() {
        return vec!["暂无模型。用 CLI 配置:raincode model add <id> --provider openai --api-key ...".into()];
    }
    registry
        .profiles
        .iter()
        .map(|p| {
            let mark = if p.id == active { "✻" } else { "○" };
            format!("{mark} {:<24} {} ({})", p.id, p.name, p.model)
        })
        .collect()
}

fn use_model(env: &dyn ReplEnv, registry: &mut rc_profile::model::Registry, id: &str) -> Result<Vec<String>, String> {
    if registry.get(id).is_none() {
        return Err(format!("model '{id}' 不存在(见 /models)"));
    }
    registry
        .set_active(id)
        .map_err(|e| e.to_string())?;
    env.save_registry(registry).map_err(|e| e.to_string())?;
    Ok(vec![format!("✓ 默认模型切换到 {id}(下次新任务生效)")])
}

/// 读 store 最近会话 → SessionEntry(/resume 选择器条目)。
fn session_entries(store: &rc_state::Store) -> Result<Vec<SessionEntry>, String> {
    let sessions = store.list_sessions(50).map_err(|e| e.to_string())?;
    Ok(sessions
        .into_iter()
        .map(|s| SessionEntry {
            short_id: s.id.chars().take(8).collect(),
            id: s.id,
            summary: s.summary,
            updated_at: s.updated_at,
        })
        .collect())
}

/// 打开会话选择器(/resume 无参、/sessions、NL "继续上次"):读 store → 条目 → 开选择器。
fn start_session_picker(model: &mut ReplModel, env: &dyn ReplEnv) {
    let store = match env.open_store() {
        Ok(s) => s,
        Err(e) => {
            model.push_line(format!("[error] {e}"), LineStyle::Error);
            return;
        }
    };
    match session_entries(&store) {
        Ok(entries) if entries.is_empty() => {
            model.push_line("暂无历史会话".into(), LineStyle::Dim);
        }
        Ok(entries) => {
            model.push_line(
                "✻ 会话选择器:输入过滤 · ↑↓ 选择 · Enter 确定 · Esc 退出".into(),
                LineStyle::Dim,
            );
            model.open_session_picker(entries);
        }
        Err(e) => model.push_line(format!("[error] {e}"), LineStyle::Error),
    }
}

/// 自然语言恢复会话短意图:开头 "继续上次"/"恢复会话"/"resume"(大小写不敏感)。
fn try_nl_resume(text: &str) -> bool {
    let t = text.trim();
    t.starts_with("继续上次")
        || t.starts_with("恢复会话")
        || t.eq_ignore_ascii_case("resume")
        || t.to_lowercase().starts_with("resume ")
}

/// 自然语言命中 /compact(压缩上下文):锚定意图短语,不误吞真实任务里的裸词
/// "压缩"/"compact"(如 "帮我压缩一下这个图片" / "compact the code now")。
/// 命中三类:含 "压缩上下文" / "压缩一下上下文"(中文意图);英文整句 "compact"
/// 或以 "compact the context" 开头的意图。/compact 斜杠命令是精确路径。
fn nl_compact_trigger(text: &str) -> bool {
    let t = text.trim();
    let lower = t.to_lowercase();
    lower.contains("压缩上下文")
        || lower.contains("压缩一下上下文")
        || lower.eq_ignore_ascii_case("compact")
        || lower.starts_with("compact the context")
}

fn format_skills(skill_store: &SkillStore) -> Vec<String> {
    let skills = skill_store.discover();
    if skills.is_empty() {
        return vec!["暂无技能。CLI 可管理:raincode skills list".into()];
    }
    skills
        .iter()
        .map(|s| format!("✻ {} · {}", s.name, truncate_line(&s.description, 60)))
        .collect()
}

/// skill 网络学习状态(来自 state.db skills 表):使用次数/成功率/置信度/最近使用。
/// 直接回答"skill 网络是否在正常学习"。
fn format_skill_learning(skills: &[rc_state::SkillRow]) -> Vec<String> {
    let mut out = vec![String::from("— 学习状态 —")];
    if skills.is_empty() {
        out.push("  暂无技能使用记录(还没有 skill 被加载过)。".into());
        return out;
    }
    let used = skills.iter().filter(|s| s.usage_count > 0).count();
    for s in skills {
        let rate = if s.usage_count > 0 {
            (s.success_count as f64 / s.usage_count as f64 * 100.0).round() as u64
        } else {
            0
        };
        let scope = if s.scope == "system" { "" } else { &s.scope };
        let last = s
            .last_used
            .as_deref()
            .map(|t| format!(" · {t}"))
            .unwrap_or_default();
        if s.usage_count == 0 {
            out.push(format!(
                "  ⚠ {} · 未使用(0 次){last}{}",
                s.name, scope
            ));
        } else {
            out.push(format!(
                "  ✓ {} · 使用 {} 次 · 成功率 {rate}% · conf {:.2}{last}{}",
                s.name, s.usage_count, s.confidence, scope
            ));
        }
    }
    out.push(format!(
        "  共 {} 个 skill,{used} 个被使用过(未使用 = 模型还没主动加载过)。",
        skills.len()
    ));
    out
}

/// 模型配置向导状态。每步通过 pending Question 提示,答案经 std 通道回传。
#[derive(Default)]
struct Setup {
    step: SetupStep,
    entry: Option<rc_profile::ProviderCatalogEntry>,
    model: Option<String>,
    /// 自定义供应商的 base_url(entry.base_url 为空时,向导里提示输入)。
    base_url: Option<String>,
}

#[derive(Default, PartialEq)]
enum SetupStep {
    #[default]
    Provider,
    /// 自定义(OpenAI 兼容)条目:先问 base_url,再问模型。
    BaseUrl,
    Model,
    Key,
}

/// 把供应商列表/模型列表打进输出区(向导的可选项),并在输入行弹提问。
fn wizard_list(model: &mut ReplModel, title: &str, items: &[String]) {
    model.push_line(format!("— {title} —"), LineStyle::Warn);
    for (i, item) in items.iter().enumerate() {
        model.push_line(format!("  [{}] {item}", i + 1), LineStyle::Dim);
    }
    // 启用 ↑↓ 选择器:高亮移动 + Enter 确认(编号仍可直接输入+Enter)。
    if !items.is_empty() {
        model.setup_picker = Some(crate::repl::model::SetupPicker {
            items: items.to_vec(),
            selected: 0,
        });
    }
}

fn wizard_ask(
    model: &mut ReplModel,
    text: String,
    secret: bool,
    rx_holder: &mut Option<std::sync::mpsc::Receiver<String>>,
) {
    let (tx, rx) = std::sync::mpsc::channel();
    if secret {
        model.set_pending_secret(text, tx);
    } else {
        model.set_pending_question(text, tx);
    }
    *rx_holder = Some(rx);
}

/// 启动向导:列出供应商,弹第一步提问。
fn start_setup(
    model: &mut ReplModel,
    setup: &mut Setup,
    rx_holder: &mut Option<std::sync::mpsc::Receiver<String>>,
) {
    *setup = Setup::default();
    let names: Vec<String> = rc_profile::catalog::catalog()
        .iter()
        .map(|e| e.display_name.to_string())
        .collect();
    wizard_list(model, "选择模型供应商", &names);
    wizard_ask(model, "输入供应商编号 [1-12]:".into(), false, rx_holder);
}

/// 用自然语言识别供应商:文本里包含目录里某供应商的 id 或 display_name。
/// 如 "配置 kimi 的模型" → kimi;"加个 opencode 的 ds" → opencode。
fn find_provider_in_text(text: &str) -> Option<rc_profile::ProviderCatalogEntry> {
    let t = text.to_lowercase();
    rc_profile::catalog::catalog()
        .into_iter()
        .find(|e| {
            t.contains(&e.id.to_lowercase())
                || t.contains(&e.display_name.to_lowercase())
        })
}

/// 预设供应商进入向导(Model 步起),用于 `/configure <自然语言>`。
fn start_setup_with_entry(
    model: &mut ReplModel,
    setup: &mut Setup,
    rx_holder: &mut Option<std::sync::mpsc::Receiver<String>>,
    entry: rc_profile::ProviderCatalogEntry,
) {
    *setup = Setup::default();
    setup.entry = Some(entry.clone());
    setup.step = SetupStep::Model;
    let items: Vec<String> = entry.models.iter().map(|m| m.to_string()).collect();
    wizard_list(model, &format!("{} 模型", entry.display_name), &items);
    wizard_ask(
        model,
        format!("{} 选模型编号或输入自定义 id:", entry.display_name),
        false,
        rx_holder,
    );
}

/// 处理向导的一步答案。返回 true = 向导结束。
/// 处理向导的一步答案。返回 true = 向导结束。
async fn wizard_advance(
    setup: &mut Setup,
    model: &mut ReplModel,
    registry: &mut rc_profile::model::Registry,
    rx_holder: &mut Option<std::sync::mpsc::Receiver<String>>,
    answer: String,
    env: &dyn ReplEnv,
) -> Result<bool> {
    // 中途中止:q/quit/exit 干净退出向导(首次使用无 key 也能退出)。
    let t = answer.trim().to_lowercase();
    if t == "q" || t == "quit" || t == "exit" {
        model.push_line("已取消配置".into(), LineStyle::Dim);
        return Ok(true);
    }
    match setup.step {
        SetupStep::Provider => {
            match wizard_parse_provider(&answer) {
                Some(entry) => {
                    setup.entry = Some(entry.clone());
                    if entry.base_url.is_empty() {
                        // 自定义(OpenAI 兼容):先问 base_url。
                        setup.step = SetupStep::BaseUrl;
                        wizard_ask(
                            model,
                            "自定义:输入 base_url(如 https://api.example.com/v1,不含 /chat/completions):".into(),
                            false,
                            rx_holder,
                        );
                    } else {
                        setup.step = SetupStep::Model;
                        let items: Vec<String> = entry.models.iter().map(|m| m.to_string()).collect();
                        wizard_list(model, &format!("{} 模型", entry.display_name), &items);
                        wizard_ask(
                            model,
                            format!("{} 选模型编号或输入自定义 id:", entry.display_name),
                            false,
                            rx_holder,
                        );
                    }
                }
                None => wizard_ask(model, "无效编号,重新输入供应商编号 [1-12]:".into(), false, rx_holder),
            }
        }
        SetupStep::BaseUrl => {
            let base_url = answer.trim().trim_end_matches('/').to_string();
            if base_url.is_empty() {
                wizard_ask(model, "base_url 不能为空,重新输入:".into(), false, rx_holder);
            } else {
                setup.base_url = Some(base_url);
                setup.step = SetupStep::Model;
                wizard_ask(
                    model,
                    "输入模型 id(如 deepseek-v4-flash;输入任意值即可):".into(),
                    false,
                    rx_holder,
                );
            }
        }
        SetupStep::Model => {
            let entry = setup.entry.clone().ok_or_else(|| anyhow!("wizard: no provider"))?;
            match wizard_parse_model(&entry, &answer) {
                Some(model_id) => {
                    setup.model = Some(model_id.clone());
                    if entry.kind == rc_profile::model::ProfileKind::Ollama {
                        save_wizard_profile(env, setup, None, registry, model, rx_holder).await?;
                        return Ok(true);
                    }
                    setup.step = SetupStep::Key;
                    wizard_ask(
                        model,
                        format!("{model_id} 输入 API key(掩码,回车确认);直接回车 = 用环境变量:"),
                        true,
                        rx_holder,
                    );
                }
                None => wizard_ask(model, "无效模型,重新输入编号或自定义 id:".into(), false, rx_holder),
            }
        }
        SetupStep::Key => {
            let key = if answer.trim().is_empty() || answer.trim() == "e" || answer.trim() == "f" {
                None // 空/e=环境变量/f=key 文件:profile 已带 env_var 引用
            } else {
                Some(answer.trim().to_string())
            };
            save_wizard_profile(env, setup, key, registry, model, rx_holder).await?;
            return Ok(true);
        }
    }
    Ok(false)
}

async fn save_wizard_profile(
    env: &dyn ReplEnv,
    setup: &mut Setup,
    key: Option<String>,
    registry: &mut rc_profile::model::Registry,
    model: &mut ReplModel,
    rx_holder: &mut Option<std::sync::mpsc::Receiver<String>>,
) -> Result<()> {
    let entry = setup.entry.as_ref().ok_or_else(|| anyhow!("wizard: no provider"))?;
    let model_id = setup.model.as_deref().unwrap_or(entry.default_model);
    // id 必须带模型:同供应商多个模型各自独立,不覆盖;也避免与同名模型的
    // 其他供应商混淆(如 opencode-go 的 deepseek-v4-flash ≠ DeepSeek 官方的)。
    let id = format!("{}-{}", entry.id, model_id);
    let mut profile = rc_profile::model::Profile {
        id: id.clone(),
        name: format!("{} / {}", entry.display_name, model_id),
        app: "raincode".into(),
        kind: entry.kind,
        base_url: setup
            .base_url
            .clone()
            .unwrap_or_else(|| entry.base_url.to_string()),
        model: model_id.to_string(),
        api_key: None,
        api_key_env: entry.env_var.map(str::to_string),
        api_key_file: None,
        embedding_model: entry.embedding_model.map(str::to_string),
        headers: Default::default(),
        extra: json!({}),
    };
    // 连通性自检:非本地供应商、且用户提供 key 时,先验证再保存。
    // 本地(ollama/lmstudio/vllm)无 key 或 localhost,跳过;验证失败不保存,提示重试。
    let is_local = profile.kind == rc_profile::model::ProfileKind::Ollama
        || profile.base_url.contains("localhost")
        || profile.base_url.contains("127.0.0.1");
    if !is_local {
        if let Some(key) = &key {
            let mut probe = profile.clone();
            probe.api_key = Some(key.clone());
            match env.verify_connectivity(&probe).await {
                Ok(msg) => model.push_line(msg, LineStyle::Success),
                Err(e) => {
                    model.push_line(
                        format!("[verify] 连接失败,未保存:{e}(重试或回车用环境变量)"),
                        LineStyle::Error,
                    );
                    // 回到 Key 步重试。
                    setup.step = SetupStep::Key;
                    wizard_ask(
                        model,
                        format!("{model_id} 重新输入 API key(掩码,回车确认):"),
                        true,
                        rx_holder,
                    );
                    return Ok(());
                }
            }
        }
    }
    if let Some(key) = key {
        env.store_key(&id, &key)?;
        profile.api_key = None;
        profile.api_key_file = Some(env.key_ref(&id));
    }
    registry.add(profile);
    registry.set_active(&id).ok();
    env.save_registry(registry)?;
    model.model = model_id.to_string();
    model.push_line(format!("✓ 模型已配置:{id} ({model_id})"), LineStyle::Success);
    Ok(())
}

fn wizard_parse_provider(answer: &str) -> Option<rc_profile::ProviderCatalogEntry> {
    let t = answer.trim();
    if let Ok(n) = t.parse::<usize>() {
        if n >= 1 {
            return rc_profile::catalog::catalog().get(n - 1).cloned();
        }
    }
    rc_profile::catalog::find(t)
}

fn wizard_parse_model(entry: &rc_profile::ProviderCatalogEntry, answer: &str) -> Option<String> {
    let t = answer.trim();
    if let Ok(n) = t.parse::<usize>() {
        if n >= 1 {
            return entry.models.get(n - 1).cloned().map(str::to_string);
        }
    }
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

pub(crate) fn help_lines() -> Vec<String> {
    let mut lines: Vec<String> = command::COMMANDS
        .iter()
        .map(|s| format!("/{:<10} {}", s.name, s.desc))
        .collect();
    // 按键说明(帮助新用户发现:子代理树/思考展开/steering 等交互能力)。
    lines.push(String::new());
    lines.push("— 按键 —".into());
    lines.push("  Esc             运行中=中断 · 空闲+空输入=二次回溯上条消息 · 聚焦 agent=退出".into());
    lines.push("  Ctrl+C          中断当前任务(Ctrl+C 再按退出)".into());
    lines.push("  Ctrl+T          折叠/展开子代理任务树".into());
    lines.push("  Tab             有子代理时循环聚焦 · 运行中+有输入=入队".into());
    lines.push("  Ctrl+O          展开/收起完整思维链".into());
    lines.push("  , / . / p       聚焦子代理时:上一/下一/返回父级".into());
    lines.push("  PageUp/Down     滚动对话 · Ctrl+L 清屏 · Ctrl+R 历史搜索".into());
    lines.push("  Enter 提交 · Shift+Enter 换行 · ↑↓ 输入历史".into());
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn help_lists_every_command_and_keybindings() {
        let lines = help_lines();
        assert!(lines.len() >= command::COMMANDS.len());
        assert!(lines.iter().any(|l| l.starts_with("/chat")));
        // 按键说明段存在(子代理树/思维展开等交互能力的发现入口)。
        assert!(lines.iter().any(|l| l.contains("Ctrl+T")));
        assert!(lines.iter().any(|l| l.contains("Ctrl+O")));
        assert!(lines.iter().any(|l| l.contains("Tab")));
    }

    #[test]
    fn smalltalk_matches_greetings_and_acknowledgements() {
        assert!(is_smalltalk("你好"));
        assert!(is_smalltalk("hello"));
        assert!(is_smalltalk("在吗?"));
        assert!(is_smalltalk("好的"));
        assert!(is_smalltalk("  ok  "));
        // 带实际任务内容的输入绝不误判为闲聊。
        assert!(!is_smalltalk("你好,帮我写一个排序函数"));
        assert!(!is_smalltalk("好的,现在测试一下"));
        assert!(!is_smalltalk("hello world 程序"));
        assert!(!is_smalltalk("把代码改成用 python"));
    }

    #[test]
    fn skill_learning_flags_unused_and_shows_stats() {
        let mk = |name: &str, usage: i64, success: i64, confidence: f64| rc_state::SkillRow {
            id: name.into(),
            name: name.into(),
            category: "cat".into(),
            path: "".into(),
            description: "".into(),
            frontmatter: serde_json::json!({}),
            version: 1,
            confidence,
            usage_count: usage,
            success_count: success,
            last_used: None,
            auto: false,
            origin: "manual".into(),
            origin_url: None,
            scope: "user".into(),
            allow_implicit: true,
            relations: serde_json::json!([]),
            embedding: None,
            created_at: "".into(),
            updated_at: "".into(),
        };
        let lines = format_skill_learning(&[
            mk("used-skill", 4, 3, 0.9),
            mk("idle-skill", 0, 0, 0.8),
        ]);
        let joined = lines.join("\n");
        assert!(joined.contains("used-skill · 使用 4 次 · 成功率 75%"));
        assert!(joined.contains("⚠ idle-skill · 未使用"));
        assert!(joined.contains("1 个被使用过"));
    }

    #[test]
    fn key_enter_submits_bare_line_as_run() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.input.insert_char('h');
        m.input.insert_char('i');
        let action = handle_key(&mut m, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(action, Action::Cmd(Cmd::Run(_))));
        assert!(m.input.text.is_empty());
    }

    #[test]
    fn ctrl_c_interrupts_when_running() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.apply_event(AgentEvent::AgentSpawned {
            id: "a1".into(),
            model: "gpt-5".into(),
            role: "coder".into(),
            task: "build".into(),
        });
        let action =
            handle_key(&mut m, KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(matches!(action, Action::Interrupt));
    }

    #[test]
    fn pending_enter_resolves() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        let (tx, rx) = std::sync::mpsc::channel();
        m.set_pending_approval(
            ApprovalRequest {
                tool: "run_shell".into(),
                description: "rm -rf /".into(),
                args: json!({}),
            },
            tx,
        );
        m.input.insert_char('y');
        let action = handle_key(&mut m, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(action, Action::None));
        assert!(m.pending.is_none());
        assert!(matches!(rx.recv(), Ok(ApprovalDecision::Allow)));
    }

    #[test]
    fn pending_secret_enter_does_not_enter_history() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        let (tx, rx) = std::sync::mpsc::channel();
        m.set_pending_secret("sk-test".into(), tx);
        for ch in "sk-secret-123".chars() {
            m.input.insert_char(ch);
        }
        let action = handle_key(&mut m, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(action, Action::None));
        assert!(m.pending.is_none());
        assert_eq!(rx.recv(), Ok("sk-secret-123".into()));
        assert!(m.input.text.is_empty());
        assert!(m.input.history.is_empty());
    }

    #[tokio::test]
    async fn wizard_advance_accepts_answer_and_can_cancel() {
        let mut setup = Setup::default();
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        let mut registry = rc_profile::model::Registry::default();
        let mut rx_holder: Option<std::sync::mpsc::Receiver<String>> = None;
        // Provider step:直接给合法编号,answer 来自入参而非通道。
        // 需要 env 提供 save_registry/store_key。用空实现 env(不真正保存)。
        struct EmptyEnv;
        #[async_trait::async_trait(?Send)]
        impl ReplEnv for EmptyEnv {
            fn load_registry(&self) -> anyhow::Result<rc_profile::model::Registry> { Ok(Default::default()) }
            fn save_registry(&self, _r: &rc_profile::model::Registry) -> anyhow::Result<()> { Ok(()) }
            fn home_dir(&self) -> std::path::PathBuf { std::path::PathBuf::new() }
            fn skills_dir(&self) -> std::path::PathBuf { std::path::PathBuf::new() }
            fn workspace(&self) -> std::path::PathBuf { std::path::PathBuf::from(".") }
            fn create_session(&self) -> anyhow::Result<String> { Ok("s".into()) }
            fn open_store(&self) -> anyhow::Result<rc_state::Store> {
                rc_state::Store::open_in_memory().map_err(anyhow::Error::from)
            }
            fn make_provider(&self, _r: &rc_profile::model::Registry) -> anyhow::Result<crate::repl::env::BoxProvider> {
                anyhow::bail!("no provider")
            }
            fn dispatch_slash(&self, _n: &str, _a: &serde_json::Value) -> Result<String, String> { Ok("".into()) }
            fn skill_nav(&self, _t: &str) -> Result<Vec<String>, String> { Ok(vec![]) }
            fn store_key(&self, _i: &str, _k: &str) -> anyhow::Result<()> { Ok(()) }
            fn key_ref(&self, _i: &str) -> String { String::new() }
            async fn verify_connectivity(&self, _p: &rc_profile::model::Profile) -> anyhow::Result<String> {
                Ok("ok".into())
            }
            async fn agent_config(&self, _r: &rc_profile::model::Registry, _w: bool) -> anyhow::Result<rc_core::AgentConfig> {
                anyhow::bail!("no agent")
            }
            fn context_window(&self, _r: &rc_profile::model::Registry) -> u64 { 128_000 }
            async fn refresh_profiles(&self) -> anyhow::Result<String> { Ok("ok".into()) }
            fn model_picker_entries(&self) -> anyhow::Result<Vec<crate::repl::env::ModelPickerEntry>> {
                Ok(Vec::new())
            }
            fn route_run(
                &self,
                _prompt: String,
                _plan_only: bool,
                _emit: std::sync::Arc<dyn Fn(rc_proto::AgentEvent) + Send + Sync>,
                _steer_hub: std::sync::Arc<rc_core::SteerHub>,
                _cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
                _risk_mode: rc_router::risk::RiskMode,
                _subagent_approval: std::sync::Arc<dyn rc_sandbox::ApprovalHook>,
                _supervisor: Option<std::sync::Arc<rc_core::Supervisor>>,
                _feed: AgentFeed,
            ) {
            }
            fn supervise_start(
                &self,
                _r: &rc_profile::model::Registry,
                _m: Option<&str>,
            ) -> Result<std::sync::Arc<rc_core::Supervisor>, String> {
                Ok(std::sync::Arc::new(rc_core::Supervisor {
                    provider: Box::new(StubCompactProvider),
                    cfg: rc_sandbox::guard::SuperviseConfig::default(),
                    boundaries: String::new(),
                }))
            }
            fn supervise_config_path(&self) -> std::path::PathBuf {
                std::path::PathBuf::new()
            }
        }
        let env = EmptyEnv;
        // Provider step:直接给合法编号,answer 来自入参而非通道。
        let done = wizard_advance(&mut setup, &mut m, &mut registry, &mut rx_holder, "1".into(), &env)
            .await
            .unwrap();
        assert!(!done);
        assert!(matches!(setup.step, SetupStep::Model));
        // 取消:q 结束向导。
        let done = wizard_advance(&mut setup, &mut m, &mut registry, &mut rx_holder, "q".into(), &env)
            .await
            .unwrap();
        assert!(done);
        assert!(m.output.back().unwrap().text.contains("已取消配置"));
    }

    #[test]
    fn pending_approval_ctrl_c_cancels() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        let (tx, rx) = std::sync::mpsc::channel();
        m.set_pending_approval(
            ApprovalRequest {
                tool: "run_shell".into(),
                description: "rm -rf /".into(),
                args: json!({}),
            },
            tx,
        );
        let action =
            handle_key(&mut m, KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(matches!(action, Action::None));
        assert!(m.pending.is_none());
        assert!(matches!(rx.recv(), Ok(ApprovalDecision::Deny { .. })));
    }

    #[test]
    fn pending_single_key_y_resolves_approval_without_enter() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        let (tx, rx) = std::sync::mpsc::channel();
        m.set_pending_approval(
            ApprovalRequest {
                tool: "run_shell".into(),
                description: "rm -rf /".into(),
                args: json!({}),
            },
            tx,
        );
        // 单键 'y' → Allow,不进输入栏。
        let action = handle_key(&mut m, KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert!(matches!(action, Action::None));
        assert!(m.pending.is_none());
        assert!(matches!(rx.recv(), Ok(ApprovalDecision::Allow)));
        // 历史里记录 ✓ 已允许行。
        assert!(m.output.iter().any(|l| l.text.contains("✓ 已允许")));
    }

    #[test]
    fn pending_guard_single_key_3_forever() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        let (tx, rx) = std::sync::mpsc::channel();
        m.set_pending_guard(
            GuardRequest {
                tool: "run_shell".into(),
                reason: "high risk".into(),
                command: None,
                path: None,
            },
            tx,
        );
        let action = handle_key(&mut m, KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE));
        assert!(matches!(action, Action::None));
        assert!(m.pending.is_none());
        assert!(matches!(rx.recv(), Ok(GuardConsent::Forever)));
        assert!(m.output.iter().any(|l| l.text.contains("✓ 已允许(永久)")));
    }

    #[test]
    fn pending_question_char_edits_input_bar() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        let (tx, _rx) = std::sync::mpsc::channel();
        m.set_pending_question("age?".into(), tx);
        // Question 不消费单键:字符进输入栏,pending 保持(等 Enter 提交)。
        let action = handle_key(&mut m, KeyEvent::new(KeyCode::Char('4'), KeyModifiers::NONE));
        assert!(matches!(action, Action::None));
        assert!(m.pending.is_some());
        assert_eq!(m.input.text, "4");
    }

    #[test]
    fn tab_completes_unique_slash() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        for ch in "/cha".chars() {
            m.input.insert_char(ch);
        }
        complete_slash(&mut m);
        assert!(m.input.text.starts_with("/chat"));
    }

    #[test]
    fn enter_accepts_unique_slash_from_menu() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        for ch in "/cha".chars() {
            m.input.insert_char(ch);
        }
        // 菜单开启:唯一候选 chat,Enter 填入 `/chat ` 并收起菜单。
        let action = handle_key(&mut m, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(action, Action::None));
        assert!(m.input.text.starts_with("/chat "));
        assert!(m.slash_menu.is_none()); // 填了空格 → 菜单收起
    }

    #[test]
    fn session_picker_keys_navigate_and_enter_selects() {
        use crate::repl::model::SessionEntry;
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.open_session_picker(vec![
            SessionEntry { id: "1111aaaa".into(), short_id: "1111aaaa".into(), summary: "build api".into(), updated_at: "t".into() },
            SessionEntry { id: "2222bbbb".into(), short_id: "2222bbbb".into(), summary: "fix tests".into(), updated_at: "u".into() },
        ]);
        assert!(m.session_picker.is_some());
        // 键入过滤。
        let action = handle_key(&mut m, KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE));
        assert!(matches!(action, Action::None));
        assert_eq!(m.session_picker.as_ref().unwrap().filtered.len(), 1);
        // Enter → SessionPick。
        let action = handle_key(&mut m, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(action, Action::SessionPick));
        // Esc 关闭。
        m.open_session_picker(vec![SessionEntry {
            id: "1111aaaa".into(),
            short_id: "1111aaaa".into(),
            summary: "build api".into(),
            updated_at: "t".into(),
        }]);
        let action = handle_key(&mut m, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(action, Action::None));
        assert!(m.session_picker.is_none());
    }

    #[test]
    fn session_picker_nl_trigger_recognizes_resume() {
        assert!(try_nl_resume("继续上次"));
        assert!(try_nl_resume("恢复会话 abc"));
        assert!(try_nl_resume("resume"));
        assert!(try_nl_resume("Resume the api work"));
        assert!(!try_nl_resume("修复登录"));
        assert!(!try_nl_resume(""));
    }

    #[test]
    fn slash_menu_keys_move_selection_and_esc_closes() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        for ch in "/st".chars() {
            m.input.insert_char(ch);
        }
        // ↓ 下移选择。
        let action = handle_key(&mut m, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert!(matches!(action, Action::None));
        assert_eq!(m.slash_menu.as_ref().unwrap().selected, 1);
        // ↑ 回退。
        let action = handle_key(&mut m, KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert!(matches!(action, Action::None));
        assert_eq!(m.slash_menu.as_ref().unwrap().selected, 0);
        // Esc 只关菜单,不动 focus。
        m.focus_agent = Some("a1".into());
        let action = handle_key(&mut m, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(action, Action::None));
        assert!(m.slash_menu.is_none());
        assert!(m.focus_agent.is_some()); // 菜单 Esc 不波及 agent 焦点
    }

    #[test]
    fn ctrl_r_searches_history() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.input.history = vec!["cargo test".into(), "cargo build".into()];
        for ch in "cargo".chars() {
            m.input.insert_char(ch);
        }
        let action = handle_key(
            &mut m,
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL),
        );
        assert!(matches!(action, Action::None));
        assert_eq!(m.input.text, "cargo build");
    }

    #[test]
    fn tab_cycles_agents_when_present() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.apply_event(AgentEvent::AgentSpawned {
            id: "a1".into(),
            model: "m".into(),
            role: "r".into(),
            task: "t".into(),
        });
        let action = handle_key(&mut m, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert!(matches!(action, Action::FocusNext));
        // 有 / 前缀输入时进入斜杠菜单:Tab 在菜单内循环(不再返回 Complete)。
        for ch in "/st".chars() {
            m.input.insert_char(ch);
        }
        let action = handle_key(&mut m, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert!(matches!(action, Action::None));
        assert!(m.slash_menu.is_some());
        assert_eq!(m.slash_menu.as_ref().unwrap().selected, 1); // stop → status
    }

    #[test]
    fn esc_clears_focus() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.focus_agent = Some("a1".into());
        let action = handle_key(&mut m, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(action, Action::None));
        assert!(m.focus_agent.is_none());
    }

    #[test]
    fn tab_queues_input_while_running() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.apply_event(AgentEvent::AgentSpawned {
            id: "a1".into(),
            model: "gpt-5".into(),
            role: "coder".into(),
            task: "build".into(),
        });
        assert!(m.agent_turn_running());
        for ch in "steer me".chars() {
            m.input.insert_char(ch);
        }
        let action = handle_key(&mut m, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert!(matches!(action, Action::None));
        assert_eq!(m.queued_input.len(), 1);
        assert_eq!(m.queued_input.front().map(String::as_str), Some("steer me"));
        assert!(m.input.text.is_empty(), "queued input clears the input bar");
        assert!(m.output.iter().any(|l| l.text.contains("[queued] steer me")));
    }

    #[test]
    fn esc_while_running_returns_interrupt() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.start_run();
        let action = handle_key(&mut m, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(action, Action::Interrupt));
    }

    #[test]
    fn esc_twice_idle_empty_backtracks_to_last_user_message() {
        // 实时路径:用户行带 `› ` 显示前缀存储。
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.push_user_line("› first task".into());
        // 第一次 Esc:布防 + 提示。
        let action = handle_key(&mut m, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(action, Action::None));
        assert!(m.backtrack_armed);
        assert!(m.output.iter().any(|l| l.text.contains("Esc again")));
        // 第二次 Esc:恢复上一条用户消息到输入框(剥离 `› ` 前缀)。
        let action = handle_key(&mut m, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(action, Action::None));
        assert!(!m.backtrack_armed);
        assert_eq!(m.input.text, "first task");
        assert_eq!(m.input.cursor, m.input.text.len());
    }

    #[test]
    fn backtrack_restores_unprefixed_resume_path_lines() {
        // resume 路径:用户行不带 `› ` 前缀,同样应干净恢复。
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.push_user_line("first task".into());
        handle_key(&mut m, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        let action = handle_key(&mut m, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(action, Action::None));
        assert_eq!(m.input.text, "first task");
        assert_eq!(m.input.cursor, m.input.text.len());
    }

    #[test]
    fn any_non_esc_key_resets_backtrack() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.push_user_line("hi".into());
        handle_key(&mut m, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(m.backtrack_armed);
        // 任意非 Esc 键取消布防(走 `_` 兜底)。
        handle_key(&mut m, KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(!m.backtrack_armed);
    }

    #[test]
    fn esc_with_focus_does_not_arm_backtrack() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.focus_agent = Some("a1".into());
        let action = handle_key(&mut m, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(action, Action::None));
        assert!(m.focus_agent.is_none());
        assert!(!m.backtrack_armed, "focus-clearing Esc must not arm backtrack");
    }

    #[test]
    fn page_up_page_down_scroll_conversation() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        for i in 0..10 {
            m.push_line(format!("l{i}"), LineStyle::Plain);
        }
        // PageUp:上滚 5 行,解锁自动滚动。
        let action = handle_key(&mut m, KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
        assert!(matches!(action, Action::None));
        assert_eq!(m.scroll_offset, 5);
        assert!(!m.autoscroll, "page up unlocks autoscroll");
        // PageDown:下滚 5 行,回到底部重新贴底。
        let action = handle_key(&mut m, KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
        assert!(matches!(action, Action::None));
        assert_eq!(m.scroll_offset, 0);
        assert!(m.autoscroll, "page down back to bottom re-pins");
    }

    #[test]
    fn scroll_works_while_pending_prompt_active() {
        // setup 向导/审批等 pending 状态下,PageUp/PageDown 也必须能滚动历史
        // (否则 pending 分支会把它们当编辑键吞掉 → 滚轮失效)。
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        for i in 0..10 {
            m.push_line(format!("l{i}"), LineStyle::Plain);
        }
        let (tx, _rx) = std::sync::mpsc::channel();
        m.set_pending_approval(
            ApprovalRequest {
                tool: "run_shell".into(),
                description: "d".into(),
                args: serde_json::json!({}),
            },
            tx,
        );
        assert!(m.pending.is_some());
        // pending 状态下 PageUp 仍滚动(而不是被 pending 分支吞掉)。
        let action = handle_key(&mut m, KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
        assert!(matches!(action, Action::None));
        assert_eq!(m.scroll_offset, 5, "scroll must work even while pending");
        // pending 未被误消费。
        assert!(m.pending.is_some());
    }

    #[test]
    fn agent_nav_keys_move_focus_when_focused() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.apply_event(AgentEvent::AgentSpawned {
            id: "a1".into(),
            model: "m".into(),
            role: "r".into(),
            task: "t".into(),
        });
        m.apply_event(AgentEvent::AgentSpawned {
            id: "a2".into(),
            model: "m".into(),
            role: "r".into(),
            task: "t".into(),
        });
        m.focus_agent = Some("a1".into());
        // . = next。
        handle_key(&mut m, KeyEvent::new(KeyCode::Char('.'), KeyModifiers::NONE));
        assert_eq!(m.focus_agent.as_deref(), Some("a2"));
        // , = prev。
        handle_key(&mut m, KeyEvent::new(KeyCode::Char(','), KeyModifiers::NONE));
        assert_eq!(m.focus_agent.as_deref(), Some("a1"));
        // p = parent(平铺无父 → 清 focus 回根)。
        handle_key(&mut m, KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE));
        assert!(m.focus_agent.is_none());
        // 无 focus 时 , 正常输入(不吞键)。
        handle_key(&mut m, KeyEvent::new(KeyCode::Char(','), KeyModifiers::NONE));
        assert_eq!(m.input.text, ",");
    }

    #[test]
    fn nav_keys_freed_for_steer_typing_when_focused() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.apply_event(AgentEvent::AgentSpawned {
            id: "a1".into(),
            model: "m".into(),
            role: "r".into(),
            task: "t".into(),
        });
        m.apply_event(AgentEvent::AgentSpawned {
            id: "a2".into(),
            model: "m".into(),
            role: "r".into(),
            task: "t".into(),
        });
        m.focus_agent = Some("a1".into());
        m.input.text = "use . for".into();
        m.input.cursor = m.input.text.len();
        // 聚焦 + 输入非空:导航键让位给输入(文档化 steer 流程:聚焦时输入 + Enter)。
        handle_key(&mut m, KeyEvent::new(KeyCode::Char('.'), KeyModifiers::NONE));
        assert_eq!(
            m.focus_agent.as_deref(),
            Some("a1"),
            "navigation must not fire while typing a steer"
        );
        assert_eq!(m.input.text, "use . for.");
        // ',' 与 'p' 同样自由输入,不切 agent/不清 focus。
        handle_key(&mut m, KeyEvent::new(KeyCode::Char(','), KeyModifiers::NONE));
        assert_eq!(m.focus_agent.as_deref(), Some("a1"));
        assert_eq!(m.input.text, "use . for.,");
        handle_key(&mut m, KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE));
        assert_eq!(m.focus_agent.as_deref(), Some("a1"), "p must not clear focus while typing");
        assert_eq!(m.input.text, "use . for.,p");
        // 清空输入后导航恢复。
        m.input.text.clear();
        m.input.cursor = 0;
        handle_key(&mut m, KeyEvent::new(KeyCode::Char('.'), KeyModifiers::NONE));
        assert_eq!(m.focus_agent.as_deref(), Some("a2"));
    }

    #[test]
    fn shift_tab_cycles_risk_mode() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        use rc_router::risk::RiskMode::*;
        assert_eq!(m.risk_mode, Ask);
        // handle_key 只返回动作;真正切换在 main loop 的 Action::CycleRisk 分支。
        let action = handle_key(&mut m, KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert!(matches!(action, Action::CycleRisk));
        // 直接调用 cycle 验证模式翻转 + 标签。
        m.cycle_risk_mode();
        assert_eq!(m.risk_mode, Auto);
        assert_eq!(m.risk_label(), "auto");
        m.cycle_risk_mode();
        assert_eq!(m.risk_mode, Assisted);
        assert_eq!(m.risk_label(), "assisted");
        m.cycle_risk_mode();
        assert_eq!(m.risk_mode, Manual);
        assert_eq!(m.risk_label(), "manual");
    }

    #[test]
    fn approval_hook_respects_risk_mode() {
        use rc_router::risk::RiskMode;
        let (hook_tx, _hook_rx) = tokio::sync::mpsc::unbounded_channel::<HookMsg>();
        let mode = std::sync::Arc::new(std::sync::Mutex::new(RiskMode::Manual));
        let hook = risk_approval_hook(&hook_tx, mode);
        // manual + 安全命令 → Allow(否则 agent 完全无法工作)。
        let decision = futures::executor::block_on(hook.ask(&rc_sandbox::ApprovalRequest {
            tool: "run_shell".into(),
            description: "git status".into(),
            args: serde_json::json!({}),
        }));
        assert!(matches!(decision, rc_sandbox::ApprovalDecision::Allow));
        // manual + 高危命令 → Deny。
        let decision = futures::executor::block_on(hook.ask(&rc_sandbox::ApprovalRequest {
            tool: "run_shell".into(),
            description: "rm -rf /x".into(),
            args: serde_json::json!({}),
        }));
        assert!(matches!(decision, rc_sandbox::ApprovalDecision::Deny { .. }));
        // 非 run_shell 工具(读文件等)在 manual 下也放行。
        let decision = futures::executor::block_on(hook.ask(&rc_sandbox::ApprovalRequest {
            tool: "read_file".into(),
            description: "a.txt".into(),
            args: serde_json::json!({}),
        }));
        assert!(matches!(decision, rc_sandbox::ApprovalDecision::Allow));
    }

    #[test]
    fn repl_guard_hook_bridges_to_main_loop_consent() {
        // 授权闸 hook:ask(req) → 主循环收到 HookMsg::Guard → set_pending_guard 弹窗
        // → 用户答 2=Session → consent 送回 hook.ask 的调用方。
        let (hook_tx, mut hook_rx) = tokio::sync::mpsc::unbounded_channel::<HookMsg>();
        let hook = repl_guard_hook(&hook_tx);
        let handle = std::thread::spawn(move || {
            let req = GuardRequest {
                tool: "run_shell".into(),
                reason: "command matches deny pattern 'rm -rf'".into(),
                command: Some("rm -rf /x".into()),
                path: None,
            };
            futures::executor::block_on(hook.ask(&req))
        });
        let HookMsg::Guard { req, reply } = hook_rx.blocking_recv().unwrap() else {
            panic!("expected HookMsg::Guard");
        };
        assert_eq!(req.tool, "run_shell");
        // 主循环侧:用户选 2=本会话。
        reply.send(GuardConsent::Session).unwrap();
        assert_eq!(handle.join().unwrap(), GuardConsent::Session);
    }

    #[test]
    fn repl_guard_hook_denies_when_channel_closed() {
        let (hook_tx, _hook_rx) = tokio::sync::mpsc::unbounded_channel::<HookMsg>();
        let hook = repl_guard_hook(&hook_tx);
        drop(_hook_rx); // 主循环关闭 → ask 必须保守拒绝,不 panic。
        let consent = futures::executor::block_on(hook.ask(&GuardRequest {
            tool: "run_shell".into(),
            reason: "x".into(),
            command: None,
            path: None,
        }));
        assert_eq!(consent, GuardConsent::Deny);
    }

    #[test]
    fn status_reports_real_state() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.apply_event(AgentEvent::AgentSpawned {
            id: "a1".into(),
            model: "gpt-5".into(),
            role: "coder".into(),
            task: "build".into(),
        });
        let lines = status_lines(&m);
        assert!(lines.iter().any(|l| l.contains("s1")));
        assert!(lines.iter().any(|l| l.contains("a1")));
    }

    #[test]
    fn status_reports_turns() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.push_user_line("task".into());
        let lines = status_lines(&m);
        assert!(lines.iter().any(|l| l.contains("turns 1")));
    }

    #[test]
    fn configure_parses_provider_from_natural_language() {
        // "配置 kimi 的模型" → kimi;识别不出 → None。
        let kimi = find_provider_in_text("帮我配置 kimi 的模型");
        assert!(kimi.is_some_and(|e| e.id == "kimi"));
        assert!(find_provider_in_text("配置 kimi 的模型").is_some());
        assert!(find_provider_in_text("完全无关的话").is_none());
        // 空文本不 panic。
        assert!(find_provider_in_text("").is_none());
    }

    #[test]
    fn models_marks_active() {
        let mut registry = rc_profile::model::Registry::default();
        let p = rc_profile::model::Profile {
            id: "gpt-5".into(),
            name: "GPT-5".into(),
            app: "openai".into(),
            kind: rc_profile::model::ProfileKind::OpenAiCompat,
            base_url: String::new(),
            model: "gpt-5".into(),
            api_key: None,
            api_key_env: None,
            api_key_file: None,
            embedding_model: None,
            headers: Default::default(),
            extra: serde_json::Value::Object(Default::default()),
        };
        registry.profiles.push(p);
        registry.set_active("gpt-5").unwrap();
        let lines = format_models(&registry);
        assert!(lines[0].starts_with("✻"));
    }

    #[test]
    fn chat_request_has_no_tools_and_user_message() {
        let request = chat_request(
            "gpt-5",
            vec![rc_pro::canonical::CanonicalMessage::user("hi")],
        );
        assert!(request.tools.is_empty());
        assert_eq!(request.messages.len(), 1);
        assert_eq!(request.messages[0].text(), "hi");
    }

    #[test]
    fn wizard_parses_provider_by_index_and_id() {
        assert_eq!(wizard_parse_provider("1").map(|e| e.id), Some("openai"));
        assert_eq!(wizard_parse_provider("deepseek").map(|e| e.id), Some("deepseek"));
        assert_eq!(wizard_parse_provider("99"), None);
    }

    #[test]
    fn wizard_parses_model_by_index_or_custom() {
        let openai = rc_profile::catalog::find("openai").unwrap();
        // OpenAI 目录首个模型现为 gpt-5.6-sol(旗舰系列更新后)。
        assert_eq!(wizard_parse_model(&openai, "1"), Some("gpt-5.6-sol".into()));
        assert_eq!(wizard_parse_model(&openai, "my-custom"), Some("my-custom".into()));
        assert_eq!(wizard_parse_model(&openai, ""), None);
    }

    #[test]
    fn compact_nl_trigger_dispatches() {
        // NL 短意图命中:中文 "压缩上下文"/"压缩一下上下文"、英文整句 "compact" 或
        // "compact the context"(意图短语),不误吞真实任务里的裸词。
        assert!(nl_compact_trigger("帮我压缩一下上下文"));
        assert!(nl_compact_trigger("压缩上下文"));
        assert!(nl_compact_trigger("帮我压缩一下上下文"));
        assert!(nl_compact_trigger("compact the context"));
        assert!(nl_compact_trigger("compact"));
        assert!(nl_compact_trigger("Compact the context now"));
        // 纯任务文本不误吞:裸 contains("压缩")/contains("compact") 已换成意图短语,
        // "压缩一下"、"帮我压缩一下这个图片"、"compact the code now" 这类不再命中。
        assert!(!nl_compact_trigger("帮我压缩一下"));
        assert!(!nl_compact_trigger("帮我压缩一下这个图片"));
        assert!(!nl_compact_trigger("帮我压缩文件"));
        assert!(!nl_compact_trigger("write a compact function"));
        assert!(!nl_compact_trigger("compact the code now"));
        assert!(!nl_compact_trigger("帮我写个 api"));
        assert!(!nl_compact_trigger(""));
    }

    /// 最小 Agent(仅 stop_run 测试用;不跑真实 loop)。
    fn test_agent() -> Agent {
        let skill_store = rc_skill::SkillStore::new(std::path::Path::new("."));
        let cfg = rc_core::AgentConfig {
            provider: std::sync::Arc::new(StubCompactProvider),
            plan_provider: None,
            review_provider: None,
            store: rc_state::Store::open_in_memory().unwrap(),
            skill_store: skill_store.clone(),
            tools: rc_tool::builtin::default_tools(skill_store),
            approval: std::sync::Arc::new(rc_sandbox::AutoApproveHook),
            command_policy: rc_sandbox::CommandPolicy::default(),
            network_policy: rc_sandbox::NetworkPolicy::default(),
            cwd: std::path::PathBuf::from("."),
            state_path: std::path::PathBuf::from("."),
            max_turns: 1,
            max_steps: 0,
            evolve_on_finish: false,
            plan_mode: false,
            hooks: rc_core::HooksConfig::default(),
            agent: None,
            max_history_bytes: None,
            mcp_servers: vec![],
            entropy_mode: false,
            plan_max_rounds: 1,
            plan_max_questions: 1,
            review_max_rounds: 1,
            max_cycles: 1,
            user_input: std::sync::Arc::new(rc_sandbox::AutoUserHook::default()),
            steer_rx: None,
            context_window: 0,
            subagent: None,
            guard_cfg: None,
            guard_hook: None,
            guard_memo: None,
            guard_home: None,
        };
        Agent::new(cfg)
    }

    #[test]
    fn stop_sets_route_cancel_flag() {
        let agent = test_agent();
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        m.phase = Phase::Running;
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let run_cancel = Some(cancel.clone());
        stop_run(&mut m, &agent, &run_cancel);
        assert!(
            cancel.load(std::sync::atomic::Ordering::Relaxed),
            "/stop must set the route/autonomous cancel flag"
        );
        assert!(m.output.back().unwrap().text.contains("[stop]"));
    }

    #[test]
    fn stop_when_idle_does_not_touch_run_cancel() {
        let agent = test_agent();
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let run_cancel = Some(cancel.clone());
        stop_run(&mut m, &agent, &run_cancel);
        assert!(!cancel.load(std::sync::atomic::Ordering::Relaxed));
        assert!(m.output.back().unwrap().text.contains("没有运行中的任务"));
    }

    #[test]
    fn error_teardown_clears_thinking_state() {
        let mut thinking = ThinkingFlow {
            plan_running: true,
            prompt: Some("stale plan prompt".into()),
        };
        let mut task_handle: Option<tokio::task::JoinHandle<()>> = None;
        let mut run_cancel =
            Some(std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)));
        let mut sup_batch: Vec<AgentEvent> = Vec::new();
        let mut sup_since: Option<Instant> = Some(Instant::now());
        let mut run_epoch: u64 = 0;
        teardown_run(
            &mut thinking,
            &mut task_handle,
            &mut run_cancel,
            &mut sup_batch,
            &mut sup_since,
            &mut run_epoch,
            &LoopEvent::Agent(AgentEvent::Error {
                message: "boom".into(),
            }),
        );
        // plan-only 失败后 Thinking 状态机必须清空:plan_running 不能残留。
        assert!(!thinking.plan_running, "plan_running must be reset on Error");
        assert!(
            thinking.prompt.is_none(),
            "stale prompt must be cleared on Error"
        );
        assert!(run_cancel.is_none());
        assert_eq!(run_epoch, 1);
    }

    #[test]
    fn done_teardown_keeps_thinking_state_for_confirm() {
        let mut thinking = ThinkingFlow {
            plan_running: true,
            prompt: Some("plan prompt".into()),
        };
        let mut task_handle: Option<tokio::task::JoinHandle<()>> = None;
        let mut run_cancel =
            Some(std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)));
        let mut sup_batch: Vec<AgentEvent> = Vec::new();
        let mut sup_since: Option<Instant> = None;
        let mut run_epoch: u64 = 0;
        teardown_run(
            &mut thinking,
            &mut task_handle,
            &mut run_cancel,
            &mut sup_batch,
            &mut sup_since,
            &mut run_epoch,
            &LoopEvent::Agent(AgentEvent::Done {
                summary: "plan".into(),
                usage: None,
                session_id: "s1".into(),
                reasoning: None,
            }),
        );
        // Done 不清 Thinking:plan_only 成功后由主循环的确认逻辑取走 prompt。
        assert!(thinking.plan_running);
        assert!(thinking.prompt.is_some());
        assert!(run_cancel.is_none());
        assert_eq!(run_epoch, 1);
    }

    /// /compact 集成:成功路径 push "[compact]" 摘要行 + 摘要正文,不 panic。
    #[tokio::test]
    async fn do_compact_success_pushes_summary_line() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("state.db");
        let store = rc_state::Store::open(&db_path).unwrap();
        let s = store.create_session(".").unwrap();
        for i in 0..3 {
            store
                .append_message(&s.id, rc_state::MessageRole::User, &format!("msg {i}"))
                .unwrap();
        }
        let mut m = ReplModel::new(s.id.clone(), "gpt-5".into(), 128_000);
        let env = CompactTestEnv { db_path: db_path.clone(), fail_provider: false };
        let mut registry = rc_profile::model::Registry::default();
        let ok = do_compact(&mut m, &env, &mut registry).await.unwrap();
        assert!(ok);
        let texts: Vec<&str> = m.output.iter().map(|l| l.text.as_str()).collect();
        assert!(texts.iter().any(|t| t.contains("[compact] 摘要已生成")));
        assert!(texts.iter().any(|t| t.contains("SUM")));
        // 摘要被真正落库:历史被改写为 摘要(1) + 最近消息(3) = 4 条。
        // 摘要消息是 <conversation-checkpoint> 格式(锚定摘要,见 rc-core/compact.rs)。
        let store2 = rc_state::Store::open(&db_path).unwrap();
        let after = store2.list_messages(&s.id).unwrap();
        assert_eq!(after.len(), 4);
        assert!(after[0].content.contains("<conversation-checkpoint>"));
        assert!(after[0].content.contains("SUM"));
    }

    /// /compact 失败路径:provider 缺失 → push [error],返回 Ok(true),不 panic。
    #[tokio::test]
    async fn do_compact_provider_failure_pushes_error() {
        let mut m = ReplModel::new("s1".into(), "gpt-5".into(), 128_000);
        let env = CompactTestEnv { db_path: std::path::PathBuf::new(), fail_provider: true };
        let mut registry = rc_profile::model::Registry::default();
        let ok = do_compact(&mut m, &env, &mut registry).await.unwrap();
        assert!(ok);
        assert!(m.output.iter().any(|l| l.text.contains("[error]")));
    }

    /// /compact 集成用 stub provider:返回固定摘要 "SUM"。
    struct StubCompactProvider;

    #[async_trait::async_trait]
    impl rc_pro::Provider for StubCompactProvider {
        fn id(&self) -> &str {
            "mock:compact"
        }
        async fn stream(
            &self,
            _req: rc_pro::canonical::CanonicalRequest,
        ) -> Result<
            std::pin::Pin<Box<dyn futures::Stream<Item = Result<rc_pro::ProvEvent, rc_pro::ProviderError>> + Send>>,
            rc_pro::ProviderError,
        > {
            let stream = futures::stream::iter(vec![
                Ok::<_, rc_pro::ProviderError>(rc_pro::ProvEvent::Delta { text: "SUM".into() }),
                Ok(rc_pro::ProvEvent::Finish { stop_reason: "stop".into(), usage: None }),
            ]);
            Ok(Box::pin(stream))
        }
        async fn embed(&self, _texts: Vec<String>) -> Result<Vec<Vec<f32>>, rc_pro::ProviderError> {
            Ok(vec![])
        }
    }

    /// /compact 集成用 env:make_provider 返回 stub(或 fail_provider 时 bail),open_store 打开同一 db。
    struct CompactTestEnv {
        db_path: std::path::PathBuf,
        fail_provider: bool,
    }

    #[async_trait::async_trait(?Send)]
    impl ReplEnv for CompactTestEnv {
        fn load_registry(&self) -> anyhow::Result<rc_profile::model::Registry> {
            Ok(Default::default())
        }
        fn save_registry(&self, _r: &rc_profile::model::Registry) -> anyhow::Result<()> {
            Ok(())
        }
        fn home_dir(&self) -> std::path::PathBuf {
            std::path::PathBuf::new()
        }
        fn skills_dir(&self) -> std::path::PathBuf {
            std::path::PathBuf::new()
        }
        fn workspace(&self) -> std::path::PathBuf {
            std::path::PathBuf::from(".")
        }
        fn create_session(&self) -> anyhow::Result<String> {
            Ok("x".into())
        }
        fn open_store(&self) -> anyhow::Result<rc_state::Store> {
            rc_state::Store::open(&self.db_path).map_err(anyhow::Error::from)
        }
        fn make_provider(
            &self,
            _r: &rc_profile::model::Registry,
        ) -> anyhow::Result<crate::repl::env::BoxProvider> {
            if self.fail_provider {
                anyhow::bail!("no provider")
            }
            Ok(std::sync::Arc::new(StubCompactProvider))
        }
        fn dispatch_slash(&self, _n: &str, _a: &serde_json::Value) -> Result<String, String> {
            Ok("".into())
        }
        fn skill_nav(&self, _t: &str) -> Result<Vec<String>, String> {
            Ok(vec![])
        }
        fn store_key(&self, _i: &str, _k: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn key_ref(&self, _i: &str) -> String {
            String::new()
        }
        async fn verify_connectivity(
            &self,
            _p: &rc_profile::model::Profile,
        ) -> anyhow::Result<String> {
            Ok("ok".into())
        }
        async fn agent_config(
            &self,
            _r: &rc_profile::model::Registry,
            _w: bool,
        ) -> anyhow::Result<rc_core::AgentConfig> {
            anyhow::bail!("no agent")
        }
        fn context_window(&self, _r: &rc_profile::model::Registry) -> u64 {
            128_000
        }
        async fn refresh_profiles(&self) -> anyhow::Result<String> {
            Ok("ok".into())
        }
        fn model_picker_entries(
            &self,
        ) -> anyhow::Result<Vec<crate::repl::env::ModelPickerEntry>> {
            Ok(Vec::new())
        }
        fn route_run(
            &self,
            _prompt: String,
            _plan_only: bool,
            _emit: std::sync::Arc<dyn Fn(rc_proto::AgentEvent) + Send + Sync>,
            _steer_hub: std::sync::Arc<rc_core::SteerHub>,
            _cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
            _risk_mode: rc_router::risk::RiskMode,
            _subagent_approval: std::sync::Arc<dyn rc_sandbox::ApprovalHook>,
            _supervisor: Option<std::sync::Arc<rc_core::Supervisor>>,
            _feed: AgentFeed,
        ) {
        }
        fn supervise_start(
            &self,
            _r: &rc_profile::model::Registry,
            _m: Option<&str>,
        ) -> Result<std::sync::Arc<rc_core::Supervisor>, String> {
            Ok(std::sync::Arc::new(rc_core::Supervisor {
                provider: Box::new(StubCompactProvider),
                cfg: rc_sandbox::guard::SuperviseConfig::default(),
                boundaries: String::new(),
            }))
        }
        fn supervise_config_path(&self) -> std::path::PathBuf {
            std::path::PathBuf::new()
        }
    }
}
