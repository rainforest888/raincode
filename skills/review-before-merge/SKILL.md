---
name: review-before-merge
description: Before declaring work done, re-read the diff as a reviewer — check correctness, edge cases, and whether the change matches intent; run the relevant tests; then write a concise summary of what changed and why.
short_description: Review your own work before declaring done.
category: review.workflow
triggers:
  - 审查
  - 复查
  - 检查代码
  - 检查一下
  - review
  - 验证一下
  - 完成了吗
  - 检查是否正确
tags: [review, verification, quality]
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
# Review Before Merge

"Done" means reviewed, not just written.

1. Re-read the diff as a fresh reviewer: does it do what was asked? Any edge
   case, off-by-one, or silent failure?
2. Run the smallest relevant test / build to confirm the change compiles and
   behaves.
3. When the user asks "is it done?" / "检查一下", summarize concretely: what
   changed, what was verified, and any remaining caveats — not a vague "all
   good".
