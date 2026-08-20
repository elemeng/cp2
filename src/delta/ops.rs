//! Delta encoding and decoding for content-defined-chunk synchronization.
//!
//! A delta represents the difference between a source file and a basis file,
//! expressed as a sequence of copy and literal operations. Copy ops reference
//! content-defined chunks of the basis; literal ops carry new bytes inline.
//!
//! Adapted from the copia crate (MIT licensed).

use serde::{Deserialize, Serialize};

use crate::delta::error::{DeltaError, DeltaResult};

/// Delta instruction types.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeltaOp {
    /// Copy `len` bytes from basis file starting at `offset`.
    Copy {
        /// Byte offset in the basis file.
        offset: u64,
        /// Number of bytes to copy.
        len: u32,
    },
    /// Insert literal bytes directly.
    Literal(Vec<u8>),
}

impl DeltaOp {
    /// Create a new copy operation.
    #[must_use]
    pub const fn copy(offset: u64, len: u32) -> Self {
        Self::Copy { offset, len }
    }

    /// Create a literal from a slice.
    #[must_use]
    pub fn literal_from_slice(data: &[u8]) -> Self {
        Self::Literal(data.to_vec())
    }
}

/// Encoded delta representing the difference between source and basis files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delta {
    /// Total size of the source file.
    pub source_size: u64,
    /// Total size of the basis file.
    pub basis_size: u64,
    /// Sequence of delta operations.
    pub ops: Vec<DeltaOp>,
    /// BLAKE3 checksum of the expected output (source file).
    ///
    /// `Some` only when verification is requested (`--verify` /
    /// `--remove-source-files`): the sender computes it while reading the
    /// source, and the receiver compares its reconstruction against it. In
    /// the default mode there is no whole-file checksum — chunk identity
    /// matching already covers the content, and the transport protects the
    /// wire; the default `None` saves a full-file BLAKE3 pass on each side.
    pub checksum: Option<[u8; 32]>,
}

impl Delta {
    /// Create a new empty delta.
    #[must_use]
    pub const fn new(source_size: u64, basis_size: u64) -> Self {
        Self {
            source_size,
            basis_size,
            ops: Vec::new(),
            checksum: None,
        }
    }

    /// Add a copy operation, merging contiguous copies.
    pub fn push_copy(&mut self, offset: u64, len: u32) {
        if len == 0 {
            return;
        }
        if let Some(DeltaOp::Copy {
            offset: prev_offset,
            len: prev_len,
        }) = self.ops.last_mut()
            && *prev_offset + u64::from(*prev_len) == offset
            && let Some(new_len) = prev_len.checked_add(len)
        {
            *prev_len = new_len;
            return;
        }
        self.ops.push(DeltaOp::copy(offset, len));
    }

    /// Add a literal operation, merging with the previous literal.
    pub fn push_literal(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        if let Some(DeltaOp::Literal(prev_data)) = self.ops.last_mut() {
            prev_data.extend_from_slice(data);
            return;
        }
        self.ops.push(DeltaOp::literal_from_slice(data));
    }

    /// Add a literal operation from an owned buffer — the buffer becomes the
    /// op's data, no copy (the zero-copy batch frame path: the wire read
    /// buffer is the content).
    pub fn push_literal_owned(&mut self, data: Vec<u8>) {
        if data.is_empty() {
            return;
        }
        if let Some(DeltaOp::Literal(prev_data)) = self.ops.last_mut() {
            prev_data.extend_from_slice(&data);
            return;
        }
        self.ops.push(DeltaOp::Literal(data));
    }

    /// Calculate total bytes copied from basis.
    #[must_use]
    pub fn bytes_matched(&self) -> u64 {
        self.ops
            .iter()
            .filter_map(|op| match op {
                DeltaOp::Copy { len, .. } => Some(u64::from(*len)),
                DeltaOp::Literal(_) => None,
            })
            .sum()
    }

    /// Calculate total literal bytes.
    #[must_use]
    pub fn bytes_literal(&self) -> u64 {
        self.ops
            .iter()
            .filter_map(|op| match op {
                DeltaOp::Literal(data) => Some(data.len() as u64),
                DeltaOp::Copy { .. } => None,
            })
            .sum()
    }

    /// Validate that all copy operations are within basis bounds.
    ///
    /// # Errors
    ///
    /// Returns [`DeltaError::InvalidCopyBounds`] if any `Copy` op exceeds the
    /// basis size.
    pub fn validate(&self) -> DeltaResult<()> {
        for op in &self.ops {
            if let DeltaOp::Copy { offset, len } = op {
                let end = offset.saturating_add(u64::from(*len));
                if end > self.basis_size {
                    return Err(DeltaError::InvalidCopyBounds {
                        offset: *offset,
                        len: *len,
                        basis_size: self.basis_size,
                    });
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delta_new() {
        let delta = Delta::new(5000, 4000);
        assert_eq!(delta.source_size, 5000);
        assert_eq!(delta.basis_size, 4000);
        assert!(delta.ops.is_empty());
    }

    #[test]
    fn delta_push_copy_merge_contiguous() {
        let mut delta = Delta::new(1000, 1000);
        delta.push_copy(0, 500);
        delta.push_copy(500, 200);
        assert_eq!(delta.ops.len(), 1);
        assert_eq!(delta.bytes_matched(), 700);
    }

    #[test]
    fn delta_push_copy_non_contiguous() {
        let mut delta = Delta::new(1000, 1000);
        delta.push_copy(0, 500);
        delta.push_copy(600, 200);
        assert_eq!(delta.ops.len(), 2);
        assert_eq!(delta.bytes_matched(), 700);
    }

    #[test]
    fn delta_push_literal_merges() {
        let mut delta = Delta::new(100, 0);
        delta.push_literal(b"hello");
        delta.push_literal(b" world");
        assert_eq!(delta.ops.len(), 1);
        assert_eq!(delta.bytes_literal(), 11);
    }

    #[test]
    fn delta_validate_valid() {
        let mut delta = Delta::new(1000, 1000);
        delta.push_copy(0, 500);
        delta.push_copy(500, 500);
        assert!(delta.validate().is_ok());
    }

    #[test]
    fn delta_validate_invalid_bounds() {
        let mut delta = Delta::new(1000, 500);
        delta.push_copy(0, 600);
        assert!(matches!(
            delta.validate(),
            Err(DeltaError::InvalidCopyBounds { .. })
        ));
    }

    #[test]
    fn delta_serde_roundtrip() {
        let mut delta = Delta::new(1000, 800);
        delta.push_copy(0, 400);
        delta.push_literal(b"inserted data");
        delta.push_copy(500, 300);

        let serialized = postcard::to_allocvec(&delta).unwrap();
        let restored: Delta = postcard::from_bytes(&serialized).unwrap();
        assert_eq!(delta, restored);
    }
}
