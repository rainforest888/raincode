# `raincode profiles enrich` — 设计

日期：2026-08-12
状态：已批准（用户确认，含"价格搜不到→交互式提示用户自己搜"兜底）

## 1. 目的

让用户一条命令刷新/扩充模型能力评分库（`~/.raincode/state.db` 的 `model_profiles` 表）：
- **定价**：取 opencode.ai 实际支付价（优先）+ OpenRouter 价（回退）
- **评分**：结合网上大型榜单（OpenRouter Artificial-Analysis 指数 + Design-Arena Elo，缺口由 LLM 补）
- 只丰富**外部热门榜前 N（默认 15）最常用模型** + 用户 `--add` 指定的模型
- 每模型**只留最新版、删除旧版**，避免派发到过时模型

采用**混合式**（方案 B）：确定性 OpenRouter 基准 + LLM 研究 agent 主动上网补漏，而非纯 agent 或纯确定性。

## 2. 命令接口

```
raincode profiles enrich [--add <model1,model2>] [--top <N>] [--model <profile-id>] [--dry-run]
```

- `--add`：热门榜之外，用户额外指定要搜的模型（裸名，如 `glm-5.2,kimi-k3`）
- `--top`：外部热门榜取前几名（默认 `15`）
- `--model`：研究 agent 用哪个 provider profile（默认 = 当前活跃模型）
- `--dry-run`：只打印将写入的内容，不改库

`ProfilesCmd` 枚举新增 `Enrich` 变体，与现有 `Show` / `Refresh` 平级。

## 3. 架构

`crates/rc-cli/src/enrich.rs` 新模块承载整个流程，复用现有基建：
- **provider_for_profile / create_provider**：研究 agent 的 provider（含 key 解析）
- **network_tools**：web_fetch / web_search 工具（默认网络策略放行）
- **Agent + run**：rc-core 一次性 agent（`--dry-run` 外真正执行）
- **upsert_model_profile / all_model_profiles / delete_model_profile**：rc-state 存取
- **user_input_hook**：交互式定价兜底（`ask_user`）

三个职责单元（enrich.rs 内的子模块/函数，各单一用途）：
1. `baseline`：确定性拉 OpenRouter → 基线行
2. `research`：构建 agent 提示词、spawn agent、解析其 JSON → 增量行
3. `apply`：合并基线+增量 → 只留最新版/删旧版 → upsert → 打印对比

## 4. 数据流（三阶段）

**阶段 1 — 确定性基准（命令自己做）**
1. `GET https://openrouter.ai/api/v1/models`（406 模型，公开接口）
2. 解析：reasoning/coding/math = artificial_analysis 指数；frontend/backend = design_arena Elo（1000→0,1400→100）；long_context = context/128k 归一化
3. **定价修正**：OpenRouter 给的是 $/token，必须 ×1e6 得 $/M（修复 `parse_openrouter_models` 缺失 ×1e6 的 bug，见 §8）
4. 结果 = 全量基线行（`source=openrouter-arena`）

**阶段 2 — LLM 上网研究（研究 agent，活跃模型 + web 工具）**
agent 收到提示词，用 web_fetch/web_search 调研并输出 JSON：

1. **外部热门榜前 N**：抓 LMArena 投票榜 → 最常用模型 id 列表；LMArena 不可达 → OpenRouter 热门兜底；再不可达 → 内置常用兜底清单
2. **opencode 定价**：抓 opencode.ai 定价页（JS 页，用 web_fetch 尽力提取）→ 每模型实际支付 $/M；**搜不到 → 用 ask_user 提示用户自己搜、输入价格**（非交互 → 回退 OpenRouter 价，source 标 `openrouter`）
3. **评分补缺口**：对基线里缺 frontend/backend（如 qwen3.8-max）或 reasoning/coding 为 0 的模型，搜 Artificial Analysis / 官方页补估算
4. **最新版判断**：每模型取最新版本 id（如 `deepseek-v4-flash-0731` vs `-latest` 取新者）
5. **`--add` 模型**：同样流程搜这些

agent 输出 JSON 结构（每模型一条）：
```json
{
  "popular": ["deepseek-v4-flash", "qwen3.8-max", "mimo-v2.5", "glm-5.2", ...],
  "models": [
    {"id": "deepseek-v4-flash-0731", "latest": true,
     "reasoning": 51.8, "coding": 69.1, "frontend": 65.2, "backend": 65.0,
     "math": 51.8, "long_context": 90, "input_cost_per_m": 0.08,
     "output_cost_per_m": 0.18, "context_window": 1048576,
     "price_source": "opencode" | "openrouter" | "user"}
  ]
}
```

**阶段 3 — 落库（命令自己做）**
1. 解析 agent JSON；失败 → 报错、**不部分写入**（库保持原样）
2. 目标集合 = `popular` 前 N + `--add`
3. 对目标集合内每模型：**只保留 `latest:true` 行，删除其余同名旧版本行**。latest 判定：agent 显式给 `latest:true`；缺失时命令按"同名组（去掉 provider 前缀与版本后缀后相同）内版本号最高/日期最新"兜底判定
4. upsert（`ON CONFLICT(model) DO UPDATE`）
5. 打印前后对比表（model | reasoning | coding | frontend | backend | $/M in | ctx | source）

## 5. 数据源与优先级

| 数据 | 主源 | 回退 1 | 回退 2 |
|---|---|---|---|
| 评分 | OpenRouter AA/DA（确定性） | LLM 补缺口（AA/官方页） | — |
| 定价 | opencode.ai（LLM 搜，`source=opencode`） | 交互式 ask_user（`source=user`） | OpenRouter（`source=openrouter`） |
| 热门榜 | LMArena 投票榜 | OpenRouter 热门 | 内置兜底清单 |

## 6. 错误处理

- LMArena / opencode 不可达 → 静默回退下一优先级，不报错、不中断
- 研究 agent 输出无有效 JSON → 命令报错退出，**不写库**（原子性）
- `--add` 的模型搜不到任何数据 → 跳过该模型并在对比表里标注 `skipped`
- 非交互模式 + 定价搜不到 → OpenRouter 价 + `source=openrouter`（不卡住等待输入）

## 7. 测试

**单元**：
- `parse_openrouter_models` 定价 ×1e6 后数值正确（deepseek 0.00000008 → 0.08）
- agent JSON 解析：正常 / 少字段 / 坏 JSON → 报错不写库
- keep-latest/delete-old：同名多版本只留最新
- normalize_arena_elo 边界

**集成**：
- mock agent 输出 → 验证 upsert 与删旧版
- 网络不可达 → 验证回退路径（dry-run 可测）

**手动**：真实 opencode key 跑一遍，观察对比表与交互式定价提示

## 8. 附带修复

`parse_openrouter_models`（rc-cli/src/main.rs）的定价单位 bug：
- 现在：`input_cost_per_m = pricing.prompt.max(0.0001)`（$/token 直接当 $/M 存，全部 ≈0.0001）
- 修复：×1e6 得真实 $/M；否则 enrich 的确定性基准定价错误，且任何 `profiles refresh` 会覆盖修正

## 9. 涉及文件

- `crates/rc-cli/src/main.rs`：`ProfilesCmd::Enrich` + dispatch + `parse_openrouter_models` 定价修复
- `crates/rc-cli/src/enrich.rs`（新）：三阶段流程（baseline / research / apply）
- `crates/rc-state/src/db.rs`：如有需要加 `delete_model_profile`、`model_latest_versions` 辅助
- `tools/enrich_models.py`（已存在）：保留作确定性基准的参考实现

## 10. 范围界定（YAGNI）

- 不做自动定时刷新 / daemon
- 不做评分权重/维度扩展（沿用现有 6 维）
- 不做多 provider 的评分去重合并（只 opencode + openrouter 定价，评分统一 OpenRouter 系）
- 交互式定价仅对 opencode 来源触发，OpenRouter 价不需要用户输入
