//! Raincode agent loop.
//!
//! A run starts a session, selects the top skills for the task, injects them
//! plus the tool registry into the system prompt, streams provider events,
//! executes tools, persists the transcript, and triggers the evolve engine
//! when the run finishes.

use std::collections::HashMap;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::{Stream, StreamExt};
use rc_evolve::{EvolveConfig, EvolveEngine};
use rc_pro::canonical::{CanonicalMessage, CanonicalRequest, CanonicalRole, CanonicalToolCall, ToolDef};
use rc_pro::ProvEvent;
use rc_pro::Provider;
use rc_proto::AgentEvent;
use rc_sandbox::{ApprovalHook, CommandPolicy, NetworkPolicy, UserInputHook};
use rc_skill::{SkillRouter, SkillStore, SkillSummary};
use rc_state::{MessageRole, Store};
use rc_tool::{Tool, ToolContext, ToolRegistry, ToolResult};
use serde_json::{json, Value};
use tokio::sync::mpsc::{channel, Sender};
use tokio_stream::wrappers::ReceiverStream;

pub mod append_only;
pub mod compact;
pub mod hooks;
pub mod supervise;
pub use hooks::HooksConfig;
pub use supervise::{agent_id_of, Supervisor, SupervisorAction, SupervisorBatch};
use hooks::{decision as hook_decision, run_hook, session_payload};
use append_only::{AppendOnlyLog, StablePrefix};

pub type AgentStream = Pin<Box<dyn Stream<Item = AgentEvent> + Send>>;

/// 向运行中的子代理注入引导文本的注册表。每个子代理 spawn 前 `register`,
/// 获得自己的 receiver;用户对某 agent 发命令/喂引导 → `send(id, text)`。
/// 该文本由 Agent 的 steering 检查点拾取,作为下一轮最高优先级 user 消息注入。
pub struct SteerHub {
    inner: Mutex<HashMap<String, tokio::sync::mpsc::UnboundedSender<String>>>,
}

impl SteerHub {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    pub fn register(
        &self,
        id: &str,
    ) -> tokio::sync::mpsc::UnboundedReceiver<String> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        if let Ok(mut inner) = self.inner.lock() {
            inner.insert(id.to_string(), tx);
        }
        rx
    }

    pub fn unregister(&self, id: &str) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.remove(id);
        }
    }

    /// 返回 false 表示该 agent 未注册/已结束。
    pub fn send(&self, id: &str, text: &str) -> bool {
        self.inner
            .lock()
            .map(|m| m.get(id).map(|tx| tx.send(text.to_string()).is_ok()).unwrap_or(false))
            .unwrap_or(false)
    }
}

impl Default for SteerHub {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("state error: {0}")]
    State(#[from] rc_state::DbError),
    #[error("provider error: {0}")]
    Provider(#[from] rc_pro::ProviderError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("event channel closed")]
    ChannelClosed,
    #[error("state lock poisoned")]
    LockPoisoned,
    #[error("cancelled by user")]
    Cancelled,
}

pub const DEFAULT_AGENT: &str = "coding";

/// Fallback context window (tokens) until the provider catalog's
/// `context_window` is wired in (see the TODO in `run_execute_phase`).
const CONTEXT_LIMIT_FALLBACK: u64 = 128_000;

pub struct AgentConfig {
    pub provider: Arc<dyn Provider>,
    pub plan_provider: Option<Arc<dyn Provider>>,
    pub review_provider: Option<Arc<dyn Provider>>,
    pub store: Store,
    pub skill_store: SkillStore,
    pub tools: Vec<Box<dyn Tool>>,
    pub approval: Arc<dyn ApprovalHook>,
    pub command_policy: CommandPolicy,
    pub network_policy: NetworkPolicy,
    pub cwd: PathBuf,
    pub state_path: PathBuf,
    pub max_turns: usize,
    /// step 上限守卫:最后一步不物化工具 + MAX STEPS 消息。0 = 用 max_turns。
    pub max_steps: usize,
    pub evolve_on_finish: bool,
    pub plan_mode: bool,
    pub hooks: HooksConfig,
    pub agent: Option<String>,
    pub max_history_bytes: Option<usize>,
    pub mcp_servers: Vec<(String, Vec<String>)>,
    pub entropy_mode: bool,
    pub plan_max_rounds: usize,
    pub plan_max_questions: usize,
    pub review_max_rounds: usize,
    pub max_cycles: usize,
    pub user_input: Arc<dyn UserInputHook>,
    /// 可选的 steering 接收端:用户注入的引导文本(见 [`SteerHub`])。
    pub steer_rx: Option<tokio::sync::mpsc::UnboundedReceiver<String>>,
    /// 当前模型的上下文窗口(token)。0 = 未知,使用兜底值。
    pub context_window: u64,
    /// 子代理工厂(delegate_research 工具用):主模型按需派聚焦子代理。
    pub subagent: Option<std::sync::Arc<rc_tool::SubagentFn>>,
    /// 监督守卫配置(默认 None = 守卫关闭)。有值则所有工具执行前过 guard_check。
    pub guard_cfg: Option<rc_sandbox::guard::SuperviseConfig>,
    /// 用户授权闸 hook(高危操作四选一;TUI 注入真实弹窗)。None = 无 hook,
    /// 需要授权时保守拦截。
    pub guard_hook: Option<Arc<dyn rc_sandbox::guard_hook::GuardHook>>,
    /// 会话级放行记忆(Session 同意后同类不再弹)。
    pub guard_memo: Option<Arc<rc_sandbox::guard_hook::SessionGuardMemo>>,
    /// 策略文件所在目录(含 supervise.toml);Forever 放行写回用。None = 不可写回。
    pub guard_home: Option<PathBuf>,
}

#[derive(Clone)]
pub struct Agent {
    inner: Arc<AgentInner>,
}

struct AgentInner {
    provider: Arc<dyn Provider>,
    plan_provider: Option<Arc<dyn Provider>>,
    review_provider: Option<Arc<dyn Provider>>,
    store: Arc<Mutex<Store>>,
    skill_store: SkillStore,
    tools: ToolRegistry,
    approval: Arc<dyn ApprovalHook>,
    command_policy: CommandPolicy,
    network_policy: NetworkPolicy,
    cwd: PathBuf,
    state_path: PathBuf,
    max_turns: usize,
    max_steps: usize,
    evolve_on_finish: bool,
    plan_mode: bool,
    hooks: HooksConfig,
    agent: Option<String>,
    max_history_bytes: Option<usize>,
    mcp_servers: Vec<(String, Vec<String>)>,
    entropy_mode: bool,
    plan_max_rounds: usize,
    plan_max_questions: usize,
    review_max_rounds: usize,
    max_cycles: usize,
    user_input: Arc<dyn UserInputHook>,
    steer_rx: Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<String>>>,
    cancel: Arc<AtomicBool>,
    context_window: u64,
    /// 工具执行超时:ToolRegistry::run 本身不强制 deadline,execute_tool 用它
    /// 包一层 tokio::time::timeout(挂死的工具/子代理不至于永久卡住 run)。
    tool_timeout: Duration,
    /// 会话级稳定前缀(system + tools),字节恒等缓存。
    /// 【VESTIGIAL】当前循环不读它:字节稳定性由结构保证(system prompt + tool_defs
    /// 每次请求前统一重建,缓存命中已通过请求体字节不变达成),因此该字段没有读方。
    /// 保留给未来 in-place-rewrite(Plan B)显式 prefix-cache 命中复用;
    /// `#[allow(dead_code)]` 因此必须保留,勿删字段。
    #[allow(dead_code)]
    prefix: Mutex<Option<StablePrefix>>,
    /// 工具定义按名缓存(tool_defs 不再每轮重建 → 字节稳定)。
    tool_cache: Mutex<Option<Vec<ToolDef>>>,
    subagent: Option<std::sync::Arc<rc_tool::SubagentFn>>,
    guard_cfg: Option<rc_sandbox::guard::SuperviseConfig>,
    guard_hook: Option<Arc<dyn rc_sandbox::guard_hook::GuardHook>>,
    guard_memo: Option<Arc<rc_sandbox::guard_hook::SessionGuardMemo>>,
    guard_home: Option<PathBuf>,
}

impl Agent {
    pub fn new(config: AgentConfig) -> Self {
        let inner = AgentInner {
            provider: config.provider,
            plan_provider: config.plan_provider,
            review_provider: config.review_provider,
            store: Arc::new(Mutex::new(config.store)),
            skill_store: config.skill_store,
            tools: ToolRegistry::new(config.tools),
            approval: config.approval,
            command_policy: config.command_policy,
            network_policy: config.network_policy,
            cwd: config.cwd,
            state_path: config.state_path,
            max_turns: config.max_turns,
            max_steps: config.max_steps,
            evolve_on_finish: config.evolve_on_finish,
            plan_mode: config.plan_mode,
            hooks: config.hooks,
            agent: config.agent,
            max_history_bytes: config.max_history_bytes,
            mcp_servers: config.mcp_servers,
            entropy_mode: config.entropy_mode,
            plan_max_rounds: config.plan_max_rounds,
            plan_max_questions: config.plan_max_questions,
            review_max_rounds: config.review_max_rounds,
            max_cycles: config.max_cycles,
            user_input: config.user_input,
            steer_rx: Mutex::new(config.steer_rx),
            cancel: Arc::new(AtomicBool::new(false)),
            context_window: config.context_window,
            tool_timeout: Duration::from_secs(180),
            prefix: Mutex::new(None),
            tool_cache: Mutex::new(None),
            subagent: config.subagent.clone(),
            guard_cfg: config.guard_cfg,
            guard_hook: config.guard_hook,
            guard_memo: config.guard_memo,
            guard_home: config.guard_home,
        };
        Self {
            inner: Arc::new(inner),
        }
    }

    /// Start a run and return its event stream. The loop runs in a background
    /// task so callers can consume tokens, tool calls and results as they
    /// happen.
    pub fn run(&self, session_id: String, prompt: String) -> AgentStream {
        self.inner.cancel.store(false, Ordering::Relaxed);
        let inner = self.inner.clone();
        let (tx, rx) = channel(128);
        tokio::spawn(async move {
            if let Err(error) = inner.run_loop(session_id, prompt, tx.clone()).await {
                let _ = tx.send(AgentEvent::Error {
                    message: error.to_string(),
                })
                .await;
            }
        });
        Box::pin(ReceiverStream::new(rx))
    }

    /// Signal the running task to stop at its next checkpoint. Each subsequent
    /// `run()` resets the flag, so the same `Agent` can be reused.
    pub fn cancel(&self) {
        self.inner.cancel.store(true, Ordering::Relaxed);
    }
}

impl AgentInner {
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// 清理托管 tool_output 目录中超过 7 天的输出文件。该目录由工具输出
    /// 有界化(>50KB 持久化)写入,生产环境需定期清理防无限增长。
    /// 每次 run() 幂等执行一次,目录不存在时静默返回 0。
    fn cleanup_tool_outputs(&self) -> usize {
        let dir = self
            .state_path
            .parent()
            .unwrap_or(&self.state_path)
            .join("tool_output");
        rc_tool::tool_output::ToolOutputStore::new(dir).cleanup_older_than(7)
    }

    async fn run_loop(
        &self,
        session_id: String,
        prompt: String,
        tx: Sender<AgentEvent>,
    ) -> Result<(), CoreError> {
        tx.send(AgentEvent::SessionStarted {
            session_id: session_id.clone(),
        })
        .await
        .map_err(|_| CoreError::ChannelClosed)?;

        // 每次 run() 清理一次托管 tool_output 目录:删除超过 7 天的持久化
        // 工具输出,防止生产环境目录无限增长(与 opencode 7 天清理对齐)。
        self.cleanup_tool_outputs();

        self.run_session_hook(&session_id, "session_start").await;

        for (server, tools) in &self.mcp_servers {
            tx.send(AgentEvent::McpToolList {
                server: server.clone(),
                tools: tools.clone(),
            })
            .await
            .map_err(|_| CoreError::ChannelClosed)?;
        }

        {
            let store = self.store.lock().map_err(|_| CoreError::LockPoisoned)?;
            store.append_message(&session_id, MessageRole::User, &prompt)?;
        }

        let skills = self.select_skills(&prompt).await;
        for skill in &skills {
            tx.send(AgentEvent::SkillSuggested {
                name: skill.name.clone(),
                category: skill.category.clone(),
                confidence: skill.score,
            })
            .await
            .map_err(|_| CoreError::ChannelClosed)?;
        }

        let system = self.build_system_prompt();
        let stored = {
            let store = self.store.lock().map_err(|_| CoreError::LockPoisoned)?;
            store.list_messages(&session_id)?
        };
        let mut messages = vec![CanonicalMessage::system(system)];
        // 相关 skill 提示(动态尾,不进稳定前缀):告诉模型本任务最相关的 skill,
        // 鼓励按需加载 → 产生 usage 数据 → darwinian 演化才能学习。缓存安全:
        // 稳定 system 前缀保持字节恒等,这条按任务变化的提示是缓存 miss 的尾部。
        if let Some(hint) = relevant_skills_hint(&skills) {
            messages.push(CanonicalMessage::system(hint));
            // 强匹配(score ≥ 3.0)自动注入 skill 正文 + 记录使用:不依赖模型主观
            // 决定是否调 skill 工具——否则学习数据不稳定(模型经常"觉得没必要加载")。
            // 阈值 3.0 ≈ 至少一个 trigger 命中,代表真实相关。
            if let Some(top) = skills.iter().max_by(|a, b| a.score.total_cmp(&b.score)) {
                if top.score >= 3.0 {
                    if let Some(skill) = self.skill_store.load(&top.name) {
                        messages.push(CanonicalMessage::system(format!(
                            "<skill_content name=\"{}\">\n{}\n</skill_content>",
                            skill.name, skill.body
                        )));
                        // 自动注入 = skill 指导已进入上下文 → 记一次使用(学习数据)。
                        if let Ok(store) = self.store.lock() {
                            let _ = store.upsert_skill(&skill.to_row());
                            let _ = store.bump_skill_usage(&skill.name, true);
                        }
                        tx.send(AgentEvent::SkillLoaded {
                            name: skill.name.clone(),
                            path: skill.path.to_string_lossy().to_string(),
                        })
                        .await
                        .map_err(|_| CoreError::ChannelClosed)?;
                    }
                }
            }
        }
        let mut last_call: Option<(String, String)> = None;
        for message in stored {
            match message.role {
                MessageRole::User => messages.push(CanonicalMessage::user(message.content)),
                MessageRole::Assistant => {
                    if let Some((id, name, arguments)) = parse_tool_call_text(&message.content) {
                        last_call = Some((id.clone(), name.clone()));
                        messages.push(CanonicalMessage::assistant_tool_calls(vec![
                            CanonicalToolCall {
                                id,
                                name,
                                arguments,
                            },
                        ]));
                    } else {
                        messages.push(CanonicalMessage::assistant_text(message.content));
                    }
                }
                MessageRole::Tool => {
                    let (name, output) = parse_tool_result_text(&message.content);
                    let id = last_call
                        .as_ref()
                        .map(|(id, _)| id.clone())
                        .unwrap_or_default();
                    messages.push(CanonicalMessage::tool(id, name, output));
                }
                MessageRole::System => messages.push(CanonicalMessage::system(message.content)),
            }
        }
        messages = self.compact_messages(messages);

        let result = self
            .run_main_phases(&session_id, messages, &tx)
            .await;
        // 取消时也要收尾:session_end hook 对已启动的会话必须触发(遥测/通知/技能沉淀),
        // 否则 cancel 会静默跳过收尾。plan-only/entropy/正常完成路径已在内部触发过。
        if matches!(&result, Err(CoreError::Cancelled)) {
            self.run_session_hook(&session_id, "session_end").await;
        }
        result
    }

    /// plan/entropy/执行/审查主流程,返回最终结果。run_loop 负责在 Cancelled 时
    /// 补发 session_end(plan-only/entropy/正常完成路径已在内部触发收尾)。
    async fn run_main_phases(
        &self,
        session_id: &str,
        mut messages: Vec<CanonicalMessage>,
        tx: &Sender<AgentEvent>,
    ) -> Result<(), CoreError> {
        if self.plan_mode {
            if self.entropy_mode {
                let mut plan_messages = messages.clone();
                plan_messages[0] =
                    CanonicalMessage::system(self.build_entropy_plan_prompt());
                let plan = self
                    .run_entropy_plan(session_id, plan_messages, tx)
                    .await?;
                tx.send(AgentEvent::Done {
                    summary: plan,
                    usage: None,
                    session_id: session_id.to_string(),
                    reasoning: None,
                })
                .await
                .map_err(|_| CoreError::ChannelClosed)?;
                self.run_session_hook(session_id, "session_end").await;
                if self.evolve_on_finish {
                    self.evolve(session_id).await;
                }
                return Ok(());
            }
            let plan_request = CanonicalRequest {
                model: self.provider.id().to_string(),
                messages: messages.clone(),
                tools: vec![],
                temperature: Some(0.2),
                max_tokens: Some(4000),
                stream: true,
                extra: json!({"session_id": session_id}),
            };
            let mut stream = self.provider.stream(plan_request).await?;
            let mut plan = String::new();
            let mut plan_usage = None;
            while let Some(event) = stream.next().await {
                match event.map_err(CoreError::Provider)? {
                    ProvEvent::Delta { text } => {
                        plan.push_str(&text);
                        tx.send(AgentEvent::Token { delta: text })
                            .await
                            .map_err(|_| CoreError::ChannelClosed)?;
                    }
                    ProvEvent::Thinking { text } => {
                        tx.send(AgentEvent::Thinking { delta: text })
                            .await
                            .map_err(|_| CoreError::ChannelClosed)?;
                    }
                    ProvEvent::Finish { usage, .. } => plan_usage = usage,
                    ProvEvent::Error { message } => {
                        return Err(rc_pro::ProviderError::Config(message).into())
                    }
                    _ => {}
                }
            }
            {
                let store = self.store.lock().map_err(|_| CoreError::LockPoisoned)?;
                store.append_message(session_id, MessageRole::Assistant, &plan)?;
                store.set_session_summary(session_id, &plan)?;
            }
            tx.send(AgentEvent::PlanProposed {
                summary: plan.clone(),
                session_id: session_id.to_string(),
            })
            .await
            .map_err(|_| CoreError::ChannelClosed)?;
            tx.send(AgentEvent::Done {
                summary: plan.clone(),
                usage: plan_usage,
                session_id: session_id.to_string(),
                reasoning: None,
            })
            .await
            .map_err(|_| CoreError::ChannelClosed)?;
            // 与 entropy-plan 路径/正常路径一致:plan-only 会话也触发 session_end
            // hook 与技能演化,否则通知/遥测/技能沉淀对 plan-only 会话静默缺失。
            self.run_session_hook(session_id, "session_end").await;
            if self.evolve_on_finish {
                self.evolve(session_id).await;
            }
            return Ok(());
        }

        let cycles = if self.entropy_mode {
            self.max_cycles.max(1)
        } else {
            1
        };
        let mut final_summary = String::new();
        let mut final_usage = None;
        let mut final_thinking = String::new();
        let mut review_note = String::new();
        for cycle in 0..cycles {
            if self.cancelled() {
                return Err(CoreError::Cancelled);
            }
            if self.entropy_mode {
                self.send_phase(tx, session_id, "plan", cycle).await?;
                let mut plan_messages = messages.clone();
                plan_messages[0] =
                    CanonicalMessage::system(self.build_entropy_plan_prompt());
                let plan = self
                    .run_entropy_plan(session_id, plan_messages, tx)
                    .await?;
                messages.push(CanonicalMessage::assistant_text(plan));
                self.send_phase(tx, session_id, "execute", cycle).await?;
            }

            let (summary, usage, updated, thinking) =
                self.run_execute_phase(session_id, messages, tx).await?;
            messages = updated;
            final_summary = summary;
            final_usage = usage;
            final_thinking = thinking;

            if !self.entropy_mode {
                break;
            }

            self.send_phase(tx, session_id, "review", cycle).await?;
            let review = self.run_review(session_id, &messages, tx).await?;
            review_note = review.text.clone();
            messages.push(CanonicalMessage::assistant_text(review.text.clone()));
            tx.send(AgentEvent::ReviewProposed {
                verdict: review.verdict.as_str().to_string(),
                reason: review.reason.clone(),
                next_intent: review.next_intent.clone(),
                summary: review.text.clone(),
                cycle,
                session_id: session_id.to_string(),
            })
            .await
            .map_err(|_| CoreError::ChannelClosed)?;

            if review.verdict == ReviewVerdict::Accept {
                break;
            }
            if cycle + 1 >= cycles {
                break;
            }
            self.send_phase(tx, session_id, "re-understand", cycle)
                .await?;
            let reunderstood = if review.next_intent.trim().is_empty() {
                review.reason.clone()
            } else {
                review.next_intent.clone()
            };
            let revision = format!(
                "Review rejected this attempt.\nReason: {}\nRe-understood user intent: {}\nProduce a revised plan and execute it again.",
                review.reason, reunderstood
            );
            {
                let store = self.store.lock().map_err(|_| CoreError::LockPoisoned)?;
                store.append_message(session_id, MessageRole::User, &revision)?;
            }
            messages.push(CanonicalMessage::user(revision));
        }

        let done_summary = if review_note.is_empty() {
            final_summary
        } else {
            format!("{}\n\nReview:\n{}", final_summary, review_note)
        };
        tx.send(AgentEvent::Done {
            summary: done_summary,
            usage: final_usage,
            session_id: session_id.to_string(),
            reasoning: (!final_thinking.is_empty()).then_some(final_thinking),
        })
        .await
        .map_err(|_| CoreError::ChannelClosed)?;

        self.run_session_hook(session_id, "session_end").await;

        if self.evolve_on_finish {
            self.evolve(session_id).await;
        }
        Ok(())
    }

    async fn select_skills(&self, task: &str) -> Vec<SkillSummary> {
        let router = SkillRouter::new(self.skill_store.discover());
        let key = content_hash(&format!("{}|{task}", self.provider.id()));
        let cached = self
            .store
            .lock()
            .ok()
            .and_then(|store| store.get_embedding(&key).ok().flatten())
            .and_then(|raw| serde_json::from_str::<Vec<f32>>(&raw).ok());
        if let Some(embedding) = cached {
            return router.select_with_embedding(task, &embedding, 4);
        }
        match self.provider.embed(vec![task.to_string()]).await {
            Ok(mut vecs) => {
                let Some(embedding) = vecs.pop() else {
                    return router.select_keyword(task, 4);
                };
                if let Ok(raw) = serde_json::to_string(&embedding) {
                    if let Ok(store) = self.store.lock() {
                        let _ = store.cache_embedding(&key, self.provider.id(), "", &raw);
                    }
                }
                router.select_with_embedding(task, &embedding, 4)
            }
            Err(_) => router.select_keyword(task, 4),
        }
    }

    fn build_system_prompt(&self) -> String {
        let mut prompt = String::from(
            "You are Raincode, an autonomous coding agent. Work in the workspace, \
             keep changes small and verifiable, and prefer loading a matching skill \
             before doing unfamiliar work.",
        );
        if let Some(agent) = &self.agent {
            prompt.push_str(&format!(
                "\n\nAgent profile `{agent}`:\n{}",
                self.agent_instruction(agent)
            ));
        }
        prompt.push_str("\n\nAvailable skills (load one with the `skill` tool when relevant):\n");
        prompt.push_str(&self.skill_catalog());
        prompt.push_str("\nTools:\n");
        for spec in self.tool_defs() {
            prompt.push_str(&format!(
                "- `{}`: {} | schema: {}\n",
                spec.name, spec.description, spec.input_schema
            ));
        }
        for file in ["AGENTS.md", "CLAUDE.md"] {
            let path = self.cwd.join(file);
            if let Ok(text) = std::fs::read_to_string(&path) {
                prompt.push_str(&format!("\nWorkspace {file}:\n"));
                prompt.push_str(&text);
            }
        }
        if self.plan_mode {
            prompt.push_str(
                "You are in PLAN MODE. Produce a concise, actionable step-by-step plan. Do not call tools.",
            );
        } else {
            prompt.push_str("Do not announce what you are about to do; do it.");
        }
        prompt
    }

    /// 稳定 skill 目录:discover 全部 → 按名排序 → 名字 + 一句描述。
    /// 不随任务/轮次变(不含 score),保证 system 前缀字节稳定。
    fn skill_catalog(&self) -> String {
        let mut skills = self.skill_store.discover();
        skills.sort_by(|a, b| a.name.cmp(&b.name));
        if skills.is_empty() {
            return "  (none yet; the skill network is still growing)\n".to_string();
        }
        let mut out = String::new();
        for skill in skills {
            out.push_str(&format!("- `{}`: {}\n", skill.name, skill.description));
        }
        out
    }

    fn build_entropy_plan_prompt(&self) -> String {        let mut prompt = String::from(
            "You are Raincode in ENTROPY REDUCTION PLANNING mode. \
             Your job is to make the task deterministic before execution. \
             Inspect the workspace with read-only tools, load matching skills, \
             and call `ask_user` whenever intent, scope, success criteria, \
             constraints or trade-offs are uncertain. Do not edit files or run \
             shell commands. Finish with a concise numbered plan.",
        );
        if let Some(agent) = &self.agent {
            prompt.push_str(&format!(
                "\n\nAgent profile `{agent}`:\n{}",
                self.agent_instruction(agent)
            ));
        }
        prompt.push_str("\n\nRelevant skills:\n");
        prompt.push_str(&self.skill_catalog());
        prompt.push_str("\nPlanning tools:\n");
        for spec in self.entropy_plan_tool_defs() {
            prompt.push_str(&format!(
                "- `{}`: {} | schema: {}\n",
                spec.name, spec.description, spec.input_schema
            ));
        }
        for file in ["AGENTS.md", "CLAUDE.md"] {
            let path = self.cwd.join(file);
            if let Ok(text) = std::fs::read_to_string(&path) {
                prompt.push_str(&format!("\nWorkspace {file}:\n"));
                prompt.push_str(&text);
            }
        }
        prompt.push_str(
            "Reduce entropy first: ask until the remaining uncertainty is \
             small enough to write an executable plan.",
        );
        prompt
    }

    fn entropy_plan_tool_defs(&self) -> Vec<ToolDef> {
        self.tools
            .specs()
            .into_iter()
            .filter(|spec| {
                matches!(
                    spec.name.as_str(),
                    "ask_user"
                        | "read_file"
                        | "list_dir"
                        | "grep"
                        | "skill"
                        | "web_fetch"
                        | "web_search"
                )
            })
            .map(|spec| ToolDef {
                name: spec.name,
                description: spec.description,
                input_schema: spec.input_schema,
            })
            .collect()
    }

    async fn run_entropy_plan(
        &self,
        session_id: &str,
        mut messages: Vec<CanonicalMessage>,
        tx: &Sender<AgentEvent>,
    ) -> Result<String, CoreError> {
        let provider = self
            .plan_provider
            .as_deref()
            .unwrap_or(self.provider.as_ref());
        let mut plan_parts = Vec::new();
        let mut asked = 0usize;
        // entropy-plan 阶段的工具快照:只允许执行本阶段暴露的工具集。
        let plan_tool_names: Vec<String> = self
            .entropy_plan_tool_defs()
            .iter()
            .map(|t| t.name.clone())
            .collect();
        for _ in 0..self.plan_max_rounds.max(1) {
            if self.cancelled() {
                return Err(CoreError::Cancelled);
            }
            let request = CanonicalRequest {
                model: provider.id().to_string(),
                messages: messages.clone(),
                tools: self.entropy_plan_tool_defs(),
                temperature: Some(0.2),
                max_tokens: Some(4000),
                stream: true,
                // 与主循环一致:plan 调用也携带 session_id → OpenAI/OpenRouter
                // 会用同一 prompt_cache_key 复用 prompt 前缀缓存。
                extra: json!({"session_id": session_id}),
            };
            let mut stream = provider.stream(request).await?;
            let mut text = String::new();
            let mut calls = Vec::new();
            while let Some(event) = stream.next().await {
                match event.map_err(CoreError::Provider)? {
                    ProvEvent::Delta { text: delta } => {
                        text.push_str(&delta);
                        tx.send(AgentEvent::Token { delta })
                            .await
                            .map_err(|_| CoreError::ChannelClosed)?;
                    }
                    ProvEvent::Thinking { text: delta } => {
                        tx.send(AgentEvent::Thinking { delta })
                            .await
                            .map_err(|_| CoreError::ChannelClosed)?;
                    }
                    ProvEvent::ToolCall {
                        id,
                        name,
                        arguments,
                    } => {
                        tx.send(AgentEvent::ToolCall {
                            id: id.clone(),
                            name: name.clone(),
                            args: arguments.clone(),
                        })
                        .await
                        .map_err(|_| CoreError::ChannelClosed)?;
                        calls.push(CanonicalToolCall {
                            id,
                            name,
                            arguments,
                        });
                    }
                    ProvEvent::ToolCallEnd { .. } => {}
                    ProvEvent::Finish { .. } => {}
                    ProvEvent::Error { message } => {
                        return Err(rc_pro::ProviderError::Config(message).into())
                    }
                }
            }
            if !text.trim().is_empty() {
                plan_parts.push(text.trim().to_string());
                messages.push(CanonicalMessage::assistant_text(text));
            }
            if calls.is_empty() {
                break;
            }
            messages.push(CanonicalMessage::assistant_tool_calls(calls.clone()));
            for call in &calls {
                let allowed = matches!(
                    call.name.as_str(),
                    "ask_user"
                        | "read_file"
                        | "list_dir"
                        | "grep"
                        | "skill"
                        | "web_fetch"
                        | "web_search"
                );
                if !allowed {
                    let result = ToolResult::err("tool not allowed in entropy-reduction planning");
                    self.send_tool_result(tx, call, &result).await?;
                    messages.push(CanonicalMessage::tool(
                        call.id.clone(),
                        call.name.clone(),
                        result.output,
                    ));
                    continue;
                }
                let result = if call.name == "ask_user" {
                    if asked >= self.plan_max_questions {
                        let result = ToolResult::err(
                            "Maximum clarifying questions reached; proceed with best judgment.",
                        );
                        self.send_tool_result(tx, call, &result).await?;
                        result
                    } else {
                        asked += 1;
                        self.execute_tool(session_id, tx, call, &plan_tool_names).await?
                    }
                } else {
                    self.execute_tool(session_id, tx, call, &plan_tool_names).await?
                };
                messages.push(CanonicalMessage::tool(
                    call.id.clone(),
                    call.name.clone(),
                    result.output.clone(),
                ));
            }
        }
        let plan = if plan_parts.is_empty() {
            "Plan: proceed with best judgment after clarifying intent.".to_string()
        } else {
            plan_parts.join("\n\n")
        };
        {
            let store = self.store.lock().map_err(|_| CoreError::LockPoisoned)?;
            store.append_message(session_id, MessageRole::Assistant, &plan)?;
            store.set_session_summary(session_id, &plan)?;
        }
        tx.send(AgentEvent::PlanProposed {
            summary: plan.clone(),
            session_id: session_id.to_string(),
        })
        .await
        .map_err(|_| CoreError::ChannelClosed)?;
        Ok(plan)
    }

    async fn send_phase(
        &self,
        tx: &Sender<AgentEvent>,
        session_id: &str,
        phase: &str,
        cycle: usize,
    ) -> Result<(), CoreError> {
        tx.send(AgentEvent::PhaseChanged {
            phase: phase.to_string(),
            cycle,
            session_id: session_id.to_string(),
        })
        .await
        .map_err(|_| CoreError::ChannelClosed)
    }

    async fn run_execute_phase(
        &self,
        session_id: &str,
        messages: Vec<CanonicalMessage>,
        tx: &Sender<AgentEvent>,
    ) -> Result<(String, Option<Value>, Vec<CanonicalMessage>, String), CoreError> {
        let mut usage = None;
        let mut summary = String::new();
        // 累计本阶段完整思维链,随 Done.reasoning 带出(UI 可展开,不回传模型)。
        let mut assistant_thinking = String::new();
        let mut used_tokens = 0u64;
        // 消息层只追加:系统前缀/已定对话前缀一旦写入就不再重写。
        // 每轮压缩照旧(保 messages[0] + 从 index 1 起整组移除),但压缩只在
        // 确实缩容时才重建日志(与 pi 参考一致);digest 截断助手保留给原地
        // 重写场景,本轮不参与。
        let mut log = AppendOnlyLog::new(messages);
        // step 上限守卫:max_steps > 0 时用它覆盖 max_turns;0 = 兜底用 max_turns。
        let steps = if self.max_steps > 0 { self.max_steps } else { self.max_turns };
        for turn in 0..steps {
            if self.cancelled() {
                return Err(CoreError::Cancelled);
            }
            // 每轮在发请求前重新压缩:多轮工具调用会把历史推得越来越大,
            // 只在 store 重放时压一次不够(max_history_bytes 才有意义)。
            // 先取只读快照再压缩;只有真的删掉消息时才整体重建日志。
            let snapshot: Vec<CanonicalMessage> = log.as_slice().to_vec();
            let compacted = self.compact_messages(snapshot);
            if compacted.len() != log.as_slice().len() {
                log = AppendOnlyLog::new(compacted);
            }
            // Steering 检查点:注入文本作为本轮最高优先级 user 消息追加到尾部
            // (不重写已定前缀,AppendOnlyLog 保证只追加)。
            {
                let mut guard = self
                    .steer_rx
                    .lock()
                    .map_err(|_| CoreError::LockPoisoned)?;
                if let Some(rx) = guard.as_mut() {
                    while let Ok(s) = rx.try_recv() {
                        log.push(CanonicalMessage::user(format!("[steer] {s}")));
                    }
                }
            }
            // 每轮工具快照:只允许执行本轮 tool_defs 内的工具;不在快照的调用
            // (stale call,如模型重复已移除的工具)在 execute_tool 顶部被直接拒绝。
            // 顺序关键:先算 last_step,再取快照 —— 最后一步快照必须为空,这样
            // 非合规 provider 在最后一步发出的工具调用也会被 stale-check 拒绝。
            let last_step = turn + 1 >= steps;
            let turn_tools: Vec<String> = if last_step {
                vec![]
            } else {
                self.tool_defs().iter().map(|t| t.name.clone()).collect()
            };
            if last_step {
                // 追加硬性步数上限提示,模型只能文本回复。
                log.push(CanonicalMessage::assistant_text(
                    "MAXIMUM STEPS REACHED — respond with text only.",
                ));
            }
            let request = CanonicalRequest {
                model: self.provider.id().to_string(),
                messages: log.as_slice().to_vec(),
                tools: if last_step { vec![] } else { self.tool_defs() },
                temperature: None,
                max_tokens: None,
                stream: true,
                extra: json!({"session_id": session_id}),
            };
            let mut stream = self.provider.stream(request).await?;
            let mut assistant_text = String::new();
            // pending 收集本轮全部调用(id → call);call_order 记录调用到达顺序
            // (HashMap 不保序,并发结果回灌时要按调用顺序保证 executed 有序)。
            let mut pending: HashMap<String, CanonicalToolCall> = HashMap::new();
            let mut call_order: Vec<String> = Vec::new();
            let mut executed: Vec<(CanonicalToolCall, ToolResult)> = Vec::new();

            while let Some(event) = stream.next().await {
                if self.cancelled() {
                    return Err(CoreError::Cancelled);
                }
                match event.map_err(CoreError::Provider)? {
                    ProvEvent::Delta { text } => {
                        assistant_text.push_str(&text);
                        tx.send(AgentEvent::Token { delta: text })
                            .await
                            .map_err(|_| CoreError::ChannelClosed)?;
                    }
                    ProvEvent::Thinking { text } => {
                        assistant_thinking.push_str(&text);
                        tx.send(AgentEvent::Thinking { delta: text })
                            .await
                            .map_err(|_| CoreError::ChannelClosed)?;
                    }
                    ProvEvent::ToolCall {
                        id,
                        name,
                        arguments,
                    } => {
                        tx.send(AgentEvent::ToolCall {
                            id: id.clone(),
                            name: name.clone(),
                            args: arguments.clone(),
                        })
                        .await
                        .map_err(|_| CoreError::ChannelClosed)?;
                        let call = CanonicalToolCall {
                            id: id.clone(),
                            name,
                            arguments,
                        };
                        if pending.insert(id.clone(), call).is_none() {
                            call_order.push(id);
                        }
                    }
                    ProvEvent::ToolCallEnd { .. } => {
                        // 执行推迟到流结束后的并发阶段:这里只忽略结束标记,
                        // 调用留在 pending,由下方的 join_all 统一并发执行。
                    }
                    ProvEvent::Finish {
                        stop_reason,
                        usage: event_usage,
                    } => {
                        // Accumulate the provider-reported token usage into the
                        // running context counter and surface it to the frontend
                        // (supervision board top bar).
                        // 假设:provider 的 usage.total_tokens 是"单次请求"的 token 数
                        // (OpenAI/DeepSeek/Anthropic 均如此),累加 = 会话累计。若某
                        // provider 返回"会话累计",这里会重复计数 —— 见 M8 审计。
                        // TODO(plan3/task5): read the real `context_window` from
                        // the provider catalog in rc-profile instead of the
                        // 128k fallback constant.
                        if let Some(value) = &event_usage {
                            used_tokens = used_tokens.saturating_add(context_used_tokens(value));
                            // 用配置的真实 context_window;0/未知则退回 128k 兜底。
                            let limit = if self.context_window > 0 {
                                self.context_window
                            } else {
                                CONTEXT_LIMIT_FALLBACK
                            };
                            let pct = ((used_tokens as f64 / limit as f64) * 100.0) as u8;
                            tx.send(AgentEvent::ContextUpdate {
                                used: used_tokens,
                                limit,
                                pct: pct.min(100),
                                agent_id: None,
                            })
                            .await
                            .map_err(|_| CoreError::ChannelClosed)?;
                        }
                        usage = event_usage;
                        if stop_reason == "tool_calls" || stop_reason == "tool_use" {
                            // 工具调用已在流结束后并发执行;此 arm 只标记助手回合结束。
                        }
                    }
                    ProvEvent::Error { message } => {
                        // 并发 drain 只在流自然结束后执行 pending 调用;这里 mid-stream
                        // 出错直接返回 ⇒ 已收集的 pending 调用全部丢弃、不执行(旧串行
                        // 实现会先执行已到 ToolCallEnd 的调用)。有意的行为固化:provider
                        // 出错时不再执行任何工具,避免半途调用留下副作用。
                        return Err(rc_pro::ProviderError::Config(message).into());
                    }
                }
            }

            // 全部调用收集后并发执行:join_all 同时启动所有 future,结果按
            // remaining(调用到达顺序)回灌,保证 executed 与调用顺序一致。
            // 只有流正常结束才走到这里 —— mid-stream 出错时(上方 Error arm)
            // pending 已收集的调用会随 Err 返回被丢弃,不会在此执行。
            let mut remaining: Vec<CanonicalToolCall> = Vec::with_capacity(pending.len());
            for id in call_order {
                if let Some(call) = pending.remove(&id) {
                    remaining.push(call);
                }
            }
            // 防御:call_order 未覆盖的残留(理论上不应存在)也一并执行。
            remaining.extend(pending.into_values());
            if !remaining.is_empty() {
                let futs: Vec<_> = remaining
                    .iter()
                    .map(|call| self.execute_tool(session_id, tx, call, &turn_tools))
                    .collect();
                let results = futures::future::join_all(futs).await;
                for (call, result) in remaining.into_iter().zip(results) {
                    executed.push((call, result?));
                }
            }

            if executed.is_empty() {
                summary = assistant_text.clone();
                {
                    let store = self.store.lock().map_err(|_| CoreError::LockPoisoned)?;
                    if !assistant_text.is_empty() {
                        store.append_message(
                            session_id,
                            MessageRole::Assistant,
                            &assistant_text,
                        )?;
                    }
                    store.set_session_summary(session_id, &summary)?;
                }
                break;
            }

            let calls: Vec<CanonicalToolCall> =
                executed.iter().map(|(call, _)| call.clone()).collect();
            // 模型在工具调用前的前言/推理文本也要保留:否则下一轮模型看不到自己的
            // 思考过程,resume 会话的转录也缺一段。只在非空时追加。
            if !assistant_text.is_empty() {
                log.push(CanonicalMessage::assistant_text(assistant_text.clone()));
                {
                    let store = self.store.lock().map_err(|_| CoreError::LockPoisoned)?;
                    store.append_message(
                        session_id,
                        MessageRole::Assistant,
                        &assistant_text,
                    )?;
                }
            }
            log.push(CanonicalMessage::assistant_tool_calls(calls));
            for (call, result) in &executed {
                let output = result.output.clone();
                log.push(CanonicalMessage::tool(&call.id, &call.name, output.clone()));
                {
                    let store = self.store.lock().map_err(|_| CoreError::LockPoisoned)?;
                    store.append_message(
                        session_id,
                        MessageRole::Assistant,
                        &format!("tool_call {} {} {}", call.id, call.name, call.arguments),
                    )?;
                    store.append_message(
                        session_id,
                        MessageRole::Tool,
                        &format!("{}: {}", call.name, output),
                    )?;
                }
            }
        }
        Ok((summary, usage, log.into_messages(), assistant_thinking))
    }

    fn build_review_prompt(&self) -> String {
        let mut prompt = String::from(
            "You are the Raincode REVIEW model. Verify the execution results against the plan and the user's original intent. \
             Use read-only inspection tools when needed; never edit files. \
             End your reply with these lines:\nVERDICT: ACCEPT\nREASON: ...\nNEXT_USER_INTENT: ...\n\
             Use ACCEPT only when the work is complete and matches intent. Otherwise use REJECT and state what must be revisited.",
        );
        if let Some(agent) = &self.agent {
            prompt.push_str(&format!(
                "\n\nAgent profile `{agent}`:\n{}",
                self.agent_instruction(agent)
            ));
        }
        prompt
    }

    fn review_tool_defs(&self) -> Vec<ToolDef> {
        self.tools
            .specs()
            .into_iter()
            .filter(|spec| {
                matches!(
                    spec.name.as_str(),
                    "read_file" | "list_dir" | "grep" | "skill" | "web_fetch" | "web_search"
                )
            })
            .map(|spec| ToolDef {
                name: spec.name,
                description: spec.description,
                input_schema: spec.input_schema,
            })
            .collect()
    }

    async fn run_review(
        &self,
        session_id: &str,
        messages: &[CanonicalMessage],
        tx: &Sender<AgentEvent>,
    ) -> Result<ReviewOutcome, CoreError> {
        let provider = self
            .review_provider
            .as_deref()
            .unwrap_or(self.provider.as_ref());
        let mut review_messages = messages.to_vec();
        review_messages.insert(0, CanonicalMessage::system(self.build_review_prompt()));
        let mut parts = Vec::new();
        // review 阶段的工具快照:只允许执行本阶段暴露的工具集。
        let review_tool_names: Vec<String> = self
            .review_tool_defs()
            .iter()
            .map(|t| t.name.clone())
            .collect();
        for _ in 0..self.review_max_rounds.max(1) {
            if self.cancelled() {
                return Err(CoreError::Cancelled);
            }
            let request = CanonicalRequest {
                model: provider.id().to_string(),
                messages: review_messages.clone(),
                tools: self.review_tool_defs(),
                temperature: Some(0.0),
                max_tokens: Some(3000),
                stream: true,
                // 与主循环一致:review 调用也携带 session_id → 同样的 prompt_cache_key。
                extra: json!({"session_id": session_id}),
            };
            let mut stream = provider.stream(request).await?;
            let mut text = String::new();
            let mut calls = Vec::new();
            while let Some(event) = stream.next().await {
                match event.map_err(CoreError::Provider)? {
                    ProvEvent::Delta { text: delta } => {
                        text.push_str(&delta);
                        tx.send(AgentEvent::Token { delta })
                            .await
                            .map_err(|_| CoreError::ChannelClosed)?;
                    }
                    ProvEvent::Thinking { text: delta } => {
                        tx.send(AgentEvent::Thinking { delta })
                            .await
                            .map_err(|_| CoreError::ChannelClosed)?;
                    }
                    ProvEvent::ToolCall {
                        id,
                        name,
                        arguments,
                    } => {
                        tx.send(AgentEvent::ToolCall {
                            id: id.clone(),
                            name: name.clone(),
                            args: arguments.clone(),
                        })
                        .await
                        .map_err(|_| CoreError::ChannelClosed)?;
                        calls.push(CanonicalToolCall {
                            id,
                            name,
                            arguments,
                        });
                    }
                    ProvEvent::ToolCallEnd { .. } => {}
                    ProvEvent::Finish { .. } => {}
                    ProvEvent::Error { message } => {
                        return Err(rc_pro::ProviderError::Config(message).into())
                    }
                }
            }
            if !text.trim().is_empty() {
                parts.push(text.trim().to_string());
                review_messages.push(CanonicalMessage::assistant_text(text));
            }
            if calls.is_empty() {
                break;
            }
            review_messages.push(CanonicalMessage::assistant_tool_calls(calls.clone()));
            for call in &calls {
                let allowed = matches!(
                    call.name.as_str(),
                    "read_file" | "list_dir" | "grep" | "skill" | "web_fetch" | "web_search"
                );
                let result = if allowed {
                    self.execute_tool(session_id, tx, call, &review_tool_names).await?
                } else {
                    let result = ToolResult::err("tool not allowed in review phase");
                    self.send_tool_result(tx, call, &result).await?;
                    result
                };
                review_messages.push(CanonicalMessage::tool(
                    call.id.clone(),
                    call.name.clone(),
                    result.output.clone(),
                ));
            }
        }
        let text = if parts.is_empty() {
            "Review: no verdict produced.".to_string()
        } else {
            parts.join("\n\n")
        };
        let (verdict, reason, next_intent) = parse_review_outcome(&text);
        {
            let store = self.store.lock().map_err(|_| CoreError::LockPoisoned)?;
            store.append_message(session_id, MessageRole::Assistant, &text)?;
        }
        Ok(ReviewOutcome {
            verdict,
            reason,
            next_intent,
            text,
        })
    }

    fn compact_messages(&self, mut messages: Vec<CanonicalMessage>) -> Vec<CanonicalMessage> {
        let Some(max_bytes) = self.max_history_bytes else {
            return messages;
        };
        let message_size = |m: &CanonicalMessage| -> usize {
            serde_json::to_string(m)
                .map(|s| s.len())
                .unwrap_or_else(|_| m.text().len())
        };
        let mut total: usize = messages.iter().map(message_size).sum();
        while total > max_bytes && messages.len() > 2 {
            // 不要留下"孤儿 tool 结果":assistant_tool_calls 与紧跟其 tool() 结果
            // 必须成组移除。若预算边界落在二者之间,请求里会残留一个 tool 角色消息,
            // 其引用的 tool_call_id 已不存在 → provider 400。整组从最前移除。
            let is_tool_call_group =
                messages[1].role == CanonicalRole::Assistant && !messages[1].tool_calls.is_empty();
            let removed = messages.remove(1);
            total = total.saturating_sub(message_size(&removed));
            if is_tool_call_group {
                while messages.len() > 1 && messages[1].role == CanonicalRole::Tool {
                    let removed = messages.remove(1);
                    total = total.saturating_sub(message_size(&removed));
                }
            }
        }
        messages
    }

    fn tool_defs(&self) -> Vec<ToolDef> {
        if let Ok(cache) = self.tool_cache.lock() {
            if let Some(cached) = cache.as_ref() {
                return cached.clone();
            }
        }
        let defs: Vec<ToolDef> = self
            .tools
            .specs()
            .into_iter()
            .map(|spec| ToolDef {
                name: spec.name,
                description: spec.description,
                input_schema: spec.input_schema,
            })
            .collect();
        if let Ok(mut cache) = self.tool_cache.lock() {
            *cache = Some(defs.clone());
        }
        defs
    }

    async fn execute_tool(
        &self,
        session_id: &str,
        tx: &Sender<AgentEvent>,
        call: &CanonicalToolCall,
        snapshot: &[String],
    ) -> Result<ToolResult, CoreError> {
        // Stale 拒绝:工具名不在本轮快照 → 直接拒绝,不执行工具本体。
        // 模型可能重复上一轮/本轮已移除的工具调用,避免让这些 stale 调用真实运行。
        if !snapshot.contains(&call.name) {
            let result = ToolResult::err(format!(
                "Stale tool call: `{}` is not in this turn's tool set. Do not call it again.",
                call.name
            ));
            self.send_tool_result(tx, call, &result).await?;
            return Ok(result);
        }
        let pre_payload = json!({
            "phase": "pre_tool",
            "tool": call.name,
            "tool_call_id": call.id,
            "args": call.arguments,
        });
        for command in &self.hooks.pre_tool {
            let output = match run_hook(command, &pre_payload, &self.cwd).await {
                Ok(output) => output,
                Err(error) => {
                    let result = ToolResult::err(format!("pre_tool hook failed: {error}"));
                    self.send_tool_result(tx, call, &result).await?;
                    return Ok(result);
                }
            };
            let verdict = hook_decision(&output);
            if !verdict.allow {
                let result = ToolResult::err(format!("pre_tool hook denied: {}", verdict.reason));
                self.send_tool_result(tx, call, &result).await?;
                return Ok(result);
            }
        }

        // 取消检查点:进入工具执行前先看取消(挂死工具由下方的 timeout 兜底)。
        if self.cancelled() {
            return Err(CoreError::Cancelled);
        }
        let mut ctx = ToolContext::new(self.cwd.clone(), self.approval.clone());
        ctx.command_policy = self.command_policy.clone();
        ctx.network_policy = self.network_policy.clone();
        ctx.user_input = self.user_input.clone();
        ctx.timeout = self.tool_timeout;
        ctx.subagent = self.subagent.clone();
        // 监督守卫闸:所有 agent(主模型/plan/review/子代理)的工具执行都过 guard_check,
        // 不可被绕过。有 hook(TUI)时高危操作弹四选一;无 hook 时保守拦截。
        ctx = ctx.with_guard(
            self.guard_cfg.clone(),
            self.guard_hook.clone(),
            self.guard_memo.clone(),
        );
        ctx.supervise_dir = self.guard_home.clone();
        if call.name == "ask_user" {
            let question = call
                .arguments
                .get("question")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            tx.send(AgentEvent::AskingQuestion {
                id: call.id.clone(),
                question,
                session_id: session_id.to_string(),
            })
            .await
            .map_err(|_| CoreError::ChannelClosed)?;
        }
        if call.name == "run_shell" {
            let description = call
                .arguments
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            tx.send(AgentEvent::AskingApproval {
                id: call.id.clone(),
                tool: call.name.clone(),
                description,
            })
            .await
            .map_err(|_| CoreError::ChannelClosed)?;
        }
        // ctx.timeout 真正生效:ToolRegistry::run 本身不强制超时,挂在挂死工具
        // (如 delegate_research 子代理无应答)会让整个 run 永久卡住且 cancel() 无效。
        // 用 tokio::time::timeout 强制工具级 deadline;超时返回错误结果并继续。
        let result = match tokio::time::timeout(
            ctx.timeout,
            self.tools.run(&call.name, call.arguments.clone(), &ctx),
        )
        .await
        {
            Ok(result) => result,
            Err(_elapsed) => {
                let result = ToolResult::err(format!(
                    "tool '{}' timed out after {}s",
                    call.name,
                    ctx.timeout.as_secs()
                ));
                self.send_tool_result(tx, call, &result).await?;
                return Ok(result);
            }
        };
        // 工具返回后再次检查取消:若用户在工具执行期间中断,直接收尾返回。
        if self.cancelled() {
            return Err(CoreError::Cancelled);
        }
        if call.name == "skill" && result.ok {
            if let Some(name) = call.arguments.get("name").and_then(Value::as_str) {
                if let Some(skill) = self.skill_store.load(name) {
                    tx.send(AgentEvent::SkillLoaded {
                        name: skill.name.clone(),
                        path: skill.path.to_string_lossy().to_string(),
                    })
                    .await
                    .map_err(|_| CoreError::ChannelClosed)?;
                    if let Ok(store) = self.store.lock() {
                        let _ = store.upsert_skill(&skill.to_row());
                        let _ = store.bump_skill_usage(&skill.name, result.ok);
                    }
                }
            }
        }

        // 工具输出有界化:>50KB 持久化到托管目录 + head/marker/tail 预览替换,
        // 完整内容路径随事件带出(UI 可抓取)。state_path 是文件(~/.raincode/state.db),
        // 输出目录建在其父目录下。
        let bound = {
            let dir = self
                .state_path
                .parent()
                .unwrap_or(&self.state_path)
                .join("tool_output");
            rc_tool::tool_output::ToolOutputStore::new(dir).bound(&call.id, &result.output)
        };
        let mut result = result;
        result.output = bound.text;
        if let Some(path) = bound.path {
            result.output_path = Some(path);
        }

        let post_payload = json!({
            "phase": "post_tool",
            "tool": call.name,
            "tool_call_id": call.id,
            "args": call.arguments,
            "ok": result.ok,
            "output": result.output,
        });
        for command in &self.hooks.post_tool {
            match run_hook(command, &post_payload, &self.cwd).await {
                Ok(output) if !output.ok => tracing::warn!(
                    "post_tool hook `{command}` exited {}: {}",
                    output.code.unwrap_or(-1),
                    output.stderr
                ),
                Ok(_) => {}
                Err(error) => tracing::warn!("post_tool hook `{command}` failed: {error}"),
            }
        }
        self.send_tool_result(tx, call, &result).await?;
        Ok(result)
    }

    async fn send_tool_result(
        &self,
        tx: &Sender<AgentEvent>,
        call: &CanonicalToolCall,
        result: &ToolResult,
    ) -> Result<(), CoreError> {
        tx.send(AgentEvent::ToolResult {
            id: call.id.clone(),
            name: call.name.clone(),
            ok: result.ok,
            output: result.output.clone(),
            output_path: result.output_path.clone(),
        })
        .await
        .map_err(|_| CoreError::ChannelClosed)
    }

    async fn run_session_hook(&self, session_id: &str, phase: &str) {
        let commands = match phase {
            "session_start" => &self.hooks.session_start,
            "session_end" => &self.hooks.session_end,
            _ => return,
        };
        let payload = session_payload(phase, session_id);
        for command in commands {
            if let Err(error) = run_hook(command, &payload, &self.cwd).await {
                tracing::warn!("{phase} hook `{command}` failed: {error}");
            }
        }
    }

    fn agent_instruction(&self, name: &str) -> String {
        let local = self.cwd.join("AGENTS").join(format!("{name}.md"));
        if let Ok(text) = std::fs::read_to_string(&local) {
            return text;
        }
        let raincode_home = std::env::var_os("RAINCODE_HOME")
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|home| home.join(".raincode")));
        if let Some(home) = raincode_home {
            let user = home.join("agents").join(format!("{name}.md"));
            if let Ok(text) = std::fs::read_to_string(&user) {
                return text;
            }
        }
        builtin_agent_prompt(name)
            .map(str::to_string)
            .unwrap_or_else(|| format!("You are acting as the `{name}` agent profile.\n"))
    }

    async fn evolve(&self, session_id: &str) {
        let store = match Store::open(&self.state_path) {
            Ok(store) => store,
            Err(e) => {
                tracing::warn!("evolve: cannot reopen state: {e}");
                return;
            }
        };
        let skill_store = SkillStore::new(self.skill_store.root());
        let mut engine = EvolveEngine::new(
            self.provider.clone(),
            store,
            skill_store,
            EvolveConfig::default(),
        );
        match engine.digest(session_id).await {
            Ok(report) => tracing::info!(
                "evolve {}: {:?} ({} matched)",
                session_id,
                report.action,
                report.matched_skill.is_some()
            ),
            Err(e) => tracing::warn!("evolve failed for {session_id}: {e}"),
        }
    }
}

fn builtin_agent_prompt(name: &str) -> Option<&'static str> {
    match name {
        "coding" => Some(include_str!("../../../agents/coding.md")),
        "architect" => Some(include_str!("../../../agents/architect.md")),
        "researcher" => Some(include_str!("../../../agents/researcher.md")),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReviewVerdict {
    Accept,
    Reject,
}

impl ReviewVerdict {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Accept => "accept",
            Self::Reject => "reject",
        }
    }
}

struct ReviewOutcome {
    verdict: ReviewVerdict,
    reason: String,
    next_intent: String,
    text: String,
}

/// 把路由选出的相关 skill 拼成模型可见提示(动态尾,不进稳定 system 前缀)。
/// 鼓励模型按需加载 skill → 产生 usage → darwinian 演化才有数据可学。
fn relevant_skills_hint(skills: &[SkillSummary]) -> Option<String> {
    if skills.is_empty() {
        return None;
    }
    let hint = skills
        .iter()
        .map(|s| format!("- `{}`: {}", s.name, s.description))
        .collect::<Vec<_>>()
        .join("\n");
    Some(format!(
        "Highly relevant skills for this task (load one with the `skill` tool if it would help):\n{hint}"
    ))
}

fn content_hash(text: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Best-effort token count from a provider's `Finish.usage` payload.
/// Prefers `total_tokens`; falls back to `input_tokens + output_tokens`.
fn context_used_tokens(usage: &Value) -> u64 {
    if let Some(total) = usage.get("total_tokens").and_then(Value::as_u64) {
        return total;
    }
    let input = usage
        .get("input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output = usage
        .get("output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    input.saturating_add(output)
}

fn parse_review_outcome(text: &str) -> (ReviewVerdict, String, String) {
    let mut verdict = ReviewVerdict::Reject;
    let mut reason = String::new();
    let mut next_intent = String::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let upper = line.to_uppercase();
        if upper.starts_with("VERDICT:") {
            if upper.contains("ACCEPT") {
                verdict = ReviewVerdict::Accept;
            } else if upper.contains("REJECT") {
                verdict = ReviewVerdict::Reject;
            }
        } else if upper.starts_with("REASON:") {
            reason = line[7..].trim().to_string();
        } else if upper.starts_with("NEXT_USER_INTENT:") {
            next_intent = line[17..].trim().to_string();
        }
    }
    if reason.is_empty() {
        reason = "Review produced no explicit reason.".to_string();
    }
    (verdict, reason, next_intent)
}

fn parse_tool_call_text(text: &str) -> Option<(String, String, serde_json::Value)> {
    let rest = text.strip_prefix("tool_call ")?;
    let mut parts = rest.splitn(3, ' ');
    let id = parts.next()?.to_string();
    let name = parts.next()?.to_string();
    // 存储格式(tool_call id name <json>)里 arguments 永远是序列化 JSON。
    // 要求严格 JSON:模型正文若恰好以 "tool_call x y" 开头(非 JSON 参数)不会
    // 在 resume 时被误重建为工具调用,污染角色/工具映射。
    let arguments = serde_json::from_str(parts.next()?).ok()?;
    Some((id, name, arguments))
}

fn parse_tool_result_text(text: &str) -> (String, String) {
    match text.split_once(':') {
        Some((name, output)) => (name.trim().to_string(), output.trim().to_string()),
        None => ("tool".to_string(), text.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rc_pro::mock::MockProvider;
    use rc_pro::ProviderConfig;
    use rc_proto::EventKind;
    use rc_sandbox::{AutoApproveHook, AutoUserHook};
    use rc_skill::Skill;
    use rc_tool::builtin::default_tools;

    #[test]
    fn steer_hub_register_send_routes_to_matching_agent() {
        let hub = SteerHub::new();
        let mut rx = hub.register("s1");
        assert!(hub.send("s1", "改用 pytest"));
        assert!(!hub.send("nope", "x")); // 未注册
        assert_eq!(rx.try_recv().unwrap(), "改用 pytest");
        hub.unregister("s1");
        assert!(!hub.send("s1", "y"));
    }

    fn seed_skill(store: &SkillStore, name: &str, category: &str) {
        let skill = Skill {
            name: name.into(),
            description: format!("method for {name}"),
            short_description: None,
            category: category.into(),
            path: PathBuf::new(),
            body: format!("# {name}\n\nFollow the reusable method.\n"),
            relations: vec![],
            triggers: vec![name.into()],
            tags: vec![],
            version: 1,
            confidence: 0.9,
            usage_count: 0,
            success_rate: 0.0,
            last_used: None,
            auto: false,
            origin: "test".into(),
            origin_url: None,
            scope: "system".into(),
            allow_implicit: true,
            embedding: None,
        };
        store.save(&skill).unwrap();
    }

    fn mock_agent(
        tmp: &tempfile::TempDir,
        script: Vec<serde_json::Value>,
        plan: bool,
    ) -> (Agent, Store, String) {
        mock_agent_with(tmp, script, plan, HooksConfig::default())
    }

    fn mock_agent_with(
        tmp: &tempfile::TempDir,
        script: Vec<serde_json::Value>,
        plan: bool,
        hooks: HooksConfig,
    ) -> (Agent, Store, String) {
        let store = Store::open(tmp.path().join("state.db")).unwrap();
        let session = store.create_session(".").unwrap();
        let skill_dir = tmp.path().join("skills");
        let skill_store = SkillStore::new(&skill_dir);
        seed_skill(&skill_store, "shell-method", "shell");
        let cfg = ProviderConfig {
            kind: "mock".into(),
            base_url: String::new(),
            model: "mock-1".into(),
            api_key: None,
            api_key_env: None,
            embedding_model: None,
            headers: Default::default(),
            extra: json!({"script": script, "auto_advance": true}),
        };
        let agent = Agent::new(AgentConfig {
            provider: Arc::new(MockProvider::new(cfg, "mock-1".into())),
            plan_provider: None,
            review_provider: None,
            store,
            skill_store: skill_store.clone(),
            tools: default_tools(skill_store),
            approval: Arc::new(AutoApproveHook),
            command_policy: CommandPolicy::default(),
            network_policy: NetworkPolicy::default(),
            cwd: tmp.path().to_path_buf(),
            state_path: tmp.path().join("state.db"),
            max_turns: 4,
            max_steps: 0,
            max_history_bytes: Some(64 * 1024),
            mcp_servers: vec![("mock-mcp".into(), vec!["echo".into()])],
            entropy_mode: false,
            plan_max_rounds: 6,
            plan_max_questions: 5,
            review_max_rounds: 3,
            max_cycles: 1,
            user_input: Arc::new(AutoUserHook::default()),
            steer_rx: None,
            context_window: 0,
            subagent: None,
            guard_cfg: None,
            guard_hook: None,
            guard_memo: None,
            guard_home: None,
            evolve_on_finish: false,
            plan_mode: plan,
            hooks,
            agent: Some("coding".into()),
        });
        (
            agent,
            Store::open(tmp.path().join("state.db")).unwrap(),
            session.id,
        )
    }

    #[tokio::test]
    async fn run_emits_mcp_approval_tool_and_done_events() {
        let tmp = tempfile::tempdir().unwrap();
        let (agent, _store, session_id) = mock_agent(
            &tmp,
            vec![
                json!({"type": "text", "text": "Running the method."}),
                json!({"type": "tool", "name": "run_shell", "arguments": {"command": "echo hi"}}),
                json!({"type": "done", "stop_reason": "end_turn"}),
            ],
            false,
        );
        let mut events = Vec::new();
        let mut stream = agent.run(session_id, "run the shell method".into());
        while let Some(event) = stream.next().await {
            let done = matches!(event, AgentEvent::Done { .. });
            events.push(event);
            if done {
                break;
            }
        }
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::McpToolList { server, .. } if server == "mock-mcp")));
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::AskingApproval { tool, .. } if tool == "run_shell")));
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolResult { name, .. } if name == "run_shell")));
        assert!(events.iter().any(|e| matches!(e, AgentEvent::Done { .. })));
    }

    #[tokio::test]
    async fn agent_guard_is_plumbed_to_tool_execution() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        let session = store.create_session(".").unwrap();
        let skill_dir = tmp.path().join("skills");
        let skill_store = SkillStore::new(&skill_dir);
        seed_skill(&skill_store, "shell-method", "shell");
        let cfg = ProviderConfig {
            kind: "mock".into(),
            base_url: String::new(),
            model: "mock-1".into(),
            api_key: None,
            api_key_env: None,
            embedding_model: None,
            headers: Default::default(),
            extra: json!({
                "script": [
                    json!({"type": "text", "text": "Running the method."}),
                    json!({"type": "tool", "name": "run_shell", "arguments": {"command": "rm -rf /etc"}}),
                    json!({"type": "done", "stop_reason": "end_turn"}),
                ],
                "auto_advance": true
            }),
        };
        // 守卫配置挂在 AgentConfig(而非仅测试工具层):deny 命令 rm -rf →
        // guard_check 返回 NeedsUserApproval → 无 hook → 保守拦截。
        // 证明守卫已从 AgentConfig 经 execute_tool 通到真实工具执行点。
        let guard_cfg = rc_sandbox::guard::SuperviseConfig {
            deny: rc_sandbox::guard::DenyRules {
                commands: vec!["rm -rf".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        let agent = Agent::new(AgentConfig {
            provider: Arc::new(MockProvider::new(cfg, "mock-1".into())),
            plan_provider: None,
            review_provider: None,
            store,
            skill_store: skill_store.clone(),
            tools: default_tools(skill_store),
            approval: Arc::new(AutoApproveHook),
            command_policy: CommandPolicy::default(),
            network_policy: NetworkPolicy::default(),
            cwd: tmp.path().to_path_buf(),
            state_path: tmp.path().join("state.db"),
            max_turns: 4,
            max_steps: 0,
            max_history_bytes: Some(64 * 1024),
            mcp_servers: vec![],
            entropy_mode: false,
            plan_max_rounds: 6,
            plan_max_questions: 5,
            review_max_rounds: 3,
            max_cycles: 1,
            user_input: Arc::new(AutoUserHook::default()),
            steer_rx: None,
            context_window: 0,
            subagent: None,
            guard_cfg: Some(guard_cfg),
            guard_hook: None,
            guard_memo: Some(Arc::new(
                rc_sandbox::guard_hook::SessionGuardMemo::default(),
            )),
            guard_home: Some(tmp.path().to_path_buf()),
            evolve_on_finish: false,
            plan_mode: false,
            hooks: HooksConfig::default(),
            agent: Some("coding".into()),
        });
        let mut events = Vec::new();
        let mut stream = agent.run(session.id, "run the shell method".into());
        while let Some(event) = stream.next().await {
            let done = matches!(event, AgentEvent::Done { .. });
            events.push(event);
            if done {
                break;
            }
        }
        let blocked: Vec<(bool, String)> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ToolResult { name, ok, output, .. } if name == "run_shell" => {
                    Some((*ok, output.clone()))
                }
                _ => None,
            })
            .collect();
        assert!(!blocked.is_empty(), "expected a run_shell ToolResult");
        let (ok, output) = &blocked[0];
        assert!(!ok, "guard must block the denied command");
        assert!(output.contains("guard"), "output: {output}");
    }

    #[tokio::test]
    async fn cancel_stops_run_with_cancelled_error() {
        let tmp = tempfile::tempdir().unwrap();
        let (agent, _store, session_id) = mock_agent(
            &tmp,
            vec![
                json!({"type": "text", "text": "first"}),
                json!({"type": "sleep", "ms": 200}),
                json!({"type": "text", "text": "second"}),
                json!({"type": "sleep", "ms": 200}),
                json!({"type": "text", "text": "third"}),
            ],
            false,
        );
        let mut stream = agent.run(session_id, "run something".into());
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        agent.cancel();
        let mut cancelled = false;
        while let Some(event) = stream.next().await {
            if let AgentEvent::Error { message } = &event {
                if message == "cancelled by user" {
                    cancelled = true;
                    break;
                }
            }
        }
        assert!(cancelled, "expected 'cancelled by user' error event");
    }

    #[tokio::test]
    async fn mock_run_writes_file_and_emits_ordered_events() {
        let tmp = tempfile::tempdir().unwrap();
        let (agent, _store, session_id) = mock_agent(
            &tmp,
            vec![
                json!({"type": "text", "text": "Writing the file."}),
                json!({
                    "type": "tool",
                    "name": "write_file",
                    "arguments": {"path": "hello.txt", "content": "hello raincode"}
                }),
                json!({"type": "done", "stop_reason": "end_turn"}),
            ],
            false,
        );
        let mut events = Vec::new();
        let mut stream = agent.run(session_id, "write hello.txt".into());
        while let Some(event) = stream.next().await {
            let done = matches!(event, AgentEvent::Done { .. });
            events.push(event);
            if done {
                break;
            }
        }
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("hello.txt")).unwrap(),
            "hello raincode"
        );
        let kinds: Vec<EventKind> = events.iter().map(|e| e.kind()).collect();
        let tool_idx = kinds
            .iter()
            .position(|k| *k == EventKind::ToolCall)
            .unwrap();
        let result_idx = kinds
            .iter()
            .position(|k| *k == EventKind::ToolResult)
            .unwrap();
        let done_idx = kinds.iter().position(|k| *k == EventKind::Done).unwrap();
        assert!(tool_idx < result_idx && result_idx < done_idx);
    }
    #[tokio::test]
    async fn run_emits_skill_loaded_when_skill_tool_used() {
        let tmp = tempfile::tempdir().unwrap();
        let (agent, store, session_id) = mock_agent(
            &tmp,
            vec![
                json!({"type": "text", "text": "Loading the method."}),
                json!({"type": "tool", "name": "skill", "arguments": {"name": "shell-method"}}),
                json!({"type": "done", "stop_reason": "end_turn"}),
            ],
            false,
        );
        let mut events = Vec::new();
        let mut stream = agent.run(session_id, "apply the shell method".into());
        while let Some(event) = stream.next().await {
            let done = matches!(event, AgentEvent::Done { .. });
            events.push(event);
            if done {
                break;
            }
        }
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::SkillLoaded { name, .. } if name == "shell-method")));
        let row = store.get_skill("shell-method").unwrap().unwrap();
        assert_eq!(row.usage_count, 1);
        assert_eq!(row.success_count, 1);
    }

    #[tokio::test]
    async fn run_caches_task_embedding_in_state() {
        let tmp = tempfile::tempdir().unwrap();
        let (agent, store, session_id) = mock_agent(
            &tmp,
            vec![json!({"type": "done", "stop_reason": "end_turn"})],
            false,
        );
        let task = "apply the shell method";
        let mut stream = agent.run(session_id, task.to_string());
        while let Some(event) = stream.next().await {
            if matches!(event, AgentEvent::Done { .. }) {
                break;
            }
        }
        assert!(store
            .get_embedding(&content_hash(&format!("mock-1|{task}")))
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn plan_mode_emits_plan_and_skips_tools() {
        let tmp = tempfile::tempdir().unwrap();
        let (agent, _store, session_id) = mock_agent(
            &tmp,
            vec![
                json!({"type": "text", "text": "1. Read.\n2. Fix."}),
                json!({"type": "done", "stop_reason": "end_turn"}),
            ],
            true,
        );
        let mut events = Vec::new();
        let mut stream = agent.run(session_id, "plan a fix".into());
        while let Some(event) = stream.next().await {
            let done = matches!(event, AgentEvent::Done { .. });
            events.push(event);
            if done {
                break;
            }
        }
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::PlanProposed { .. })));
        assert!(!events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolCall { .. })));
    }

    #[tokio::test]
    async fn entropy_cycle_plans_executes_reviews_and_retries() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        let session = store.create_session(".").unwrap();
        let skill_dir = tmp.path().join("skills");
        let skill_store = SkillStore::new(&skill_dir);
        seed_skill(&skill_store, "shell-method", "shell");
        let plan_cfg = ProviderConfig {
            kind: "mock".into(),
            base_url: String::new(),
            model: "plan".into(),
            api_key: None,
            api_key_env: None,
            embedding_model: None,
            headers: Default::default(),
            extra: json!({
                "auto_advance": false,
                "script_sequence": [[
                    {"type": "text", "text": "Plan: read, fix, verify."},
                    {"type": "done", "stop_reason": "end_turn"}
                ]]
            }),
        };
        let execute_cfg = ProviderConfig {
            kind: "mock".into(),
            base_url: String::new(),
            model: "execute".into(),
            api_key: None,
            api_key_env: None,
            embedding_model: None,
            headers: Default::default(),
            extra: json!({
                "auto_advance": false,
                "script_sequence": [[
                    {"type": "text", "text": "Executing the plan."},
                    {"type": "done", "stop_reason": "end_turn"}
                ]]
            }),
        };
        let review_cfg = ProviderConfig {
            kind: "mock".into(),
            base_url: String::new(),
            model: "review".into(),
            api_key: None,
            api_key_env: None,
            embedding_model: None,
            headers: Default::default(),
            extra: json!({
                "auto_advance": false,
                "script_sequence": [
                    [
                        {"type": "text", "text": "VERDICT: REJECT\nREASON: verification missing\nNEXT_USER_INTENT: add a check and rerun"},
                        {"type": "done", "stop_reason": "end_turn"}
                    ],
                    [
                        {"type": "text", "text": "VERDICT: ACCEPT\nREASON: verification added"},
                        {"type": "done", "stop_reason": "end_turn"}
                    ]
                ]
            }),
        };
        let agent = Agent::new(AgentConfig {
            provider: Arc::new(MockProvider::new(execute_cfg, "execute".into())),
            plan_provider: Some(Arc::new(MockProvider::new(plan_cfg, "plan".into()))),
            review_provider: Some(Arc::new(MockProvider::new(review_cfg, "review".into()))),
            store,
            skill_store: skill_store.clone(),
            tools: default_tools(skill_store),
            approval: Arc::new(AutoApproveHook),
            command_policy: CommandPolicy::default(),
            network_policy: NetworkPolicy::default(),
            cwd: tmp.path().to_path_buf(),
            state_path: tmp.path().join("state.db"),
            max_turns: 4,
            max_steps: 0,
            max_history_bytes: Some(64 * 1024),
            mcp_servers: vec![],
            evolve_on_finish: false,
            plan_mode: false,
            hooks: HooksConfig::default(),
            agent: Some("coding".into()),
            entropy_mode: true,
            plan_max_rounds: 3,
            plan_max_questions: 3,
            review_max_rounds: 2,
            max_cycles: 2,
            user_input: Arc::new(AutoUserHook::default()),
            steer_rx: None,
            context_window: 0,
            subagent: None,
            guard_cfg: None,
            guard_hook: None,
            guard_memo: None,
            guard_home: None,
});
        let mut events = Vec::new();
        let mut stream = agent.run(session.id, "fix the failing test".into());
        while let Some(event) = stream.next().await {
            let done = matches!(event, AgentEvent::Done { .. });
            events.push(event);
            if done {
                break;
            }
        }
        let phases: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::PhaseChanged { phase, .. } => Some(phase.clone()),
                _ => None,
            })
            .collect();
        assert!(phases
            .windows(2)
            .any(|w| w[0] == "plan" && w[1] == "execute"));
        assert!(phases
            .windows(2)
            .any(|w| w[0] == "execute" && w[1] == "review"));
        assert!(phases.contains(&"re-understand".to_string()));
        let reviews: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ReviewProposed { verdict, .. } => Some(verdict.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(reviews, vec!["reject", "accept"]);
        let done = events
            .iter()
            .find_map(|e| match e {
                AgentEvent::Done { summary, .. } => Some(summary.clone()),
                _ => None,
            })
            .unwrap();
        assert!(done.contains("Review"));
    }

    #[tokio::test]
    async fn run_emits_context_update_from_provider_usage() {
        let tmp = tempfile::tempdir().unwrap();
        let (agent, _store, session_id) = mock_agent(
            &tmp,
            vec![
                json!({"type": "text", "text": "Working."}),
                json!({"type": "tool", "name": "run_shell", "arguments": {"command": "echo hi"}}),
                json!({"type": "done", "stop_reason": "end_turn", "usage": {"total_tokens": 4200}}),
            ],
            false,
        );
        let mut events = Vec::new();
        let mut stream = agent.run(session_id, "run the shell method".into());
        while let Some(event) = stream.next().await {
            let done = matches!(event, AgentEvent::Done { .. });
            events.push(event);
            if done {
                break;
            }
        }
        let updates: Vec<(u64, u64, u8)> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ContextUpdate { used, limit, pct, .. } => Some((*used, *limit, *pct)),
                _ => None,
            })
            .collect();
        assert!(!updates.is_empty(), "expected at least one ContextUpdate");
        let (used, limit, _pct) = updates[updates.len() - 1];
        assert_eq!(used, 4200);
        assert_eq!(limit, 128_000);
    }

    #[tokio::test]
    async fn run_accumulates_context_usage_across_turns() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        let session = store.create_session(".").unwrap();
        let skill_dir = tmp.path().join("skills");
        let skill_store = SkillStore::new(&skill_dir);
        let cfg = ProviderConfig {
            kind: "mock".into(),
            base_url: String::new(),
            model: "execute".into(),
            api_key: None,
            api_key_env: None,
            embedding_model: None,
            headers: Default::default(),
            extra: json!({
                "auto_advance": false,
                "script_sequence": [
                    [
                        {"type": "text", "text": "Turn one."},
                        {"type": "tool", "name": "list_dir", "arguments": {"path": "."}},
                        {"type": "done", "stop_reason": "tool_calls", "usage": {"input_tokens": 1000, "output_tokens": 500}}
                    ],
                    [
                        {"type": "text", "text": "Turn two."},
                        {"type": "tool", "name": "list_dir", "arguments": {"path": "."}},
                        {"type": "done", "stop_reason": "tool_calls", "usage": {"total_tokens": 2500}}
                    ],
                    [
                        {"type": "text", "text": "Done."},
                        {"type": "done", "stop_reason": "end_turn", "usage": {"total_tokens": 300}}
                    ]
                ]
            }),
        };
        let agent = Agent::new(AgentConfig {
            provider: Arc::new(MockProvider::new(cfg, "execute".into())),
            plan_provider: None,
            review_provider: None,
            store,
            skill_store: skill_store.clone(),
            tools: default_tools(skill_store),
            approval: Arc::new(AutoApproveHook),
            command_policy: CommandPolicy::default(),
            network_policy: NetworkPolicy::default(),
            cwd: tmp.path().to_path_buf(),
            state_path: tmp.path().join("state.db"),
            max_turns: 4,
            max_steps: 0,
            max_history_bytes: Some(64 * 1024),
            mcp_servers: vec![],
            evolve_on_finish: false,
            plan_mode: false,
            hooks: HooksConfig::default(),
            agent: Some("coding".into()),
            entropy_mode: false,
            plan_max_rounds: 3,
            plan_max_questions: 3,
            review_max_rounds: 2,
            max_cycles: 2,
            user_input: Arc::new(AutoUserHook::default()),
            steer_rx: None,
            context_window: 0,
            subagent: None,
            guard_cfg: None,
            guard_hook: None,
            guard_memo: None,
            guard_home: None,
});
        let mut events = Vec::new();
        let mut stream = agent.run(session.id, "accumulate usage".into());
        while let Some(event) = stream.next().await {
            let done = matches!(event, AgentEvent::Done { .. });
            events.push(event);
            if done {
                break;
            }
        }
        let updates: Vec<(u64, u64, u8)> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ContextUpdate { used, limit, pct, .. } => Some((*used, *limit, *pct)),
                _ => None,
            })
            .collect();
        // Turn 1: input+output fallback = 1500. Turn 2: +2500 total = 4000.
        // Turn 3: +300 total = 4300, then the turn ends (no tools -> break).
        assert_eq!(
            updates,
            vec![(1500, 128_000, 1), (4000, 128_000, 3), (4300, 128_000, 3)]
        );
    }

    /// 构造一个最小 AgentInner(测试私有字段/方法用)。路径是临时的,不要求存在。
    fn mock_inner(
        tools: ToolRegistry,
        tool_timeout: Duration,
        max_history_bytes: Option<usize>,
    ) -> AgentInner {
        let tmp = tempfile::tempdir().unwrap();
        mock_inner_at(
            tools,
            tool_timeout,
            max_history_bytes,
            tmp.path(),
            tmp.path().join("state.db"),
        )
    }

    /// mock_inner 变体:允许指定临时基目录与 state_path(用于测试 state_path
    /// 派生路径如 tool_output 的清理逻辑)。调用方需保证 base 在测试期间存活。
    fn mock_inner_at(
        tools: ToolRegistry,
        tool_timeout: Duration,
        max_history_bytes: Option<usize>,
        base: &std::path::Path,
        state_path: PathBuf,
    ) -> AgentInner {
        mock_inner_with_provider(
            Arc::new(MockProvider::new(
                ProviderConfig {
                    kind: "mock".into(),
                    base_url: String::new(),
                    model: "mock-1".into(),
                    api_key: None,
                    api_key_env: None,
                    embedding_model: None,
                    headers: Default::default(),
                    extra: json!({}),
                },
                "mock-1".into(),
            )),
            tools,
            tool_timeout,
            max_history_bytes,
            0, // max_steps:0 = 兜底用 max_turns,保持既有行为。
            base,
            state_path,
        )
    }

    /// mock_inner 变体:允许指定 provider(并发/时序测试需要自定义脚本的 provider)。
    #[allow(clippy::too_many_arguments)]
    fn mock_inner_with_provider(
        provider: Arc<dyn Provider>,
        tools: ToolRegistry,
        tool_timeout: Duration,
        max_history_bytes: Option<usize>,
        max_steps: usize,
        base: &std::path::Path,
        state_path: PathBuf,
    ) -> AgentInner {
        AgentInner {
            provider,
            plan_provider: None,
            review_provider: None,
            store: Arc::new(Mutex::new(Store::open_in_memory().unwrap())),
            skill_store: SkillStore::new(base.join("skills")),
            tools,
            approval: Arc::new(AutoApproveHook),
            command_policy: CommandPolicy::default(),
            network_policy: NetworkPolicy::default(),
            cwd: base.to_path_buf(),
            state_path,
            max_turns: 1,
            max_steps,
            max_history_bytes,
            evolve_on_finish: false,
            plan_mode: false,
            hooks: HooksConfig::default(),
            agent: None,
            mcp_servers: vec![],
            entropy_mode: false,
            plan_max_rounds: 6,
            plan_max_questions: 5,
            review_max_rounds: 3,
            max_cycles: 1,
            user_input: Arc::new(AutoUserHook::default()),
            steer_rx: Mutex::new(None),
            context_window: 0,
            tool_timeout,
            prefix: Mutex::new(None),
            tool_cache: Mutex::new(None),
            subagent: None,
            guard_cfg: None,
            guard_hook: None,
            guard_memo: None,
            guard_home: None,
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    #[test]
    fn cleanup_tool_outputs_removes_files_older_than_7_days() {
        let tmp = tempfile::tempdir().unwrap();
        // 模拟 >50KB 工具输出落盘的托管目录(state_path 父目录/tool_output)。
        let tool_dir = tmp.path().join("tool_output");
        std::fs::create_dir_all(&tool_dir).unwrap();
        let old_file = tool_dir.join("tool_old.txt");
        std::fs::write(&old_file, "stale output").unwrap();
        let fresh_file = tool_dir.join("tool_fresh.txt");
        std::fs::write(&fresh_file, "fresh output").unwrap();
        // 旧文件 mtime 拨到 8 天前;新文件保留当前 mtime。
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(8 * 86400);
        filetime::set_file_mtime(&old_file, filetime::FileTime::from_system_time(old)).unwrap();

        let inner = mock_inner_at(
            ToolRegistry::new(vec![]),
            Duration::from_secs(180),
            None,
            tmp.path(),
            tmp.path().join("state.db"),
        );
        // 生产调用点(run_loop 每次 run 开始都会调用这个 helper)。
        let removed = inner.cleanup_tool_outputs();
        assert_eq!(removed, 1, "only the 8-day-old file should be removed");
        assert!(!old_file.exists(), "old file must be cleaned up");
        assert!(fresh_file.exists(), "fresh file must be kept");
    }

    /// 永不返回的工具:execute_tool 的 tool_timeout 必须能把它截断,而不是挂死 run。
    struct HangingTool;

    #[async_trait::async_trait]
    impl rc_tool::Tool for HangingTool {
        fn spec(&self) -> rc_tool::ToolSpec {
            rc_tool::ToolSpec {
                name: "hang".into(),
                description: "never resolves".into(),
                input_schema: json!({}),
            }
        }
        async fn run(&self, _args: Value, _ctx: &rc_tool::ToolContext) -> rc_tool::ToolResult {
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn execute_tool_times_out_hung_tool() {
        let inner = mock_inner(
            ToolRegistry::new(vec![Box::new(HangingTool)]),
            Duration::from_millis(50),
            None,
        );
        let (tx, _rx) = channel(16);
        let call = CanonicalToolCall {
            id: "c1".into(),
            name: "hang".into(),
            arguments: json!({}),
        };
        let started = std::time::Instant::now();
        let snapshot: Vec<String> = vec!["hang".into()];
        let result = inner.execute_tool("s1", &tx, &call, &snapshot).await.unwrap();
        assert!(!result.ok, "hung tool must time out with an error result");
        assert!(result.output.contains("timed out"), "output: {}", result.output);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "execute_tool should return after the tool_timeout, not hang forever"
        );
    }

    /// 固定 sleep 100ms 的工具:并发测试用两个不同名的实例(slow1/slow2)验证
    /// run_execute_phase 的并行执行(串行 ≥200ms,并行 <190ms)。
    struct SlowTool {
        name: &'static str,
    }

    #[async_trait::async_trait]
    impl rc_tool::Tool for SlowTool {
        fn spec(&self) -> rc_tool::ToolSpec {
            rc_tool::ToolSpec {
                name: self.name.into(),
                description: "sleeps 100ms then succeeds".into(),
                input_schema: json!({}),
            }
        }
        async fn run(&self, _args: Value, _ctx: &rc_tool::ToolContext) -> rc_tool::ToolResult {
            tokio::time::sleep(Duration::from_millis(100)).await;
            rc_tool::ToolResult::ok("slow done")
        }
    }

    /// 输出 >50KB 的工具:execute_tool 必须持久化到 tool_output 目录并替换为预览,
    /// 完整路径同时出现在 ToolResult 和 AgentEvent::ToolResult 上。
    struct HugeOutputTool;

    #[async_trait::async_trait]
    impl rc_tool::Tool for HugeOutputTool {
        fn spec(&self) -> rc_tool::ToolSpec {
            rc_tool::ToolSpec {
                name: "huge".into(),
                description: "returns 60KB".into(),
                input_schema: json!({}),
            }
        }
        async fn run(&self, _args: Value, _ctx: &rc_tool::ToolContext) -> rc_tool::ToolResult {
            rc_tool::ToolResult::ok("z".repeat(60 * 1024))
        }
    }

    #[tokio::test]
    async fn execute_tool_persists_large_output_and_carries_path() {
        let inner = mock_inner(
            ToolRegistry::new(vec![Box::new(HugeOutputTool)]),
            Duration::from_secs(10),
            None,
        );
        let (tx, mut rx) = channel(16);
        let call = CanonicalToolCall {
            id: "c1".into(),
            name: "huge".into(),
            arguments: json!({}),
        };
        let snapshot: Vec<String> = vec!["huge".into()];
        let result = inner.execute_tool("s1", &tx, &call, &snapshot).await.unwrap();
        assert!(result.ok);
        let path = result
            .output_path
            .clone()
            .expect("large output must persist");
        assert!(
            result.output.len() < rc_tool::tool_output::MAX_INLINE_BYTES,
            "model-visible text must stay bounded"
        );
        assert!(result.output.contains("truncated; full content saved to"));
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert_eq!(on_disk.len(), 60 * 1024, "full output on disk");
        match rx.try_recv().unwrap() {
            AgentEvent::ToolResult { output_path, .. } => {
                assert_eq!(output_path.as_deref(), Some(path.as_str()));
            }
            other => panic!("expected ToolResult event, got {other:?}"),
        }
    }

    /// 可计数调用的工具:stale 拒绝测试用它验证工具本体在拒绝分支下不会执行。
    #[derive(Default)]
    struct CountingTool {
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl rc_tool::Tool for CountingTool {
        fn spec(&self) -> rc_tool::ToolSpec {
            rc_tool::ToolSpec {
                name: "count".into(),
                description: "count calls".into(),
                input_schema: json!({"type": "object"}),
            }
        }
        async fn run(
            &self,
            _args: serde_json::Value,
            _ctx: &ToolContext,
        ) -> rc_tool::ToolResult {
            self.calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            rc_tool::ToolResult::ok("counted")
        }
    }

    /// 捕获收到的请求:max_steps 守卫测试用它断言最后一步 tools 为空 +
    /// MAX STEPS 消息已进入发给 provider 的 messages。
    struct CapturingProvider {
        id: String,
        requests: Mutex<Vec<CanonicalRequest>>,
    }

    impl CapturingProvider {
        fn new(id: &str) -> Self {
            Self {
                id: id.into(),
                requests: Mutex::new(Vec::new()),
            }
        }

        fn requests(&self) -> Vec<CanonicalRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl Provider for CapturingProvider {
        fn id(&self) -> &str {
            &self.id
        }

        async fn stream(
            &self,
            req: CanonicalRequest,
        ) -> Result<rc_pro::provider::ProvStream, rc_pro::ProviderError> {
            self.requests.lock().unwrap().push(req);
            // 纯文本回复(无工具调用)→ run_execute_phase 本轮结束 break,循环干净终止。
            let events = vec![
                Ok(ProvEvent::Delta {
                    text: "Max steps reached; wrapping up.".into(),
                }),
                Ok(ProvEvent::Finish {
                    stop_reason: "end_turn".into(),
                    usage: None,
                }),
            ];
            Ok(Box::pin(futures::stream::iter(events)))
        }

        async fn embed(
            &self,
            texts: Vec<String>,
        ) -> Result<Vec<Vec<f32>>, rc_pro::ProviderError> {
            Ok(texts
                .iter()
                .map(|t| rc_pro::mock::hash_embedding(t))
                .collect())
        }
    }

    #[tokio::test]
    async fn stale_tool_call_rejected_when_not_in_snapshot() {
        // execute_tool 的拒绝分支:快照不含 count → 返回 ok:false 且含 "Stale tool call",
        // 且工具本体未被调用(计数器保持 0)。
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counting = CountingTool {
            calls: counter.clone(),
        };
        let inner = mock_inner(
            ToolRegistry::new(vec![Box::new(counting)]),
            Duration::from_secs(10),
            None,
        );
        let (tx, _rx) = channel(16);
        let call = CanonicalToolCall {
            id: "c1".into(),
            name: "count".into(),
            arguments: json!({}),
        };
        let snapshot: Vec<String> = vec!["read_file".into()];
        let result = inner
            .execute_tool("s1", &tx, &call, &snapshot)
            .await
            .unwrap();
        assert!(!result.ok, "stale call must be rejected");
        assert!(
            result.output.contains("Stale tool call"),
            "output: {}",
            result.output
        );
        assert_eq!(
            counter.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "tool body must never run for a stale call"
        );
    }

    #[tokio::test]
    async fn tool_calls_run_concurrently_and_results_in_order() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_path_buf();
        let state_path = base.join("state.db");
        // mock provider 一轮发 2 个工具调用(slow1/slow2),各 sleep 100ms。
        let cfg = ProviderConfig {
            kind: "mock".into(),
            base_url: String::new(),
            model: "mock-1".into(),
            api_key: None,
            api_key_env: None,
            embedding_model: None,
            headers: Default::default(),
            extra: json!({
                "script": [
                    {"type": "tool", "name": "slow1", "arguments": {}},
                    {"type": "tool", "name": "slow2", "arguments": {}},
                    {"type": "done", "stop_reason": "tool_calls"}
                ]
            }),
        };
        let inner = mock_inner_with_provider(
            Arc::new(MockProvider::new(cfg, "mock-1".into())),
            ToolRegistry::new(vec![
                Box::new(SlowTool { name: "slow1" }),
                Box::new(SlowTool { name: "slow2" }),
            ]),
            Duration::from_secs(10),
            None,
            0, // max_steps:0 = 兜底用 max_turns。
            &base,
            state_path,
        );
        // store 有外键约束:先建会话,再跑执行阶段。
        let session = inner
            .store
            .lock()
            .unwrap()
            .create_session(".")
            .unwrap();
        let (tx, _rx) = channel(128);
        let messages = vec![CanonicalMessage::user("run two slow tools")];
        let started = std::time::Instant::now();
        let (_summary, _usage, log, _thinking) = inner
            .run_execute_phase(&session.id, messages, &tx)
            .await
            .unwrap();
        let elapsed = started.elapsed();

        // 并行:两个 100ms 工具应在 <190ms 内完成(串行会 ≥200ms)。
        assert!(
            elapsed < Duration::from_millis(190),
            "two 100ms tools should run concurrently (<190ms), took {:?}",
            elapsed
        );

        // 两个调用都执行了,且结果按调用到达顺序回灌。
        let tool_messages: Vec<&CanonicalMessage> = log
            .iter()
            .filter(|m| m.role == CanonicalRole::Tool)
            .collect();
        assert_eq!(tool_messages.len(), 2, "expected two tool results");
        let names: Vec<&str> = tool_messages
            .iter()
            .map(|m| m.name.as_deref().unwrap_or(""))
            .collect();
        assert_eq!(names, vec!["slow1", "slow2"], "results must be in call order");
        let calls_msg = log
            .iter()
            .find(|m| !m.tool_calls.is_empty())
            .expect("assistant_tool_calls message must be present");
        assert_eq!(calls_msg.tool_calls.len(), 2);
        let call_ids: Vec<&str> = calls_msg
            .tool_calls
            .iter()
            .map(|c| c.id.as_str())
            .collect();
        let result_ids: Vec<&str> = tool_messages
            .iter()
            .map(|m| m.tool_call_id.as_deref().unwrap_or(""))
            .collect();
        assert_eq!(
            call_ids, result_ids,
            "each tool result must pair back to its call in order"
        );
    }

    #[tokio::test]
    async fn last_step_has_no_tools_and_max_steps_message() {
        // max_steps = 1(mock_inner 的 max_turns 固定 1):唯一一步即最后一步,
        // 请求的 tools 应为空,且 messages 含 "MAXIMUM STEPS REACHED" 助手消息。
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_path_buf();
        let state_path = base.join("state.db");
        let provider = Arc::new(CapturingProvider::new("capture"));
        let inner = mock_inner_with_provider(
            provider.clone(),
            // 注册一个工具:未实现守卫时请求的 tools 非空 → 断言失败(RED)。
            ToolRegistry::new(vec![Box::new(SlowTool { name: "ls" })]),
            Duration::from_secs(10),
            None,
            1, // max_steps:仅 1 步,且为最后一步。
            &base,
            state_path,
        );
        let session = inner
            .store
            .lock()
            .unwrap()
            .create_session(".")
            .unwrap();
        let (tx, _rx) = channel(128);
        let messages = vec![CanonicalMessage::user("run a task")];
        let (_summary, _usage, log, _thinking) = inner
            .run_execute_phase(&session.id, messages, &tx)
            .await
            .unwrap();

        let requests = provider.requests();
        assert_eq!(
            requests.len(),
            1,
            "max_steps=1 should run exactly one step"
        );
        let req = &requests[0];
        assert!(
            req.tools.is_empty(),
            "last step must not materialize tools"
        );
        assert!(
            req.messages
                .iter()
                .any(|m| m.text().contains("MAXIMUM STEPS REACHED")),
            "last step request must include the MAX STEPS message"
        );
        assert!(
            log.as_slice()
                .iter()
                .any(|m| m.text().contains("MAXIMUM STEPS REACHED")),
            "returned log must carry the MAX STEPS message"
        );
    }

    #[tokio::test]
    async fn mid_stream_provider_error_drops_pending_calls_without_executing() {
        // provider 先发出 ToolCall 再 mid-stream 报错 ⇒ 并发 drain 前就已 return Err,
        // 已收集的 pending 调用被丢弃、不执行(counter 保持 0)。行为固化,见 Error arm 注释。
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counting = CountingTool {
            calls: counter.clone(),
        };
        let provider = Arc::new(MockProvider::new(
            ProviderConfig {
                kind: "mock".into(),
                base_url: String::new(),
                model: "mock-err".into(),
                api_key: None,
                api_key_env: None,
                embedding_model: None,
                headers: Default::default(),
                extra: json!({
                    "script": [
                        {"type": "tool", "name": "count", "arguments": {}},
                        {"type": "error", "message": "mid-stream failure"}
                    ]
                }),
            },
            "mock-err".into(),
        ));
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_path_buf();
        let inner = mock_inner_with_provider(
            provider,
            ToolRegistry::new(vec![Box::new(counting)]),
            Duration::from_secs(10),
            None,
            2, // max_steps:2 ⇒ 第 0 步不是最后一步,快照含 "count"(排除 last-step 干扰)。
            &base,
            base.join("state.db"),
        );
        let session = inner
            .store
            .lock()
            .unwrap()
            .create_session(".")
            .unwrap();
        let (tx, _rx) = channel(16);
        let messages = vec![CanonicalMessage::user("run a task")];
        let err = inner
            .run_execute_phase(&session.id, messages, &tx)
            .await
            .expect_err("mid-stream provider error must surface as Err");
        assert!(
            err.to_string().contains("mid-stream failure"),
            "err message: {err}"
        );
        assert_eq!(
            counter.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "pending tool call must NOT execute after a mid-stream provider error"
        );
    }

    #[test]
    fn compact_messages_removes_tool_call_groups_atomically() {
        let inner = mock_inner(ToolRegistry::new(vec![]), Duration::from_secs(180), Some(64));
        // assistant_tool_calls 与紧跟的 tool 结果必须成组移除;预算极小时两者一起消失,
        // 不能留下"孤儿 tool"(其引用的 tool_call_id 已不存在 → provider 400)。
        let messages = vec![
            CanonicalMessage::system("system"),
            CanonicalMessage::assistant_tool_calls(vec![CanonicalToolCall {
                id: "call_1".into(),
                name: "list_dir".into(),
                arguments: json!({}),
            }]),
            CanonicalMessage::tool("call_1", "list_dir", "big output ".repeat(200)),
            CanonicalMessage::user("current task"),
        ];
        let compacted = inner.compact_messages(messages);
        assert_eq!(compacted.len(), 2);
        assert_eq!(compacted[0].text(), "system");
        assert_eq!(compacted[1].text(), "current task");
        assert!(
            !compacted.iter().any(|m| m.role == CanonicalRole::Tool),
            "must not leave an orphan tool result behind"
        );
    }

    #[tokio::test]
    async fn cancel_fires_session_end_hook() {
        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("session_end_marker.txt");
        // 用相对文件名:run_hook 的 cwd = tmp,相对重定向跨平台(cmd/sh)都可靠,
        // 避免 Windows 下 cmd /C 参数引号被转义导致绝对路径重定向失败。
        let hooks = HooksConfig {
            session_end: vec!["echo session_end > session_end_marker.txt".to_string()],
            ..Default::default()
        };
        let (agent, _store, session_id) = mock_agent_with(
            &tmp,
            vec![
                json!({"type": "text", "text": "first"}),
                json!({"type": "sleep", "ms": 200}),
                json!({"type": "text", "text": "second"}),
            ],
            false,
            hooks,
        );
        let mut stream = agent.run(session_id, "run something".into());
        tokio::time::sleep(Duration::from_millis(50)).await;
        agent.cancel();
        let mut cancelled = false;
        while let Some(event) = stream.next().await {
            if let AgentEvent::Error { message } = &event {
                if message == "cancelled by user" {
                    cancelled = true;
                    break;
                }
            }
        }
        assert!(cancelled, "expected 'cancelled by user' error event");
        assert!(
            marker.exists(),
            "session_end hook must fire even when the run is cancelled"
        );
    }

    #[test]
    fn compact_messages_keeps_system_and_latest_turn() {
        let inner = mock_inner(ToolRegistry::new(vec![]), Duration::from_secs(180), Some(16));
        let messages = vec![
            CanonicalMessage::system("system"),
            CanonicalMessage::user("old turn"),
            CanonicalMessage::assistant_text("old reply"),
            CanonicalMessage::user("current task"),
        ];
        let compacted = inner.compact_messages(messages);
        assert_eq!(compacted.len(), 2);
        assert_eq!(compacted[0].text(), "system");
        assert_eq!(compacted[1].text(), "current task");
    }

    #[test]
    fn relevant_skills_hint_lists_selected_skills() {
        let skills = vec![
            SkillSummary {
                name: "test-after-change".into(),
                description: "run tests after changes".into(),
                category: "workflow".into(),
                score: 0.8,
                is_leaf: true,
            },
            SkillSummary {
                name: "coding-cycle".into(),
                description: "full task lifecycle".into(),
                category: "workflow".into(),
                score: 0.7,
                is_leaf: true,
            },
        ];
        let hint = relevant_skills_hint(&skills).unwrap();
        assert!(hint.contains("test-after-change"));
        assert!(hint.contains("skill` tool"));
        // 空 → None(不给提示,保持前缀最简)。
        assert!(relevant_skills_hint(&[]).is_none());
    }
}
