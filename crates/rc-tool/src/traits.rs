use async_trait::async_trait;
use rc_sandbox::guard::SuperviseConfig;
use rc_sandbox::guard_hook::{GuardHook, SessionGuardMemo};
use rc_sandbox::{ApprovalHook, AutoUserHook, CommandPolicy, NetworkPolicy, UserInputHook};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// 子代理工厂:主模型通过 delegate_research 工具派一个聚焦子代理,拿回最终文本。
/// 宿主 CLI 注入闭包(用活跃 provider + 新会话建子 Agent);未注入时工具报错。
pub type SubagentFn = dyn Fn(String) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'static>,
    > + Send
    + Sync;

#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Debug, Clone, Default)]
pub struct ToolResult {
    pub ok: bool,
    pub output: String,
    /// 输出超限时完整内容的落盘路径(None = 内联)。
    pub output_path: Option<String>,
}

impl ToolResult {
    pub fn ok(output: impl Into<String>) -> Self {
        Self {
            ok: true,
            output: output.into(),
            output_path: None,
        }
    }

    pub fn err(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            output: message.into(),
            output_path: None,
        }
    }
}

pub struct ToolContext {
    pub cwd: PathBuf,
    pub approval: Arc<dyn ApprovalHook>,
    pub command_policy: CommandPolicy,
    pub network_policy: NetworkPolicy,
    pub user_input: Arc<dyn UserInputHook>,
    pub timeout: Duration,
    pub max_output_bytes: usize,
    /// 子代理工厂(delegate_research 工具用);宿主注入后 chat 模型可派聚焦子代理。
    pub subagent: Option<Arc<SubagentFn>>,
    /// 监督守卫配置(默认 None = 守卫关闭)。
    pub guard_cfg: Option<SuperviseConfig>,
    /// 用户授权闸 hook(高危操作弹三选一)。
    pub guard_hook: Option<Arc<dyn GuardHook>>,
    /// 会话级放行记忆。
    pub guard_memo: Option<Arc<SessionGuardMemo>>,
    /// 策略文件所在目录(含 supervise.toml);Forever 放行写回用。None = 不可写回。
    pub supervise_dir: Option<PathBuf>,
}

impl ToolContext {
    pub fn new(cwd: PathBuf, approval: Arc<dyn ApprovalHook>) -> Self {
        Self {
            cwd,
            approval,
            command_policy: CommandPolicy::default(),
            network_policy: NetworkPolicy::default(),
            user_input: Arc::new(AutoUserHook::default()),
            timeout: Duration::from_secs(120),
            max_output_bytes: 128 * 1024,
            subagent: None,
            guard_cfg: None,
            guard_hook: None,
            guard_memo: None,
            supervise_dir: None,
        }
    }

    /// Builder:挂上监督守卫(配置/授权闸 hook/会话记忆)。None = 该项不启用。
    pub fn with_guard(
        mut self,
        guard_cfg: Option<SuperviseConfig>,
        guard_hook: Option<Arc<dyn GuardHook>>,
        guard_memo: Option<Arc<SessionGuardMemo>>,
    ) -> Self {
        self.guard_cfg = guard_cfg;
        self.guard_hook = guard_hook;
        self.guard_memo = guard_memo;
        self
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    async fn run(&self, args: Value, ctx: &ToolContext) -> ToolResult;
}

pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new(tools: Vec<Box<dyn Tool>>) -> Self {
        Self { tools }
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.iter().map(|t| t.spec()).collect()
    }

    pub fn find(&self, name: &str) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .find(|t| t.spec().name == name)
            .map(|t| t.as_ref())
    }

    pub async fn run(&self, name: &str, args: Value, ctx: &ToolContext) -> ToolResult {
        match self.find(name) {
            Some(tool) => tool.run(args, ctx).await,
            None => ToolResult::err(format!("unknown tool '{name}'")),
        }
    }
}
