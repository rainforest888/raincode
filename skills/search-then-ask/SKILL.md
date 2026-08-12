---
name: search-then-ask
description: Grep the codebase first, then use web search or MCP tools only when local evidence is insufficient.
short_description: Local search before external lookup.
category: research.workflow
relations:
  - kind: prerequisite
    skill: read-then-edit
triggers:
  - search
  - find
  - look up
  - why
  - api docs
  - 搜索
  - 查找
  - 查询
  - 为什么
  - 调研
  - api
tags: [search, research, efficiency]
version: 1
confidence: 0.88
usage_count: 0
success_rate: 0.0
auto: false
origin: manual
scope: user
allow_implicit: true
products: []
---
# Search Then Ask

1. Use `grep` over the workspace for symbols, config names and error strings.
2. If local evidence is thin, load a related skill from the skill network.
3. Only then use `web_search`/`web_fetch` or MCP tools for external APIs.
4. Cite the source URL or local file path in the result.
5. If the answer is still uncertain, say what is uncertain instead of guessing.
