//! Raincode CLI: the terminal entry point for running, evolving, serving,
//! managing skills/profiles/MCP servers and acting as a CC-Switch-style
//! provider gateway.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use clap::{Parser, Subcommand};
use futures::StreamExt;
use rc_core::{Agent, AgentConfig, HooksConfig, DEFAULT_AGENT};
use rc_evolve::{DaemonConfig, EvolveConfig, EvolveEngine, PatternDaemon};
use rc_gateway::{serve as serve_gateway, GatewayConfig};
use rc_mcp::{McpManager, McpServerConfig};
use rc_net::{tools::network_tools, SearchConfig};
use rc_pro::{create_provider, Provider};
use rc_profile::cc_switch::{import_from_db, parse_deeplink, ProfileImport};
use rc_profile::model::{Profile, ProfileKind, Registry};
use rc_profile::writers::all_writers;
use rc_profile::{delete_key, find_provider, key_ref, protect_profile, store_key};
use rc_proto::{encode_line, AgentEvent, Request, RequestMethod, Response, RpcError};
use rc_router::capability::CapabilityProfile;
use rc_router::execute_subtasks_batched;
use rc_router::recursion::{ExecAction, ExecPlan};
use rc_router::risk::{EscalationTrigger, RiskState};
use rc_router::vision::{needs_vision, should_bridge};
use rc_router::{DispatchEntry, Risk};
use rc_sandbox::{
    ApprovalHook, AutoApproveHook, AutoUserHook, CommandPolicy, DenyHook, NetworkPolicy,
    PolicyDefault, PromptHook, PromptUserHook, UserInputHook,
};
use rc_skill::source::{LocalSource, RemoteSource, SkillSource};
use rc_skill::{
    install_seed, seed_installed, NavAction, NavFrame, NavigatorLimits, Skill, SkillNavigator,
    SkillNetwork, SkillNode, SkillRouter, SkillStore,
};
use rc_state::{NavOutcome, NavigationRecord, Store};
use rc_tool::builtin::default_tools;
use rc_tool::{Tool, ToolContext, ToolResult, ToolSpec};
use serde::Deserialize;
use serde_json::{json, Value};
use tracing_subscriber::EnvFilter;

/// Hosting-CLI environment for the raincode-tui REPL. Bridges every CLI-side
/// helper (registry / store / providers / skills / slash dispatch) into the
/// TUI's [`raincode_tui::repl::env::ReplEnv`] trait so the REPL never depends
/// on `rc-cli` (avoids a crate cycle).
pub(crate) struct FileEnv {
    pub config: Arc<FileConfig>,
}

#[async_trait::async_trait(?Send)]
impl raincode_tui::repl::env::ReplEnv for FileEnv {
    fn load_registry(&self) -> Result<Registry> {
        load_registry()
    }
    fn save_registry(&self, registry: &Registry) -> Result<()> {
        save_registry(registry)
    }
    fn home_dir(&self) -> PathBuf {
        raincode_home()
    }
    fn skills_dir(&self) -> PathBuf {
        skills_dir(&self.config)
    }
    fn workspace(&self) -> PathBuf {
        self.config
            .core
            .workspace
            .as_deref()
            .map(expand_tilde)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
    }
    fn create_session(&self) -> Result<String> {
        let store = Store::open(state_path())?;
        let ws = self.workspace();
        Ok(store.create_session(&ws.to_string_lossy())?.id)
    }
    fn open_store(&self) -> Result<Store> {
        Store::open(state_path()).map_err(anyhow::Error::from)
    }
    fn make_provider(&self, registry: &Registry) -> Result<std::sync::Arc<dyn Provider + Send + Sync + 'static>> {
        let p: Arc<dyn Provider> = make_provider(registry)?;
        Ok(p)
    }
    fn dispatch_slash(&self, name: &str, args: &serde_json::Value) -> Result<String, String> {
        dispatch_slash(name, args)
    }
    fn skill_nav(&self, task: &str) -> Result<Vec<String>, String> {
        let skill_dir = skills_dir(&self.config);
        if !skill_dir.exists() {
            return Err(format!(
                "skills dir not found: {}(先跑 `raincode skills list` 或安装技能)",
                skill_dir.display()
            ));
        }
        let store = SkillStore::new(&skill_dir);
        let network = SkillNetwork::from_store(&store);
        let router = SkillRouter::new(store.discover());
        // 导航结果写 navigation_log(生产路径的 darwinian fitness 数据源)。
        // state.db 打开失败不阻断导航:返回 None → 本次不记录(记录失败只 warn)。
        let state_store = Store::open(state_path()).ok();
        drive_skill_nav(&network, &router, task, state_store.as_ref())
    }
    fn store_key(&self, id: &str, key: &str) -> Result<()> {
        store_key(id, key)?;
        Ok(())
    }
    fn key_ref(&self, id: &str) -> String {
        key_ref(id)
    }
    async fn verify_connectivity(&self, profile: &Profile) -> Result<String> {
        verify_provider_connectivity(profile).await
    }
    async fn agent_config(
        &self,
        registry: &Registry,
        with_slash_command: bool,
    ) -> Result<AgentConfig> {
        let store = Store::open(state_path())?;
        let skill_store = SkillStore::new(skills_dir(&self.config));
        agent_config(
            &self.config,
            registry,
            store,
            skill_store,
            false,
            false,
            true,
            None,
            with_slash_command,
        )
        .await
    }
    fn context_window(&self, registry: &Registry) -> u64 {
        let store = match Store::open(state_path()) {
            Ok(s) => s,
            Err(_) => return 0,
        };
        context_for_model(&store, registry)
    }
    async fn refresh_profiles(&self) -> anyhow::Result<String> {
        let store = Store::open(state_path())?;
        let n = refresh_profiles(&store).await?;
        Ok(format!("已刷新 {n} 个模型评分(OpenRouter/arena 真实榜单)"))
    }
    fn model_picker_entries(&self) -> anyhow::Result<Vec<raincode_tui::repl::env::ModelPickerEntry>> {
        use raincode_tui::repl::env::ModelPickerEntry;
        let registry = load_registry()?;
        let store = Store::open(state_path())?;
        let loaded: Vec<CapabilityProfile> = {
            let from_db = store.all_model_profiles()?;
            if from_db.is_empty() {
                rc_router::capability::seed_profiles()
            } else {
                from_db.into_iter().map(CapabilityProfile::from_row).collect()
            }
        };
        let active_id = registry.active().map(|p| p.id.clone());
        let mut entries = Vec::new();
        for p in &registry.profiles {
            if p.kind == rc_profile::model::ProfileKind::Mock {
                continue;
            }
            let cp = resolve_capability_profile(&loaded, &p.model)
                .unwrap_or_else(|| default_capability_profile(&p.model));
            entries.push(ModelPickerEntry {
                id: p.id.clone(),
                provider: provider_label(&p.base_url, &p.id),
                model: p.model.clone(),
                active: active_id.as_deref() == Some(p.id.as_str()),
                reasoning: cp.reasoning,
                coding: cp.coding,
                frontend: cp.frontend,
                backend: cp.backend,
            });
        }
        Ok(entries)
    }
    fn route_run(
        &self,
        prompt: String,
        plan_only: bool,
        emit: Arc<dyn Fn(AgentEvent) + Send + Sync>,
        steer_hub: Arc<rc_core::SteerHub>,
        cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
        risk_mode: rc_router::risk::RiskMode,
        subagent_approval: std::sync::Arc<dyn rc_sandbox::ApprovalHook>,
        supervisor: Option<Arc<rc_core::Supervisor>>,
        feed: raincode_tui::repl::env::AgentFeed,
    ) {
        // 路由 future 是 !Send(rusqlite &Store 跨 await),必须在单线程内跑完。
        // 起独立线程 + 独立 runtime,主事件循环保持响应(期间可 Tab 选 agent + 发 steer)。
        // plan_only:只拆解计划不执行(thinking 确认阶段);否则 rc-router 引擎真执行
        // (拆解→自动选模型→子代理→结果回灌→Done)。
        let config = self.config.clone();
        let supervise_active = supervisor.is_some();
        std::thread::spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(r) => r,
                Err(_) => return,
            };
            // 表面化错误:route_command 失败不再静默丢弃,发 Error 事件给 TUI。
            let emit_for_error = emit.clone();
            // 监督转发:supervise_active(监督会话已启动)时,emit 之外再抄一份子代理
            // 事件到 feed(TUI 主循环周期排空 + should_judge/judge)。supervisor 本身
            // 不过线程边界 — feed 是唯一跨线程共享结构。
            let routed_emit: Arc<dyn Fn(AgentEvent) + Send + Sync> = if supervise_active {
                let feed = feed.clone();
                let base = emit.clone();
                Arc::new(move |ev: AgentEvent| {
                    forward_to_supervisor_feed(&feed, &ev);
                    base(ev);
                })
            } else {
                emit.clone()
            };
            let result = runtime.block_on(route_command(
                &config,
                &prompt,
                plan_only,
                None,
                None,
                true,
                Some(routed_emit),
                Some(steer_hub),
                Some(&cancel),
                Some(subagent_approval), // TUI:子代理跟随共享风险档
                risk_mode,
            ));
            if let Err(e) = result {
                emit_for_error(AgentEvent::Error { message: format!("{e:#}") });
            }
        });
    }
    fn supervise_start(&self, registry: &Registry, model: Option<&str>) -> Result<Arc<rc_core::Supervisor>, String> {
        let provider = supervisor_provider(registry, model).map_err(|e| e.to_string())?;
        let cfg = rc_sandbox::load_supervise_config(&self.home_dir()).map_err(|e| e.to_string())?;
        let boundaries = cfg.nl.join("\n");
        // 会话锚点:Supervisor 由 TUI 主循环持有(judge 在 TUI 线程调用),route_run
        // 时作为监督开关传入,子代理事件经 feed 转发回主循环做周期判断。
        Ok(Arc::new(rc_core::Supervisor { provider, cfg, boundaries }))
    }
    fn supervise_config_path(&self) -> PathBuf {
        self.home_dir().join("supervise.toml")
    }
}

/// 抄一份子代理事件到监督 feed(仅 AgentSpawned/AgentToolCall/AgentResult;
/// Token 流等非代理事件不转发,防 feed 无限增长)。TUI 主循环排空后做周期判断。
fn forward_to_supervisor_feed(feed: &raincode_tui::repl::env::AgentFeed, ev: &AgentEvent) {
    if matches!(
        ev,
        AgentEvent::AgentSpawned { .. }
            | AgentEvent::AgentToolCall { .. }
            | AgentEvent::AgentResult { .. }
    ) {
        if let Ok(mut v) = feed.lock() {
            v.push(ev.clone());
        }
    }
}

#[derive(Parser)]
#[command(
    name = "raincode",
    version,
    about = "Self-evolving coding agent harness"
)]
struct Cli {
    /// Run the stdio JSON-RPC server instead of a subcommand.
    #[arg(long)]
    serve: bool,
    /// Resume the most recent session instead of starting a new one.
    #[arg(short, long)]
    resume: bool,
    /// Produce a plan without executing tools.
    #[arg(long)]
    plan: bool,
    /// Entropy-reduction mode: expensive plan model asks clarifying questions, then a cheap execution model works.
    #[arg(long)]
    entropy: bool,
    /// Agent profile to use (coding, architect, researcher).
    #[arg(long)]
    agent: Option<String>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run one prompt to completion and print events.
    Run {
        prompt: String,
        /// Resume the most recent session instead of starting a new one.
        #[arg(short, long)]
        resume: bool,
        /// Produce a plan without executing tools.
        #[arg(long)]
        plan: bool,
        /// Entropy-reduction mode: clarify intent with a plan model, then execute with a cheap model.
        #[arg(long)]
        entropy: bool,
        /// Agent profile to use (coding, architect, researcher).
        #[arg(long)]
        agent: Option<String>,
    },
    /// Start the stdio JSON-RPC server.
    Serve,
    /// 交互式 REPL(裸行=发任务;无子命令且 stdin 是终端时自动进入)。
    Repl,
    /// Run the background pattern daemon (or one scan with --once).
    Daemon {
        #[arg(long)]
        once: bool,
    },
    /// Digest one session (default: newest) into the skill network.
    Evolve { session: Option<String> },
    /// Show experience and skill-network insights, or run a scan with --scan.
    Insights {
        #[arg(long)]
        scan: bool,
    },
    Skills {
        #[command(subcommand)]
        cmd: SkillsCmd,
    },
    Model {
        #[command(subcommand)]
        cmd: ModelCmd,
    },
    /// Route a task: allocator decomposes it into sub-tasks, the scoring engine
    /// dispatches each to a model, then sub-tasks execute concurrently.
    Route {
        prompt: String,
        /// Print the dispatch plan (intent + per-subtask model assignment) without executing.
        #[arg(long)]
        plan_only: bool,
        /// Comma-separated model pool constraint, e.g. deepseek,qwen (empty = all).
        #[arg(long)]
        pool: Option<String>,
        /// Comma-separated subtask pins as id:model pairs, e.g. s1:deepseek,s2:qwen (overrides routing).
        #[arg(long)]
        pin: Option<String>,
    },
    /// Manage the cached model capability profiles in the state DB.
    Profiles {
        #[command(subcommand)]
        cmd: ProfilesCmd,
    },
    Mcp {
        #[command(subcommand)]
        cmd: McpCmd,
    },
    /// Start the CC-Switch-compatible HTTP gateway.
    Proxy {
        #[arg(long, default_value = "8787")]
        port: u16,
    },
}

#[derive(Subcommand)]
enum SkillsCmd {
    List,
    Show {
        name: String,
    },
    Create {
        name: String,
        category: String,
        description: String,
        #[arg(long)]
        body: Option<String>,
    },
    Edit {
        name: String,
        #[arg(long)]
        description: Option<String>,
        #[arg(long)]
        category: Option<String>,
        #[arg(long)]
        body: Option<String>,
    },
    Install {
        spec: String,
    },
    Update {
        spec: String,
    },
    Uninstall {
        name: String,
    },
    Search {
        query: String,
    },
    Review {
        name: Option<String>,
        #[arg(long)]
        approve: bool,
        #[arg(long)]
        reject: bool,
    },
    Stats,
    Graph,
}

#[derive(Subcommand)]
enum ModelCmd {
    List,
    /// List vendors in the one-key provider catalog.
    Catalog,
    Use {
        id: String,
        #[arg(long)]
        app: Option<String>,
    },
    Add {
        id: String,
        #[arg(long)]
        name: Option<String>,
        /// Provider catalog id: openai, anthropic, deepseek, kimi, qwen, gemini, zhipu, groq, openrouter, ollama.
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        base_url: Option<String>,
        #[arg(long)]
        api_key: Option<String>,
        #[arg(long)]
        api_key_env: Option<String>,
        #[arg(long)]
        app: Option<String>,
        /// Skip connectivity check (useful for local endpoints / proxy issues).
        #[arg(long)]
        no_verify: bool,
    },
    /// Store or replace a profile's API key in the private key store.
    SetKey {
        id: String,
        #[arg(long)]
        api_key: Option<String>,
    },
    Remove {
        id: String,
    },
    Import {
        #[arg(long)]
        db: Option<String>,
        link: Option<String>,
    },
    Test {
        #[arg(long)]
        prompt: Option<String>,
    },
}

#[derive(Subcommand)]
enum ProfilesCmd {
    /// Show cached model capability profiles (model, context, pricing, multimodal).
    Show,
    /// Fetch model profiles from OpenRouter and normalize them into the state DB.
    Refresh,
}

#[derive(Subcommand)]
enum McpCmd {
    List {
        /// Connect to configured servers and print their exposed tools.
        #[arg(long)]
        check: bool,
    },
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct FileConfig {
    core: CoreSection,
    hooks: HooksConfig,
    model: ModelSection,
    evolve: EvolveSection,
    entropy: EntropySection,
    skills: SkillsSection,
    sandbox: SandboxSection,
    gateway: GatewaySection,
    mcp: McpSection,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct McpSection {
    servers: BTreeMap<String, McpServerConfig>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct CoreSection {
    workspace: Option<String>,
    max_turns: Option<usize>,
    max_history_bytes: Option<usize>,
    approval_mode: Option<String>,
    agent: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct ModelSection {
    profile: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct EvolveSection {
    enabled: Option<bool>,
    auto_approve: Option<bool>,
    min_experiences: Option<u32>,
    similarity_threshold: Option<f32>,
    daemon_interval_minutes: Option<u64>,
    daemon_lock: Option<String>,
    daemon_min_cluster: Option<usize>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct EntropySection {
    enabled: Option<bool>,
    mode: Option<String>,
    plan_profile: Option<String>,
    execute_profile: Option<String>,
    review_profile: Option<String>,
    plan_max_rounds: Option<usize>,
    plan_max_questions: Option<usize>,
    review_max_rounds: Option<usize>,
    max_cycles: Option<usize>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct SkillsSection {
    dir: Option<String>,
    top_k: Option<usize>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct SandboxSection {
    commands: CommandSection,
    network: NetworkSection,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct CommandSection {
    allow: Vec<String>,
    deny: Vec<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct NetworkSection {
    allow_hosts: Vec<String>,
    deny_hosts: Vec<String>,
    default: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct GatewaySection {
    enabled: bool,
    listen: Option<String>,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::from_default_env().add_directive("raincode=info".parse().unwrap()),
        )
        .try_init()
        .ok();
    let cli = Cli::parse();
    let runtime = tokio::runtime::Runtime::new().context("tokio runtime")?;
    runtime.block_on(async_main(cli))
}

async fn async_main(cli: Cli) -> Result<()> {
    let config = load_config()?;
    if cli.serve {
        return serve_stdio(&config).await;
    }
    match cli.command {
        Some(Command::Run {
            prompt,
            resume,
            plan,
            entropy,
            agent,
        }) => {
            run_prompt(
                &config,
                &prompt,
                resume,
                plan,
                entropy,
                agent.as_deref(),
            )
            .await
        }
        Some(Command::Serve) => serve_stdio(&config).await,
        Some(Command::Repl) => {
            let env = FileEnv { config: Arc::new(config) };
            raincode_tui::repl::r#loop::repl_command(&env).await
        }
        Some(Command::Daemon { once }) => daemon_command(&config, once).await,
        Some(Command::Evolve { session }) => evolve_command(&config, session.as_deref()).await,
        Some(Command::Insights { scan }) => insights_command(&config, scan).await,
        Some(Command::Skills { cmd }) => skills_command(&config, cmd).await,
        Some(Command::Model { cmd }) => model_command(&config, cmd).await,
        Some(Command::Route { prompt, plan_only, pool, pin }) => {
            route_command(&config, &prompt, plan_only, pool, pin.as_deref(), false, None, None, None, None, rc_router::risk::RiskMode::Ask).await
        }
        Some(Command::Profiles { cmd }) => profiles_command(&config, cmd).await,
        Some(Command::Mcp { cmd }) => mcp_command(&config, cmd).await,
        Some(Command::Proxy { port }) => {
            let addr = format!("127.0.0.1:{port}");
            let gateway_config = GatewayConfig {
                addr: addr.parse().context("invalid gateway address")?,
                registry_path: registry_path(),
            };
            serve_gateway(gateway_config).await?;
            Ok(())
        }
        None => {
            use std::io::IsTerminal;
            if std::io::stdin().is_terminal() {
                let env = FileEnv { config: Arc::new(config) };
                raincode_tui::repl::r#loop::repl_command(&env).await?;
                Ok(())
            } else {
                let prompt = read_stdin_prompt()?;
                // 管道输入:走纯 stdout,不进全屏 TUI(否则无终端时黑屏)。
                run_prompt(
                    &config,
                    &prompt,
                    cli.resume,
                    cli.plan,
                    cli.entropy,
                    cli.agent.as_deref(),
                )
                .await
            }
        }
    }
}

fn load_config() -> Result<FileConfig> {
    let path = config_path();
    if !path.exists() {
        return Ok(FileConfig::default());
    }
    let text = std::fs::read_to_string(&path).context("read config")?;
    toml::from_str(&text).context("parse config")
}

fn raincode_home() -> PathBuf {
    std::env::var_os("RAINCODE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".raincode")
        })
}

fn config_path() -> PathBuf {
    raincode_home().join("config.toml")
}

fn registry_path() -> PathBuf {
    raincode_home().join("profiles.toml")
}

fn state_path() -> PathBuf {
    raincode_home().join("state.db")
}

fn skills_dir(config: &FileConfig) -> PathBuf {
    let home = raincode_home().join("skills");
    config
        .skills
        .dir
        .as_deref()
        .map(expand_tilde)
        .unwrap_or(home)
}

fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(rest)
    } else {
        PathBuf::from(path)
    }
}

fn load_registry() -> Result<Registry> {
    let mut registry = Registry::load(registry_path())?;
    registry.ensure_default();
    let _ = registry.save(registry_path());
    Ok(registry)
}

fn save_registry(registry: &Registry) -> Result<()> {
    registry.save(registry_path())?;
    Ok(())
}

fn active_profile(registry: &Registry) -> Result<Profile> {
    registry
        .active()
        .cloned()
        .or_else(|| registry.profiles.first().cloned())
        .ok_or_else(|| anyhow!("no active provider profile; run `raincode model add`"))
}

/// 连通性自检:用 profile(含已解析 key)发一个最小 chat 请求,验证「选模型 →
/// 贴 key → 连上」闭环。成功返回描述消息,失败返回错误(不打印 key)。
async fn verify_provider_connectivity(profile: &Profile) -> Result<String> {
    use rc_pro::canonical::{CanonicalMessage, CanonicalRequest};
    use rc_pro::ProvEvent;
    use futures::StreamExt;

    let mut cfg = profile.to_provider_config();
    if cfg.api_key.is_none() {
        cfg.api_key = profile.resolved_api_key()?;
    }
    let provider = create_provider(cfg).map_err(|e| anyhow!("provider config: {e}"))?;
    let request = CanonicalRequest {
        model: profile.model.clone(),
        messages: vec![
            CanonicalMessage::system("You are a connectivity probe."),
            CanonicalMessage::user("Reply with exactly: ok"),
        ],
        tools: vec![],
        temperature: Some(0.0),
        max_tokens: Some(8),
        stream: true,
        extra: json!({}),
    };
    // 超时兜底:防止挂死的供应商阻塞向导。
    let timed = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        let mut stream = provider.stream(request).await?;
        while let Some(ev) = stream.next().await {
            match ev.map_err(|e| anyhow!("provider error: {e}"))? {
                ProvEvent::Delta { .. } | ProvEvent::Finish { .. } => return Ok(()),
                ProvEvent::Error { message } => return Err(anyhow!("{message}")),
                _ => {}
            }
        }
        Ok(())
    })
    .await;
    match timed {
        Ok(Ok(())) => Ok(format!("✓ 连接成功:{} ({})", profile.model, profile.base_url)),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(anyhow!("连接超时(20s):{}", profile.base_url)),
    }
}

fn make_provider(registry: &Registry) -> Result<Arc<dyn Provider>> {
    provider_for_profile(registry, None)
}

fn provider_for_profile(registry: &Registry, id: Option<&str>) -> Result<Arc<dyn Provider>> {
    let profile = match id {
        Some(id) => registry
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow!("provider profile '{id}' not found"))?,
        None => active_profile(registry)?,
    };
    let mut cfg = profile.to_provider_config();
    if cfg.api_key.is_none() {
        cfg.api_key = profile.resolved_api_key()?;
    }
    let provider = create_provider(cfg).map_err(|e| anyhow!("provider config: {e}"))?;
    Ok(Arc::from(provider))
}

/// 监督 provider:指定 profile 或活跃 profile → `Box<dyn Provider>`(Supervisor 字段类型)。
/// 与 provider_for_profile 同源(解析 key + create_provider),只是保留 Box 而非包 Arc。
fn supervisor_provider(registry: &Registry, model: Option<&str>) -> Result<Box<dyn Provider>> {
    let profile = match model {
        Some(id) => registry
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow!("provider profile '{id}' not found"))?,
        None => active_profile(registry)?,
    };
    let mut cfg = profile.to_provider_config();
    if cfg.api_key.is_none() {
        cfg.api_key = profile.resolved_api_key()?;
    }
    create_provider(cfg).map_err(|e| anyhow!("provider config: {e}"))
}

fn user_input_hook(interactive: bool) -> Arc<dyn UserInputHook> {
    if interactive {
        Arc::new(PromptUserHook::new(|question| {
            print!("\n[question] {question}\n> ");
            io::stdout().flush().ok();
            let mut line = String::new();
            io::stdin().lock().read_line(&mut line).ok();
            let answer = line.trim().to_string();
            if answer.is_empty() {
                "No response provided; use best judgment.".to_string()
            } else {
                answer
            }
        }))
    } else {
        Arc::new(AutoUserHook::default())
    }
}

fn network_policy(config: &FileConfig) -> NetworkPolicy {
    NetworkPolicy {
        allow_hosts: config.sandbox.network.allow_hosts.clone(),
        deny_hosts: config.sandbox.network.deny_hosts.clone(),
        default: match config.sandbox.network.default.as_deref() {
            Some("allow") => PolicyDefault::Allow,
            _ => PolicyDefault::Deny,
        },
    }
}

fn approval_hook(mode: &str) -> Arc<dyn ApprovalHook> {
    match mode {
        "auto" => Arc::new(AutoApproveHook),
        "deny" => Arc::new(DenyHook),
        _ => Arc::new(PromptHook::new(|req| {
            print!("\n[approval] {} {}? [y/N] ", req.tool, req.description);
            io::stdout().flush().ok();
            let mut line = String::new();
            io::stdin().lock().read_line(&mut line).ok();
            if line.trim().eq_ignore_ascii_case("y") {
                rc_sandbox::ApprovalDecision::Allow
            } else {
                rc_sandbox::ApprovalDecision::Deny {
                    reason: "declined by user".into(),
                }
            }
        })),
    }
}

fn ensure_seed(skills_dir: &Path) -> Result<()> {
    if !seed_installed(skills_dir) {
        let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let seed_root = repo_root.join("skills");
        if seed_root.exists() {
            install_seed(&seed_root, skills_dir)
                .map_err(|e| anyhow!("seed install failed: {e}"))?;
        }
    }
    Ok(())
}

fn sync_skill_index(store: &Store, skills: &SkillStore) -> Result<()> {
    for skill in skills.discover() {
        store
            .upsert_skill(&skill.to_row())
            .map_err(|e| anyhow!("skill index: {e}"))?;
    }
    Ok(())
}

/// 任务文本对一个 skill 的匹配分(命中 name/description/tags/triggers 加权)。
/// 驱动 skill 导航时用于选择下钻分支(纯 keyword,与 SkillRouter 同思路)。
fn skill_match_score(task: &str, skill: &Skill) -> f32 {
    let lower_task = task.to_lowercase();
    let mut hits = 0.0;
    for t in &skill.triggers {
        if lower_task.contains(&t.to_lowercase()) {
            hits += 2.0;
        }
    }
    let haystack = format!(
        "{} {} {}",
        skill.name,
        skill.description,
        skill.tags.join(" ")
    )
    .to_lowercase();
    let words: Vec<&str> = task
        .split(|c: char| !c.is_alphanumeric() && c != '.' && c != '_')
        .filter(|w| w.len() > 1)
        .collect();
    let overlap = words
        .iter()
        .filter(|w| haystack.contains(&w.to_lowercase()))
        .count() as f32;
    hits + overlap / (words.len().max(1) as f32)
}

/// 叶子正文输出:标题 + 完整正文;经过导航时附带路径。
fn leaf_output(leaf: &Skill, path: &[String]) -> Vec<String> {
    let mut lines = vec![format!("## {} ({})", leaf.name, leaf.category), String::new()];
    lines.extend(leaf.body.lines().map(|l| l.to_string()));
    if path.len() > 1 {
        lines.push(String::new());
        lines.push(format!("[skill-nav] 路径: {}", path.join(" → ")));
    }
    lines
}

/// 索引菜单输出(SkillNavigator::menu + 下钻提示)。
fn menu_output(nav: &SkillNavigator, name: &str) -> Vec<String> {
    let mut lines: Vec<String> = nav.menu(name).lines().map(str::to_string).collect();
    lines.push(String::new());
    lines.push(format!(
        "[skill-nav] 索引 {name}:用 /skill-nav <子名> 下钻,或用 /skill-nav <新任务> 重新路由"
    ));
    lines
}

/// 回溯预算耗尽时的输出:当前菜单 + 明确提示停止自动导航(不让驱动无限回溯)。
fn budget_exhausted_menu(nav: &SkillNavigator, name: &str, limit: usize) -> Vec<String> {
    let mut lines = menu_output(nav, name);
    lines.push(format!(
        "[skill-nav] 回溯预算耗尽(backtrack_budget={limit}),已停止自动导航;请用 /skill-nav <子名> 手动下钻"
    ));
    lines
}

/// 回溯一次(驱动方计数):预算未耗尽 → pop 并计数;预算耗尽或已在根 → 不再回溯。
/// 返回 false = 停止回溯(backtrack_budget 耗尽或已在根)。Task 3 的 SkillNavigator
/// 本身不 enforce backtrack_budget(visited 是 per-frame,pop 即丢失,否则可能无限
/// descend/backtrack),由本驱动方计数约束 —— 这正是 Task 8 导航驱动方要补的缺口。
fn do_nav_backtrack(
    stack: &mut Vec<NavFrame>,
    nav: &SkillNavigator,
    used: &mut usize,
    limit: usize,
) -> Result<bool, String> {
    if *used >= limit {
        return Ok(false); // backtrack_budget 耗尽:不再允许回溯。
    }
    match nav.backtrack(stack) {
        Ok(_) => {
            *used += 1;
            Ok(true)
        }
        Err(_) => Ok(false), // 已在根,无法回溯。
    }
}

/// 记录一次导航决策到 navigation_log(生产路径的 darwinian fitness 数据源)。
/// store 为 None(TUI/测试显式不记录)时不写;记录失败只 warn,绝不阻断导航。
/// root = 路径首元素(顶层 skill),path_json = 访问路径 JSON 数组,task = 任务文本。
fn record_nav_outcome(store: Option<&Store>, task: &str, path: &[String], outcome: NavOutcome) {
    let Some(store) = store else { return };
    let root = path.first().cloned().unwrap_or_default();
    let path_json = serde_json::to_string(path).unwrap_or_else(|_| "[]".to_string());
    let rec = NavigationRecord {
        id: String::new(),
        task_signature: task.to_string(),
        root,
        path_json,
        outcome,
        model: "skill-nav".into(),
        created_at: String::new(),
    };
    if let Err(e) = store.record_navigation(&rec) {
        tracing::warn!("skill-nav: failed to record navigation: {e}");
    }
}

/// 回溯停点分类:预算耗尽(backtracks_used 已达上限)→ BudgetExhausted;
/// 预算未耗尽却无法回溯(已在根的死胡同)→ WrongBranch。
fn nav_stop_outcome(backtracks_used: usize, backtrack_limit: usize) -> NavOutcome {
    if backtracks_used >= backtrack_limit {
        NavOutcome::BudgetExhausted
    } else {
        NavOutcome::WrongBranch
    }
}

/// 一次 /skill-nav 导航:叶子 → 正文;索引 → 带预算约束的下钻循环(菜单或命中叶子)。
/// 驱动方(而非 SkillNavigator)计数 backtrack_budget,命中预算即停止自动导航。
/// 每个落点(叶子命中/回溯停/预算停)写一条 navigation_log(store=Some 时)——
/// 这是 darwinian `fitness()` 的生产数据源,演化循环据此评估 skill。
fn drive_skill_nav(
    network: &SkillNetwork,
    router: &SkillRouter,
    task: &str,
    store: Option<&Store>,
) -> Result<Vec<String>, String> {
    let nav = SkillNavigator {
        network,
        limits: NavigatorLimits::default(),
    };
    let trimmed = task.trim();
    // 直接点名一个 skill → 打开该节点(叶子给正文 / 索引给菜单),不再重路由。
    if !trimmed.is_empty() {
        if let Some(node) = network.nodes.iter().find(|n| n.skill.name == trimmed) {
            return if node.is_leaf {
                let path = vec![node.skill.name.clone()];
                record_nav_outcome(store, task, &path, NavOutcome::Success);
                Ok(leaf_output(&node.skill, std::slice::from_ref(&node.skill.name)))
            } else {
                // 直接点名索引 → 菜单,无导航决策(outcome 无从谈起),不记录。
                Ok(menu_output(&nav, &node.skill.name))
            };
        }
    }
    let selections = router.select_networked(network, task, 3, None);
    let top = selections
        .first()
        .ok_or_else(|| format!("没有 skill 匹配 '{task}'"))?;
    // 叶子 → 直接给完整正文(最小可交付)。
    if top.is_leaf() {
        let leaf = network
            .leaf(&top.summary.name)
            .ok_or_else(|| format!("leaf '{}' not found in network", top.summary.name))?;
        let path = vec![leaf.name.clone()];
        record_nav_outcome(store, task, &path, NavOutcome::Success);
        return Ok(leaf_output(leaf, std::slice::from_ref(&leaf.name)));
    }
    // 索引 → 导航循环:从 top 出发顺着最佳匹配子下钻;死胡同/预算耗尽则回溯,
    // 回溯次数受 backtrack_budget 约束(默认 2)。无清晰匹配 → 列菜单让用户下钻。
    let mut stack: Vec<NavFrame> = vec![NavFrame {
        skill: top.summary.name.clone(),
        menu: nav.menu(&top.summary.name),
        siblings: vec![],
        visited: HashSet::new(),
    }];
    let mut tried: HashSet<String> = HashSet::new();
    let mut backtracks_used = 0usize;
    let backtrack_limit = nav.limits.backtrack_budget;
    // 步数硬上限:双保险(tried + backtrack_budget 已保证有界,这里再兜底防意外环)。
    for _ in 0..128 {
        let current = stack.last().map(|f| f.skill.clone()).unwrap_or_default();
        // 当前是叶子 → 命中正文。
        if let Some(leaf) = network.leaf(&current) {
            let path: Vec<String> = stack.iter().map(|f| f.skill.clone()).collect();
            record_nav_outcome(store, task, &path, NavOutcome::Success);
            return Ok(leaf_output(leaf, &path));
        }
        // 候选分支:未尝试过的子。
        let candidates: Vec<&SkillNode> = network
            .children_of(&current)
            .into_iter()
            .filter(|c| !tried.contains(&c.skill.name))
            .collect();
        let best = candidates
            .iter()
            .max_by(|a, b| {
                skill_match_score(task, &a.skill)
                    .partial_cmp(&skill_match_score(task, &b.skill))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .copied();
        let Some(child) = best else {
            // 死胡同(无候选子):回溯;预算耗尽 → 停,给当前菜单 + 提示。
            if !do_nav_backtrack(&mut stack, &nav, &mut backtracks_used, backtrack_limit)? {
                let name = stack.last().map(|f| f.skill.clone()).unwrap_or_default();
                let path: Vec<String> = stack.iter().map(|f| f.skill.clone()).collect();
                record_nav_outcome(store, task, &path, nav_stop_outcome(backtracks_used, backtrack_limit));
                return Ok(budget_exhausted_menu(&nav, &name, backtrack_limit));
            }
            continue;
        };
        // 无清晰匹配的子 → 给菜单(最小可交付),让用户 /skill-nav <子名> 手动下钻。
        // 这是自动导航的一次失配停点 → 记 WrongBranch。
        if skill_match_score(task, &child.skill) <= 0.0 {
            let path: Vec<String> = stack.iter().map(|f| f.skill.clone()).collect();
            record_nav_outcome(store, task, &path, NavOutcome::WrongBranch);
            return Ok(menu_output(&nav, &current));
        }
        let name = child.skill.name.clone();
        tried.insert(name.clone());
        match nav.descend(&mut stack, &name) {
            Ok(NavAction::AtLeaf { body }) => {
                let mut lines = vec![format!("## {name}"), String::new()];
                lines.extend(body.lines().map(str::to_string));
                let mut path: Vec<String> = stack.iter().map(|f| f.skill.clone()).collect();
                path.push(name.clone());
                record_nav_outcome(store, task, &path, NavOutcome::Success);
                return Ok(lines);
            }
            Ok(NavAction::Menu { .. }) => continue,
            // 下钻预算/深度耗尽或分支不可用(已访问/不存在)→ 回溯计数。
            Ok(NavAction::BudgetExhausted) | Err(_) => {
                if !do_nav_backtrack(&mut stack, &nav, &mut backtracks_used, backtrack_limit)? {
                    let name = stack.last().map(|f| f.skill.clone()).unwrap_or_default();
                    let path: Vec<String> = stack.iter().map(|f| f.skill.clone()).collect();
                    record_nav_outcome(store, task, &path, nav_stop_outcome(backtracks_used, backtrack_limit));
                    return Ok(budget_exhausted_menu(&nav, &name, backtrack_limit));
                }
                continue;
            }
        }
    }
    Err("skill navigation exceeded step guard".to_string())
}


/// ToolSpec for `run_slash_command`: the chat model executes a built-in slash
/// command (compact/clear/resume/model/route/risk/status) ONLY when the user
/// explicitly asks for it — never on its own initiative. The description is
/// load-bearing: it tells the model to stay passive otherwise.
fn slash_command_spec() -> ToolSpec {
    ToolSpec {
        name: "run_slash_command".into(),
        description: "Execute a built-in slash command (e.g. /compact) ONLY when the user explicitly instructs you to. Never initiate on your own.".into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "command name without leading slash, e.g. compact" },
                "args": { "type": "object" }
            },
            "required": ["name"]
        }),
    }
}

/// 把 run_slash_command 工具的名字分派到对应动作;返回工具输出文本。
/// 这是分发契约:后续与 rc-core compact / rc-router route 的真实接线在对应
/// crate 完成,这里先给 chat 一个可调用的、用户驱动的命令出口。
///
/// 诚实约束(禁止嘴硬):所有占位命令只明确声明「未实现」,绝不伪造数字或
/// 声称已发生的动作(如 "12.4k -> 3.2k tokens" 式的假压缩结果)。
fn dispatch_slash(name: &str, _args: &serde_json::Value) -> Result<String, String> {
    match name {
        // 真实 rc-core 压缩在 serve 模式不可达:serve 不持有跨请求的运行中 agent,
        // 也没有会话消息数组可压缩。见 serve_stdio 头注释(deferred 项)。
        "compact" => Ok("compact: 占位(未实现真实压缩)".to_string()),
        "clear" => Ok("clear: 占位(未实现真实新会话)".to_string()),
        // resume/model 在会话内未接线(分别对应 RequestMethod::Resume 的 prompt 语义
        // 与 ModelUse 的 registry 操作)。返回 Err 让调用方明确看到「未实现」,而不是
        // 假装成功。真实入口:`raincode run --resume` / `raincode model use <id>`。
        "resume" => Err(
            "resume: 未接线(会话内 resume 未实现;请用 `raincode run --resume`)".to_string(),
        ),
        "model" => Err(
            "model: 未接线(会话内切换模型未实现;请用 `raincode model use <id>`)".to_string(),
        ),
        "route" => Ok("route: 占位(对话内未执行路由;请在启动流程提交 prompt 触发真实路由)".to_string()),
        "risk" => Ok("risk: 占位(未实现风险模式切换)".to_string()),
        "status" => Ok("status: 占位(真实用量/上下文见监督看板)".to_string()),
        other => Err(format!("unknown command: {other}")),
    }
}

/// 一条 chat 输入应触发什么动作:显式 slash(`/compact`)或自然语言意图
/// 命中内置命令时走 slash 通道;否则仅确认(完整 chat loop 超出本里程碑范围)。
enum ChatAction {
    /// 命中内置命令:(命令名, 参数)。
    Slash(String, serde_json::Value),
    /// 未命中任何命令,原样确认。
    Ack,
}

fn dispatch_chat(text: &str) -> ChatAction {
    let t = text.trim();
    if t.is_empty() {
        return ChatAction::Ack;
    }
    if let Some(rest) = t.strip_prefix('/') {
        let (name, args_str) = rest
            .split_once(' ')
            .map(|(n, a)| (n, a.trim()))
            .unwrap_or((rest, ""));
        // 参数优先按 JSON 解析;裸词(如 `/model deepseek`)按命令键位映射。
        let args = match serde_json::from_str::<serde_json::Value>(args_str) {
            Ok(v) => v,
            Err(_) if !args_str.is_empty() => match name {
                "model" => json!({ "name": args_str }),
                "resume" => json!({ "id": args_str }),
                "risk" => json!({ "mode": args_str }),
                _ => json!({}),
            },
            _ => json!({}),
        };
        return ChatAction::Slash(name.to_string(), args);
    }
    // 自然语言意图(桌面验收清单:"压缩上下文" → chat 调 run_slash_command)。
    if t.contains("压缩") {
        return ChatAction::Slash("compact".into(), json!({}));
    }
    if t.contains("清空") || t.contains("清除") || t.contains("新会话") {
        return ChatAction::Slash("clear".into(), json!({}));
    }
    if t.contains("路由") || t.contains("拆分") {
        return ChatAction::Slash("route".into(), json!({}));
    }
    if t.contains("风险") || t.contains("降级") {
        return ChatAction::Slash("risk".into(), json!({}));
    }
    if t.contains("状态") || t.to_lowercase().contains("token") || t.contains("上下文") {
        return ChatAction::Slash("status".into(), json!({}));
    }
    if t.contains("模型") || t.to_lowercase().contains("model") {
        return ChatAction::Slash("model".into(), json!({}));
    }
    ChatAction::Ack
}

/// Steer 接管语义的最小后端:把指令追加到 `RAINCODE_HOME/steering/<agent_id>.jsonl`。
/// 完整实现需要在运行中 agent 的每个检查点读取 steering 文档(plan2 的 steering
/// 通道设计),serve 模式不持有跨请求的运行中 agent,故此处只做持久化记录 + 明确说明。
fn record_steering(agent_id: &str, text: &str) -> Result<std::path::PathBuf> {
    let dir = raincode_home().join("steering");
    std::fs::create_dir_all(&dir).context("steering dir")?;
    let file = dir.join(format!("{agent_id}.jsonl"));
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let mut out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&file)
        .context("open steering doc")?;
    writeln!(
        out,
        "{}",
        json!({ "ts": ts, "agent_id": agent_id, "text": text })
    )?;
    Ok(file)
}

/// `Tool` wrapper around `dispatch_slash`: parses `args.name` / `args.args` from
/// the model's call, routes to the matching command, and returns the text output.
struct SlashCommandTool {
    spec: ToolSpec,
}

fn slash_command_tool(spec: ToolSpec) -> Box<dyn Tool> {
    Box::new(SlashCommandTool { spec })
}

#[async_trait]
impl Tool for SlashCommandTool {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    async fn run(&self, args: Value, _ctx: &ToolContext) -> ToolResult {
        let Some(name) = args.get("name").and_then(Value::as_str) else {
            return ToolResult::err("missing 'name'");
        };
        let cmd_args = args.get("args").cloned().unwrap_or_else(|| json!({}));
        match dispatch_slash(name, &cmd_args) {
            Ok(output) => ToolResult::ok(output),
            Err(message) => ToolResult::err(message),
        }
    }
}

// 参数多但各自是 agent 装配的显式依赖;包 context struct 会大改多处调用点。
#[allow(clippy::too_many_arguments)]
async fn agent_config(
    config: &FileConfig,
    registry: &Registry,
    store: Store,
    skill_store: SkillStore,
    plan: bool,
    entropy: bool,
    interactive: bool,
    agent: Option<&str>,
    with_slash_command: bool,
) -> Result<AgentConfig> {
    let entropy_enabled = entropy || config.entropy.enabled.unwrap_or(false);
    let single_api = config
        .entropy
        .mode
        .as_deref()
        .map(|mode| mode != "multi")
        .unwrap_or(true);
    let provider = if single_api {
        make_provider(registry)?
    } else {
        match config.entropy.execute_profile.as_deref() {
            Some(id) => provider_for_profile(registry, Some(id))?,
            None => make_provider(registry)?,
        }
    };
    let plan_provider = if entropy_enabled {
        if single_api {
            Some(provider.clone())
        } else {
            Some(provider_for_profile(
                registry,
                config.entropy.plan_profile.as_deref(),
            )?)
        }
    } else {
        None
    };
    let review_provider = if entropy_enabled {
        if single_api {
            Some(provider.clone())
        } else {
            Some(provider_for_profile(
                registry,
                config.entropy.review_profile.as_deref(),
            )?)
        }
    } else {
        None
    };
    let cwd = config
        .core
        .workspace
        .as_deref()
        .map(expand_tilde)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let mut tools = default_tools(skill_store.clone());
    tools.extend(network_tools(SearchConfig::default()));
    // 用户驱动的斜杠命令出口:仅聊天上下文(桌面 ChatView 驱动的 agent)装配
    // run_slash_command;CLI 一次性 run(无实时 chat 循环)与 route 子任务(collect_executable
    // 只装 default_tools)都不带它,防止 executor/planner/reviewer 任意阶段误触发命令。
    // 分发本身无害(dispatch_slash 对未知命令返回 Err),此处加门是显式收缩工具面。
    if with_slash_command {
        tools.push(slash_command_tool(slash_command_spec()));
    }
    let mut mcp_servers: BTreeMap<String, Vec<String>> = BTreeMap::new();
    if !config.mcp.servers.is_empty() {
        match McpManager::connect_all(&config.mcp.servers).await {
            Ok(manager) => {
                for tool in manager.tools {
                    let spec = tool.spec();
                    if let Some(rest) = spec.name.strip_prefix("mcp__") {
                        if let Some((server, tool_name)) = rest.split_once('_') {
                            mcp_servers
                                .entry(server.to_string())
                                .or_default()
                                .push(tool_name.to_string());
                        }
                    }
                    tools.push(Box::new(tool));
                }
            }
            Err(e) => tracing::warn!("MCP servers skipped: {e}"),
        }
    }
    // store 随后被移入 AgentConfig,先查真实 context_window。
    let context_window = context_for_model(&store, registry);
    // 监督守卫:从 ~/.raincode/supervise.toml 加载(文件缺失默认守卫全开)。
    // 坏 TOML 不静默失败 — 记 warn 并关闭守卫(错误策略文件不应卡死所有工具)。
    let guard_cfg = match rc_sandbox::load_supervise_config(&raincode_home()) {
        Ok(cfg) => Some(cfg),
        Err(e) => {
            tracing::warn!("supervise.toml 解析失败,守卫关闭: {e}");
            None
        }
    };
    let guard_memo = guard_cfg
        .as_ref()
        .map(|_| std::sync::Arc::new(rc_sandbox::guard_hook::SessionGuardMemo::default()));
    let guard_home = guard_cfg.as_ref().map(|_| raincode_home());
    // 子代理工厂(delegate_research 工具):用活跃 provider + 新会话建聚焦子代理,
    // 跑查询/研究/小任务,返回最终文本。子代理不自带 delegate(防递归派发)。
    let sub_provider = provider.clone();
    let sub_skill = skill_store.clone();
    let sub_cwd = cwd.clone();
    let sub_guard_cfg = guard_cfg.clone();
    let sub_guard_memo = guard_memo.clone();
    let sub_guard_home = guard_home.clone();
    let subagent: Option<std::sync::Arc<rc_tool::SubagentFn>> = Some(std::sync::Arc::new(
        move |task: String| {
            let provider = sub_provider.clone();
            let skill_store = sub_skill.clone();
            let cwd = sub_cwd.clone();
            // Fn 闭包可多次调用:守卫件在闭包内再 clone,async move 才可消费。
            let guard_cfg = sub_guard_cfg.clone();
            let guard_memo = sub_guard_memo.clone();
            let guard_home = sub_guard_home.clone();
            Box::pin(async move {
                let store = Store::open(state_path()).map_err(|e| e.to_string())?;
                let session = store
                    .create_session(&cwd.to_string_lossy())
                    .map_err(|e| e.to_string())?;
                let mut tools = default_tools(skill_store.clone());
                tools.retain(|t| t.spec().name != "delegate_research");
                tools.extend(network_tools(SearchConfig::default()));
                let cfg = AgentConfig {
                    provider,
                    plan_provider: None,
                    review_provider: None,
                    store,
                    skill_store,
                    tools,
                    approval: std::sync::Arc::new(AutoApproveHook),
                    command_policy: CommandPolicy::default(),
                    network_policy: NetworkPolicy::default(),
                    cwd,
                    state_path: state_path(),
                    max_turns: 6,
                    max_steps: 0,
                    evolve_on_finish: false,
                    plan_mode: false,
                    hooks: HooksConfig::default(),
                    agent: Some("researcher".into()),
                    max_history_bytes: Some(64 * 1024),
                    mcp_servers: vec![],
                    entropy_mode: false,
                    plan_max_rounds: 1,
                    plan_max_questions: 1,
                    review_max_rounds: 1,
                    max_cycles: 1,
                    user_input: std::sync::Arc::new(AutoUserHook::default()),
                    steer_rx: None,
                    context_window: 0,
                    subagent: None,
                    // 子代理也带守卫(不可绕过);无 hook → 高危操作保守拦截。
                    guard_cfg: guard_cfg.clone(),
                    guard_hook: None,
                    guard_memo: guard_memo.clone(),
                    guard_home: guard_home.clone(),
                };
                let agent = Agent::new(cfg);
                let mut stream = agent.run(session.id, task);
                let mut final_text = String::new();
                while let Some(ev) = stream.next().await {
                    match ev {
                        AgentEvent::Token { delta } => final_text.push_str(&delta),
                        AgentEvent::Done { summary, .. } => final_text = summary,
                        AgentEvent::Error { message } => return Err(message),
                        _ => {}
                    }
                }
                Ok(final_text.trim().to_string())
            })
        },
    ));
    Ok(AgentConfig {
        provider,
        plan_provider,
        review_provider,
        store,
        skill_store,
        tools,
        approval: approval_hook(config.core.approval_mode.as_deref().unwrap_or("ask")),
        command_policy: CommandPolicy {
            allow: config.sandbox.commands.allow.clone(),
            deny: config.sandbox.commands.deny.clone(),
        },
        network_policy: network_policy(config),
        cwd,
        state_path: state_path(),
        max_turns: config.core.max_turns.unwrap_or(24),
        max_steps: 0,
        max_history_bytes: config.core.max_history_bytes,
        mcp_servers: mcp_servers.into_iter().collect(),
        entropy_mode: entropy_enabled,
        plan_max_rounds: config.entropy.plan_max_rounds.unwrap_or(6),
        plan_max_questions: config.entropy.plan_max_questions.unwrap_or(5),
        review_max_rounds: config.entropy.review_max_rounds.unwrap_or(3),
        max_cycles: config.entropy.max_cycles.unwrap_or(3),
        user_input: user_input_hook(interactive),
        steer_rx: None,
        context_window,
        subagent,
        // 监督守卫(有值则所有工具执行前过 guard_check;TUI 会覆盖 guard_hook 为真实弹窗)。
        guard_cfg,
        guard_hook: None,
        guard_memo,
        guard_home,
        evolve_on_finish: config.evolve.enabled.unwrap_or(true),
        plan_mode: plan,
        hooks: config.hooks.clone(),
        agent: Some(
            agent
                .or(config.core.agent.as_deref())
                .unwrap_or(DEFAULT_AGENT)
                .to_string(),
        ),
    })
}

/// 从 DB 能力画像或供应商 catalog 查当前活跃模型的真实 context_window(token)。
/// 查不到返回 0,由 rc-core 退用 128k 兜底。
fn context_for_model(store: &Store, registry: &Registry) -> u64 {
    let active = registry.active();
    let Some(model) = active.map(|p| p.model.clone()) else {
        return 0;
    };
    // 1) DB 能力画像(profiles refresh 建的 model_profiles)优先。
    if let Ok(rows) = store.all_model_profiles() {
        if let Some(r) = rows.iter().find(|r| r.model == model) {
            return r.context_window as u64;
        }
    }
    // 2) 回退:供应商 catalog 的 context_window。
    //    先按 profile id 找 provider;找不到则遍历 catalog 找 model 所属条目
    //    (如 profile.id="deepseek-v4-flash" 而 catalog provider id="deepseek")。
    if let Some(pid) = active.map(|p| p.id.as_str()) {
        if let Some(entry) = rc_profile::catalog::find(pid) {
            return entry.context_window as u64;
        }
    }
    for entry in rc_profile::catalog::catalog() {
        if entry.models.iter().any(|m| *m == model) {
            return entry.context_window as u64;
        }
    }
    0
}

async fn run_prompt(
    config: &FileConfig,
    prompt: &str,
    resume: bool,
    plan: bool,
    entropy: bool,
    agent: Option<&str>,
) -> Result<()> {
    let registry = load_registry()?;
    let skill_dir = skills_dir(config);
    ensure_seed(&skill_dir)?;
    let store = Store::open(state_path())?;
    sync_skill_index(&store, &SkillStore::new(&skill_dir))?;
    let workspace = config
        .core
        .workspace
        .as_deref()
        .map(expand_tilde)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let session = if resume {
        store
            .list_sessions(1)?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("no session to resume"))?
    } else {
        store.create_session(&workspace.to_string_lossy())?
    };
    let skill_store = SkillStore::new(&skill_dir);
    let agent_cfg = agent_config(
        config,
        &registry,
        store,
        skill_store,
        plan,
        entropy,
        true,
        agent,
        false, // CLI 一次性 run:无实时 chat 循环,不装配 run_slash_command 工具
    )
    .await?;
    let agent = Agent::new(agent_cfg);
    let mut stream = agent.run(session.id.clone(), prompt.to_string());
    while let Some(event) = stream.next().await {
        print_event(&event);
    }
    Ok(())
}

fn print_event(event: &AgentEvent) {
    match event {
        AgentEvent::Token { delta } => print!("{delta}"),
        AgentEvent::Thinking { delta } => print!("\n[think] {delta}"),
        AgentEvent::ToolCall { name, args, .. } => print!("\n[tool] {name} {}", args),
        AgentEvent::ToolResult {
            name,
            ok,
            output,
            output_path,
            ..
        } => {
            print!("\n[result] {name} ok={ok}\n{output}");
            if let Some(path) = output_path {
                print!("\n[full output] {path}");
            }
        }
        AgentEvent::SkillSuggested {
            name,
            category,
            confidence,
        } => {
            print!("\n[skill] {name} ({category}, {confidence:.2})")
        }
        AgentEvent::SkillLoaded { name, path } => print!("\n[skill-loaded] {name} ({path})"),
        AgentEvent::AskingApproval {
            tool, description, ..
        } => print!("\n[approval] {tool}: {description}"),
        AgentEvent::AskingQuestion { question, .. } => print!("\n[question] {question}"),
        AgentEvent::McpToolList { server, tools } => {
            print!("\n[mcp] {server}: {}", tools.join(", "))
        }
        AgentEvent::SessionStarted { session_id } => print!("\n[session] {session_id}"),
        AgentEvent::Done {
            summary,
            session_id,
            ..
        } => {
            println!("\n[done] {session_id}\n{summary}")
        }
        AgentEvent::PlanProposed {
            summary,
            session_id,
        } => {
            println!("\n[plan] {session_id}\n{summary}")
        }
        AgentEvent::PhaseChanged { phase, cycle, .. } => {
            print!("\n[phase] {phase} (cycle {cycle})")
        }
        AgentEvent::ReviewProposed {
            verdict,
            reason,
            next_intent,
            summary,
            cycle,
            ..
        } => {
            println!("\n[review] cycle {cycle} {verdict}: {reason}\n{summary}");
            if !next_intent.is_empty() {
                println!("[next-intent] {next_intent}");
            }
        }
        AgentEvent::AgentSpawned { id, model, role, task } => {
            println!("\n[agent] spawn {role} {id} ({model})\n{task}")
        }
        AgentEvent::AgentToolCall { id, tool, args_preview } => {
            println!("\n[agent] {id} -> {tool}: {args_preview}")
        }
        AgentEvent::AgentStatus {
            id,
            phase,
            tokens,
            elapsed_ms,
        } => print!("\n[agent] {id} {phase} (chars {tokens}, {elapsed_ms}ms)"),
        AgentEvent::AgentResult { id, verdict, tests, cost } => {
            println!("\n[agent] {id} {verdict} (tests: {tests}, cost {cost:.4})")
        }
        AgentEvent::ContextUpdate { used, limit, pct, .. } => {
            println!("\n[context] {used}/{limit} ({pct}%)")
        }
        AgentEvent::Error { message } => eprintln!("\n[error] {message}"),
        AgentEvent::OrchestratorPlan { node_id, plan } => {
            println!("\n[plan] {node_id}: {plan}")
        }
        AgentEvent::OrchestratorDispatch { child_id, prompt, model, .. } => {
            println!("\n[dispatch] {child_id} ({model}) {prompt}")
        }
        AgentEvent::OrchestratorResult { node_id, status, summary } => {
            println!("\n[result] {node_id} {status}: {summary}")
        }
    }
    io::stdout().flush().ok();
}

async fn daemon_command(config: &FileConfig, once: bool) -> Result<()> {
    let registry = load_registry()?;
    let skill_dir = skills_dir(config);
    ensure_seed(&skill_dir)?;
    let store = Store::open(state_path())?;
    sync_skill_index(&store, &SkillStore::new(&skill_dir))?;
    let daemon_config = DaemonConfig {
        interval_minutes: config.evolve.daemon_interval_minutes.unwrap_or(15),
        min_cluster: config.evolve.daemon_min_cluster.unwrap_or(3),
        similarity_threshold: config.evolve.similarity_threshold.unwrap_or(0.78),
        coverage_factor: 0.95,
        stale_days: 30,
        lock_path: config
            .evolve
            .daemon_lock
            .clone()
            .unwrap_or_else(|| "~/.raincode/.daemon.lock".into()),
        enabled: config.evolve.enabled.unwrap_or(true),
        auto_approve: config.evolve.auto_approve.unwrap_or(true),
    };
    let provider = make_provider(&registry)?;
    let daemon = PatternDaemon::new(provider, store, SkillStore::new(&skill_dir), daemon_config);
    if once {
        let report = daemon.scan().await?;
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        daemon.run().await?;
    }
    Ok(())
}

async fn evolve_command(config: &FileConfig, session: Option<&str>) -> Result<()> {
    let registry = load_registry()?;
    let skill_dir = skills_dir(config);
    ensure_seed(&skill_dir)?;
    let store = Store::open(state_path())?;
    sync_skill_index(&store, &SkillStore::new(&skill_dir))?;
    let session_id = match session {
        Some(id) => id.to_string(),
        None => {
            store
                .list_sessions(1)?
                .into_iter()
                .next()
                .ok_or_else(|| anyhow!("no sessions found"))?
                .id
        }
    };
    let provider = make_provider(&registry)?;
    let mut engine = EvolveEngine::new(
        provider,
        store,
        SkillStore::new(&skill_dir),
        EvolveConfig {
            min_experiences: config.evolve.min_experiences.unwrap_or(3),
            similarity_threshold: config.evolve.similarity_threshold.unwrap_or(0.78),
            auto_approve: config.evolve.auto_approve.unwrap_or(true),
        },
    );
    let report = engine.digest(&session_id).await?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

async fn insights_command(config: &FileConfig, scan: bool) -> Result<()> {
    let skill_dir = skills_dir(config);
    ensure_seed(&skill_dir)?;
    let store = Store::open(state_path())?;
    sync_skill_index(&store, &SkillStore::new(&skill_dir))?;
    if scan {
        let registry = load_registry()?;
        let provider = make_provider(&registry)?;
        let daemon = PatternDaemon::new(
            provider,
            store,
            SkillStore::new(&skill_dir),
            DaemonConfig::default(),
        );
        let report = daemon.scan().await?;
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    let experiences = store.list_experiences(None)?;
    let skills = SkillStore::new(&skill_dir).discover();
    println!("experiences: {}", experiences.len());
    for exp in &experiences {
        println!(
            "- {} | {} | {} | {}",
            exp.id, exp.task_signature, exp.category_guess, exp.outcome
        );
    }
    println!("\nskills: {}", skills.len());
    for skill in &skills {
        println!(
            "- {} ({} v{}, conf {:.2}, used {})",
            skill.name, skill.category, skill.version, skill.confidence, skill.usage_count
        );
    }
    Ok(())
}

async fn skills_command(config: &FileConfig, cmd: SkillsCmd) -> Result<()> {
    let skill_dir = skills_dir(config);
    ensure_seed(&skill_dir)?;
    let store = Store::open(state_path())?;
    let skills = SkillStore::new(&skill_dir);
    sync_skill_index(&store, &skills)?;
    match cmd {
        SkillsCmd::List => {
            for skill in skills.discover() {
                println!(
                    "{} ({}) conf={:.2} auto={}",
                    skill.name, skill.category, skill.confidence, skill.auto
                );
            }
        }
        SkillsCmd::Show { name } => match skills.load(&name) {
            Some(skill) => print!(
                "{}",
                skill.render().unwrap_or_else(|_| "render failed".into())
            ),
            None => return Err(anyhow!("skill '{name}' not found")),
        },
        SkillsCmd::Create {
            name,
            category,
            description,
            body,
        } => {
            if name.trim().is_empty() || category.trim().is_empty() {
                return Err(anyhow!("name and category are required"));
            }
            if skills.load(&name).is_some() {
                return Err(anyhow!("skill '{name}' already exists"));
            }
            let skill = Skill {
                name: name.clone(),
                description,
                short_description: None,
                category,
                path: PathBuf::new(),
                body: body
                    .unwrap_or_else(|| format!("# {name}\n\nWrite the reusable method here.\n")),
                relations: vec![],
                triggers: vec![],
                tags: vec![],
                version: 1,
                confidence: 0.5,
                usage_count: 0,
                success_rate: 0.0,
                last_used: None,
                auto: false,
                origin: "manual".into(),
                origin_url: None,
                scope: "user".into(),
                allow_implicit: true,
                embedding: None,
            };
            let path = skills.save(&skill).map_err(|e| anyhow!(e))?;
            sync_skill_index(&store, &skills)?;
            println!("created {}", path.display());
        }
        SkillsCmd::Edit {
            name,
            description,
            category,
            body,
        } => {
            let mut skill = skills
                .load(&name)
                .ok_or_else(|| anyhow!("skill '{name}' not found"))?;
            if let Some(description) = description {
                skill.description = description;
            }
            if let Some(category) = category {
                skill.category = category;
            }
            if let Some(body) = body {
                skill.body = body;
                skill.version += 1;
            }
            let path = skills.save(&skill).map_err(|e| anyhow!(e))?;
            sync_skill_index(&store, &skills)?;
            println!("updated {}", path.display());
        }
        SkillsCmd::Install { spec } => {
            let source: Box<dyn SkillSource> =
                if spec.starts_with("http") || spec.contains('/') && !Path::new(&spec).exists() {
                    Box::new(RemoteSource::new())
                } else {
                    Box::new(LocalSource::new(PathBuf::from(&spec)))
                };
            let report = source.install(&spec, &skill_dir).await?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            let store = Store::open(state_path())?;
            sync_skill_index(&store, &skills)?;
        }
        SkillsCmd::Update { spec } => {
            let source: Box<dyn SkillSource> =
                if spec.starts_with("http") || spec.contains('/') && !Path::new(&spec).exists() {
                    Box::new(RemoteSource::new())
                } else {
                    Box::new(LocalSource::new(PathBuf::from(&spec)))
                };
            let report = source.install(&spec, &skill_dir).await?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            sync_skill_index(&store, &skills)?;
        }
        SkillsCmd::Uninstall { name } => {
            skills.remove(&name).map_err(|e| anyhow!(e))?;
            store.delete_skill(&name)?;
            println!("removed {name}");
        }
        SkillsCmd::Search { query } => {
            let source = RemoteSource::new();
            for hit in source.search(&query).await {
                println!("{} | {} | {}", hit.name, hit.description, hit.url);
            }
        }
        SkillsCmd::Review {
            name,
            approve,
            reject,
        } => {
            if approve || reject {
                // 优先 review 隔离区(pending 确认的 auto skill)。
                let review_skill = name
                    .as_deref()
                    .and_then(|n| skills.discover_review().into_iter().find(|s| s.name == n));
                if let Some(skill) = review_skill {
                    let skill_name = skill.name.clone();
                    if reject {
                        skills.reject(&skill_name).map_err(|e| anyhow!(e))?;
                        store.delete_skill(&skill_name)?;
                        store.add_audit("skills.review.reject", &format!("rejected {skill_name}"), "user")?;
                        println!("rejected {skill_name}");
                    } else {
                        let path = skills.approve(&skill_name).map_err(|e| anyhow!(e))?;
                        // load 可能为 None(approve 成功但加载失败):报错而非 panic。
                        let Some(loaded) = skills.load(&skill_name) else {
                            println!("failed to load {skill_name} after approve");
                            return Ok(());
                        };
                        store.upsert_skill(&loaded.to_row())?;
                        store.add_audit(
                            "skills.review.approve",
                            &format!("approved {} at {}", skill_name, path.display()),
                            "user",
                        )?;
                        println!("approved {}", path.display());
                    }
                    return Ok(());
                }
                // 回退:review 已有 skill 的 approve/reject(转正 auto skill)。
                let skill = skills.load(name.as_deref().unwrap_or("")).ok_or_else(|| {
                    anyhow!("skill '{}' not found", name.as_deref().unwrap_or(""))
                })?;
                if reject {
                    let name = skill.name.clone();
                    skills.remove(&name).map_err(|e| anyhow!(e))?;
                    store.delete_skill(&name)?;
                    store.add_audit("skills.review.reject", &format!("rejected {name}"), "user")?;
                    println!("rejected {name}");
                } else {
                    let mut approved = skill;
                    approved.auto = false;
                    approved.confidence = approved.confidence.max(0.7);
                    let path = skills.save(&approved).map_err(|e| anyhow!(e))?;
                    store.upsert_skill(&approved.to_row())?;
                    store.add_audit(
                        "skills.review.approve",
                        &format!("approved {} at {}", approved.name, path.display()),
                        "user",
                    )?;
                    println!("approved {}", path.display());
                }
            } else {
                // 列出待确认的 review 区 skill(优先)+ 已有 auto skill。
                let mut listed = false;
                for skill in skills.discover_review() {
                    println!(
                        "[PENDING] {} ({}) conf={:.2} origin={} — run `skills review {} --approve|--reject`",
                        skill.name, skill.category, skill.confidence, skill.origin, skill.name
                    );
                    listed = true;
                }
                for skill in skills.discover().into_iter().filter(|s| s.auto) {
                    println!(
                        "[AUTO] {} ({}) conf={:.2} origin={}",
                        skill.name, skill.category, skill.confidence, skill.origin
                    );
                }
                if !listed {
                    let _ = listed;
                }
            }
        }
        SkillsCmd::Stats => {
            let all = skills.discover();
            println!(
                "total={} auto={} seed={}",
                all.len(),
                all.iter().filter(|s| s.auto).count(),
                all.iter().filter(|s| s.origin == "seed").count()
            );
        }
        SkillsCmd::Graph => {
            let all = skills.discover();
            if let Err(msg) = rc_skill::validate_dag(&all) {
                eprintln!("warning: {msg}");
            }
            for skill in all {
                for rel in &skill.relations {
                    println!("{} --{}--> {}", skill.name, rel.kind.as_str(), rel.skill);
                }
            }
        }
    }
    Ok(())
}

async fn model_command(_config: &FileConfig, cmd: ModelCmd) -> Result<()> {
    let mut registry = load_registry()?;
    match cmd {
        ModelCmd::List => {
            for profile in &registry.profiles {
                let active = registry.active_id.as_deref() == Some(profile.id.as_str());
                let key = if profile.api_key_file.is_some() {
                    " key=file"
                } else if profile.api_key_env.is_some() {
                    " key=env"
                } else if profile.api_key.is_some() {
                    " key=inline"
                } else {
                    ""
                };
                println!(
                    "{}{} | {} | {}{} | {}",
                    if active { "* " } else { "  " },
                    profile.id,
                    profile.kind.as_str(),
                    profile.model,
                    key,
                    profile.base_url
                );
            }
        }
        ModelCmd::Catalog => {
            for entry in rc_profile::catalog::catalog() {
                println!(
                    "{:<12} {:<28} {} | env: {}",
                    entry.id,
                    entry.display_name,
                    entry.default_model,
                    entry.env_var.unwrap_or("-")
                );
            }
        }
        ModelCmd::Use { id, app } => {
            registry.set_active(&id)?;
            save_registry(&registry)?;
            if let Some(app_name) = app {
                let profile = active_profile(&registry)?;
                for writer in all_writers() {
                    if writer.app() == app_name {
                        writer.apply(&profile)?;
                        println!("wrote {} config", app_name);
                    }
                }
            }
            println!("active profile: {id}");
        }
        ModelCmd::Add {
            id,
            name,
            provider,
            kind,
            model,
            base_url,
            api_key,
            api_key_env,
            app,
            no_verify,
        } => {
            let mut profile = if let Some(provider_id) = provider {
                let entry = find_provider(&provider_id)
                    .ok_or_else(|| anyhow!("unknown provider '{provider_id}'"))?;
                Profile {
                    id: id.clone(),
                    name: name.unwrap_or_else(|| format!("{} ({})", entry.display_name, id)),
                    app: app.unwrap_or_else(|| "raincode".into()),
                    kind: entry.kind,
                    base_url: base_url.unwrap_or_else(|| entry.base_url.to_string()),
                    model: model.unwrap_or_else(|| entry.default_model.to_string()),
                    api_key: None,
                    api_key_env: api_key_env.or_else(|| entry.env_var.map(str::to_string)),
                    api_key_file: None,
                    embedding_model: entry.embedding_model.map(str::to_string),
                    headers: Default::default(),
                    extra: json!({}),
                }
            } else {
                Profile {
                    id: id.clone(),
                    name: name.unwrap_or_else(|| id.clone()),
                    app: app.unwrap_or_else(|| "raincode".into()),
                    kind: ProfileKind::from_str(kind.as_deref().unwrap_or("openai-compat")),
                    base_url: base_url.unwrap_or_default(),
                    model: model.unwrap_or_else(|| "gpt-4o-mini".to_string()),
                    api_key: None,
                    api_key_env,
                    api_key_file: None,
                    embedding_model: None,
                    headers: Default::default(),
                    extra: json!({}),
                }
            };
            // 连通性自检:非本地供应商、且用户提供了 key 时,先验证再保存。
            // 本地(ollama/lmstudio/vllm)无 key,跳过;用户明确 --no-verify 才跳过。
            let is_local = matches!(profile.kind, ProfileKind::Ollama)
                || profile.base_url.contains("localhost")
                || profile.base_url.contains("127.0.0.1");
            let verify = !no_verify && !is_local && api_key.is_some();
            if verify {
                // 先用临时 profile(key 注入内存)验证,通过后才真正写 key 文件 + 保存。
                let mut probe = profile.clone();
                if let Some(key) = &api_key {
                    probe.api_key = Some(key.clone());
                }
                match verify_provider_connectivity(&probe).await {
                    Ok(msg) => println!("{msg}"),
                    Err(e) => {
                        // 验证失败:不保存,提示重试或 --no-verify 跳过。
                        eprintln!("[verify] 连接失败,未保存: {e}");
                        eprintln!("[verify] 若确定 key 无误,可用 --no-verify 跳过检查");
                        return Ok(());
                    }
                }
            }
            if let Some(key) = api_key {
                store_key(&id, &key)?;
                profile.api_key = None;
                profile.api_key_file = Some(key_ref(&id));
            }
            registry.add(profile);
            save_registry(&registry)?;
            println!("added profile {id}");
        }
        ModelCmd::SetKey { id, api_key } => {
            let key = match api_key {
                Some(key) => key,
                None => {
                    let mut line = String::new();
                    print!("Paste API key: ");
                    io::stdout().flush().ok();
                    io::stdin().lock().read_line(&mut line).ok();
                    line.trim().to_string()
                }
            };
            store_key(&id, &key)?;
            if let Some(profile) = registry.get_mut(&id) {
                profile.api_key = None;
                profile.api_key_file = Some(key_ref(&id));
            } else {
                return Err(anyhow!("profile '{id}' not found"));
            }
            save_registry(&registry)?;
            println!("stored key for {id}");
        }
        ModelCmd::Remove { id } => {
            registry.remove(&id);
            delete_key(&id)?;
            save_registry(&registry)?;
            println!("removed profile {id}");
        }
        ModelCmd::Import { db, link } => {
            let imports: Vec<ProfileImport> = if let Some(link) = link {
                parse_deeplink(&link).into_iter().collect()
            } else {
                let db_path = db.unwrap_or_else(|| {
                    dirs::home_dir()
                        .unwrap_or_else(|| PathBuf::from("."))
                        .join(".cc-switch")
                        .join("cc-switch.db")
                        .to_string_lossy()
                        .to_string()
                });
                import_from_db(PathBuf::from(db_path))?
            };
            if imports.is_empty() {
                return Err(anyhow!("no profiles imported"));
            }
            for imported in &imports {
                let id = slugify(&imported.name);
                let mut profile = imported.to_profile(id);
                protect_profile(&mut profile)?;
                registry.add(profile);
            }
            save_registry(&registry)?;
            println!("imported {} profiles", imports.len());
        }
        ModelCmd::Test { prompt } => {
            let prompt = prompt.unwrap_or_else(|| "Say hello in one sentence.".to_string());
            let provider = make_provider(&registry)?;
            let req = rc_pro::CanonicalRequest {
                model: provider.id().to_string(),
                messages: vec![rc_pro::CanonicalMessage::user(prompt)],
                tools: vec![],
                temperature: Some(0.2),
                max_tokens: Some(200),
                stream: true,
                extra: json!({}),
            };
            let mut stream = provider.stream(req).await?;
            while let Some(event) = stream.next().await {
                if let rc_pro::ProvEvent::Delta { text } = event? {
                    print!("{text}");
                    io::stdout().flush().ok();
                }
            }
            println!();
        }
    }
    Ok(())
}

/// OpenRouter `/api/v1/models` entry (fields we care about; the rest is ignored).
#[derive(Debug, Deserialize)]
struct OpenRouterModel {
    id: String,
    #[serde(rename = "context_length", default)]
    context_length: u64,
    #[serde(default)]
    pricing: OpenRouterPricing,
    #[serde(default)]
    benchmarks: OpenRouterBenchmarks,
}

#[derive(Debug, Deserialize, Default)]
struct OpenRouterPricing {
    #[serde(default)]
    prompt: String,
    #[serde(default)]
    completion: String,
}

/// OpenRouter 模型的公开榜单分(真实数据,非编造):
/// `artificial_analysis`(Artificial Analysis 0-100 指数)与 `design_arena`(LMArena Elo)。
#[derive(Debug, Deserialize, Default)]
struct OpenRouterBenchmarks {
    #[serde(default)]
    artificial_analysis: Option<ArtificialAnalysis>,
    #[serde(default)]
    design_arena: Option<Vec<DesignArenaEntry>>,
}

#[derive(Debug, Deserialize, Clone, Default)]
struct ArtificialAnalysis {
    #[serde(default)]
    intelligence_index: Option<f64>,
    #[serde(default)]
    coding_index: Option<f64>,
}

#[derive(Debug, Deserialize, Clone, Default)]
struct DesignArenaEntry {
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    elo: Option<f64>,
}

/// 解析 OpenRouter /api/v1/models 的 data 数组 → CapabilityProfileRow
/// (能力分暂用种子同名回退,定价/窗口取真实值)。
fn parse_openrouter_models(raw: &str) -> Result<Vec<rc_state::CapabilityProfileRow>, serde_json::Error> {
    #[derive(Deserialize)]
    struct Wrapper {
        data: Vec<OpenRouterModel>,
    }
    let w: Wrapper = serde_json::from_str(raw)?;
    Ok(w.data
        .into_iter()
        .map(|m| {
            let inp = m.pricing.prompt.parse::<f64>().unwrap_or(0.0);
            let outp = m.pricing.completion.parse::<f64>().unwrap_or(0.0);
            // context_length 缺省/为 0 时用默认窗口兜底,不让刷新整体失败。
            let context = if m.context_length == 0 { 128_000 } else { m.context_length as u32 };
            // 真实榜单分(非编造):artificial_analysis 指数直接是 0-100;design_arena
            // 是 Elo(~1000-1400),归一化到 0-100。缺失的维度=0(诚实:无公开数据)。
            let aa = m.benchmarks.artificial_analysis.clone().unwrap_or_default();
            let reasoning = aa.intelligence_index.unwrap_or(0.0);
            let coding = aa.coding_index.unwrap_or(0.0);
            let math = aa.intelligence_index.unwrap_or(0.0); // 无独立 math 榜单,用 intelligence 代理
            let da = m.benchmarks.design_arena.clone().unwrap_or_default();
            let elo = |cat: &str| da.iter()
                .find(|e| e.category.as_deref() == Some(cat))
                .and_then(|e| e.elo)
                .map(normalize_arena_elo)
                .unwrap_or(0.0);
            let frontend = elo("website");
            let backend = elo("codecategories");
            // long_context:由真实 context_length 归一化(128k 起步,1M 满分)。
            let long_context = ((context as f64 / 128_000.0).min(1.0) * 100.0).max(0.0);
            let has_benchmarks = aa.intelligence_index.is_some() || !da.is_empty();
            rc_state::CapabilityProfileRow {
                model: m.id,
                reasoning,
                coding,
                frontend,
                backend,
                math,
                long_context,
                input_cost_per_m: inp.max(0.0001),
                output_cost_per_m: outp.max(0.0001),
                context_window: context,
                source: if has_benchmarks { "openrouter-arena".into() } else { "openrouter".into() },
                updated_at: "now".into(),
                multimodal: false,
            }
        })
        .collect())
}

/// LMArena Elo(~1000-1400)→ 0-100:1000≈0 分,1400≈100 分。
fn normalize_arena_elo(elo: f64) -> f64 {
    ((elo - 1000.0) / 400.0 * 100.0).clamp(0.0, 100.0)
}

async fn profiles_command(_config: &FileConfig, cmd: ProfilesCmd) -> Result<()> {
    let store = Store::open(state_path())?;
    match cmd {
        ProfilesCmd::Show => {
            let rows = store.all_model_profiles()?;
            for r in &rows {
                println!(
                    "{} · ctx {} · in {} out {} · multimodal {}",
                    r.model, r.context_window, r.input_cost_per_m, r.output_cost_per_m, r.multimodal
                );
            }
            Ok(())
        }
        ProfilesCmd::Refresh => {
            let n = refresh_profiles(&store).await?;
            println!("refreshed {n} models");
            Ok(())
        }
    }
}

/// 从 OpenRouter 拉取模型列表 + 公开榜单分并入库,返回写入的模型数。
/// 供 CLI `profiles refresh` 与 REPL `/refresh` 命令共用。
pub async fn refresh_profiles(store: &Store) -> Result<usize> {
    // /api/v1/models 是公开接口,key 可选(有 key 时带上防限流,不落库、不打印)。
    let key = std::fs::read_to_string(rc_profile::secrets::key_path("openrouter"))
        .unwrap_or_default()
        .trim()
        .to_string();
    let url = "https://openrouter.ai/api/v1/models";
    let client = reqwest::Client::new();
    let mut req = client.get(url);
    if !key.is_empty() {
        req = req.header("Authorization", format!("Bearer {key}"));
    }
    let resp = req.send().await?;
    // 显式检查 HTTP 状态:非 2xx 立即报错,而不是带着错误 body 往下解析。
    let resp = resp.error_for_status()?;
    let raw = resp.text().await?;
    let rows = parse_openrouter_models(&raw)?;
    for r in &rows {
        store.upsert_model_profile(r)?;
    }
    Ok(rows.len())
}

/// 为真实配置但 DB/seed 无画像的模型合成一个默认能力画像(中位能力、平价)。
/// 保证 dispatch 只在用户真实模型里选,不会派给没有 key 的 seed 幽灵模型。
fn default_capability_profile(model: &str) -> CapabilityProfile {
    CapabilityProfile {
        model: model.into(),
        reasoning: 70.0,
        coding: 70.0,
        frontend: 70.0,
        backend: 70.0,
        math: 70.0,
        long_context: 70.0,
        input_cost_per_m: 1.0,
        output_cost_per_m: 3.0,
        context_window: 128_000,
        provenance: "default".into(),
        multimodal: false,
    }
}

/// 把 registry 裸模型名匹配到 OpenRouter 真实画像:suffix 匹配(deepseek-v4-flash ↔
/// deepseek/deepseek-v4-flash-0731),多个候选时优先"榜单数据最全"的(有实测值而非 0)。
fn resolve_capability_profile(
    profiles: &[CapabilityProfile],
    model: &str,
) -> Option<CapabilityProfile> {
    // 1) 精确匹配。
    if let Some(p) = profiles.iter().find(|p| p.model == model) {
        return Some(p.clone());
    }
    // 2) suffix 匹配:OpenRouter id 末段 == model,或以 model- 开头(版本化 id)。
    let mut candidates: Vec<&CapabilityProfile> = profiles
        .iter()
        .filter(|p| {
            let last = p.model.rsplit('/').next().unwrap_or(&p.model);
            last == model || last.strip_prefix(model).is_some_and(|rest| rest.starts_with('-'))
        })
        .collect();
    if candidates.is_empty() {
        return None;
    }
    // 3) 榜单数据最全者优先(推理/编码/前端/后端都有实测值的胜出)。
    candidates.sort_by_key(|b| std::cmp::Reverse(benchmark_coverage(b)));
    candidates.into_iter().next().cloned()
}

/// 该画像有多少个能力维度有真实榜单分(>0)。
fn benchmark_coverage(p: &CapabilityProfile) -> u32 {
    [p.reasoning, p.coding, p.frontend, p.backend]
        .iter()
        .filter(|v| **v > 0.0)
        .count() as u32
}

/// 从 base_url 推导供应渠道显示名(区分不同渠道的同名模型,如 deepseek/ds-* vs
/// opencode/ds-*):去协议 → 去 api./www. 前缀 → 取域名首段。如
/// https://api.deepseek.com/v1 → deepseek;https://opencode.example/v1 → opencode。
fn provider_label(base_url: &str, fallback: &str) -> String {
    let host = base_url
        .replace("https://", "")
        .replace("http://", "")
        .split('/')
        .next()
        .unwrap_or("")
        .to_string();
    let stripped = host
        .strip_prefix("api.")
        .or_else(|| host.strip_prefix("www."))
        .unwrap_or(&host);
    let first = stripped.split('.').next().unwrap_or("").to_string();
    if first.is_empty() { fallback.to_string() } else { first }
}

/// `raincode route`: allocator decomposes the prompt into a sub-task graph, the
/// scoring engine dispatches each sub-task to a model profile, then sub-tasks
/// execute as independent rc-core agents (bounded concurrency).
///
/// 风险治理:抽查发现的撕裂信号会触发系统自动棘轮升级(链路 B)。用户主动降级通道
/// 与 `/risk` 模式切换属交互层功能,延后到交互计划(interaction plan)实现,此处
/// 只做系统自动棘轮升级 + escalation_log 输出,让链路可演示。
// 参数多但都是 route 引擎的显式依赖;包 context struct 会大改多个调用点。
#[allow(clippy::too_many_arguments)]
async fn route_command(
    config: &FileConfig,
    prompt: &str,
    plan_only: bool,
    pool: Option<String>,
    pin: Option<&str>,
    quiet: bool,
    emit: Option<Arc<dyn Fn(AgentEvent) + Send + Sync>>,
    steer_hub: Option<Arc<rc_core::SteerHub>>,
    cancel: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
    // TUI 传入 risk_approval_hook → 子代理跟随共享风险档;CLI 传 None → 按 config。
    subagent_approval: Option<Arc<dyn rc_sandbox::ApprovalHook>>,
    risk_mode: rc_router::risk::RiskMode,
) -> Result<()> {
    let registry = load_registry()?;
    let skill_dir = skills_dir(config);
    ensure_seed(&skill_dir)?;
    let store = Store::open(state_path())?;

    // 1) 能力画像:DB model_profiles(profiles refresh 拉取的真实榜单分)优先;空库用种子。
    let profiles_loaded: Vec<CapabilityProfile> = {
        let from_db = store.all_model_profiles()?;
        if from_db.is_empty() {
            rc_router::capability::seed_profiles()
        } else {
            from_db.into_iter().map(CapabilityProfile::from_row).collect()
        }
    };
    // 1.5) 只派发用户真实配置的模型(非 mock);每个模型匹配其真实榜单画像
    //     (OpenRouter arena/Artificial Analysis,见 resolve_capability_profile)。
    //     无画像的模型用默认兜底并提示跑 `profiles refresh` 获取真实评分。
    let registry_models: HashSet<String> = registry
        .profiles
        .iter()
        .filter(|p| p.kind != rc_profile::model::ProfileKind::Mock)
        .map(|p| p.model.clone())
        .collect();
    let mut profiles: Vec<CapabilityProfile> = Vec::new();
    let mut missing_real: Vec<String> = Vec::new();
    for model in &registry_models {
        if let Some(mut cp) = resolve_capability_profile(&profiles_loaded, model) {
            cp.model = model.clone(); // 用裸名,collect_executable 才能在 registry 精确匹配
            profiles.push(cp);
        } else {
            missing_real.push(model.clone());
            profiles.push(default_capability_profile(model));
        }
    }
    if !missing_real.is_empty() {
        eprintln!(
            "note: 模型 {} 无真实榜单画像,用默认分;跑 `raincode profiles refresh` 获取 OpenRouter/arena 评分",
            missing_real.join(", ")
        );
    }
    // 2) 用户意图:池过滤(--pool 逗号分隔;None/空 = 全部)
    let pool: Vec<String> = pool
        .map(|p| {
            p.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let profiles = rc_router::intent::filter_pool(profiles, &pool);
    if profiles.is_empty() {
        return Err(anyhow!(
            "no real (non-mock) model configured to route; run `raincode model add` first"
        ));
    }
    let cost = rc_router::cost::CostModel::new(5);
    let allocator = provider_for_profile(
        &registry,
        registry.active().or_else(|| registry.profiles.first()).map(|p| p.id.as_str()),
    )?;
    // 3) 递归执行(顶层 depth=0):同质短路 / 成本门 / MAX_DEPTH。
    let root = rc_router::capability::Subtask {
        id: "root".into(),
        description: prompt.into(),
        requirements: rc_router::capability::Requirements::default(),
        cost_pressure: rc_router::capability::CostPressure::Med,
        depends_on: vec![],
        risk: rc_router::capability::Risk::Med,
    };
    // 3.1) 树头即时出现:先发 OrchestratorPlan(根)+ 拆解阶段事件,再进递归。
    //     否则长拆解期间用户看不到任何东西(任务树/阶段都来自这些事件)。
    if let Some(emit) = &emit {
        emit(AgentEvent::OrchestratorPlan {
            node_id: "root".into(),
            plan: prompt.to_string(),
        });
        emit(AgentEvent::PhaseChanged {
            phase: "拆解".into(),
            cycle: 0,
            session_id: String::new(),
        });
    }
    // 分解预算:限制分配者 API 调用次数(推理模型每次 10-20s),防 run 拖到几分钟。
    let decompose_budget = std::sync::atomic::AtomicUsize::new(
        rc_router::recursion::DECOMPOSE_BUDGET,
    );
    let mut plan = match rc_router::recursion::process(
        root,
        0,
        &profiles,
        &cost,
        &store,
        allocator.as_ref(),
        emit.as_deref(),
        cancel,
        Some(&decompose_budget),
    )
    .await
    {
        Ok(p) => p,
        Err(rc_router::allocator::AllocatorError::Cancelled) => {
            // 用户 /stop:发 cancelled 让 TUI 显示"已中断"。
            if let Some(emit) = &emit {
                emit(AgentEvent::Error { message: "cancelled by user".into() });
            }
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    };
    // 3.5) 用户 pin(--pin id:model,逗号分隔):解析后过 apply_pins 回写计划叶子,覆盖路由结果。
    let pins = parse_pins(pin);
    apply_pins_to_plan(&mut plan, &pins);
    if !quiet {
        print_plan(&plan, 0);
    }
    // 视觉桥决策:子任务需要视觉且派给非多模态模型 → 打桥(决策在此作出,实际桥调用延后)。
    if !quiet {
        print_vision_bridge_plan(&plan, &profiles);
    }
    if plan_only {
        // 计划模式收尾:发 Done 让 TUI phase 复位(不执行,只拆解预览)。
        if let Some(emit) = &emit {
            let mut evs = Vec::new();
            plan_to_dispatch_events(&plan, &mut evs);
            emit(AgentEvent::Done {
                summary: format!("已拆解计划,{} 个子任务(plan only)", evs.len()),
                usage: None,
                session_id: String::new(),
                reasoning: None,
            });
        }
        return Ok(());
    }
    // 4) 执行:扁平化 ExecPlan 的 Execute 叶子 → execute_subtasks_batched(按 depends_on 分批)。
    let mut leaf_ids = std::collections::HashSet::new();
    collect_leaf_ids(&plan, &mut leaf_ids);
    let mut jobs = Vec::new();
    collect_executable(&plan, &mut jobs, &leaf_ids, config, &registry, &skill_dir, subagent_approval)?;
    if cancel.is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed)) {
        if let Some(emit) = &emit {
            emit(AgentEvent::Error { message: "cancelled by user".into() });
        }
        return Ok(());
    }
    if let Some(emit) = &emit {
        emit(AgentEvent::PhaseChanged {
            phase: "执行".into(),
            cycle: 0,
            session_id: String::new(),
        });
    }
    // execute_subtasks_batched 按依赖分批执行,结果回灌 OrchestratorResult。
    let results = execute_subtasks_batched(
        jobs,
        &store,
        2,
        emit.clone(),
        steer_hub,
        cancel.cloned(),
    )
    .await;
    // 4.5) 终局:取消发 cancelled(已中断),否则发 Done 复位 TUI phase。
    //     OrchestratorResult 已由 execute_subtasks_batched 逐子任务回灌。
    if let Some(emit) = &emit {
        // 取消:发 cancelled(已中断),不报"完成"。
        if cancel.is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed)) {
            emit(AgentEvent::Error { message: "cancelled by user".into() });
            return Ok(());
        }
        // 终局:发 Done 复位 TUI phase(否则 /autonomous 结束后界面一直 Running)。
        let ok = results.iter().filter(|r| r.ok).count();
        emit(AgentEvent::Done {
            summary: format!("{ok}/{} 子任务完成,自动编排结束", results.len()),
            usage: None,
            session_id: String::new(),
            reasoning: None,
        });
    }
    // 5) 风险治理:抽查(始终自动)+ 系统自动棘轮升级(链路 B 可演示)。
    //    用户降级/风险模式切换属交互层,延后实现(见函数头注释)。
    let mut risk = RiskState::new(risk_mode);
    let verdicts = rc_router::risk::spot_check::inspect(&results);
    for (r, v) in results.iter().zip(&verdicts) {
        // 校准写侧:撕裂观测 → cost_stats(severity>0 → 1.0)。
        let _ = store.record_stat(
            &format!("{}|usage", r.model),
            "tear_p",
            if v.severity > 0 { 1.0 } else { 0.0 },
        );
        if v.severity > 0 {
            if !quiet {
                eprintln!("spot-check: {} issue {}", v.subtask_id, v.issue);
            }
            let current = find_subtask_risk(&plan, &v.subtask_id);
            if let Some(higher) = higher_risk(current) {
                if let Some(ev) = risk.maybe_escalate(
                    &v.subtask_id,
                    current,
                    higher,
                    EscalationTrigger::System("spot-check"),
                    &cost,
                    &store,
                    50.0,
                ) {
                    if !quiet {
                        println!(
                            "[escalate] {} {} -> {} (system:spot-check: {})",
                            ev.subtask_id,
                            risk_name(ev.from),
                            risk_name(ev.to),
                            ev.reason
                        );
                    }
                }
            }
        }
    }
    // 6) 校准写侧:真实 usage → cost_stats(此前 record_stat 仅测试调用,这是真实生产者)。
    for r in &results {
        if let Some(u) = &r.usage {
            let tokens = u
                .get("total")
                .and_then(Value::as_u64)
                .or_else(|| u.get("total_tokens").and_then(Value::as_u64));
            if let Some(t) = tokens {
                let _ = store.record_stat(&format!("{}|usage", r.model), "tokens", t as f64);
            }
        }
    }
    if !quiet {
        for r in &results {
            println!(
                "== {} {} {}",
                r.subtask_id,
                r.model,
                if r.ok { "ok" } else { "FAILED" }
            );
        }
        // 棘轮升级日志(链路 B 的端到端证据)。
        if !risk.log.is_empty() {
            println!("[escalation_log]");
            for ev in &risk.log {
                println!(
                    "  {}: {} -> {} ({})",
                    ev.subtask_id,
                    risk_name(ev.from),
                    risk_name(ev.to),
                    ev.trigger
                );
            }
        }
    }
    Ok(())
}

/// 解析 `--pin` 逗号分隔的 `id:model` 对 → HashMap。
fn parse_pins(pin: Option<&str>) -> HashMap<String, String> {
    let mut out = HashMap::new();
    if let Some(spec) = pin {
        for pair in spec.split(',') {
            if let Some((id, model)) = pair.trim().split_once(':') {
                let id = id.trim();
                let model = model.trim();
                if !id.is_empty() && !model.is_empty() {
                    out.insert(id.to_string(), model.to_string());
                }
            }
        }
    }
    out
}

/// 把 `--pin` 应用到计划树:收集叶子派发表 → `rc_router::intent::apply_pins` → 回写。
fn apply_pins_to_plan(plan: &mut ExecPlan, pins: &HashMap<String, String>) {
    if pins.is_empty() {
        return;
    }
    let mut leaves: Vec<DispatchEntry> = Vec::new();
    collect_leaf_entries(plan, &mut leaves);
    let pinned = rc_router::intent::apply_pins(leaves, pins);
    let by_id: HashMap<String, DispatchEntry> =
        pinned.into_iter().map(|e| (e.subtask_id.clone(), e)).collect();
    overwrite_leaf_entries(plan, &by_id);
}

fn collect_leaf_entries(plan: &ExecPlan, out: &mut Vec<DispatchEntry>) {
    match &plan.action {
        ExecAction::Execute { entry } => out.push(entry.clone()),
        ExecAction::Decompose { children } => {
            for c in children {
                collect_leaf_entries(c, out);
            }
        }
    }
}

fn overwrite_leaf_entries(plan: &mut ExecPlan, by_id: &HashMap<String, DispatchEntry>) {
    match &mut plan.action {
        ExecAction::Execute { entry } => {
            if let Some(pinned) = by_id.get(&entry.subtask_id) {
                *entry = pinned.clone();
            }
        }
        ExecAction::Decompose { children } => {
            for c in children {
                overwrite_leaf_entries(c, by_id);
            }
        }
    }
}

/// 视觉桥决策打印:子任务需要视觉且派给非多模态模型 → 决策打桥(实际调用延后到交互计划)。
fn print_vision_bridge_plan(plan: &ExecPlan, profiles: &[CapabilityProfile]) {
    match &plan.action {
        ExecAction::Execute { entry } => {
            if needs_vision(&plan.subtask) {
                let bridged = profiles
                    .iter()
                    .find(|p| p.model == entry.model)
                    .map(should_bridge)
                    .unwrap_or(true);
                if bridged {
                    println!("[vision-bridge] {} -> vision_profile", plan.subtask.id);
                }
            }
        }
        ExecAction::Decompose { children } => {
            for c in children {
                print_vision_bridge_plan(c, profiles);
            }
        }
    }
}

fn risk_name(r: Risk) -> &'static str {
    match r {
        Risk::Low => "low",
        Risk::Med => "med",
        Risk::High => "high",
    }
}

fn higher_risk(r: Risk) -> Option<Risk> {
    match r {
        Risk::Low => Some(Risk::Med),
        Risk::Med => Some(Risk::High),
        Risk::High => None,
    }
}

/// 在计划树中找子任务的原始 risk 等级。
fn find_subtask_risk(plan: &ExecPlan, id: &str) -> Risk {
    if plan.subtask.id == id {
        return plan.subtask.risk;
    }
    if let ExecAction::Decompose { children } = &plan.action {
        for c in children {
            if let Some(r) = find_subtask_risk_in(c, id) {
                return r;
            }
        }
    }
    Risk::Low
}

fn find_subtask_risk_in(plan: &ExecPlan, id: &str) -> Option<Risk> {
    if plan.subtask.id == id {
        return Some(plan.subtask.risk);
    }
    if let ExecAction::Decompose { children } = &plan.action {
        for c in children {
            if let Some(r) = find_subtask_risk_in(c, id) {
                return Some(r);
            }
        }
    }
    None
}

fn print_plan(plan: &rc_router::recursion::ExecPlan, depth: usize) {
    let pad = "  ".repeat(depth);
    match &plan.action {
        rc_router::recursion::ExecAction::Execute { entry } => println!(
            "{pad}{} -> {} (cap {:.1}, eff {:.2}, score {:.1}, {})",
            plan.subtask.id, entry.model, entry.capability, entry.efficiency, entry.score, plan.basis
        ),
        rc_router::recursion::ExecAction::Decompose { children } => {
            println!("{pad}{} (decompose)", plan.subtask.id);
            for c in children {
                print_plan(c, depth + 1);
            }
        }
    }
}

/// 从 ExecPlan 收集每个 Execute 叶子的派发事件,携带自动选中的模型 —— 让 TUI
/// 的"子代理自动选模型"可见(每个子任务派给哪个模型一目了然)。
fn plan_to_dispatch_events(plan: &rc_router::recursion::ExecPlan, out: &mut Vec<AgentEvent>) {
    match &plan.action {
        rc_router::recursion::ExecAction::Execute { entry } => out.push(
            AgentEvent::OrchestratorDispatch {
                parent_id: "root".into(),
                child_id: plan.subtask.id.clone(),
                prompt: plan.subtask.description.clone(),
                model: entry.model.clone(),
            },
        ),
        rc_router::recursion::ExecAction::Decompose { children } => {
            for c in children {
                plan_to_dispatch_events(c, out);
            }
        }
    }
}

/// 收集 ExecPlan 里所有 Execute 叶子的 subtask id(用于 depends_on 过滤:只保留
/// 也确实是叶子任务的依赖,否则 batched 会因依赖永不满足而卡死)。
fn collect_leaf_ids(plan: &rc_router::recursion::ExecPlan, out: &mut std::collections::HashSet<String>) {
    match &plan.action {
        rc_router::recursion::ExecAction::Execute { .. } => {
            out.insert(plan.subtask.id.clone());
        }
        rc_router::recursion::ExecAction::Decompose { children } => {
            for c in children {
                collect_leaf_ids(c, out);
            }
        }
    }
}

/// 把 ExecPlan 树的 Execute 叶子扁平化成 `(subtask_id, prompt, depends_on, AgentConfig)`,
/// depends_on = 该子任务的依赖过滤到确实在本批叶子里的 id。AgentConfig 复用旧
/// route_command 的构造(provider 回退链:精确 .model 匹配 → 活跃 profile → 第一个 profile)。
fn collect_executable(
    plan: &rc_router::recursion::ExecPlan,
    jobs: &mut Vec<(String, String, Vec<String>, AgentConfig)>,
    leaf_ids: &std::collections::HashSet<String>,
    config: &FileConfig,
    registry: &Registry,
    skill_dir: &Path,
    subagent_approval: Option<Arc<dyn rc_sandbox::ApprovalHook>>,
) -> Result<()> {
    match &plan.action {
        rc_router::recursion::ExecAction::Execute { entry } => {
            let profile = registry
                .profiles
                .iter()
                .find(|p| p.model == entry.model)
                .or_else(|| registry.active())
                .or_else(|| registry.profiles.first())
                .ok_or_else(|| {
                    anyhow!("no provider profiles configured; run `raincode model add`")
                })?;
            let provider = provider_for_profile(registry, Some(&profile.id))?;
            let skill_store = SkillStore::new(skill_dir);
            let workspace = config
                .core
                .workspace
                .as_deref()
                .map(expand_tilde)
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
            let store = Store::open(state_path())?;
            // 子代理上下文窗口复用 context_for_model(DB 能力画像),而非硬编码 0(0 → 128k 兜底)。
            let context_window = context_for_model(&store, registry);
            // 监督守卫:route 子代理也带守卫(不可绕过);无 hook → 高危操作保守拦截。
            let guard_cfg = match rc_sandbox::load_supervise_config(&raincode_home()) {
                Ok(cfg) => Some(cfg),
                Err(e) => {
                    tracing::warn!("supervise.toml 解析失败,守卫关闭: {e}");
                    None
                }
            };
            let guard_memo = guard_cfg
                .as_ref()
                .map(|_| std::sync::Arc::new(rc_sandbox::guard_hook::SessionGuardMemo::default()));
            let guard_home = guard_cfg.as_ref().map(|_| raincode_home());
            // 依赖排序:只保留确实是本批叶子任务的依赖(自我依赖剔除)。
            let depends_on: Vec<String> = plan
                .subtask
                .depends_on
                .iter()
                .filter(|d| leaf_ids.contains(*d) && *d != &plan.subtask.id)
                .cloned()
                .collect();
            jobs.push((
                plan.subtask.id.clone(),
                plan.subtask.description.clone(),
                depends_on,
                AgentConfig {
                    provider,
                    plan_provider: None,
                    review_provider: None,
                    store,
                    skill_store: skill_store.clone(),
                    // 子代理也要能联网查资料(web_fetch/web_search)。
                    tools: {
                        let mut t = default_tools(skill_store.clone());
                        t.extend(network_tools(SearchConfig::default()));
                        t
                    },
                    // 子代理授权钩子:TUI 传 risk_approval_hook(跟随共享风险档,
                    // Auto 放行/Manual 拒绝/Ask 弹审批);CLI 传 None → 按 config。
                    approval: subagent_approval.clone().unwrap_or_else(|| {
                        approval_hook(config.core.approval_mode.as_deref().unwrap_or("ask"))
                    }),
                    command_policy: CommandPolicy {
                        allow: config.sandbox.commands.allow.clone(),
                        deny: config.sandbox.commands.deny.clone(),
                    },
                    network_policy: network_policy(config),
                    cwd: workspace.clone(),
                    state_path: state_path(),
                    // 子代理按上下文限制而非步数上限:给足轮次(至少 48),让它能
                    // 先探索再实现,完成任务自然停(模型不再调工具即结束)。用户
                    // 配置的 max_turns 只作为下限,不再成为复杂子任务的硬卡点。
                    max_turns: config.core.max_turns.unwrap_or(24).max(48),
                    max_steps: 0,
                    evolve_on_finish: config.evolve.enabled.unwrap_or(true),
                    plan_mode: false,
                    hooks: config.hooks.clone(),
                    agent: Some(
                        config
                            .core
                            .agent
                            .clone()
                            .unwrap_or_else(|| DEFAULT_AGENT.to_string()),
                    ),
                    max_history_bytes: config.core.max_history_bytes,
                    mcp_servers: vec![],
                    entropy_mode: false,
                    plan_max_rounds: config.entropy.plan_max_rounds.unwrap_or(6),
                    plan_max_questions: config.entropy.plan_max_questions.unwrap_or(5),
                    review_max_rounds: config.entropy.review_max_rounds.unwrap_or(3),
                    max_cycles: config.entropy.max_cycles.unwrap_or(3),
                    user_input: user_input_hook(false),
                    steer_rx: None,
                    context_window,
                    subagent: None,
                    guard_cfg,
                    guard_hook: None,
                    guard_memo,
                    guard_home,
                },
            ));
            Ok(())
        }
        rc_router::recursion::ExecAction::Decompose { children } => {
            for c in children {
                collect_executable(c, jobs, leaf_ids, config, registry, skill_dir, subagent_approval.clone())?;
            }
            Ok(())
        }
    }
}

async fn mcp_command(config: &FileConfig, cmd: McpCmd) -> Result<()> {
    match cmd {
        McpCmd::List { check } => {
            if check {
                match McpManager::connect_all(&config.mcp.servers).await {
                    Ok(manager) => {
                        // 全部失败时也要非零退出:先捕获工具是否为空(下面循环会 move tools)。
                        let all_failed = manager.tools.is_empty() && !manager.failed.is_empty();
                        let mut by_server: BTreeMap<String, Vec<String>> = BTreeMap::new();
                        for tool in manager.tools {
                            let spec = tool.spec();
                            if let Some(rest) = spec.name.strip_prefix("mcp__") {
                                if let Some((server, tool_name)) = rest.split_once('_') {
                                    by_server
                                        .entry(server.to_string())
                                        .or_default()
                                        .push(tool_name.to_string());
                                }
                            }
                        }
                        for (name, tools) in by_server {
                            println!("{name} | connected | {}", tools.join(", "));
                        }
                        // 表面化连接失败的服务器,而不是静默退出 0。
                        for name in &manager.failed {
                            println!("{name} | failed");
                        }
                        if all_failed {
                            // 一个服务器都没连上 ⇒ `--check` 应视作失败。
                            std::process::exit(1);
                        }
                    }
                    Err(e) => println!("mcp check failed: {e}"),
                }
            } else {
                for (name, server) in &config.mcp.servers {
                    println!(
                        "{} | {} | {}",
                        name,
                        server.kind,
                        server
                            .url
                            .clone()
                            .unwrap_or_else(|| server.command.clone().unwrap_or_default())
                    );
                }
            }
        }
    }
    Ok(())
}

async fn serve_stdio(config: &FileConfig) -> Result<()> {
    // 串行 serve 循环:阻塞 read_line → await handle_rpc 到完成。因此一条运行中的
    // route(如 RequestMethod::Route)期间,steer/chat 请求不会被处理,直到 route 结束。
    // 【DEFERRED】真正的并发 serve(路由任务跑在独立 task + 常驻 stdin 读取线程)是
    // 架构级改造,本次评审显式延期实现。当前缓解:前端 LiveCore.request 带 5s 超时,
    // steer 结果携带 note 如实说明「仅记录、agent 当前未读取」,UI 不再假装生效。
    // 另一个延期项:rc-core agent 在检查点读取 steering/ 文档(plan2 steering 通道)。
    let stdin = io::stdin();
    let mut line = String::new();
    loop {
        line.clear();
        {
            let mut lock = stdin.lock();
            let read = lock.read_line(&mut line)?;
            if read == 0 {
                break;
            }
        }
        if line.trim().is_empty() {
            continue;
        }
        let request: Request = match serde_json::from_str(line.trim()) {
            Ok(request) => request,
            Err(_) => {
                println!(
                    "{}",
                    encode_line(&Response::err(json!("parse"), RpcError::parse_error()))?
                );
                continue;
            }
        };
        let response = handle_rpc(config, request).await?;
        if let Some(response) = response {
            println!("{}", encode_line(&response)?);
        }
        io::stdout().flush()?;
    }
    Ok(())
}

async fn stream_rpc_run(
    config: &FileConfig,
    id: Value,
    prompt: &str,
    resume: bool,
    plan: bool,
    entropy: bool,
    agent: Option<&str>,
) -> Result<()> {
    let registry = load_registry()?;
    let skill_dir = skills_dir(config);
    ensure_seed(&skill_dir)?;
    let store = Store::open(state_path())?;
    sync_skill_index(&store, &SkillStore::new(&skill_dir))?;
    let workspace = config
        .core
        .workspace
        .as_deref()
        .map(expand_tilde)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let session = if resume {
        store
            .list_sessions(1)?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("no session to resume"))?
    } else {
        store.create_session(&workspace.to_string_lossy())?
    };
    let skill_store = SkillStore::new(&skill_dir);
    let agent = Agent::new(
        agent_config(
            config,
            &registry,
            store,
            skill_store,
            plan,
            entropy,
            false,
            agent,
            true, // 桌面聊天上下文:装配 run_slash_command,模型按用户指示执行 /命令
        )
        .await?,
    );
    let response = Response::ok(id, json!({"session_id": session.id.clone()}));
    println!("{}", encode_line(&response)?);
    let mut stream = agent.run(session.id.clone(), prompt.to_string());
    while let Some(event) = stream.next().await {
        println!(
            "{}",
            encode_line(&json!({"jsonrpc": "2.0", "method": "event", "params": event}))?
        );
    }
    Ok(())
}

async fn handle_rpc(config: &FileConfig, request: Request) -> Result<Option<Response>> {
    let id = request.id.clone();
    match request.method {
        RequestMethod::Start => {
            let prompt = request
                .params
                .get("prompt")
                .and_then(Value::as_str)
                .unwrap_or("");
            let resume = request
                .params
                .get("resume")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let plan = request
                .params
                .get("plan")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let entropy = request
                .params
                .get("entropy")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let agent = request
                .params
                .get("agent")
                .and_then(Value::as_str)
                .map(str::to_string);
            stream_rpc_run(config, id, prompt, resume, plan, entropy, agent.as_deref()).await?;
            Ok(None)
        }
        RequestMethod::Resume => {
            let prompt = request
                .params
                .get("prompt")
                .and_then(Value::as_str)
                .unwrap_or("");
            let plan = request
                .params
                .get("plan")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let entropy = request
                .params
                .get("entropy")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let agent = request
                .params
                .get("agent")
                .and_then(Value::as_str)
                .map(str::to_string);
            stream_rpc_run(config, id, prompt, true, plan, entropy, agent.as_deref()).await?;
            Ok(None)
        }
        RequestMethod::Respond | RequestMethod::SetModel => {
            Ok(Some(Response::ok(id, json!({"ok": true}))))
        }
        RequestMethod::SkillList => {
            let skills = SkillStore::new(skills_dir(config)).discover();
            let names: Vec<String> = skills.into_iter().map(|s| s.name).collect();
            Ok(Some(Response::ok(id, json!({"skills": names}))))
        }
        RequestMethod::SkillLoad => {
            let name = request
                .params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("");
            match SkillStore::new(skills_dir(config)).load(name) {
                Some(skill) => match skill.render() {
                    Ok(text) => Ok(Some(Response::ok(id, Value::String(text)))),
                    Err(e) => Ok(Some(Response::err(
                        id,
                        RpcError::new(-32003, e.to_string()),
                    ))),
                },
                None => Ok(Some(Response::err(
                    id,
                    RpcError::new(-32002, format!("skill {name} not found")),
                ))),
            }
        }
        RequestMethod::SkillCreate => {
            let name = request
                .params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let category = request
                .params
                .get("category")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let description = request
                .params
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let body = request
                .params
                .get("body")
                .and_then(Value::as_str)
                .map(str::to_string);
            if name.is_empty() || category.is_empty() {
                return Ok(Some(Response::err(
                    id,
                    RpcError::new(-32002, "name and category are required"),
                )));
            }
            let skills = SkillStore::new(skills_dir(config));
            if skills.load(&name).is_some() {
                return Ok(Some(Response::err(
                    id,
                    RpcError::new(-32003, format!("skill {name} already exists")),
                )));
            }
            let skill = Skill {
                name: name.clone(),
                description,
                short_description: None,
                category,
                path: PathBuf::new(),
                body: body
                    .unwrap_or_else(|| format!("# {name}\n\nWrite the reusable method here.\n")),
                relations: vec![],
                triggers: vec![],
                tags: vec![],
                version: 1,
                confidence: 0.5,
                usage_count: 0,
                success_rate: 0.0,
                last_used: None,
                auto: false,
                origin: "manual".into(),
                origin_url: None,
                scope: "user".into(),
                allow_implicit: true,
                embedding: None,
            };
            let path = skills.save(&skill).map_err(|e| anyhow!(e))?;
            sync_skill_index(&Store::open(state_path())?, &skills)?;
            Ok(Some(Response::ok(id, json!({"path": path}))))
        }
        RequestMethod::SkillEdit => {
            let name = request
                .params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let skills = SkillStore::new(skills_dir(config));
            let mut skill = match skills.load(&name) {
                Some(skill) => skill,
                None => {
                    return Ok(Some(Response::err(
                        id,
                        RpcError::new(-32002, format!("skill {name} not found")),
                    )))
                }
            };
            if let Some(description) = request.params.get("description").and_then(Value::as_str) {
                skill.description = description.to_string();
            }
            if let Some(category) = request.params.get("category").and_then(Value::as_str) {
                skill.category = category.to_string();
            }
            if let Some(body) = request.params.get("body").and_then(Value::as_str) {
                skill.body = body.to_string();
                skill.version += 1;
            }
            let path = skills.save(&skill).map_err(|e| anyhow!(e))?;
            sync_skill_index(&Store::open(state_path())?, &skills)?;
            Ok(Some(Response::ok(id, json!({"path": path}))))
        }
        RequestMethod::SkillInstall => {
            let spec = request
                .params
                .get("spec")
                .and_then(Value::as_str)
                .unwrap_or("");
            let dest = skills_dir(config);
            let source: Box<dyn SkillSource> =
                if spec.starts_with("http") || spec.contains('/') && !Path::new(spec).exists() {
                    Box::new(RemoteSource::new())
                } else {
                    Box::new(LocalSource::new(PathBuf::from(spec)))
                };
            let report = source.install(spec, &dest).await?;
            let store = Store::open(state_path())?;
            sync_skill_index(&store, &SkillStore::new(&dest))?;
            Ok(Some(Response::ok(id, serde_json::to_value(report)?)))
        }
        RequestMethod::SkillUpdate => {
            let spec = request
                .params
                .get("spec")
                .and_then(Value::as_str)
                .unwrap_or("");
            let dest = skills_dir(config);
            let source: Box<dyn SkillSource> =
                if spec.starts_with("http") || spec.contains('/') && !Path::new(spec).exists() {
                    Box::new(RemoteSource::new())
                } else {
                    Box::new(LocalSource::new(PathBuf::from(spec)))
                };
            let report = source.install(spec, &dest).await?;
            sync_skill_index(&Store::open(state_path())?, &SkillStore::new(&dest))?;
            Ok(Some(Response::ok(id, serde_json::to_value(report)?)))
        }
        RequestMethod::SkillUninstall => {
            let name = request
                .params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let skills = SkillStore::new(skills_dir(config));
            skills.remove(&name).map_err(|e| anyhow!(e))?;
            Store::open(state_path())?.delete_skill(&name)?;
            Ok(Some(Response::ok(id, json!({"ok": true, "name": name}))))
        }
        RequestMethod::SkillSearch => {
            let query = request
                .params
                .get("query")
                .and_then(Value::as_str)
                .unwrap_or("");
            let hits = RemoteSource::new().search(query).await;
            Ok(Some(Response::ok(id, json!(hits))))
        }
        RequestMethod::Evolve => {
            let session = request
                .params
                .get("session_id")
                .and_then(Value::as_str)
                .map(str::to_string);
            let report = evolve_session(config, session.as_deref()).await?;
            Ok(Some(Response::ok(id, serde_json::to_value(report)?)))
        }
        RequestMethod::InsightsScan => {
            let registry = load_registry()?;
            let store = Store::open(state_path())?;
            let provider = make_provider(&registry)?;
            let daemon = PatternDaemon::new(
                provider,
                store,
                SkillStore::new(skills_dir(config)),
                DaemonConfig::default(),
            );
            let report = daemon.scan().await?;
            Ok(Some(Response::ok(id, serde_json::to_value(report)?)))
        }
        RequestMethod::ModelList => {
            let registry = load_registry()?;
            Ok(Some(Response::ok(
                id,
                serde_json::to_value(registry.profiles)?,
            )))
        }
        RequestMethod::ModelUse => {
            let profile_id = request
                .params
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("");
            let mut registry = load_registry()?;
            registry.set_active(profile_id)?;
            save_registry(&registry)?;
            Ok(Some(Response::ok(
                id,
                json!({"ok": true, "profile": profile_id}),
            )))
        }
        RequestMethod::McpList => Ok(Some(Response::ok(
            id,
            json!(config.mcp.servers.keys().collect::<Vec<_>>()),
        ))),
        // --- 桌面交互 RPC(Task 10)---
        RequestMethod::Steer => {
            let agent_id = request
                .params
                .get("agent_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let text = request
                .params
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if agent_id.is_empty() {
                return Ok(Some(Response::err(
                    id,
                    RpcError::new(-32002, "agent_id is required"),
                )));
            }
            if text.is_empty() {
                return Ok(Some(Response::err(
                    id,
                    RpcError::new(-32002, "text is required"),
                )));
            }
            match record_steering(&agent_id, &text) {
                Ok(path) => Ok(Some(Response::ok(
                    id,
                    json!({
                        "ok": true,
                        "agent_id": agent_id,
                        "recorded_to": path.to_string_lossy(),
                        "note": "steering recorded for next checkpoint (minimal backend; running agents do not read it yet)"
                    }),
                ))),
                Err(e) => Ok(Some(Response::err(id, RpcError::new(-32003, e.to_string())))),
            }
        }
        RequestMethod::Chat => {
            let text = request.params.get("text").and_then(Value::as_str).unwrap_or("");
            match dispatch_chat(text) {
                ChatAction::Slash(name, args) => match dispatch_slash(&name, &args) {
                    Ok(output) => Ok(Some(Response::ok(
                        id,
                        json!({"ok": true, "command": name, "output": output}),
                    ))),
                    Err(e) => Ok(Some(Response::err(id, RpcError::new(-32002, e)))),
                },
                ChatAction::Ack => Ok(Some(Response::ok(
                    id,
                    json!({
                        "ok": true,
                        "note": "chat acknowledged; full chat loop is out of scope for this milestone"
                    }),
                ))),
            }
        }
        RequestMethod::Sessions => {
            let store = Store::open(state_path())?;
            let sessions = store.list_sessions(200)?;
            Ok(Some(Response::ok(id, serde_json::to_value(sessions)?)))
        }
        RequestMethod::SessionDelete => {
            let id_str = request
                .params
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if id_str.is_empty() {
                return Ok(Some(Response::err(
                    id,
                    RpcError::new(-32002, "id is required"),
                )));
            }
            let store = Store::open(state_path())?;
            store.delete_session(&id_str)?;
            Ok(Some(Response::ok(id, json!({"ok": true, "id": id_str}))))
        }
        RequestMethod::Compact => slash_rpc_ok(id, "compact", &request.params),
        RequestMethod::Clear => slash_rpc_ok(id, "clear", &request.params),
        RequestMethod::Risk => slash_rpc_ok(id, "risk", &request.params),
        RequestMethod::Status => slash_rpc_ok(id, "status", &request.params),
        RequestMethod::Route => {
            let prompt = request
                .params
                .get("prompt")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if prompt.is_empty() {
                // palette 的 /route 无参数入口:退化为 slash 占位输出(不执行)。
                let output = dispatch_slash("route", &request.params).unwrap_or_else(|e| e);
                return Ok(Some(Response::ok(
                    id,
                    json!({"ok": true, "command": "route", "output": output}),
                )));
            }
            // 真实 route:quiet 压制人类可读打印,监督事件经 emit 以 JSON event 行输出,
            // 桌面监督板据此渲染 agent 分组 + 上下文进度。
            let emit: Arc<dyn Fn(AgentEvent) + Send + Sync> = Arc::new(|event| {
                if let Ok(line) = encode_line(&json!({"jsonrpc": "2.0", "method": "event", "params": event}))
                {
                    print!("{line}");
                    let _ = io::stdout().flush();
                }
            });
            let pool = request
                .params
                .get("pool")
                .and_then(Value::as_str)
                .map(str::to_string);
            let pin = request
                .params
                .get("pin")
                .and_then(Value::as_str)
                .map(str::to_string);
            route_command(config, &prompt, false, pool, pin.as_deref(), true, Some(emit), None, None, None, rc_router::risk::RiskMode::Ask).await?;
            Ok(Some(Response::ok(id, json!({"ok": true, "command": "route"}))))
        }
        RequestMethod::Stop => Ok(None),
    }
}

/// 内置 slash 命令的统一 RPC 响应(compact/clear/risk/status)。
fn slash_rpc_ok(id: Value, name: &str, params: &Value) -> Result<Option<Response>> {
    match dispatch_slash(name, params) {
        Ok(output) => Ok(Some(Response::ok(
            id,
            json!({"ok": true, "command": name, "output": output}),
        ))),
        Err(e) => Ok(Some(Response::err(id, RpcError::new(-32002, e)))),
    }
}

async fn evolve_session(
    config: &FileConfig,
    session: Option<&str>,
) -> Result<rc_evolve::EvolveReport> {
    let registry = load_registry()?;
    let skill_dir = skills_dir(config);
    ensure_seed(&skill_dir)?;
    let store = Store::open(state_path())?;
    sync_skill_index(&store, &SkillStore::new(&skill_dir))?;
    let session_id = match session {
        Some(id) => id.to_string(),
        None => {
            store
                .list_sessions(1)?
                .into_iter()
                .next()
                .ok_or_else(|| anyhow!("no sessions"))?
                .id
        }
    };
    let provider = make_provider(&registry)?;
    let mut engine = EvolveEngine::new(
        provider,
        store,
        SkillStore::new(&skill_dir),
        EvolveConfig::default(),
    );
    engine.digest(&session_id).await.map_err(|e| anyhow!(e))
}

fn read_stdin_prompt() -> Result<String> {
    print!("> ");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    let prompt = line.trim().to_string();
    if prompt.is_empty() {
        Err(anyhow!("no prompt provided; use `raincode run \"...\"`"))
    } else {
        Ok(prompt)
    }
}

fn slugify(input: &str) -> String {
    let mut out = String::new();
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
        } else if ch.is_whitespace() {
            out.push('-');
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        "imported".to_string()
    } else {
        out.to_lowercase()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // OpenRouter /api/v1/models 返回格式的真实子集(fixture)。
    const OPENROUTER_FIXTURE: &str = r#"{
      "data": [
        {
          "id": "deepseek/deepseek-chat",
          "context_length": 128000,
          "pricing": {"prompt": "0.14", "completion": "0.28"}
        },
        {
          "id": "qwen/qwen2.5-coder-32b-instruct",
          "context_length": 32768,
          "pricing": {"prompt": "0.2", "completion": "0.6"}
        },
        {
          "id": "meta-llama/llama-3.3-70b-instruct",
          "context_length": 131072
        }
      ]
    }"#;

    #[test]
    fn parse_openrouter_models_extracts_pricing_and_context() {
        let rows = parse_openrouter_models(OPENROUTER_FIXTURE).unwrap();
        assert_eq!(rows.len(), 3);
        let first = &rows[0];
        assert_eq!(first.model, "deepseek/deepseek-chat");
        assert_eq!(first.context_window, 128_000);
        assert!((first.input_cost_per_m - 0.14).abs() < 1e-9);
        assert!((first.output_cost_per_m - 0.28).abs() < 1e-9);
        assert_eq!(first.source, "openrouter");
        assert_eq!(first.updated_at, "now");
        assert!(!first.multimodal);
        // 第二个:无榜单数据 → 能力分诚实为 0(不再是 seed 中位 70)。
        assert_eq!(rows[1].model, "qwen/qwen2.5-coder-32b-instruct");
        assert!((rows[1].reasoning - 0.0).abs() < 1e-9);
        assert_eq!(rows[1].source, "openrouter");
        // 第三个:pricing 缺省 → 0.0 → max(0.0001) 兜底
        let third = &rows[2];
        assert_eq!(third.context_window, 131_072);
        assert!((third.input_cost_per_m - 0.0001).abs() < 1e-9);
        assert!((third.output_cost_per_m - 0.0001).abs() < 1e-9);
    }

    #[test]
    fn parse_openrouter_models_extracts_real_arena_scores() {
        // 带 benchmarks 的模型:artificial_analysis(0-100)与 design_arena(Elo)映射。
        let raw = r#"{
          "data": [
            {
              "id": "deepseek/deepseek-v4-flash-0731",
              "context_length": 1048576,
              "pricing": {"prompt": "0.1", "completion": "0.3"},
              "benchmarks": {
                "artificial_analysis": {"intelligence_index": 51.8, "coding_index": 69.1, "agentic_index": 48.4},
                "design_arena": [
                  {"category": "website", "elo": 1230.0},
                  {"category": "codecategories", "elo": 1233.0}
                ]
              }
            }
          ]
        }"#;
        let rows = parse_openrouter_models(raw).unwrap();
        let p = &rows[0];
        // Artificial Analysis 指数直接用。
        assert!((p.reasoning - 51.8).abs() < 1e-9);
        assert!((p.coding - 69.1).abs() < 1e-9);
        assert!((p.math - 51.8).abs() < 1e-9);
        // design_arena Elo 归一化:website=1230 → (1230-1000)/400*100 = 57.5;codecategories=1233 → 58.25。
        assert!((p.frontend - 57.5).abs() < 1e-9);
        assert!((p.backend - 58.25).abs() < 1e-9);
        // long_context:1M ctx → 100。
        assert!((p.long_context - 100.0).abs() < 1e-9);
        assert_eq!(p.source, "openrouter-arena");
        assert_eq!(p.context_window, 1_048_576);
    }

    #[test]
    fn resolve_profile_matches_suffix_and_prefers_complete_benchmarks() {
        // 三个 OpenRouter 候选:latest 无榜单、未版本化只有 design_arena、-0731 数据最全。
        let mk = |model: &str, reasoning: f64, coding: f64, frontend: f64, backend: f64| CapabilityProfile {
            model: model.into(), reasoning, coding, frontend, backend, math: reasoning,
            long_context: 100.0, input_cost_per_m: 0.1, output_cost_per_m: 0.3,
            context_window: 1_000_000, provenance: "openrouter".into(), multimodal: false,
        };
        let profiles = vec![
            mk("~deepseek/deepseek-v4-flash-latest", 0.0, 0.0, 0.0, 0.0),
            mk("deepseek/deepseek-v4-flash", 0.0, 0.0, 57.5, 58.25),
            mk("deepseek/deepseek-v4-flash-0731", 51.8, 69.1, 57.5, 58.25),
        ];
        // registry 裸名 "deepseek-v4-flash" → suffix 匹配多个,取榜单最全的 -0731。
        let resolved = resolve_capability_profile(&profiles, "deepseek-v4-flash").unwrap();
        assert!((resolved.coding - 69.1).abs() < 1e-9, "must prefer the -0731 complete profile");
        assert!((resolved.reasoning - 51.8).abs() < 1e-9);
        // 无匹配 → None。
        assert!(resolve_capability_profile(&profiles, "kimi-k3").is_none());
    }

    #[test]
    fn parse_openrouter_models_rejects_malformed_json() {
        assert!(parse_openrouter_models("not json").is_err());
    }

    #[test]
    fn slash_command_dispatch_routes_compact() {
        let r = dispatch_slash("compact", &serde_json::json!({})).unwrap();
        // 诚实占位:绝不伪造压缩数字(禁止嘴硬)。
        assert!(r.contains("占位"));
        assert!(!r.contains("12.4k"));
        assert!(dispatch_slash("nope", &serde_json::json!({})).is_err());
    }

    #[test]
    fn slash_command_dispatch_is_honest_about_unwired_commands() {
        // 未接线的命令返回 Err,而不是假装成功/伪造结果。
        let model = dispatch_slash("model", &serde_json::json!({"name": "deepseek"}));
        assert!(model.is_err(), "model switch not wired: must error, not fake");
        let resume = dispatch_slash("resume", &serde_json::json!({"id": "abc"}));
        assert!(resume.is_err(), "resume not wired in-session: must error, not fake");
        // 占位命令明确声明未实现,不出现假数字。
        for cmd in ["compact", "clear", "route", "risk", "status"] {
            let r = dispatch_slash(cmd, &serde_json::json!({}));
            let output = r.unwrap_or_else(|e| e);
            assert!(output.contains("占位"), "{cmd}: {output}");
        }
    }

    #[tokio::test]
    async fn slash_command_tool_runs_named_command() {
        let tool = slash_command_tool(slash_command_spec());
        assert_eq!(tool.spec().name, "run_slash_command");
        let ctx = rc_tool::ToolContext::new(
            std::env::current_dir().unwrap(),
            std::sync::Arc::new(rc_sandbox::DenyHook),
        );
        let ok = tool
            .run(
                serde_json::json!({"name": "compact", "args": {}}),
                &ctx,
            )
            .await;
        assert!(ok.ok, "{}", ok.output);
        assert!(ok.output.contains("占位"));
        let err = tool
            .run(
                serde_json::json!({"name": "bogus", "args": {}}),
                &ctx,
            )
            .await;
        assert!(!err.ok);
        assert!(err.output.contains("unknown command"));
        let missing = tool.run(serde_json::json!({}), &ctx).await;
        assert!(!missing.ok);
        assert!(missing.output.contains("missing 'name'"));
    }

    #[test]
    fn dispatch_chat_routes_explicit_slash() {
        match dispatch_chat("/compact") {
            ChatAction::Slash(name, _) => assert_eq!(name, "compact"),
            _ => panic!("expected slash compact"),
        }
        match dispatch_chat("/model deepseek") {
            ChatAction::Slash(name, args) => {
                assert_eq!(name, "model");
                assert_eq!(args["name"], "deepseek");
            }
            _ => panic!("expected slash model"),
        }
    }

    #[test]
    fn dispatch_chat_routes_natural_language() {
        match dispatch_chat("压缩上下文") {
            ChatAction::Slash(name, _) => assert_eq!(name, "compact"),
            _ => panic!("expected compact"),
        }
        match dispatch_chat("帮我路由拆分这个任务") {
            ChatAction::Slash(name, _) => assert_eq!(name, "route"),
            _ => panic!("expected route"),
        }
        match dispatch_chat("查看 token 使用") {
            ChatAction::Slash(name, _) => assert_eq!(name, "status"),
            _ => panic!("expected status"),
        }
    }

    #[test]
    fn dispatch_chat_acks_free_text() {
        match dispatch_chat("你好,介绍一下你自己") {
            ChatAction::Ack => {}
            _ => panic!("expected ack"),
        }
        match dispatch_chat("") {
            ChatAction::Ack => {}
            _ => panic!("expected ack for empty"),
        }
    }

    #[test]
    fn record_steering_writes_jsonl() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let home = std::env::temp_dir().join(format!("rc-cli-steer-{stamp}"));
        std::env::set_var("RAINCODE_HOME", &home);
        let path = record_steering("s1", "小心改这个文件").unwrap();
        assert!(path.ends_with("steering/s1.jsonl"));
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("\"agent_id\":\"s1\""));
        assert!(content.contains("小心改这个文件"));
        // 再次写入是追加,不是覆盖。
        record_steering("s1", "再来一条").unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content.lines().count(), 2);
        std::env::remove_var("RAINCODE_HOME");
        let _ = std::fs::remove_dir_all(&home);
    }

    fn exec_subtask(id: &str, desc: &str, model: &str) -> ExecPlan {
        ExecPlan {
            subtask: rc_router::capability::Subtask {
                id: id.into(),
                description: desc.into(),
                requirements: rc_router::capability::Requirements::default(),
                cost_pressure: rc_router::capability::CostPressure::Med,
                depends_on: vec![],
                risk: rc_router::capability::Risk::Low,
            },
            depth: 1,
            action: ExecAction::Execute {
                entry: rc_router::capability::DispatchEntry {
                    subtask_id: id.into(),
                    model: model.into(),
                    capability: 0.9,
                    efficiency: 0.5,
                    score: 0.45,
                    escalated: false,
                },
            },
            basis: "test".into(),
        }
    }

    #[test]
    fn plan_to_dispatch_events_collects_execute_leaves_with_model() {
        // 根:Decompose → [Execute s1 (deepseek-v4), Execute s2 (qwen3)]。
        let root = ExecPlan {
            subtask: rc_router::capability::Subtask {
                id: "root".into(),
                description: "build app".into(),
                requirements: rc_router::capability::Requirements::default(),
                cost_pressure: rc_router::capability::CostPressure::Med,
                depends_on: vec![],
                risk: rc_router::capability::Risk::Low,
            },
            depth: 0,
            action: ExecAction::Decompose {
                children: vec![exec_subtask("s1", "backend api", "deepseek-v4"), exec_subtask("s2", "react page", "qwen3")],
            },
            basis: "gate-decompose".into(),
        };
        let mut events = Vec::new();
        plan_to_dispatch_events(&root, &mut events);
        assert_eq!(events.len(), 2);
        match &events[0] {
            AgentEvent::OrchestratorDispatch { child_id, model, .. } => {
                assert_eq!(child_id, "s1");
                assert_eq!(model, "deepseek-v4"); // 自动选中的模型
            }
            other => panic!("expected dispatch, got {other:?}"),
        }
        match &events[1] {
            AgentEvent::OrchestratorDispatch { child_id, model, prompt, .. } => {
                assert_eq!(child_id, "s2");
                assert_eq!(model, "qwen3");
                assert_eq!(prompt, "react page");
            }
            other => panic!("expected dispatch, got {other:?}"),
        }
    }

    // ---- /skill-nav 驱动(FileEnv::skill_nav → drive_skill_nav)----

    /// 按 SkillStore 的落盘约定(category 点分 → 嵌套目录)写一个 SKILL.md。
    fn write_nav_skill(root: &std::path::Path, name: &str, desc: &str, category: &str, body: &str) {
        let dir = root.join(category.replace('.', "/")).join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!(
                "---\nname: {name}\ndescription: {desc}\ncategory: {category}\n---\n{body}"
            ),
        )
        .unwrap();
    }

    fn nav_tempdir() -> std::path::PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("rc-cli-nav-{stamp}"))
    }

    fn nav_cleanup(root: &std::path::Path) {
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn skill_nav_leaf_returns_body() {
        let root = nav_tempdir();
        write_nav_skill(&root, "react", "react framework", "frontend", "REACT BODY");
        let store = SkillStore::new(&root);
        let network = SkillNetwork::from_store(&store);
        let router = SkillRouter::new(store.discover());
        let lines = drive_skill_nav(&network, &router, "build a react page", None).unwrap();
        let joined = lines.join("\n");
        assert!(joined.contains("## react"), "leaf title missing: {joined}");
        assert!(joined.contains("REACT BODY"), "leaf body missing: {joined}");
        nav_cleanup(&root);
    }

    #[test]
    fn skill_nav_index_shows_menu_when_no_child_matches() {
        // 索引 frontend(空正文)+ 两个叶子子(react/css);任务只命中索引本身,
        // 不命中任何子 → 应给菜单(最小可交付)而非自动下钻到叶子正文。
        let root = nav_tempdir();
        write_nav_skill(&root, "frontend", "frontend skills", "frontend", "");
        write_nav_skill(&root, "react", "react framework", "frontend.frontend", "REACT BODY");
        write_nav_skill(&root, "css", "css styling", "frontend.frontend", "CSS BODY");
        let store = SkillStore::new(&root);
        let network = SkillNetwork::from_store(&store);
        let router = SkillRouter::new(store.discover());
        let lines = drive_skill_nav(&network, &router, "frontend", None).unwrap();
        let joined = lines.join("\n");
        assert!(joined.contains("可选方向"), "menu must list directions: {joined}");
        assert!(joined.contains("[react]"), "menu must list child react: {joined}");
        assert!(joined.contains("[css]"), "menu must list child css: {joined}");
        assert!(!joined.contains("REACT BODY"), "must not descend into leaf body: {joined}");
        nav_cleanup(&root);
    }

    #[test]
    fn skill_nav_direct_name_opens_index_or_leaf() {
        let root = nav_tempdir();
        write_nav_skill(&root, "frontend", "frontend skills", "frontend", "");
        write_nav_skill(&root, "react", "react framework", "frontend.frontend", "REACT BODY");
        let store = SkillStore::new(&root);
        let network = SkillNetwork::from_store(&store);
        let router = SkillRouter::new(store.discover());
        // 直接点名叶子 → 正文。
        let lines = drive_skill_nav(&network, &router, "react", None).unwrap();
        assert!(lines.join("\n").contains("REACT BODY"));
        // 直接点名索引 → 菜单。
        let lines = drive_skill_nav(&network, &router, "frontend", None).unwrap();
        assert!(lines.join("\n").contains("可选方向"));
        nav_cleanup(&root);
    }

    #[test]
    fn skill_nav_backtrack_budget_limits_loop() {
        // 4 层索引链 root-index → alpha → beta → gamma → delta(叶子),任务命中每层。
        // descend_budget(3) 在 gamma 处耗尽 → 回溯;回溯 2 次后到达 backtrack_budget(2)
        // 上限 → 驱动方停止自动导航,输出预算耗尽提示(不再无限 descend/backtrack)。
        let root = nav_tempdir();
        write_nav_skill(&root, "root-index", "root index alpha beta gamma delta", "root", "");
        write_nav_skill(&root, "alpha", "alpha index", "root.root-index", "");
        write_nav_skill(&root, "beta", "beta index", "root.root-index.alpha", "");
        write_nav_skill(&root, "gamma", "gamma index", "root.root-index.alpha.beta", "");
        write_nav_skill(
            &root,
            "delta",
            "delta leaf",
            "root.root-index.alpha.beta.gamma",
            "DELTA BODY",
        );
        let store = SkillStore::new(&root);
        let network = SkillNetwork::from_store(&store);
        let router = SkillRouter::new(store.discover());
        let lines = drive_skill_nav(&network, &router, "alpha beta gamma delta", None).unwrap();
        let joined = lines.join("\n");
        assert!(
            joined.contains("回溯预算耗尽"),
            "driver must stop backtracking at backtrack_budget(2): {joined}"
        );
        // 预算耗尽后给出的是索引菜单(可选方向),不是更深处的叶子正文。
        assert!(joined.contains("可选方向"), "must fall back to an index menu: {joined}");
        nav_cleanup(&root);
    }

    #[test]
    fn skill_nav_records_leaf_hit_success() {
        // 生产路径驱动方应把叶子命中写进 navigation_log(darwinian fitness 数据源),
        // 而非只有测试在 record。root=顶层 skill,task=任务文本,path=[叶子]。
        let root = nav_tempdir();
        write_nav_skill(&root, "react", "react framework", "frontend", "REACT BODY");
        let skill_store = SkillStore::new(&root);
        let network = SkillNetwork::from_store(&skill_store);
        let router = SkillRouter::new(skill_store.discover());
        let store = Store::open_in_memory().unwrap();

        let lines = drive_skill_nav(&network, &router, "build a react page", Some(&store)).unwrap();
        assert!(lines.join("\n").contains("REACT BODY"));

        let recs = store.list_navigation(10).unwrap();
        assert_eq!(recs.len(), 1, "leaf hit must record exactly one navigation");
        assert!(matches!(recs[0].outcome, NavOutcome::Success), "outcome: {:?}", recs[0].outcome);
        assert_eq!(recs[0].root, "react");
        assert_eq!(recs[0].task_signature, "build a react page");
        assert_eq!(recs[0].path_json, r#"["react"]"#);
        nav_cleanup(&root);
    }

    #[test]
    fn skill_nav_records_budget_exhausted_stop() {
        // 回溯预算耗尽 → 记 BudgetExhausted(root=顶层 skill)。与
        // skill_nav_backtrack_budget_limits_loop 同一目录结构,只是断言数据写入。
        let root = nav_tempdir();
        write_nav_skill(&root, "root-index", "root index alpha beta gamma delta", "root", "");
        write_nav_skill(&root, "alpha", "alpha index", "root.root-index", "");
        write_nav_skill(&root, "beta", "beta index", "root.root-index.alpha", "");
        write_nav_skill(&root, "gamma", "gamma index", "root.root-index.alpha.beta", "");
        write_nav_skill(
            &root,
            "delta",
            "delta leaf",
            "root.root-index.alpha.beta.gamma",
            "DELTA BODY",
        );
        let skill_store = SkillStore::new(&root);
        let network = SkillNetwork::from_store(&skill_store);
        let router = SkillRouter::new(skill_store.discover());
        let store = Store::open_in_memory().unwrap();

        let lines = drive_skill_nav(&network, &router, "alpha beta gamma delta", Some(&store)).unwrap();
        assert!(lines.join("\n").contains("回溯预算耗尽"));

        let recs = store.list_navigation(10).unwrap();
        assert_eq!(recs.len(), 1, "budget stop must record exactly one navigation");
        assert!(
            matches!(recs[0].outcome, NavOutcome::BudgetExhausted),
            "outcome: {:?}",
            recs[0].outcome
        );
        assert_eq!(recs[0].root, "root-index");
        assert_eq!(recs[0].task_signature, "alpha beta gamma delta");
        nav_cleanup(&root);
    }
}
