//! Environment bridge between the TUI REPL and the hosting CLI binary.
//!
//! The TUI crate must not depend on `rc-cli` (that would be a cycle:
//! rc-cli → raincode-tui → rc-cli). Instead the REPL is written against this
//! trait; `rc-cli` implements it with its own `FileConfig` / helpers, and the
//! concrete session/agent/store plumbing stays in the CLI.

use std::path::PathBuf;
use std::sync::Arc;

use rc_core::AgentConfig;
use rc_pro::Provider;
use rc_profile::model::Registry;
use rc_state::Store;

pub type BoxProvider = Arc<dyn Provider + Send + Sync + 'static>;

/// 监督 feed:route_run 线程把子代理事件写入,TUI 主循环周期排空并判断。
/// `Arc<Mutex<Vec<AgentEvent>>>` 是唯一跨线程共享结构(route_run 持有写入端,
/// TUI 持有读取端),避免把 `Supervisor`/`Provider` 传过线程边界。
pub type AgentFeed = std::sync::Arc<std::sync::Mutex<Vec<rc_proto::AgentEvent>>>;

/// 模型选择器条目:配置过的真实模型 + 其真实榜单能力分(用于 ⬆/⬇ 标注)。
/// `id` 是 registry 的 profile id(切活跃用);`provider/model` 区分供应渠道
/// (同一模型名在不同渠道是不同的,如 deepseek/ds-* vs opencode/ds-*)。
#[derive(Debug, Clone)]
pub struct ModelPickerEntry {
    pub id: String,
    pub provider: String,
    pub model: String,
    pub active: bool,
    pub reasoning: f64,
    pub coding: f64,
    pub frontend: f64,
    pub backend: f64,
}

/// Everything the REPL needs from the hosting CLI. Implemented by `rc-cli`.
///
/// `?Send` futures: the REPL loop runs under `Runtime::block_on` (main thread),
/// never `tokio::spawn`, so the `&Store` borrow held across `route_run`'s
/// awaits (rusqlite `Connection` is `Send` but not `Sync`) is acceptable.
#[async_trait::async_trait(?Send)]
pub trait ReplEnv {
    fn load_registry(&self) -> anyhow::Result<Registry>;
    fn save_registry(&self, registry: &Registry) -> anyhow::Result<()>;
    /// Raincode home dir (`~/.raincode` or RAINCODE_HOME). Used for persisted
    /// prompt history.
    fn home_dir(&self) -> std::path::PathBuf;
    fn skills_dir(&self) -> PathBuf;
    fn workspace(&self) -> PathBuf;
    /// Create a session row and return its id (persisted in state.db).
    fn create_session(&self) -> anyhow::Result<String>;
    /// Open a Store handle (same state.db as create_session).
    fn open_store(&self) -> anyhow::Result<Store>;
    fn make_provider(&self, registry: &Registry) -> anyhow::Result<BoxProvider>;
    fn dispatch_slash(&self, name: &str, args: &serde_json::Value) -> Result<String, String>;
    /// 导航 skill 网络(`/skill-nav <task>`):命中索引 → 返回菜单(用户经
    /// `/skill-nav <子名>` 下钻),命中叶子 → 返回完整正文。由宿主 CLI 实现。
    fn skill_nav(&self, task: &str) -> Result<Vec<String>, String>;
    fn store_key(&self, id: &str, key: &str) -> anyhow::Result<()>;
    fn key_ref(&self, id: &str) -> String;
    /// 连通性自检:用 profile(含 key)发最小请求,验证「选模型 → 贴 key → 连上」。
    /// 返回成功消息;失败返回错误(不打印 key)。由宿主 CLI 实现。
    async fn verify_connectivity(&self, profile: &rc_profile::model::Profile) -> anyhow::Result<String>;
    /// Build an agent config for a task run. `with_slash_command` enables the
    /// user-driven slash-command tool (chat model executes explicit commands).
    async fn agent_config(
        &self,
        registry: &Registry,
        with_slash_command: bool,
    ) -> anyhow::Result<AgentConfig>;
    /// 当前活跃模型的上下文窗口(token);0 = 未知(用 128k 兜底)。
    fn context_window(&self, registry: &Registry) -> u64;
    /// 刷新模型能力评分(`/refresh-model-scores`):拉取 OpenRouter/arena 真实榜单分入库。
    /// 返回摘要文本。网络失败返回 Err。
    async fn refresh_profiles(&self) -> anyhow::Result<String>;
    /// 列出配置过的真实模型 + 真实榜单能力分(交互式 `/model` 选择器用)。
    fn model_picker_entries(&self) -> anyhow::Result<Vec<ModelPickerEntry>>;
    /// 启动监督,返回监督锚点(`Arc<Supervisor>`)。model 可选指定监督模型;
    /// None 用活跃模型。TUI 主循环持有它,route_run 时作为监督开关传入。
    fn supervise_start(&self, registry: &Registry, model: Option<&str>) -> Result<std::sync::Arc<rc_core::Supervisor>, String>;
    /// 策略文件路径(默认 ~/.raincode/supervise.toml)。
    fn supervise_config_path(&self) -> PathBuf;
    /// Start a routed multi-agent run **off the main loop** (own thread + runtime),
    /// forwarding supervision events into `emit` and registering each sub-agent
    /// with `steer_hub`. Returns immediately; the run continues in the background.
    /// Non-async because the route future is `!Send` (rusqlite `&Store` across
    /// await) and must live on a single thread. `plan_only=true` 只拆解计划不执行
    /// (`/autonomous --plan` / thinking 确认阶段)。`cancel` 是 run 的取消令牌:
    /// `/stop` 置位 → 引擎在下一检查点中断。`risk_mode` 是本次 run 的风险模式
    /// (TUI 共享 Arc 的当前值,驱动 CLI 侧 `RiskState` 的棘轮升级策略)。
    /// `supervisor` 为 Some 时(监督会话已启动)route_run 把子代理事件
    /// (AgentSpawned/AgentToolCall/AgentResult)转发一份到 `feed`,由 TUI 主循环
    /// 周期排空并调用 `Supervisor::should_judge`/`judge`。
    #[allow(clippy::too_many_arguments)]
    fn route_run(&self, prompt: String, plan_only: bool, emit: Arc<dyn Fn(rc_proto::AgentEvent) + Send + Sync>, steer_hub: Arc<rc_core::SteerHub>, cancel: std::sync::Arc<std::sync::atomic::AtomicBool>, risk_mode: rc_router::risk::RiskMode, supervisor: Option<std::sync::Arc<rc_core::Supervisor>>, feed: AgentFeed);
}
