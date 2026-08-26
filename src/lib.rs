//! cp2 — a pure-Rust, high-performance `cp` and `rsync`-style copy/sync tool
//! for local directories or over the network (Linux, macOS, Windows).
//!
//! cp2 syncs a directory tree between two machines like rsync, but with a
//! modern delta engine: content-defined chunking (`FastCDC`) plus `BLAKE3`, so an
//! update retransmits only the chunks an edit actually touched — not the tail
//! of the file. It rides the system `ssh` client (the remote end runs
//! `cp2 --server`; sshd does authentication and permission enforcement, so
//! cp2 contains zero auth code), and the client auto-deploys a matching
//! binary to a fresh server on first use.
//!
//! # Library usage
//!
//! The sync engine runs over **any byte stream** — the CLI wires it to an
//! `ssh` channel, but a pipe, a socket, or a future mobile connection works
//! the same way:
//!
//! ```no_run
//! use cp2::{Executor, ExecutorOptions};
//! use tokio::io::{AsyncRead, AsyncWrite};
//!
//! async fn push_to_peer() -> cp2::Result<()> {
//!     // Any byte stream with a `cp2 --server` on the other end.
//!     let send: Box<dyn AsyncWrite + Unpin + Send> = Box::new(tokio::io::sink());
//!     let recv: Box<dyn AsyncRead + Unpin + Send> = Box::new(tokio::io::empty());
//!
//!     let mut executor = Executor::new(send, recv);
//!     executor
//!         .push(std::path::Path::new("photos/"), &ExecutorOptions::default())
//!         .await?;
//!     Ok(())
//! }
//! # let _ = push_to_peer; // not run: needs a live peer on the stream
//! ```
//!
//! [`Executor`] drives push (`push`), pull (`pull`), multi-root glob pushes
//! (`push_multi`), and the sshd-invoked server role (`serve`). [`Location`]
//! parses `user@host:path` targets; [`ExecutorOptions`] carries the rsync
//! decision flags (`--delete`, `--checksum`, `--archive`, ...).
//!
//! # What you get
//!
//! - **Delta updates** — changed chunks only, via `FastCDC` content-defined
//!   chunking and `BLAKE3` (see [`delta`]).
//! - **Verification & move-off** — `--verify` proves the destination bytes
//!   match the source (on-the-fly hashing, no re-reads); `--remove-source-files`
//!   deletes the source only after the destination is hash-verified, fsynced,
//!   and re-checked — safe for freeing an instrument's disk.
//! - **rsync semantics** — `-a` (special files/devices on Unix; owner/group
//!   are never preserved — 0-Root), `--delete`,
//!   `--backup`, `--no-*` opt-outs, include/exclude globs, exit code 23.
//! - **Realtime watch** (`-W`) — event-driven push, server-driven pull.
//! - **Cross-platform** — Linux, macOS, Windows, in any combination; the
//!   engine is embeddable (runs over any byte stream).
//!
//! # Architecture
//!
//! Decision logic is pure; async I/O is orchestrated by the executor:
//!
//! ```text
//! scanner → Manifest → planner → SyncPlan → strategy → executor → verify
//! ```
//!
//! - [`sync::scanner`] walks a tree into a serializable [`Manifest`]
//! - [`sync::planner`] maps source × destination manifests to a [`SyncPlan`]
//! - [`sync::strategy`] picks the transfer tier per file (batch/whole/delta/chunked)
//! - [`sync::sender`] / [`sync::receiver`] play the two protocol roles
//! - [`protocol`] is the wire format, agnostic of the transport
//! - [`platform`] is the portable filesystem layer (staging, metadata)
//!
//! # Security model
//!
//! Authentication and access control are entirely sshd's: the remote
//! `cp2 --server` runs as your account, and the receiver sanitizes every
//! peer-supplied path against directory traversal and symlink escapes.
//!
//! # Acknowledgments
//!
//! cp2's design and parts of its code are informed by copia, sparsync, sy,
//! robosync, pxs, and the rsync ecosystem (rsync, librsync, rusync, zsync-rs,
//! ripsync, msy, syncz). They are adapted (MIT/BSD-licensed, compatible with
//! cp2's MIT license) and credited per module in the source headers. See the
//! README's Acknowledgments table for the full map.

pub mod cli;
pub mod commands;
pub mod delta;
pub mod error;
pub mod platform;
pub mod protocol;
pub mod security;
pub mod sync;
pub mod target;
pub mod transport;

#[cfg(test)]
mod test_fuzz;

pub use error::{Error, Result};
pub use sync::{
    BandwidthLimiter, Executor, ExecutorOptions, FileEntry, FilterSet, Manifest, Planner, PlannerConfig, ScanOptions,
    Scanner, SyncAction, SyncPlan, SyncStats, SyncTask,
};
pub use target::{Location, RemoteTarget};
pub use transport::{JumpHost, RemoteClient, Session, SessionHandle, Transport, spawn_ssh};
