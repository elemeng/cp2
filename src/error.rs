//! High-level error type aggregating all crate errors.

use thiserror::Error;

/// Aggregated error type for the crate.
#[derive(Error, Debug)]
pub enum Error {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Delta error: {0}")]
    Delta(#[from] crate::delta::error::DeltaError),

    #[error("Protocol error: {0}")]
    Protocol(#[from] crate::protocol::error::ProtocolError),

    /// The peer's Hello reported a different build fingerprint (a stale
    /// remote binary). The one-shot flow redeploys and retries; a
    /// `--no-auto-install` run surfaces it as-is.
    #[error("peer build {peer_build} does not match this build")]
    HandshakeRejected { peer_build: String },

    #[error("Other error: {0}")]
    Other(String),
}

/// Result type alias.
pub type Result<T> = std::result::Result<T, Error>;
