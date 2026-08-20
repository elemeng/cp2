//! File-size tiering and transfer-strategy selection.
//!
//! Adapted from robosync `mixed_strategy.rs`: categorize files by size, then
//! choose an execution strategy per tier. Pure functions, no I/O.

#![forbid(unsafe_code)]

/// Minimum size for delta-transfer eligibility: below this, a whole-file
/// copy (or batch) is cheaper than the signature/delta round trip.
pub const DELTA_MIN_SIZE: u64 = 10 * 1024 * 1024; // 10MB

/// Execution strategy for a transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferStrategy {
    /// Copy the whole file.
    Copy,
    /// Compute a delta against an existing destination file.
    Delta,
}

/// Wire-format file class (sparsync §2C).
///
/// Small files are batched together into a single frame to amortize stream
/// overhead; medium files are transferred directly; large files are
/// transferred individually (as literals for new files, deltas for updates).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileClass {
    /// ≤ 128KB: batch with other small files.
    Small,
    /// ≤ 16MB: direct transfer.
    Medium,
    /// > 16MB: chunked transfer.
    Large,
}

/// Maximum size for batching small files into one frame.
/// Files at or below this size ride the sender's small-file batch (the
/// zero-copy raw frames, coalesced into 128 MiB frames) on a fresh transfer
/// instead of one postcard delta frame each — the per-frame wire cost of
/// ~192 KiB frames dominates below ~1 MiB (measured on the 10 GiB mixed
/// tree). Cross-file basis candidates () are excluded
/// from the batch by the sender.
pub const SMALL_FILE_MAX: u64 = 2 * 1024 * 1024;
/// Maximum size for direct (unbatched, unchunked) files: above this a new
/// file streams as sequential chunks. Large enough that a whole-file literal
/// delta fits comfortably in memory on any modern machine (the in-flight
/// window caps the aggregate).
pub const MEDIUM_FILE_MAX: u64 = 16 * 1024 * 1024;

/// Choose the number of hash workers for a sync (robosync
/// `optimal_thread_count`).
#[must_use]
pub fn optimal_thread_count(is_network: bool) -> usize {
    let cpus = num_cpus::get();
    if is_network { cpus.min(16) } else { cpus }
}

/// Classify a file by its wire-format size (sparsync thresholds).
#[must_use]
pub fn classify_file_size(size: u64) -> FileClass {
    if size <= SMALL_FILE_MAX {
        FileClass::Small
    } else if size <= MEDIUM_FILE_MAX {
        FileClass::Medium
    } else {
        FileClass::Large
    }
}

/// Choose a transfer strategy for a file.
///
/// `dest_exists` indicates whether the destination already has a file at the
/// same path (enabling delta transfer for files above [`DELTA_MIN_SIZE`]).
/// Smaller files — and new files without a basis — are plain whole-file
/// copies.
#[must_use]
pub fn determine_strategy(src_size: u64, dest_exists: bool) -> TransferStrategy {
    if dest_exists && src_size > DELTA_MIN_SIZE {
        TransferStrategy::Delta
    } else {
        TransferStrategy::Copy
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strategy_copy_below_delta_min() {
        assert_eq!(determine_strategy(0, true), TransferStrategy::Copy);
        assert_eq!(determine_strategy(DELTA_MIN_SIZE, true), TransferStrategy::Copy);
    }

    #[test]
    fn strategy_delta_above_delta_min_with_dest() {
        assert_eq!(
            determine_strategy(DELTA_MIN_SIZE + 1, true),
            TransferStrategy::Delta
        );
    }

    #[test]
    fn strategy_copy_for_small() {
        assert_eq!(determine_strategy(100, true), TransferStrategy::Copy);
        assert_eq!(determine_strategy(100, false), TransferStrategy::Copy);
    }

    #[test]
    fn strategy_delta_for_large_with_dest() {
        assert_eq!(
            determine_strategy(50 * 1024 * 1024, true),
            TransferStrategy::Delta
        );
    }

    #[test]
    fn strategy_copy_for_large_without_dest() {
        assert_eq!(
            determine_strategy(50 * 1024 * 1024, false),
            TransferStrategy::Copy
        );
    }

    #[test]
    fn strategy_copy_for_extra_large_without_dest() {
        assert_eq!(
            determine_strategy(500 * 1024 * 1024, false),
            TransferStrategy::Copy
        );
    }

    #[test]
    fn classify_size_small() {
        assert_eq!(classify_file_size(0), FileClass::Small);
        assert_eq!(classify_file_size(SMALL_FILE_MAX), FileClass::Small);
    }

    #[test]
    fn classify_size_medium() {
        assert_eq!(classify_file_size(SMALL_FILE_MAX + 1), FileClass::Medium);
        assert_eq!(classify_file_size(MEDIUM_FILE_MAX), FileClass::Medium);
    }

    #[test]
    fn classify_size_large() {
        assert_eq!(classify_file_size(MEDIUM_FILE_MAX + 1), FileClass::Large);
    }
}
