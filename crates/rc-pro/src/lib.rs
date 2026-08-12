//! Raincode provider abstraction: a canonical conversation format plus
//! streaming/embedding adapters for OpenAI, Anthropic, OpenAI-compatible,
//! Ollama and a scripted mock provider used by tests and demos.
pub mod anthropic;
pub mod canonical;
pub mod mock;
pub mod ollama;
pub mod openai;
pub mod provider;
pub mod sse;

pub use canonical::{
    CanonicalContent, CanonicalMessage, CanonicalRequest, CanonicalRole, CanonicalToolCall,
    ProvEvent, ToolDef,
};
pub use provider::{create_provider, Provider, ProviderConfig, ProviderError};
