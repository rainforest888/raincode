//! Autonomous development orchestration.
//!
//! 三角色:Ambassador(界面交互模型,用户大使)/ Orchestrator(主控模型,编排)/
//! Executor(执行子代理)。树状分发,最多 3 层;紧凑协议通信;上下文保护。
//!
//! 注意:编排执行引擎已归一化到 `rc-router`(allocator/capability/recursion/execute),
//! 本 crate 只保留 TUI 侧的数据结构:`tree::TaskTree`(任务树视图)与
//! `todo::TodoList`(每 agent 待办)。旧的 orchestrator/ambassador/pool/protocol/
//! context 模块是平行骨架,已删除(见 Plan10 A1)。
pub mod todo;
pub mod tree;
