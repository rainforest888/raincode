---
name: coding-cycle
description: "Run the full task lifecycle: understand intent, plan, execute, review the result, and re-understand before repeating when the review rejects the work."
short_description: Own the whole coding cycle.
category: workflow.coding
relations:
  - kind: composes
    skill: search-then-ask
  - kind: composes
    skill: git-discipline
  - kind: composes
    skill: read-then-edit
  - kind: composes
    skill: small-helpers
  - kind: composes
    skill: test-after-change
triggers:
  - coding-cycle
  - task lifecycle
  - review
  - re-understand
  - complete the task
  - 任务
  - 实现
  - 编写
  - 开发
  - 完成
  - 拆解
  - 重构
tags: [workflow, lifecycle, planning, review]
version: 1
confidence: 0.95
usage_count: 0
success_rate: 0.0
auto: false
origin: manual
scope: user
allow_implicit: true
products: []
---
# Coding Cycle

Follow the full lifecycle: understand intent, plan, execute, review the actual
result, and re-understand the user's intent before repeating.

1. Understand intent: read the request, inspect the workspace, and ask about
   ambiguity before acting.
2. Plan: choose the relevant skills from the network and state the expected
   outcome.
3. Execute: make small, scoped changes with the selected sub-skills.
4. Review: verify the change with the narrowest test or build command, then
   compare the result to the original intent.
5. Re-understand: when the review rejects the result, revise the intent, adjust
   the plan, and repeat.

Composed skills: search-then-ask, git-discipline, read-then-edit,
small-helpers, and test-after-change.