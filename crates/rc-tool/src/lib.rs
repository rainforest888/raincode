//! Tool abstraction and built-in tools for Raincode.
pub mod builtin;
pub mod tool_output;
pub mod traits;

pub use traits::{SubagentFn, Tool, ToolContext, ToolRegistry, ToolResult, ToolSpec};
