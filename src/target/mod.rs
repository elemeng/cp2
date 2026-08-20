//! Sync targets: local paths and remote `user@host:port/path` locations.

pub mod address;

pub use address::{Location, RemoteTarget};
