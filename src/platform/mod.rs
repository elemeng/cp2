//! Platform abstraction layer.
//!
//! `fs.rs` provides portable filesystem primitives (positioned I/O, cloning,
//! preallocation, metadata), `staging.rs` the atomic staged-file sink built
//! on top, and `storage.rs` best-effort HDD/SSD detection for adaptive
//! worker tuning. There is no per-OS auth or storage: authentication is
//! sshd's job, and the sync engine keeps no persistent state of its own.

pub mod fs;
pub mod staging;
pub mod storage;
