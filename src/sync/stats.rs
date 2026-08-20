//! Statistics for sync operations

use std::time::Duration;

use crate::protocol::SkippedFile;

/// Statistics for a sync operation
#[derive(Debug, Clone, Default)]
pub struct SyncStats {
    pub files_sent: usize,
    pub files_received: usize,
    pub bytes_transferred: u64,
    pub duration: Duration,
    /// Files skipped instead of applied (locked by another process, path too
    /// long, reserved name, ...). Populated on both sides: the receiver
    /// records them locally and sends them back in the `Ack`.
    pub skipped: Vec<SkippedFile>,
}
