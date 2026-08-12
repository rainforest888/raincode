---
name: plan-before-code
description: Think through the approach before writing code — clarify intent, sketch the smallest viable plan, then implement step by step. Never jump straight to edits on an ambiguous task.
short_description: Plan before you code.
category: planning.workflow
triggers:
  - 计划
  - 规划
  - 设计
  - 方案
  - 先想
  - plan
  - design
  - approach
  - 思路
tags: [planning, design, clarity]
version: 1
confidence: 0.9
usage_count: 0
success_rate: 0.0
auto: false
origin: seed
scope: system
allow_implicit: true
products: []
---
# Plan Before Code

Rushing into edits on a half-understood task produces churn. Establish the
approach first, cheaply.

1. Restate the task in one sentence; if any ambiguity would change the
   implementation, resolve it (ask, or state the assumption).
2. Sketch the smallest viable change: files touched, data flow, edge cases.
3. Implement in order, verifying each step.
4. Only after the plan is clear do you reach for tools.

When the task is genuinely small (a one-line fix), a one-sentence plan is
enough — planning is proportional to risk, not a ceremony.
