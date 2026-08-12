//! CC-Switch-style provider profile management: a registry of profiles per
//! target app, switching the active profile, writing it into each target
//! CLI's config, importing from cc-switch (sqlite + deep links), and
//! feeding Raincode's own provider abstraction.
pub mod catalog;
pub mod cc_switch;
pub mod model;
pub mod secrets;
pub mod writers;

pub use catalog::{find as find_provider, ProviderCatalogEntry};
pub use cc_switch::{parse_deeplink, ProfileImport};
pub use model::{Profile, ProfileKind, Registry};
pub use secrets::{delete_key, home_dir, key_ref, protect_profile, store_key};
pub use writers::{all_writers, TargetConfigWriter};
