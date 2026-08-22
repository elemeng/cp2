//! Statistics for sync operations

use std::time::Duration;

use crate::protocol::{FileKind, SkippedFile};

/// What happened to one file, for `--itemize-changes` (`-i`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemizeAction {
    /// File created (new on the destination).
    Create,
    /// Existing file changed.
    Update,
    /// File removed from the destination (`--delete`).
    Delete,
    /// File already in sync (unchanged).
    Skip,
}

/// One per-file change entry, rendered by `--itemize-changes` (rsync `-i`).
#[derive(Debug, Clone)]
pub struct ItemizeEntry {
    /// The change performed (or the no-op for in-sync files).
    pub action: ItemizeAction,
    /// Path relative to the sync root.
    pub path: String,
    /// rsync file-type letter: `f` file, `d` dir, `L` symlink, `S` special.
    pub kind: char,
    /// File size in bytes (remote listings set it; itemize lines leave 0).
    pub size: u64,
}

impl ItemizeEntry {
    /// Create a change entry with the rsync file-type letter derived by the
    /// caller.
    #[must_use]
    pub fn new(action: ItemizeAction, path: String, kind: char) -> Self {
        Self {
            action,
            path,
            kind,
            size: 0,
        }
    }

    /// Attach the file size (used by `--list-only` listings).
    #[must_use]
    pub fn with_size(mut self, size: u64) -> Self {
        self.size = size;
        self
    }
}

/// rsync file-type letter for a [`FileMeta`]-shaped entry: `L` for links,
/// `d` for directories, `S` for specials, `f` for regular files.
pub(crate) fn kind_letter(kind: FileKind, is_link: bool) -> char {
    if is_link {
        'L'
    } else {
        match kind {
            FileKind::Dir => 'd',
            FileKind::File => 'f',
            FileKind::Symlink => 'L',
            _ => 'S',
        }
    }
}

/// Statistics for a sync operation
#[derive(Debug, Clone, Default)]
pub struct SyncStats {
    pub files_sent: usize,
    pub files_received: usize,
    pub bytes_transferred: u64,
    pub duration: Duration,
    /// Per-file change entries for `--itemize-changes` (`-i`). Populated on
    /// the side of the run that holds the plan: the sender on push / local
    /// copy, the receiver on pull.
    pub changes: Vec<ItemizeEntry>,
    /// Files skipped instead of applied (locked by another process, path too
    /// long, reserved name, ...). Populated on both sides: the receiver
    /// records them locally and sends them back in the `Ack`.
    pub skipped: Vec<SkippedFile>,
}
