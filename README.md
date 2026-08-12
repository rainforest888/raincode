# Raincode

**自我进化的编程 agent(Rust 实现)——像 Claude Code 一样在终端里干活,但内置可学习的 skill 网络与多 agent 编排。**

Raincode 是一个 Rust 编写的自主编码 agent:你在终端里给它一句话,它会像 Claude Code 一样思考、调用工具、读写文件、跑测试,直到完成任务。但它不止于此——

## 核心优势

### 1. 会学习的 Skill 网络(born capable + 持续进化)
- **开箱即用**:内置 10 个精选 seed skill(规划 / 编码循环 / 调试 / 测试 / git / 审查 / 重构 / 调研…),新会话不用从零调教。
- **真正在学**:每个任务完成后,会话摘要喂给 darwinian 演化引擎——提炼跨任务模式、提升/细化 skill、调整导航判断。`raincode insights` 直接看学习进度。
- **中文友好**:skill 触发词中英双语,中文任务也能精准匹配(实测 `"写个测试验证"` → 命中 test-after-change)。

### 2. 两模式路由(普通 ↔ Thinking)
- **普通模式**:快。chat 模型单模型 + skill,适合简单任务。
- **Thinking 模式**:稳。先拆解任务划分 → 你确认 → 展开模型网络分步执行 + 审查。
- 每次任务自动判难度路由,`/thinking` `/normal` 可手动覆盖。

### 3. 子代理网络(任务自动分解 + 并行执行)
- `raincode route` 把复杂任务自动拆成子任务,按能力分派给最合适的模型,**并发执行**,TUI 里可展开任务树看每个子代理进度。
- 递归深度 / 预算守卫,防失控。

### 4. 三层工具守卫
用户授权闸(最高,不可绕过)> 确定性硬规则(工作区外销毁 / 上传 / deny 列表)> LLM 监督 agent(`/supervise` 用自然语言定底线)。高危操作在**对话流里直接打勾**(Y=允许 / N=拒绝 / A=本会话允许),不进输入栏。

### 5. 工程级 Prompt 缓存(省钱、提速)
- system 前缀字节稳定(任务不进 system,skill 目录稳定,工具定义缓存)。
- Anthropic 3 个 `cache_control` 断点 + OpenAI `prompt_cache_key`。
- 对齐 pi / codex / opencode 的缓存架构,长会话成本显著下降。

### 6. 对齐 Claude Code 的 TUI
- 思考链实时 3 行 + `Ctrl+O` 展开全程。
- 每工具一行状态机(spinner → ✓/✗/删除线)。
- 7 色 agent 轮换 + 子代理树(`Ctrl+T`)+ 导航 footer。
- 运行中可打字排队(steering)+ `Esc` 按状态分派。
- 智能自动滚动 + 消息导航点。

### 7. 多 Provider + MCP
- OpenAI / Anthropic / OpenAI-compatible(DeepSeek 等)/ Ollama / mock。
- MCP 外部工具(stdio / HTTP),`mcp__` 命名,慢连接不阻塞。

### 8. Rust 单二进制
- 无 Node / 无 Electron,一个可执行文件,启动快、内存小。

---

## 快速开始

### 环境
- Rust 1.75+([rustup](https://rustup.rs))。
- 一个支持 OpenAI/Anthropic 协议的模型 API key(DeepSeek / OpenAI / Claude…)。

### 安装(全局 CLI——任何文件夹输入 `raincode` 即可打开)

```bash
git clone https://github.com/raincode/raincode.git
cd raincode
cargo build --release
```

**Windows(PowerShell)**:
```powershell
powershell -ExecutionPolicy Bypass -File install.ps1
```
**macOS / Linux**:
```bash
bash install.sh
```
脚本会把 `raincode` 复制到 `~/.raincode/bin/` 并加入 PATH。**新开一个终端**,任意文件夹输入:

```bash
raincode repl      # 交互式 TUI(推荐)
```

### 首次配置

```bash
raincode setup     # 交互式向导:选 provider → 填 API key
```
API key 只存在 `~/.raincode/keys/`,**绝不进项目文件夹 / 不上传 GitHub**。也支持环境变量:`DEEPSEEK_API_KEY=xxx` 等。

配置文件在 `~/.raincode/config.toml`(不创建也可用默认值):

```toml
[core]
max_turns = 8          # 单任务最大轮数
approval_mode = "auto" # ask | auto | deny

[model]
profile = "deepseek-v4-flash"   # 与 ~/.raincode/profiles.toml 里的 profile id 对应

[evolve]
enabled = false        # 打开后启用 darwinian skill 演化
```

### 用起来

```bash
raincode run "写一个 python 函数对列表去重排序,并写测试验证"   # 单次任务
raincode route "做一个带界面的计算器"                         # 复杂任务:拆解 + 子代理
raincode run --entropy "给配置系统加数据库支持"               # Thinking 模式(先问清再干)
raincode insights                                              # 看 skill 网络学习进度
raincode skills list / skills show <name>                     # 管理 skill
raincode repl                                                   # 交互式 TUI
```

### TUI 快捷键

| 键 | 作用 |
|---|---|
| `Enter` | 发任务 |
| `Esc` | 运行中=中断 · 空闲+空输入=回溯上条消息 · 聚焦 agent=退出 |
| `Ctrl+C` | 中断(再按退出) |
| `Ctrl+T` | 折叠/展开子代理任务树 |
| `Tab` | 循环聚焦子代理 · 运行中+有输入=入队 |
| `Ctrl+O` | 展开/收起完整思维链 |
| `,` `.` `p` | 聚焦子代理时:上一/下一/返回父级 |
| `PageUp/Down` | 滚动对话 |

---

## 架构

```
crates/rc-core       agent 循环:StablePrefix 缓存 / AppendOnlyLog / 工具并发 / step 上限 / 锚定压缩
crates/rc-pro        provider 抽象(OpenAI/Anthropic/兼容/Ollama/mock)+ 3 cache_control 断点
crates/rc-profile    provider 注册表 + 密钥管理(密钥只在 ~/.raincode/keys/)
crates/rc-router     能力路由 + allocator 拆解 + 子代理并行执行 + 递归守卫
crates/rc-skill      skill 解析 / 目录拓扑 / 嵌入路由 / seed 语料 / 导航器
crates/rc-evolve     darwinian 演化(session digest → 提炼 → propose/refine)+ 导航反馈
crates/rc-tool       内置工具 + 输出持久化(>50KB → 文件 + 预览)
crates/rc-sandbox    命令/网络策略 + 监督守卫
crates/rc-mcp        MCP 客户端(stdio/HTTP)
crates/rc-state      SQLite 持久化(会话/skill/经验/导航日志)
tui/                 ratatui 交互式终端 UI(Claude Code 观感)
```

## 测试

```bash
cargo test --workspace    # 460+ 测试,mock provider,零网络
cargo clippy --workspace --all-targets  # 零警告
```

## 路线图

- [x] agent 循环 + 缓存架构(StablePrefix / 3 断点 / prompt_cache_key)
- [x] 两模式路由 + 子代理网络
- [x] 可学习 skill 网络(中英触发词 + darwinian 演化)
- [x] Claude-Code 对齐 TUI(思考展开 / 审批打勾 / 7 色 / steering)
- [ ] macOS / Linux 原生打包发布
- [ ] tencent agent-memory 适配
- [ ] 更丰富的 skill 社区语料

## License

Apache-2.0
