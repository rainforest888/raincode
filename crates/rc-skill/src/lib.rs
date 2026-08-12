//! The Raincode skill network.
//!
//! A skill is a `SKILL.md` file with YAML frontmatter describing its
//! category, typed DAG relations (`refines`, `prerequisite`, `variant_of`,
//! `composes`, `contradicts`), triggers, tags and provenance. The router
//! selects top-k skills for a task by embedding cosine plus keyword overlap;
//! the sources layer installs seeds and remote GitHub skills; the store
//! persists them to disk.
pub mod frontmatter;
pub mod model;
pub mod navigator;
pub mod network;
pub mod router;
pub mod seed;
pub mod source;
pub mod store;

pub use frontmatter::{parse_frontmatter, Relation, RelationKind, SkillFrontmatter};
pub use model::{validate_dag, Skill, SkillSummary};
pub use navigator::{NavAction, NavFrame, NavigatorLimits, SkillNavigator};
pub use network::{
    enforce_skill_shape, load_network_config, NetworkConfig, SkillNetwork, SkillNode, SoftLink,
};
pub use router::{cosine, SkillRouter, SkillSelection};
pub use seed::{install_seed, seed_installed};
pub use source::{InstallReport, LocalSource, RemoteSource, SkillHit, SkillSource};
pub use store::SkillStore;
