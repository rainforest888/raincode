# Raincode

**模型层 harness 优化器(Rust 实现):低价模型池、能力路由、prompt 缓存、provider 可靠性、自我迭代,为你的 AI harness 提供高效可靠的模型层。**

---

## 它解决什么

如果你在用某个 AI 编码 harness(Claude Code / opencode / 自己的聚合通道 / 多模型代理),你面对的问题通常是:

| 问题 | 表现 |
|---|---|
| **模型贵** | 一个强模型干所有活,钱烧得快 |
| **缓存命中率低** | 反复把同样的 system/工具定义发给模型,白付 token |
| **单点故障** | 某个模型端点一挂,任务全挂,无重试无切换 |
| **模型池难维护** | 哪个便宜、哪个能打、评分过时,全靠手查 |

raincode 就是针对这些的**模型层优化器**——它不抢你主 agent 的活,管好它背后的模型层。

## 核心能力(均已实现并验证)

### 1. 低价模型池 + 自动 enrich
- 从 OpenAI 兼容聚合通道(如 `opencode.ai/zen/go/v1`)一把 key 通吃几十个低价模型。
- `raincode profiles enrich`:自动上网拉取**真实定价与评分**(OpenRouter Artificial-Analysis / Design-Arena),构建高性价比模型池;只留最新版删旧版;价格搜不到时交互式问你要。

### 2. 能力 + 成本路由
- `raincode route` 把复杂任务拆成子任务,按能力与成本派给池里最合适的模型:
  - 常规/便宜任务 → 低价模型(如 deepseek-v4-flash ≈ $0.08/M)
  - 硬核/能力关键任务 → 强模型(如 qwen3.8-max ≈ $2/M,只在能力差距明显时启用)
- 成本偏置可调:`[core] cost_bias`(>1 更愿用便宜模型)。

### 3. 面向 deepseek 的 prompt 缓存优化
- **StablePrefix**:系统前缀字节稳定、工具定义稳定、消息只增不改 —— 命中 deepseek 的自动前缀缓存,目标极高命中率。
- Anthropic 3 个 `cache_control` 断点 + OpenAI `prompt_cache_key`。

### 4. Provider 可靠性
- 503/429/5xx/传输抖动**自动重试**(指数退避)。
- 子任务因 provider 失败 → **自动故障转移**到池内另一模型(如 qwen 宕机 → 切 deepseek 重跑)。

### 5. 自我迭代
- 它能审查并改进自己的代码库(rc-mcp、缓存架构等核心)。
- 在副本上跑 `raincode route "<改进任务>"` 迭代,产物经验证后同步回主仓库。本次开发中,它自己产出了 MCP 规范补全(initialized 通知、SSE 多行解析)并由此暴露了重试/故障转移/编译门禁等真实缺口。

### 6. MCP 客户端
- stdio / HTTP / SSE MCP 服务器,工具以 `mcp__<server>_<tool>` 命名暴露。

## 快速开始

```bash
# 配置模型池(选 provider → 填 API key,密钥只在 ~/.raincode/keys/)
raincode setup

# 自动 enrich 模型池:上网拉真实定价/评分(热门榜前 N + 自定义)
raincode profiles enrich --top 15 --add glm-5.2,kimi-k3 --dry-run

# 用模型池跑一个复杂任务:自动拆解 + 按能力派活 + 并发执行
raincode route "做一个带界面的计算器"

# 单次任务(单个模型)
raincode run "写一个 python 函数并测试"

# 让它迭代自己的核心(MCP/缓存/可靠性)
raincode route "审查 crates/rc-mcp 和缓存架构,找出改进点并实施"
```

常用配置(`~/.raincode/config.toml`):

```toml
[core]
approval_mode = "auto"
max_turns = 24
subtask_timeout_secs = 600   # 子任务超时(跑 cargo test 的编译型子任务需要)
cost_bias = 1.0              # >1 更愿用便宜模型,<1 更能力优先
```

## 架构

```
crates/rc-core        agent 循环 + StablePrefix 缓存 + 工具并发 + step 上限
crates/rc-pro         provider 抽象(OpenAI/Anthropic/兼容/Ollama/mock)+ 缓存断点 + 重试
crates/rc-profile     provider 注册表 + 密钥管理(密钥只在 ~/.raincode/keys/)
crates/rc-router      能力路由 + allocator 拆解 + 子代理并行 + 故障转移 + 递归守卫
crates/rc-skill       skill 解析 / 目录拓扑 / 嵌入路由 / seed 语料 / 导航器
crates/rc-tool        内置工具 + 输出持久化
crates/rc-sandbox     命令/网络策略 + 监督守卫
crates/rc-mcp         MCP 客户端(stdio/HTTP/SSE)
crates/rc-state       SQLite 持久化(会话/skill/模型评分/经验)
crates/rc-cli         CLI:`run` / `route` / `profiles enrich` / `setup`
tui/                  ratatui 终端 UI(极简,面向 harness 观测)
```

## 测试

```bash
cargo test --workspace    # 零网络(mock provider)+ 部分真实网络集成测试
cargo clippy --workspace --all-targets
```

## License

MIT
