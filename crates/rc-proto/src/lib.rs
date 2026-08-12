//! Raincode wire protocol: newline-delimited JSON-RPC 2.0 between the Rust
//! core (`raincode --serve`) and any frontend (TUI, TS SDK, web).
pub mod events;
pub mod rpc;

pub use events::{AgentEvent, EventKind};
pub use rpc::{encode_line, Request, RequestMethod, Response, RpcError};
