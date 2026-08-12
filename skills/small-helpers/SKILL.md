---
name: small-helpers
description: Prefer small, single-purpose helper functions/files over one large abstraction; use them consistently.
short_description: Build small reusable pieces.
category: design.helpers
relations:
  - kind: refines
    skill: read-then-edit
triggers:
  - helper
  - utility
  - abstraction
  - refactor
  - 工具函数
  - 封装
  - 抽象
  - 小函数
  - 复用
  - helper
tags: [design, helpers, maintainability]
version: 1
confidence: 0.85
usage_count: 0
success_rate: 0.0
auto: false
origin: manual
scope: user
allow_implicit: true
products: []
---
# Small Helpers

1. Extract repeated logic into a small helper with one clear responsibility.
2. Give it a descriptive name and a focused signature; avoid configuration
   blobs and hidden global state.
3. Reuse it across call sites instead of duplicating inline logic.
4. Only create a wider abstraction when two or more helpers share a real
   contract; otherwise keep them flat.
5. Cover each helper with a focused test where the project has a test setup.
