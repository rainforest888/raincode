---
name: read-then-edit
description: Read the exact target file and surrounding context before making any edit; never patch blind.
short_description: Read before you edit.
category: workflow.editing
relations:
  - kind: prerequisite
    skill: test-after-change
triggers:
  - edit
  - patch
  - modify
  - refactor
  - 修改
  - 编辑
  - 改
  - 重构
  - 补丁
  - patch
tags: [editing, safety, context]
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
# Read Then Edit

Before changing a file, load it with `read_file` and confirm the exact text you
plan to replace exists. Prefer `apply_patch` with a unique old/new hunk over
regenerating whole files.

1. Read the file and, when useful, grep for related symbols.
2. Choose `apply_patch` mode `hunk` (first match) or `whole` only when replacing
   the entire file is intended.
3. Re-read the changed region after writing.
4. If the patch fails because old text is missing, stop and re-read; do not
   blindly rewrite from memory.

Pitfalls: line endings differ on Windows; use exact file content, not an
approximation from memory.
