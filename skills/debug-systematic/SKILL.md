---
name: debug-systematic
description: Debug by forming a hypothesis and testing it, not by guessing. Reproduce first, read the actual error, isolate the cause, fix, then prove the fix with a regression test.
short_description: Debug systematically, not by guesswork.
category: debugging.workflow
triggers:
  - 调试
  - bug
  - 报错
  - 崩溃
  - 出错
  - 异常
  - debug
  - error
  - panic
  - 修复问题
  - 排查
tags: [debugging, diagnosis, testing]
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
# Debug Systematically

Guessing at a bug wastes more time than a structured diagnosis.

1. Reproduce it deterministically — you cannot debug what you cannot trigger.
2. Read the actual error message; trace the failing path; state what you
   EXPECT vs what actually happened.
3. Form one hypothesis at a time and test it (a focused log, a minimal repro).
4. Fix the root cause, not the symptom.
5. Prove the fix with a regression test, then re-run the original repro.
