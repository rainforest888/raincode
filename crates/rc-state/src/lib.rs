//! SQLite persistence for Raincode: sessions, messages, experiences,
//! skill index, embedding cache and the evolution audit log.
mod db;
mod models;

pub use db::{DbError, Store};
pub use models::{
    AuditEntry, CapabilityProfileRow, ExperienceRecord, Message, MessageRole, NavigationRecord,
    NavOutcome, Session, SkillRow,
};
