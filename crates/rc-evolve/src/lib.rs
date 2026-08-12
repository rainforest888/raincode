//! Self-evolution: turns finished sessions into experience records, decides
//! whether to propose/refine skills, and runs a background daemon that
//! clusters cross-session experience into higher-order skills.
pub mod cluster;
pub mod daemon;
pub mod darwinian;
pub mod engine;
pub mod nav_feedback;

pub use cluster::{greedy_clusters, Cluster};
pub use daemon::{DaemonConfig, DaemonError, DaemonReport, PatternDaemon};
pub use darwinian::{apply_organism, evolve_skill, fitness, mutate, FitnessScore, Organism};
pub use engine::{EvolveAction, EvolveConfig, EvolveEngine, EvolveReport};
pub use nav_feedback::{digest_navigation, NavDigest};
