---
name: git-discipline
description: Inspect git status/diff before and after changes, keep edits scoped, and never overwrite unrelated user work.
short_description: Respect git boundaries.
category: workflow.git
relations:
  - kind: prerequisite
    skill: read-then-edit
triggers:
  - git
  - commit
  - diff
  - branch
  - git
  - 提交
  - 分支
  - commit
  - diff
tags: [git, safety, workflow]
version: 1
confidence: 0.9
usage_count: 0
success_rate: 0.0
auto: false
origin: manual
scope: user
allow_implicit: true
products: []
---
# Git Discipline

1. Before editing, run `git status --short` and `git diff` when the repo is
   dirty; understand what the user already changed.
2. Keep edits scoped to the requested modules; do not reformat or churn
   unrelated files.
3. Never revert or reset user changes.
4. After work, show `git status --short` and `git diff --stat` so the user can
   review exactly what changed.
5. Suggest a commit only when asked, and never use destructive git commands
   without explicit approval.
