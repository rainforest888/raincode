---
name: test-after-change
description: Run the smallest relevant test suite after any code change and fix failures before declaring done.
short_description: Verify every change.
category: testing.workflow
triggers:
  - test
  - verify
  - cargo test
  - pnpm test
  - pytest
  - 测试
  - 验证
  - 单测
  - 运行测试
  - pytest
  - 测试通过
tags: [testing, verification, regression]
version: 1
confidence: 0.92
usage_count: 0
success_rate: 0.0
auto: false
origin: manual
scope: user
allow_implicit: true
products: []
---
# Test After Change

Every edit that can affect behavior must be verified.

1. Identify the narrowest test command for the changed area.
2. Run it after the change; record the command and result.
3. If tests fail, read the failure, fix the implementation (not the test) unless
   the test itself is stale, then rerun.
4. Include the passing command in the final summary.

When no test exists for the area, add a focused one when practical, or at
minimum run a build/type check (`cargo check`, `tsc --noEmit`, `pnpm build`).
