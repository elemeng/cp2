//! Small pure helpers shared by the sender and receiver roles.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use typed_path::Utf8UnixPathBuf;

use crate::protocol::{FileKind, LinkKind, FileMeta, Frame};
use crate::sync::scanner::{FileEntry, Manifest};
use crate::{Error, Result};

/// Stable identifier for a file (hash of its relative path).
pub(crate) fn file_id(path: &str) -> u64 {
    let mut h = DefaultHasher::new();
    path.hash(&mut h);
    h.finish()
}

/// The on-disk source path of an entry/task: an explicitly carried source
/// path (a subtree pulled in via `--follow-links` recursion) wins; otherwise
/// the relative path resolves under `root`.
pub(crate) fn file_source(root: &Path, source: Option<&Path>, relative_path: &Path) -> PathBuf {
    match source {
        Some(path) => path.to_path_buf(),
        None => root.join(relative_path),
    }
}

/// Encode a scanner mtime (`i64`) into its wire form (`u64`). Pre-1970
/// mtimes wrap; [`mtime_from_wire`] applies the inverse, so the roundtrip is
/// exact for every value.
#[expect(clippy::cast_sign_loss)]
pub(crate) fn mtime_to_wire(mtime: i64) -> u64 {
    mtime as u64
}

/// Decode a wire mtime (`u64`) back into an `i64` mtime.
#[expect(clippy::cast_possible_wrap)]
pub(crate) fn mtime_from_wire(mtime: u64) -> i64 {
    mtime as i64
}

/// Convert a lossy string path to its wire form ('/' separators on every
/// OS), for paths that arrive as strings rather than `Path` (see [`wire_rel`]).
///
/// The '/' contract is a type-level property: each `\`-separated host segment
/// (a Windows-style path) is pushed onto a [`Utf8UnixPathBuf`] and rendered
/// back through the Unix encoding, so the result is unambiguously Unix-syntax
/// on every compilation target — while the '/' structure (including a leading
/// root slash on absolute targets) passes through verbatim.
#[must_use]
pub(crate) fn wire_str(s: &str) -> String {
    let mut out = Utf8UnixPathBuf::new();
    for part in s.split('\\') {
        out.push(part);
    }
    out.to_string()
}

/// Convert a scanned entry into its wire form.
pub(crate) fn file_meta_from_entry(entry: &FileEntry) -> FileMeta {
    FileMeta {
        path: entry.relative_path.clone(),
        size: entry.size,
        mtime: mtime_to_wire(entry.mtime_sec),
        mtime_nsec: entry.mtime_nsec,
        mode: entry.mode,
        hash: entry.file_hash,
        kind: entry.kind,
        link_target: entry.link_target.clone(),
        inode: entry.inode,
        rdev: entry.rdev,
        uid: entry.uid,
        gid: entry.gid,
        atime: mtime_to_wire(entry.atime_sec),
        atime_nsec: entry.atime_nsec,
        xattrs: entry.xattrs.clone(),
    }
}

/// Rebuild a local manifest from the peer's wire file list.
///
/// The wire `FileMeta` carries no link *kind* — the source side alone decides
/// how a link is materialized (`LinkKind`, spec §3.2 预决策), and the receiver
/// executes the resulting `LinkSpec.kind` verbatim. Rebuilt entries therefore
/// default to `Symlink`, which is never consulted on this side: the planner
/// compares only `link_target` strings, and the sender plans from its own
/// scanned manifest. Preserve that invariant — a future consumer of a peer
/// entry's kind would silently mislabel every `.lnk` destination entry.
pub(crate) fn manifest_from_file_meta(metas: &[FileMeta]) -> Manifest {
    let files: Vec<FileEntry> = metas
        .iter()
        .map(|m| FileEntry {
            relative_path: m.path.clone(),
            size: m.size,
            mode: m.mode,
            mtime_sec: mtime_from_wire(m.mtime),
            mtime_nsec: m.mtime_nsec,
            file_hash: m.hash,
            kind: m.kind,
            is_dir: m.kind == FileKind::Dir,
            link_target: m.link_target.clone(),
            link_kind: LinkKind::Symlink,
            inode: m.inode,
            rdev: m.rdev,
            uid: m.uid,
            gid: m.gid,
            atime_sec: mtime_from_wire(m.atime),
            atime_nsec: m.atime_nsec,
            xattrs: m.xattrs.clone(),
            source_path: None,
            dereferenced: false,
        })
        .collect();
    let total_bytes = files.iter().map(|f| f.size).sum();
    Manifest {
        root: PathBuf::from("."),
        files,
        total_bytes,
        skipped: Vec::new(),
    }
}

/// Render a local path as a wire path: always '/'-separated, on every OS.
/// The scanner produces '/'-separated manifests; paths that round-trip through
/// `PathBuf` (e.g. the planner's) must be re-normalized before they go on the
/// wire, or a Windows sender would emit '\\' and a Unix receiver would treat
/// it as a literal filename character. Delegates to [`wire_str`] for the
/// Unix-semantic round-trip.
pub(crate) fn wire_rel(path: &std::path::Path) -> String {
    wire_str(&path.to_string_lossy())
}

/// Convert a frame received from the peer, mapping its `Frame::Error`
/// payload into a distinct error kind. All protocol loops funnel received
/// frames through this.
pub(crate) fn from_peer(frame: Frame) -> Result<Frame> {
    match frame {
        Frame::Error { message } => Err(Error::Other(format!("Peer error: {message}"))),
        other => Ok(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn wire_rel_normalizes_separators() {
        assert_eq!(wire_rel(Path::new("sub/file.txt")), "sub/file.txt");
        // A Windows-style path normalizes to '/'-separated on any OS.
        assert_eq!(wire_rel(Path::new(r"sub\file.txt")), "sub/file.txt");
        assert_eq!(wire_rel(Path::new("a/b/c.txt")), "a/b/c.txt");
        assert_eq!(wire_rel(Path::new("top.txt")), "top.txt");
    }
}
