//! Synchronization orchestration: pipeline of scanner → planner → strategy →
//! executor, over a single byte stream (the ssh stdio channel).

pub mod bandwidth;
pub mod executor;
pub mod filter;
mod handshake;
mod linkpolicy;
pub mod planner;
mod receiver;
pub mod scanner;
mod sender;
mod sigcache;
mod stats;
pub mod strategy;
pub(crate) mod watcher;
mod wire;

pub use bandwidth::BandwidthLimiter;
pub use executor::{Executor, ExecutorOptions, ProgressFn};
pub use filter::FilterSet;
pub use planner::{Planner, PlannerConfig, SyncAction, SyncPlan, SyncTask};
pub use scanner::{FileEntry, Manifest, ScanOptions, Scanner};
pub use stats::SyncStats;
pub use strategy::{
    FileClass, TransferStrategy, classify_file_size, determine_strategy,
};
