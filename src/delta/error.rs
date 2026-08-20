//! Delta module errors

use thiserror::Error;

/// Delta computation errors
#[derive(Error, Debug)]
pub enum DeltaError {
    #[error("Chunking error: {0}")]
    Chunking(String),

    #[error("Copy operation out of bounds: offset={offset} len={len} basis_size={basis_size}")]
    InvalidCopyBounds {
        /// Byte offset in the basis file.
        offset: u64,
        /// Number of bytes to copy.
        len: u32,
        /// Total size of the basis file.
        basis_size: u64,
    },

    #[error("Checksum mismatch: expected {expected:?}, got {actual:?}")]
    ChecksumMismatch {
        /// Expected checksum.
        expected: [u8; 32],
        /// Computed checksum.
        actual: [u8; 32],
    },

    #[error("Patch error: {0}")]
    Patch(String),

    #[error("Delta literal payload exceeds the {limit} byte budget (the basis matched nothing useful)")]
    LiteralBudgetExceeded {
        /// The literal-payload ceiling the caller imposed.
        limit: u64,
    },
}

pub type DeltaResult<T> = std::result::Result<T, DeltaError>;
