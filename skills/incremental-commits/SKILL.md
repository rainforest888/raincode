---
name: incremental-commits
description: Land changes in small, focused commits with clear messages, so each commit is reviewable and reversible. Group related edits; never bury multiple unrelated changes in one commit.
short_description: Small, focused, well-named commits.
category: git.workflow
triggers:
  - 提交
  - commit
  - 版本控制
  - 保存改动
  - 怎么提交
  - 一个提交
tags: [git, workflow, history]
version: 1
confidence: 0.88
usage_count: 0
success_rate: 0.0
auto: false
origin: seed
scope: system
allow_implicit: true
products: []
---
# Incremental Commits

A history of small, well-named commits is a superpower for review and bisect.

1. Make changes in logical units; commit each unit separately when practical.
2. Write a concise subject (`feat:`/`fix:`/`chore:`/`docs:` + short summary)
   and a body only when context is non-obvious.
3. Don't mix unrelated changes in one commit — split them.
4. Check `git status`/`git diff` before committing to confirm only the
   intended files are staged.
