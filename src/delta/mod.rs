//! Delta computation: content-defined-chunk (`FastCDC`) delta.
//!
//! `signature` signs a basis file as a list of content-defined chunks
//! (via `chunkrs`); `compute` chunks the source the same way and matches by
//! BLAKE3 hash, emitting `Copy`/`Literal` ops; `ops` is the delta encoding;
//! `apply_patch` reconstructs the source from basis + delta.
//!
//! This module is pure decision logic: no I/O, no async, no unsafe.
//! See [`compute::compute_delta_limited`] and [`compute::apply_patch`].

#![forbid(unsafe_code)]

pub mod compute;
pub mod error;
pub mod ops;
pub(crate) mod rollsum;
pub mod signature;

pub use compute::{apply_patch, compute_delta_limited, compute_delta_rollsum};
pub use error::{DeltaError, DeltaResult};
pub use ops::{Delta, DeltaOp};
pub use signature::{ChunkSignature, Signature, chunk_config};

/// rsync's automatic block size for a file of `file_size` bytes: the square
/// root rounded down to a multiple of 8, clamped to [700, 32768] (pure
/// function of the size, so both peers agree without configuration).
#[must_use]
pub fn block_size(file_size: u64) -> usize {
    rollsum::block_size(file_size)
}
